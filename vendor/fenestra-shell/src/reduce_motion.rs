//! Best-effort detection of the OS "reduce motion" accessibility setting.
//!
//! Queried through each platform's own CLI rather than native FFI, so the
//! workspace's `unsafe_code = forbid` holds (no `objc2` / Win32 bindings). The
//! query is cheap and run only at window creation and on focus-regained, so a
//! mid-session toggle is picked up. Any read failure — or an unrecognized
//! platform — reports `false` (full motion), the safe, unchanged default.

#[cfg(all(not(target_arch = "wasm32"), target_os = "macos"))]
fn query() -> bool {
    // `reduceMotion = 1` under the Universal Access domain.
    std::process::Command::new("defaults")
        .args(["read", "com.apple.universalaccess", "reduceMotion"])
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "1")
        .unwrap_or(false)
}

#[cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
fn query() -> bool {
    // GNOME: animations disabled is the reduce-motion signal.
    std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "enable-animations"])
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "false")
        .unwrap_or(false)
}

#[cfg(all(not(target_arch = "wasm32"), target_os = "windows"))]
fn query() -> bool {
    // NEOSONIA patch：原实现 spawn `reg query ... MinAnimate`，窗口创建与
    // **每次重新聚焦**都会拉起一个 reg.exe 控制台进程——管理员模式下会闪窗
    // （用户用 Process Monitor 抓到 reg.exe query WindowMetrics /v MinAnimate）。
    // 改为进程内直接调 SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION)：
    // 无子进程、无注册表读取、语义完全一致（返回"动画是否开启"）。
    #[link(name = "user32")]
    unsafe extern "system" {
        fn SystemParametersInfoW(
            ui_action: u32,
            ui_param: u32,
            pv_param: *mut core::ffi::c_void,
            win_ini: u32,
        ) -> i32;
    }
    const SPI_GETCLIENTAREAANIMATION: u32 = 0x1042;
    let mut animations_enabled: i32 = 1;
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            &mut animations_enabled as *mut i32 as *mut core::ffi::c_void,
            0,
        )
    };
    // 读取失败 → 按"全动画"处理（与旧实现一致）；成功且动画关闭 → 减少动效。
    ok != 0 && animations_enabled == 0
}

#[cfg(all(
    not(target_arch = "wasm32"),
    not(any(target_os = "macos", target_os = "linux", target_os = "windows"))
))]
fn query() -> bool {
    false
}

/// Whether the OS currently requests reduced motion. Drives
/// [`FrameState::reduced_motion`](fenestra_core::FrameState) so animations snap
/// for users who asked for it (WCAG 2.3.3). Non-wasm only; the web build reads
/// `prefers-reduced-motion` through the browser instead.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn os_reduce_motion() -> bool {
    query()
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    #[test]
    fn os_reduce_motion_never_panics() {
        // The value is environment-dependent (the user's accessibility setting);
        // what we guarantee is that querying it is safe and falls back cleanly
        // when the platform CLI is missing.
        let _ = super::os_reduce_motion();
    }
}
