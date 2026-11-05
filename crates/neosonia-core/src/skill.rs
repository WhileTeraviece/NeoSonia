//! 技能系统：`skills/<id>/SKILL.md`（YAML front-matter + markdown 正文）。
//!
//! 技能 = 一段可复用的操作说明（可能带脚本/模板附件）。系统提示里只放摘要
//! （name + when_to_use），模型需要时用 `skill_load` 加载正文再照做。
//!
//! 首次加载会写盘两个内置示例技能；之后不再重复写（用 `.builtin-seeded` 标记）。
//!
//! OWNER: core-llm 队友。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use async_trait::async_trait;
use parking_lot::{Mutex, RwLock};
use serde_json::{Value, json};

use crate::config::Settings;
use crate::llm::{LlmClient, LlmRequest};
use crate::model::{ChatMessage, ToolCtx, ToolOutcome, ToolSpec};
use crate::persona::{parse_front_matter, quote_yaml, split_front_matter};
use crate::registry::{Tool, ToolRegistry};
use crate::shared::{SkillDetail, SkillSummary};
use crate::util::{is_within, middle_truncate, now_ms, truncate_chars};

/// 技能适用的模式 id（与 `AgentMode::id()` 一致）。
pub const SKILL_MODES: [&str; 3] = ["daily", "coding", "computer_use"];

/// 内置示例技能：`(id, SKILL.md 内容)`。
const BUILTIN_SKILLS: &[(&str, &str)] = &[
    ("screen-review", BUILTIN_SCREEN_REVIEW),
    ("mail-draft", BUILTIN_MAIL_DRAFT),
];

const BUILTIN_SCREEN_REVIEW: &str = r#"---
id: "screen-review"
name: "看截图定位界面问题"
description: "根据截图分析窗口布局、控件位置与显示异常，并给出可执行的点击或输入步骤。"
when_to_use: "用户说界面显示不对、按钮找不到、要按某个位置点击，或需要你复核截图内容时。"
source: "builtin"
modes: [daily, computer_use]
---

# 看截图定位界面问题

## 步骤

1. 先用 `screenshot` 工具截屏（必要时带 `region`）。工具返回文本里会有
   `[screenshot:<绝对路径>]` 标记；如果模型支持视觉输入，截图会自动作为图片消息发给你。
2. 观察截图，先描述整体布局：窗口标题、主要区域、可疑元素的大致坐标（以截图像素为准）。
3. 需要点击时用 `mouse_click` 并传 `x, y`（物理像素；`screenshot` 返回里有 scale 可用于换算）。
   需要输入时先点击目标输入框，再用 `keyboard_type`；需要快捷键用 `keyboard_key`。
4. 操作后再截一张图确认结果；如果界面没有变化，说明坐标或焦点不对，重新定位而不要连续盲点。

## 注意

- 坐标一律用截图的物理像素；不要用逻辑像素猜。
- 不要点击删除、发送、支付等有副作用的按钮，除非用户明确要求。
- 用户描述与截图不一致时，先指出差异再询问，不要自行猜测。
"#;

const BUILTIN_MAIL_DRAFT: &str = r#"---
id: "mail-draft"
name: "按人设写邮件草稿"
description: "根据收件人与上下文写中文商务邮件：称呼、正文、结尾，并可直接用 mail_reply 发出。"
when_to_use: "用户让你回复邮件、写通知/催办/进度汇报，或需要把一段内容整理成正式邮件时。"
source: "builtin"
modes: [daily]
---

# 按人设写邮件草稿

## 步骤

1. 先用 `mail_read` 读取原邮件正文（需要 uid 时先 `mail_list` 找 uid），确认发件人、
   主题、需要回答的问题与截止时间。
2. 起草回复：称呼用对方署名或"您好"；第一段直接回应对方的核心诉求；
   需要分点时用编号；结尾一句简短致意。正文控制在 200-400 字。
3. 不确定的信息写"待确认"，不要编造数字、日期或承诺。
4. 用户确认后调用 `mail_reply`（或 `mail_send`）；收件人不在白名单时先说明再发。

## 注意

- 中文商务文风：正式、简洁，不用网络用语，不用 emoji。
- 不把本机路径、账号、密钥、证件号等敏感信息写进邮件。
- 自动回复场景（后台白名单来信）不要反问用户，直接给出可发送的简短回复。
"#;

/// 一个技能的内存表示。
#[derive(Clone, Debug)]
struct SkillEntry {
    summary: SkillSummary,
    body: String,
    files: Vec<String>,
}

/// 技能仓库（线程安全，可 clone）。
#[derive(Clone)]
pub struct SkillStore {
    dir: PathBuf,
    inner: Arc<RwLock<BTreeMap<String, SkillEntry>>>,
    /// 安装后自动分类模式用（`AgentEngine::start` 注入；测试/独立使用时为 None）。
    settings: Arc<Mutex<Option<Arc<RwLock<Settings>>>>>,
}

