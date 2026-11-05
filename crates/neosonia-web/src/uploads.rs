//! 远程附件上传（v0.7）。
//!
//! 不引入 multipart/multer：SPA 直接把文件原始字节作为 body 发到
//! `POST /api/upload?name=<...>`，服务端落盘到 `paths.uploads/<id>__<安全文件名>`。
//!
//! 安全：
//! - 文件名白名单化（去路径/控制字符，限长）；
//! - id 必须是 32 位小写 hex（uuid simple）；
//! - 读取时 canonicalize + `is_within` 校验前缀，防目录穿越；
//! - 单文件上限 [`MAX_UPLOAD_BYTES`]。
//!
//! 元数据**不建库**：从文件名 `<id>__<name>` 和文件大小/扩展名重建，重启后仍可读。

use std::path::{Path, PathBuf};

use neosonia_core::Paths;
use neosonia_core::util::is_within;
use serde::Serialize;

/// 单文件上限：25 MB。
pub const MAX_UPLOAD_BYTES: usize = 25 * 1024 * 1024;
/// 单条消息最多附件数。
pub const MAX_ATTACHMENTS: usize = 8;
/// 文本附件内联进模型 prompt 的最大字符数。
pub const MAX_TEXT_INLINE_CHARS: usize = 8000;
/// 文件名最大字符数。
const MAX_NAME_CHARS: usize = 80;

/// 上传成功后的附件描述（`POST /api/upload` 返回、SPA 附件卡用）。
#[derive(Clone, Debug, Serialize)]
pub struct UploadInfo {
    pub id: String,
    pub name: String,
    pub size: u64,
    pub mime: String,
    /// `image` / `text` / `file` / `folder`（远程只有前三种）
    pub kind: String,
    /// 下载地址（同源相对路径，需要 Bearer）。
    pub url: String,
}

/// 已落盘的附件（含磁盘路径，供 chat 读取）。
#[derive(Clone, Debug)]
pub struct UploadEntry {
    pub info: UploadInfo,
    pub path: PathBuf,
}

/// 文件名安全化：去掉目录部分/控制字符/路径分隔符，限长；空则返回 "附件"。
pub fn sanitize_file_name(raw: &str) -> String {
    let base = raw
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(raw)
        .trim();
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control() && *c != '/' && *c != '\\')
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return "附件".to_string();
    }
    trimmed.chars().take(MAX_NAME_CHARS).collect()
}

/// 上传 id 合法性：32 位小写 hex。
pub fn is_safe_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// 按扩展名猜 MIME（够用即可；body 的 Content-Type 优先）。
pub fn guess_mime(name: &str) -> &'static str {
    let ext = name
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "txt" | "log" => "text/plain",
        "md" => "text/markdown",
        "json" => "application/json",
        "toml" | "yaml" | "yml" => "text/plain",
        "csv" => "text/csv",
        "rs" | "py" | "js" | "ts" | "html" | "css" | "sh" | "ps1" | "bat" | "c" | "h" | "cpp"
        | "java" | "go" => "text/plain",
        _ => "application/octet-stream",
    }
}

/// 根据 mime + 文件名判定附件类别。
pub fn kind_for(mime: &str, name: &str) -> &'static str {
    let mime = mime.to_ascii_lowercase();
    if mime.starts_with("image/") {
        return "image";
    }
    if mime.starts_with("text/")
        || mime.contains("json")
        || mime.contains("xml")
        || mime.contains("csv")
        || mime.contains("yaml")
        || matches!(
            name.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str(),
            "md" | "txt" | "log" | "rs" | "py" | "js" | "ts" | "toml" | "yaml" | "yml" | "csv"
                | "json" | "sh" | "ps1" | "bat" | "c" | "h" | "cpp" | "java" | "go" | "html"
                | "css"
        )
    {
        return "text";
    }
    "file"
}

fn uploads_dir(paths: &Paths) -> &Path {
    &paths.uploads
}

fn path_for(paths: &Paths, id: &str, name: &str) -> PathBuf {
    uploads_dir(paths).join(format!("{id}__{name}"))
}

/// 保存一个上传文件，返回附件描述。
pub fn save(
    paths: &Paths,
    name: &str,
    mime_hint: &str,
    bytes: &[u8],
) -> anyhow::Result<UploadInfo> {
    if bytes.is_empty() {
        anyhow::bail!("上传内容为空");
    }
    if bytes.len() > MAX_UPLOAD_BYTES {
        anyhow::bail!(
            "文件过大（{} MB），上限 {} MB",
            bytes.len() / 1024 / 1024,
            MAX_UPLOAD_BYTES / 1024 / 1024
        );
    }
    let name = sanitize_file_name(name);
    let mime = {
        let hint = mime_hint.trim().to_ascii_lowercase();
        if hint.is_empty() || hint == "application/octet-stream" {
            guess_mime(&name).to_string()
        } else {
            hint
        }
    };
    let id = uuid::Uuid::new_v4().simple().to_string();
    std::fs::create_dir_all(uploads_dir(paths))?;
    let path = path_for(paths, &id, &name);
    neosonia_core::util::atomic_write(&path, bytes)?;
    Ok(UploadInfo {
        url: format!("/api/uploads/{id}"),
        kind: kind_for(&mime, &name).to_string(),
        size: bytes.len() as u64,
        mime,
        name,
        id,
    })
}

