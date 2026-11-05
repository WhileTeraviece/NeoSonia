//! 屏幕活动提示（HUD）。
//!
//! 模型调用「看屏幕 / 操作鼠标键盘 / UIA / CDP」类工具时，主屏四周亮起一圈提示条，
//! 顶部中央再显示一个胶囊提示（例如「NeoSonia 正在查看屏幕」），让用户始终知道
//! AI 正在对电脑做什么；工具结束后约 0.9 秒自动消失。
//!
//! 实现要点：
//! - 提示窗口**在主线程、第一次需要时才创建**：fenestra/winit 的事件循环会替我们泵消息，
//!   所以不需要自己的消息循环；在别的线程建窗口会让 fenestra 的主循环提前退出。
//! - 5 个置顶、点击穿透、不抢焦点的分层窗口：4 条边 + 1 个胶囊。
//! - 自动消失靠 app 的 Tick 调 [`ActivityOverlay::tick`]。
//! - 关闭设置 `ui.computer_activity_indicator` 时不创建任何窗口。
//! - 非 Windows 平台为空实现（只写日志）。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// 提示类型：看图（紫）还是操作（橙）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HudKind {
    /// 正在识别屏幕（screenshot / screen_metrics / cdp_screenshot …）
    Look,
    /// 正在操作鼠标键盘 / 界面（mouse_* / keyboard_* / ui_* / cdp_click …）
    Act,
}

/// 屏幕活动提示控制器（只在主线程使用）。
pub struct ActivityOverlay {
    enabled: Arc<AtomicBool>,
    #[cfg(windows)]
    imp: std::sync::Mutex<Option<imp::HudWindows>>,
}

impl ActivityOverlay {
    pub fn new() -> Self {
        Self {
            enabled: Arc::new(AtomicBool::new(true)),
            #[cfg(windows)]
            imp: std::sync::Mutex::new(None),
        }
    }

    /// 设置里的开关同步过来（false = 立即隐藏、之后不再创建窗口）。
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
        if !on {
            self.hide();
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// 显示提示（一直显示到下次 show/hide 或 tick 到期）。
    pub fn show(&self, kind: HudKind, label: &str) {
        self.present(kind, label, None);
    }

    /// 显示并在 `ms` 毫秒后自动隐藏（工具结束时用）。
    pub fn show_for(&self, kind: HudKind, label: &str, ms: u64) {
        self.present(kind, label, Some(ms));
    }

    /// 立即隐藏。
    pub fn hide(&self) {
        #[cfg(windows)]
        {
            let mut guard = self.imp.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(w) = guard.as_mut() {
                w.hide();
            }
        }
    }

    /// 每 Tick 调一次，处理自动隐藏。
    pub fn tick(&self) {
        if !self.is_enabled() {
            return;
        }
        #[cfg(windows)]
        {
            let mut guard = self.imp.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(w) = guard.as_mut() {
                if w.deadline_passed() {
                    w.hide();
                }
            }
        }
    }

    fn present(&self, kind: HudKind, label: &str, hide_after_ms: Option<u64>) {
        if !self.is_enabled() {
            return;
        }
        #[cfg(windows)]
        {
            let mut guard = self.imp.lock().unwrap_or_else(|e| e.into_inner());
            if guard.is_none() {
                match imp::HudWindows::create() {
                    Ok(w) => *guard = Some(w),
                    Err(e) => {
                        tracing::warn!("屏幕提示窗口创建失败: {e}");
                        return;
                    }
                }
            }
            if let Some(w) = guard.as_mut() {
                w.present(kind, label, hide_after_ms);
            }
        }
        #[cfg(not(windows))]
        {
            tracing::info!(label, ?kind, "屏幕活动提示（非 Windows 平台只写日志）");
        }
    }
}

impl Default for ActivityOverlay {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================
// 工具名 -> 提示文字
// ============================================================

/// 判断一个工具是不是「看屏幕 / 操作电脑」类，返回要显示的提示。
pub fn hud_for_tool(tool: &str) -> Option<(HudKind, &'static str)> {
    let t = tool.trim();
    let look = matches!(
        t,
        "screenshot"
            | "screen_metrics"
            | "screen_size"
            | "list_displays"
            | "cdp_screenshot"
            | "cdp_targets"
    );
    if look {
        return Some((HudKind::Look, "NeoSonia 正在查看屏幕"));
    }
    if t.starts_with("mouse_") {
        return Some((HudKind::Act, "NeoSonia 正在操作鼠标"));
    }
    if t.starts_with("keyboard_") {
        return Some((HudKind::Act, "NeoSonia 正在操作键盘"));
    }
    if t.starts_with("ui_") {
        return Some((HudKind::Act, "NeoSonia 正在操作界面"));
    }
    if t.starts_with("cdp_") || t == "wait_for_window" || t == "foreground_window" {
        return Some((HudKind::Act, "NeoSonia 正在操作窗口"));
    }
    if t.starts_with("clipboard_") {
        return Some((HudKind::Act, "NeoSonia 正在操作剪贴板"));
    }
    None
}

// ============================================================
// Windows 实现
// ============================================================

#[cfg(windows)]
mod imp {
    use super::HudKind;
    use std::time::{Duration, Instant};

