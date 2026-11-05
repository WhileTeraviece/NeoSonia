//! 配对码与设备 token。
//!
//! - 配对码：8 位大写字母/数字（去掉 I/O/0/1 等易混字符），**一次性**且带 TTL；
//! - token：32 字节 CSPRNG 随机数，hex 编码（64 字符），只在响应里明文返回一次，
//!   服务端只存 `sha256(token)`；
//! - 二维码：`qrcode` 生成 PNG，内容为 `http://<lan_ip>:<port>/pair#<code>`
//!   （code 放 fragment，浏览器不会把它发给服务器）。

use std::path::Path;
use std::sync::Mutex;

use anyhow::Context;
use neosonia_core::util::{atomic_write, now_ms};
use qrcode::{EcLevel, QrCode};
use rand::RngCore;

/// 配对码字符集：32 个字符（5 bit/字符），去掉 0/O/1/I 之类易混项。
const CODE_ALPHABET: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
/// 配对码长度。
const CODE_LEN: usize = 8;
/// token 随机字节数。
const TOKEN_BYTES: usize = 32;

/// 一次配对校验的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PairCheck {
    /// 配对码正确，已标记为已用。
    Ok,
    /// 已被使用过（一次性）。
    Used,
    /// 超过 TTL。
    Expired,
    /// 与当前配对码不匹配。
    Mismatch,
}

/// 配对码状态（带过期时间与一次性标记）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairingView {
    pub code: String,
    pub created_at_ms: i64,
    pub expires_at_ms: i64,
    pub used: bool,
}

impl PairingView {
    /// 剩余有效时间（毫秒）。
    #[allow(dead_code)]
    pub fn remaining_ms(&self) -> i64 {
        (self.expires_at_ms - now_ms()).max(0)
    }

    /// 是否还能用来配对。
    #[allow(dead_code)]
    pub fn is_active(&self) -> bool {
        !self.used && self.remaining_ms() > 0
    }
}

#[derive(Debug)]
struct Inner {
    code: String,
    created_at_ms: i64,
    expires_at_ms: i64,
    used: bool,
}

/// 配对码状态机。线程安全，可放进 axum state。
pub struct PairingState {
    inner: Mutex<Inner>,
}

impl PairingState {
    /// 新建并立即生成一个配对码。
    pub fn new(ttl_secs: u64) -> Self {
        let state = Self {
            inner: Mutex::new(Inner {
                code: String::new(),
                created_at_ms: 0,
                expires_at_ms: 0,
                used: true,
            }),
        };
        state.rotate(ttl_secs);
        state
    }

    /// 换一个新码（app 点「刷新配对码」/ 服务启动时调用）。
    pub fn rotate(&self, ttl_secs: u64) -> PairingView {
        let now = now_ms();
        let ttl_ms = ttl_secs.saturating_mul(1000) as i64;
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.code = random_code();
        inner.created_at_ms = now;
        inner.expires_at_ms = now.saturating_add(ttl_ms);
        inner.used = false;
        view_locked(&inner)
    }

    /// 当前配对码快照。
    pub fn view(&self) -> PairingView {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        view_locked(&inner)
    }

    /// 校验并消费配对码（一次性：成功一次后即失效）。
    pub fn consume(&self, input: &str) -> PairCheck {
        let normalized = normalize_code(input);
        let now = now_ms();
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.used {
            return PairCheck::Used;
        }
        if now >= inner.expires_at_ms {
            return PairCheck::Expired;
        }
        if normalized.is_empty() || !constant_time_eq(normalized.as_bytes(), inner.code.as_bytes()) {
            return PairCheck::Mismatch;
        }
        inner.used = true;
        PairCheck::Ok
    }
}

fn view_locked(inner: &Inner) -> PairingView {
    PairingView {
        code: inner.code.clone(),
        created_at_ms: inner.created_at_ms,
        expires_at_ms: inner.expires_at_ms,
        used: inner.used,
    }
}

