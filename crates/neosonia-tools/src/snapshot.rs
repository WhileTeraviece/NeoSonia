//! 即时存档（快照）与恢复：备份区 `paths.backups`（安装目录下 `backups/`）。
//!
//! 设计要点：
//! - 每个「原始绝对路径」对应一个稳定备份 id：`sha256(规范化绝对路径小写) 前 12 位 hex
//!   + "_" + 安全化文件名`；
//! - 同一 id 再次保存 = 新版本覆盖旧版本（manifest 里 `version` 自增）；
//! - 目录布局：`backups/<id>/payload` + `backups/<id>/manifest.json`；
//! - 备份区对模型只读：普通文件/命令工具不能写/删/移动它，只能通过
//!   `snapshot_save`（新版本覆盖）、`snapshot_restore`（放回原位置）、
//!   `snapshot_delete`（删除备份）三个入口修改；读取工具（read_file / list_dir /
//!   search_files）允许访问；
//! - 恢复前校验 payload 的 SHA-256，哈希不符（损坏/被篡改）时拒绝恢复。
//!
//! 本模块同时提供只读防护 helper（供 `lib.rs::resolve_write_path` 与
//! `shell.rs::start_job` 调用）：`guard_backups_write` / `backups_dir_mentioned`。

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use neosonia_core::config::Paths;
use neosonia_core::model::{ToolCtx, ToolOutcome};
use neosonia_core::registry::ToolRegistry;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{arg_str, arg_str_req, io_error_hint, resolve_read_path};

/// 单个备份文件的大小上限（32 MiB）。
const MAX_BACKUP_BYTES: u64 = 32 * 1024 * 1024;
/// 备份目录内的 payload 文件名。
const PAYLOAD_FILE: &str = "payload";
/// 备份目录内的 manifest 文件名。
const MANIFEST_FILE: &str = "manifest.json";
/// 备份 id 里文件名部分的最大字符数（避免目录名过长）。
const MAX_NAME_CHARS: usize = 48;

// ============================================================
// 备份区只读防护 helper（供 fs / shell 共用）
// ============================================================

/// 判断路径是否落在备份区内。
///
/// 先做词法判断（大小写不敏感、处理 `.`/`..`），再尽量按真实路径判断，
/// 防止通过符号链接 / 父目录链接绕过。
pub(crate) fn path_in_backups(paths: &Paths, path: &Path) -> bool {
    if neosonia_core::util::is_within(&paths.backups, path) {
        return true;
    }
    // 备份目录本身可能是链接：拿真实路径再比一次。
    let real_backups = std::fs::canonicalize(&paths.backups)
        .map(strip_verbatim)
        .unwrap_or_else(|_| neosonia_core::util::normalize_lexical(&paths.backups));
    if neosonia_core::util::is_within(&real_backups, path) {
        return true;
    }
    if let Some(real) = canonical_or_parent(path) {
        if neosonia_core::util::is_within(&real_backups, &real) {
            return true;
        }
        if neosonia_core::util::is_within(
            &neosonia_core::util::normalize_lexical(&paths.backups),
            &real,
        ) {
            return true;
        }
    }
    false
}

/// 写/创建/删除/移动类工具的统一防护：目标落在备份区内一律拒绝。
pub(crate) fn guard_backups_write(paths: &Paths, path: &Path) -> Result<(), ToolOutcome> {
    if path_in_backups(paths, path) {
        return Err(ToolOutcome::err(format!(
            "备份区只读：{} 位于备份区 {} 内，不能直接写入/创建/删除/移动。\n\
             备份区只能通过 snapshot_save（新版本覆盖旧备份）、snapshot_restore（恢复到原位置）、\
             snapshot_delete（删除备份）操作；读取可用 read_file / list_dir / search_files。",
            path.display(),
            paths.backups.display()
        )));
    }
    Ok(())
}

/// 命令被拒绝时的中文错误。
pub(crate) fn command_backups_denied(paths: &Paths, reason: &str) -> ToolOutcome {
    ToolOutcome::err(format!(
        "命令被拒绝：{reason}。\n备份区目录：{}（对模型只读）。\n\
         不能用命令修改备份区；请改用 snapshot_save / snapshot_restore / snapshot_delete。\n\
         如需查看备份内容，可用 read_file / list_dir / search_files（这些读取工具允许访问备份区）。",
        paths.backups.display()
    ))
}

/// 命令字符串里是否提到了备份区目录（绝对路径，或 `../backups` 这种规范化后指到备份区的写法）。
///
/// 匹配规则：
/// - 大小写不敏感，斜杠/反斜杠等价；
/// - 绝对路径必须完整匹配 `<root>/backups`（避免把别的项目里同名的 backups 误伤）；
/// - 也识别 `../backups`（从任意子目录往上跳一层再进备份区）；
/// - 要求前后是路径边界，`backups2`、`my_backups` 不算。
pub(crate) fn backups_dir_mentioned(command: &str, root: &Path) -> bool {
    if command.trim().is_empty() {
        return false;
    }
    let hay = command.replace('\\', "/").to_lowercase();
    let absolute = root
        .join("backups")
        .to_string_lossy()
        .replace('\\', "/")
        .to_lowercase();
    let needle = absolute.trim_end_matches('/');
    if !needle.is_empty() && contains_path_token(&hay, needle) {
        return true;
    }
    contains_path_token(&hay, "../backups")
}

