//! 后台邮件轮询器。
//!
//! 由 app 在启动时 `MailPoller::spawn(...)`；它周期性拉取收件箱未读邮件，把**没处理过**的
//! 邮件通过 [`MailEvent`] 推给 app。app 收到 `NewMail` 后自己决定是否组织一次 agent turn
//! （白名单内且 `auto_reply = true` 时）。**poller 自己不调用模型、不自动回复**。
//!
//! 去重：已处理的 uid 存在 `data/mail_seen.json`（按文件夹分组，只保留最近若干条），
//! 进程重启后不会重复推送同一封邮件。
//!
//! 轮询间隔每轮都从 `Settings::mail::poll_interval_secs` 重读，所以 UI 改配置后自动生效。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use neosonia_core::config::{Paths, Settings};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;

use crate::{MailEvent, MailService, MAX_LIST_LIMIT};

/// 每个文件夹最多记住多少个已处理 uid。
const MAX_SEEN_PER_FOLDER: usize = 5_000;
/// 配置不可用时，重读配置的间隔（秒）。
const IDLE_RECHECK_SECS: u64 = 5;
/// OAuth 需要重新授权时的退避（秒）：避免每轮都去刷令牌端点。
const REAUTH_BACKOFF_SECS: u64 = 600;

// ============================================================
// MailPoller
// ============================================================

/// 后台轮询器句柄。`stop()` 或 drop 时停止。
///
/// 轮询任务跑在**自己专用的 OS 线程 + current_thread tokio runtime** 上，
/// 因此 `spawn` 不要求调用方已处于 tokio runtime 上下文（app 的 UI 线程、
/// 设置页回调里直接调用都安全，不会因 `tokio::spawn` 缺 reactor 而 panic）。
pub struct MailPoller {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl MailPoller {
    /// 启动后台轮询任务。任何线程都可以调用。
    pub fn spawn(
        settings: Arc<RwLock<Settings>>,
        tx: UnboundedSender<MailEvent>,
    ) -> MailPoller {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_task = stop.clone();
        let seen_path = default_seen_path();
        let thread = std::thread::Builder::new()
            .name("neosonia-mail-poller".to_string())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = tx.send(MailEvent::Error(format!(
                            "邮件轮询器启动失败：无法创建 tokio runtime：{e}"
                        )));
                        return;
                    }
                };
                rt.block_on(run_loop(settings, tx, stop_for_task, seen_path));
            });
        let thread = match thread {
            Ok(handle) => Some(handle),
            Err(e) => {
                tracing::warn!(error = %e, "创建邮件轮询线程失败");
                None
            }
        };
        MailPoller { stop, thread }
    }

    /// 请求停止。最多等到当前轮询周期结束（睡眠按 250ms 分片，通常很快退出；
    /// 若正卡在一次 IMAP 请求里，要等该请求的 socket 超时）。不阻塞调用方。
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    pub fn is_stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
}

impl Drop for MailPoller {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // 不 join：避免 UI 线程被一次网络超时卡住；线程会在下个检查点退出。
        drop(self.thread.take());
    }
}

fn default_seen_path() -> PathBuf {
    Paths::resolve().data_dir.join("mail_seen.json")
}

// ============================================================
// 主循环
// ============================================================