    use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        BeginPaint, CreateFontW, CreatePen, CreateSolidBrush, DeleteObject, DrawTextW, Ellipse,
        EndPaint, FillRect, GetDC, GetDeviceCaps, InvalidateRect, LOGPIXELSY, PAINTSTRUCT,
        ReleaseDC, RoundRect, SelectObject, SetBkMode, SetTextColor, DT_CENTER, DT_SINGLELINE,
        DT_VCENTER, HGDIOBJ, PS_SOLID, TRANSPARENT,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, GetClientRect, GetSystemMetrics, HWND_TOPMOST,
        RegisterClassW, SW_HIDE, SW_SHOWNOACTIVATE, SM_CXSCREEN, SM_CYSCREEN,
        SetLayeredWindowAttributes, SetWindowPos, ShowWindow, WNDCLASSW, WS_EX_LAYERED,
        WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP, LWA_ALPHA,
        SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    };
    use windows::core::{HSTRING, PCWSTR};

    /// 只做默认处理：HUD 窗口的内容由 `paint_all` 直接画到窗口 DC。
    unsafe extern "system" fn default_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    const ALPHA: u8 = 235;
    const BARS: usize = 4;

    /// HWND 不是 Send/Sync，但这里所有调用都发生在主线程；用一层包装明确声明。
    #[derive(Clone, Copy)]
    pub(super) struct Handle(pub HWND);
    unsafe impl Send for Handle {}
    unsafe impl Sync for Handle {}

    pub struct HudWindows {
        pill: Handle,
        bars: [Handle; BARS],
        font: HGDIOBJ,
        scale: i32,
        kind: HudKind,
        label: String,
        deadline: Option<Instant>,
    }