/// 在已小写化、正斜杠化的字符串里查找路径 token（前后必须是路径边界）。
fn contains_path_token(hay: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let hay_bytes = hay.as_bytes();
    let needle_bytes = needle.as_bytes();
    if hay_bytes.len() < needle_bytes.len() {
        return false;
    }
    let mut start = 0usize;
    while start + needle_bytes.len() <= hay_bytes.len() {
        let Some(offset) = hay_bytes[start..]
            .windows(needle_bytes.len())
            .position(|window| window == needle_bytes)
        else {
            break;
        };
        let at = start + offset;
        let before_ok = at == 0 || {
            let byte = hay_bytes[at - 1] as char;
            !byte.is_ascii_alphanumeric() && byte != '_'
        };
        let after = at + needle_bytes.len();
        let after_ok = after >= hay_bytes.len() || {
            let byte = hay_bytes[after] as char;
            !byte.is_ascii_alphanumeric() && byte != '_'
        };
        if before_ok && after_ok {
            return true;
        }
        start = at + 1;
    }
    false
}

/// 取路径的真实形式：存在就 canonicalize，不存在就 canonicalize 父目录再拼文件名。
fn canonical_or_parent(path: &Path) -> Option<PathBuf> {
    if let Ok(real) = std::fs::canonicalize(path) {
        return Some(strip_verbatim(real));
    }
    let parent = path.parent()?;
    let real_parent = std::fs::canonicalize(parent).ok()?;
    let name = path.file_name()?;
    Some(strip_verbatim(real_parent).join(name))
}

/// 去掉 Windows canonicalize 产生的 `\\?\` 前缀（还原成普通绝对路径）。
fn strip_verbatim(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy().to_string();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    path
}

// ============================================================
// manifest 与 id
// ============================================================

/// 一份备份的元信息（`backups/<id>/manifest.json`）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Manifest {
    pub id: String,
    pub original_path: String,
    pub version: u64,
    pub saved_at_ms: i64,
    pub sha256: String,
    pub size: u64,
    #[serde(default)]
    pub note: String,
}

/// 备份 id 的文件名部分：只保留安全字符。
fn safe_file_name(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        let keep = ch.is_ascii_alphanumeric()
            || matches!(ch, '-' | '_' | '.')
            || (ch as u32) >= 0x2E80;
        out.push(if keep { ch } else { '_' });
    }
    // Windows 不允许文件名以点/空格结尾；这里顺手去掉首尾的点/下划线/空格。
    let trimmed: String = out
        .trim_matches(|ch: char| ch == '.' || ch == '_' || ch == ' ')
        .to_string();
    let mut short: String = trimmed.chars().take(MAX_NAME_CHARS).collect();
    if short.is_empty() {
        short.push_str("file");
    }
    short
}

/// 由「规范化绝对路径小写」+ 文件名生成稳定备份 id。
fn backup_id(path_key: &str, file_name: &str) -> String {
    let digest = sha256_hex(path_key.as_bytes());
    format!("{}_{}", &digest[..12], safe_file_name(file_name))
}

/// 校验并规范化备份 id（同时挡住目录穿越）。
fn validate_backup_id(raw: &str) -> Result<String, ToolOutcome> {
    let id = raw.trim();
    if id.is_empty() {
        return Err(ToolOutcome::err("参数 id 不能为空。".to_string()));
    }
    let bad_char = |ch: char| {
        matches!(
            ch,
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0'
        )
    };
    let looks_like_dir = id == "." || id == ".." || id.starts_with('.');
    if id.chars().count() > 128 || looks_like_dir || id.chars().any(bad_char) {
        return Err(ToolOutcome::err(format!(
            "备份 id 非法：{id}。\n备份 id 应为 snapshot_list 返回的形如 \"0123456789ab_文件名\" 的字符串。"
        )));
    }
    Ok(id.to_string())
}

/// 读取备份目录里的 manifest；不存在或损坏时返回 None。
fn read_manifest_from(dir: &Path) -> Option<Manifest> {
    let bytes = std::fs::read(dir.join(MANIFEST_FILE)).ok()?;
    serde_json::from_slice::<Manifest>(&bytes).ok()
}

/// 读取备份目录里的 manifest；不存在/损坏时给出清晰中文错误。
fn read_manifest_required(dir: &Path, id: &str) -> Result<Manifest, ToolOutcome> {
    let path = dir.join(MANIFEST_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return Err(ToolOutcome::err(format!(
                "备份 {id} 缺少 manifest.json，备份已损坏（可用 snapshot_delete 删除后重新保存）。"
            )))
        }
        Err(e) => {
            return Err(ToolOutcome::err(format!(
                "读取备份 {id} 的 manifest.json 失败：{}",
                io_error_hint(&e)
            )))
        }
    };
    serde_json::from_slice(&bytes).map_err(|e| {
        ToolOutcome::err(format!(
            "备份 {id} 的 manifest.json 损坏，无法解析：{e}（可用 snapshot_delete 删除后重新保存）。"
        ))
    })
}

// ============================================================
// SHA-256（本 crate 没有 sha2 直接依赖，这里自带实现，避免动 Cargo.toml）
// ============================================================

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
    0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
    0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
    0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
    0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
    0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
    0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
    0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
    0xc67178f2,
];

