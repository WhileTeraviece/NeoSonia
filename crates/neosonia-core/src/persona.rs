//! 人设加载：`personas/*.md`（YAML front-matter）或 `personas/*.toml`。
//!
//! 不使用 serde_yaml（依赖清单里没有），front-matter 用一个小型行解析器处理，
//! 支持 `key: "带 \" 和 \\n 的字符串"`、缩进的 `initial_memory:` 子映射、块标量。
//! 内置 `default` 人设；目录为空时会写入一份 `default.md` 方便用户直接改。
//!
//! OWNER: core-llm 队友。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use parking_lot::RwLock;

use crate::model::Persona;
use crate::util::{atomic_write, slugify};

/// 内置默认人设的系统提示。
pub const DEFAULT_SYSTEM_PROMPT: &str = "你是 NeoSonia，一个常驻用户桌面的 AI Agent（不是聊天机器人）。\
你能读写文件、执行命令、截屏、模拟鼠标键盘、操作其它程序的界面、访问网页、收发邮件，\
也能使用长期记忆和子代理。\n\
\n\
工作原则：\n\
1. 先行动，后解释：能直接用工具完成任务时不要反问用户；信息确实不足且影响正确性时才提问，一次只问关键的一个问题。\n\
2. 简洁直接：默认用中文回答，先给结论或结果，再给必要说明；不寒暄、不铺陈。\n\
3. 危险操作先说明：删除或覆盖文件、执行有副作用的命令、关闭程序、发送邮件、模拟输入之前，\
用一句话说明要做什么和可能的影响；用户已经明确要求时直接执行。\n\
4. 失败要诚实：工具返回 [err] 时先读错误信息，说明原因和下一步，不要假装成功；\
同一失败不要连续重试超过两次。\n\
5. 长任务要耐心：命令、下载、编译可能持续很久，会流式返回输出；不要因为慢就重复发起或放弃。\n\
6. 尊重边界：文件操作默认限制在工作目录内；需要访问外部路径时先说明理由。\n\
7. 记忆偏好：用户表露长期偏好、身份信息或明确要求记住的事，写入长期记忆；\
不要把一次性细节当事实。\n\
8. 不确定就验证：涉及文件内容、命令结果、系统状态时用工具确认，不要凭猜测回答。\n\
\n\
回答格式：中文；步骤用编号列表；代码用围栏代码块；不要使用 emoji。";

/// 默认人设的固定记忆（用户可在设置页维护）。
const DEFAULT_BLOCKS: &[(&str, &str)] = &[
    ("style", "偏好简洁、直接的中文表达；不要 emoji。"),
    (
        "user_profile",
        "尚未填写。用户可在设置/记忆页补充称呼、语言、时区等。",
    ),
];

/// 目录为空时写入的 `default.md` 内容。
pub const DEFAULT_PERSONA_FILE: &str = "---
id: \"default\"
name: \"NeoSonia\"
description: \"常驻桌面的 AI Agent 助手\"
initial_memory:
  \"style\": \"偏好简洁、直接的中文表达；不要 emoji。\"
  \"user_profile\": \"尚未填写。用户可在设置/记忆页补充称呼、语言、时区等。\"
---

你是 NeoSonia，一个常驻用户桌面的 AI Agent（不是聊天机器人）。你能读写文件、执行命令、截屏、模拟鼠标键盘、操作其它程序的界面、访问网页、收发邮件，也能使用长期记忆和子代理。

工作原则：
1. 先行动，后解释：能直接用工具完成任务时不要反问用户；信息确实不足且影响正确性时才提问，一次只问关键的一个问题。
2. 简洁直接：默认用中文回答，先给结论或结果，再给必要说明；不寒暄、不铺陈。
3. 危险操作先说明：删除或覆盖文件、执行有副作用的命令、关闭程序、发送邮件、模拟输入之前，用一句话说明要做什么和可能的影响；用户已经明确要求时直接执行。
4. 失败要诚实：工具返回 [err] 时先读错误信息，说明原因和下一步，不要假装成功；同一失败不要连续重试超过两次。
5. 长任务要耐心：命令、下载、编译可能持续很久，会流式返回输出；不要因为慢就重复发起或放弃。
6. 尊重边界：文件操作默认限制在工作目录内；需要访问外部路径时先说明理由。
7. 记忆偏好：用户表露长期偏好、身份信息或明确要求记住的事，写入长期记忆；不要把一次性细节当事实。
8. 不确定就验证：涉及文件内容、命令结果、系统状态时用工具确认，不要凭猜测回答。