    impl HudWindows {
        pub fn create() -> windows::core::Result<Self> {
            unsafe {
                let hinstance = GetModuleHandleW(None)?;
                let scale = {
                    let dc = GetDC(None);
                    let dpi = GetDeviceCaps(dc, LOGPIXELSY);
                    let _ = ReleaseDC(None, dc);
                    if dpi > 0 { (dpi / 96).max(1) } else { 2 }
                };

                let class_pill = HSTRING::from("NeoSoniaHudPill");
                let class_bar = HSTRING::from("NeoSoniaHudBar");
                for (name, is_pill) in [(&class_pill, true), (&class_bar, false)] {
                    let mut wc = WNDCLASSW::default();
                    wc.hInstance = hinstance.into();
                    wc.lpszClassName = PCWSTR(name.as_ptr());
                    wc.lpfnWndProc = Some(default_proc);
                    let atom = RegisterClassW(&wc);
                    if atom == 0 {
                        let err = windows::core::Error::from_win32();
                        if err.code() != windows::Win32::Foundation::ERROR_CLASS_ALREADY_EXISTS.to_hresult() {
                            return Err(err);
                        }
                    }
                }

                let font_h = CreateFontW(
                    -15 * scale,
                    0,
                    0,
                    0,
                    500,
                    0,
                    0,
                    0,
                    1,
                    0,
                    0,
                    0,
                    0,
                    &HSTRING::from("Microsoft YaHei UI"),
                );
                let font = HGDIOBJ(font_h.0);

                let ex = WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW
                    | WS_EX_TRANSPARENT
                    | WS_EX_LAYERED
                    | WS_EX_NOACTIVATE;
                let mut bars = [Handle(HWND::default()); BARS];
                for slot in bars.iter_mut() {
                    let h = CreateWindowExW(
                        ex,
                        &class_bar,
                        &HSTRING::from(""),
                        WS_POPUP,
                        0,
                        0,
                        10,
                        10,
                        None,
                        None,
                        hinstance,
                        None,
                    )?;
                    let _ = SetLayeredWindowAttributes(h, COLORREF(0), ALPHA, LWA_ALPHA);
                    let _ = ShowWindow(h, SW_HIDE);
                    *slot = Handle(h);
                }
                let pill = CreateWindowExW(
                    ex,
                    &class_pill,
                    &HSTRING::from(""),
                    WS_POPUP,
                    0,
                    0,
                    10,
                    10,
                    None,
                    None,
                    hinstance,
                    None,
                )?;
                let _ = SetLayeredWindowAttributes(pill, COLORREF(0), ALPHA, LWA_ALPHA);
                let _ = ShowWindow(pill, SW_HIDE);

                Ok(Self {
                    pill: Handle(pill),
                    bars,
                    font,
                    scale,
                    kind: HudKind::Look,
                    label: String::new(),
                    deadline: None,
                })
            }
        }

        pub fn deadline_passed(&self) -> bool {
            matches!(self.deadline, Some(t) if Instant::now() >= t)
        }

        pub fn hide(&mut self) {
            unsafe {
                for b in self.bars.iter() {
                    let _ = ShowWindow(b.0, SW_HIDE);
                }
                let _ = ShowWindow(self.pill.0, SW_HIDE);
            }
            self.deadline = None;
        }

