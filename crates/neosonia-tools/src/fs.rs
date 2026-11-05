//! 文件工具：读写、编辑、目录、搜索、元信息、复制/移动/删除。
//!
//! 约定：
//! - 相对路径相对工作目录（`ToolCtx::workspace()`）。
//! - 读操作不限制路径；写操作默认限制在工作目录内，除非 `allow_outside=true`。
//! - 写文件走原子写（临时文件 + rename），避免半截文件。

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use neosonia_core::model::{ToolCtx, ToolOutcome};
use neosonia_core::registry::ToolRegistry;
use neosonia_core::util::{human_bytes, truncate_chars};
use regex::{Regex, RegexBuilder};
use serde_json::{json, Value};
use walkdir::WalkDir;

use crate::{
    arg_bool, arg_str, arg_str_req, arg_u64, io_error_hint, resolve_read_path, resolve_write_path,
};

/// 内容搜索时单文件读取上限（更大的文件跳过）。
const CONTENT_MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// 内容搜索扫描时间上限，避免在巨型目录树上挂死。
const SEARCH_TIME_BUDGET: Duration = Duration::from_secs(120);
/// 目录树最大深度。
const WALK_MAX_DEPTH: usize = 24;

/// 搜索/列表默认跳过的目录名。
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    "target",
    "dist",
    "build",
    ".venv",
    "venv",
    "__pycache__",
    ".idea",
    ".vscode",
    "AppData",
    ".next",
    ".nuxt",
    ".cache",
    "coverage",
    ".gradle",
    ".m2",
    "Pods",
    "vendor",
    ".svelte-kit",
    "out",
];

pub fn register(reg: &ToolRegistry) {
    reg.register(Arc::new(ReadFile));
    reg.register(Arc::new(WriteFile));
    reg.register(Arc::new(EditFile));
    reg.register(Arc::new(AppendFile));
    reg.register(Arc::new(ListDir));
    reg.register(Arc::new(SearchFiles));
    reg.register(Arc::new(FileInfo));
    reg.register(Arc::new(MakeDir));
    reg.register(Arc::new(CopyPath));
    reg.register(Arc::new(MovePath));
    reg.register(Arc::new(DeletePath));
}

// ============================================================
// 通用：编码识别 / 二进制判定 / 行号渲染
// ============================================================

/// 把字节解码成文本：BOM 优先，UTF-8 次之，失败回退 GBK。
pub(crate) fn decode_text(bytes: &[u8]) -> (String, &'static str) {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return (
            String::from_utf8_lossy(&bytes[3..]).into_owned(),
            "UTF-8 (BOM)",
        );
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        return (String::from_utf16_lossy(&units), "UTF-16 LE");
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        return (String::from_utf16_lossy(&units), "UTF-16 BE");
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => (s.to_string(), "UTF-8"),
        Err(_) => {
            let (decoded, _, had_errors) = encoding_rs::GBK.decode(bytes);
            if !had_errors {
                (decoded.into_owned(), "GBK")
            } else {
                (String::from_utf8_lossy(bytes).into_owned(), "UTF-8 (有损坏字节)")
            }
        }
    }
}

/// 粗略判断是否二进制：前 8KB 里出现 NUL 或大量不可打印字节。
pub(crate) fn is_binary(bytes: &[u8]) -> bool {
    let probe = &bytes[..bytes.len().min(8192)];
    if probe.is_empty() {
        return false;
    }
    if probe.contains(&0u8) {
        return true;
    }
    let mut weird = 0usize;
    for &b in probe {
        // 允许 tab / \n / \r / 正常可见字符 / UTF-8 高位字节
        if b < 0x09 || (0x0E..0x20).contains(&b) {
            weird += 1;
        }
    }
    weird * 100 / probe.len() > 10
}

fn normalize_newlines(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

/// 行号右对齐渲染。
fn render_lines(lines: &[&str], start: usize, total: usize) -> String {
    let width = total.to_string().len().max(3);
    let mut out = String::new();
    for (idx, line) in lines.iter().enumerate() {
        let n = start + idx;
        out.push_str(&format!("{:>width$}: {}\n", n, line, width = width));
    }
    out
}

fn first_lines_preview(text: &str, max_lines: usize, max_chars: usize) -> String {
    let mut out = String::new();
    for (i, line) in text.lines().take(max_lines).enumerate() {
        out.push_str(&format!("{:>4}: {}\n", i + 1, line));
        if out.chars().count() > max_chars {
            out = truncate_chars(&out, max_chars);
            break;
        }
    }
    out
}

// ============================================================
// read_file
// ============================================================

async fn read_file(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    let raw = match arg_str_req(&args, "path") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let path = match resolve_read_path(ctx, &raw) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) => {
            return ToolOutcome::err(format!(
                "无法读取 {}：{}。\n相对路径相对工作目录 {}。",
                path.display(),
                io_error_hint(&e),
                ctx.workspace().display()
            ))
        }
    };
    if meta.is_dir() {
        return ToolOutcome::err(format!(
            "{} 是目录，不是文件；查看目录请用 list_dir。",
            path.display()
        ));
    }
    if meta.len() > 64 * 1024 * 1024 {
        return ToolOutcome::err(format!(
            "文件太大（{}），超过 64MB 不按文本读取；可用 file_info 查看，或复制/分段处理。",
            human_bytes(meta.len())
        ));
    }
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            return ToolOutcome::err(format!(
                "读取 {} 失败：{}",
                path.display(),
                io_error_hint(&e)
            ))
        }
    };
    if is_binary(&bytes) {
        return ToolOutcome::err(format!(
            "{} 看起来是二进制文件（{}），不能按文本读取。可改用 file_info 查看元信息。",
            path.display(),
            human_bytes(meta.len())
        ));
    }
    let (text, encoding) = decode_text(&bytes);
    let text = normalize_newlines(&text);
    let all_lines: Vec<&str> = text.split('\n').collect();
    let total = all_lines.len();
    let start_line = arg_u64(&args, "start_line", 1).max(1) as usize;
    let end_line = arg_u64(&args, "end_line", total as u64).max(1) as usize;
    let max_chars = arg_u64(&args, "max_chars", 20_000).max(200) as usize;

    if start_line > total {
        return ToolOutcome::ok(format!(
            "文件：{}\n编码：{}\n总行数：{}\n起始行 {} 超过文件总行数，没有内容可显示。",
            path.display(),
            encoding,
            total,
            start_line
        ))
        .with_display(format!("共 {total} 行，起始行越界"));
    }
    let end_line = end_line.min(total);
    if end_line < start_line {
        return ToolOutcome::err(format!(
            "end_line={} 小于 start_line={}，无法读取。",
            end_line, start_line
        ));
    }
    let slice = &all_lines[(start_line - 1)..end_line];
    let numbered = render_lines(slice, start_line, total);
    let (body, truncated) = if numbered.chars().count() > max_chars {
        (
            truncate_chars(&numbered, max_chars),
            true,
        )
    } else {
        (numbered, false)
    };

    let mut out = format!(
        "文件：{}\n编码：{}\n总行数：{}\n显示：第 {}-{} 行（{} 行）\n\n{}",
        path.display(),
        encoding,
        total,
        start_line,
        end_line,
        slice.len(),
        body
    );
    if truncated {
        out.push_str(&format!(
            "\n[已截断：内容超过 max_chars={} 字符。可用 start_line/end_line 分段读取。]\n",
            max_chars
        ));
    }
    ToolOutcome::ok(out).with_display(format!(
        "{} 第 {}-{} 行 / 共 {} 行",
        path.display(),
        start_line,
        end_line,
        total
    ))
}