回答格式：中文；步骤用编号列表；代码用围栏代码块；不要使用 emoji。";

/// 内置完整版 default 人设（`Persona::builtin_default` 的系统提示为空，由这里补全）。
pub fn builtin_persona() -> Persona {
    let mut persona = Persona::builtin_default();
    persona.name = "NeoSonia".to_string();
    persona.description = "常驻桌面的 AI Agent 助手".to_string();
    persona.system_prompt = DEFAULT_SYSTEM_PROMPT.to_string();
    persona.initial_memory = DEFAULT_BLOCKS
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    persona
}

/// 人设仓库。目录里每个文件一个人设，`default` 为内置兜底。
#[derive(Clone)]
pub struct PersonaStore {
    dir: PathBuf,
    inner: Arc<RwLock<BTreeMap<String, Persona>>>,
}

impl PersonaStore {
    /// 扫描目录并加载；目录不存在则创建，并写入一份默认人设文件。
    pub fn load(dir: &Path) -> Self {
        let store = Self {
            dir: dir.to_path_buf(),
            inner: Arc::new(RwLock::new(BTreeMap::new())),
        };
        if let Err(e) = std::fs::create_dir_all(dir) {
            tracing::warn!(dir = %dir.display(), error = %e, "创建人设目录失败");
        }
        store.ensure_default_file();
        store.reload();
        store
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 重新扫描（UI 里点"刷新人设"）。
    pub fn reload(&self) {
        let mut map = BTreeMap::new();
        map.insert("default".to_string(), builtin_persona());
        if let Ok(rd) = std::fs::read_dir(&self.dir) {
            for entry in rd.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                if let Some(persona) = load_persona_file(&path) {
                    map.insert(persona.id.clone(), persona);
                }
            }
        }
        *self.inner.write() = map;
    }

    pub fn get(&self, id: &str) -> Persona {
        let id = id.trim();
        let guard = self.inner.read();
        if !id.is_empty() {
            if let Some(found) = guard.get(id) {
                return found.clone();
            }
        }
        guard
            .get("default")
            .cloned()
            .unwrap_or_else(builtin_persona)
    }

    pub fn list(&self) -> Vec<Persona> {
        self.inner.read().values().cloned().collect()
    }

    pub fn ids(&self) -> Vec<String> {
        self.inner.read().keys().cloned().collect()
    }

    /// 写入一个人设文件（UI 新建/编辑人设）。返回写入路径。
    pub fn save_persona(&self, persona: &Persona) -> anyhow::Result<PathBuf> {
        let raw_id = {
            let id = persona.id.trim();
            if id.is_empty() {
                crate::util::slugify(&persona.name)
            } else {
                id.to_string()
            }
        };
        let file_id = sanitize_id(&raw_id);
        if file_id.is_empty() {
            bail!("人设 id 非法：不能为空，也不能只包含路径分隔符。请换一个名字。");
        }
        if let Err(e) = std::fs::create_dir_all(&self.dir) {
            return Err(anyhow::anyhow!(
                "创建人设目录失败（{}）：{e}",
                self.dir.display()
            ));
        }
        let path = self.dir.join(format!("{file_id}.md"));

        let name = if persona.name.trim().is_empty() {
            file_id.clone()
        } else {
            persona.name.trim().to_string()
        };
        let mut text = String::new();
        text.push_str("---\n");
        // 文件名字符集有限制，但 front-matter 里的 id 保留用户输入的原始值。
        text.push_str(&format!("id: {}\n", quote_yaml(&raw_id)));
        text.push_str(&format!("name: {}\n", quote_yaml(&name)));
        text.push_str(&format!(
            "description: {}\n",
            quote_yaml(persona.description.trim())
        ));
        if !persona.initial_memory.is_empty() {
            text.push_str("initial_memory:\n");
            for (key, value) in &persona.initial_memory {
                text.push_str(&format!("  {}: {}\n", quote_yaml(key), quote_yaml(value)));
            }
        }
        text.push_str("---\n\n");
        text.push_str(persona.system_prompt.trim());
        text.push('\n');

        atomic_write(&path, text.as_bytes())
            .with_context(|| format!("写入人设文件失败: {}", path.display()))?;
        self.reload();
        Ok(path)
    }