        pub fn present(&mut self, kind: HudKind, label: &str, hide_after_ms: Option<u64>) {
            self.kind = kind;
            self.label = label.to_string();
            self.deadline = hide_after_ms.map(|ms| Instant::now() + Duration::from_millis(ms));
            paint_state(kind, &self.label, self.scale);
            set_font(self.font);
            self.reposition();
            unsafe {
                for b in self.bars.iter() {
                    let _ = ShowWindow(b.0, SW_SHOWNOACTIVATE);
                    let _ = SetWindowPos(
                        b.0,
                        HWND_TOPMOST,
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                    );
                    let _ = InvalidateRect(b.0, None, true);
                }
                let _ = ShowWindow(self.pill.0, SW_SHOWNOACTIVATE);
                let _ = SetWindowPos(
                    self.pill.0,
                    HWND_TOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
            // 直接画到窗口 DC：fenestra/winit 的消息泵只泵它自己的窗口，
            // 我们的 HUD 窗口收不到 WM_PAINT，所以不能等重绘消息。
            unsafe { self.paint_all() };
        }

        unsafe fn paint_all(&self) {
            let accent = accent_of(self.kind);
            for b in self.bars.iter() {
                unsafe {
                    let dc = GetDC(b.0);
                    if dc.is_invalid() {
                        continue;
                    }
                    let mut r = RECT::default();
                    let _ = GetClientRect(b.0, &mut r);
                    let brush = CreateSolidBrush(accent);
                    FillRect(dc, &r, brush);
                    let _ = DeleteObject(brush);
                    let _ = ReleaseDC(b.0, dc);
                }
            }
            unsafe {
                let dc = GetDC(self.pill.0);
                if dc.is_invalid() {
                    return;
                }
                let mut r = RECT::default();
                let _ = GetClientRect(self.pill.0, &mut r);
                let scale = self.scale;
                let bg = CreateSolidBrush(COLORREF(0x00141112));
                let pen = CreatePen(PS_SOLID, 2, accent);
                let old_b = SelectObject(dc, bg);
                let old_p = SelectObject(dc, pen);
                let _ = RoundRect(dc, r.left + 1, r.top + 1, r.right - 1, r.bottom - 1, 22, 22);
                SelectObject(dc, old_b);
                SelectObject(dc, old_p);
                let _ = DeleteObject(bg);
                let _ = DeleteObject(pen);

                let dot = CreateSolidBrush(accent);
                let dot_pen = CreatePen(PS_SOLID, 1, accent);
                let old_db = SelectObject(dc, dot);
                let old_dp = SelectObject(dc, dot_pen);
                let cy = (r.top + r.bottom) / 2;
                let cx = r.left + 11 * scale;
                let rad = 3 * scale;
                let _ = Ellipse(dc, cx - rad, cy - rad, cx + rad, cy + rad);
                SelectObject(dc, old_db);
                SelectObject(dc, old_dp);
                let _ = DeleteObject(dot);
                let _ = DeleteObject(dot_pen);

                SetBkMode(dc, TRANSPARENT);
                SetTextColor(dc, COLORREF(0x00f4eff0));
                let old_font = SelectObject(dc, self.font);
                let mut tr = RECT {
                    left: r.left + 20 * scale,
                    top: r.top,
                    right: r.right - 8 * scale,
                    bottom: r.bottom,
                };
                let mut wide: Vec<u16> = self.label.encode_utf16().collect();
                DrawTextW(dc, &mut wide, &mut tr, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
                SelectObject(dc, old_font);
                let _ = ReleaseDC(self.pill.0, dc);
            }
        }

        fn reposition(&mut self) {
            unsafe {
                let w = GetSystemMetrics(SM_CXSCREEN);
                let h = GetSystemMetrics(SM_CYSCREEN);
                let t = (3 * self.scale).max(4);
                let rects: [(i32, i32, i32, i32); 4] = [
                    (0, 0, w, t),
                    (0, h - t, w, h),
                    (0, 0, t, h),
                    (w - t, 0, w, h),
                ];
                for (slot, r) in self.bars.iter_mut().zip(rects.iter()) {
                    let _ = SetWindowPos(
                        slot.0,
                        HWND_TOPMOST,
                        r.0,
                        r.1,
                        r.2 - r.0,
                        r.3 - r.1,
                        SWP_NOACTIVATE,
                    );
                }
                let text_w: i32 = self
                    .label
                    .chars()
                    .map(|c| if c.is_ascii() { 8 } else { 17 })
                    .sum::<i32>();
                let pill_w = (text_w + 72 * self.scale).max(230 * self.scale);
                let pill_h = 24 * self.scale;
                let x0 = (w - pill_w) / 2;
                let y0 = 22 * self.scale;
                let _ = SetWindowPos(
                    self.pill.0,
                    HWND_TOPMOST,
                    x0,
                    y0,
                    pill_w,
                    pill_h,
                    SWP_NOACTIVATE,
                );
            }
        }

        pub(super) fn font_handle(&self) -> HGDIOBJ {
            self.font
        }
    }

    thread_local! {
        /// 当前提示颜色（窗口过程读取）
        static HUD_ACCENT: std::cell::Cell<COLORREF> = const { std::cell::Cell::new(COLORREF(0)) };
        /// 胶囊文字/颜色/缩放
        static HUD_PILL: std::cell::RefCell<(COLORREF, String, i32)> =
            const { std::cell::RefCell::new((COLORREF(0), String::new(), 2)) };
        /// 胶囊字体
        static HUD_FONT: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
    }

    fn accent_of(kind: HudKind) -> COLORREF {
        match kind {
            HudKind::Look => COLORREF(0x00ff9cb8),
            HudKind::Act => COLORREF(0x006595ff),
        }
    }

    fn paint_state(kind: HudKind, label: &str, scale: i32) {
        HUD_ACCENT.with(|c| c.set(accent_of(kind)));
        HUD_PILL.with(|c| *c.borrow_mut() = (accent_of(kind), label.to_string(), scale));
    }

    pub(super) fn set_font(font: HGDIOBJ) {
        HUD_FONT.with(|c| c.set(font.0 as isize));
    }

    unsafe extern "system" fn bar_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe {
            match msg {
                windows::Win32::UI::WindowsAndMessaging::WM_ERASEBKGND => LRESULT(1),
                windows::Win32::UI::WindowsAndMessaging::WM_PAINT => {
                    paint_bar(hwnd, HUD_ACCENT.with(|c| c.get()));
                    LRESULT(0)
                }
                _ => DefWindowProcW(hwnd, msg, wparam, lparam),
            }
        }
    }

    unsafe extern "system" fn pill_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe {
            match msg {
                windows::Win32::UI::WindowsAndMessaging::WM_ERASEBKGND => LRESULT(1),
                windows::Win32::UI::WindowsAndMessaging::WM_PAINT => {
                    let (accent, label, scale) = HUD_PILL.with(|c| {
                        let s = c.borrow();
                        (s.0, s.1.clone(), s.2)
                    });
                    let font = HGDIOBJ(HUD_FONT.with(|c| c.get()) as *mut core::ffi::c_void);
                    paint_pill(hwnd, accent, &label, scale, font);
                    LRESULT(0)
                }
                _ => DefWindowProcW(hwnd, msg, wparam, lparam),
            }
        }
    }