// ============================================================
// write_file / append_file
// ============================================================

async fn write_file(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    let raw = match arg_str_req(&args, "path") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let content = match arg_str(&args, "content") {
        Some(c) => c,
        None => {
            return ToolOutcome::err(
                "缺少必填参数 content（要写入的文本内容，可为空字符串）。".to_string(),
            )
        }
    };
    let allow_outside = arg_bool(&args, "allow_outside", false);
    let create_dirs = arg_bool(&args, "create_dirs", true);
    let overwrite = arg_bool(&args, "overwrite", true);
    let path = match resolve_write_path(ctx, &raw, allow_outside) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let old_size = std::fs::metadata(&path).ok().map(|m| m.len());
    if old_size.is_some() && !overwrite {
        return ToolOutcome::err(format!(
            "{} 已存在（{}），overwrite=false 时不会覆盖。",
            path.display(),
            human_bytes(old_size.unwrap_or(0))
        ));
    }
    if let Some(parent) = path.parent() {
        if create_dirs {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return ToolOutcome::err(format!(
                    "创建目录 {} 失败：{}",
                    parent.display(),
                    io_error_hint(&e)
                ));
            }
        } else if !parent.as_os_str().is_empty() && !parent.exists() {
            return ToolOutcome::err(format!(
                "目录 {} 不存在，且 create_dirs=false。请先 make_dir 或把 create_dirs 设为 true。",
                parent.display()
            ));
        }
    }
    let bytes = content.as_bytes();
    if let Err(e) = neosonia_core::util::atomic_write(&path, bytes) {
        return ToolOutcome::err(format!("写入 {} 失败：{e}", path.display()));
    }
    let line_count = content.lines().count();
    let mut out = format!(
        "[ok] 已写入 {}\n字节数：{}（{}）\n行数：{}\n",
        path.display(),
        bytes.len(),
        human_bytes(bytes.len() as u64),
        line_count
    );
    match old_size {
        Some(old) => out.push_str(&format!(
            "覆盖：原 {} -> 现 {}",
            human_bytes(old),
            human_bytes(bytes.len() as u64)
        )),
        None => out.push_str("新建文件。"),
    }
    ToolOutcome::ok(out).with_display(format!("写入 {} 字节", bytes.len()))
}

async fn append_file(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    let raw = match arg_str_req(&args, "path") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let content = match arg_str(&args, "content") {
        Some(c) => c,
        None => return ToolOutcome::err("缺少必填参数 content（要追加的文本）。".to_string()),
    };
    let path = match resolve_write_path(ctx, &raw, false) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return ToolOutcome::err(format!(
                    "创建目录 {} 失败：{}",
                    parent.display(),
                    io_error_hint(&e)
                ));
            }
        }
    }
    let existed = path.is_file();
    let old_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let needs_newline = existed && old_size > 0 && !content.starts_with('\n');
    let payload = if needs_newline {
        format!("\n{content}")
    } else {
        content.clone()
    };
    use std::io::Write;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path);
    let mut file = match file {
        Ok(f) => f,
        Err(e) => {
            return ToolOutcome::err(format!(
                "打开 {} 失败：{}",
                path.display(),
                io_error_hint(&e)
            ))
        }
    };
    if let Err(e) = file.write_all(payload.as_bytes()).and_then(|_| file.flush()) {
        return ToolOutcome::err(format!(
            "追加写入 {} 失败：{}",
            path.display(),
            io_error_hint(&e)
        ));
    }
    let new_size = old_size + payload.len() as u64;
    ToolOutcome::ok(format!(
        "[ok] 已追加到 {}\n追加：{} 字节\n文件现在：{}",
        path.display(),
        payload.len(),
        human_bytes(new_size)
    ))
    .with_display(format!("追加 {} 字节", payload.len()))
}

// ============================================================
// edit_file
// ============================================================