    /// 删除人设文件（内置 default 不允许删除，返回 false）。
    pub fn delete_persona(&self, id: &str) -> anyhow::Result<bool> {
        let id = id.trim();
        if id.is_empty() || id == "default" {
            bail!("内置 default 人设不能删除");
        }
        let path = self.dir.join(format!("{}.md", sanitize_id(id)));
        if path.is_file() {
            std::fs::remove_file(&path)
                .with_context(|| format!("删除人设文件失败: {}", path.display()))?;
            self.reload();
            return Ok(true);
        }
        let toml_path = self.dir.join(format!("{}.toml", sanitize_id(id)));
        if toml_path.is_file() {
            std::fs::remove_file(&toml_path)
                .with_context(|| format!("删除人设文件失败: {}", toml_path.display()))?;
            self.reload();
            return Ok(true);
        }
        Ok(false)
    }

    /// 新建人设（app 设置页用）。id 只允许 `[a-z0-9_-]`，重名报错。
    pub fn create(
        &self,
        id: &str,
        name: &str,
        description: &str,
        body: &str,
    ) -> anyhow::Result<Persona> {
        let id = validate_persona_id(id)?;
        if self.inner.read().contains_key(&id) {
            bail!("人设 id 已存在：{id}。请换一个 id，或直接编辑现有人设。");
        }
        let path = self.dir.join(format!("{id}.md"));
        if path.exists() {
            bail!("人设文件已存在：{}。请换一个 id。", path.display());
        }
        let mut persona = Persona::builtin_default();
        persona.id = id.clone();
        persona.initial_memory.clear();
        self.write_persona(&mut persona, name, description, body)?;
        Ok(self.get(&id))
    }

    /// 保存（覆盖）已有人设。id 不存在时报中文错误（新建请用 create）。
    pub fn save(
        &self,
        id: &str,
        name: &str,
        description: &str,
        body: &str,
    ) -> anyhow::Result<Persona> {
        let id = validate_persona_id(id)?;
        if !self.inner.read().contains_key(&id) {
            bail!("人设不存在：{id}。请先新建人设，或检查 id 是否正确。");
        }
        let mut persona = self.get(&id);
        self.write_persona(&mut persona, name, description, body)?;
        Ok(self.get(&id))
    }

    /// 删除人设；内置 default 不允许删除。返回是否真的删除了文件。
    pub fn delete(&self, id: &str) -> anyhow::Result<bool> {
        let id = id.trim();
        if id.eq_ignore_ascii_case("default") {
            bail!("内置 default 人设不能删除");
        }
        self.delete_persona(id)
    }

    fn write_persona(
        &self,
        persona: &mut Persona,
        name: &str,
        description: &str,
        body: &str,
    ) -> anyhow::Result<()> {
        let name = name.trim();
        let body = body.trim();
        if body.is_empty() {
            bail!("人设正文不能为空，请写入这个人设的行为说明。");
        }
        persona.name = if name.is_empty() {
            persona.id.clone()
        } else {
            name.to_string()
        };
        persona.description = description.trim().to_string();
        persona.system_prompt = body.to_string();
        let path = self.save_persona(persona)?;
        tracing::debug!(id = %persona.id, path = %path.display(), "人设已保存");
        Ok(())
    }

