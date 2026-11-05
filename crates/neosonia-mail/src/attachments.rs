//! 发信附件：读取、大小/数量校验、MIME 类型推断。
//!
//! 约束（产品要求）：
//! - 最多 [`MAX_ATTACHMENTS`] 个附件；
//! - 单个文件不超过 [`MAX_SINGLE_BYTES`]（25MB）；
//! - 合计不超过 [`MAX_TOTAL_BYTES`]（40MB）；
//! - 附件路径必须是**绝对路径**；
//! - 文件名交给 lettre，由它按 RFC2231 处理中文/长文件名（见 `Content-Disposition`）。
//!
//! 所有错误都是中文，不含 emoji、不 panic。附件内容不写日志。

use std::path::{Path, PathBuf};

use anyhow::{bail, Context};

use crate::truncate_chars;

/// 附件数量上限。
pub const MAX_ATTACHMENTS: usize = 10;
/// 单个附件大小上限：25 MB。
pub const MAX_SINGLE_BYTES: u64 = 25 * 1024 * 1024;
/// 附件合计大小上限：40 MB。
pub const MAX_TOTAL_BYTES: u64 = 40 * 1024 * 1024;

/// 已读取、可交给 lettre 组装的附件。
#[derive(Clone, Debug)]
pub struct MailAttachment {
    /// 原始绝对路径（诊断用）。
    pub path: PathBuf,
    /// 展示/发送用的文件名（不含路径）。
    pub filename: String,
    /// MIME 类型，例如 `image/png`。
    pub mime: String,
    /// 文件内容。
    pub bytes: Vec<u8>,
}

impl MailAttachment {
    pub fn size(&self) -> u64 {
        self.bytes.len() as u64
    }
}

/// 按扩展名推断 MIME 类型；未知类型用 `application/octet-stream`。
pub fn mime_for_path(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
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
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "doc" => "application/msword",
        "xls" => "application/vnd.ms-excel",
        "ppt" => "application/vnd.ms-powerpoint",
        "zip" => "application/zip",
        "7z" => "application/x-7z-compressed",
        "rar" => "application/vnd.rar",
        "gz" | "tgz" => "application/gzip",
        "txt" => "text/plain",
        "csv" => "text/csv",
        "md" => "text/markdown",
        "json" => "application/json",
        "xml" => "application/xml",
        "html" | "htm" => "text/html",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "mp4" => "video/mp4",
        _ => "application/octet-stream",
    }
}

/// 把用户/模型给的附件路径校验并读入内存。
///
/// `paths` 可以是 `~` 开头的路径（会展开），但展开后必须是绝对路径。
pub fn load_attachments(paths: &[String]) -> anyhow::Result<Vec<MailAttachment>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    if paths.len() > MAX_ATTACHMENTS {
        bail!(
            "附件最多 {} 个，这次给了 {} 个。请删掉一些再发送。",
            MAX_ATTACHMENTS,
            paths.len()
        );
    }

    let mut attachments = Vec::with_capacity(paths.len());
    let mut total: u64 = 0;
    for (index, raw) in paths.iter().enumerate() {
        let raw = raw.trim();
        if raw.is_empty() {
            bail!("第 {} 个附件路径是空的：请给绝对路径。", index + 1);
        }
        let expanded = neosonia_core::util::expand_tilde(raw);
        if !expanded.is_absolute() {
            bail!(
                "附件必须是绝对路径（第 {} 个：{raw}）。例如 Windows 下写 C:\\Users\\me\\a.pdf",
                index + 1
            );
        }
        let meta = match std::fs::metadata(&expanded) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                bail!("附件不存在（第 {} 个）：{}", index + 1, expanded.display())
            }
            Err(e) => bail!(
                "无法读取附件信息（第 {} 个）：{}：{e}",
                index + 1,
                expanded.display()
            ),
        };
        if meta.is_dir() {
            bail!(
                "附件不能是目录（第 {} 个）：{}（请先打包或选择文件）",
                index + 1,
                expanded.display()
            );
        }
        if !meta.is_file() {
            bail!("附件不是普通文件（第 {} 个）：{}", index + 1, expanded.display());
        }
        let size = meta.len();
        if size > MAX_SINGLE_BYTES {
            bail!(
                "附件「{}」大小 {}，超过单文件上限 25MB。请压缩后再发送。",
                file_name_of(&expanded),
                human_size(size)
            );
        }
        if total.saturating_add(size) > MAX_TOTAL_BYTES {
            bail!(
                "附件合计大小超过 40MB 上限（当前已到 {}，加上「{}」后是 {}）。请删减附件或压缩后再发送。",
                human_size(total),
                file_name_of(&expanded),
                human_size(total.saturating_add(size))
            );
        }
        let bytes = std::fs::read(&expanded)
            .with_context(|| format!("读取附件失败：{}", expanded.display()))?;
        total = total.saturating_add(bytes.len() as u64);
        let filename = file_name_of(&expanded);
        let mime = mime_for_path(&expanded).to_string();
        attachments.push(MailAttachment {
            path: expanded,
            filename,
            mime,
            bytes,
        });
    }
    Ok(attachments)
}