/// 计算 SHA-256 的十六进制小写摘要。
fn sha256_hex(bytes: &[u8]) -> String {
    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (bytes.len() as u64).wrapping_mul(8);
    let mut data = Vec::with_capacity(bytes.len() + 72);
    data.extend_from_slice(bytes);
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in data.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut a = state[0];
        let mut b = state[1];
        let mut c = state[2];
        let mut d = state[3];
        let mut e = state[4];
        let mut f = state[5];
        let mut g = state[6];
        let mut h = state[7];
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA256_K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
        state[5] = state[5].wrapping_add(f);
        state[6] = state[6].wrapping_add(g);
        state[7] = state[7].wrapping_add(h);
    }

    let mut hex = String::with_capacity(64);
    for word in state {
        hex.push_str(&format!("{word:08x}"));
    }
    hex
}

// ============================================================
// 工具实现
// ============================================================

/// 把 `ToolOutcome` 里要返回给模型的结构化 JSON 统一序列化（失败也不 panic）。
fn json_outcome(value: Value, display: impl Into<String>) -> ToolOutcome {
    let text = serde_json::to_string_pretty(&value)
        .unwrap_or_else(|_| value.to_string());
    ToolOutcome::ok(text).with_display(display.into())
}

/// 解析成「已存在的普通文件」的规范化绝对路径（解析符号链接）。
fn resolve_existing_file(ctx: &ToolCtx, raw: &str) -> Result<(PathBuf, std::fs::Metadata), ToolOutcome> {
    let lexical = resolve_read_path(ctx, raw)?;
    let meta = match std::fs::symlink_metadata(&lexical) {
        Ok(meta) => meta,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return Err(ToolOutcome::err(format!(
                "文件不存在：{}。快照只能保存已存在的普通文件。",
                lexical.display()
            )))
        }
        Err(e) => {
            return Err(ToolOutcome::err(format!(
                "无法访问 {}：{}",
                lexical.display(),
                io_error_hint(&e)
            )))
        }
    };
    if meta.file_type().is_symlink() {
        // 符号链接：规范化到真实目标后再判断。
        let real = match std::fs::canonicalize(&lexical) {
            Ok(path) => strip_verbatim(path),
            Err(e) => {
                return Err(ToolOutcome::err(format!(
                    "符号链接 {} 的目标不可用：{}",
                    lexical.display(),
                    io_error_hint(&e)
                )))
            }
        };
        let real_meta = std::fs::metadata(&real).map_err(|e| {
            ToolOutcome::err(format!(
                "无法读取 {} 的目标 {}：{}",
                lexical.display(),
                real.display(),
                io_error_hint(&e)
            ))
        })?;
        if real_meta.is_dir() {
            return Err(ToolOutcome::err(format!(
                "{} 是符号链接，指向目录 {}；快照只支持普通文件。",
                lexical.display(),
                real.display()
            )));
        }
        return Ok((real, real_meta));
    }
    let canonical = match std::fs::canonicalize(&lexical) {
        Ok(path) => strip_verbatim(path),
        Err(e) => {
            return Err(ToolOutcome::err(format!(
                "无法访问 {}：{}",
                lexical.display(),
                io_error_hint(&e)
            )))
        }
    };
    let canonical_meta = match std::fs::metadata(&canonical) {
        Ok(meta) => meta,
        Err(e) => {
            return Err(ToolOutcome::err(format!(
                "无法读取 {}：{}",
                canonical.display(),
                io_error_hint(&e)
            )))
        }
    };
    Ok((canonical, canonical_meta))
}

async fn snapshot_save(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    let raw = match arg_str_req(&args, "path") {
        Ok(path) => path,
        Err(e) => return e,
    };
    let note = arg_str(&args, "note")
        .map(|note| note.trim().to_string())
        .unwrap_or_default();
    let paths = ctx.paths();
    let (path, meta) = match resolve_existing_file(ctx, &raw) {
        Ok(v) => v,
        Err(e) => return e,
    };
    if meta.is_dir() {
        return ToolOutcome::err(format!(
            "{} 是目录，快照只支持普通文件（可先用 list_dir / search_files 找到具体文件）。",
            path.display()
        ));
    }
    if !meta.is_file() {
        return ToolOutcome::err(format!(
            "{} 不是普通文件（可能是设备、管道或特殊文件），不能存档。",
            path.display()
        ));
    }
    if path_in_backups(&paths, &path) {
        return ToolOutcome::err(format!(
            "{} 位于备份区 {} 内，不能再存档（备份区只能通过 snapshot_save 新版本覆盖 / snapshot_delete 删除）。",
            path.display(),
            paths.backups.display()
        ));
    }
    if meta.len() > MAX_BACKUP_BYTES {
        return ToolOutcome::err(format!(
            "文件太大（{}，{} 字节），单个备份上限 32 MiB（{} 字节），未保存。",
            neosonia_core::util::human_bytes(meta.len()),
            meta.len(),
            MAX_BACKUP_BYTES
        ));
    }
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) => {
            return ToolOutcome::err(format!(
                "读取 {} 失败：{}",
                path.display(),
                io_error_hint(&e)
            ))
        }
    };
    // 读的瞬间文件可能变大：再按实际字节数复查一次。
    if bytes.len() as u64 > MAX_BACKUP_BYTES {
        return ToolOutcome::err(format!(
            "文件太大（{} 字节），单个备份上限 32 MiB（{} 字节），未保存。",
            bytes.len(),
            MAX_BACKUP_BYTES
        ));
    }

    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let path_key = path.to_string_lossy().to_lowercase();
    let id = backup_id(&path_key, &file_name);
    let dir = paths.backups.join(&id);
    let old_version = read_manifest_from(&dir).map(|m| m.version).unwrap_or(0);
    let version = old_version.saturating_add(1);
    let digest = sha256_hex(&bytes);
    let manifest = Manifest {
        id: id.clone(),
        original_path: path.to_string_lossy().to_string(),
        version,
        saved_at_ms: neosonia_core::util::now_ms(),
        sha256: digest.clone(),
        size: bytes.len() as u64,
        note,
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return ToolOutcome::err(format!(
            "创建备份目录 {} 失败：{}",
            dir.display(),
            io_error_hint(&e)
        ));
    }
    if let Err(e) = neosonia_core::util::atomic_write(&dir.join(PAYLOAD_FILE), &bytes) {
        return ToolOutcome::err(format!(
            "写入备份 payload {} 失败：{e}",
            dir.join(PAYLOAD_FILE).display()
        ));
    }
    let manifest_bytes = match serde_json::to_vec_pretty(&manifest) {
        Ok(bytes) => bytes,
        Err(e) => {
            return ToolOutcome::err(format!("序列化 manifest 失败：{e}"));
        }
    };
    if let Err(e) = neosonia_core::util::atomic_write(&dir.join(MANIFEST_FILE), &manifest_bytes) {
        return ToolOutcome::err(format!(
            "写入备份 manifest {} 失败：{e}",
            dir.join(MANIFEST_FILE).display()
        ));
    }

    json_outcome(
        json!({
            "id": id,
            "original_path": manifest.original_path,
            "version": version,
            "sha256": digest,
            "size": manifest.size,
        }),
        format!("已备份 {file_name}（v{version}）"),
    )
}