    fn ensure_default_file(&self) {
        let has_persona_file = std::fs::read_dir(&self.dir)
            .map(|rd| {
                rd.flatten().any(|entry| {
                    let path = entry.path();
                    matches!(
                        persona_extension(&path).as_deref(),
                        Some("md" | "markdown" | "toml")
                    )
                })
            })
            .unwrap_or(false);
        if has_persona_file {
            return;
        }
        let path = self.dir.join("default.md");
        if let Err(e) = atomic_write(&path, DEFAULT_PERSONA_FILE.as_bytes()) {
            tracing::warn!(path = %path.display(), error = %e, "写入默认人设文件失败");
        }
    }
}

/// 文件名里不允许出现的字符替换掉。
fn sanitize_id(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for ch in id.trim().chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || (ch as u32) > 0x2E80 {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        slugify(id)
    } else {
        trimmed
    }
}

/// 校验 app 传入的人设 id：只允许 `[a-z0-9_-]`，长度 1..=64。
fn validate_persona_id(raw: &str) -> anyhow::Result<String> {
    let id = raw.trim();
    if id.is_empty() {
        bail!("人设 id 不能为空，请使用小写字母、数字、_ 或 -。");
    }
    if id.chars().count() > 64 {
        bail!("人设 id 太长（最多 64 个字符）：{id}");
    }
    let valid = id
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-');
    if !valid {
        bail!("人设 id 非法：「{id}」。只允许小写字母、数字、下划线 _ 和短横线 -。");
    }
    Ok(id.to_string())
}

fn persona_extension(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
}

fn load_persona_file(path: &Path) -> Option<Persona> {
    match persona_extension(path)?.as_str() {
        "md" | "markdown" => load_markdown(path),
        "toml" => load_toml(path),
        _ => None,
    }
}

fn load_markdown(path: &Path) -> Option<Persona> {
    let text = std::fs::read_to_string(path).ok()?;
    let (front, body) = split_front_matter(&text);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("persona")
        .trim()
        .to_string();

    let mut persona = Persona {
        id: stem.clone(),
        name: stem.clone(),
        description: String::new(),
        system_prompt: body.trim().to_string(),
        initial_memory: BTreeMap::new(),
        source_path: Some(path.display().to_string()),
    };

    if let Some(front) = front {
        let parsed = parse_front_matter(&front);
        if let Some(value) = parsed.scalars.get("id") {
            if !value.trim().is_empty() {
                persona.id = value.trim().to_string();
            }
        }
        if let Some(value) = parsed.scalars.get("name") {
            if !value.trim().is_empty() {
                persona.name = value.trim().to_string();
            }
        }
        if let Some(value) = parsed.scalars.get("description") {
            persona.description = value.trim().to_string();
        }
        if let Some(value) = parsed.scalars.get("system_prompt") {
            let value = value.trim();
            if !value.is_empty() {
                persona.system_prompt = value.to_string();
            }
        }
        persona.initial_memory = parsed.initial_memory;
    }

    if persona.id.trim().is_empty() {
        return None;
    }
    if persona.name.trim().is_empty() {
        persona.name = persona.id.clone();
    }
    Some(persona)
}

fn load_toml(path: &Path) -> Option<Persona> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: toml::Value = toml::from_str(&text).ok()?;
    let table = value.as_table()?;
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("persona")
        .trim()
        .to_string();

    let mut persona = Persona {
        id: stem.clone(),
        name: stem.clone(),
        description: String::new(),
        system_prompt: String::new(),
        initial_memory: BTreeMap::new(),
        source_path: Some(path.display().to_string()),
    };
    if let Some(id) = table.get("id").and_then(|v| v.as_str()) {
        if !id.trim().is_empty() {
            persona.id = id.trim().to_string();
        }
    }
    if let Some(name) = table.get("name").and_then(|v| v.as_str()) {
        if !name.trim().is_empty() {
            persona.name = name.trim().to_string();
        }
    }
    if let Some(description) = table.get("description").and_then(|v| v.as_str()) {
        persona.description = description.trim().to_string();
    }
    if let Some(prompt) = table.get("system_prompt").and_then(|v| v.as_str()) {
        persona.system_prompt = prompt.trim().to_string();
    }
    if let Some(memory) = table.get("initial_memory").and_then(|v| v.as_table()) {
        for (key, value) in memory {
            if let Some(value) = value.as_str() {
                persona
                    .initial_memory
                    .insert(key.clone(), value.to_string());
            }
        }
    }
    if persona.id.trim().is_empty() {
        return None;
    }
    Some(persona)
}