impl SkillStore {
    /// 加载技能目录；目录不存在会自动创建，并写入内置示例技能。
    pub fn load(root: &Path) -> Self {
        let store = Self {
            dir: root.to_path_buf(),
            inner: Arc::new(RwLock::new(BTreeMap::new())),
            settings: Arc::new(Mutex::new(None)),
        };
        if let Err(e) = std::fs::create_dir_all(root) {
            tracing::warn!(dir = %root.display(), error = %e, "创建技能目录失败");
        }
        store.seed_builtins();
        store.reload();
        store
    }

    /// 注入设置（引擎启动时调用），安装技能后用它自动跑一次模式分类。
    pub fn attach_settings(&self, settings: Arc<RwLock<Settings>>) {
        *self.settings.lock() = Some(settings);
    }

    pub fn settings(&self) -> Option<Arc<RwLock<Settings>>> {
        self.settings.lock().clone()
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 重新扫描磁盘（UI 里点"刷新技能"、安装/卸载后调用）。
    pub fn reload(&self) {
        let mut map = BTreeMap::new();
        if let Ok(rd) = std::fs::read_dir(&self.dir) {
            for entry in rd.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let skill_md = path.join("SKILL.md");
                if !skill_md.is_file() {
                    continue;
                }
                if let Some(entry) = load_skill_dir(&path, &skill_md) {
                    map.insert(entry.summary.id.clone(), entry);
                }
            }
        }
        *self.inner.write() = map;
    }

    /// 全部技能摘要，按名字排序。
    pub fn list(&self) -> Vec<SkillSummary> {
        let mut out: Vec<SkillSummary> = self
            .inner
            .read()
            .values()
            .map(|entry| entry.summary.clone())
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// 只列当前模式匹配的技能（`modes` 为空视为 daily）。
    pub fn list_for_mode(&self, mode: &str) -> Vec<SkillSummary> {
        self.list()
            .into_iter()
            .filter(|skill| matches_mode(&skill.modes, mode))
            .collect()
    }

    /// 按 id 或名字取技能全文。
    pub fn get(&self, id: &str) -> Option<SkillDetail> {
        let guard = self.inner.read();
        let key = find_key(&guard, id)?;
        let entry = guard.get(&key)?;
        Some(SkillDetail {
            summary: entry.summary.clone(),
            body: entry.body.clone(),
            files: entry.files.clone(),
        })
    }

    /// 从本地路径安装：
    /// - 目录：必须包含 `SKILL.md`，整个目录（含附件）复制到 `skills/<id>/`；
    /// - 文件：作为 `SKILL.md` 复制到 `skills/<id>/`。
    pub fn install_from_path(&self, path: &Path) -> anyhow::Result<SkillSummary> {
        if !path.exists() {
            bail!("技能来源不存在：{}", path.display());
        }
        let md_path = if path.is_dir() {
            let md = path.join("SKILL.md");
            if !md.is_file() {
                bail!(
                    "技能目录里没有 SKILL.md：{}。请把技能说明命名为 SKILL.md 后重试。",
                    path.display()
                );
            }
            md
        } else {
            path.to_path_buf()
        };
        let text = std::fs::read_to_string(&md_path)
            .with_context(|| format!("读取技能文件失败: {}", md_path.display()))?;
        let scalars = front_matter_scalars(&text);
        let fallback_id = if path.is_dir() {
            path.file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default()
        } else {
            path.file_stem()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default()
        };
        let id = skill_id_from(&scalars, &fallback_id);
        let dest = self.dir.join(&id);
        if !is_within(&self.dir, &dest) || dest == self.dir {
            bail!("技能 id 非法（会写到技能目录之外）：{id}");
        }

        // 覆盖安装：先清掉旧目录，避免残留文件。
        if dest.exists() {
            std::fs::remove_dir_all(&dest)
                .with_context(|| format!("清理旧技能目录失败: {}", dest.display()))?;
        }
        std::fs::create_dir_all(&dest)
            .with_context(|| format!("创建技能目录失败: {}", dest.display()))?;

        if path.is_dir() {
            copy_dir_recursive(path, &dest)?;
        } else {
            let target = dest.join("SKILL.md");
            std::fs::copy(&md_path, &target).with_context(|| {
                format!(
                    "复制技能文件失败: {} -> {}",
                    md_path.display(),
                    target.display()
                )
            })?;
        }

        self.reload();
        let summary = self.get(&id).map(|detail| detail.summary).ok_or_else(|| {
            anyhow::anyhow!(
                "技能已复制到 {}，但重新扫描时没能解析出技能信息。请检查 front-matter 的 id/name。",
                dest.display()
            )
        })?;
        self.schedule_classification(&id);
        Ok(summary)
    }

    /// 从 URL 安装单个 markdown 技能（暂不支持 zip）。
    pub async fn install_from_url(&self, url: &str) -> anyhow::Result<SkillSummary> {
        let parsed = url::Url::parse(url.trim()).context("技能 URL 不合法")?;
        if !matches!(parsed.scheme(), "http" | "https") {
            bail!("只支持 http/https 链接，收到：{}", parsed.scheme());
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent("NeoSonia/0.2 (+skill-install)")
            .build()
            .context("创建下载客户端失败")?;
        let response = client
            .get(parsed.clone())
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("下载技能失败：{e}。请检查链接与网络。"))?;
        let status = response.status();
        if !status.is_success() {
            bail!("下载技能失败：HTTP {}", status.as_u16());
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|e| anyhow::anyhow!("读取技能内容失败：{e}"))?;
        if bytes.len() > 4 * 1024 * 1024 {
            bail!("技能文件太大（{} 字节，上限 4MB）", bytes.len());
        }
        if bytes.starts_with(b"PK\x03\x04") {
            bail!("暂不支持 zip 技能包：请先解压，把 SKILL.md 作为本地路径安装。");
        }
        let text = String::from_utf8_lossy(&bytes).to_string();
        let (front, body) = split_front_matter(&text);
        let scalars = front
            .as_deref()
            .map(parse_front_matter)
            .map(|parsed| parsed.scalars)
            .unwrap_or_default();
        let segments: Vec<&str> = parsed
            .path_segments()
            .map(|s| s.collect())
            .unwrap_or_default();
        let fallback = segments
            .last()
            .map(|name| name.trim_end_matches(".md").trim_end_matches(".markdown"))
            .unwrap_or("skill")
            .to_string();
        let id = skill_id_from(&scalars, &fallback);
        let name = scalars
            .get("name")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| id.clone());
        let description = scalars
            .get("description")
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let when_to_use = scalars
            .get("when_to_use")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| description.clone());