async fn snapshot_list(ctx: &ToolCtx, _args: Value) -> ToolOutcome {
    let paths = ctx.paths();
    let mut manifests: Vec<Manifest> = Vec::new();
    match std::fs::read_dir(&paths.backups) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let entry_path = entry.path();
                if !entry_path.is_dir() {
                    continue;
                }
                if let Some(manifest) = read_manifest_from(&entry_path) {
                    manifests.push(manifest);
                }
            }
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => {
            return ToolOutcome::err(format!(
                "读取备份区 {} 失败：{}",
                paths.backups.display(),
                io_error_hint(&e)
            ))
        }
    }
    // 按保存时间从新到旧；同一时间按 id 稳定排序。
    manifests.sort_by(|a, b| {
        b.saved_at_ms
            .cmp(&a.saved_at_ms)
            .then_with(|| a.id.cmp(&b.id))
    });
    let items: Vec<Value> = manifests
        .iter()
        .map(|m| {
            json!({
                "id": m.id,
                "original_path": m.original_path,
                "version": m.version,
                "saved_at_ms": m.saved_at_ms,
                "size": m.size,
                "note": m.note,
            })
        })
        .collect();
    let count = items.len();
    json_outcome(Value::Array(items), format!("{count} 个备份"))
}

async fn snapshot_restore(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    let raw = match arg_str_req(&args, "id") {
        Ok(id) => id,
        Err(e) => return e,
    };
    let id = match validate_backup_id(&raw) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let paths = ctx.paths();
    let dir = paths.backups.join(&id);
    if !dir.is_dir() {
        return ToolOutcome::err(format!(
            "备份 id 不存在：{id}。可用 snapshot_list 查看现有备份。"
        ));
    }
    let manifest = match read_manifest_required(&dir, &id) {
        Ok(manifest) => manifest,
        Err(e) => return e,
    };
    let payload_path = dir.join(PAYLOAD_FILE);
    let bytes = match std::fs::read(&payload_path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return ToolOutcome::err(format!(
                "备份 {id} 缺少 payload 文件，备份已损坏，无法恢复。"
            ))
        }
        Err(e) => {
            return ToolOutcome::err(format!(
                "读取备份 {id} 的 payload 失败：{}",
                io_error_hint(&e)
            ))
        }
    };
    let digest = sha256_hex(&bytes);
    if digest != manifest.sha256.trim().to_ascii_lowercase() {
        return ToolOutcome::err(format!(
            "哈希校验失败：备份 {id} 的 payload 与 manifest 记录的 SHA-256 不一致\
             （manifest={}，实际={}）。备份可能已损坏或被篡改，已拒绝恢复。",
            manifest.sha256, digest
        ));
    }
    if bytes.len() as u64 != manifest.size {
        return ToolOutcome::err(format!(
            "备份 {id} 的 payload 大小与 manifest 记录不一致（manifest={} 字节，实际={} 字节），已拒绝恢复。",
            manifest.size,
            bytes.len()
        ));
    }
    let original = PathBuf::from(manifest.original_path.trim());
    if original.as_os_str().is_empty() || !original.is_absolute() {
        return ToolOutcome::err(format!(
            "备份 {id} 的 manifest 里 original_path 非法（{:?}），无法恢复。",
            manifest.original_path
        ));
    }
    if path_in_backups(&paths, &original) {
        return ToolOutcome::err(format!(
            "备份 {id} 记录的原位置 {} 落在备份区内，拒绝恢复（备份区只读）。",
            original.display()
        ));
    }
    if let Err(e) = neosonia_core::util::atomic_write(&original, &bytes) {
        return ToolOutcome::err(format!(
            "恢复到 {} 失败：{e}",
            original.display()
        ));
    }
    json_outcome(
        json!({
            "restored_path": original.to_string_lossy(),
            "size": bytes.len() as u64,
            "sha256": digest,
        }),
        format!("已恢复 {}", original.display()),
    )
}