async fn run_loop(
    settings: Arc<RwLock<Settings>>,
    tx: UnboundedSender<MailEvent>,
    stop: Arc<AtomicBool>,
    seen_path: PathBuf,
) {
    let _ = tx.send(MailEvent::Started);
    let mut seen = SeenStore::load(seen_path);
    let mut last_error: Option<String> = None;

    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let snapshot = settings.read().clone();
        let interval = snapshot.mail.poll_interval_secs.clamp(5, 3600);

        if !snapshot.mail.enabled {
            let _ = tx.send(MailEvent::Idle);
            if sleep_interruptible(&stop, Duration::from_secs(IDLE_RECHECK_SECS)).await {
                return;
            }
            continue;
        }

        let svc = MailService::from_settings(&snapshot);
        if !svc.imap_configured() {
            let msg = format!(
                "邮件轮询已开启，但 IMAP 不可用：{}。请在设置页选择邮箱类型（QQ/163/126/Outlook/Gmail）自动填服务器，填写账号+授权码；Outlook/Gmail 推荐点 OAuth2 登录。",
                svc.configuration_help()
            );
            if last_error.as_deref() != Some(msg.as_str()) {
                let _ = tx.send(MailEvent::Error(msg.clone()));
                last_error = Some(msg);
            }
            if sleep_interruptible(&stop, Duration::from_secs(IDLE_RECHECK_SECS)).await {
                return;
            }
            continue;
        }

        let folder = svc.default_folder().to_string();
        match svc.list(&folder, MAX_LIST_LIMIT, true).await {
            Ok(mails) => {
                last_error = None;
                let mut changed = false;
                for m in &mails {
                    if m.uid == 0 || seen.contains(&folder, m.uid) {
                        continue;
                    }
                    seen.insert(&folder, m.uid);
                    changed = true;
                    let _ = tx.send(MailEvent::NewMail(m.clone()));
                    if snapshot.mail.mark_read {
                        if let Err(e) = svc.mark_read(m.uid, true, &folder).await {
                            tracing::warn!(uid = m.uid, error = %e, "轮询：标记已读失败");
                        }
                    }
                }
                if changed {
                    seen.save();
                } else {
                    let _ = tx.send(MailEvent::Idle);
                }
            }
            Err(e) => {
                let msg = e.to_string();
                // OAuth 诊断：把 access_token 的 aud/scp 写日志（不含 token 本体），便于区分
                // “Office 公共 client 的 scope 问题” 与 “账号未开启 IMAP”。
                if snapshot.mail.oauth.enabled || !snapshot.mail.oauth.access_token.trim().is_empty() {
                    let claims =
                        crate::oauth::access_token_claims(&snapshot.mail.oauth.access_token);
                    tracing::warn!(
                        audience = ?claims.audience,
                        scopes = ?claims.scopes,
                        has_imap_scope = claims.has_imap_scope,
                        has_smtp_scope = claims.has_smtp_scope,
                        refresh_token_present = !snapshot.mail.oauth.refresh_token.trim().is_empty(),
                        "OAuth token claims 诊断（IMAP 轮询失败；不含 token 明文）"
                    );
                }
                let needs_reauth =
                    crate::oauth::needs_reauth() || crate::oauth::error_text_needs_reauth(&msg);
                if needs_reauth {
                    crate::oauth::mark_needs_reauth(&msg);
                }
                if last_error.as_deref() != Some(msg.as_str()) {
                    tracing::warn!(error = %msg, "邮件轮询失败");
                    let _ = tx.send(MailEvent::Error(msg.clone()));
                    last_error = Some(msg);
                }
                if needs_reauth {
                    // 清空 access_token（保留 refresh_token）：避免继续拿坏 token 刷请求。
                    // 真正重新授权由用户点「用 Microsoft 登录」完成，app 负责保存 settings。
                    {
                        let mut settings = settings.write();
                        if !settings.mail.oauth.access_token.is_empty() {
                            settings.mail.oauth.access_token.clear();
                            tracing::warn!("OAuth 需要重新授权：已清空缓存的 access_token");
                        }
                    }
                    // 退避 10 分钟，避免每 5 秒刷屏/刷接口。
                    if sleep_interruptible(&stop, Duration::from_secs(REAUTH_BACKOFF_SECS)).await {
                        return;
                    }
                    continue;
                }
            }
        }

        if sleep_interruptible(&stop, Duration::from_secs(interval)).await {
            return;
        }
    }
}