// ============================================================
// front-matter 解析
// ============================================================

#[derive(Default, Debug)]
pub(crate) struct ParsedFrontMatter {
    pub(crate) scalars: BTreeMap<String, String>,
    #[allow(dead_code)]
    pub(crate) initial_memory: BTreeMap<String, String>,
}

/// 拆出 `--- ... ---` 之间的 front-matter 与正文。
pub(crate) fn split_front_matter(text: &str) -> (Option<String>, String) {
    let normalized = text.trim_start_matches('\u{feff}').replace("\r\n", "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();
    let mut start = 0usize;
    while start < lines.len() && lines[start].trim().is_empty() {
        start += 1;
    }
    if start >= lines.len() || lines[start].trim() != "---" {
        return (None, normalized);
    }
    let content_start = start + 1;
    let mut content_end = None;
    for (idx, line) in lines.iter().enumerate().skip(content_start) {
        let trimmed = line.trim();
        if trimmed == "---" || trimmed == "..." {
            content_end = Some(idx);
            break;
        }
    }
    let Some(end) = content_end else {
        return (None, normalized);
    };
    let front = lines[content_start..end].join("\n");
    let mut body: Vec<&str> = lines[end + 1..].to_vec();
    while body
        .first()
        .map(|line| line.trim().is_empty())
        .unwrap_or(false)
    {
        body.remove(0);
    }
    (Some(front), body.join("\n"))
}

pub(crate) fn parse_front_matter(front: &str) -> ParsedFrontMatter {
    let lines: Vec<&str> = front.lines().collect();
    let mut parsed = ParsedFrontMatter::default();
    let mut idx = 0usize;
    while idx < lines.len() {
        let raw = lines[idx];
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            idx += 1;
            continue;
        }
        let indent = raw.len() - raw.trim_start().len();
        if indent > 0 {
            idx += 1;
            continue;
        }
        let Some((key, rest)) = split_key_value(trimmed) else {
            idx += 1;
            continue;
        };
        let key = unquote(key);

        // 顶层块标量：description: | 之类。
        if is_block_scalar(rest) {
            let mut block: Vec<String> = Vec::new();
            let mut j = idx + 1;
            while j < lines.len() {
                let line = lines[j];
                if line.trim().is_empty() {
                    block.push(String::new());
                    j += 1;
                    continue;
                }
                if line.starts_with(' ') || line.starts_with('\t') {
                    block.push(line.trim().to_string());
                    j += 1;
                } else {
                    break;
                }
            }
            parsed.scalars.insert(key, block.join("\n"));
            idx = j;
            continue;
        }

        if key == "initial_memory" && rest.trim().is_empty() {
            let mut j = idx + 1;
            let mut last_key: Option<String> = None;
            while j < lines.len() {
                let line = lines[j];
                if line.trim().is_empty() {
                    j += 1;
                    continue;
                }
                let ind = line.len() - line.trim_start().len();
                if ind == 0 {
                    break;
                }
                let content = line.trim();
                if let Some((sub_key, sub_rest)) = split_key_value(content) {
                    let sub_key = unquote(sub_key);
                    if sub_rest.trim().is_empty() || is_block_scalar(sub_rest) {
                        parsed.initial_memory.insert(sub_key.clone(), String::new());
                        last_key = Some(sub_key);
                    } else {
                        parsed.initial_memory.insert(sub_key, unquote(sub_rest));
                        last_key = None;
                    }
                } else if let Some(last) = &last_key {
                    // 块标量的续行。
                    let entry = parsed.initial_memory.entry(last.clone()).or_default();
                    if !entry.is_empty() {
                        entry.push('\n');
                    }
                    entry.push_str(content);
                }
                j += 1;
            }
            idx = j;
            continue;
        }

        parsed.scalars.insert(key, unquote(rest));
        idx += 1;
    }
    parsed
}

