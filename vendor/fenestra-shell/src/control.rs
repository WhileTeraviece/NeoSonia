//! NEOSONIA patch：全局窗口控制（显隐 / 停靠 / 置顶 / 聚焦 / 屏幕信息）。
//!
//! 上游 fenestra 的 `App` 拿不到 `winit::Window`，但"右侧常驻浮窗 + 全局快捷键显隐"
//! 必须能操作原生窗口。这里在窗口创建后把 `Arc<Window>` 存进全局槽，
//! 暴露一组线程安全的控制函数给应用层调用（`fenestra_shell::set_visible` 等）。
//!
//! 公开 API（应用层只用这些）：
//! - [`is_visible`] / [`set_visible`] / [`toggle`] / [`focus`]
//! - [`dock_right`] / [`dock_left`] / [`screen_info`]
//! - [`set_always_on_top`] / [`set_outer_position_physical`]

use std::sync::{Arc, Mutex, OnceLock};

use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::window::{Window, WindowLevel};

static WINDOW: OnceLock<Mutex<Option<Arc<Window>>>> = OnceLock::new();

fn slot() -> &'static Mutex<Option<Arc<Window>>> {
    WINDOW.get_or_init(|| Mutex::new(None))
}

fn with_window<R>(f: impl FnOnce(&Arc<Window>) -> R) -> Option<R> {
    let guard = slot().lock().unwrap_or_else(|e| e.into_inner());
    guard.as_ref().map(f)
}

/// 窗口创建后由 runner 调用，登记窗口句柄。
pub fn attach(window: Arc<Window>) {
    #[cfg(windows)]
    apply_windows_polish(&window);
    let mut guard = slot().lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(window);
}

/// NEOSONIA patch：Windows 11 上给无边框浮窗加原生圆角 + 深色边框，
/// 并去掉系统绘制的 1px 亮边。这样即使 swapchain 不透明（DX12 集显驱动
/// 只支持 Opaque），窗口轮廓仍是圆角的浮层，而不是生硬的矩形。
#[cfg(windows)]
fn apply_windows_polish(window: &Window) {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    #[link(name = "dwmapi")]
    unsafe extern "system" {
        fn DwmSetWindowAttribute(
            hwnd: isize,
            attr: u32,
            pv: *const core::ffi::c_void,
            cb: u32,
        ) -> i32;
    }

    let Ok(handle) = window.window_handle() else { return };
    let RawWindowHandle::Win32(w32) = handle.as_raw() else { return };
    let hwnd = w32.hwnd.get();

    const DWMWA_USE_IMMERSIVE_DARK_MODE: u32 = 20;
    const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
    const DWMWA_BORDER_COLOR: u32 = 34;
    const DWMWCP_ROUND: i32 = 2;
    const DWMWA_COLOR_NONE: u32 = 0xFFFF_FFFE;

    unsafe {
        let dark: i32 = 1;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            (&dark as *const i32).cast(),
            4,
        );
        let round: i32 = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            (&round as *const i32).cast(),
            4,
        );
        let none: u32 = DWMWA_COLOR_NONE;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            (&none as *const u32).cast(),
            4,
        );
    }
}

/// runner 销毁窗口时调用，避免悬垂引用。
pub fn detach() {
    let mut guard = slot().lock().unwrap_or_else(|e| e.into_inner());
    *guard = None;
}

/// 屏幕（窗口所在显示器）的物理尺寸与缩放。
#[derive(Clone, Copy, Debug)]
pub struct ScreenInfo {
    pub width_physical: f64,
    pub height_physical: f64,
    pub scale: f64,
}

pub fn screen_info() -> Option<ScreenInfo> {
    with_window(|w| {
        let monitor = w.current_monitor().or_else(|| w.primary_monitor())?;
        let size = monitor.size();
        Some(ScreenInfo {
            width_physical: size.width as f64,
            height_physical: size.height as f64,
            scale: monitor.scale_factor(),
        })
    })
    .flatten()
}

pub fn is_visible() -> bool {
    with_window(|w| w.is_visible().unwrap_or(true)).unwrap_or(false)
}

/// 显示/隐藏窗口。返回是否成功拿到窗口。
pub fn set_visible(visible: bool) -> bool {
    let ok = with_window(|w| {
        w.set_visible(visible);
        if visible {
            w.focus_window();
        }
    })
    .is_some();
    ok
}

/// 切换显隐，返回切换后的可见状态。
pub fn toggle() -> Option<bool> {
    let next = !is_visible();
    if set_visible(next) { Some(next) } else { None }
}