    unsafe fn paint_bar(hwnd: HWND, accent: COLORREF) {
        unsafe {
            let mut ps = PAINTSTRUCT::default();
            let dc = BeginPaint(hwnd, &mut ps);
            let brush = CreateSolidBrush(accent);
            let mut r = RECT::default();
            let _ = GetClientRect(hwnd, &mut r);
            FillRect(dc, &r, brush);
            let _ = DeleteObject(brush);
            let _ = EndPaint(hwnd, &ps);
        }
    }

    unsafe fn paint_pill(hwnd: HWND, accent: COLORREF, label: &str, scale: i32, font: HGDIOBJ) {
        unsafe {
            let mut ps = PAINTSTRUCT::default();
            let dc = BeginPaint(hwnd, &mut ps);
            let mut r = RECT::default();
            let _ = GetClientRect(hwnd, &mut r);

            let bg = CreateSolidBrush(COLORREF(0x00141112));
            let pen = CreatePen(PS_SOLID, 2, accent);
            let old_b = SelectObject(dc, bg);
            let old_p = SelectObject(dc, pen);
            let _ = RoundRect(dc, r.left + 1, r.top + 1, r.right - 1, r.bottom - 1, 22, 22);
            SelectObject(dc, old_b);
            SelectObject(dc, old_p);
            let _ = DeleteObject(bg);
            let _ = DeleteObject(pen);

            let dot = CreateSolidBrush(accent);
            let dot_pen = CreatePen(PS_SOLID, 1, accent);
            let old_db = SelectObject(dc, dot);
            let old_dp = SelectObject(dc, dot_pen);
            let cy = (r.top + r.bottom) / 2;
            let cx = r.left + 11 * scale;
            let rad = 3 * scale;
            let _ = Ellipse(dc, cx - rad, cy - rad, cx + rad, cy + rad);
            SelectObject(dc, old_db);
            SelectObject(dc, old_dp);
            let _ = DeleteObject(dot);
            let _ = DeleteObject(dot_pen);

            SetBkMode(dc, TRANSPARENT);
            SetTextColor(dc, COLORREF(0x00f4eff0));
            let old_font = SelectObject(dc, font);
            let mut tr = RECT {
                left: r.left + 20 * scale,
                top: r.top,
                right: r.right - 8 * scale,
                bottom: r.bottom,
            };
            let mut wide: Vec<u16> = label.encode_utf16().collect();
            DrawTextW(dc, &mut wide, &mut tr, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
            SelectObject(dc, old_font);
            let _ = EndPaint(hwnd, &ps);
        }
    }
}