async fn snapshot_delete(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    let raw = match arg_str_req(&args, "id") {
        Ok(id) => id,
        Err(e) => return e,
    };
    let id = match validate_backup_id(&raw) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let paths = ctx.paths();
    let dir = paths.backups.join(&id);
    if !dir.is_dir() {
        return ToolOutcome::err(format!(
            "备份 id 不存在：{id}。可用 snapshot_list 查看现有备份。"
        ));
    }
    if let Err(e) = std::fs::remove_dir_all(&dir) {
        return ToolOutcome::err(format!(
            "删除备份 {id} 失败：{}",
            io_error_hint(&e)
        ));
    }
    json_outcome(
        json!({ "deleted_id": id }),
        format!("已删除备份 {id}"),
    )
}

// ============================================================
// 注册
// ============================================================

/// 注册即时存档的四个工具。不依赖 shell/input 设置，`register_all` 里无条件注册。
pub fn register(reg: &ToolRegistry) {
    reg.register(Arc::new(SnapshotSave));
    reg.register(Arc::new(SnapshotList));
    reg.register(Arc::new(SnapshotRestore));
    reg.register(Arc::new(SnapshotDelete));
}

tool_impl!(
    SnapshotSave,
    "snapshot_save",
    "把文件即时存档到应用的备份区（安装目录下 backups/，绝对路径见返回的 original_path 所在根目录）。\
     目标必须是已存在的普通文件（目录不行），单文件上限 32 MiB；相对路径相对工作目录。\
     每个原始路径对应一个稳定备份 id：再次保存会用新版本覆盖旧版本（version 自增）。\
     返回结构化 JSON：id、original_path、version、sha256、size。\
     备份区只能通过 snapshot_save（新版本覆盖）或 snapshot_delete（删除备份）修改，\
     模型不能直接用其他文件工具或命令修改备份区；读取用 read_file / list_dir / search_files。",
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "要存档的文件路径。相对路径相对工作目录（workspace），也可用 ~ 开头。"},
            "note": {"type": "string", "description": "可选备注，例如为什么存档、这是什么版本"}
        },
        "required": ["path"]
    }),
    snapshot_save
);

tool_impl!(
    SnapshotList,
    "snapshot_list",
    "列出备份区里所有即时存档，按保存时间从新到旧排序，返回 JSON 数组\
     （每项含 id、original_path、version、saved_at_ms、size、note）。\
     只读操作；用返回的 id 调用 snapshot_restore / snapshot_delete。",
    json!({
        "type": "object",
        "properties": {}
    }),
    snapshot_list
);

tool_impl!(
    SnapshotRestore,
    "snapshot_restore",
    "把指定备份 id 的文件内容恢复到它原本的绝对路径（原文件会被覆盖；自动创建缺失的父目录，原子写）。\
     恢复前先校验备份 payload 的 SHA-256，哈希不符（备份损坏或被篡改）时拒绝恢复并返回原因。\
     返回结构化 JSON：restored_path、size、sha256。备份 id 用 snapshot_list 获取。",
    json!({
        "type": "object",
        "properties": {
            "id": {"type": "string", "description": "备份 id（snapshot_list / snapshot_save 返回的 id）"}
        },
        "required": ["id"]
    }),
    snapshot_restore
);

