//! 截图文件的安全读取（`GET /api/screenshot/{name}`）。
//!
//! 防目录穿越：
//! 1. 文件名白名单（只允许 ASCII 字母/数字/`-`/`_`/`.`，必须以 `.png` 结尾，
//!    不允许 `..`、不允许以 `.` 开头、不允许路径分隔符）；
//! 2. 拼接后 `canonicalize`，再用 `core::util::is_within` 校验仍位于
//!    `paths.screenshots` 目录内（防御符号链接/大小写/`..` 之类的花招）。

use std::path::{Path, PathBuf};

use neosonia_core::util::is_within;

/// 文件名最大长度。
const MAX_NAME_LEN: usize = 128;

/// 校验并规范化文件名；不合法返回 None。
pub fn safe_file_name(name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        return None;
    }
    // 只允许单层文件名，不含任何路径分隔符或上跳。
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return None;
    }
    if name.starts_with('.') {
        return None;
    }
    if !name.to_ascii_lowercase().ends_with(".png") {
        return None;
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return None;
    }
    Some(name.to_string())
}

/// 在截图目录里安全解析出一个已存在的 PNG 绝对路径。
///
/// 返回的路径已经 `canonicalize`，可直接读。
pub fn resolve(screens_dir: &Path, name: &str) -> Result<PathBuf, String> {
    let clean = safe_file_name(name).ok_or_else(|| "截图文件名不合法。".to_string())?;
    let dir = screens_dir
        .canonicalize()
        .map_err(|_| "截图目录不存在。".to_string())?;
    let candidate = dir.join(&clean);
    let real = candidate
        .canonicalize()
        .map_err(|_| "截图不存在。".to_string())?;
    if !is_within(&dir, &real) {
        return Err("拒绝读取截图目录之外的文件。".to_string());
    }
    if !real.is_file() {
        return Err("截图不存在。".to_string());
    }
    Ok(real)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_traversal_names() {
        for bad in [
            "../../etc/passwd",
            "..\\..\\windows\\win.ini",
            "/etc/passwd",
            "a/b.png",
            "a\\b.png",
            "..",
            ".",
            ".hidden.png",
            "shot.png.exe",
            "shot.jpg",
            "",
            "   ",
        ] {
            assert!(safe_file_name(bad).is_none(), "{bad:?} 应被拒绝");
        }
        // 前后空白会被 trim 掉，返回干净文件名（仍然安全）。
        assert_eq!(safe_file_name(" shot.png "), Some("shot.png".to_string()));
    }

    #[test]
    fn accepts_plain_png_names() {
        for ok in ["shot.png", "screen-2026_11_01.png", "A1.png", "a.b.c.png"] {
            assert_eq!(safe_file_name(ok), Some(ok.to_string()), "{ok:?} 应被接受");
        }
    }

    #[test]
    fn percent_encoded_traversal_rejected() {
        // axum 的 Path 提取器会先 percent-decode，所以这里模拟解码后的字符串。
        assert!(safe_file_name("..%2f..%2fsecret.png").is_none());
        assert!(safe_file_name("../secret.png").is_none());
    }

    #[test]
    fn resolve_inside_dir_works() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("shot.png");
        std::fs::write(&path, b"png").expect("写文件");
        let out = resolve(dir.path(), "shot.png").expect("应能解析");
        assert_eq!(out, path.canonicalize().unwrap());
    }

    #[test]
    fn resolve_missing_file_errors() {
        let dir = tempfile::tempdir().expect("临时目录");
        assert!(resolve(dir.path(), "nope.png").is_err());
    }

    #[test]
    fn resolve_rejects_outside_and_non_png() {
        let dir = tempfile::tempdir().expect("临时目录");
        let nested = dir.path().join("shots");
        std::fs::create_dir_all(&nested).expect("建目录");
        std::fs::write(dir.path().join("secret.png"), b"secret").expect("写文件");
        std::fs::write(nested.join("ok.png"), b"ok").expect("写文件");
        // 从 shots 目录里读上级目录的文件必须失败。
        assert!(resolve(&nested, "../secret.png").is_err());
        // 非 png（即使存在）也必须失败。
        std::fs::write(nested.join("note.txt"), b"txt").expect("写文件");
        assert!(resolve(&nested, "note.txt").is_err());
        // 正常文件可以读。
        assert!(resolve(&nested, "ok.png").is_ok());
    }
}