fn is_block_scalar(rest: &str) -> bool {
    matches!(rest.trim(), "|" | "|-" | "|+" | ">" | ">-" | ">+")
}

/// `key: value` 拆成两半（只按第一个冒号拆，值里的冒号保留）。
fn split_key_value(line: &str) -> Option<(&str, &str)> {
    let idx = line.find(':')?;
    let key = line[..idx].trim();
    if key.is_empty() {
        return None;
    }
    Some((key, line[idx + 1..].trim()))
}

fn unquote(raw: &str) -> String {
    let value = raw.trim();
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        return unescape_double_quoted(&value[1..value.len() - 1]);
    }
    if value.len() >= 2 && value.starts_with('\'') && value.ends_with('\'') {
        return value[1..value.len() - 1].replace("''", "'");
    }
    value.to_string()
}

fn unescape_double_quoted(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// 写回文件时统一用双引号 + 转义，保证能被上面的解析器读回。
pub(crate) fn quote_yaml(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 2);
    out.push('"');
    for ch in input.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (tempfile::TempDir, PersonaStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = PersonaStore::load(&dir.path().join("personas"));
        (dir, store)
    }

    #[test]
    fn load_creates_default_persona_file() {
        let (dir, store) = temp_store();
        assert!(dir.path().join("personas").join("default.md").is_file());
        let ids = store.ids();
        assert!(ids.contains(&"default".to_string()));
        let default = store.get("default");
        assert!(default.system_prompt.contains("NeoSonia"));
        assert!(!default.initial_memory.is_empty());
        // 未知 id 回退到 default。
        assert_eq!(store.get("不存在的人设").id, "default");
    }

    #[test]
    fn create_save_delete_persona_api() {
        let (_dir, store) = temp_store();

        let created = store
            .create("coder", "程序员", "写代码用", "你是程序员，先读代码再改。")
            .unwrap();
        assert_eq!(created.id, "coder");
        assert_eq!(created.name, "程序员");
        assert_eq!(created.system_prompt, "你是程序员，先读代码再改。");
        assert_eq!(store.get("coder").name, "程序员");
        // 创建后落盘，重新加载仍在。
        let reloaded = PersonaStore::load(store.dir());
        assert_eq!(reloaded.get("coder").description, "写代码用");

        // 重名、非法 id、空正文都要报中文错误。
        assert!(store.create("coder", "x", "", "正文").is_err());
        assert!(store.create("Bad ID", "x", "", "正文").is_err());
        assert!(store.create("", "x", "", "正文").is_err());
        assert!(store.create("ok_id", "x", "", "   ").is_err());
        // 内置 default 也不能被 create 覆盖。
        assert!(store.create("default", "x", "", "正文").is_err());

        // 保存覆盖。
        let saved = store.save("coder", "新名字", "新描述", "新的正文").unwrap();
        assert_eq!(saved.name, "新名字");
        assert_eq!(saved.description, "新描述");
        assert_eq!(saved.system_prompt, "新的正文");
        // 保存不存在的人设报错。
        assert!(store.save("nobody", "x", "", "正文").is_err());

        // 删除：内置保护 + 正常删除 + 再删返回 false。
        assert!(store.delete("default").is_err());
        assert!(store.delete("coder").unwrap());
        assert!(!store.delete("coder").unwrap());
        assert!(!store.ids().contains(&"coder".to_string()));
    }

    #[test]
    fn parse_markdown_persona() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("coder.md");
        std::fs::write(
            &path,
            "---\nid: coder\nname: \"代码助手\"\ndescription: \"专注 Rust\"\n\
             initial_memory:\n  style: \"先看代码再改\"\n  lang: \"Rust\"\n---\n\n\
             你是一个严谨的 Rust 工程师。\n第二行。\n",
        )
        .unwrap();
        let persona = load_persona_file(&path).unwrap();
        assert_eq!(persona.id, "coder");
        assert_eq!(persona.name, "代码助手");
        assert_eq!(persona.description, "专注 Rust");
        assert!(
            persona
                .system_prompt
                .starts_with("你是一个严谨的 Rust 工程师。")
        );
        assert_eq!(
            persona.initial_memory.get("lang").map(|s| s.as_str()),
            Some("Rust")
        );
        assert!(persona.source_path.is_some());
    }

    #[test]
    fn parse_markdown_without_front_matter_uses_stem() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("简洁.md");
        std::fs::write(&path, "只做一件事：回答问题。\n").unwrap();
        let persona = load_persona_file(&path).unwrap();
        assert_eq!(persona.id, "简洁");
        assert_eq!(persona.system_prompt, "只做一件事：回答问题。");
    }

    #[test]
    fn parse_toml_persona() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("writer.toml");
        std::fs::write(
            &path,
            "id = \"writer\"\nname = \"写作助手\"\ndescription = \"润色\"\n\
             system_prompt = \"你负责润色中文。\"\n\n[initial_memory]\nstyle = \"克制\"\n",
        )
        .unwrap();
        let persona = load_persona_file(&path).unwrap();
        assert_eq!(persona.id, "writer");
        assert_eq!(persona.name, "写作助手");
        assert_eq!(persona.system_prompt, "你负责润色中文。");
        assert_eq!(
            persona.initial_memory.get("style").map(|s| s.as_str()),
            Some("克制")
        );
    }

    #[test]
    fn save_roundtrip_handles_quotes_and_newlines() {
        let (_dir, store) = temp_store();
        let mut persona = store.get("default");
        persona.id = "test round trip".to_string();
        persona.name = "测试 \"人设\"".to_string();
        persona.description = "带: 冒号 的描述".to_string();
        persona.system_prompt = "第一行\n第二行带 \"引号\" 和 \\ 反斜杠".to_string();
        persona
            .initial_memory
            .insert("note".to_string(), "多行\n记忆".to_string());

        let path = store.save_persona(&persona).unwrap();
        assert_eq!(
            path.file_name().and_then(|s| s.to_str()),
            Some("test-round-trip.md")
        );

        store.reload();
        let loaded = store.get("test round trip");
        assert_eq!(loaded.name, "测试 \"人设\"");
        assert_eq!(loaded.description, "带: 冒号 的描述");
        assert_eq!(loaded.system_prompt, persona.system_prompt);
        assert_eq!(
            loaded.initial_memory.get("note").map(|s| s.as_str()),
            Some("多行\n记忆")
        );
    }

    #[test]
    fn builtin_persona_is_complete() {
        let persona = builtin_persona();
        assert_eq!(persona.id, "default");
        assert!(persona.system_prompt.contains("先行动，后解释"));
        assert!(persona.initial_memory.contains_key("style"));
        // 不允许出现 emoji：用最粗略的范围检查（CJK 之外的符号区）。
        assert!(persona.system_prompt.chars().all(|c| (c as u32) < 0x1F000));
    }

    #[test]
    fn quote_and_unquote_roundtrip() {
        let input = "a\"b\\c\nd\te";
        assert_eq!(unquote(&quote_yaml(input)), input);
        assert_eq!(unquote("bare"), "bare");
        assert_eq!(unquote("'single'"), "single");
    }

    #[test]
    fn split_front_matter_variants() {
        let (front, body) = split_front_matter("---\nid: a\n---\n正文");
        assert_eq!(front.as_deref(), Some("id: a"));
        assert_eq!(body, "正文");

        let (front, body) = split_front_matter("没有 front matter");
        assert!(front.is_none());
        assert_eq!(body, "没有 front matter");

        // 没闭合时整体当正文，不 panic。
        let (front, body) = split_front_matter("---\nid: a\n正文");
        assert!(front.is_none());
        assert!(body.contains("正文"));
    }
}