async fn edit_file(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    let raw = match arg_str_req(&args, "path") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let old_string = match arg_str(&args, "old_string") {
        Some(s) => s,
        None => return ToolOutcome::err("缺少必填参数 old_string（要被替换的原文）。".to_string()),
    };
    let new_string = match arg_str(&args, "new_string") {
        Some(s) => s,
        None => return ToolOutcome::err("缺少必填参数 new_string（替换后的文本）。".to_string()),
    };
    if old_string.is_empty() {
        return ToolOutcome::err("old_string 不能为空字符串。".to_string());
    }
    let replace_all = arg_bool(&args, "replace_all", false);
    let path = match resolve_write_path(ctx, &raw, false) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if !path.is_file() {
        return ToolOutcome::err(format!(
            "{} 不存在或不是文件；新建文件请用 write_file。",
            path.display()
        ));
    }
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            return ToolOutcome::err(format!(
                "读取 {} 失败：{}",
                path.display(),
                io_error_hint(&e)
            ))
        }
    };
    if is_binary(&bytes) {
        return ToolOutcome::err(format!(
            "{} 看起来是二进制文件，不支持 edit_file。",
            path.display()
        ));
    }
    let (text, encoding) = decode_text(&bytes);
    if text.contains('\0') {
        return ToolOutcome::err(format!("{} 含 NUL 字节，不支持文本编辑。", path.display()));
    }
    let count = text.matches(&old_string).count();
    if count == 0 {
        let preview = first_lines_preview(&text, 40, 1500);
        return ToolOutcome::err(format!(
            "在 {} 里没有找到 old_string，未做修改。\n\
             提示：old_string 必须与文件内容逐字符一致（包括缩进和换行）；\
             可先用 read_file 确认。\n\n--- 文件前部内容 ---\n{}",
            path.display(),
            preview
        ));
    }
    if count > 1 && !replace_all {
        let mut positions = Vec::new();
        for (idx, line) in text.lines().enumerate() {
            if line.contains(&old_string) && positions.len() < 10 {
                positions.push(idx + 1);
            }
        }
        let where_hint = if positions.is_empty() {
            String::new()
        } else {
            format!(
                "\n出现位置（行号）：{}",
                positions
                    .iter()
                    .map(|n| n.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        return ToolOutcome::err(format!(
            "old_string 在 {} 里出现 {} 次，不唯一，未做修改。{}\n\
             请提供更长的上下文使其唯一，或传 replace_all=true 全部替换。",
            path.display(),
            count,
            where_hint
        ));
    }
    let replaced = if replace_all {
        text.replace(&old_string, &new_string)
    } else {
        text.replacen(&old_string, &new_string, 1)
    };
    if let Err(e) = neosonia_core::util::atomic_write(&path, replaced.as_bytes()) {
        return ToolOutcome::err(format!("写回 {} 失败：{e}", path.display()));
    }
    let n = if replace_all { count } else { 1 };
    let mut out = format!(
        "[ok] 已在 {} 替换 {} 处\n旧：{} 字节 -> 新：{} 字节",
        path.display(),
        n,
        bytes.len(),
        replaced.len()
    );
    if encoding != "UTF-8" {
        out.push_str(&format!(
            "\n注意：原文件编码为 {encoding}，已按 UTF-8 写回。"
        ));
    }
    ToolOutcome::ok(out).with_display(format!("替换 {n} 处"))
}

// ============================================================
// list_dir
// ============================================================

async fn list_dir(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    let raw = arg_str(&args, "path").unwrap_or_else(|| ".".to_string());
    let recursive = arg_bool(&args, "recursive", false);
    let max_entries = arg_u64(&args, "max_entries", 200).clamp(1, 5000) as usize;
    let show_hidden = arg_bool(&args, "show_hidden", false);
    let root = match resolve_read_path(ctx, &raw) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if !root.exists() {
        return ToolOutcome::err(format!(
            "目录不存在：{}\n相对路径相对工作目录 {}。",
            root.display(),
            ctx.workspace().display()
        ));
    }
    if !root.is_dir() {
        return ToolOutcome::err(format!(
            "{} 不是目录；查看文件请用 read_file / file_info。",
            root.display()
        ));
    }

    let mut lines: Vec<String> = Vec::new();
    let mut truncated = false;

    if recursive {
        let walker = WalkDir::new(&root)
            .max_depth(WALK_MAX_DEPTH)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| show_hidden || !is_hidden_entry(e.file_name().to_string_lossy().as_ref(), e.file_type().is_dir()));
        for entry in walker {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            if entry.path() == root {
                continue;
            }
            let rel = entry.path().strip_prefix(&root).unwrap_or(entry.path());
            let rel_disp = rel.to_string_lossy().replace('\\', "/");
            if entry.file_type().is_dir() {
                lines.push(format!("[D] {rel_disp}/"));
            } else {
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                lines.push(format!("[F] {rel_disp}  ({})", human_bytes(size)));
            }
            if lines.len() >= max_entries {
                truncated = true;
                break;
            }
        }
    } else {
        let mut entries: Vec<(bool, String, u64)> = Vec::new();
        let rd = match std::fs::read_dir(&root) {
            Ok(rd) => rd,
            Err(e) => {
                return ToolOutcome::err(format!(
                    "读取目录 {} 失败：{}",
                    root.display(),
                    io_error_hint(&e)
                ))
            }
        };
        for ent in rd.flatten() {
            let name = ent.file_name().to_string_lossy().to_string();
            let ft = match ent.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if !show_hidden && name.starts_with('.') {
                continue;
            }
            if ft.is_dir() {
                let count = std::fs::read_dir(ent.path()).map(|it| it.count()).unwrap_or(0);
                entries.push((true, name, count as u64));
            } else {
                let size = ent.metadata().map(|m| m.len()).unwrap_or(0);
                entries.push((false, name, size));
            }
        }
        entries.sort_by(|a, b| match (a.0, b.0) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.1.to_lowercase().cmp(&b.1.to_lowercase()),
        });
        for (is_dir, name, num) in entries {
            if is_dir {
                lines.push(format!("[D] {name}/  ({num} 项)"));
            } else {
                lines.push(format!("[F] {name}  ({})", human_bytes(num)));
            }
            if lines.len() >= max_entries {
                truncated = true;
                break;
            }
        }
    }

    let mut out = format!(
        "目录：{}\n模式：{}\n条目数：{}{}\n\n",
        root.display(),
        if recursive { "递归" } else { "单层" },
        lines.len(),
        if truncated {
            format!("（已达到 max_entries={max_entries}，还有更多未显示）")
        } else {
            String::new()
        }
    );
    if lines.is_empty() {
        out.push_str("（空目录）\n");
    } else {
        for line in &lines {
            out.push_str(line);
            out.push('\n');
        }
    }
    ToolOutcome::ok(out).with_display(format!("列出 {} 项", lines.len()))
}

fn is_hidden_entry(name: &str, is_dir: bool) -> bool {
    if name.starts_with('.') {
        return true;
    }
    is_dir && SKIP_DIRS.iter().any(|d| d.eq_ignore_ascii_case(name))
}

// ============================================================
// search_files —— 完全自实现：walkdir + globset + regex，不依赖 Everything
// ============================================================

enum TextMatcher {
    Substr { needle: String, case_insensitive: bool },
    Regex(Regex),
}

impl TextMatcher {
    fn build(query: &str, use_regex: bool, case_sensitive: bool) -> Result<Self, String> {
        if use_regex {
            let re = RegexBuilder::new(query)
                .case_insensitive(!case_sensitive)
                .build()
                .map_err(|e| format!("正则表达式非法：{e}"))?;
            Ok(TextMatcher::Regex(re))
        } else {
            Ok(TextMatcher::Substr {
                needle: if case_sensitive {
                    query.to_string()
                } else {
                    query.to_lowercase()
                },
                case_insensitive: !case_sensitive,
            })
        }
    }

    fn is_match(&self, text: &str) -> bool {
        match self {
            TextMatcher::Substr {
                needle,
                case_insensitive,
            } => {
                if *case_insensitive {
                    text.to_lowercase().contains(needle)
                } else {
                    text.contains(needle)
                }
            }
            TextMatcher::Regex(re) => re.is_match(text),
        }
    }
}

fn should_skip_dir(name: &str) -> bool {
    if name.starts_with('.') && name != "." {
        return true;
    }
    SKIP_DIRS.iter().any(|d| d.eq_ignore_ascii_case(name))
}

fn build_glob_set(glob: Option<&str>) -> Result<Option<globset::GlobSet>, String> {
    let Some(glob) = glob else {
        return Ok(None);
    };
    let mut builder = globset::GlobSetBuilder::new();
    let mut any = false;
    for pat in glob.split([',', ';']).map(str::trim).filter(|s| !s.is_empty()) {
        let g = globset::Glob::new(pat).map_err(|e| format!("glob 模式 {pat:?} 非法：{e}"))?;
        builder.add(g);
        any = true;
    }
    if !any {
        return Ok(None);
    }
    builder
        .build()
        .map(Some)
        .map_err(|e| format!("构建 glob 失败：{e}"))
}

