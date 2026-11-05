//! 已信任设备表（`data/devices.json`）。
//!
//! 结构（契约 5.1）：
//! ```json
//! {"devices":[{"id":"...","name":"...","user_agent":"...","last_addr":"...",
//!              "created_at_ms":0,"last_seen_ms":0,"token_sha256":"..."}]}
//! ```
//! - **只存 sha256(token)**，不存明文；
//! - 原子写（core 的 `util::atomic_write`），避免半截文件；
//! - 文件损坏时备份成 `devices.corrupt-<ts>.json` 并按空表启动（不 panic）；
//! - `last_seen_ms`/`last_addr` 更新节流（默认 30 秒），避免每个请求都写盘。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use neosonia_core::TrustedDevice;
use neosonia_core::util::{atomic_write, now_ms};
use serde::{Deserialize, Serialize};

use crate::pairing::{constant_time_eq, new_token, sha256_hex};

/// 超过这个时间没见到的设备不算"在线"。
const ONLINE_WINDOW_MS: i64 = 120_000;
/// last_seen 落盘节流窗口。
const SEEN_SAVE_THROTTLE_MS: i64 = 30_000;
/// 设备名最大长度（字符）。
const MAX_NAME_CHARS: usize = 32;
/// UA 摘要最大长度（字节）。
const MAX_UA_BYTES: usize = 200;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct DevicesFile {
    #[serde(default)]
    devices: Vec<DeviceRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DeviceRecord {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    user_agent: String,
    #[serde(default)]
    last_addr: String,
    #[serde(default)]
    created_at_ms: i64,
    #[serde(default)]
    last_seen_ms: i64,
    #[serde(default)]
    token_sha256: String,
}

#[derive(Debug)]
struct StoreState {
    file: DevicesFile,
    last_save_ms: i64,
}

/// 设备数量超限。
#[derive(Debug)]
pub enum PairError {
    TooMany(u32),
    Save(String),
}

impl std::fmt::Display for PairError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PairError::TooMany(max) => write!(
                f,
                "已信任设备数达到上限（{max}）。请先在桌面端「远程访问」页撤销一台旧设备。"
            ),
            PairError::Save(e) => write!(f, "保存设备表失败：{e}"),
        }
    }
}

/// 设备表。线程安全。
pub struct DeviceStore {
    path: PathBuf,
    state: Mutex<StoreState>,
}

impl DeviceStore {
    /// 从磁盘加载；文件不存在/损坏都降级为空表。
    pub fn load(path: &Path) -> Self {
        let file = read_file_tolerant(path);
        Self {
            path: path.to_path_buf(),
            state: Mutex::new(StoreState { file, last_save_ms: 0 }),
        }
    }

    /// 设备列表（`online` 按最近活动时间推算）。
    pub fn list(&self) -> Vec<TrustedDevice> {
        let now = now_ms();
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .file
            .devices
            .iter()
            .map(|r| to_public(r, now))
            .collect()
    }

    /// 已信任设备数。
    pub fn count(&self) -> usize {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.file.devices.len()
    }