/// 安全解析一个 upload id 到磁盘文件。
pub fn get(paths: &Paths, id: &str) -> Option<UploadEntry> {
    let id = id.trim();
    if !is_safe_id(id) {
        return None;
    }
    let dir = uploads_dir(paths).canonicalize().ok()?;
    let prefix = format!("{id}__");
    let mut best: Option<PathBuf> = None;
    for entry in std::fs::read_dir(&dir).ok()? {
        let entry = entry.ok()?;
        let file_name = entry.file_name().to_string_lossy().to_string();
        if !file_name.starts_with(&prefix) {
            continue;
        }
        let candidate = dir.join(&file_name);
        if !candidate.is_file() {
            continue;
        }
        best = Some(candidate);
        break;
    }
    let path = best?;
    // 防穿越：canonicalize 后必须仍在 uploads 目录内。
    let real = path.canonicalize().ok()?;
    if !is_within(&dir, &real) {
        return None;
    }
    let file_name = real.file_name()?.to_string_lossy().to_string();
    let name = file_name
        .split_once("__")
        .map(|(_, n)| n.to_string())
        .unwrap_or_else(|| file_name.clone());
    let size = real.metadata().ok()?.len();
    // 先借用 name 计算 mime/kind，再把 name move 进结构体（否则 E0382 borrow of moved value）。
    let mime = guess_mime(&name).to_string();
    let kind = kind_for(&mime, &name).to_string();
    Some(UploadEntry {
        info: UploadInfo {
            id: id.to_string(),
            name,
            size,
            mime,
            kind,
            url: format!("/api/uploads/{id}"),
        },
        path: real,
    })
}

/// 批量解析（去重、保持顺序、忽略不存在/非法 id）。
pub fn resolve_ids(paths: &Paths, ids: &[String]) -> Vec<UploadEntry> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for raw in ids.iter().take(MAX_ATTACHMENTS) {
        let id = raw.trim();
        if !seen.insert(id.to_string()) {
            continue;
        }
        if let Some(entry) = get(paths, id) {
            out.push(entry);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().expect("临时目录");
        let paths = Paths::from_root(dir.path());
        paths.ensure().expect("建目录");
        (dir, paths)
    }

    #[test]
    fn sanitize_file_name_blocks_traversal() {
        assert_eq!(sanitize_file_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_file_name("C:\\Windows\\win.ini"), "win.ini");
        assert_eq!(sanitize_file_name("a/b/c.txt"), "c.txt");
        assert_eq!(sanitize_file_name("  "), "附件");
        assert_eq!(sanitize_file_name("正常 名字.txt"), "正常 名字.txt");
        let long: String = "a".repeat(200);
        assert_eq!(sanitize_file_name(&long).chars().count(), MAX_NAME_CHARS);
    }

    #[test]
    fn id_validation() {
        assert!(is_safe_id("0123456789abcdef0123456789abcdef"));
        assert!(!is_safe_id("0123456789ABCDEF0123456789abcdef"));
        assert!(!is_safe_id("short"));
        assert!(!is_safe_id("../0123456789abcdef0123456789abcd"));
    }

    #[test]
    fn mime_and_kind() {
        assert_eq!(guess_mime("a.png"), "image/png");
        assert_eq!(guess_mime("a.JPG"), "image/jpeg");
        assert_eq!(kind_for("image/png", "a.png"), "image");
        assert_eq!(kind_for("text/plain", "a.txt"), "text");
        assert_eq!(kind_for("text/markdown", "a.md"), "text");
        assert_eq!(kind_for("application/octet-stream", "a.bin"), "file");
        assert_eq!(kind_for("application/octet-stream", "a.py"), "text");
    }

    #[test]
    fn save_get_roundtrip() {
        let (_dir, paths) = paths();
        let info = save(&paths, "../乱 名字.md", "", "# 标题\n正文".as_bytes()).expect("保存");
        assert_eq!(info.name, "乱 名字.md");
        assert_eq!(info.kind, "text");
        assert_eq!(info.size, "# 标题\n正文".len() as u64);
        assert!(is_safe_id(&info.id));
        assert_eq!(info.url, format!("/api/uploads/{}", info.id));

        let entry = get(&paths, &info.id).expect("读取");
        assert_eq!(entry.info.name, "乱 名字.md");
        let text = std::fs::read_to_string(&entry.path).expect("读文件");
        assert!(text.contains("标题"));

        // 非法/不存在 id。
        assert!(get(&paths, "../../etc/passwd").is_none());
        assert!(get(&paths, "0123456789abcdef0123456789abcdef").is_none());
    }

    #[test]
    fn rejects_empty_and_oversized() {
        let (_dir, paths) = paths();
        assert!(save(&paths, "a.txt", "", b"").is_err());
        let big = vec![0u8; MAX_UPLOAD_BYTES + 1];
        assert!(save(&paths, "big.bin", "", &big).is_err());
    }

    #[test]
    fn resolve_ids_dedupes_and_ignores_bad() {
        let (_dir, paths) = paths();
        let a = save(&paths, "a.txt", "", b"aaa").expect("保存");
        let b = save(&paths, "b.txt", "", b"bbb").expect("保存");
        let out = resolve_ids(
            &paths,
            &[
                a.id.clone(),
                "bad-id".to_string(),
                a.id.clone(),
                b.id.clone(),
            ],
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].info.name, "a.txt");
        assert_eq!(out[1].info.name, "b.txt");
        // 最多 MAX_ATTACHMENTS 个。
        let many: Vec<String> = (0..20).map(|_| a.id.clone()).collect();
        assert!(resolve_ids(&paths, &many).len() <= MAX_ATTACHMENTS);
    }

    #[test]
    fn png_preview_file_is_served() {
        let (_dir, paths) = paths();
        // 1x1 PNG
        let png: &[u8] = &[
            0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0,
        ];
        let info = save(&paths, "shot.png", "image/png", png).expect("保存");
        assert_eq!(info.kind, "image");
        assert_eq!(info.mime, "image/png");
        assert!(get(&paths, &info.id).is_some());
    }
}