pub fn focus() {
    with_window(|w| w.focus_window());
}

/// 开始原生窗口拖拽（无边框窗口的标题栏拖动用）。
/// 拖拽期间由 winit 接管，直到鼠标抬起。
pub fn start_dragging() {
    with_window(|w| {
        let _ = w.drag_window();
    });
}

pub fn set_always_on_top(on: bool) {
    with_window(|w| {
        w.set_window_level(if on { WindowLevel::AlwaysOnTop } else { WindowLevel::Normal })
    });
}

pub fn set_outer_position_physical(x: f64, y: f64) {
    with_window(|w| {
        w.set_outer_position(PhysicalPosition::new(x.round() as i32, y.round() as i32))
    });
}

/// 把窗口贴到屏幕右侧（或左侧）：`margin` 逻辑像素留白，
/// `width` 逻辑宽度，`height_ratio` 占屏高比例。
pub fn dock(side_right: bool, margin: f64, width: f64, height_ratio: f64) -> bool {
    let Some(info) = screen_info() else { return false };
    let scale = info.scale;
    let phys_w = (width * scale).round();
    let phys_h = (info.height_physical * height_ratio.clamp(0.2, 1.0)).round();
    let gap = (margin * scale).round();
    let x = if side_right {
        info.width_physical - phys_w - gap
    } else {
        gap
    };
    // 垂直居中，避免压住任务栏
    let y = ((info.height_physical - phys_h) / 2.0).floor();
    set_outer_position_physical(x, y);
    with_window(|w| {
        let _ = w.request_inner_size(LogicalSize::new(
            width,
            (phys_h / scale).max(320.0),
        ));
    });
    true
}

/// NEOSONIA patch：是否在任务栏显示图标。
///
/// winit 0.30 没有运行时切换任务栏的 API，这里直接改 Win32 扩展样式：
/// `WS_EX_TOOLWINDOW`（不进任务栏 + 不进 Alt-Tab）与 `WS_EX_APPWINDOW`。
#[cfg(windows)]
pub fn set_skip_taskbar(skip: bool) {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetWindowLongPtrW(hwnd: isize, index: i32) -> isize;
        fn SetWindowLongPtrW(hwnd: isize, index: i32, value: isize) -> isize;
        fn SetWindowPos(
            hwnd: isize,
            after: isize,
            x: i32,
            y: i32,
            cx: i32,
            cy: i32,
            flags: u32,
        ) -> i32;
    }
    const GWL_EXSTYLE: i32 = -20;
    const WS_EX_TOOLWINDOW: isize = 0x0000_0080;
    const WS_EX_APPWINDOW: isize = 0x0004_0000;
    const SWP_NOMOVE: u32 = 0x0002;
    const SWP_NOSIZE: u32 = 0x0001;
    const SWP_NOZORDER: u32 = 0x0004;
    const SWP_FRAMECHANGED: u32 = 0x0020;
    const HWND_TOPMOST: isize = -1;

    with_window(|w| {
        let Ok(handle) = w.window_handle() else { return };
        let RawWindowHandle::Win32(h) = handle.as_raw() else {
            return;
        };
        let hwnd = h.hwnd.get();
        unsafe {
            let mut ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            if skip {
                ex |= WS_EX_TOOLWINDOW;
                ex &= !WS_EX_APPWINDOW;
            } else {
                ex &= !WS_EX_TOOLWINDOW;
                ex |= WS_EX_APPWINDOW;
            }
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex);
            SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
            );
        }
    });
}

#[cfg(not(windows))]
pub fn set_skip_taskbar(_skip: bool) {}

/// NEOSONIA patch：设置窗口图标（RGBA，失败忽略）。用于任务栏/Alt-Tab 显示项目 logo。
pub fn set_window_icon(width: u32, height: u32, rgba: Vec<u8>) {
    with_window(|w| {
        match winit::window::Icon::from_rgba(rgba, width, height) {
            Ok(icon) => w.set_window_icon(Some(icon)),
            Err(_) => {}
        }
    });
}
/// NEOSONIA patch：窗口是否被最小化（点任务栏图标会触发）。
pub fn is_minimized() -> bool {
    with_window(|w| w.is_minimized().unwrap_or(false)).unwrap_or(false)
}

/// NEOSONIA patch：恢复最小化窗口（只还原，不改变位置/焦点策略）。
pub fn restore_from_minimized() {
    with_window(|w| {
        if w.is_minimized().unwrap_or(false) {
            w.set_minimized(false);
        }
    });
}