async fn search_files(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    let query = match arg_str_req(&args, "query") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let root_raw = arg_str(&args, "path").unwrap_or_else(|| ".".to_string());
    let mode = arg_str(&args, "mode")
        .unwrap_or_else(|| "name".to_string())
        .to_ascii_lowercase();
    if mode != "name" && mode != "content" {
        return ToolOutcome::err(format!(
            "mode 只能是 name 或 content，收到 {mode:?}。"
        ));
    }
    let max_results = arg_u64(&args, "max_results", 50).clamp(1, 500) as usize;
    let case_sensitive = arg_bool(&args, "case_sensitive", false);
    let use_regex = arg_bool(&args, "regex", false);
    let glob_raw = arg_str(&args, "glob");
    let root = match resolve_read_path(ctx, &root_raw) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if !root.is_dir() {
        return ToolOutcome::err(format!(
            "搜索目录不存在或不是目录：{}",
            root.display()
        ));
    }
    let globs = match build_glob_set(glob_raw.as_deref()) {
        Ok(g) => g,
        Err(e) => return ToolOutcome::err(e),
    };
    let matcher = match TextMatcher::build(&query, use_regex, case_sensitive) {
        Ok(m) => m,
        Err(e) => return ToolOutcome::err(e),
    };
    // name 模式下如果 query 本身像 glob（含 * ?），按 glob 匹配文件名。
    let name_glob = if mode == "name" && !use_regex && (query.contains('*') || query.contains('?')) {
        match globset::Glob::new(&query) {
            Ok(g) => Some(g.compile_matcher()),
            Err(_) => None,
        }
    } else {
        None
    };

    let started = Instant::now();
    let mut scanned = 0usize;
    let mut results: Vec<String> = Vec::new();
    let mut stop_reason: Option<&'static str> = None;

    let walker = WalkDir::new(&root)
        .max_depth(WALK_MAX_DEPTH)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            if e.depth() == 0 {
                return true;
            }
            let name = e.file_name().to_string_lossy().to_string();
            if e.file_type().is_dir() {
                !should_skip_dir(&name)
            } else {
                true
            }
        });

    'walk: for entry in walker {
        if ctx.is_cancelled() {
            return ToolOutcome::err("搜索已取消（用户中断）。".to_string());
        }
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        scanned += 1;
        if scanned % 256 == 0 && started.elapsed() > SEARCH_TIME_BUDGET {
            stop_reason = Some("扫描超过 120 秒，已提前结束");
            break;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let rel = path.strip_prefix(&root).unwrap_or(path);
        if let Some(gs) = &globs {
            if !gs.is_match(rel) {
                continue;
            }
        }
        let file_name = entry.file_name().to_string_lossy().to_string();
        if mode == "name" {
            let hit = match &name_glob {
                Some(g) => g.is_match(file_name.as_str()),
                None => matcher.is_match(&file_name),
            };
            if hit {
                results.push(path.display().to_string());
                if results.len() >= max_results {
                    break 'walk;
                }
            }
        } else {
            let too_big = entry
                .metadata()
                .map(|m| m.len() > CONTENT_MAX_FILE_BYTES)
                .unwrap_or(true);
            if too_big {
                continue;
            }
            let bytes = match std::fs::read(path) {
                Ok(b) => b,
                Err(_) => continue,
            };
            if is_binary(&bytes) {
                continue;
            }
            let (text, _) = decode_text(&bytes);
            for (idx, line) in text.lines().enumerate() {
                if matcher.is_match(line) {
                    let snippet = truncate_chars(line.trim(), 300);
                    results.push(format!("{}:{}: {}", path.display(), idx + 1, snippet));
                    if results.len() >= max_results {
                        break 'walk;
                    }
                }
            }
        }
    }

    let elapsed = started.elapsed();
    let mut out = format!(
        "搜索：{}\n目录：{}\n模式：{}{}{}\n命中：{} 条（扫描 {} 个条目，耗时 {:.1}s）\n\n",
        query,
        root.display(),
        mode,
        glob_raw
            .as_deref()
            .map(|g| format!("，glob={g}"))
            .unwrap_or_default(),
        if use_regex { "，正则" } else { "" },
        results.len(),
        scanned,
        elapsed.as_secs_f64()
    );
    if results.is_empty() {
        out.push_str("未找到匹配项。可检查 query 拼写、改用 mode=content，或放宽 glob。\n");
    } else {
        for (i, r) in results.iter().enumerate() {
            out.push_str(&format!("{}. {}\n", i + 1, r));
        }
    }
    if let Some(reason) = stop_reason {
        out.push_str(&format!("\n[注意] {reason}\n"));
    }
    if results.len() >= max_results {
        out.push_str(&format!(
            "\n[注意] 已达到 max_results={max_results}，可能还有更多结果。\n"
        ));
    }
    ToolOutcome::ok(out).with_display(format!("命中 {} 条", results.len()))
}

// ============================================================
// file_info
// ============================================================

fn format_time(st: std::io::Result<std::time::SystemTime>) -> String {
    match st {
        Ok(t) => {
            let dt: chrono::DateTime<chrono::Local> = t.into();
            dt.format("%Y-%m-%d %H:%M:%S").to_string()
        }
        Err(_) => "（未知）".to_string(),
    }
}

async fn file_info(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    let raw = match arg_str_req(&args, "path") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let path = match resolve_read_path(ctx, &raw) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let sym = std::fs::symlink_metadata(&path);
    let meta = match sym {
        Ok(m) => m,
        Err(e) => {
            return ToolOutcome::err(format!(
                "无法获取 {} 的信息：{}",
                path.display(),
                io_error_hint(&e)
            ))
        }
    };
    let ft = meta.file_type();
    let kind = if ft.is_symlink() {
        "符号链接"
    } else if ft.is_dir() {
        "目录"
    } else {
        "文件"
    };
    let mut out = format!(
        "路径：{}\n类型：{}\n大小：{}（{} 字节）\n修改时间：{}\n创建时间：{}\n只读：{}\n",
        path.display(),
        kind,
        human_bytes(meta.len()),
        meta.len(),
        format_time(meta.modified()),
        format_time(meta.created()),
        if meta.permissions().readonly() { "是" } else { "否" }
    );
    if ft.is_dir() {
        let mut n_files = 0u64;
        let mut n_dirs = 0u64;
        if let Ok(rd) = std::fs::read_dir(&path) {
            for ent in rd.flatten() {
                match ent.file_type() {
                    Ok(t) if t.is_dir() => n_dirs += 1,
                    Ok(_) => n_files += 1,
                    Err(_) => {}
                }
            }
        }
        out.push_str(&format!("直接子项：{n_files} 个文件，{n_dirs} 个目录\n"));
    }
    if ft.is_symlink() {
        if let Ok(target) = std::fs::read_link(&path) {
            out.push_str(&format!("链接目标：{}\n", target.display()));
        }
    }
    out.push_str(&format!(
        "工作目录内：{}\n",
        if crate::within_workspace(ctx, &path) {
            "是"
        } else {
            "否（写操作需要 allow_outside=true）"
        }
    ));
    ToolOutcome::ok(out).with_display(format!("{kind} {}", human_bytes(meta.len())))
}

