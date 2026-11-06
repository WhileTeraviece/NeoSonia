//! UI 与后台引擎的桥：tokio runtime、工具注册、agent 引擎、邮件轮询、
//! 对话历史库、技能库、局域网 Web 服务、截图/附件。
//!
//! 所有跨线程都走 `Proxy<Msg>`；UI 只持有 `Bridge` 的句柄。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use fenestra::prelude::Proxy;
use neosonia_core::agent::{AgentEngine, EngineConfig, EventSink};
use neosonia_core::config::{Paths, Settings};
use neosonia_core::conversation::ConversationStore;
use neosonia_core::memory::MemoryStore;
use neosonia_core::model::AgentEvent;
use neosonia_core::persona::PersonaStore;
use neosonia_core::registry::ToolRegistry;
use neosonia_core::shared::{Attachment, AttachmentKind, ConversationHit, ConversationSummary};
use neosonia_core::skill::SkillStore;
use parking_lot::{Mutex, RwLock};
use tokio::sync::broadcast;

use crate::state::{Msg, NoticeLevel};

/// 事件广播容量（远端 WebSocket 用；慢了就丢旧事件，不阻塞主链路）。
const EVENT_CHANNEL: usize = 512;

/// 流式增量合并：模型每秒几十个 delta，逐条触发重绘会让老 GPU 掉帧。
/// 这里把 TextDelta/ReasoningDelta 攒起来，每 ~60ms 合成一条推给 UI。
#[derive(Default)]
struct DeltaBuffer {
    text: String,
    reasoning: String,
}

pub struct Bridge {
    pub rt: tokio::runtime::Runtime,
    pub engine: Arc<AgentEngine>,
    pub memory: Arc<MemoryStore>,
    pub personas: PersonaStore,
    pub settings: Arc<RwLock<Settings>>,
    pub conversations: Arc<ConversationStore>,
    pub skills: SkillStore,
    pub events: broadcast::Sender<AgentEvent>,
    paths: Paths,
    proxy: Proxy<Msg>,
    poller: Mutex<Option<neosonia_mail::MailPoller>>,
    web: Mutex<Option<neosonia_web::WebServer>>,
    /// MCP 服务器管理器（设置里启用时才有）。
    mcp: Mutex<Option<Arc<neosonia_mcp::McpManager>>>,
}

impl Bridge {
    /// 启动后台：记录、人设、工具注册、agent 引擎、文件索引、邮件轮询、Web 服务。
    pub fn start(paths: &Paths, settings: Settings, proxy: Proxy<Msg>) -> anyhow::Result<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(3)
            .thread_name("neosonia-worker")
            .enable_all()
            .build()
            .map_err(|e| anyhow::anyhow!("tokio runtime 创建失败: {e}"))?;

        let memory = Arc::new(MemoryStore::open(&paths.memory_db)?);
        let personas = PersonaStore::load(&paths.personas);
        let conversations = Arc::new(ConversationStore::open(&paths.conversations_db)?);
        let skills = SkillStore::load(&paths.skills);
        let settings_arc = Arc::new(RwLock::new(settings.clone()));

        let registry = ToolRegistry::new();
        neosonia_tools::register_all(&registry, &settings);
        neosonia_mail::register(&registry);
        tracing::info!(tools = registry.len(), "tools registered");

        // 本地文件索引（后台线程，类 Everything）
        if settings.index.enabled {
            let idx = neosonia_tools::index::FileIndex::global(paths);
            idx.start_background(settings_arc.clone());
        }

