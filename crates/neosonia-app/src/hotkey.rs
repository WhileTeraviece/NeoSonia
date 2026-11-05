//! 全局快捷键。
//!
//! - 主快捷键（默认 `Alt+Space`）显示 / 隐藏窗口；
//! - **组合键**：按主快捷键后 `ui.chord_timeout_ms` 内再按 `ui.screenshot_chord`
//!   （默认 `S`）= 立即截图、打开窗口并附加到聊天输入框；
//! - 单键截图快捷键（默认 `Ctrl+Alt+A`）也保留。
//!
//! 实现说明（v0.4 重写）：
//! v0.3 用两个 `global-hotkey` 服务分别轮询，**它们共用一个全局事件通道**，
//! 先轮询的服务会把另一个服务的按键事件取走丢掉 —— 这正是"Alt+Space 没反应"
//! 的根因。现在改成：一个 manager + 一张 id→动作表 + 只在一个地方 drain 事件；
//! 组合键用 `GetAsyncKeyState` 的低位（"自上次调用以来是否按过"）判定，
//! 快速点按也不会漏。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use global_hotkey::GlobalHotKeyEvent;
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::GlobalHotKeyManager;

/// 主快捷键 / 截图快捷键在 pending 位掩码里的位。
const BIT_TOGGLE: u32 = 1;
const BIT_SCREENSHOT: u32 = 2;

/// 一个全局快捷键触发后要做的事。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HotkeyAction {
    /// 显示 / 隐藏窗口。
    ToggleWindow,
    /// 立即截图、打开窗口、附加到聊天框。
    Screenshot,
}

// ============================================================
// 字符串解析（设置页里填的就是这种格式）
// ============================================================

/// 解析后的快捷键：主键虚拟码 + 修饰键。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Spec {
    pub vk: u8,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
}

fn parse_code(raw: &str) -> Option<u8> {
    let s = raw.trim().to_ascii_lowercase();
    let vk = match s.as_str() {
        "space" | "spacebar" => 0x20,
        "enter" | "return" => 0x0D,
        "tab" => 0x09,
        "esc" | "escape" => 0x1B,
        "backspace" => 0x08,
        "delete" | "del" => 0x2E,
        "insert" | "ins" => 0x2D,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" | "pgup" => 0x21,
        "pagedown" | "pgdn" => 0x22,
        "up" | "arrowup" => 0x26,
        "down" | "arrowdown" => 0x28,
        "left" | "arrowleft" => 0x25,
        "right" | "arrowright" => 0x27,
        "minus" | "-" => 0xBD,
        "equal" | "=" => 0xBB,
        "comma" | "," => 0xBC,
        "period" | "." => 0xBE,
        "slash" | "/" => 0xBF,
        "semicolon" | ";" => 0xBA,
        "quote" | "'" => 0xDE,
        "backquote" | "`" => 0xC0,
        "backslash" | "\\" => 0xDC,
        "bracketleft" | "[" => 0xDB,
        "bracketright" | "]" => 0xDD,
        _ => {
            if s.len() == 1 {
                let ch = s.chars().next()?;
                return match ch {
                    'a'..='z' => Some(0x41 + (ch as u8 - b'a')),
                    '0'..='9' => Some(0x30 + (ch as u8 - b'0')),
                    _ => None,
                };
            }
            if let Some(rest) = s.strip_prefix('f') {
                if let Ok(n) = rest.parse::<u8>() {
                    if (1..=12).contains(&n) {
                        return Some(0x70 + n - 1);
                    }
                }
            }
            return None;
        }
    };
    Some(vk)
}

/// 解析 `"Ctrl+Alt+Space"` 这类字符串；裸键（例如 `"F8"`）也允许。
pub fn parse_spec(spec: &str) -> anyhow::Result<Spec> {
    let spec = spec.trim();
    if spec.is_empty() {
        anyhow::bail!("快捷键不能为空");
    }
    let mut out = Spec::default();
    let mut vk: Option<u8> = None;
    for part in spec.split('+') {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        match p.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => out.ctrl = true,
            "alt" | "option" | "opt" => out.alt = true,
            "shift" => out.shift = true,
            "win" | "super" | "meta" | "cmd" | "command" => out.win = true,
            other => {
                let code =
                    parse_code(other).ok_or_else(|| anyhow::anyhow!("无法识别的按键: {other}"))?;
                if vk.is_some() {
                    anyhow::bail!("一个快捷键只能有一个主键: {spec}");
                }
                vk = Some(code);
            }
        }
    }
    out.vk = vk.ok_or_else(|| anyhow::anyhow!("缺少主键，例如 Alt+Space: {spec}"))?;
    Ok(out)
}