    /// 配对一台新设备：生成 token（明文只在返回值里出现一次），只存 sha256。
    pub fn pair(
        &self,
        name: &str,
        user_agent: &str,
        addr: &str,
        max_devices: u32,
    ) -> Result<(String, TrustedDevice), PairError> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.file.devices.len() as u32 >= max_devices {
            return Err(PairError::TooMany(max_devices));
        }
        let token = new_token();
        let now = now_ms();
        let record = DeviceRecord {
            id: uuid::Uuid::new_v4().to_string(),
            name: sanitize_name(name),
            user_agent: sanitize_ua(user_agent),
            last_addr: addr.to_string(),
            created_at_ms: now,
            last_seen_ms: now,
            token_sha256: sha256_hex(&token),
        };
        let public = to_public(&record, now);
        state.file.devices.push(record);
        save(&self.path, &state.file).map_err(|e| PairError::Save(e.to_string()))?;
        state.last_save_ms = now;
        Ok((token, public))
    }

    /// 校验 token：命中设备表则更新 last_seen/last_addr 并返回设备。
    pub fn authenticate(&self, token: &str, addr: Option<&str>) -> Option<TrustedDevice> {
        if token.trim().is_empty() {
            return None;
        }
        let hash = sha256_hex(token.trim());
        let now = now_ms();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut updated: Option<DeviceRecord> = None;
        for rec in state.file.devices.iter_mut() {
            if !constant_time_eq(hash.as_bytes(), rec.token_sha256.as_bytes()) {
                continue;
            }
            let addr_changed = match addr {
                Some(a) if !a.is_empty() => rec.last_addr != a,
                _ => false,
            };
            let stale = now.saturating_sub(rec.last_seen_ms) >= SEEN_SAVE_THROTTLE_MS;
            if stale || addr_changed {
                rec.last_seen_ms = now;
                if let Some(a) = addr {
                    if !a.is_empty() {
                        rec.last_addr = a.to_string();
                    }
                }
                updated = Some(rec.clone());
            } else {
                // 内存里也刷新一点点，但不写盘。
                rec.last_seen_ms = rec.last_seen_ms.max(now);
                updated = Some(rec.clone());
            }
            break;
        }
        let found = updated?;
        // 节流写盘：同一设备的更新最多 30 秒落一次盘。
        if now.saturating_sub(state.last_save_ms) >= SEEN_SAVE_THROTTLE_MS {
            if let Err(e) = save(&self.path, &state.file) {
                tracing::warn!("保存设备表失败：{e}");
            } else {
                state.last_save_ms = now;
            }
        }
        Some(to_public(&found, now))
    }

    /// 撤销一台设备；返回是否删除了记录。
    pub fn revoke(&self, device_id: &str) -> bool {
        let id = device_id.trim();
        if id.is_empty() {
            return false;
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let before = state.file.devices.len();
        state.file.devices.retain(|d| d.id != id);
        if state.file.devices.len() == before {
            return false;
        }
        if let Err(e) = save(&self.path, &state.file) {
            tracing::warn!("撤销设备后保存失败：{e}");
        }
        true
    }

    /// 重命名设备（v0.7）；返回更新后的设备，找不到返回 None。
    pub fn rename(&self, device_id: &str, name: &str) -> Option<TrustedDevice> {
        let id = device_id.trim();
        if id.is_empty() {
            return None;
        }
        let now = now_ms();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let idx = state.file.devices.iter().position(|d| d.id == id)?;
        state.file.devices[idx].name = sanitize_name(name);
        let public = to_public(&state.file.devices[idx], now);
        if let Err(e) = save(&self.path, &state.file) {
            tracing::warn!("重命名设备后保存失败：{e}");
        }
        Some(public)
    }
}

fn to_public(rec: &DeviceRecord, now: i64) -> TrustedDevice {
    TrustedDevice {
        id: rec.id.clone(),
        name: rec.name.clone(),
        user_agent: rec.user_agent.clone(),
        last_addr: rec.last_addr.clone(),
        created_at_ms: rec.created_at_ms,
        last_seen_ms: rec.last_seen_ms,
        online: now.saturating_sub(rec.last_seen_ms) <= ONLINE_WINDOW_MS,
    }
}

fn sanitize_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .chars()
        .take(MAX_NAME_CHARS)
        .collect();
    if cleaned.is_empty() {
        "远程设备".to_string()
    } else {
        cleaned
    }
}

fn sanitize_ua(ua: &str) -> String {
    let cleaned: String = ua.chars().filter(|c| !c.is_control()).collect();
    if cleaned.len() <= MAX_UA_BYTES {
        cleaned
    } else {
        // 按 char 边界截断，避免切坏 UTF-8。
        let mut out = String::new();
        for ch in cleaned.chars() {
            if out.len() + ch.len_utf8() > MAX_UA_BYTES {
                break;
            }
            out.push(ch);
        }
        out
    }
}

fn read_file_tolerant(path: &Path) -> DevicesFile {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return DevicesFile::default(),
    };
    match serde_json::from_slice::<DevicesFile>(&bytes) {
        Ok(file) => DevicesFile {
            devices: file
                .devices
                .into_iter()
                .filter(|d| !d.id.trim().is_empty() && !d.token_sha256.trim().is_empty())
                .collect(),
        },
        Err(e) => {
            tracing::warn!("设备表解析失败（将按空表启动）：{e}");
            backup_corrupt(path);
            DevicesFile::default()
        }
    }
}

fn backup_corrupt(path: &Path) {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "devices".to_string());
    let backup = parent.join(format!("{stem}.corrupt-{}.json", now_ms()));
    if let Err(e) = std::fs::rename(path, &backup) {
        tracing::warn!("备份损坏的设备表失败：{e}");
    }
}