fn file_name_of(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| "attachment".to_string());
    // 文件名里不允许 CR/LF（防止头部注入）；其余交给 lettre 的 RFC2231 编码。
    let cleaned: String = name
        .chars()
        .map(|c| if c == '\r' || c == '\n' { '_' } else { c })
        .collect();
    truncate_chars(&cleaned, 200)
}

fn human_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes}B")
    }
}

/// 附件清单的一行摘要（给工具输出用）。
pub fn summary_line(attachments: &[MailAttachment]) -> String {
    if attachments.is_empty() {
        return String::new();
    }
    let names: Vec<String> = attachments
        .iter()
        .map(|a| format!("{}（{}，{}）", a.filename, a.mime, human_size(a.size())))
        .collect();
    format!(
        "附件 {} 个：{}",
        attachments.len(),
        truncate_chars(&names.join("、"), 800)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "neosonia-attach-{tag}-{}-{}",
            std::process::id(),
            neosonia_core::util::short_id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn mime_mapping_covers_contract_types() {
        let cases = [
            ("a.png", "image/png"),
            ("a.JPG", "image/jpeg"),
            ("a.jpeg", "image/jpeg"),
            ("a.gif", "image/gif"),
            ("a.webp", "image/webp"),
            ("a.pdf", "application/pdf"),
            (
                "a.docx",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            ),
            (
                "a.xlsx",
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            ),
            (
                "a.pptx",
                "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            ),
            ("a.zip", "application/zip"),
            ("a.txt", "text/plain"),
            ("a.csv", "text/csv"),
            ("a.bin", "application/octet-stream"),
            ("a.unknown", "application/octet-stream"),
        ];
        for (name, expected) in cases {
            assert_eq!(mime_for_path(Path::new(name)), expected, "file={name}");
        }
    }

    #[test]
    fn loads_files_and_keeps_metadata() {
        let dir = temp_dir("load");
        let file = dir.join("图片 测试.png");
        std::fs::write(&file, b"fake-png").unwrap();
        let list = load_attachments(&[file.to_string_lossy().to_string()]).expect("load");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].filename, "图片 测试.png");
        assert_eq!(list[0].mime, "image/png");
        assert_eq!(list[0].size(), 8);
        let summary = summary_line(&list);
        assert!(summary.contains("图片 测试.png"));
        assert!(summary.contains("image/png"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_list_is_ok() {
        assert!(load_attachments(&[]).unwrap().is_empty());
        assert!(summary_line(&[]).is_empty());
    }

    #[test]
    fn rejects_missing_file() {
        let missing = std::env::temp_dir().join("neosonia-attach-definitely-missing.pdf");
        let err = load_attachments(&[missing.to_string_lossy().to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("附件不存在"), "got: {err}");
    }

    #[test]
    fn rejects_relative_path() {
        let err = load_attachments(&["relative/file.txt".to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("绝对路径"), "got: {err}");
    }

    #[test]
    fn rejects_directory() {
        let dir = temp_dir("dir");
        let err = load_attachments(&[dir.to_string_lossy().to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("不能是目录"), "got: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_too_many_attachments() {
        let paths: Vec<String> = (0..=MAX_ATTACHMENTS)
            .map(|i| format!("/tmp/attachment-{i}.txt"))
            .collect();
        let err = load_attachments(&paths).unwrap_err().to_string();
        assert!(err.contains("最多"), "got: {err}");
        assert!(err.contains("11"), "got: {err}");
    }

    #[test]
    fn rejects_single_file_over_25mb() {
        let dir = temp_dir("big-single");
        let file = dir.join("big.bin");
        let f = std::fs::File::create(&file).unwrap();
        f.set_len(MAX_SINGLE_BYTES + 1).unwrap();
        drop(f);
        let err = load_attachments(&[file.to_string_lossy().to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("25MB"), "got: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_total_over_40mb() {
        let dir = temp_dir("big-total");
        let a = dir.join("a.bin");
        let b = dir.join("b.bin");
        for path in [&a, &b] {
            let f = std::fs::File::create(path).unwrap();
            f.set_len(21 * 1024 * 1024).unwrap();
            drop(f);
        }
        let err = load_attachments(&[
            a.to_string_lossy().to_string(),
            b.to_string_lossy().to_string(),
        ])
        .unwrap_err()
        .to_string();
        assert!(err.contains("40MB"), "got: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
