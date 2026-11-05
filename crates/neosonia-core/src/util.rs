//! 小工具函数，被四个 crate 共用。

use std::io::Write;
use std::path::{Component, Path, PathBuf};

/// 当前 Unix 毫秒时间戳。
pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 原子写文件（同目录临时文件 + rename），避免半截文件。
pub fn atomic_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(bytes)?;
    tmp.flush()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// 按字符（不是字节）截断，保留头 `head` 和尾 `tail` 字符。
pub fn middle_truncate(s: &str, max_chars: usize) -> String {
    let total = s.chars().count();
    if max_chars == 0 || total <= max_chars {
        return s.to_string();
    }
    let head = max_chars * 2 / 3;
    let tail = max_chars.saturating_sub(head);
    let mut out: String = s.chars().take(head).collect();
    out.push_str(&format!(
        "\n\n... [已省略 {} 字符] ...\n\n",
        total.saturating_sub(head + tail)
    ));
    if tail > 0 {
        out.extend(s.chars().skip(total - tail));
    }
    out
}

/// 按字符截断到最多 `n` 个字符，尾部加省略号。
pub fn truncate_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n).collect();
    out.push('…');
    out
}

/// 粗略 token 估算：CJK 约 1 字 1 token，ASCII 约 4 字符 1 token。
pub fn estimate_tokens(s: &str) -> usize {
    let mut cjk = 0usize;
    let mut other = 0usize;
    for ch in s.chars() {
        if ch as u32 >= 0x2E80 {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    cjk + other / 4
}

pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

pub fn human_duration_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else if ms < 3_600_000 {
        format!("{}m{:02}s", ms / 60_000, (ms % 60_000) / 1000)
    } else {
        format!("{}h{:02}m", ms / 3_600_000, (ms % 3_600_000) / 60_000)
    }
}

/// 展开 `~` 与 `~/...`。
pub fn expand_tilde(p: &str) -> PathBuf {
    let trimmed = p.trim();
    if trimmed == "~" {
        if let Some(h) = home_dir() {
            return h;
        }
    }
    if let Some(rest) = trimmed
        .strip_prefix("~/")
        .or_else(|| trimmed.strip_prefix("~\\"))
    {
        if let Some(h) = home_dir() {
            return h.join(rest);
        }
    }
    PathBuf::from(trimmed)
}

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

/// 规范化（不要求目标存在）并判断 `child` 是否在 `root` 内。
/// 用于文件工具的沙箱判断；符号链接不做解析（Windows 上按路径前缀比较）。
pub fn is_within(root: &Path, child: &Path) -> bool {
    let r = normalize_lexical(root);
    let c = normalize_lexical(child);
    #[cfg(windows)]
    {
        let rs = r.to_string_lossy().to_ascii_lowercase();
        let cs = c.to_string_lossy().to_ascii_lowercase();
        cs == rs || cs.starts_with(&format!("{rs}\\")) || cs.starts_with(&format!("{rs}/"))
    }
    #[cfg(not(windows))]
    {
        c == r || c.starts_with(&r)
    }
}

/// 纯词法规范化：处理 `.` 与 `..`，不触碰文件系统。
pub fn normalize_lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// 生成短 id（8 位十六进制）。
pub fn short_id() -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    id[..8].to_string()
}

/// 文件名安全化。
pub fn slugify(s: &str) -> String {
    let mut out = String::new();
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if ch == ' ' || ch == '-' || ch == '_' || (ch as u32) > 0x2E80 {
            if ch.is_ascii() {
                out.push('-');
            } else {
                out.push(ch);
            }
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "item".into()
    } else {
        trimmed
    }
}

/// 把多行文本包成 Markdown 代码块，自动选择不会冲突的围栏长度。
pub fn code_block(lang: &str, body: &str) -> String {
    let mut fence = "```".to_string();
    while body.contains(&fence) {
        fence.push('`');
    }
    format!("{fence}{lang}\n{body}\n{fence}")
}

/// 解析 "1.5s" / "500ms" / "2m" / "90" 为毫秒。
pub fn parse_duration_ms(s: &str) -> Option<u64> {
    let t = s.trim().to_ascii_lowercase();
    if t.is_empty() {
        return None;
    }
    let (num, mult) = if let Some(x) = t.strip_suffix("ms") {
        (x, 1f64)
    } else if let Some(x) = t.strip_suffix('s') {
        (x, 1000f64)
    } else if let Some(x) = t.strip_suffix('m') {
        (x, 60_000f64)
    } else if let Some(x) = t.strip_suffix('h') {
        (x, 3_600_000f64)
    } else {
        (t.as_str(), 1000f64)
    };
    num.trim().parse::<f64>().ok().map(|v| (v * mult) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_counts_chars() {
        let s = "中文中文中文";
        assert_eq!(middle_truncate(s, 100), s);
        let t = middle_truncate(s, 4);
        assert!(t.contains("已省略"));
        t.as_bytes();
    }

    #[test]
    fn sandbox_prefix() {
        let root = Path::new("E:/NeoSonia/workspace");
        assert!(is_within(root, Path::new("E:/NeoSonia/workspace/a/b.txt")));
        assert!(!is_within(root, Path::new("E:/NeoSonia/workspace/../etc")));
        assert!(!is_within(root, Path::new("E:/other/x")));
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration_ms("500ms"), Some(500));
        assert_eq!(parse_duration_ms("2s"), Some(2000));
        assert_eq!(parse_duration_ms("3"), Some(3000));
        assert_eq!(parse_duration_ms("1.5m"), Some(90_000));
    }

    #[test]
    fn tokens_estimate() {
        assert!(estimate_tokens("hello world") > 0);
        assert_eq!(estimate_tokens("中文"), 2);
    }
}