/// 规范化显示（"ctrl+alt+space" -> "Ctrl+Alt+Space"）。
pub fn normalize(spec: &str) -> String {
    let mut mods: Vec<&str> = Vec::new();
    let mut key = String::new();
    for part in spec.split('+') {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        match p.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods.push("Ctrl"),
            "alt" | "option" => mods.push("Alt"),
            "shift" => mods.push("Shift"),
            "win" | "super" | "meta" | "cmd" => mods.push("Win"),
            other => {
                let mut chars = other.chars();
                if let Some(first) = chars.next() {
                    let rest: String = chars.collect();
                    let mut s = first.to_ascii_uppercase().to_string();
                    s.push_str(&rest);
                    key = s;
                }
            }
        }
    }
    if key.is_empty() {
        return String::new();
    }
    if mods.is_empty() {
        key
    } else {
        format!("{}+{}", mods.join("+"), key)
    }
}

fn to_hotkey(spec: &Spec) -> HotKey {
    let mut mods = Modifiers::empty();
    if spec.ctrl {
        mods |= Modifiers::CONTROL;
    }
    if spec.alt {
        mods |= Modifiers::ALT;
    }
    if spec.shift {
        mods |= Modifiers::SHIFT;
    }
    if spec.win {
        mods |= Modifiers::SUPER;
    }
    HotKey::new(
        if mods.is_empty() { None } else { Some(mods) },
        code_from_vk(spec.vk),
    )
}

fn code_from_vk(vk: u8) -> Code {
    match vk {
        0x20 => Code::Space,
        0x0D => Code::Enter,
        0x09 => Code::Tab,
        0x1B => Code::Escape,
        0x08 => Code::Backspace,
        0x2E => Code::Delete,
        0x24 => Code::Home,
        0x23 => Code::End,
        0x21 => Code::PageUp,
        0x22 => Code::PageDown,
        0x26 => Code::ArrowUp,
        0x28 => Code::ArrowDown,
        0x25 => Code::ArrowLeft,
        0x27 => Code::ArrowRight,
        0x41..=0x5A => letters(vk - 0x41),
        0x30..=0x39 => digits(vk - 0x30),
        0x70..=0x7B => fkeys(vk - 0x70),
        _ => Code::Space,
    }
}

fn letters(i: u8) -> Code {
    const A: [Code; 26] = [
        Code::KeyA,
        Code::KeyB,
        Code::KeyC,
        Code::KeyD,
        Code::KeyE,
        Code::KeyF,
        Code::KeyG,
        Code::KeyH,
        Code::KeyI,
        Code::KeyJ,
        Code::KeyK,
        Code::KeyL,
        Code::KeyM,
        Code::KeyN,
        Code::KeyO,
        Code::KeyP,
        Code::KeyQ,
        Code::KeyR,
        Code::KeyS,
        Code::KeyT,
        Code::KeyU,
        Code::KeyV,
        Code::KeyW,
        Code::KeyX,
        Code::KeyY,
        Code::KeyZ,
    ];
    A.get(i as usize).copied().unwrap_or(Code::Space)
}

fn digits(i: u8) -> Code {
    const D: [Code; 10] = [
        Code::Digit0,
        Code::Digit1,
        Code::Digit2,
        Code::Digit3,
        Code::Digit4,
        Code::Digit5,
        Code::Digit6,
        Code::Digit7,
        Code::Digit8,
        Code::Digit9,
    ];
    D.get(i as usize).copied().unwrap_or(Code::Space)
}

fn fkeys(i: u8) -> Code {
    const F: [Code; 12] = [
        Code::F1,
        Code::F2,
        Code::F3,
        Code::F4,
        Code::F5,
        Code::F6,
        Code::F7,
        Code::F8,
        Code::F9,
        Code::F10,
        Code::F11,
        Code::F12,
    ];
    F.get(i as usize).copied().unwrap_or(Code::Space)
}