// ============================================================
// make_dir / copy_path / move_path / delete_path
// ============================================================

async fn make_dir(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    let raw = match arg_str_req(&args, "path") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let recursive = arg_bool(&args, "recursive", true);
    let path = match resolve_write_path(ctx, &raw, false) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if path.is_dir() {
        return ToolOutcome::ok(format!("[ok] 目录已存在：{}", path.display()))
            .with_display("目录已存在".to_string());
    }
    let res = if recursive {
        std::fs::create_dir_all(&path)
    } else {
        std::fs::create_dir(&path)
    };
    match res {
        Ok(()) => {
            ToolOutcome::ok(format!("[ok] 已创建目录：{}", path.display()))
                .with_display("已创建目录".to_string())
        }
        Err(e) => ToolOutcome::err(format!(
            "创建目录 {} 失败：{}",
            path.display(),
            io_error_hint(&e)
        )),
    }
}

fn copy_dir_recursive(from: &Path, to: &Path, cancelled: &dyn Fn() -> bool) -> Result<(u64, u64), String> {
    let mut files = 0u64;
    let mut bytes = 0u64;
    for entry in WalkDir::new(from).follow_links(false).into_iter() {
        if cancelled() {
            return Err("复制已取消（用户中断）。".to_string());
        }
        let entry = match entry {
            Ok(e) => e,
            Err(e) => return Err(format!("遍历 {} 失败：{e}", from.display())),
        };
        let rel = entry.path().strip_prefix(from).unwrap_or(entry.path());
        let target = to.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target)
                .map_err(|e| format!("创建目录 {} 失败：{e}", target.display()))?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("创建目录 {} 失败：{e}", parent.display()))?;
            }
            let n = std::fs::copy(entry.path(), &target)
                .map_err(|e| format!("复制 {} 失败：{e}", entry.path().display()))?;
            files += 1;
            bytes += n;
            if files > 200_000 {
                return Err("复制文件数超过 20 万，已中止。".to_string());
            }
        }
    }
    Ok((files, bytes))
}

async fn copy_path(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    let from_raw = match arg_str_req(&args, "from") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let to_raw = match arg_str_req(&args, "to") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let allow_outside = arg_bool(&args, "allow_outside", false);
    let from = match resolve_read_path(ctx, &from_raw) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let to = match resolve_write_path(ctx, &to_raw, allow_outside) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if !from.exists() {
        return ToolOutcome::err(format!("源路径不存在：{}", from.display()));
    }
    if to.exists() {
        return ToolOutcome::err(format!(
            "目标已存在：{}。copy_path 不覆盖，请先 delete_path 或换目标名。",
            to.display()
        ));
    }
    let from_norm = neosonia_core::util::normalize_lexical(&from);
    let to_norm = neosonia_core::util::normalize_lexical(&to);
    if from_norm == to_norm {
        return ToolOutcome::err("from 与 to 是同一个路径。".to_string());
    }
    if to_norm.starts_with(&from_norm) {
        return ToolOutcome::err("不能把目录复制到它自己的子目录里。".to_string());
    }
    let ctx_for_task = ctx.clone();
    let from2 = from.clone();
    let to2 = to.clone();
    let joined = tokio::task::spawn_blocking(move || {
        let cancelled = || ctx_for_task.is_cancelled();
        if from2.is_dir() {
            copy_dir_recursive(&from2, &to2, &cancelled)
        } else {
            if let Some(parent) = to2.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("创建目录 {} 失败：{e}", parent.display()))?;
            }
            let n = std::fs::copy(&from2, &to2)
                .map_err(|e| format!("复制 {} 失败：{e}", from2.display()))?;
            Ok((1u64, n))
        }
    })
    .await;
    match joined {
        Ok(Ok((files, bytes))) => ToolOutcome::ok(format!(
            "[ok] 已复制\n从：{}\n到：{}\n文件：{} 个，共 {}",
            from.display(),
            to.display(),
            files,
            human_bytes(bytes)
        ))
        .with_display(format!("复制 {} 个文件", files)),
        Ok(Err(e)) => ToolOutcome::err(e),
        Err(e) => ToolOutcome::err(format!("复制任务异常：{e}")),
    }
}

async fn move_path(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    let from_raw = match arg_str_req(&args, "from") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let to_raw = match arg_str_req(&args, "to") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let allow_outside = arg_bool(&args, "allow_outside", false);
    let from = match resolve_read_path(ctx, &from_raw) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let to = match resolve_write_path(ctx, &to_raw, allow_outside) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if !from.exists() {
        return ToolOutcome::err(format!("源路径不存在：{}", from.display()));
    }
    if to.exists() {
        return ToolOutcome::err(format!(
            "目标已存在：{}。move_path 不覆盖，请先删除目标。",
            to.display()
        ));
    }
    if let Some(parent) = to.parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return ToolOutcome::err(format!(
                    "创建目录 {} 失败：{}",
                    parent.display(),
                    io_error_hint(&e)
                ));
            }
        }
    }
    match std::fs::rename(&from, &to) {
        Ok(()) => ToolOutcome::ok(format!(
            "[ok] 已移动\n从：{}\n到：{}",
            from.display(),
            to.display()
        ))
        .with_display("已移动".to_string()),
        Err(rename_err) => {
            // 跨卷等情况：退化成复制 + 删除。
            let from2 = from.clone();
            let to2 = to.clone();
            let ctx_for_task = ctx.clone();
            let joined = tokio::task::spawn_blocking(move || {
                let cancelled = || ctx_for_task.is_cancelled();
                if from2.is_dir() {
                    copy_dir_recursive(&from2, &to2, &cancelled)?;
                    std::fs::remove_dir_all(&from2)
                        .map_err(|e| format!("删除源目录 {} 失败：{e}", from2.display()))?;
                } else {
                    if let Some(parent) = to2.parent() {
                        std::fs::create_dir_all(parent)
                            .map_err(|e| format!("创建目录 {} 失败：{e}", parent.display()))?;
                    }
                    std::fs::copy(&from2, &to2)
                        .map_err(|e| format!("复制 {} 失败：{e}", from2.display()))?;
                    std::fs::remove_file(&from2)
                        .map_err(|e| format!("删除源文件 {} 失败：{e}", from2.display()))?;
                }
                Ok::<(), String>(())
            })
            .await;
            match joined {
                Ok(Ok(())) => ToolOutcome::ok(format!(
                    "[ok] 已移动（跨卷：复制后删除源）\n从：{}\n到：{}",
                    from.display(),
                    to.display()
                )),
                Ok(Err(e)) => ToolOutcome::err(format!(
                    "直接 rename 失败（{rename_err}），退化复制也失败：{e}"
                )),
                Err(e) => ToolOutcome::err(format!("移动任务异常：{e}")),
            }
        }
    }
}