        let dest = self.dir.join(&id);
        if !is_within(&self.dir, &dest) || dest == self.dir {
            bail!("技能 id 非法（会写到技能目录之外）：{id}");
        }
        std::fs::create_dir_all(&dest)
            .with_context(|| format!("创建技能目录失败: {}", dest.display()))?;
        let normalized = format!(
            "---\nid: {}\nname: {}\ndescription: {}\nwhen_to_use: {}\nsource: \"url\"\nmodes: [daily]\n---\n\n{}\n",
            quote_yaml(&id),
            quote_yaml(&name),
            quote_yaml(&description),
            quote_yaml(&when_to_use),
            body.trim()
        );
        crate::util::atomic_write(&dest.join("SKILL.md"), normalized.as_bytes())
            .with_context(|| format!("写入技能文件失败: {}", dest.display()))?;
        self.reload();
        let summary = self
            .get(&id)
            .map(|detail| detail.summary)
            .ok_or_else(|| anyhow::anyhow!("技能已下载并写入，但解析失败，请检查 front-matter"))?;
        self.schedule_classification(&id);
        Ok(summary)
    }

    /// 卸载技能（删除 `skills/<id>/`）。返回是否真的删除了目录。
    pub fn uninstall(&self, id: &str) -> anyhow::Result<bool> {
        let id = sanitize_id(id);
        if id.is_empty() {
            bail!("技能 id 不能为空");
        }
        let dest = self.dir.join(&id);
        if !is_within(&self.dir, &dest) || dest == self.dir {
            bail!("技能 id 非法：{id}");
        }
        let removed = if dest.is_dir() {
            std::fs::remove_dir_all(&dest)
                .with_context(|| format!("删除技能目录失败: {}", dest.display()))?;
            true
        } else {
            false
        };
        self.reload();
        Ok(removed)
    }

    /// 首次启动写入两个内置示例技能（用标记文件保证只写一次）。
    fn seed_builtins(&self) {
        let marker = self.dir.join(".builtin-seeded");
        if marker.exists() {
            return;
        }
        let mut all_ok = true;
        for (id, content) in BUILTIN_SKILLS.iter().copied() {
            let dir = self.dir.join(id);
            let path = dir.join("SKILL.md");
            if path.is_file() {
                continue;
            }
            if let Err(e) = std::fs::create_dir_all(&dir) {
                tracing::warn!(dir = %dir.display(), error = %e, "创建内置技能目录失败");
                all_ok = false;
                continue;
            }
            if let Err(e) = crate::util::atomic_write(&path, content.as_bytes()) {
                tracing::warn!(path = %path.display(), error = %e, "写入内置技能失败");
                all_ok = false;
            }
        }
        if all_ok {
            let _ = crate::util::atomic_write(&marker, b"builtin skills seeded\n");
        }
    }

    /// 用 LLM 给技能分类适用模式并写回 SKILL.md（失败兜底 `[daily]`）。
    ///
    /// 安装动作是同步 API，这里用后台 tokio 任务异步完成分类；没有 runtime/设置时
    /// 只保留安装时的默认 modes。
    pub async fn classify_with_llm(&self, id: &str) -> anyhow::Result<Vec<String>> {
        let Some(detail) = self.get(id) else {
            bail!("没有找到技能 {id}");
        };
        let Some(settings_arc) = self.settings() else {
            bail!("技能分类需要设置（未调用 attach_settings）");
        };
        let settings = settings_arc.read().clone();
        let llm = LlmClient::new(settings.effective_server(), settings.effective_api_key());
        let modes = classify_modes_via_llm(&llm, &detail).await;
        self.rewrite_modes(&detail.summary.id, &modes)?;
        self.reload();
        Ok(modes)
    }

    fn schedule_classification(&self, id: &str) {
        if self.settings().is_none() {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let store = self.clone();
        let id = id.to_string();
        handle.spawn(async move {
            if let Err(error) = store.classify_with_llm(&id).await {
                tracing::warn!(skill = %id, error = %error, "技能模式分类失败，保留默认 modes");
                let _ = store.rewrite_modes(&id, &["daily".to_string()]);
            }
        });
    }

    /// 把 `modes` 写回 `skills/<id>/SKILL.md` 的 front-matter。
    pub fn rewrite_modes(&self, id: &str, modes: &[String]) -> anyhow::Result<()> {
        let detail = self
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("没有找到技能 {id}"))?;
        let path = PathBuf::from(&detail.summary.path);
        if !path.is_file() {
            bail!("技能文件不存在：{}", path.display());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("读取技能文件失败: {}", path.display()))?;
        let (front, body) = split_front_matter(&text);
        let scalars = front
            .as_deref()
            .map(parse_front_matter)
            .map(|parsed| parsed.scalars)
            .unwrap_or_default();
        let modes = normalize_modes(modes);
        let modes_line = format!("modes: [{}]", modes.join(", "));
        let mut out = String::from("---\n");
        for key in ["id", "name", "description", "when_to_use", "source"] {
            if let Some(value) = scalars.get(key) {
                let value = value.trim();
                if !value.is_empty() {
                    out.push_str(&format!("{key}: {}\n", quote_yaml(value)));
                }
            }
        }
        out.push_str(&modes_line);
        out.push_str("\n---\n\n");
        out.push_str(body.trim());
        out.push('\n');
        crate::util::atomic_write(&path, out.as_bytes())
            .with_context(|| format!("写入技能 modes 失败: {}", path.display()))?;
        Ok(())
    }
}