        // 事件广播：UI 的 proxy + 远端 WebSocket 同时消费。
        // 流式增量在这里合并（每 ~60ms 一批），其余事件立即转发。
        let (events, _rx) = broadcast::channel::<AgentEvent>(EVENT_CHANNEL);
        let buf: Arc<Mutex<DeltaBuffer>> = Arc::new(Mutex::new(DeltaBuffer::default()));
        let flush = {
            let p = proxy.clone();
            let b = buf.clone();
            let tx = events.clone();
            Arc::new(move || {
                let (text, reasoning) = {
                    let mut g = b.lock();
                    (std::mem::take(&mut g.text), std::mem::take(&mut g.reasoning))
                };
                if !reasoning.is_empty() {
                    let ev = AgentEvent::ReasoningDelta(reasoning);
                    let _ = tx.send(ev.clone());
                    p.send(Msg::AgentEvent(Arc::new(ev)));
                }
                if !text.is_empty() {
                    let ev = AgentEvent::TextDelta(text);
                    let _ = tx.send(ev.clone());
                    p.send(Msg::AgentEvent(Arc::new(ev)));
                }
            })
        };
        let sink: EventSink = {
            let p = proxy.clone();
            let tx = events.clone();
            let buf = buf.clone();
            let flush = flush.clone();
            Arc::new(move |ev: AgentEvent| match &ev {
                AgentEvent::TextDelta(t) => buf.lock().text.push_str(t),
                AgentEvent::ReasoningDelta(t) => buf.lock().reasoning.push_str(t),
                _ => {
                    flush();
                    let _ = tx.send(ev.clone());
                    p.send(Msg::AgentEvent(Arc::new(ev)));
                }
            })
        };
        // 定时把攒下的增量推给 UI（60ms 一批 ≈ 16 次/秒，够顺滑也不会刷屏）
        {
            let flush = flush.clone();
            rt.spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_millis(60));
                loop {
                    tick.tick().await;
                    flush();
                }
            });
        }

        let mcp_registry = registry.clone();
        let cfg = EngineConfig {
            settings: settings_arc.clone(),
            paths: paths.clone(),
            registry,
            memory: memory.clone(),
            personas: personas.clone(),
            conversations: conversations.clone(),
            skills: skills.clone(),
        };

        let engine = {
            let _enter = rt.enter();
            AgentEngine::start(cfg, sink)?
        };
        let engine = Arc::new(engine);

        // MCP：设置里启用时启动所有服务器（握手 + tools/list），把工具注册进同一个 registry。
        // 启动可能涉及起子进程，放到独立线程里 block_on，避免在 runtime 上下文里再 block_on。
        let mcp_manager: Option<Arc<neosonia_mcp::McpManager>> = {
            let enabled = settings_arc.read().mcp.enabled;
            if enabled {
                let handle = rt.handle().clone();
                let settings_for_mcp = settings_arc.clone();
                let paths_for_mcp = paths.clone();
                let joined = std::thread::Builder::new()
                    .name("neosonia-mcp-start".into())
                    .spawn(move || {
                        handle.block_on(neosonia_mcp::McpManager::start(
                            settings_for_mcp,
                            &mcp_registry,
                            &paths_for_mcp,
                        ))
                    })
                    .map(|jh| jh.join());
                match joined {
                    Ok(Ok(Ok(mgr))) => {
                        let servers = mgr.servers();
                        let total: usize = servers.iter().map(|s| s.tools).sum();
                        tracing::info!(servers = servers.len(), tools = total, "MCP 已启动");
                        Some(mgr)
                    }
                    Ok(Ok(Err(e))) => {
                        tracing::warn!("MCP 启动失败: {e}");
                        None
                    }
                    Ok(Err(_)) => {
                        tracing::warn!("MCP 启动线程异常退出");
                        None
                    }
                    Err(e) => {
                        tracing::warn!("MCP 启动线程创建失败: {e}");
                        None
                    }
                }
            } else {
                None
            }
        };

        let ui_proxy = proxy.clone();
        let bridge = Self {
            rt,
            engine,
            memory,
            personas,
            settings: settings_arc,
            conversations,
            skills,
            events,
            paths: paths.clone(),
            proxy,
            poller: Mutex::new(None),
            web: Mutex::new(None),
            mcp: Mutex::new(mcp_manager),
        };
        bridge.restart_poller();
        // v0.6 数据迁移：修"OAuth token 有值但 imap/smtp host 为空、mail.enabled=false"的旧配置。
        {
            let mut s = bridge.settings.write();
            if let Some(note) = neosonia_mail::migrate_settings(&mut s) {
                tracing::info!(note = %note, "邮件配置已自动修复");
                if let Err(e) = s.save(&bridge.paths.settings_file) {
                    tracing::warn!(error = %e, "保存修复后的邮件配置失败");
                }
            }
        }
        bridge.install_window_hook(ui_proxy);
        bridge.restart_web();
        Ok(bridge)
    }

    /// 设置 Agent 的"隐藏/恢复主窗口"回调（计算机使用/后台任务用）。
    fn install_window_hook(&self, proxy: crate::state::ProxyMsg) {
        let hook = std::sync::Arc::new(move |hidden: bool| {
            proxy.send(crate::state::Msg::AgentWindowVisibility(hidden));
        });
        self.engine.set_window_visibility_hook(hook);
    }

    pub fn submit(&self, text: String) -> String {
        self.engine.submit(text)
    }

    /// 提交一轮用户消息，附带图片（视觉输入）。
    pub fn submit_with_images(
        &self,
        text: String,
        images: Vec<neosonia_core::model::ImagePart>,
    ) -> String {
        self.engine.submit_with_images(text, images)
    }

    pub fn cancel(&self) {
        self.engine.cancel();
    }

    pub fn reset(&self) {
        self.engine.new_conversation();
    }

    pub fn approve(&self, call_id: &str, ok: bool) {
        self.engine.approve(call_id, ok);
    }

    pub fn set_persona(&self, id: &str) {
        self.engine.set_persona(id);
    }

    pub fn conversation_id(&self) -> Option<String> {
        self.engine.conversation_id()
    }

    pub fn load_conversation(&self, id: String) {
        self.engine.load_conversation(id);
    }

    pub fn new_conversation(&self) {
        self.engine.new_conversation();
    }

    /// 配置变更后：重建 LLM 客户端、重启轮询与 Web 服务、同步自启、重载技能。
    pub fn reload_config(&self, settings: Settings) {
        *self.settings.write() = settings.clone();
        self.engine.reload_config();
        self.restart_poller();
        self.restart_web();
        self.engine.reload_skills();
        // 开机自启同步统一由 app 层（启动/应用设置时）处理，这里不再重复 spawn reg。
    }

    /// 泛型任务提交（测试连接、邮件操作、技能下载等）。
    pub fn spawn<F>(&self, fut: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        self.rt.spawn(fut);
    }

    pub fn toast(&self, text: impl Into<String>, level: NoticeLevel) {
        self.proxy.send(Msg::Toast { text: text.into(), level });
    }

    pub fn paths(&self) -> Paths {
        self.paths.clone()
    }

    pub fn workspace(&self) -> PathBuf {
        let s = self.settings.read();
        let p = PathBuf::from(&s.agent.workspace_dir);
        if p.is_absolute() {
            p
        } else {
            self.paths.workspace.clone()
        }
    }

    // ---------------- 对话历史 ----------------

    pub fn list_conversations(&self, query: &str) -> Vec<ConversationSummary> {
        let q = query.trim();
        let q = if q.is_empty() { None } else { Some(q) };
        self.conversations.list(q, 100).unwrap_or_default()
    }

    pub fn search_conversations(&self, query: &str) -> Vec<ConversationHit> {
        let q = query.trim();
        if q.is_empty() {
            return Vec::new();
        }
        self.conversations.search_messages(q, 50).unwrap_or_default()
    }

    pub fn delete_conversation(&self, id: &str) -> bool {
        self.conversations.delete(id).unwrap_or(false)
    }

    // ---------------- 技能 ----------------

    pub fn skill_list(&self) -> Vec<neosonia_core::SkillSummary> {
        self.skills.list()
    }

    pub fn install_skill_path(&self, path: PathBuf) {
        let skills = self.skills.clone();
        let proxy = self.proxy.clone();
        let engine = self.engine.clone();
        self.rt.spawn(async move {
            let msg = match skills.install_from_path(&path) {
                Ok(s) => Msg::Toast {
                    text: format!("技能已安装: {}（{}）", s.name, s.id),
                    level: NoticeLevel::Success,
                },
                Err(e) => Msg::Toast {
                    text: format!("技能安装失败: {e}"),
                    level: NoticeLevel::Error,
                },
            };
            engine.reload_skills();
            let _ = proxy.send(msg);
            let _ = proxy.send(Msg::SkillsLoaded(skills.list()));
        });
    }

    pub fn install_skill_url(&self, url: String) {
        let skills = self.skills.clone();
        let proxy = self.proxy.clone();
        let engine = self.engine.clone();
        self.rt.spawn(async move {
            let msg = match skills.install_from_url(&url).await {
                Ok(s) => Msg::Toast {
                    text: format!("技能已安装: {}（{}）", s.name, s.id),
                    level: NoticeLevel::Success,
                },
                Err(e) => Msg::Toast {
                    text: format!("从 URL 安装失败: {e}"),
                    level: NoticeLevel::Error,
                },
            };
            engine.reload_skills();
            let _ = proxy.send(msg);
            let _ = proxy.send(Msg::SkillsLoaded(skills.list()));
        });
    }

    pub fn uninstall_skill(&self, id: &str) -> bool {
        let ok = self.skills.uninstall(id).unwrap_or(false);
        if ok {
            self.engine.reload_skills();
        }
        ok
    }

    // ---------------- 本地文件索引 ----------------

    pub fn rebuild_index(&self) {
        let idx = neosonia_tools::index::FileIndex::global(&self.paths);
        let proxy = self.proxy.clone();
        let settings = self.settings.clone();
        self.rt.spawn(async move {
            let s = settings.read().clone();
            let roots: Vec<PathBuf> = s
                .index
                .roots
                .iter()
                .map(|r| neosonia_core::util::expand_tilde(r))
                .collect();
            let exclude = s.index.exclude.clone();
            let max = s.index.max_entries;
            let result =
                tokio::task::spawn_blocking(move || idx.scan_now(&roots, &exclude, max)).await;
            let msg = match result {
                Ok(Ok(stats)) => Msg::Toast {
                    text: format!(
                        "索引完成: {} 个文件 / {} 个目录（{}）",
                        stats.files, stats.dirs, neosonia_core::util::human_duration_ms(stats.elapsed_ms)
                    ),
                    level: NoticeLevel::Success,
                },
                Ok(Err(e)) => Msg::Toast {
                    text: format!("索引失败: {e}"),
                    level: NoticeLevel::Error,
                },
                Err(e) => Msg::Toast {
                    text: format!("索引任务失败: {e}"),
                    level: NoticeLevel::Error,
                },
            };
            let _ = proxy.send(msg);
        });
    }

    // ---------------- 附件 ----------------

    /// 加载一个附件（图片解码成像素供缩略图；文本读预览；目录统计条目数）。
    pub fn load_attachment(&self, path: PathBuf) -> Attachment {
        let meta = std::fs::metadata(&path).ok();
        let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string());
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();

        if is_dir {
            let count = std::fs::read_dir(&path)
                .map(|rd| rd.flatten().take(200).count())
                .unwrap_or(0);
            return Attachment {
                path,
                name,
                kind: AttachmentKind::Folder,
                size,
                text_preview: Some(format!("目录，约 {count} 个顶层条目")),
                pixels: None,
            };
        }

        if matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp") {
            if let Ok(img) = image::open(&path) {
                let thumb = img.thumbnail(256, 256).to_rgba8();
                let (w, h) = thumb.dimensions();
                return Attachment {
                    path,
                    name,
                    kind: AttachmentKind::Image,
                    size,
                    text_preview: None,
                    pixels: Some((w, h, thumb.into_raw())),
                };
            }
        }

        if matches!(
            ext.as_str(),
            "txt" | "md" | "markdown"
                | "rs"
                | "py"
                | "js"
                | "ts"
                | "json"
                | "toml"
                | "yaml"
                | "yml"
                | "csv"
                | "log"
                | "html"
                | "css"
                | "sh"
                | "ps1"
                | "bat"
                | "c"
                | "h"
                | "cpp"
                | "java"
                | "go"
                | "ini"
                | "conf"
        ) {
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let preview: String = text.chars().take(2000).collect();
            return Attachment {
                path,
                name,
                kind: AttachmentKind::Text,
                size,
                text_preview: Some(preview),
                pixels: None,
            };
        }

        Attachment { path, name, kind: AttachmentKind::File, size, text_preview: None, pixels: None }
    }

    /// 「截图发模型」快捷键：抓主屏 → 存 PNG → 返回路径。
    ///
    /// 直接在 app 里用 xcap 抓屏（不走工具层）：快捷键要求低延迟，
    /// 也不希望依赖 agent 的 ToolCtx。
    pub fn capture_screen(&self) -> anyhow::Result<PathBuf> {
        use xcap::Monitor;
        let monitors = Monitor::all().map_err(|e| anyhow::anyhow!("枚举显示器失败: {e}"))?;
        let primary = monitors
            .iter()
            .find(|m| m.is_primary())
            .or_else(|| monitors.first())
            .ok_or_else(|| anyhow::anyhow!("没有可用显示器"))?;
        let image = primary
            .capture_image()
            .map_err(|e| anyhow::anyhow!("抓屏失败: {e}"))?;
        let name = format!("shot-{}.png", neosonia_core::util::now_ms());
        let path = self.paths.screenshots.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        image
            .save(&path)
            .map_err(|e| anyhow::anyhow!("保存截图失败: {e}"))?;
        Ok(path)
    }

    // ---------------- 邮件轮询 ----------------

    fn restart_poller(&self) {
        let mut slot = self.poller.lock();
        if let Some(p) = slot.take() {
            p.stop();
        }
        if !self.settings.read().mail.enabled {
            return;
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<neosonia_mail::MailEvent>();
        let poller = neosonia_mail::MailPoller::spawn(self.settings.clone(), tx);
        *slot = Some(poller);

        let proxy = self.proxy.clone();
        let engine = self.engine.clone();
        let settings = self.settings.clone();
        self.rt.spawn(async move {
            while let Some(ev) = rx.recv().await {
                match ev {
                    neosonia_mail::MailEvent::NewMail(m) => {
                        let (auto, wl) = {
                            let s = settings.read();
                            (s.mail.auto_reply, s.is_whitelisted(&m.from))
                        };
                        proxy.send(Msg::Toast {
                            text: format!("新邮件: {} - {}", m.from_name, m.subject),
                            level: NoticeLevel::Info,
                        });
                        // 用户在窗口可见时收到新邮件：列表之前只有 toast、不会刷新，
                        // 所以"打开窗口后新邮件不出现在列表"。这里主动让 UI 重新拉一次
                        // INBOX（app 侧 refresh_mail 有 loading 去重）。
                        proxy.send(Msg::MailRefresh);
                        if auto && wl {
                            let prompt = compose_mail_prompt(&m);
                            proxy.send(Msg::Toast {
                                text: format!("白名单来信，正在起草回复（uid {}）", m.uid),
                                level: NoticeLevel::Info,
                            });
                            engine.submit(prompt);
                        } else if auto {
                            proxy.send(Msg::Toast {
                                text: "收到非白名单邮件，未自动回复".into(),
                                level: NoticeLevel::Warning,
                            });
                        }
                    }
                    neosonia_mail::MailEvent::Error(e) => {
                        proxy.send(Msg::Toast {
                            text: format!("邮件错误: {e}"),
                            level: NoticeLevel::Error,
                        });
                    }
                    neosonia_mail::MailEvent::Replied { uid: _, to } => {
                        proxy.send(Msg::Toast {
                            text: format!("已回复 {to}"),
                            level: NoticeLevel::Success,
                        });
                    }
                    neosonia_mail::MailEvent::Started | neosonia_mail::MailEvent::Idle => {}
                }
            }
        });
    }

    // ---------------- 局域网 Web 服务 ----------------

    fn restart_web(&self) {
        let mut slot = self.web.lock();
        if let Some(server) = slot.take() {
            server.stop();
        }
        // 远程服务停止时清空待审列表（避免残留过期的审查提示）
        neosonia_web::review_hub().clear();
        if !self.settings.read().remote.enabled {
            return;
        }
        // 手机/平板要连过来：必须监听 0.0.0.0（绑回环连不上），并且二维码/配对页用 LAN IP。
        let port = self.settings.read().remote.port;
        let lan = lan_ip().map(|ip| ip.to_string());
        let advertise = lan
            .as_ref()
            .map(|ip| format!("http://{ip}:{port}"))
            .unwrap_or_else(|| format!("http://127.0.0.1:{port}"));
        ensure_firewall_rule(port);
        let cfg = neosonia_web::WebConfig {
            paths: self.paths.clone(),
            settings: self.settings.clone(),
            engine: self.engine.clone(),
            conversations: self.conversations.clone(),
            events: self.events.clone(),
            bind: Some("0.0.0.0".to_string()),
            advertise_url: Some(advertise.clone()),
        };
        let _enter = self.rt.enter();
        match neosonia_web::WebServer::start(cfg) {
            Ok(server) => {
                let info = server.info();
                tracing::info!(url = %info.url, bind = %advertise, code = %info.code, qr = ?info.qr_png, "LAN web server started");
                *slot = Some(server);
            }
            Err(e) => {
                tracing::warn!("Web 服务启动失败: {e}");
                self.toast(format!("局域网服务启动失败: {e}"), NoticeLevel::Warning);
            }
        }
    }

    /// 最近一次已知的厂商 API 状态（输入框右侧小球用）。
    pub fn provider_status(&self) -> Option<neosonia_core::provider_status::ProviderStatus> {
        self.engine.current_provider_status()
    }

    /// 刷新模型厂商 API 状态（输入框小球用）。
    ///
    /// 注意：core 内部用 tokio::spawn，必须在 runtime 上下文里调用，
    /// 否则会 panic（thread has no reactor running）而状态永远 None。
    pub fn refresh_provider_status(&self) {
        let _enter = self.rt.enter();
        self.engine.refresh_provider_status();
    }

    /// 本地索引运行态（设置页摘要用；SQLite 查询很快，但只在 Tick 里周期调用）。
    pub fn index_stats(&self) -> (u64, bool) {
        let idx = neosonia_tools::index::FileIndex::global(&self.paths);
        match idx.stats() {
            Ok(s) => (s.total_entries, s.scanning),
            Err(_) => (0, false),
        }
    }

    /// 审查弹窗的决定：允许 / 拒绝 / 拒绝并 1 天内拒绝同类。
    pub fn approve_decision(&self, call_id: &str, decision: crate::state::RiskChoice) {
        let d = match decision {
            crate::state::RiskChoice::Approve => neosonia_core::agent::ApproveDecision::Approve,
            crate::state::RiskChoice::Deny => neosonia_core::agent::ApproveDecision::Deny,
            crate::state::RiskChoice::DenyClass => {
                neosonia_core::agent::ApproveDecision::DenyClass
            }
        };
        self.engine.approve(call_id, d);
        // 局域网待审列表里移除这条（手机端只是只读提示）
        neosonia_web::review_hub().resolve(call_id);
    }

    /// 切换工作模式（会 emit ModeChanged，提示词按新模式重建）。
    pub fn set_mode(&self, mode: neosonia_core::config::AgentMode) {
        self.engine.set_mode(mode);
    }

    /// 当前邮箱 OAuth 诊断状态（token 受众/权限/是否需要重新授权）。
    pub fn mail_oauth_status(&self) -> Option<neosonia_mail::OAuthStatus> {
        let settings = self.settings.read().clone();
        Some(neosonia_mail::MailService::from_settings(&settings).oauth_status())
    }

    /// 退出邮箱登录：清除 OAuth 凭证，保留 IMAP/SMTP 服务器与账号（可继续用密码/授权码）。
    pub fn mail_logout(&self) -> String {
        let mut s = self.settings.write();
        let note = neosonia_mail::clear_oauth_authorization(&mut s);
        if let Err(e) = s.save(&self.paths.settings_file) {
            tracing::warn!(error = %e, "退出登录后保存设置失败");
        }
        drop(s);
        self.restart_poller();
        note
    }

    /// 启动邮箱 OAuth2 授权流程：返回浏览器要打开的授权地址；完成后台等待并写回 settings。
    pub fn mail_oauth_start(
        &self,
        provider: &str,
        proxy: crate::state::ProxyMsg,
    ) -> anyhow::Result<(String, Option<String>)> {
        use neosonia_core::config::OAuthProvider;
        let provider_id = provider.trim().to_ascii_lowercase();
        let prov = match provider_id.as_str() {
            "google" | "gmail" => OAuthProvider::Google,
            "custom" => OAuthProvider::Custom,
            _ => OAuthProvider::Microsoft,
        };
        let oauth_cfg = self.settings.read().mail.oauth.clone();
        let settings = self.settings.clone();
        let runner = self.rt.handle().clone();
        let cfg2 = oauth_cfg.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("neosonia-oauth-start".into())
            .spawn(move || {
                let res = runner.block_on(async move {
                    // 用带完整配置的入口：client_id/tenant/scope/auth_url/token_url/client_secret
                    // 都能传给 OAuth 实现（自定义 OAuth / Google 自建桌面应用必须这样做）。
                    neosonia_mail::OAuth::start_flow_with_settings(&cfg2).await
                });
                let _ = tx.send(res);
            })
            .map_err(|e| anyhow::anyhow!("OAuth 线程创建失败: {e}"))?;
        let flow = rx
            .recv_timeout(std::time::Duration::from_secs(60))
            .map_err(|_| anyhow::anyhow!("OAuth 启动超时（网络不通？）"))??;
        let authorize_url = flow.authorize_url.clone();
        let user_code = flow.user_code.clone();
        let wait = flow.wait;
        let paths = self.paths.clone();
        self.rt.spawn(async move {
            let msg = match wait.await {
                Ok(Ok(res)) => {
                    {
                        let mut s = settings.write();
                        // v0.6：OAuth 成功后不仅存 token，还要按服务商回填 IMAP/SMTP
                        // 主机/端口/TLS/账号并启用邮箱——否则用户"授权成功但收不到信"
                        // （真实配置证据：token 有值、imap.host 为空、mail.enabled=false）。
                        match neosonia_mail::apply_oauth_result(&mut s, &res) {
                            Ok(note) => tracing::info!(note = %note, "OAuth 授权结果已回填"),
                            Err(e) => {
                                tracing::warn!(error = %e, "OAuth 回填失败，退回只写 token");
                                let o = &mut s.mail.oauth;
                                o.enabled = true;
                                o.email = res.email.clone();
                                o.refresh_token = res.refresh_token.clone();
                                o.access_token = res.access_token.clone();
                                o.expires_at_ms = res.expires_at_ms;
                            }
                        }
                        if let Err(e) = s.save(&paths.settings_file) {
                            tracing::warn!("OAuth 登录后保存设置失败: {e}");
                        }
                    }
                    crate::state::Msg::MailOAuthDone(Ok(res.email))
                }
                Ok(Err(e)) => crate::state::Msg::MailOAuthDone(Err(format!("{e}"))),
                Err(e) => crate::state::Msg::MailOAuthDone(Err(format!("授权等待任务失败: {e}"))),
            };
            proxy.send(msg);
        });
        Ok((authorize_url, user_code))
    }

    /// 新建人设（写 personas/<id>.md）。
    pub fn persona_create(
        &self,
        id: &str,
        name: &str,
        description: &str,
        body: &str,
    ) -> anyhow::Result<()> {
        self.personas.create(id, name, description, body)?;
        Ok(())
    }

    /// 保存已有的人设。
    pub fn persona_save(
        &self,
        id: &str,
        name: &str,
        description: &str,
        body: &str,
    ) -> anyhow::Result<()> {
        self.personas.save(id, name, description, body)?;
        Ok(())
    }

    /// 删除人设（内置人设由 core 拒绝）。
    pub fn persona_delete(&self, id: &str) -> anyhow::Result<bool> {
        self.personas.delete(id)
    }

    /// MCP 服务器运行态（设置页展示）。
    pub fn mcp_servers(&self) -> Vec<crate::state::McpServerView> {
        self.mcp
            .lock()
            .as_ref()
            .map(|m| {
                m.servers()
                    .into_iter()
                    .map(|s| crate::state::McpServerView {
                        name: s.name,
                        ok: s.ok,
                        tools: s.tools,
                        error: s.error.unwrap_or_default(),
                        pid: s.pid,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 重启单台 MCP 服务器（按 name）。
    pub fn mcp_restart(&self, name: &str) -> anyhow::Result<()> {
        match self.mcp.lock().as_ref() {
            Some(m) => m.restart(name),
            None => anyhow::bail!("MCP 服务未启用（请在设置里启用并重启应用）"),
        }
    }

    pub fn mcp_enabled(&self) -> bool {
        self.mcp.lock().is_some()
    }

    pub fn web_info(&self) -> Option<neosonia_web::WebInfo> {
        self.web.lock().as_ref().map(|s| s.info())
    }

    pub fn refresh_pairing(&self) -> Option<neosonia_web::WebInfo> {
        self.web.lock().as_ref().map(|s| s.refresh_pairing())
    }

    /// 重命名已信任设备：优先走运行中的 WebServer（与远程改名同一份内存 store，
    /// 避免"写文件后被内存状态覆盖"），Web 未启用时直接改 devices.json。
    pub fn rename_device(&self, id: &str, name: &str) -> Option<neosonia_core::TrustedDevice> {
        if let Some(server) = self.web.lock().as_ref() {
            return server.rename_device(id, name);
        }
        neosonia_web::rename_device_in_file(&self.paths, id, name)
    }

    pub fn web_devices(&self) -> Vec<neosonia_core::TrustedDevice> {
        self.web.lock().as_ref().map(|s| s.devices()).unwrap_or_default()
    }

    pub fn revoke_device(&self, id: &str) -> bool {
        self.web.lock().as_ref().map(|s| s.revoke(id)).unwrap_or(false)
    }

    pub fn web_enabled(&self) -> bool {
        self.settings.read().remote.enabled
    }

    /// 打开本地路径（文件/目录）到系统默认程序。
    pub fn open_path(&self, path: &Path) -> bool {
        #[cfg(windows)]
        {
            std::process::Command::new("explorer")
                .arg(format!("\"{}\"", path.display()))
                .spawn()
                .is_ok()
        }
        #[cfg(not(windows))]
        {
            std::process::Command::new("xdg-open").arg(path).spawn().is_ok()
        }
    }
}

/// 白名单来信 -> 交给 agent 的指令。
fn compose_mail_prompt(m: &neosonia_mail::MailSummary) -> String {
    format!(
        "【后台自动化：白名单邮件】收到一封白名单来信。\n\
         发件人: {} <{}>\n主题: {}\nUID: {}\n摘要: {}\n\n\
         请执行：1) 用 mail_read 读取 uid={} 的正文；2) 按人设写一封简洁的中文回复；\
         3) 用 mail_reply 工具发出（uid={}）。完成后用一句话汇报。",
        m.from_name, m.from, m.subject, m.uid, m.snippet, m.uid, m.uid
    )
}

/// 把一条等待用户确认的审查请求镜像到局域网待审列表（手机端只读提示）。
pub fn publish_review_pending(
    call_id: &str,
    tool: &str,
    args: &str,
    level: u8,
    reason: &str,
    class: &str,
    emphasized: bool,
) {
    let risk_level = match level {
        0 => "",
        1 => "low",
        2 => "medium",
        3 => "high",
        _ => "critical",
    };
    let mut item = neosonia_web::ReviewItem::new(call_id, tool);
    item.args_summary = neosonia_core::util::truncate_chars(args, 200);
    item.risk_level = risk_level.to_string();
    item.risk_reason = neosonia_core::util::truncate_chars(reason, 200);
    item.class = class.to_string();
    item.emphasized = emphasized;
    neosonia_web::review_hub().push(item);
}
/// 抓主屏并存 PNG（后台线程用，不依赖 Bridge 实例）。
pub fn capture_screen_to(paths: &Paths) -> anyhow::Result<std::path::PathBuf> {
    // v0.6：改用 tools 的 Windows GDI 快路径（BitBlt + 快速 PNG），比 xcap 快很多；
    // 失败再退回 xcap。
    if let Ok(p) = neosonia_tools::screen::capture_primary(&paths.screenshots) {
        return Ok(p);
    }
    use xcap::Monitor;
    let monitors = Monitor::all().map_err(|e| anyhow::anyhow!("枚举显示器失败: {e}"))?;
    let primary = monitors
        .iter()
        .find(|m| m.is_primary())
        .or_else(|| monitors.first())
        .ok_or_else(|| anyhow::anyhow!("没有可用显示器"))?;
    let image = primary
        .capture_image()
        .map_err(|e| anyhow::anyhow!("抓屏失败: {e}"))?;
    let name = format!("shot-{}.png", neosonia_core::util::now_ms());
    let path = paths.screenshots.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    image
        .save(&path)
        .map_err(|e| anyhow::anyhow!("保存截图失败: {e}"))?;
    Ok(path)
}
/// 本机局域网 IP（UDP connect 不发包，只让路由表选出口网卡）。
pub fn lan_ip() -> Option<std::net::IpAddr> {
    use std::net::UdpSocket;
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    // 不真的发包；connect 只是让系统选择默认出口地址
    if sock.connect("8.8.8.8:80").is_err() {
        return None;
    }
    sock.local_addr().ok().map(|a| a.ip()).filter(|ip| !ip.is_loopback())
}

/// 确保 Windows 防火墙放行局域网端口（用户报"手机连不上"，根因之一是防火墙拦截）。
///
/// 需要管理员权限；没权限时静默失败（UI 里会有排障提示）。只添加一次规则。
pub fn ensure_firewall_rule(port: u16) {
    #[cfg(windows)]
    {
        // 每个端口每个进程只检查一次：用户报"展开设置/切页就弹控制台窗口"，
        // 而 netsh 也是控制台程序（即使 CREATE_NO_WINDOW，仍多余且慢）。
        static DONE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<u16>>> =
            std::sync::OnceLock::new();
        let done_mu = DONE.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()));
        {
            let mut done = done_mu.lock().unwrap_or_else(|e| e.into_inner());
            if !done.insert(port) {
                return;
            }
        }
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let name = format!("NeoSonia LAN {port}");
        // 已有规则就跳过
        let check = std::process::Command::new("netsh")
            .args([
                "advfirewall",
                "firewall",
                "show",
                "rule",
                &format!("name={name}"),
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        let exists = check
            .as_ref()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(&name))
            .unwrap_or(false);
        if exists {
            return;
        }
        let out = std::process::Command::new("netsh")
            .args([
                "advfirewall",
                "firewall",
                "add",
                "rule",
                &format!("name={name}"),
                "dir=in",
                "action=allow",
                "protocol=TCP",
                &format!("localport={port}"),
                "profile=private,domain",
                "enable=yes",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        match out {
            Ok(o) if o.status.success() => {
                tracing::info!(port, "已添加 Windows 防火墙放行规则（局域网访问）")
            }
            Ok(o) => tracing::warn!(
                port,
                err = %String::from_utf8_lossy(&o.stderr).trim(),
                "添加防火墙规则失败（需要管理员权限）"
            ),
            Err(e) => tracing::warn!(port, error = %e, "添加防火墙规则失败"),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = port;
    }
}