/// 可被停止标志打断的睡眠；返回 true 表示要停止。
async fn sleep_interruptible(stop: &AtomicBool, dur: Duration) -> bool {
    let deadline = Instant::now() + dur;
    loop {
        if stop.load(Ordering::SeqCst) {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        let remain = deadline.saturating_duration_since(now);
        tokio::time::sleep(remain.min(Duration::from_millis(250))).await;
    }
}

// ============================================================
// 去重存储 data/mail_seen.json
// ============================================================

#[derive(Debug, Default, Serialize, Deserialize)]
struct SeenFile {
    /// 文件夹名 -> 已处理的 uid 列表（升序、去重、有上限）。
    #[serde(default)]
    folders: BTreeMap<String, Vec<u32>>,
}

/// 已处理 uid 的持久化集合。
pub struct SeenStore {
    path: PathBuf,
    data: SeenFile,
}

impl SeenStore {
    /// 从磁盘加载；文件不存在/损坏时返回空集合（不报错、不 panic）。
    pub fn load(path: PathBuf) -> Self {
        let data = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<SeenFile>(&text).ok())
            .unwrap_or_default();
        Self { path, data }
    }

    pub fn contains(&self, folder: &str, uid: u32) -> bool {
        self.data
            .folders
            .get(folder)
            .map(|list| list.binary_search(&uid).is_ok())
            .unwrap_or(false)
    }

    pub fn insert(&mut self, folder: &str, uid: u32) {
        let entry = self.data.folders.entry(folder.to_string()).or_default();
        match entry.binary_search(&uid) {
            Ok(_) => {}
            Err(pos) => entry.insert(pos, uid),
        }
        if entry.len() > MAX_SEEN_PER_FOLDER {
            let excess = entry.len() - MAX_SEEN_PER_FOLDER;
            entry.drain(0..excess);
        }
    }

    /// 原子写回磁盘；失败只记日志（不影响轮询）。
    pub fn save(&self) {
        match serde_json::to_string_pretty(&self.data) {
            Ok(text) => {
                if let Err(e) = neosonia_core::util::atomic_write(&self.path, text.as_bytes()) {
                    tracing::warn!(path = %self.path.display(), error = %e, "保存 mail_seen.json 失败");
                }
            }
            Err(e) => tracing::warn!(error = %e, "序列化 mail_seen.json 失败"),
        }
    }

    pub fn len(&self) -> usize {
        self.data.folders.values().map(Vec::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_seen_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "neosonia-mail-seen-{}-{}-{}",
            std::process::id(),
            tag,
            neosonia_core::util::now_ms()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir.join("mail_seen.json")
    }

    #[test]
    fn seen_store_roundtrip() {
        let path = temp_seen_path("roundtrip");
        {
            let mut store = SeenStore::load(path.clone());
            assert!(store.is_empty());
            assert!(!store.contains("INBOX", 1));
            store.insert("INBOX", 3);
            store.insert("INBOX", 1);
            store.insert("INBOX", 3); // 重复不再插入
            store.insert("Sent", 9);
            assert!(store.contains("INBOX", 1));
            assert!(store.contains("INBOX", 3));
            assert!(store.contains("Sent", 9));
            assert!(!store.contains("INBOX", 9));
            assert_eq!(store.len(), 3);
            store.save();
        }
        {
            let store = SeenStore::load(path.clone());
            assert!(store.contains("INBOX", 1));
            assert!(store.contains("Sent", 9));
        }
        let _ = std::fs::remove_file(&path);
        if let Some(parent) = path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn seen_store_tolerates_missing_and_corrupt_file() {
        let path = temp_seen_path("corrupt");
        let store = SeenStore::load(path.clone());
        assert!(!store.contains("INBOX", 1));

        std::fs::write(&path, "{ this is not json").unwrap();
        let store = SeenStore::load(path.clone());
        assert!(store.is_empty());
        let _ = std::fs::remove_file(&path);
        if let Some(parent) = path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn seen_store_caps_per_folder() {
        let path = temp_seen_path("cap");
        let mut store = SeenStore::load(path.clone());
        for uid in 1..=(MAX_SEEN_PER_FOLDER as u32 + 10) {
            store.insert("INBOX", uid);
        }
        assert_eq!(store.data.folders["INBOX"].len(), MAX_SEEN_PER_FOLDER);
        // 最早的若干条被淘汰，最新的保留
        assert!(!store.contains("INBOX", 1));
        assert!(store.contains("INBOX", MAX_SEEN_PER_FOLDER as u32 + 10));
        let _ = std::fs::remove_file(&path);
        if let Some(parent) = path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn seen_path_points_inside_data_dir() {
        let path = default_seen_path();
        assert!(path.to_string_lossy().contains("mail_seen.json"));
    }

    #[tokio::test]
    async fn poller_stop_is_immediate_and_idempotent() {
        let settings = Arc::new(RwLock::new(Settings::default()));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let poller = MailPoller::spawn(settings, tx);
        // 应该马上收到 Started
        let first = tokio::time::timeout(Duration::from_secs(3), rx.recv()).await;
        assert!(matches!(first, Ok(Some(MailEvent::Started))));
        poller.stop();
        poller.stop();
        assert!(poller.is_stopped());
    }
}