/// 技能 `modes` 是否匹配当前模式（空 = daily，兼容旧技能）。
pub fn matches_mode(modes: &[String], mode: &str) -> bool {
    if modes.is_empty() {
        return mode == "daily";
    }
    modes.iter().any(|item| item == mode)
}

/// 解析 front-matter 里的 `modes: [daily, coding]`。
pub fn parse_modes(raw: &str) -> Vec<String> {
    let raw = raw.trim().trim_start_matches('[').trim_end_matches(']');
    let mut out = Vec::new();
    for part in raw.split(',') {
        let part = part
            .trim()
            .trim_matches(|ch| ch == '"' || ch == '\'')
            .trim();
        if part.is_empty() {
            continue;
        }
        let part = part.to_ascii_lowercase();
        if SKILL_MODES.contains(&part.as_str()) && !out.iter().any(|existing| existing == &part) {
            out.push(part);
        }
    }
    out
}

/// 规范化 modes：过滤非法值、去重、保持稳定顺序；空 → `[daily]`。
pub fn normalize_modes(modes: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for mode in modes {
        let mode = mode.trim().to_ascii_lowercase();
        if SKILL_MODES.contains(&mode.as_str()) && !out.iter().any(|existing| existing == &mode) {
            out.push(mode);
        }
    }
    if out.is_empty() {
        out.push("daily".to_string());
    }
    out
}