tool_impl!(
    SnapshotDelete,
    "snapshot_delete",
    "删除指定 id 的备份（含 payload 与 manifest），只删除备份区里的副本，不影响原始文件。\
     备份区只能通过 snapshot_save（新版本覆盖旧备份）或 snapshot_delete（删除备份）修改，\
     模型不能直接用其他文件工具或命令修改备份区。返回被删除的备份 id。",
    json!({
        "type": "object",
        "properties": {
            "id": {"type": "string", "description": "要删除的备份 id（snapshot_list 返回的 id）"}
        },
        "required": ["id"]
    }),
    snapshot_delete
);

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support;
    use neosonia_core::config::Settings;

    fn registry() -> ToolRegistry {
        let reg = ToolRegistry::new();
        register(&reg);
        reg
    }

    fn full_registry() -> ToolRegistry {
        let reg = ToolRegistry::new();
        crate::register_all(&reg, &Settings::default());
        reg
    }

    /// 解析工具返回的 JSON（测试辅助，失败即 panic）。
    fn parse_json(content: &str) -> Value {
        serde_json::from_str(content).unwrap_or_else(|e| {
            panic!("工具输出不是合法 JSON（{e}）：{content}");
        })
    }

    #[test]
    fn sha256_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"The quick brown fox jumps over the lazy dog"),
            "d7a8fbb307d7809469ca9abcb0082e4f8d5651e46d3cdb762d02d0bf37c9e592"
        );
    }

    #[test]
    fn backup_id_is_stable_and_path_sensitive() {
        let a = backup_id(r"c:\users\me\notes.txt", "notes.txt");
        let b = backup_id(r"c:\users\me\notes.txt", "notes.txt");
        let c = backup_id(r"c:\users\me\other.txt", "notes.txt");
        assert_eq!(a, b, "同一路径的 id 必须稳定");
        assert_ne!(a, c, "不同路径的 id 必须不同");
        assert_eq!(a.len(), 12 + 1 + "notes.txt".len());
        assert!(a.chars().take(12).all(|ch| ch.is_ascii_hexdigit()));
    }

    #[test]
    fn backups_dir_mentioned_matching() {
        let root = Path::new(r"C:\NSAgent\NeoSonia");
        // 绝对路径：大小写/斜杠风格都算
        assert!(backups_dir_mentioned(
            r"Remove-Item -Recurse -Force C:\NSAgent\NeoSonia\backups\abc\payload",
            root
        ));
        assert!(backups_dir_mentioned(
            "del /q c:/nsagent/neosonia/BACKUPS/abc/payload",
            root
        ));
        assert!(backups_dir_mentioned(
            r#"Get-Content "C:\NSAgent\NeoSonia\backups\abc\manifest.json""#,
            root
        ));
        // 相对写法
        assert!(backups_dir_mentioned(r"cd .. ; Remove-Item ..\backups\abc", root));
        assert!(backups_dir_mentioned("rm -rf ../backups", root));
        // 误伤：别的目录里的 backups、名字前缀相同的目录
        assert!(!backups_dir_mentioned(
            r"Remove-Item C:\NSAgent\NeoSonia\workspace\backups",
            root
        ));
        assert!(!backups_dir_mentioned(
            r"Remove-Item C:\NSAgent\NeoSonia\backups2",
            root
        ));
        assert!(!backups_dir_mentioned(
            r"type my_backups\readme.txt",
            root
        ));
        // 普通命令不受影响
        assert!(!backups_dir_mentioned("git status", root));
        assert!(!backups_dir_mentioned("cargo test --offline -p neosonia-tools", root));
        assert!(!backups_dir_mentioned("", root));
    }

    #[test]
    fn snapshot_tools_registered_unconditionally() {
        let mut settings = Settings::default();
        settings.tools.allow_shell = false;
        settings.tools.allow_input_simulation = false;
        settings.tools.uia_enabled = false;
        settings.tools.cdp_enabled = false;
        settings.index.enabled = false;
        let reg = ToolRegistry::new();
        crate::register_all(&reg, &settings);
        let names = reg.names();
        for expected in [
            "snapshot_save",
            "snapshot_list",
            "snapshot_restore",
            "snapshot_delete",
        ] {
            assert!(
                names.iter().any(|name| name == expected),
                "register_all 里缺少快照工具 {expected}: {names:?}"
            );
        }
        // 描述里必须写清楚备份区只能通过 snapshot 工具修改
        let catalog = crate::tool_catalog();
        let spec = catalog
            .iter()
            .find(|spec| spec.name == "snapshot_save")
            .expect("catalog 应包含 snapshot_save");
        assert!(spec.description.contains("snapshot_delete"));
        assert!(spec.description.contains("不能直接用"));
    }

    #[tokio::test]
    async fn save_list_restore_delete_roundtrip() {
        let root = test_support::temp_root("snapshot-roundtrip");
        let (ctx, _host) = test_support::fake_ctx(&root);
        let reg = registry();
        let ws = ctx.workspace();
        let original = ws.join("notes.txt");
        std::fs::write(&original, "第一版内容\n").unwrap();
        let original_canonical = strip_verbatim(std::fs::canonicalize(&original).unwrap());

        // save
        let saved = reg
            .call(
                &ctx,
                "snapshot_save",
                &json!({"path": "notes.txt", "note": "初版"}),
            )
            .await;
        assert!(saved.ok, "{}", saved.content);
        let saved_json = parse_json(&saved.content);
        let id = saved_json["id"].as_str().unwrap().to_string();
        assert_eq!(
            saved_json["original_path"].as_str().unwrap(),
            original_canonical.to_string_lossy()
        );
        assert_eq!(saved_json["version"].as_u64(), Some(1));
        assert_eq!(
            saved_json["size"].as_u64().unwrap(),
            std::fs::metadata(&original).unwrap().len()
        );
        assert_eq!(saved_json["sha256"].as_str().unwrap(), sha256_hex("第一版内容\n".as_bytes()));
        assert!(root.join("backups").join(&id).join("payload").is_file());
        assert!(root.join("backups").join(&id).join("manifest.json").is_file());

        // list
        let listed = reg.call(&ctx, "snapshot_list", &json!({})).await;
        assert!(listed.ok, "{}", listed.content);
        let items = parse_json(&listed.content);
        let items = items.as_array().expect("list 应返回数组");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["id"].as_str().unwrap(), id.as_str());
        assert_eq!(items[0]["note"].as_str().unwrap(), "初版");

        // 原文件被改坏后恢复
        std::fs::write(&original, "被改坏了").unwrap();
        let restored = reg
            .call(&ctx, "snapshot_restore", &json!({"id": id}))
            .await;
        assert!(restored.ok, "{}", restored.content);
        let restored_json = parse_json(&restored.content);
        assert_eq!(
            restored_json["restored_path"].as_str().unwrap(),
            original_canonical.to_string_lossy()
        );
        assert_eq!(restored_json["size"].as_u64(), saved_json["size"].as_u64());
        assert_eq!(std::fs::read_to_string(&original).unwrap(), "第一版内容\n");

        // delete
        let deleted = reg
            .call(&ctx, "snapshot_delete", &json!({"id": id}))
            .await;
        assert!(deleted.ok, "{}", deleted.content);
        assert_eq!(parse_json(&deleted.content)["deleted_id"].as_str().unwrap(), id.as_str());
        assert!(!root.join("backups").join(&id).exists());
        let listed = reg.call(&ctx, "snapshot_list", &json!({})).await;
        assert_eq!(parse_json(&listed.content).as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn new_save_overwrites_old_version() {
        let root = test_support::temp_root("snapshot-version");
        let (ctx, _host) = test_support::fake_ctx(&root);
        let reg = registry();
        let ws = ctx.workspace();
        let original = ws.join("config.toml");
        std::fs::write(&original, "version = 1\n").unwrap();

        let first = reg
            .call(&ctx, "snapshot_save", &json!({"path": "config.toml"}))
            .await;
        assert!(first.ok, "{}", first.content);
        let first_json = parse_json(&first.content);
        let id = first_json["id"].as_str().unwrap().to_string();

        // 改内容后用绝对路径再存：同一个 id、version+1、旧内容被替换
        std::fs::write(&original, "version = 2\n").unwrap();
        let absolute = original.to_string_lossy().to_string();
        let second = reg
            .call(&ctx, "snapshot_save", &json!({"path": absolute}))
            .await;
        assert!(second.ok, "{}", second.content);
        let second_json = parse_json(&second.content);
        assert_eq!(second_json["id"].as_str().unwrap(), id, "同一路径必须复用备份 id");
        assert_eq!(second_json["version"].as_u64(), Some(2));
        assert_ne!(second_json["sha256"], first_json["sha256"]);
        assert_eq!(
            std::fs::read_to_string(root.join("backups").join(&id).join("payload")).unwrap(),
            "version = 2\n"
        );

        let listed = reg.call(&ctx, "snapshot_list", &json!({})).await;
        let items = parse_json(&listed.content);
        assert_eq!(items.as_array().unwrap().len(), 1, "新版本应覆盖旧版本，不新增条目");
        assert_eq!(items[0]["version"].as_u64(), Some(2));

        // 恢复的是最新版本
        std::fs::write(&original, "被改坏了").unwrap();
        let restored = reg
            .call(&ctx, "snapshot_restore", &json!({"id": id}))
            .await;
        assert!(restored.ok, "{}", restored.content);
        assert_eq!(std::fs::read_to_string(&original).unwrap(), "version = 2\n");
    }

    #[tokio::test]
    async fn save_rejects_missing_dir_too_big_and_nested_backups() {
        let root = test_support::temp_root("snapshot-save-errors");
        let (ctx, _host) = test_support::fake_ctx(&root);
        let reg = registry();
        let ws = ctx.workspace();

        // 路径不存在
        let missing = reg
            .call(&ctx, "snapshot_save", &json!({"path": "no_such_file.txt"}))
            .await;
        assert!(!missing.ok);
        assert!(missing.content.contains("不存在"), "{}", missing.content);

        // 目录
        std::fs::create_dir_all(ws.join("a_dir")).unwrap();
        let dir = reg
            .call(&ctx, "snapshot_save", &json!({"path": "a_dir"}))
            .await;
        assert!(!dir.ok);
        assert!(dir.content.contains("目录"), "{}", dir.content);

        // 超过 32 MiB
        let big = ws.join("big.bin");
        std::fs::write(&big, vec![0u8; (MAX_BACKUP_BYTES + 1) as usize]).unwrap();
        let too_big = reg
            .call(&ctx, "snapshot_save", &json!({"path": "big.bin"}))
            .await;
        assert!(!too_big.ok);
        assert!(too_big.content.contains("32 MiB"), "{}", too_big.content);
        let _ = std::fs::remove_file(&big);

        // 备份区里的文件不能再存档
        std::fs::write(&ws.join("plain.txt"), "x").unwrap();
        let saved = reg
            .call(&ctx, "snapshot_save", &json!({"path": "plain.txt"}))
            .await;
        assert!(saved.ok, "{}", saved.content);
        let id = parse_json(&saved.content)["id"].as_str().unwrap().to_string();
        let payload = root.join("backups").join(&id).join("payload");
        let nested = reg
            .call(
                &ctx,
                "snapshot_save",
                &json!({"path": payload.to_string_lossy()}),
            )
            .await;
        assert!(!nested.ok);
        assert!(nested.content.contains("备份区"), "{}", nested.content);
    }

    #[tokio::test]
    async fn restore_delete_reject_bad_or_tampered_backups() {
        let root = test_support::temp_root("snapshot-bad");
        let (ctx, _host) = test_support::fake_ctx(&root);
        let reg = registry();
        let ws = ctx.workspace();
        std::fs::write(ws.join("doc.md"), "原始内容").unwrap();
        let saved = reg
            .call(&ctx, "snapshot_save", &json!({"path": "doc.md"}))
            .await;
        assert!(saved.ok, "{}", saved.content);
        let id = parse_json(&saved.content)["id"].as_str().unwrap().to_string();

        // 不存在的 id
        let missing = reg
            .call(&ctx, "snapshot_restore", &json!({"id": "000000000000_nope"}))
            .await;
        assert!(!missing.ok);
        assert!(missing.content.contains("不存在"), "{}", missing.content);
        let missing_del = reg
            .call(&ctx, "snapshot_delete", &json!({"id": "000000000000_nope"}))
            .await;
        assert!(!missing_del.ok);
        assert!(missing_del.content.contains("不存在"), "{}", missing_del.content);

        // 非法 id（目录穿越）
        for bad in ["../evil", r"..\evil", "a/b", "."] {
            let bad_restore = reg
                .call(&ctx, "snapshot_restore", &json!({"id": bad}))
                .await;
            assert!(!bad_restore.ok, "非法 id {bad} 应被拒绝");
            assert!(bad_restore.content.contains("非法"), "{}", bad_restore.content);
        }

        // manifest 损坏
        let manifest_path = root.join("backups").join(&id).join("manifest.json");
        std::fs::write(&manifest_path, b"{ not json").unwrap();
        let broken = reg
            .call(&ctx, "snapshot_restore", &json!({"id": id}))
            .await;
        assert!(!broken.ok);
        assert!(broken.content.contains("manifest.json"), "{}", broken.content);

        // 重新保存获得可用备份，然后篡改 payload：恢复必须被拒
        let saved_again = reg
            .call(&ctx, "snapshot_save", &json!({"path": "doc.md"}))
            .await;
        assert!(saved_again.ok, "{}", saved_again.content);
        let payload = root.join("backups").join(&id).join("payload");
        std::fs::write(&payload, "被篡改的内容").unwrap();
        let tampered = reg
            .call(&ctx, "snapshot_restore", &json!({"id": id}))
            .await;
        assert!(!tampered.ok);
        assert!(
            tampered.content.contains("哈希校验失败"),
            "{}",
            tampered.content
        );
        // 原文件不应被篡改内容覆盖
        assert_eq!(std::fs::read_to_string(ws.join("doc.md")).unwrap(), "原始内容");
    }

    #[tokio::test]
    async fn fs_tools_cannot_write_backups_but_can_read() {
        let root = test_support::temp_root("snapshot-fs-guard");
        let (ctx, _host) = test_support::fake_ctx(&root);
        let reg = full_registry();
        let ws = ctx.workspace();
        let backups = root.join("backups");
        std::fs::write(ws.join("source.txt"), "source").unwrap();

        // 直接写/建/删/复制/移动目标到备份区都应被拒
        let write = reg
            .call(
                &ctx,
                "write_file",
                &json!({"path": backups.join("hack.txt").to_string_lossy(), "content": "x", "allow_outside": true}),
            )
            .await;
        assert!(!write.ok, "{}", write.content);
        assert!(write.content.contains("备份区只读"), "{}", write.content);

        let make_dir = reg
            .call(
                &ctx,
                "make_dir",
                &json!({"path": backups.join("new_dir").to_string_lossy()}),
            )
            .await;
        assert!(!make_dir.ok);

        std::fs::create_dir_all(backups.join("fake")).unwrap();
        std::fs::write(backups.join("fake").join("payload"), "backup").unwrap();
        let delete = reg
            .call(
                &ctx,
                "delete_path",
                &json!({"path": backups.join("fake").to_string_lossy(), "recursive": true, "allow_outside": true}),
            )
            .await;
        assert!(!delete.ok);
        assert!(delete.content.contains("备份区只读"), "{}", delete.content);
        assert!(backups.join("fake").join("payload").is_file(), "备份区内容不能被删除");

        let copy = reg
            .call(
                &ctx,
                "copy_path",
                &json!({"from": "source.txt", "to": backups.join("copy.txt").to_string_lossy(), "allow_outside": true}),
            )
            .await;
        assert!(!copy.ok);
        assert!(copy.content.contains("备份区只读"), "{}", copy.content);

        let moved = reg
            .call(
                &ctx,
                "move_path",
                &json!({"from": backups.join("fake").join("payload").to_string_lossy(), "to": "stolen.txt", "allow_outside": true}),
            )
            .await;
        assert!(!moved.ok, "从备份区移出文件也必须被拒绝");
        assert!(backups.join("fake").join("payload").is_file());

        // 读取允许：read_file / list_dir
        let read = reg
            .call(
                &ctx,
                "read_file",
                &json!({"path": backups.join("fake").join("payload").to_string_lossy()}),
            )
            .await;
        assert!(read.ok, "备份区应允许读取：{}", read.content);
        assert!(read.content.contains("backup"));
        let list = reg
            .call(&ctx, "list_dir", &json!({"path": backups.to_string_lossy()}))
            .await;
        assert!(list.ok, "备份区应允许列目录：{}", list.content);
        assert!(list.content.contains("fake"));

        // 普通路径不受影响
        let normal = reg
            .call(&ctx, "write_file", &json!({"path": "ok.txt", "content": "ok"}))
            .await;
        assert!(normal.ok, "{}", normal.content);
        assert!(ws.join("ok.txt").is_file());
    }

    #[tokio::test]
    async fn shell_commands_mentioning_backups_are_rejected() {
        let root = test_support::temp_root("snapshot-shell-guard");
        let (ctx, _host) = test_support::fake_ctx(&root);
        let reg = full_registry();
        let backups = ctx.paths().backups;

        // 绝对路径（无害命令；如果防护失效也只是列目录）
        let command = format!("dir \"{}\"", backups.display());
        let out = reg
            .call(&ctx, "run_command", &json!({"command": command}))
            .await;
        assert!(!out.ok, "引用备份区的命令必须被拒绝：{}", out.content);
        assert!(out.content.contains("备份区"), "{}", out.content);

        // 后台任务同样拒绝
        let bg = reg
            .call(
                &ctx,
                "background_task",
                &json!({"command": format!("Remove-Item -Recurse -Force \"{}\"", backups.display())}),
            )
            .await;
        assert!(!bg.ok, "{}", bg.content);
        assert!(bg.content.contains("备份区"), "{}", bg.content);

        // working_dir 指到备份区也拒绝
        std::fs::create_dir_all(&backups).unwrap();
        let cwd = reg
            .call(
                &ctx,
                "run_command",
                &json!({"command": "dir", "working_dir": backups.to_string_lossy()}),
            )
            .await;
        assert!(!cwd.ok, "{}", cwd.content);
        assert!(cwd.content.contains("备份区"), "{}", cwd.content);

        // 普通命令正常执行（不误伤）
        let normal = reg
            .call(
                &ctx,
                "run_command",
                &json!({"command": "echo neosonia-snapshot-guard-ok"}),
            )
            .await;
        assert!(normal.ok, "普通命令不应被误伤：{}", normal.content);
        assert!(normal.content.contains("neosonia-snapshot-guard-ok"));
    }
}