/// 某个虚拟键"自上次调用以来是否按过 / 当前是否按下"。
#[cfg(windows)]
fn key_pressed(vk: u8) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    unsafe { (GetAsyncKeyState(vk as i32) as u16) & 0x8001 != 0 }
}

#[cfg(not(windows))]
fn key_pressed(_vk: u8) -> bool {
    false
}

/// 全局快捷键服务。
pub struct Hotkeys {
    /// 保留 manager：Drop 时注销所有注册。
    manager: Option<GlobalHotKeyManager>,
    ids: HashMap<u32, HotkeyAction>,
    toggle_spec: String,
    screenshot_spec: String,
    chord_vk: u8,
    chord_timeout_ms: u64,
    /// 主快捷键刚按下的时间（组合键窗口）。
    armed_until: std::cell::Cell<Option<Instant>>,
    /// 是否已安装。
    installed: bool,
}

impl Hotkeys {
    pub fn new() -> Self {
        Self {
            manager: None,
            ids: HashMap::new(),
            toggle_spec: String::new(),
            screenshot_spec: String::new(),
            chord_vk: 0,
            chord_timeout_ms: 1500,
            armed_until: std::cell::Cell::new(None),
            installed: false,
        }
    }

    /// 安装 / 热更新快捷键。`send` 参数保留是为了兼容旧调用点，事件在 [`Self::poll`] 里取。
    pub fn install<F>(
        &mut self,
        toggle_spec: &str,
        screenshot_spec: &str,
        chord: &str,
        chord_timeout_ms: u64,
        _send: F,
    ) -> anyhow::Result<()>
    where
        F: Fn(HotkeyAction) + Send + Sync + 'static,
    {
        // 用户明确要求：Alt+Space 只负责显示/隐藏，截图只用 Ctrl+Alt+A。默认关闭组合键。
        let chord_vk = if chord.trim().is_empty() {
            0
        } else {
            parse_code(chord).ok_or_else(|| anyhow::anyhow!("组合键无法识别: {chord}"))?
        };
        if self.installed
            && self.toggle_spec == toggle_spec
            && self.screenshot_spec == screenshot_spec
            && self.chord_vk == chord_vk
            && self.chord_timeout_ms == chord_timeout_ms
        {
            return Ok(());
        }
        self.uninstall();
        self.toggle_spec = toggle_spec.to_string();
        self.screenshot_spec = screenshot_spec.to_string();
        self.chord_vk = chord_vk;
        self.chord_timeout_ms = chord_timeout_ms;
        self.armed_until.set(None);
        self.installed = true;

        #[cfg(windows)]
        {
            let mut specs: Vec<(u32, Spec)> = Vec::new();
            if !toggle_spec.trim().is_empty() {
                specs.push((BIT_TOGGLE, parse_spec(toggle_spec)?));
            }
            if !screenshot_spec.trim().is_empty() && screenshot_spec != toggle_spec {
                if let Ok(s) = parse_spec(screenshot_spec) {
                    specs.push((BIT_SCREENSHOT, s));
                }
            }
            native::install(&specs)?;
            tracing::info!(
                toggle = %normalize(toggle_spec),
                screenshot = %normalize(screenshot_spec),
                chord = %format!("{}+{}", normalize(toggle_spec), chord),
                "全局快捷键已注册"
            );
            return Ok(());
        }
        #[cfg(not(windows))]
        {
            let manager = GlobalHotKeyManager::new()
                .map_err(|e| anyhow::anyhow!("全局快捷键初始化失败: {e}"))?;
            if !toggle_spec.trim().is_empty() {
                let hk = to_hotkey(&parse_spec(toggle_spec)?);
                manager
                    .register(hk)
                    .map_err(|e| anyhow::anyhow!("快捷键 {toggle_spec} 注册失败: {e}"))?;
                self.ids.insert(hk.id(), HotkeyAction::ToggleWindow);
            }
            if !screenshot_spec.trim().is_empty() && screenshot_spec != toggle_spec {
                if let Ok(s) = parse_spec(screenshot_spec) {
                    let hk = to_hotkey(&s);
                    if manager.register(hk).is_ok() {
                        self.ids.insert(hk.id(), HotkeyAction::Screenshot);
                    }
                }
            }
            self.manager = Some(manager);
            tracing::info!(
                toggle = %normalize(toggle_spec),
                screenshot = %normalize(screenshot_spec),
                "全局快捷键已注册"
            );
            Ok(())
        }
    }