/// 解析分类模型返回的 JSON 数组（容忍围栏/废话）；失败返回空。
pub fn parse_modes_json(text: &str) -> Vec<String> {
    let Some(start) = text.find('[') else {
        return Vec::new();
    };
    let Some(end) = text.rfind(']') else {
        return Vec::new();
    };
    if end <= start {
        return Vec::new();
    }
    let parsed: Value = match serde_json::from_str(&text[start..=end]) {
        Ok(value) => value,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    if let Some(items) = parsed.as_array() {
        for item in items {
            if let Some(mode) = item.as_str() {
                let mode = mode.trim().to_ascii_lowercase();
                if SKILL_MODES.contains(&mode.as_str()) && !out.contains(&mode) {
                    out.push(mode);
                }
            }
        }
    }
    out
}

/// 技能模式分类提示词（独立小请求，不带 agent 人设/历史）。
fn classify_modes_prompt(detail: &SkillDetail) -> (String, String) {
    let system = "你是技能分类器。判断一个技能适合在哪些模式使用，只输出 JSON 数组，\
元素取自 daily / coding / computer_use，可多选。不要解释。\n\
daily = 日常对话、文件整理、邮件、搜索；coding = 写代码、调试、构建；\
computer_use = 操作桌面应用、截图、鼠标键盘、UIA/CDP。";
    let user = format!(
        "技能名称: {}\n描述: {}\n适用场景: {}\n正文摘要:\n{}\n\n请输出 JSON 数组，例如 [\"daily\",\"coding\"]。",
        detail.summary.name,
        detail.summary.description,
        detail.summary.when_to_use,
        truncate_chars(&detail.body, 1500)
    );
    (system.to_string(), user)
}

async fn classify_modes_via_llm(llm: &LlmClient, detail: &SkillDetail) -> Vec<String> {
    let (system, user) = classify_modes_prompt(detail);
    let request = LlmRequest {
        messages: vec![ChatMessage::system(system), ChatMessage::user(user)],
        tools: Vec::new(),
        max_tokens: Some(100),
        temperature: Some(0.0),
    };
    let outcome = tokio::time::timeout(Duration::from_secs(30), llm.chat_once(request)).await;
    match outcome {
        Ok(Ok(reply)) => {
            let modes = parse_modes_json(&reply.text);
            if modes.is_empty() {
                vec!["daily".to_string()]
            } else {
                modes
            }
        }
        Ok(Err(error)) => {
            tracing::warn!(skill = %detail.summary.id, error = %error, "技能分类请求失败，兜底 daily");
            vec!["daily".to_string()]
        }
        Err(_) => {
            tracing::warn!(skill = %detail.summary.id, "技能分类请求超时，兜底 daily");
            vec!["daily".to_string()]
        }
    }
}

// ============================================================
// 解析辅助
// ============================================================

fn front_matter_scalars(text: &str) -> BTreeMap<String, String> {
    match split_front_matter(text) {
        (Some(front), _) => parse_front_matter(&front).scalars,
        (None, _) => BTreeMap::new(),
    }
}

fn skill_id_from(scalars: &BTreeMap<String, String>, fallback: &str) -> String {
    let raw = scalars
        .get("id")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| sanitize_id(fallback));
    let id = sanitize_id(&raw);
    if id.is_empty() {
        "skill".to_string()
    } else {
        id
    }
}

/// 技能 id 只允许字母数字、`-`、`_` 与 CJK，避免目录穿越。
fn sanitize_id(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for ch in id.trim().chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || (ch as u32) > 0x2E80 {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

fn find_key(map: &BTreeMap<String, SkillEntry>, id: &str) -> Option<String> {
    let wanted = id.trim();
    if wanted.is_empty() {
        return None;
    }
    if map.contains_key(wanted) {
        return Some(wanted.to_string());
    }
    let lower = wanted.to_lowercase();
    map.iter()
        .find(|(key, entry)| {
            key.to_lowercase() == lower || entry.summary.name.to_lowercase() == lower
        })
        .map(|(key, _)| key.clone())
}

fn load_skill_dir(dir: &Path, skill_md: &Path) -> Option<SkillEntry> {
    let text = std::fs::read_to_string(skill_md).ok()?;
    let (front, body) = split_front_matter(&text);
    let scalars = front
        .as_deref()
        .map(parse_front_matter)
        .map(|parsed| parsed.scalars)
        .unwrap_or_default();
    let dir_name = dir
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    let id = skill_id_from(&scalars, &dir_name);
    if id.is_empty() {
        return None;
    }
    let name = scalars
        .get("name")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| id.clone());
    let description = scalars
        .get("description")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let when_to_use = scalars
        .get("when_to_use")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| description.clone());
    let source = scalars
        .get("source")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "local".to_string());
    let modes = scalars
        .get("modes")
        .map(|raw| parse_modes(raw))
        .unwrap_or_default();
    let installed_at_ms = std::fs::metadata(skill_md)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or_else(now_ms);
    Some(SkillEntry {
        summary: SkillSummary {
            id: id.clone(),
            name,
            description,
            when_to_use,
            source,
            installed_at_ms,
            path: skill_md.display().to_string(),
            modes,
        },
        body: body.trim().to_string(),
        files: list_skill_files(dir),
    })
}

fn list_skill_files(dir: &Path) -> Vec<String> {
    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(dir)
        .max_depth(3)
        .into_iter()
        .filter_map(|entry| entry.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let relative = match path.strip_prefix(dir) {
            Ok(relative) => relative,
            Err(_) => continue,
        };
        let relative = relative.to_string_lossy().replace('\\', "/");
        if relative == "SKILL.md" || relative.starts_with('.') {
            continue;
        }
        files.push(relative);
        if files.len() >= 200 {
            break;
        }
    }
    files.sort();
    files
}