fn save(path: &Path, file: &DevicesFile) -> anyhow::Result<()> {
    let text = serde_json::to_string_pretty(file)?;
    atomic_write(path, text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, DeviceStore) {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("devices.json");
        let store = DeviceStore::load(&path);
        (dir, store)
    }

    #[test]
    fn pair_persists_only_hash() {
        let (dir, store) = store();
        let (token, device) = store
            .pair("我的手机", "Mozilla/5.0", "192.168.1.7", 8)
            .expect("配对成功");
        assert_eq!(device.name, "我的手机");
        assert!(!device.id.is_empty());
        assert_eq!(token.len(), 64);

        let raw = std::fs::read_to_string(dir.path().join("devices.json")).expect("读设备表");
        assert!(!raw.contains(&token), "明文 token 不许落盘");
        assert!(raw.contains(&sha256_hex(&token)), "应只存 sha256");

        // 重新加载后仍可鉴权。
        let reloaded = DeviceStore::load(&dir.path().join("devices.json"));
        assert_eq!(reloaded.count(), 1);
        assert!(reloaded.authenticate(&token, Some("192.168.1.8")).is_some());
    }

    #[test]
    fn wrong_token_rejected() {
        let (_dir, store) = store();
        let (token, _) = store.pair("A", "", "10.0.0.2", 8).unwrap();
        assert!(store.authenticate("deadbeef", None).is_none());
        assert!(store.authenticate("", None).is_none());
        let token2 = format!("{token}0");
        assert!(store.authenticate(&token2, None).is_none());
    }

    #[test]
    fn max_devices_enforced() {
        let (_dir, store) = store();
        store.pair("A", "", "10.0.0.2", 2).unwrap();
        store.pair("B", "", "10.0.0.3", 2).unwrap();
        match store.pair("C", "", "10.0.0.4", 2) {
            Err(PairError::TooMany(2)) => {}
            other => panic!("应返回设备数上限错误：{other:?}"),
        }
    }

    #[test]
    fn revoke_removes_device() {
        let (_dir, store) = store();
        let (token, device) = store.pair("A", "", "10.0.0.2", 8).unwrap();
        assert!(store.revoke(&device.id));
        assert!(!store.revoke(&device.id));
        assert_eq!(store.count(), 0);
        assert!(store.authenticate(&token, None).is_none());
    }

    #[test]
    fn corrupt_file_does_not_panic() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("devices.json");
        std::fs::write(&path, b"{ this is not json").expect("写坏文件");
        let store = DeviceStore::load(&path);
        assert_eq!(store.count(), 0);
        // 坏文件被备份。
        let backups = std::fs::read_dir(dir.path())
            .expect("列目录")
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains("corrupt"))
            .count();
        assert_eq!(backups, 1, "损坏的设备表应被备份");
    }

    #[test]
    fn missing_file_is_empty() {
        let dir = tempfile::tempdir().expect("临时目录");
        let store = DeviceStore::load(&dir.path().join("nope.json"));
        assert_eq!(store.count(), 0);
        assert!(store.list().is_empty());
    }

    #[test]
    fn name_and_ua_are_sanitized() {
        let (_dir, store) = store();
        let long_name: String = "名".repeat(100);
        let long_ua: String = "u".repeat(1000);
        let (_token, device) = store.pair(&long_name, &long_ua, "10.0.0.2", 8).unwrap();
        assert_eq!(device.name.chars().count(), MAX_NAME_CHARS);
        assert!(device.user_agent.len() <= MAX_UA_BYTES);
        // 空名字给默认值。
        let (_t, d2) = store.pair("   ", "", "10.0.0.2", 8).unwrap();
        assert_eq!(d2.name, "远程设备");
    }

    #[test]
    fn online_flag_reflects_last_seen() {
        let (_dir, store) = store();
        let (_token, _device) = store.pair("A", "", "10.0.0.2", 8).unwrap();
        let list = store.list();
        assert_eq!(list.len(), 1);
        assert!(list[0].online, "刚配对的设备应为在线");
    }

    #[test]
    fn rename_device_persists() {
        let (dir, store) = store();
        let (_token, device) = store.pair("旧名字", "", "10.0.0.2", 8).unwrap();
        let renamed = store.rename(&device.id, "  客厅 iPad  ").expect("应能重命名");
        assert_eq!(renamed.name, "客厅 iPad");
        // 重新加载后仍然是新名字（已落盘）。
        let reloaded = DeviceStore::load(&dir.path().join("devices.json"));
        assert_eq!(reloaded.list()[0].name, "客厅 iPad");
        // 空/不存在 id 的行为。
        let long: String = "名".repeat(100);
        let clamped = store.rename(&device.id, &long).expect("应能重命名");
        assert_eq!(clamped.name.chars().count(), MAX_NAME_CHARS);
        assert!(store.rename("不存在的-id", "x").is_none());
        assert!(store.rename("", "x").is_none());
    }
}