async fn delete_path(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    let raw = match arg_str_req(&args, "path") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let recursive = arg_bool(&args, "recursive", false);
    let allow_outside = arg_bool(&args, "allow_outside", false);
    let path = match resolve_write_path(ctx, &raw, allow_outside) {
        Ok(p) => p,
        Err(e) => return e,
    };
    // 安全护栏：绝不允许删根目录/工作目录本身。
    let ws = neosonia_core::util::normalize_lexical(&ctx.workspace());
    let norm = neosonia_core::util::normalize_lexical(&path);
    if norm == ws {
        return ToolOutcome::err("拒绝删除工作目录本身。".to_string());
    }
    if norm.parent().is_none() || norm.components().count() <= 1 {
        return ToolOutcome::err("拒绝删除磁盘根目录。".to_string());
    }
    if let Some(home) = neosonia_core::util::home_dir() {
        if neosonia_core::util::normalize_lexical(&home) == norm {
            return ToolOutcome::err("拒绝删除用户主目录。".to_string());
        }
    }
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) => {
            return ToolOutcome::err(format!(
                "路径不存在或无法访问：{}（{}）",
                path.display(),
                io_error_hint(&e)
            ))
        }
    };
    if meta.is_dir() && !recursive {
        return ToolOutcome::err(format!(
            "{} 是目录，需要 recursive=true 才能删除（会删除整个目录树）。",
            path.display()
        ));
    }
    let ctx_for_task = ctx.clone();
    let path2 = path.clone();
    let joined = tokio::task::spawn_blocking(move || {
        if ctx_for_task.is_cancelled() {
            return Err("删除已取消（用户中断）。".to_string());
        }
        if path2.is_dir() {
            std::fs::remove_dir_all(&path2)
                .map_err(|e| format!("删除目录 {} 失败：{e}", path2.display()))
        } else {
            std::fs::remove_file(&path2)
                .map_err(|e| format!("删除文件 {} 失败：{e}", path2.display()))
        }
    })
    .await;
    match joined {
        Ok(Ok(())) => ToolOutcome::ok(format!(
            "[ok] 已删除{}\n路径：{}",
            if meta.is_dir() { "目录树" } else { "文件" },
            path.display()
        ))
        .with_display("已删除".to_string()),
        Ok(Err(e)) => ToolOutcome::err(e),
        Err(e) => ToolOutcome::err(format!("删除任务异常：{e}")),
    }
}

// ============================================================
// 工具声明
// ============================================================

const PATH_PROP: &str = "路径。相对路径相对工作目录（workspace），也可用 ~ 开头。";

tool_impl!(
    ReadFile,
    "read_file",
    "读取文本文件内容并返回带行号的结果。自动识别 UTF-8 / UTF-16 / GBK 编码；\
     不会 panic，二进制文件会明确报错。相对路径相对工作目录。\
     大文件用 start_line/end_line 分段读；max_chars 控制本次返回的字符上限（默认 20000）。",
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": PATH_PROP},
            "start_line": {"type": "integer", "description": "起始行号（从 1 开始，默认 1）"},
            "end_line": {"type": "integer", "description": "结束行号（含，默认到文件末尾）"},
            "max_chars": {"type": "integer", "description": "本次返回内容的最大字符数，默认 20000"}
        },
        "required": ["path"]
    }),
    read_file
);

tool_impl!(
    WriteFile,
    "write_file",
    "创建或覆盖写入文本文件（原子写：临时文件 + rename，不会留下半个文件）。\
     默认自动创建父目录；返回字节数与行数。默认只能写工作目录内的路径，\
     写外部路径必须显式传 allow_outside=true。",
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": PATH_PROP},
            "content": {"type": "string", "description": "要写入的完整文本内容（UTF-8）"},
            "create_dirs": {"type": "boolean", "description": "父目录不存在时自动创建，默认 true"},
            "overwrite": {"type": "boolean", "description": "文件已存在时是否覆盖，默认 true"},
            "allow_outside": {"type": "boolean", "description": "允许写到工作目录之外，默认 false"}
        },
        "required": ["path", "content"]
    }),
    write_file,
    approval
);

tool_impl!(
    EditFile,
    "edit_file",
    "按精确字符串替换修改文件（不是正则）。old_string 必须在文件中唯一，否则报错并给出出现行号；\
     确实需要全部替换时传 replace_all=true。找不到 old_string 时会返回文件前部内容帮助定位。",
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": PATH_PROP},
            "old_string": {"type": "string", "description": "要被替换的原文，必须逐字符一致（含缩进）"},
            "new_string": {"type": "string", "description": "替换后的文本"},
            "replace_all": {"type": "boolean", "description": "替换所有出现位置，默认 false"}
        },
        "required": ["path", "old_string", "new_string"]
    }),
    edit_file,
    approval
);

tool_impl!(
    AppendFile,
    "append_file",
    "在文件末尾追加文本；文件已存在且结尾没有换行时会自动补一个换行。文件不存在会创建。",
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": PATH_PROP},
            "content": {"type": "string", "description": "要追加的文本"}
        },
        "required": ["path", "content"]
    }),
    append_file,
    approval
);

tool_impl!(
    ListDir,
    "list_dir",
    "列出目录内容：目录标 [D]、文件标 [F] 并带大小。recursive=true 递归列出（最多 24 层）。\
     相对路径相对工作目录。",
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "目录路径，默认工作目录"},
            "recursive": {"type": "boolean", "description": "是否递归列出，默认 false"},
            "max_entries": {"type": "integer", "description": "最多返回多少条，默认 200"},
            "show_hidden": {"type": "boolean", "description": "是否显示隐藏/常见构建目录，默认 false"}
        }
    }),
    list_dir
);