fn copy_dir_recursive(from: &Path, to: &Path) -> anyhow::Result<()> {
    for entry in walkdir::WalkDir::new(from)
        .max_depth(8)
        .into_iter()
        .filter_map(|entry| entry.ok())
    {
        let path = entry.path();
        if path == from {
            continue;
        }
        let relative = path
            .strip_prefix(from)
            .with_context(|| format!("计算相对路径失败: {}", path.display()))?;
        let target = to.join(relative);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target)
                .with_context(|| format!("创建目录失败: {}", target.display()))?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("创建目录失败: {}", parent.display()))?;
            }
            std::fs::copy(path, &target).with_context(|| {
                format!(
                    "复制技能文件失败: {} -> {}",
                    path.display(),
                    target.display()
                )
            })?;
        }
    }
    Ok(())
}

// ============================================================
// 内置工具
// ============================================================

/// 把 4 个技能工具注册进 registry。
pub fn register_builtin_tools(reg: &ToolRegistry, store: SkillStore) {
    reg.register(Arc::new(SkillListTool {
        store: store.clone(),
    }));
    reg.register(Arc::new(SkillLoadTool {
        store: store.clone(),
    }));
    reg.register(Arc::new(SkillInstallTool {
        store: store.clone(),
    }));
    reg.register(Arc::new(SkillUninstallTool { store }));
}

struct SkillListTool {
    store: SkillStore,
}

#[async_trait]
impl Tool for SkillListTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "skill_list",
            "列出已安装的技能摘要（名称 + 适用场景）。当用户的任务与某个技能的\n\
             when_to_use 匹配时，先用 skill_load 加载该技能正文再照做。",
            json!({ "type": "object", "properties": {} }),
        )
    }

    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> ToolOutcome {
        let skills = self.store.list();
        if skills.is_empty() {
            return ToolOutcome::ok(
                "当前没有安装任何技能。可以用 skill_install 从路径或 URL 安装。",
            );
        }
        let mut out = format!("已安装 {} 个技能：\n", skills.len());
        for skill in &skills {
            out.push_str(&format!(
                "- {}（id={}）: {}\n  适用: {}\n",
                skill.name,
                skill.id,
                truncate_chars(&skill.description, 160),
                truncate_chars(&skill.when_to_use, 160)
            ));
        }
        ToolOutcome::ok(out).with_display(format!("{} 个技能", skills.len()))
    }
}

struct SkillLoadTool {
    store: SkillStore,
}

#[async_trait]
impl Tool for SkillLoadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "skill_load",
            "加载一个技能的完整说明（markdown）。先 skill_list 找到合适技能，再调用本工具，\n\
             然后严格按正文里的步骤执行。",
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "技能 id 或名称。" }
                },
                "required": ["name"]
            }),
        )
    }

    async fn call(&self, _ctx: &ToolCtx, args: Value) -> ToolOutcome {
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if name.is_empty() {
            return ToolOutcome::err("skill_load 缺少 name 参数。请先用 skill_list 查看可用技能。");
        }
        match self.store.get(name) {
            None => {
                let ids = self
                    .store
                    .list()
                    .iter()
                    .map(|s| s.id.clone())
                    .collect::<Vec<_>>()
                    .join(", ");
                ToolOutcome::err(format!(
                    "没有找到技能「{name}」。可用技能 id：{}。",
                    if ids.is_empty() {
                        "（无）"
                    } else {
                        ids.as_str()
                    }
                ))
            }
            Some(detail) => {
                let mut out = format!(
                    "技能：{}（id={}）\n适用：{}\n\n{}\n",
                    detail.summary.name,
                    detail.summary.id,
                    detail.summary.when_to_use,
                    middle_truncate(&detail.body, 16_000)
                );
                if !detail.files.is_empty() {
                    out.push_str(&format!("\n附带文件：{}\n", detail.files.join(", ")));
                }
                ToolOutcome::ok(out).with_display(format!("加载技能 {}", detail.summary.name))
            }
        }
    }
}

struct SkillInstallTool {
    store: SkillStore,
}

#[async_trait]
impl Tool for SkillInstallTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "skill_install",
            "安装技能。source 可以是本地 SKILL.md 路径、包含 SKILL.md 的目录，\n\
             或 http(s) URL（单个 markdown 文件，暂不支持 zip）。",
            json!({
                "type": "object",
                "properties": {
                    "source": { "type": "string", "description": "本地路径或 http(s) 链接。" }
                },
                "required": ["source"]
            }),
        )
    }

    async fn call(&self, _ctx: &ToolCtx, args: Value) -> ToolOutcome {
        let source = args
            .get("source")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if source.is_empty() {
            return ToolOutcome::err("skill_install 缺少 source 参数（本地路径或 http(s) 链接）。");
        }
        let result = if source.starts_with("http://") || source.starts_with("https://") {
            self.store.install_from_url(source).await
        } else {
            let path = crate::util::expand_tilde(source);
            self.store.install_from_path(&path)
        };
        match result {
            Ok(summary) => ToolOutcome::ok(format!(
                "已安装技能 {}（id={}）：{}。需要时用 skill_load 加载正文。",
                summary.name, summary.id, summary.description
            ))
            .with_display(format!("安装技能 {}", summary.name)),
            Err(e) => ToolOutcome::err(format!("安装技能失败：{e}")),
        }
    }
}

