//! 开机自启：Windows 写 HKCU\...\Run，Linux 写 ~/.config/autostart/*.desktop。
//!
//! 自启时由启动器带上 `--autostart` 参数：程序只跑后端（窗口隐藏、托盘+热键可用），
//! 不主动抢屏幕；用户手动双击启动则立刻显示窗口。

use std::path::PathBuf;

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "NeoSonia";

/// 当前可执行文件路径。
fn exe_path() -> Option<PathBuf> {
    std::env::current_exe().ok()
}

/// 自启命令行（带 `--autostart`）。
fn autostart_command() -> Option<String> {
    let exe = exe_path()?;
    Some(format!("\"{}\" --autostart", exe.display()))
}

/// Windows 开机自启用**直接调用 advapi32 注册表 API**，不再 spawn `reg.exe`。
///
/// 用户反馈"尝试连接邮箱后切页等场景又开始弹 reg.exe"——即使 CREATE_NO_WINDOW，
/// 个别环境（安全软件/控制台宿主）仍可能闪窗。直接调 API 可以彻底消除子进程。
#[cfg(windows)]
mod win_reg {
    use std::ffi::c_void;

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn RegOpenKeyExW(
            hkey: isize,
            sub_key: *const u16,
            options: u32,
            sam: u32,
            out: *mut isize,
        ) -> i32;
        fn RegCreateKeyExW(
            hkey: isize,
            sub_key: *const u16,
            reserved: u32,
            class: *const u16,
            options: u32,
            sam: u32,
            security: *mut c_void,
            out: *mut isize,
            disposition: *mut u32,
        ) -> i32;
        fn RegQueryValueExW(
            hkey: isize,
            value: *const u16,
            reserved: *mut u32,
            ty: *mut u32,
            data: *mut u8,
            len: *mut u32,
        ) -> i32;
        fn RegSetValueExW(
            hkey: isize,
            value: *const u16,
            reserved: u32,
            ty: u32,
            data: *const u8,
            len: u32,
        ) -> i32;
        fn RegDeleteValueW(hkey: isize, value: *const u16) -> i32;
        fn RegCloseKey(hkey: isize) -> i32;
    }

    const HKEY_CURRENT_USER: isize = 0x8000_0001u32 as isize;
    const KEY_QUERY_VALUE: u32 = 0x0001;
    const KEY_SET_VALUE: u32 = 0x0002;
    const REG_SZ: u32 = 1;
    const ERROR_FILE_NOT_FOUND: i32 = 2;
    const ERROR_SUCCESS: i32 = 0;

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE_NAME: &str = "NeoSonia";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// 读 HKCU\...\Run\NeoSonia 是否存在。
    pub fn is_enabled() -> bool {
        let sub = wide(RUN_KEY);
        let name = wide(VALUE_NAME);
        let mut hkey: isize = 0;
        unsafe {
            let st = RegOpenKeyExW(
                HKEY_CURRENT_USER,
                sub.as_ptr(),
                0,
                KEY_QUERY_VALUE,
                &mut hkey,
            );
            if st != ERROR_SUCCESS {
                return false;
            }
            let mut ty: u32 = 0;
            let mut len: u32 = 0;
            let q = RegQueryValueExW(
                hkey,
                name.as_ptr(),
                std::ptr::null_mut(),
                &mut ty,
                std::ptr::null_mut(),
                &mut len,
            );
            let _ = RegCloseKey(hkey);
            q == ERROR_SUCCESS
        }
    }

    /// 写入（on=true）或删除（on=false）Run 键值。
    pub fn set_enabled(on: bool, value: &str) -> Result<(), String> {
        let sub = wide(RUN_KEY);
        let name = wide(VALUE_NAME);
        unsafe {
            if on {
                let mut hkey: isize = 0;
                let mut disp: u32 = 0;
                let st = RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    sub.as_ptr(),
                    0,
                    std::ptr::null(),
                    0,
                    KEY_SET_VALUE,
                    std::ptr::null_mut(),
                    &mut hkey,
                    &mut disp,
                );
                if st != ERROR_SUCCESS {
                    return Err(format!("打开注册表 Run 键失败（代码 {st}）"));
                }
                let data: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
                let bytes = data.len() * 2;
                let st = RegSetValueExW(
                    hkey,
                    name.as_ptr(),
                    0,
                    REG_SZ,
                    data.as_ptr().cast::<u8>(),
                    bytes as u32,
                );
                let _ = RegCloseKey(hkey);
                if st != ERROR_SUCCESS {
                    return Err(format!("写入开机自启注册表失败（代码 {st}）"));
                }
                Ok(())
            } else {
                let mut hkey: isize = 0;
                let st = RegOpenKeyExW(
                    HKEY_CURRENT_USER,
                    sub.as_ptr(),
                    0,
                    KEY_SET_VALUE,
                    &mut hkey,
                );
                if st != ERROR_SUCCESS {
                    // 键不存在也算删除成功
                    return Ok(());
                }
                let st = RegDeleteValueW(hkey, name.as_ptr());
                let _ = RegCloseKey(hkey);
                if st != ERROR_SUCCESS && st != ERROR_FILE_NOT_FOUND {
                    return Err(format!("删除开机自启注册表失败（代码 {st}）"));
                }
                Ok(())
            }
        }
    }
}

#[cfg(windows)]
pub fn is_enabled() -> bool {
    win_reg::is_enabled()
}

#[cfg(windows)]
pub fn set_enabled(on: bool) -> anyhow::Result<()> {
    if on {
        let cmd = autostart_command().ok_or_else(|| anyhow::anyhow!("拿不到当前 exe 路径"))?;
        win_reg::set_enabled(true, &cmd).map_err(|e| anyhow::anyhow!(e))
    } else {
        win_reg::set_enabled(false, "").map_err(|e| anyhow::anyhow!(e))
    }
}
#[cfg(not(windows))]
pub fn is_enabled() -> bool {
    tracing::info!("autostart: reg query（is_enabled）");
    desktop_file().map(|p| p.is_file()).unwrap_or(false)
}

#[cfg(not(windows))]
fn desktop_file() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("autostart")
            .join("neosonia.desktop"),
    )
}

#[cfg(not(windows))]
pub fn set_enabled(on: bool) -> anyhow::Result<()> {
    tracing::info!(on, "autostart: reg add/delete（set_enabled）");
    let path = desktop_file().ok_or_else(|| anyhow::anyhow!("找不到 HOME"))?;
    if on {
        let exe = exe_path().ok_or_else(|| anyhow::anyhow!("拿不到 exe 路径"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = format!(
            "[Desktop Entry]\nType=Application\nName=NeoSonia\nComment=常驻桌面的 AI agent\nExec=\"{}\" --autostart\nX-GNOME-Autostart-enabled=true\nTerminal=false\n",
            exe.display()
        );
        std::fs::write(&path, content)?;
    } else {
        let _ = std::fs::remove_file(&path);
    }
    Ok(())
}

/// 把系统里的自启状态同步成 `settings.ui.autostart` 想要的样子。
pub fn sync(desired: bool) -> anyhow::Result<()> {
    if is_enabled() == desired {
        return Ok(());
    }
    set_enabled(desired)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autostart_command_quotes_exe() {
        if let Some(cmd) = autostart_command() {
            assert!(cmd.starts_with('"'));
            assert!(cmd.ends_with("--autostart"));
        }
    }
}