/// 规范化用户输入的配对码：去掉空格/连字符，统一大写。
pub fn normalize_code(input: &str) -> String {
    input
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// 生成 8 位配对码。
fn random_code() -> String {
    let mut rng = rand::rngs::OsRng;
    let mut out = String::with_capacity(CODE_LEN);
    let mut buf = [0u8; 1];
    while out.len() < CODE_LEN {
        rng.fill_bytes(&mut buf);
        // 拒绝采样，避免取模偏差：224 = 32 * 7。
        let b = buf[0];
        if b < 224 {
            out.push(CODE_ALPHABET[(b % 32) as usize] as char);
        }
    }
    out
}

/// 生成 32 字节随机 token（64 位小写 hex）。
pub fn new_token() -> String {
    let mut bytes = [0u8; TOKEN_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// `sha256(input)` 的 hex 形式。设备表里只存这个，不存原始 token。
pub fn sha256_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

/// 常量时间比较（token 哈希比较用，避免时间侧信道）。
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// 生成配对二维码 PNG 并原子写盘。
///
/// 内容由调用方拼好（一般是 `http://<lan_ip>:<port>/pair#<code>`）。
pub fn write_qr_png(path: &Path, content: &str) -> anyhow::Result<()> {
    let code = QrCode::with_error_correction_level(content.as_bytes(), EcLevel::M)
        .map_err(|e| anyhow::anyhow!("生成二维码失败：{e}"))?;
    let image = code
        .render::<image::Luma<u8>>()
        .min_dimensions(360, 360)
        .quiet_zone(true)
        .build();
    let mut buf: Vec<u8> = Vec::new();
    let mut cursor = std::io::Cursor::new(&mut buf);
    image
        .write_to(&mut cursor, image::ImageFormat::Png)
        .context("编码二维码 PNG 失败")?;
    atomic_write(path, &buf).context("写入二维码文件失败")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_shape_and_alphabet() {
        let code = random_code();
        assert_eq!(code.len(), CODE_LEN);
        for ch in code.chars() {
            assert!(
                CODE_ALPHABET.contains(&(ch as u8)),
                "配对码字符 {ch} 不在字符集内"
            );
        }
        // 两次生成不应相同（概率上极低冲突）。
        assert_ne!(random_code(), random_code());
    }

    #[test]
    fn normalize_ignores_case_spaces_dashes() {
        assert_eq!(normalize_code(" a2c4-e6f8 "), "A2C4E6F8");
        assert_eq!(normalize_code("A2C4 E6F8"), "A2C4E6F8");
        assert_eq!(normalize_code(""), "");
    }

    #[test]
    fn pairing_code_is_one_time() {
        let state = PairingState::new(600);
        let code = state.view().code.clone();
        assert_eq!(state.consume(&code.to_lowercase()), PairCheck::Ok);
        // 第二次用同一个码必须失败。
        assert_eq!(state.consume(&code), PairCheck::Used);
        assert!(!state.view().is_active());
    }

    #[test]
    fn expired_code_rejected() {
        let state = PairingState::new(0);
        let code = state.view().code.clone();
        assert_eq!(state.consume(&code), PairCheck::Expired);
    }

    #[test]
    fn wrong_code_does_not_consume() {
        let state = PairingState::new(600);
        let code = state.view().code.clone();
        assert_eq!(state.consume("ZZZZZZZZ"), PairCheck::Mismatch);
        // 用错的码不会把真码作废。
        assert_eq!(state.consume(&code), PairCheck::Ok);
    }

    #[test]
    fn rotate_invalidates_old_code() {
        let state = PairingState::new(600);
        let old = state.view().code.clone();
        let new = state.rotate(600).code;
        assert_ne!(old, new);
        assert_eq!(state.consume(&old), PairCheck::Mismatch);
        assert_eq!(state.consume(&new), PairCheck::Ok);
    }

    #[test]
    fn ttl_view_reports_remaining() {
        let state = PairingState::new(600);
        let v = state.view();
        assert!(v.is_active());
        assert!(v.remaining_ms() > 590_000);
        assert!(v.remaining_ms() <= 600_000);
    }

    #[test]
    fn token_is_32_bytes_hex_and_unique() {
        let a = new_token();
        let b = new_token();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn sha256_matches_known_vector() {
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // token 原文不出现在哈希里。
        let token = new_token();
        let hash = sha256_hex(&token);
        assert_ne!(token, hash);
        assert_eq!(hash.len(), 64);
    }

    #[test]
    fn constant_time_compare() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn qr_png_written() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("pair.png");
        write_qr_png(&path, "http://192.168.1.10:8787/pair#A2C4E6F8").expect("生成二维码");
        let bytes = std::fs::read(&path).expect("读取 png");
        assert!(bytes.len() > 100, "PNG 太小，可能没写成功");
        // PNG magic。
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
    }
}