struct SkillUninstallTool {
    store: SkillStore,
}

#[async_trait]
impl Tool for SkillUninstallTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "skill_uninstall",
            "卸载技能（删除技能目录）。删除后不可恢复；不确定时先用 skill_list 确认 id。",
            json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "技能 id。" }
                },
                "required": ["id"]
            }),
        )
    }

    async fn call(&self, _ctx: &ToolCtx, args: Value) -> ToolOutcome {
        let id = args.get("id").and_then(|v| v.as_str()).unwrap_or("").trim();
        if id.is_empty() {
            return ToolOutcome::err("skill_uninstall 缺少 id 参数。请先用 skill_list 查看 id。");
        }
        match self.store.uninstall(id) {
            Ok(true) => ToolOutcome::ok(format!("已卸载技能 {id}。")),
            Ok(false) => ToolOutcome::err(format!(
                "没有找到技能 {id} 的目录（可能已卸载）。请用 skill_list 确认。"
            )),
            Err(e) => ToolOutcome::err(format!("卸载技能失败：{e}")),
        }
    }
}

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn load_store() -> (tempfile::TempDir, SkillStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = SkillStore::load(&dir.path().join("skills"));
        (dir, store)
    }

    #[test]
    fn load_seeds_two_builtin_skills() {
        let (_dir, store) = load_store();
        let list = store.list();
        assert!(list.len() >= 2, "应写入两个内置技能");
        assert!(list.iter().any(|s| s.id == "screen-review"));
        assert!(list.iter().any(|s| s.id == "mail-draft"));
        let detail = store.get("screen-review").unwrap();
        assert_eq!(detail.summary.source, "builtin");
        assert!(detail.body.contains("截图"));
        assert!(!detail.summary.when_to_use.is_empty());
        // 再次 load 不会重复写（标记文件生效）。
        let second = SkillStore::load(store.dir());
        assert_eq!(second.list().len(), list.len());
    }

    #[test]
    fn parses_custom_skill_and_attachments() {
        let (_dir, store) = load_store();
        let dir = store.dir().join("my-skill");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nid: my-skill\nname: 自定义技能\ndescription: 做点事\n\
             when_to_use: 当用户提到自定义时\n---\n\n正文第一行。\n",
        )
        .unwrap();
        std::fs::write(dir.join("helper.ps1"), "Write-Host ok").unwrap();
        std::fs::write(dir.join("template.md"), "# 模板").unwrap();
        store.reload();
        let detail = store.get("my-skill").unwrap();
        assert_eq!(detail.summary.name, "自定义技能");
        assert_eq!(detail.summary.when_to_use, "当用户提到自定义时");
        assert_eq!(detail.body, "正文第一行。");
        assert!(detail.files.contains(&"helper.ps1".to_string()));
        assert!(detail.files.contains(&"template.md".to_string()));
        assert!(!detail.files.iter().any(|f| f == "SKILL.md"));
    }

    #[test]
    fn install_from_file_and_uninstall() {
        let (dir, store) = load_store();
        let source = dir.path().join("custom.md");
        std::fs::write(
            &source,
            "---\nid: custom\nname: 单文件技能\ndescription: 来自文件\n---\n正文\n",
        )
        .unwrap();
        let summary = store.install_from_path(&source).unwrap();
        assert_eq!(summary.id, "custom");
        assert_eq!(summary.name, "单文件技能");
        assert!(store.get("custom").is_some());
        assert!(store.uninstall("custom").unwrap());
        assert!(store.get("custom").is_none());
        assert!(!store.uninstall("custom").unwrap());
    }

    #[test]
    fn install_from_directory_copies_attachments() {
        let (dir, store) = load_store();
        let source = dir.path().join("bundle");
        std::fs::create_dir_all(source.join("scripts")).unwrap();
        std::fs::write(
            source.join("SKILL.md"),
            "---\nid: bundle\nname: 打包技能\n---\n正文\n",
        )
        .unwrap();
        std::fs::write(source.join("scripts").join("run.ps1"), "echo hi").unwrap();
        let summary = store.install_from_path(&source).unwrap();
        assert_eq!(summary.id, "bundle");
        let detail = store.get("bundle").unwrap();
        assert!(detail.files.iter().any(|f| f == "scripts/run.ps1"));
    }

    #[test]
    fn install_rejects_bad_source() {
        let (dir, store) = load_store();
        assert!(store.install_from_path(&dir.path().join("不存在")).is_err());
        let empty_dir = dir.path().join("empty");
        std::fs::create_dir_all(&empty_dir).unwrap();
        assert!(store.install_from_path(&empty_dir).is_err());
    }

    #[test]
    fn install_from_url_rejects_non_http() {
        let (_dir, store) = load_store();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt
            .block_on(store.install_from_url("ftp://example.com/a.md"))
            .unwrap_err();
        assert!(err.to_string().contains("http"));
    }

    #[test]
    fn register_skill_tools() {
        let (_dir, store) = load_store();
        let reg = ToolRegistry::new();
        register_builtin_tools(&reg, store);
        let mut names = reg.names();
        names.sort();
        assert_eq!(
            names,
            vec![
                "skill_install".to_string(),
                "skill_list".to_string(),
                "skill_load".to_string(),
                "skill_uninstall".to_string(),
            ]
        );
    }

    #[test]
    fn id_sanitizing_blocks_traversal() {
        assert_eq!(sanitize_id("../../etc/passwd"), "etc-passwd");
        assert_eq!(sanitize_id("a b"), "a-b");
        assert_eq!(sanitize_id("技能.1"), "技能-1");
    }

    #[test]
    fn mode_parsing_and_matching_helpers() {
        assert_eq!(parse_modes("[daily, coding]"), vec!["daily", "coding"]);
        assert_eq!(parse_modes("[\"coding\"]"), vec!["coding"]);
        assert!(parse_modes("").is_empty());
        assert_eq!(parse_modes("[daily, unknown, daily]"), vec!["daily"]);

        assert_eq!(normalize_modes(&[]), vec!["daily"]);
        assert_eq!(normalize_modes(&["CODING".into()]), vec!["coding"]);

        assert!(matches_mode(&[], "daily"), "空 modes 视为 daily");
        assert!(!matches_mode(&[], "coding"));
        assert!(matches_mode(&["coding".into()], "coding"));
        assert!(!matches_mode(&["coding".into()], "daily"));
    }

    #[test]
    fn mode_json_parsing_tolerates_fences() {
        assert_eq!(
            parse_modes_json("```json\n[\"daily\",\"computer_use\"]\n```"),
            vec!["daily", "computer_use"]
        );
        assert_eq!(parse_modes_json("[\"coding\",\"coding\"]"), vec!["coding"]);
        assert!(parse_modes_json("无法分类").is_empty());
        assert!(parse_modes_json("[\"unknown\"]").is_empty());
    }

    #[test]
    fn builtin_skills_have_mode_metadata() {
        let (_dir, store) = load_store();
        let screen = store.get("screen-review").unwrap().summary;
        assert!(screen.modes.contains(&"computer_use".to_string()));
        assert!(screen.modes.contains(&"daily".to_string()));
        let mail = store.get("mail-draft").unwrap().summary;
        assert_eq!(mail.modes, vec!["daily".to_string()]);

        let daily = store.list_for_mode("daily");
        assert_eq!(daily.len(), 2, "两个内置技能都适用于 daily");
        // coding 模式不匹配两个内置技能。
        assert!(store.list_for_mode("coding").is_empty());
        // computer_use 模式只匹配 screen-review。
        let cu = store.list_for_mode("computer_use");
        assert_eq!(cu.len(), 1);
        assert_eq!(cu[0].id, "screen-review");
    }

    #[test]
    fn rewrite_modes_updates_front_matter_and_filters() {
        let (dir, store) = load_store();
        let source = dir.path().join("coder-skill.md");
        std::fs::write(
            &source,
            "---\nid: coder-skill\nname: 编程技能\ndescription: 写代码用\n---\n\n正文\n",
        )
        .unwrap();
        store.install_from_path(&source).unwrap();
        // 安装后默认没有 modes（空 = daily）。
        assert!(
            store
                .list_for_mode("daily")
                .iter()
                .any(|s| s.id == "coder-skill")
        );

        let modes = vec!["coding".to_string(), "daily".to_string()];
        store.rewrite_modes("coder-skill", &modes).unwrap();
        store.reload();
        let detail = store.get("coder-skill").unwrap();
        assert_eq!(detail.summary.modes, modes);
        assert!(
            store
                .list_for_mode("coding")
                .iter()
                .any(|s| s.id == "coder-skill")
        );
        // 幂等：再写一次不会破坏 front-matter。
        store
            .rewrite_modes("coder-skill", &["coding".into()])
            .unwrap();
        store.reload();
        assert_eq!(
            store.get("coder-skill").unwrap().summary.modes,
            vec!["coding".to_string()]
        );
        assert!(store.get("coder-skill").unwrap().body.contains("正文"));
    }
}