tool_impl!(
    SearchFiles,
    "search_files",
    "在工作目录内搜索文件名或文件内容，完全本地实现（不依赖 Everything/es.exe）。\
     自动跳过 .git、node_modules、target、AppData 等目录。\
     mode=name 按文件名查找；mode=content 逐行匹配内容，返回 路径:行号: 内容。\
     支持 glob 过滤（如 \"*.rs\" 或 \"src/**,*.toml\"）与 regex=true 正则。",
    json!({
        "type": "object",
        "properties": {
            "query": {"type": "string", "description": "搜索词；mode=name 时匹配文件名，mode=content 时匹配行内容"},
            "path": {"type": "string", "description": "搜索根目录，默认工作目录"},
            "glob": {"type": "string", "description": "glob 过滤，如 \"*.rs\"；多个用逗号分隔"},
            "mode": {"type": "string", "enum": ["name", "content"], "description": "搜索模式，默认 name"},
            "max_results": {"type": "integer", "description": "最多返回多少条，默认 50"},
            "case_sensitive": {"type": "boolean", "description": "是否区分大小写，默认 false"},
            "regex": {"type": "boolean", "description": "query 是否按正则解释，默认 false"}
        },
        "required": ["query"]
    }),
    search_files
);

tool_impl!(
    FileInfo,
    "file_info",
    "查看文件/目录的元信息：类型、大小、修改/创建时间、只读、是否在工作目录内。",
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": PATH_PROP}
        },
        "required": ["path"]
    }),
    file_info
);

tool_impl!(
    MakeDir,
    "make_dir",
    "创建目录（默认递归创建）。目录已存在时返回成功。",
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": PATH_PROP},
            "recursive": {"type": "boolean", "description": "递归创建父目录，默认 true"}
        },
        "required": ["path"]
    }),
    make_dir
);

tool_impl!(
    CopyPath,
    "copy_path",
    "复制文件或整个目录树。目标已存在会报错（不覆盖）。跨卷也可以（内部走系统复制）。\
     默认目标必须在工作目录内，否则需 allow_outside=true。",
    json!({
        "type": "object",
        "properties": {
            "from": {"type": "string", "description": "源路径"},
            "to": {"type": "string", "description": "目标路径"},
            "allow_outside": {"type": "boolean", "description": "允许目标在工作目录之外，默认 false"}
        },
        "required": ["from", "to"]
    }),
    copy_path,
    approval
);

tool_impl!(
    MovePath,
    "move_path",
    "移动/重命名文件或目录；跨卷时自动退化为复制+删除源。目标已存在会报错（不覆盖）。",
    json!({
        "type": "object",
        "properties": {
            "from": {"type": "string", "description": "源路径"},
            "to": {"type": "string", "description": "目标路径"},
            "allow_outside": {"type": "boolean", "description": "允许目标在工作目录之外，默认 false"}
        },
        "required": ["from", "to"]
    }),
    move_path,
    approval
);