    /// 注销全部快捷键。
    pub fn uninstall(&mut self) {
        #[cfg(windows)]
        native::uninstall();
        self.manager = None;
        self.ids.clear();
        self.installed = false;
    }
    /// 当前主快捷键的规范化字符串。
    pub fn toggle_spec(&self) -> &str {
        &self.toggle_spec
    }

    /// 完整展示（设置页提示用）。
    pub fn describe(&self) -> String {
        format!(
            "{} 显示/隐藏；{} 截图",
            normalize(&self.toggle_spec),
            normalize(&self.screenshot_spec)
        )
    }

    /// 轮询快捷键事件（Tick 里调用）。返回本轮要执行的动作。
    pub fn poll(&self) -> Vec<HotkeyAction> {
        let mut out = Vec::new();
        #[cfg(windows)]
        {
            let bits = native::poll();
            if bits & BIT_TOGGLE != 0 {
                if self.chord_vk != 0 {
                    self.armed_until.set(Some(
                        Instant::now() + Duration::from_millis(self.chord_timeout_ms),
                    ));
                }
                out.push(HotkeyAction::ToggleWindow);
            }
            if bits & BIT_SCREENSHOT != 0 {
                out.push(HotkeyAction::Screenshot);
            }
        }
        #[cfg(not(windows))]
        {
            let rx = GlobalHotKeyEvent::receiver();
            while let Ok(ev) = rx.try_recv() {
                if ev.state() != global_hotkey::HotKeyState::Pressed {
                    continue;
                }
                match self.ids.get(&ev.id()) {
                    Some(HotkeyAction::ToggleWindow) => {
                        if self.chord_vk != 0 {
                            self.armed_until.set(Some(
                                Instant::now() + Duration::from_millis(self.chord_timeout_ms),
                            ));
                        }
                        out.push(HotkeyAction::ToggleWindow);
                    }
                    Some(HotkeyAction::Screenshot) => out.push(HotkeyAction::Screenshot),
                    None => {}
                }
            }
        }
        // 组合键：窗口期内按了 screenshot_chord 就补一个截图动作
        if let Some(until) = self.armed_until.get() {
            if Instant::now() <= until {
                if self.chord_vk != 0 && key_pressed(self.chord_vk) {
                    self.armed_until.set(None);
                    out.push(HotkeyAction::Screenshot);
                }
            } else {
                self.armed_until.set(None);
            }
        }
        out
    }
}
impl Default for Hotkeys {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================
// Windows：独立线程 + 隐藏消息窗口 + RegisterHotKey
// ============================================================
//
// 为什么单独起线程：快捷键窗口必须有自己的消息循环。放在主线程时，
// 它依赖 winit 的消息泵只分发一次（我们实测只有第一次按键能收到），
// 而且主线程一忙（截图/IO）就会丢事件。独立线程后事件稳定，
// 主线程只需要 `poll()` 取位掩码。

#[cfg(windows)]
mod native {
    use super::{BIT_SCREENSHOT, BIT_TOGGLE, Spec};
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Mutex, OnceLock};

    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN, RegisterHotKey,
        UnregisterHotKey,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, HWND_MESSAGE,
        PostMessageW, RegisterClassW, TranslateMessage, WM_APP, WM_HOTKEY, WNDCLASSW, MSG,
        WS_OVERLAPPED,
    };
    use windows::core::{HSTRING, PCWSTR};

    const WM_RELOAD: u32 = WM_APP + 21;

    static PENDING: AtomicU32 = AtomicU32::new(0);
    static STARTED: AtomicBool = AtomicBool::new(false);
    /// 等待线程注册的快捷键规格（线程启动/重载时读取）。
    static SPECS: OnceLock<Mutex<Vec<(u32, Spec)>>> = OnceLock::new();

    unsafe extern "system" fn hotkey_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe {
            if msg == WM_HOTKEY {
                let id = wparam.0 as u32;
                PENDING.fetch_or(id, Ordering::SeqCst);
                return LRESULT(0);
            }
            if msg == WM_RELOAD {
                apply_specs(hwnd);
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
    }

    fn mods_of(spec: &Spec) -> HOT_KEY_MODIFIERS {
        let mut m = HOT_KEY_MODIFIERS(0);
        if spec.alt {
            m |= MOD_ALT;
        }
        if spec.ctrl {
            m |= MOD_CONTROL;
        }
        if spec.shift {
            m |= MOD_SHIFT;
        }
        if spec.win {
            m |= MOD_WIN;
        }
        m
    }

    unsafe fn apply_specs(hwnd: HWND) {
        unsafe {
            let specs = SPECS
                .get()
                .map(|m| m.lock().unwrap_or_else(|e| e.into_inner()).clone())
                .unwrap_or_default();
            for id in [BIT_TOGGLE, BIT_SCREENSHOT] {
                let _ = UnregisterHotKey(hwnd, id as i32);
            }
            for (id, spec) in specs {
                let ok = RegisterHotKey(hwnd, id as i32, mods_of(&spec), spec.vk as u32);
                if !ok.is_ok() {
                    tracing::warn!(
                        vk = format!("0x{:02X}", spec.vk),
                        "注册快捷键失败（可能被其它程序占用）"
                    );
                }
            }
        }
    }

    /// 启动快捷键线程并注册 `specs`（重复调用 = 热更新）。
    pub fn install(specs: &[(u32, Spec)]) -> anyhow::Result<()> {
        {
            let slot = SPECS.get_or_init(|| Mutex::new(Vec::new()));
            *slot.lock().unwrap_or_else(|e| e.into_inner()) = specs.to_vec();
        }
        if STARTED.swap(true, Ordering::SeqCst) {
            // 已启动：用 PostMessage 让线程重新注册（线程里保存了 hwnd）
            if let Some(raw) = HOST.get() {
                unsafe {
                    let hwnd = HWND(*raw as *mut core::ffi::c_void);
                    let _ = PostMessageW(hwnd, WM_RELOAD, WPARAM(0), LPARAM(0));
                }
            }
            return Ok(());
        }
        std::thread::Builder::new()
            .name("neosonia-hotkeys".into())
            .spawn(|| unsafe {
                let hinstance = match GetModuleHandleW(None) {
                    Ok(h) => h,
                    Err(e) => {
                        tracing::error!("快捷键线程拿模块句柄失败: {e}");
                        return;
                    }
                };
                let class = HSTRING::from("NeoSoniaHotkeyWnd");
                let mut wc = WNDCLASSW::default();
                wc.hInstance = hinstance.into();
                wc.lpszClassName = PCWSTR(class.as_ptr());
                wc.lpfnWndProc = Some(hotkey_proc);
                let atom = RegisterClassW(&wc);
                if atom == 0 {
                    let e = windows::core::Error::from_win32();
                    if e.code()
                        != windows::Win32::Foundation::ERROR_CLASS_ALREADY_EXISTS.to_hresult()
                    {
                        tracing::error!("注册快捷键窗口类失败: {e}");
                        return;
                    }
                }
                let hwnd = match CreateWindowExW(
                    Default::default(),
                    &class,
                    &HSTRING::from("NeoSoniaHotkeys"),
                    WS_OVERLAPPED,
                    0,
                    0,
                    0,
                    0,
                    HWND_MESSAGE,
                    None,
                    hinstance,
                    None,
                ) {
                    Ok(h) => h,
                    Err(e) => {
                        tracing::error!("创建快捷键窗口失败: {e}");
                        return;
                    }
                };
                let _ = HOST.set(hwnd.0 as isize);
                apply_specs(hwnd);
                tracing::info!("快捷键线程就绪");
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            })
            .map_err(|e| anyhow::anyhow!("快捷键线程创建失败: {e}"))?;
        Ok(())
    }

    static HOST: OnceLock<isize> = OnceLock::new();

    pub fn uninstall() {
        if let Some(raw) = HOST.get() {
            unsafe {
                let hwnd = HWND(*raw as *mut core::ffi::c_void);
                for id in [BIT_TOGGLE, BIT_SCREENSHOT] {
                    let _ = UnregisterHotKey(hwnd, id as i32);
                }
            }
        }
        if let Some(slot) = SPECS.get() {
            slot.lock().unwrap_or_else(|e| e.into_inner()).clear();
        }
        PENDING.store(0, Ordering::SeqCst);
    }

    pub fn poll() -> u32 {
        PENDING.swap(0, Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_specs() {
        assert!(parse_spec("Alt+Space").is_ok());
        assert!(parse_spec("ctrl+alt+s").is_ok());
        assert!(parse_spec("F8").is_ok());
        assert!(parse_spec("Ctrl+").is_err());
        assert!(parse_spec("").is_err());
        assert!(parse_spec("Ctrl+Alt+Space+X").is_err());
    }

    #[test]
    fn normalizes() {
        assert_eq!(normalize("ctrl+alt+space"), "Ctrl+Alt+Space");
        assert_eq!(normalize("f8"), "F8");
        assert_eq!(normalize("win+shift+k"), "Win+Shift+K");
        assert_eq!(normalize(""), "");
    }

    #[test]
    fn spec_vk_values() {
        let s = parse_spec("Alt+Space").unwrap();
        assert_eq!(s.vk, 0x20);
        assert!(s.alt && !s.ctrl);
        let s = parse_spec("Ctrl+Alt+A").unwrap();
        assert_eq!(s.vk, 0x41);
        assert!(s.ctrl && s.alt);
        let s = parse_spec("s").unwrap();
        assert_eq!(s.vk, 0x53);
    }
}

/// 当前是否有修饰键（Alt/Ctrl/Shift/Win）还按着。
///
/// Win32 `RegisterHotKey` 的触发依赖按键的按下/抬起状态；如果我们在
/// Alt 还没抬起时就把窗口隐藏掉，系统的热键状态可能卡住（用户实测
/// "第一次能隐藏，之后 Alt+Space 就再也唤不出来"）。所以 toggle 会
/// 等修饰键抬起后再执行。
#[cfg(windows)]
pub fn modifier_down() -> bool {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetAsyncKeyState(vk: i32) -> i16;
    }
    const VK_MENU_L: i32 = 0xA4;
    const VK_MENU_R: i32 = 0xA5;
    const VK_CONTROL_L: i32 = 0xA2;
    const VK_CONTROL_R: i32 = 0xA3;
    const VK_SHIFT_L: i32 = 0xA0;
    const VK_SHIFT_R: i32 = 0xA1;
    const VK_LWIN: i32 = 0x5B;
    const VK_RWIN: i32 = 0x5C;
    const BIT: i16 = 0x8000u16 as i16;
    unsafe {
        [VK_MENU_L, VK_MENU_R, VK_CONTROL_L, VK_CONTROL_R, VK_SHIFT_L, VK_SHIFT_R, VK_LWIN, VK_RWIN]
            .into_iter()
            .any(|vk| GetAsyncKeyState(vk) & BIT != 0)
    }
}

#[cfg(not(windows))]
pub fn modifier_down() -> bool {
    false
}
/// Shift 是否按着（区分 Enter 发送 / Shift+Enter 换行用）。
#[cfg(windows)]
pub fn shift_down() -> bool {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetAsyncKeyState(vk: i32) -> i16;
    }
    unsafe { GetAsyncKeyState(0x10) & (0x8000u16 as i16) != 0 }
}

#[cfg(not(windows))]
pub fn shift_down() -> bool {
    false
}
/// Ctrl 是否按着（仅 Windows）。
#[cfg(windows)]
pub fn ctrl_down() -> bool {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetAsyncKeyState(vk: i32) -> i16;
    }
    const BIT: i16 = 0x8000u16 as i16;
    unsafe { GetAsyncKeyState(0x11) & BIT != 0 }
}

#[cfg(not(windows))]
pub fn ctrl_down() -> bool {
    false
}

/// 任意虚拟键是否按着（仅 Windows）。
#[cfg(windows)]
pub fn key_down(vk: i32) -> bool {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetAsyncKeyState(vk: i32) -> i16;
    }
    const BIT: i16 = 0x8000u16 as i16;
    unsafe { GetAsyncKeyState(vk) & BIT != 0 }
}

#[cfg(not(windows))]
pub fn key_down(_vk: i32) -> bool {
    false
}