tool_impl!(
    DeletePath,
    "delete_path",
    "删除文件或目录树（危险操作）。删除目录必须 recursive=true；\
     拒绝删除磁盘根目录、用户主目录和工作目录本身。默认只能删工作目录内的路径。",
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": PATH_PROP},
            "recursive": {"type": "boolean", "description": "删除目录时必须为 true，默认 false"},
            "allow_outside": {"type": "boolean", "description": "允许删除工作目录之外的路径，默认 false"}
        },
        "required": ["path"]
    }),
    delete_path,
    approval
);

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{fake_ctx, temp_root};
    use serde_json::json;
    use std::path::PathBuf;

    fn workspace_of(ctx: &ToolCtx) -> PathBuf {
        ctx.workspace()
    }

    #[tokio::test]
    async fn read_write_roundtrip_with_line_numbers() {
        let root = temp_root("readwrite");
        let (ctx, _host) = fake_ctx(&root);
        let ws = workspace_of(&ctx);
        let target = ws.join("notes.txt");
        let text = "line one\nline two\nline three";
        let out = write_file(
            &ctx,
            json!({"path": "notes.txt", "content": text}),
        )
        .await;
        assert!(out.ok, "{}", out.content);
        assert!(target.is_file());

        let read = read_file(&ctx, json!({"path": "notes.txt"})).await;
        assert!(read.ok, "{}", read.content);
        assert!(read.content.contains("总行数：3"), "{}", read.content);
        assert!(read.content.contains("1: line one"), "{}", read.content);
        assert!(read.content.contains("3: line three"), "{}", read.content);

        let partial = read_file(&ctx, json!({"path": "notes.txt", "start_line": 2, "end_line": 2})).await;
        assert!(partial.ok);
        assert!(partial.content.contains("2: line two"));
        assert!(!partial.content.contains("1: line one"));
    }

    #[test]
    fn gbk_decoding() {
        let (bytes, _, _) = encoding_rs::GBK.encode("中文内容\n第二行");
        let (text, enc) = decode_text(&bytes);
        assert_eq!(enc, "GBK");
        assert!(text.contains("中文内容"));
    }

    #[tokio::test]
    async fn read_gbk_file() {
        let root = temp_root("gbk");
        let (ctx, _host) = fake_ctx(&root);
        let ws = workspace_of(&ctx);
        let (bytes, _, _) = encoding_rs::GBK.encode("第一行中文\nsecond line");
        std::fs::write(ws.join("gbk.txt"), &bytes).unwrap();
        let out = read_file(&ctx, json!({"path": "gbk.txt"})).await;
        assert!(out.ok, "{}", out.content);
        assert!(out.content.contains("编码：GBK"), "{}", out.content);
        assert!(out.content.contains("第一行中文"), "{}", out.content);
    }

    #[tokio::test]
    async fn reject_binary_file() {
        let root = temp_root("binary");
        let (ctx, _host) = fake_ctx(&root);
        std::fs::write(workspace_of(&ctx).join("a.bin"), [0u8, 1, 2, 3, 0, 255]).unwrap();
        let out = read_file(&ctx, json!({"path": "a.bin"})).await;
        assert!(!out.ok);
        assert!(out.content.contains("二进制"), "{}", out.content);
    }

    #[tokio::test]
    async fn edit_unique_and_duplicate() {
        let root = temp_root("edit");
        let (ctx, _host) = fake_ctx(&root);
        let ws = workspace_of(&ctx);
        std::fs::write(ws.join("e.txt"), "alpha beta gamma").unwrap();
        let out = edit_file(
            &ctx,
            json!({"path": "e.txt", "old_string": "beta", "new_string": "BETA"}),
        )
        .await;
        assert!(out.ok, "{}", out.content);
        assert_eq!(std::fs::read_to_string(ws.join("e.txt")).unwrap(), "alpha BETA gamma");

        // 不唯一 -> 报错
        std::fs::write(ws.join("d.txt"), "foo foo foo").unwrap();
        let dup = edit_file(
            &ctx,
            json!({"path": "d.txt", "old_string": "foo", "new_string": "bar"}),
        )
        .await;
        assert!(!dup.ok);
        assert!(dup.content.contains("出现 3 次"), "{}", dup.content);

        let all = edit_file(
            &ctx,
            json!({"path": "d.txt", "old_string": "foo", "new_string": "bar", "replace_all": true}),
        )
        .await;
        assert!(all.ok, "{}", all.content);
        assert_eq!(std::fs::read_to_string(ws.join("d.txt")).unwrap(), "bar bar bar");

        let missing = edit_file(
            &ctx,
            json!({"path": "d.txt", "old_string": "不存在", "new_string": "x"}),
        )
        .await;
        assert!(!missing.ok);
        assert!(missing.content.contains("没有找到"), "{}", missing.content);
    }

    #[tokio::test]
    async fn append_auto_newline() {
        let root = temp_root("append");
        let (ctx, _host) = fake_ctx(&root);
        let ws = workspace_of(&ctx);
        std::fs::write(ws.join("a.log"), "first").unwrap();
        let out = append_file(&ctx, json!({"path": "a.log", "content": "second"})).await;
        assert!(out.ok, "{}", out.content);
        let text = std::fs::read_to_string(ws.join("a.log")).unwrap();
        assert_eq!(text, "first\nsecond");
    }

    #[tokio::test]
    async fn sandbox_blocks_writes_outside_workspace() {
        let root = temp_root("sandbox-write");
        let (ctx, _host) = fake_ctx(&root);
        let out = write_file(
            &ctx,
            json!({"path": "../../escape.txt", "content": "x"}),
        )
        .await;
        assert!(!out.ok);
        assert!(out.content.contains("越界"), "{}", out.content);
        // 显式 allow_outside 才放行
        let outside_target = root.join("outside.txt");
        let ok = write_file(
            &ctx,
            json!({"path": format!("{}", outside_target.display()), "content": "x", "allow_outside": true}),
        )
        .await;
        assert!(ok.ok, "{}", ok.content);
        assert!(outside_target.is_file());
    }

    #[tokio::test]
    async fn list_dir_marks_types() {
        let root = temp_root("list");
        let (ctx, _host) = fake_ctx(&root);
        let ws = workspace_of(&ctx);
        std::fs::create_dir_all(ws.join("sub")).unwrap();
        std::fs::write(ws.join("a.txt"), "abc").unwrap();
        let out = list_dir(&ctx, json!({"path": "."})).await;
        assert!(out.ok, "{}", out.content);
        assert!(out.content.contains("[D] sub/"), "{}", out.content);
        assert!(out.content.contains("[F] a.txt"), "{}", out.content);
    }

    #[tokio::test]
    async fn search_name_content_and_glob() {
        let root = temp_root("search");
        let (ctx, _host) = fake_ctx(&root);
        let ws = workspace_of(&ctx);
        std::fs::create_dir_all(ws.join("src")).unwrap();
        std::fs::create_dir_all(ws.join("node_modules/pkg")).unwrap();
        std::fs::write(ws.join("src/main.rs"), "fn main() {\n    // hello rust\n}\n").unwrap();
        std::fs::write(ws.join("src/util.rs"), "pub fn helper() {}\n").unwrap();
        std::fs::write(ws.join("notes.md"), "# hello world\nhello again\n").unwrap();
        std::fs::write(ws.join("node_modules/pkg/index.js"), "hello from node_modules\n").unwrap();

        // 按名字
        let by_name = search_files(&ctx, json!({"query": "main", "mode": "name"})).await;
        assert!(by_name.ok, "{}", by_name.content);
        assert!(by_name.content.contains("main.rs"), "{}", by_name.content);

        // 按内容 + glob
        let by_content = search_files(
            &ctx,
            json!({"query": "hello", "mode": "content", "glob": "*.md"}),
        )
        .await;
        assert!(by_content.ok, "{}", by_content.content);
        assert!(by_content.content.contains("notes.md:1:"), "{}", by_content.content);
        assert!(!by_content.content.contains("main.rs"), "{}", by_content.content);

        // node_modules 默认跳过
        let skipped = search_files(&ctx, json!({"query": "node_modules", "mode": "content"})).await;
        assert!(skipped.ok, "{}", skipped.content);
        assert!(!skipped.content.contains("index.js"), "{}", skipped.content);

        // 正则
        let regex = search_files(
            &ctx,
            json!({"query": "fn\\s+\\w+", "mode": "content", "regex": true}),
        )
        .await;
        assert!(regex.ok, "{}", regex.content);
        assert!(regex.content.contains("main.rs"), "{}", regex.content);
    }

    #[tokio::test]
    async fn missing_params_return_err_not_panic() {
        let root = temp_root("missing");
        let (ctx, _host) = fake_ctx(&root);
        for out in [
            read_file(&ctx, json!({})).await,
            write_file(&ctx, json!({"path": "x"})).await,
            edit_file(&ctx, json!({"path": "x"})).await,
            append_file(&ctx, json!({})).await,
            search_files(&ctx, json!({})).await,
            file_info(&ctx, json!({})).await,
            list_dir(&ctx, json!({"path": "definitely-not-exist-12345"})).await,
        ] {
            assert!(!out.ok);
            assert!(!out.content.is_empty());
        }
    }

    #[tokio::test]
    async fn copy_move_delete() {
        let root = temp_root("cmd");
        let (ctx, _host) = fake_ctx(&root);
        let ws = workspace_of(&ctx);
        std::fs::write(ws.join("a.txt"), "hello").unwrap();
        let copied = copy_path(&ctx, json!({"from": "a.txt", "to": "b.txt"})).await;
        assert!(copied.ok, "{}", copied.content);
        assert_eq!(std::fs::read_to_string(ws.join("b.txt")).unwrap(), "hello");

        let moved = move_path(&ctx, json!({"from": "b.txt", "to": "c.txt"})).await;
        assert!(moved.ok, "{}", moved.content);
        assert!(!ws.join("b.txt").exists());
        assert!(ws.join("c.txt").exists());

        let deleted = delete_path(&ctx, json!({"path": "c.txt"})).await;
        assert!(deleted.ok, "{}", deleted.content);
        assert!(!ws.join("c.txt").exists());
    }

    #[tokio::test]
    async fn delete_dir_requires_recursive() {
        let root = temp_root("del-dir");
        let (ctx, _host) = fake_ctx(&root);
        let ws = workspace_of(&ctx);
        std::fs::create_dir_all(ws.join("d/sub")).unwrap();
        std::fs::write(ws.join("d/sub/x.txt"), "x").unwrap();
        let out = delete_path(&ctx, json!({"path": "d"})).await;
        assert!(!out.ok);
        assert!(out.content.contains("recursive=true"), "{}", out.content);
        let ok = delete_path(&ctx, json!({"path": "d", "recursive": true})).await;
        assert!(ok.ok, "{}", ok.content);
        assert!(!ws.join("d").exists());
    }
}
