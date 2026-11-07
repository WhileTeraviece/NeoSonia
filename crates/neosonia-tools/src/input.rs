//! 输入模拟工具：鼠标移动/点击/拖拽/滚轮、按住与松开、键盘输入/按键、剪贴板。
//!
//! 坐标约定：一律**物理像素**（与 enigo 在 Windows 上一致）。
//! 所有系统调用都放进 `spawn_blocking`，每次调用新建 enigo 实例，
//! 避免在 tokio 工作线程上做阻塞的 SendInput。

use std::sync::{Mutex, Once, OnceLock};
use std::sync::Arc;
use std::time::{Duration, Instant};

use enigo::{
    Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings as EnigoSettings,
};
use neosonia_core::model::{ToolCtx, ToolOutcome};
use neosonia_core::registry::ToolRegistry;
use neosonia_core::util::truncate_chars;
use serde_json::{json, Value};

use crate::{arg_bool, arg_i64, arg_str, arg_str_req, arg_string_list, arg_u64};

pub fn register(reg: &ToolRegistry) {
    reg.register(Arc::new(MouseMove));
    reg.register(Arc::new(MouseClick));
    reg.register(Arc::new(MouseClickSequence));
    reg.register(Arc::new(MouseGraph));
    reg.register(Arc::new(MousePress));
    reg.register(Arc::new(MouseRelease));
    reg.register(Arc::new(MouseDrag));
    reg.register(Arc::new(MouseScroll));
    reg.register(Arc::new(MouseDragPath));
    reg.register(Arc::new(KeyboardType));
    reg.register(Arc::new(KeyboardKey));
    reg.register(Arc::new(KeyboardCombo));
    reg.register(Arc::new(KeyboardPress));
    reg.register(Arc::new(KeyboardRelease));
    reg.register(Arc::new(ClipboardRead));
    reg.register(Arc::new(ClipboardWrite));
}

// ============================================================
// 公共辅助
// ============================================================

fn input_allowed(ctx: &ToolCtx) -> Result<(), ToolOutcome> {
    if ctx.settings().tools.allow_input_simulation {
        Ok(())
    } else {
        Err(ToolOutcome::err(
            "输入模拟已在设置中关闭（tools.allow_input_simulation=false）。".to_string(),
        ))
    }
}

/// 在阻塞线程里新建 enigo 并执行闭包；所有输入工具共用。
async fn enigo_blocking<F>(f: F) -> Result<String, String>
where
    F: FnOnce(&mut Enigo) -> Result<String, String> + Send + 'static,
{
    tokio::task::spawn_blocking(move || -> Result<String, String> {
        let mut enigo = Enigo::new(&EnigoSettings::default()).map_err(|e| {
            format!("无法初始化输入模拟（{e}）。请确认当前会话有可交互的桌面。")
        })?;
        f(&mut enigo)
    })
    .await
    .map_err(|e| format!("输入模拟任务异常：{e}"))?
}

async fn run_input<F>(display: String, f: F) -> ToolOutcome
where
    F: FnOnce(&mut Enigo) -> Result<String, String> + Send + 'static,
{
    match enigo_blocking(f).await {
        Ok(msg) => ToolOutcome::ok(msg).with_display(display),
        Err(e) => ToolOutcome::err(e),
    }
}

// ============================================================
// 按住不放：全局按下状态 + 退出兜底释放
// ============================================================

/// 当前被按住的鼠标键/键盘键（进程级共享）。
#[derive(Default, Debug)]
struct HeldState {
    /// 按按下顺序记录鼠标键。
    mouse: Vec<Button>,
    /// 键名（给模型看）与 enigo Key 的对应，按按下顺序记录。
    keys: Vec<(String, Key)>,
}

impl HeldState {
    fn mouse_is_held(&self, button: Button) -> bool {
        self.mouse.contains(&button)
    }

    fn key_is_held(&self, key: Key) -> bool {
        self.keys.iter().any(|(_, held)| *held == key)
    }

    /// 返回 false 表示之前已按下（重复 press）。
    fn press_mouse(&mut self, button: Button) -> bool {
        if self.mouse_is_held(button) {
            false
        } else {
            self.mouse.push(button);
            true
        }
    }

    /// 返回 false 表示之前并未记录为按下。
    fn release_mouse(&mut self, button: Button) -> bool {
        let before = self.mouse.len();
        self.mouse.retain(|held| *held != button);
        self.mouse.len() != before
    }

    /// 返回 false 表示之前已按下（重复 press）。
    fn press_key(&mut self, name: &str, key: Key) -> bool {
        if self.key_is_held(key) {
            false
        } else {
            self.keys.push((name.to_string(), key));
            true
        }
    }

    /// 返回被松开的键名；None 表示之前并未记录为按下。
    fn release_key(&mut self, key: Key) -> Option<String> {
        let position = self.keys.iter().position(|(_, held)| *held == key)?;
        Some(self.keys.remove(position).0)
    }

    fn take_all(&mut self) -> (Vec<Button>, Vec<(String, Key)>) {
        (
            std::mem::take(&mut self.mouse),
            std::mem::take(&mut self.keys),
        )
    }

    fn snapshot(&self) -> (Vec<String>, Vec<String>) {
        (
            self.mouse
                .iter()
                .map(|button| button_label(*button).to_string())
                .collect(),
            self.keys.iter().map(|(name, _)| name.clone()).collect(),
        )
    }
}

static HELD: OnceLock<Mutex<HeldState>> = OnceLock::new();

/// 拿全局按下状态。std::sync::Mutex 会毒化：即使别的线程 panic 过，
/// 也用 `into_inner` 拿回内部状态继续释放，避免鼠标/键盘卡在按下状态。
fn held_lock() -> std::sync::MutexGuard<'static, HeldState> {
    HELD.get_or_init(|| Mutex::new(HeldState::default()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(any(windows, unix))]
unsafe extern "C" {
    /// 进程正常退出时执行回调（MSVC/ucrt、glibc 都提供）。
    fn atexit(callback: extern "C" fn()) -> i32;
}

extern "C" fn release_all_held_atexit() {
    let _ = release_all_held_blocking();
}

/// 注册进程退出兜底：static 变量不会在进程结束时被 Drop，
/// 所以必须在 atexit 里释放，才能真正避免"退出后鼠标还按着"。
fn ensure_release_on_exit() {
    static HOOK: Once = Once::new();
    #[cfg(any(windows, unix))]
    HOOK.call_once(|| {
        // SAFETY: atexit 是 C 标准库函数；回调不 panic（内部全部忽略失败）。
        unsafe {
            atexit(release_all_held_atexit);
        }
    });
    #[cfg(not(any(windows, unix)))]
    {
        let _ = &HOOK;
    }
}

/// 同步释放所有按住的鼠标/键盘键，并清空状态。返回已释放的键名。
fn release_all_held_blocking() -> Vec<String> {
    let (mouse, keys) = {
        let mut state = held_lock();
        state.take_all()
    };
    let mut released: Vec<String> = Vec::new();
    if mouse.is_empty() && keys.is_empty() {
        return released;
    }
    if let Ok(mut enigo) = Enigo::new(&EnigoSettings::default()) {
        for (name, key) in keys.iter().rev() {
            if enigo.key(*key, Direction::Release).is_ok() {
                released.push(name.clone());
            }
        }
        for button in mouse.iter().rev() {
            if enigo.button(*button, Direction::Release).is_ok() {
                released.push(button_label(*button).to_string());
            }
        }
    }
    released
}

/// 兜底释放器：app 可以在退出流程里 drop 它，立刻释放所有按住的键。
/// （另外进程退出还有 atexit 一层，正常退出/关闭窗口都会清理。）
pub struct HoldReleaseGuard;

impl Drop for HoldReleaseGuard {
    fn drop(&mut self) {
        let _ = release_all_held_blocking();
    }
}

/// 给 app 用：拿到一个 Drop 时释放所有按下键的守卫。
pub fn hold_release_guard() -> HoldReleaseGuard {
    HoldReleaseGuard
}

/// 当前按下状态（供工具/测试查询）。
pub fn held_snapshot() -> (Vec<String>, Vec<String>) {
    held_lock().snapshot()
}

fn parse_button(name: &str) -> Result<Button, String> {
    match name.trim().to_ascii_lowercase().as_str() {
        "" | "left" | "l" | "左键" => Ok(Button::Left),
        "right" | "r" | "右键" => Ok(Button::Right),
        "middle" | "m" | "中键" => Ok(Button::Middle),
        "back" | "x1" => Ok(Button::Back),
        "forward" | "x2" => Ok(Button::Forward),
        other => Err(format!(
            "不认识的鼠标按键：{other:?}。可用 left / right / middle / back / forward。"
        )),
    }
}

fn parse_modifier(name: &str) -> Result<Key, String> {
    match name.trim().to_ascii_lowercase().as_str() {
        "ctrl" | "control" => Ok(Key::Control),
        "shift" => Ok(Key::Shift),
        "alt" | "option" => Ok(Key::Alt),
        "meta" | "win" | "windows" | "cmd" | "command" | "super" => Ok(Key::Meta),
        other => Err(format!(
            "不认识的修饰键：{other:?}。可用 ctrl / shift / alt / meta(win)。"
        )),
    }
}

fn parse_function_key(n: u32) -> Option<Key> {
    Some(match n {
        1 => Key::F1,
        2 => Key::F2,
        3 => Key::F3,
        4 => Key::F4,
        5 => Key::F5,
        6 => Key::F6,
        7 => Key::F7,
        8 => Key::F8,
        9 => Key::F9,
        10 => Key::F10,
        11 => Key::F11,
        12 => Key::F12,
        13 => Key::F13,
        14 => Key::F14,
        15 => Key::F15,
        16 => Key::F16,
        17 => Key::F17,
        18 => Key::F18,
        19 => Key::F19,
        20 => Key::F20,
        _ => return None,
    })
}

fn parse_key(name: &str) -> Result<Key, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("key 不能为空。".to_string());
    }
    let lower = trimmed.to_ascii_lowercase();
    let key = match lower.as_str() {
        "enter" | "return" => Key::Return,
        "esc" | "escape" => Key::Escape,
        "tab" => Key::Tab,
        "space" | "spacebar" => Key::Space,
        "backspace" | "back" => Key::Backspace,
        "delete" | "del" => Key::Delete,
        "insert" | "ins" => Key::Insert,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" | "pgup" | "page_up" => Key::PageUp,
        "pagedown" | "pgdn" | "page_down" => Key::PageDown,
        "up" | "arrowup" | "uparrow" => Key::UpArrow,
        "down" | "arrowdown" | "downarrow" => Key::DownArrow,
        "left" | "arrowleft" | "leftarrow" => Key::LeftArrow,
        "right" | "arrowright" | "rightarrow" => Key::RightArrow,
        "capslock" | "caps" => Key::CapsLock,
        "numlock" => Key::Numlock,
        "printscreen" | "prtsc" | "print" => Key::Print,
        "pause" | "break" => Key::Pause,
        "ctrl" | "control" => Key::Control,
        "shift" => Key::Shift,
        "alt" | "option" => Key::Alt,
        "meta" | "win" | "windows" | "cmd" | "command" | "super" => Key::Meta,
        "volumeup" | "volup" => Key::VolumeUp,
        "volumedown" | "voldown" => Key::VolumeDown,
        "volumemute" | "mute" => Key::VolumeMute,
        other => {
            if let Some(num) = other.strip_prefix('f').and_then(|n| n.parse::<u32>().ok()) {
                match parse_function_key(num) {
                    Some(k) => k,
                    None => {
                        return Err(format!(
                            "不支持 {other:?}：功能键只支持 F1-F20。"
                        ))
                    }
                }
            } else {
                let chars: Vec<char> = trimmed.chars().collect();
                if chars.len() == 1 {
                    Key::Unicode(chars[0])
                } else {
                    return Err(format!(
                        "不认识按键：{trimmed:?}。支持 enter / esc / tab / space / backspace / delete / \
                         insert / home / end / pgup / pgdn / up / down / left / right / f1-f20 / 单个字符；\
                         组合键用 modifiers 参数或写成 \"ctrl+a\"。"
                    ));
                }
            }
        }
    };
    Ok(key)
}

/// 鼠标移动插值，`smooth=true` 时按步进移动。
fn smooth_move(enigo: &mut Enigo, to_x: i32, to_y: i32, steps: u32, delay: Duration) {
    let from = enigo.location().unwrap_or((to_x, to_y));
    for i in 1..=steps {
        let t = i as f64 / steps as f64;
        let x = from.0 as f64 + (to_x - from.0) as f64 * t;
        let y = from.1 as f64 + (to_y - from.1) as f64 * t;
        let _ = enigo.move_mouse(x.round() as i32, y.round() as i32, Coordinate::Abs);
        std::thread::sleep(delay);
    }
}

fn press_combo(enigo: &mut Enigo, modifiers: &[Key], key: Key, repeat: u32) -> Result<(), String> {
    for m in modifiers {
        enigo
            .key(*m, Direction::Press)
            .map_err(|e| format!("按下修饰键失败：{e}"))?;
    }
    let mut result = Ok(());
    for i in 0..repeat {
        if let Err(e) = enigo.key(key, Direction::Click) {
            result = Err(format!("按键失败：{e}"));
            break;
        }
        if i + 1 < repeat {
            std::thread::sleep(Duration::from_millis(40));
        }
    }
    for m in modifiers.iter().rev() {
        let _ = enigo.key(*m, Direction::Release);
    }
    result
}

/// 主屏缩放比（逻辑坐标 → 物理像素）。取不到时按 1.0，不让工具失败。
fn primary_scale() -> f32 {
    use xcap::Monitor;
    Monitor::all()
        .ok()
        .and_then(|monitors| {
            monitors
                .iter()
                .find(|m| m.is_primary())
                .or_else(|| monitors.first())
                .map(|m| m.scale_factor())
        })
        .filter(|scale| *scale > 0.0)
        .unwrap_or(1.0)
}

/// 纯函数：logical=true 时把逻辑坐标按 scale 换算成物理像素。
fn convert_point(logical: bool, x: i64, y: i64, scale: f32) -> (i32, i32) {
    if logical {
        let scale = if scale > 0.0 { scale as f64 } else { 1.0 };
        (
            (x as f64 * scale).round() as i32,
            (y as f64 * scale).round() as i32,
        )
    } else {
        (x as i32, y as i32)
    }
}

/// 按 logical 标志解析一个点（需要时读取主屏 scale）。
fn resolve_point(logical: bool, x: i64, y: i64) -> (i32, i32) {
    if logical {
        convert_point(true, x, y, primary_scale())
    } else {
        convert_point(false, x, y, 1.0)
    }
}

// ============================================================
// 自适应坐标：屏幕物理像素 ⇄ 最近一次截图的图片像素
// ============================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CoordSpace {
    /// 屏幕物理像素（默认，网格刻度/UIA rect 都是这个）。
    Screen,
    /// 最近一次 screenshot 返回的图片像素（可能被 max_edge 缩小）。
    Image,
}

fn parse_coord_space(args: &Value) -> Result<CoordSpace, String> {
    match arg_str(args, "coord_space") {
        None => Ok(CoordSpace::Screen),
        Some(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "" | "screen" | "physical" => Ok(CoordSpace::Screen),
            "image" | "img" | "picture" => Ok(CoordSpace::Image),
            other => Err(format!(
                "coord_space 只能是 \"screen\"（默认，屏幕物理像素）或 \"image\"（截图图片像素），收到 {other:?}。"
            )),
        },
    }
}

/// 把一批坐标换算成屏幕物理像素；`coord_space="image"` 时用最近一次截图契约自动乘 scale。
/// 返回 (换算后坐标, 回显说明)；回显用于工具输出，方便模型核对。
fn convert_coords(
    coord_space: CoordSpace,
    logical: bool,
    points: &[(i64, i64)],
) -> Result<(Vec<(i64, i64)>, Option<String>), String> {
    if coord_space == CoordSpace::Screen {
        return Ok((points.to_vec(), None));
    }
    if logical {
        return Err(
            "coord_space=\"image\" 与 logical=true 不能同时使用：图片坐标本身已经对应屏幕像素空间。"
                .to_string(),
        );
    }
    let contract = crate::screen::last_screenshot_contract().ok_or_else(|| {
        "还没有可用的截图坐标契约：请先调用 screenshot（建议 grid:true，10 分钟内有效）再传 coord_space=\"image\"。"
            .to_string()
    })?;
    let converted: Vec<(i64, i64)> = points
        .iter()
        .map(|(x, y)| {
            let (screen_x, screen_y) = contract.image_to_screen(*x, *y);
            (screen_x as i64, screen_y as i64)
        })
        .collect();
    let echo = points
        .iter()
        .zip(converted.iter())
        .map(|((image_x, image_y), (screen_x, screen_y))| {
            format!("图片({image_x},{image_y}) → 屏幕({screen_x},{screen_y})")
        })
        .collect::<Vec<_>>()
        .join("；");
    Ok((converted, Some(echo)))
}

/// 折线插值（纯函数）：把 points 均匀切成 `steps` 段步进点，用于模拟人手拖拽。
fn interpolate_path(points: &[(i32, i32)], steps: u32) -> Vec<(i32, i32)> {
    if points.len() < 2 || steps < 2 {
        return points.to_vec();
    }
    let mut out: Vec<(i32, i32)> = Vec::with_capacity(steps as usize + 1);
    out.push(points[0]);
    let segment_count = points.len() - 1;
    let per_segment = ((steps as usize) / segment_count).max(1);
    for pair in points.windows(2) {
        let (x0, y0) = pair[0];
        let (x1, y1) = pair[1];
        for i in 1..=per_segment {
            let t = i as f64 / per_segment as f64;
            out.push((
                (x0 as f64 + (x1 - x0) as f64 * t).round() as i32,
                (y0 as f64 + (y1 - y0) as f64 * t).round() as i32,
            ));
        }
    }
    out
}

/// 解析 `points: [{x,y}, ...]`。
fn parse_points(value: Option<&Value>) -> Result<Vec<(i64, i64)>, String> {
    let Some(value) = value else {
        return Err(
            "缺少必填参数 points（至少 2 个点，例如 [{\"x\":0,\"y\":0},{\"x\":300,\"y\":200}]）。"
                .to_string(),
        );
    };
    let Some(items) = value.as_array() else {
        return Err("points 必须是数组。".to_string());
    };
    if items.len() < 2 {
        return Err("points 至少需要 2 个点。".to_string());
    }
    let mut out: Vec<(i64, i64)> = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let object = item
            .as_object()
            .ok_or_else(|| format!("points[{index}] 必须是 {{\"x\":..,\"y\":..}} 对象。"))?;
        let number = |key: &str| -> Option<i64> {
            match object.get(key) {
                Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
                Some(Value::String(s)) => s.trim().parse::<f64>().ok().map(|f| f as i64),
                _ => None,
            }
        };
        let x = number("x").ok_or_else(|| format!("points[{index}].x 缺失或不是数字。"))?;
        let y = number("y").ok_or_else(|| format!("points[{index}].y 缺失或不是数字。"))?;
        out.push((x.clamp(-100_000, 100_000), y.clamp(-100_000, 100_000)));
    }
    Ok(out)
}

// ============================================================
// 鼠标
// ============================================================

async fn mouse_move(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    if let Err(e) = input_allowed(ctx) {
        return e;
    }
    let Some(raw_x) = args.get("x").map(|_| arg_i64(&args, "x", 0)) else {
        return ToolOutcome::err("缺少必填参数 x（物理像素横坐标）。".to_string());
    };
    let Some(raw_y) = args.get("y").map(|_| arg_i64(&args, "y", 0)) else {
        return ToolOutcome::err("缺少必填参数 y（物理像素纵坐标）。".to_string());
    };
    let logical = arg_bool(&args, "logical", false);
    let coord_space = match parse_coord_space(&args) {
        Ok(space) => space,
        Err(e) => return ToolOutcome::err(e),
    };
    let (points, coord_note) = match convert_coords(coord_space, logical, &[(raw_x, raw_y)]) {
        Ok(value) => value,
        Err(e) => return ToolOutcome::err(e),
    };
    let (x, y) = points[0];
    if x < -100_000 || x > 100_000 || y < -100_000 || y > 100_000 {
        return ToolOutcome::err(format!("坐标 ({x}, {y}) 超出合理范围。坐标单位是物理像素。"));
    }
    let smooth = arg_bool(&args, "smooth", true);
    let coord_note_text = coord_note.clone();
    run_input(
        format!(
            "移动到 ({x}, {y}){}{}",
            if logical { "（逻辑坐标）" } else { "" },
            if coord_space == CoordSpace::Image {
                "（图片坐标）"
            } else {
                ""
            }
        ),
        move |enigo| {
            let (xi, yi) = resolve_point(logical, x, y);
            if smooth {
                smooth_move(enigo, xi, yi, 12, Duration::from_millis(6));
            } else {
                enigo
                    .move_mouse(xi, yi, Coordinate::Abs)
                    .map_err(|e| format!("移动鼠标失败：{e}"))?;
            }
            Ok(format!(
                "[ok] 鼠标已移动到物理像素 ({xi}, {yi}){}{}{}",
                if logical {
                    format!("（由逻辑坐标 ({x}, {y}) 换算）")
                } else {
                    String::new()
                },
                if smooth { "（平滑移动）" } else { "" },
                coord_note_text
                    .as_ref()
                    .map(|note| format!("\n坐标换算：{note}"))
                    .unwrap_or_default()
            ))
        },
    )
    .await
}

/// 鼠标按键的中文名（给结果文案用）。
fn button_label(button: Button) -> &'static str {
    match button {
        Button::Left => "左键",
        Button::Right => "右键",
        Button::Middle => "中键",
        Button::Back => "后退键",
        Button::Forward => "前进键",
        _ => "鼠标键",
    }
}

/// 解析点击计划：返回 (点击次数, 间隔毫秒)。
/// `double=true` 时至少点 2 次，且把间隔压到系统双击阈值以内，避免被当成两次单击。
fn click_plan(
    double: bool,
    count: Option<u64>,
    legacy_clicks: Option<u64>,
    interval_ms: u64,
) -> (u32, u64) {
    let mut times = count.or(legacy_clicks).unwrap_or(1);
    if double && times < 2 {
        times = 2;
    }
    let interval = if double {
        interval_ms.min(180)
    } else {
        interval_ms.min(2000)
    };
    (times.clamp(1, 10) as u32, interval)
}

async fn mouse_click(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    if let Err(e) = input_allowed(ctx) {
        return e;
    }
    let has_x = args.get("x").is_some();
    let has_y = args.get("y").is_some();
    if has_x != has_y {
        return ToolOutcome::err("x 和 y 必须同时提供，或者都不提供（点击当前位置）。".to_string());
    }
    let logical = arg_bool(&args, "logical", false);
    let coord_space = match parse_coord_space(&args) {
        Ok(space) => space,
        Err(e) => return ToolOutcome::err(e),
    };
    let mut coord_note: Option<String> = None;
    let target: Option<(i64, i64)> = if has_x {
        let raw = (arg_i64(&args, "x", 0), arg_i64(&args, "y", 0));
        match convert_coords(coord_space, logical, &[raw]) {
            Ok((converted, note)) => {
                coord_note = note;
                Some(converted[0])
            }
            Err(e) => return ToolOutcome::err(e),
        }
    } else {
        if coord_space == CoordSpace::Image {
            return ToolOutcome::err(
                "coord_space=\"image\" 需要同时提供 x 和 y（图片像素坐标）。".to_string(),
            );
        }
        None
    };
    let button_name = arg_str(&args, "button").unwrap_or_else(|| "left".to_string());
    let button = match parse_button(&button_name) {
        Ok(b) => b,
        Err(e) => return ToolOutcome::err(e),
    };
    let double = arg_bool(&args, "double", false);
    let count = args.get("count").map(|_| arg_u64(&args, "count", 1));
    let legacy_clicks = args.get("clicks").map(|_| arg_u64(&args, "clicks", 1));
    let (clicks, interval_ms) = click_plan(
        double,
        count,
        legacy_clicks,
        arg_u64(&args, "interval_ms", 80),
    );
    let description = if double && button == Button::Left {
        "双击（左键）".to_string()
    } else if double {
        format!("{}双击（{} 次）", button_label(button), clicks)
    } else {
        format!("{}点击 {} 次", button_label(button), clicks)
    };
    let coord_note_text = coord_note.clone();
    run_input(description.clone(), move |enigo| {
        if let Some((x, y)) = target {
            let (px, py) = resolve_point(logical, x, y);
            enigo
                .move_mouse(px, py, Coordinate::Abs)
                .map_err(|e| format!("移动鼠标到 ({px}, {py}) 失败：{e}"))?;
            std::thread::sleep(Duration::from_millis(30));
        }
        for i in 0..clicks {
            enigo
                .button(button, Direction::Click)
                .map_err(|e| format!("点击失败：{e}"))?;
            if i + 1 < clicks {
                std::thread::sleep(Duration::from_millis(interval_ms));
            }
        }
        Ok(format!(
            "[ok] 已{}{}{}{}",
            if target.is_some() { "移动到目标位置并" } else { "" },
            description,
            target
                .map(|(x, y)| format!(
                    "，位置 ({x}, {y}){}",
                    if logical { "（逻辑坐标）" } else { "" }
                ))
                .unwrap_or_default(),
            coord_note_text
                .as_ref()
                .map(|note| format!("\n坐标换算：{note}"))
                .unwrap_or_default()
        ))
    })
    .await
}

/// 连续点击计划里的一步。
#[derive(Clone, Debug, PartialEq, Eq)]
struct ClickStep {
    x: i32,
    y: i32,
    button: Button,
    double: bool,
    /// 这一步点击完成后到下一条之间的等待（毫秒）。
    delay_ms: u64,
    logical: bool,
}

/// 解析 `mouse_click_sequence` 的 points 参数（交给单测覆盖）。
fn parse_click_sequence(value: Option<&Value>, interval_ms: u64) -> Result<Vec<ClickStep>, String> {
    let Some(value) = value else {
        return Err("缺少必填参数 points（点击点数组，例如 [{\"x\":100,\"y\":200}]）。".to_string());
    };
    let Some(array) = value.as_array() else {
        return Err("points 必须是数组，例如 [{\"x\":100,\"y\":200}]。".to_string());
    };
    if array.is_empty() {
        return Err("points 不能为空，至少给 1 个点。".to_string());
    }
    if array.len() > 30 {
        return Err(format!(
            "points 最多 30 个点（收到 {} 个）；点位太多请拆成多次调用。",
            array.len()
        ));
    }
    let default_interval = if interval_ms == 0 { 120 } else { interval_ms };
    let mut steps = Vec::with_capacity(array.len());
    for (index, item) in array.iter().enumerate() {
        let position = index + 1;
        let Some(object) = item.as_object() else {
            return Err(format!("第 {position} 个点必须是对象（含 x、y）。"));
        };
        let x = object
            .get("x")
            .and_then(|value| value.as_i64().or_else(|| value.as_str().and_then(|s| s.trim().parse().ok())))
            .ok_or_else(|| format!("第 {position} 个点缺少整数 x。"))?;
        let y = object
            .get("y")
            .and_then(|value| value.as_i64().or_else(|| value.as_str().and_then(|s| s.trim().parse().ok())))
            .ok_or_else(|| format!("第 {position} 个点缺少整数 y。"))?;
        let button = match object.get("button").and_then(|value| value.as_str()) {
            None => Button::Left,
            Some(name) => parse_button(name).map_err(|e| format!("第 {position} 个点：{e}"))?,
        };
        let double = object
            .get("double")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        let delay_ms = object
            .get("delay_ms")
            .and_then(|value| value.as_u64().or_else(|| value.as_str().and_then(|s| s.trim().parse().ok())))
            .unwrap_or(default_interval)
            .min(5000);
        let logical = object
            .get("logical")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        steps.push(ClickStep {
            x: x.clamp(-1_000_000, 1_000_000) as i32,
            y: y.clamp(-1_000_000, 1_000_000) as i32,
            button,
            double,
            delay_ms,
            logical,
        });
    }
    Ok(steps)
}

/// 一次设定多个点，依次移动+点击（比多次 mouse_click 少来回、更稳定）。
async fn mouse_click_sequence(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    if let Err(e) = input_allowed(ctx) {
        return e;
    }
    let interval_ms = arg_u64(&args, "interval_ms", 120).min(5000);
    let mut steps = match parse_click_sequence(args.get("points"), interval_ms) {
        Ok(steps) => steps,
        Err(e) => return ToolOutcome::err(e),
    };
    // 自适应坐标：coord_space="image" 时按最近一次截图的比例换算每个点
    let coord_space = match parse_coord_space(&args) {
        Ok(space) => space,
        Err(e) => return ToolOutcome::err(e),
    };
    if coord_space == CoordSpace::Image && steps.iter().any(|step| step.logical) {
        return ToolOutcome::err(
            "coord_space=\"image\" 与 points 里的 logical=true 不能同时使用：图片坐标本身已经是屏幕像素空间。"
                .to_string(),
        );
    }
    let raw_coords: Vec<(i64, i64)> = steps
        .iter()
        .map(|step| (step.x as i64, step.y as i64))
        .collect();
    let (converted, coord_note) = match convert_coords(coord_space, false, &raw_coords) {
        Ok(value) => value,
        Err(e) => return ToolOutcome::err(e),
    };
    for (step, (x, y)) in steps.iter_mut().zip(converted.iter()) {
        if coord_space == CoordSpace::Image {
            step.logical = false;
        }
        step.x = *x as i32;
        step.y = *y as i32;
    }
    let point_count = steps.len();
    let default_delay = steps.first().map(|step| step.delay_ms).unwrap_or(interval_ms);
    let coord_note_text = coord_note.clone();
    let display = format!("连续点击 {point_count} 个点");
    let started = Instant::now();
    let result = enigo_blocking(move |enigo| {
        let mut executed = 0usize;
        for (index, step) in steps.iter().enumerate() {
            let position = index + 1;
            let (px, py) = resolve_point(step.logical, step.x as i64, step.y as i64);
            enigo.move_mouse(px, py, Coordinate::Abs).map_err(|e| {
                format!(
                    "第 {position} 步失败（移动鼠标到 ({px}, {py})）：{e}；已执行 {executed}/{point_count} 步。"
                )
            })?;
            std::thread::sleep(Duration::from_millis(20));
            if step.double {
                enigo.button(step.button, Direction::Click).map_err(|e| {
                    format!("第 {position} 步失败（第一次点击）：{e}；已执行 {executed}/{point_count} 步。")
                })?;
                std::thread::sleep(Duration::from_millis(120));
                enigo.button(step.button, Direction::Click).map_err(|e| {
                    format!("第 {position} 步失败（第二次点击）：{e}；已执行 {executed}/{point_count} 步。")
                })?;
            } else {
                enigo.button(step.button, Direction::Click).map_err(|e| {
                    format!(
                        "第 {position} 步失败（{} 点击 ({px}, {py})）：{e}；已执行 {executed}/{point_count} 步。",
                        button_label(step.button)
                    )
                })?;
            }
            executed += 1;
            if index + 1 < point_count {
                std::thread::sleep(Duration::from_millis(step.delay_ms));
            }
        }
        Ok(format!(
            "[ok] 连续点击完成：执行 {executed}/{point_count} 个点，总耗时 {}ms（默认步间 {}ms）。{}",
            started.elapsed().as_millis(),
            default_delay,
            coord_note_text
                .as_ref()
                .map(|note| format!("\n坐标换算：{note}"))
                .unwrap_or_default()
        ))
    })
    .await;
    match result {
        Ok(message) => ToolOutcome::ok(message).with_display(display),
        Err(e) => ToolOutcome::err(e),
    }
}

// ============================================================
// mouse_graph：按 y=f(x) 曲线移动鼠标
// ============================================================

/// 曲线表达式 AST（x 为自变量）。
#[derive(Clone, Debug, PartialEq)]
enum GraphExpr {
    Num(f64),
    X,
    Neg(Box<GraphExpr>),
    Add(Box<GraphExpr>, Box<GraphExpr>),
    Sub(Box<GraphExpr>, Box<GraphExpr>),
    Mul(Box<GraphExpr>, Box<GraphExpr>),
    Div(Box<GraphExpr>, Box<GraphExpr>),
    Pow(Box<GraphExpr>, Box<GraphExpr>),
    Func(GraphFunc, Vec<GraphExpr>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GraphFunc {
    Sin,
    Cos,
    Tan,
    Asin,
    Acos,
    Atan,
    Sqrt,
    Abs,
    Exp,
    Ln,
    Log,
    Floor,
    Ceil,
    Round,
    Min,
    Max,
}

impl GraphFunc {
    fn from_name(name: &str) -> Option<(GraphFunc, usize)> {
        // (函数, 参数个数)
        Some(match name {
            "sin" => (GraphFunc::Sin, 1),
            "cos" => (GraphFunc::Cos, 1),
            "tan" => (GraphFunc::Tan, 1),
            "asin" => (GraphFunc::Asin, 1),
            "acos" => (GraphFunc::Acos, 1),
            "atan" => (GraphFunc::Atan, 1),
            "sqrt" => (GraphFunc::Sqrt, 1),
            "abs" => (GraphFunc::Abs, 1),
            "exp" => (GraphFunc::Exp, 1),
            "ln" => (GraphFunc::Ln, 1),
            "log" => (GraphFunc::Log, 1),
            "floor" => (GraphFunc::Floor, 1),
            "ceil" => (GraphFunc::Ceil, 1),
            "round" => (GraphFunc::Round, 1),
            "min" => (GraphFunc::Min, 2),
            "max" => (GraphFunc::Max, 2),
            _ => return None,
        })
    }

    fn eval(self, args: &[f64]) -> f64 {
        match self {
            GraphFunc::Sin => args[0].sin(),
            GraphFunc::Cos => args[0].cos(),
            GraphFunc::Tan => args[0].tan(),
            GraphFunc::Asin => args[0].asin(),
            GraphFunc::Acos => args[0].acos(),
            GraphFunc::Atan => args[0].atan(),
            GraphFunc::Sqrt => args[0].sqrt(),
            GraphFunc::Abs => args[0].abs(),
            GraphFunc::Exp => args[0].exp(),
            GraphFunc::Ln => args[0].ln(),
            GraphFunc::Log => args[0].log10(),
            GraphFunc::Floor => args[0].floor(),
            GraphFunc::Ceil => args[0].ceil(),
            GraphFunc::Round => args[0].round(),
            GraphFunc::Min => args[0].min(args[1]),
            GraphFunc::Max => args[0].max(args[1]),
        }
    }
}

/// 递归下降表达式解析器（不依赖任何 crate）。
struct GraphParser {
    chars: Vec<char>,
    pos: usize,
}

impl GraphParser {
    fn new(src: &str) -> Self {
        Self {
            chars: src.chars().collect(),
            pos: 0,
        }
    }

    fn parse(src: &str) -> Result<GraphExpr, String> {
        let mut parser = Self::new(src);
        let expr = parser.parse_expr()?;
        parser.skip_ws();
        if parser.pos < parser.chars.len() {
            return Err(parser.error("表达式末尾有多余字符"));
        }
        Ok(expr)
    }

    fn rest(&self) -> String {
        self.chars[self.pos.min(self.chars.len())..]
            .iter()
            .collect()
    }

    fn error(&self, message: impl Into<String>) -> String {
        format!(
            "{}（位置 {}，剩余：{:?}）",
            message.into(),
            self.pos,
            truncate_chars(&self.rest(), 30)
        )
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t' | '\r' | '\n')) {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn eat(&mut self, expected: char) -> Result<(), String> {
        self.skip_ws();
        if self.peek() == Some(expected) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.error(format!("缺少 {expected:?}")))
        }
    }

    fn parse_expr(&mut self) -> Result<GraphExpr, String> {
        let mut left = self.parse_term()?;
        loop {
            self.skip_ws();
            match self.peek() {
                Some('+') => {
                    self.pos += 1;
                    let right = self.parse_term()?;
                    left = GraphExpr::Add(Box::new(left), Box::new(right));
                }
                Some('-') => {
                    self.pos += 1;
                    let right = self.parse_term()?;
                    left = GraphExpr::Sub(Box::new(left), Box::new(right));
                }
                _ => return Ok(left),
            }
        }
    }

    fn parse_term(&mut self) -> Result<GraphExpr, String> {
        let mut left = self.parse_unary()?;
        loop {
            self.skip_ws();
            match self.peek() {
                Some('*') => {
                    self.pos += 1;
                    let right = self.parse_unary()?;
                    left = GraphExpr::Mul(Box::new(left), Box::new(right));
                }
                Some('/') => {
                    self.pos += 1;
                    let right = self.parse_unary()?;
                    left = GraphExpr::Div(Box::new(left), Box::new(right));
                }
                _ => return Ok(left),
            }
        }
    }

    fn parse_unary(&mut self) -> Result<GraphExpr, String> {
        self.skip_ws();
        if self.peek() == Some('-') {
            self.pos += 1;
            let inner = self.parse_unary()?;
            return Ok(GraphExpr::Neg(Box::new(inner)));
        }
        if self.peek() == Some('+') {
            self.pos += 1;
            return self.parse_unary();
        }
        self.parse_power()
    }

    fn parse_power(&mut self) -> Result<GraphExpr, String> {
        let base = self.parse_primary()?;
        self.skip_ws();
        if self.peek() == Some('^') {
            self.pos += 1;
            // 右结合 + 允许 x^-2
            let exponent = self.parse_unary()?;
            return Ok(GraphExpr::Pow(Box::new(base), Box::new(exponent)));
        }
        Ok(base)
    }

    fn parse_primary(&mut self) -> Result<GraphExpr, String> {
        self.skip_ws();
        match self.peek() {
            Some('(') => {
                self.pos += 1;
                let inner = self.parse_expr()?;
                self.eat(')')?;
                Ok(inner)
            }
            Some(ch) if ch.is_ascii_digit() || ch == '.' => self.parse_number(),
            Some(ch) if ch.is_ascii_alphabetic() => {
                let start = self.pos;
                while matches!(self.peek(), Some(c) if c.is_ascii_alphanumeric() || c == '_') {
                    self.pos += 1;
                }
                let name: String = self.chars[start..self.pos].iter().collect();
                self.skip_ws();
                if self.peek() == Some('(') {
                    let (func, arity) = GraphFunc::from_name(&name.to_ascii_lowercase())
                        .ok_or_else(|| self.error(format!("不认识的函数：{name:?}")))?;
                    self.pos += 1;
                    let mut args: Vec<GraphExpr> = Vec::new();
                    self.skip_ws();
                    if self.peek() == Some(')') {
                        return Err(self.error(format!("函数 {name} 需要 {arity} 个参数")));
                    }
                    loop {
                        args.push(self.parse_expr()?);
                        self.skip_ws();
                        if self.peek() == Some(',') {
                            self.pos += 1;
                            continue;
                        }
                        break;
                    }
                    self.eat(')')?;
                    if args.len() != arity {
                        return Err(self.error(format!(
                            "函数 {name} 需要 {arity} 个参数，实际 {} 个",
                            args.len()
                        )));
                    }
                    Ok(GraphExpr::Func(func, args))
                } else {
                    match name.to_ascii_lowercase().as_str() {
                        "x" => Ok(GraphExpr::X),
                        "pi" => Ok(GraphExpr::Num(std::f64::consts::PI)),
                        "e" => Ok(GraphExpr::Num(std::f64::consts::E)),
                        other => Err(self.error(format!("不认识的变量/常量：{other:?}"))),
                    }
                }
            }
            _ => Err(self.error("缺少数字、x、pi/e 或函数调用")),
        }
    }

    fn parse_number(&mut self) -> Result<GraphExpr, String> {
        let start = self.pos;
        let mut seen_dot = false;
        let mut digits = 0usize;
        while let Some(ch) = self.peek() {
            if ch.is_ascii_digit() {
                digits += 1;
                self.pos += 1;
            } else if ch == '.' && !seen_dot {
                seen_dot = true;
                self.pos += 1;
            } else {
                break;
            }
        }
        if digits == 0 {
            return Err(self.error("数字格式错误"));
        }
        let text: String = self.chars[start..self.pos].iter().collect();
        text.parse::<f64>()
            .map(GraphExpr::Num)
            .map_err(|_| self.error(format!("无法解析数字：{text:?}")))
    }
}

fn parse_graph_expr(src: &str) -> Result<GraphExpr, String> {
    if src.trim().is_empty() {
        return Err("缺少必填参数 expr（例如 \"sin(x)\" 或 \"x^2\"）。".to_string());
    }
    GraphParser::parse(src)
}

/// 求值：除零/非法运算是 f64 的 inf/NaN，由调用方跳过。
fn eval_graph_expr(expr: &GraphExpr, x: f64) -> f64 {
    match expr {
        GraphExpr::Num(value) => *value,
        GraphExpr::X => x,
        GraphExpr::Neg(inner) => -eval_graph_expr(inner, x),
        GraphExpr::Add(a, b) => eval_graph_expr(a, x) + eval_graph_expr(b, x),
        GraphExpr::Sub(a, b) => eval_graph_expr(a, x) - eval_graph_expr(b, x),
        GraphExpr::Mul(a, b) => eval_graph_expr(a, x) * eval_graph_expr(b, x),
        GraphExpr::Div(a, b) => eval_graph_expr(a, x) / eval_graph_expr(b, x),
        GraphExpr::Pow(a, b) => eval_graph_expr(a, x).powf(eval_graph_expr(b, x)),
        GraphExpr::Func(func, args) => {
            let values: Vec<f64> = args.iter().map(|arg| eval_graph_expr(arg, x)).collect();
            func.eval(&values)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GraphButton {
    None,
    Left,
    Right,
}

impl GraphButton {
    fn parse(raw: Option<&str>) -> Result<GraphButton, String> {
        match raw.map(|value| value.trim().to_ascii_lowercase()) {
            None => Ok(GraphButton::None),
            Some(value) if value.is_empty() || value == "none" => Ok(GraphButton::None),
            Some(value) if value == "left" => Ok(GraphButton::Left),
            Some(value) if value == "right" => Ok(GraphButton::Right),
            Some(other) => Err(format!(
                "button 只能是 none / left / right，收到 {other:?}。"
            )),
        }
    }

    fn to_enigo(self) -> Option<Button> {
        match self {
            GraphButton::None => None,
            GraphButton::Left => Some(Button::Left),
            GraphButton::Right => Some(Button::Right),
        }
    }

    fn label(self) -> &'static str {
        match self {
            GraphButton::None => "none（不按键）",
            GraphButton::Left => "left（左键按住）",
            GraphButton::Right => "right（右键按住）",
        }
    }
}

/// 曲线采样参数（已校验）。
struct GraphParams {
    x_min: f64,
    x_max: f64,
    samples: usize,
    origin: (i32, i32),
    scale: (f64, f64),
    duration_ms: u64,
    button: GraphButton,
    dry_run: bool,
}

fn f64_arg_required(args: &Value, key: &str) -> Result<f64, String> {
    match args.get(key) {
        Some(Value::Number(n)) => n
            .as_f64()
            .ok_or_else(|| format!("参数 {key} 不是合法数字。")),
        Some(Value::String(s)) => s
            .trim()
            .parse::<f64>()
            .map_err(|_| format!("参数 {key} 不是合法数字：{s:?}。")),
        _ => Err(format!("缺少必填参数 {key}（数值）。")),
    }
}

/// 解析可选 {x,y} 对象；缺省返回 None。
fn parse_xy_pair(value: Option<&Value>, name: &str) -> Result<Option<(f64, f64)>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let object = value
        .as_object()
        .ok_or_else(|| format!("{name} 必须是对象，例如 {{\"x\":0,\"y\":0}}。"))?;
    let get = |key: &str| -> Option<f64> {
        match object.get(key) {
            Some(Value::Number(n)) => n.as_f64(),
            Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
            _ => None,
        }
    };
    let x = get("x").ok_or_else(|| format!("{name}.x 缺失或不是数字。"))?;
    let y = get("y").ok_or_else(|| format!("{name}.y 缺失或不是数字。"))?;
    if !x.is_finite() || !y.is_finite() {
        return Err(format!("{name}.x/y 必须是有限数字。"));
    }
    Ok(Some((x, y)))
}

fn parse_graph_params(args: &Value, origin: (i32, i32)) -> Result<GraphParams, String> {
    let x_min = f64_arg_required(args, "x_min")?;
    let x_max = f64_arg_required(args, "x_max")?;
    if !x_min.is_finite() || !x_max.is_finite() {
        return Err("x_min / x_max 必须是有限数字。".to_string());
    }
    if x_min >= x_max {
        return Err(format!(
            "x_min 必须小于 x_max（当前 x_min={x_min}, x_max={x_max}）。"
        ));
    }
    let span = x_max - x_min;
    if span > 10_000.0 {
        return Err(format!(
            "定义域跨度不能超过 10000（当前 {span:.3}）；请缩小 x_min..x_max。"
        ));
    }
    let samples = arg_u64(args, "samples", 120).clamp(2, 2000) as usize;
    let origin = match parse_xy_pair(args.get("origin"), "origin")? {
        Some((x, y)) => (x.round() as i32, y.round() as i32),
        None => origin,
    };
    let scale = match parse_xy_pair(args.get("scale"), "scale")? {
        Some((x, y)) => (x, y),
        None => (1.0, 1.0),
    };
    let duration_ms = arg_u64(args, "duration_ms", 800).clamp(50, 10_000);
    let button = GraphButton::parse(arg_str(args, "button").as_deref())?;
    let dry_run = arg_bool(args, "dry_run", false);
    Ok(GraphParams {
        x_min,
        x_max,
        samples,
        origin,
        scale,
        duration_ms,
        button,
        dry_run,
    })
}

/// 采样曲线并转换成屏幕坐标；返回 (屏幕点列表, 跳过点数)。
fn sample_graph_points(expr: &GraphExpr, params: &GraphParams) -> (Vec<(i32, i32)>, usize) {
    let mut points: Vec<(i32, i32)> = Vec::new();
    let mut skipped = 0usize;
    let denominator = (params.samples.saturating_sub(1)).max(1) as f64;
    for index in 0..params.samples {
        let t = index as f64 / denominator;
        let x = params.x_min + (params.x_max - params.x_min) * t;
        let y = eval_graph_expr(expr, x);
        let screen_x = params.origin.0 as f64 + x * params.scale.0;
        let screen_y = params.origin.1 as f64 - y * params.scale.1;
        if !y.is_finite() || !screen_x.is_finite() || !screen_y.is_finite() {
            skipped += 1;
            continue;
        }
        if screen_x < i32::MIN as f64
            || screen_x > i32::MAX as f64
            || screen_y < i32::MIN as f64
            || screen_y > i32::MAX as f64
        {
            skipped += 1;
            continue;
        }
        points.push((screen_x.round() as i32, screen_y.round() as i32));
    }
    (points, skipped)
}

async fn current_cursor_position() -> Result<(i32, i32), String> {
    tokio::task::spawn_blocking(|| -> Result<(i32, i32), String> {
        let enigo = Enigo::new(&EnigoSettings::default())
            .map_err(|e| format!("无法初始化输入模拟（{e}）。请确认当前会话有可交互的桌面。"))?;
        enigo
            .location()
            .map_err(|e| format!("读取当前鼠标位置失败：{e}"))
    })
    .await
    .map_err(|e| format!("读取鼠标位置任务异常：{e}"))?
}

fn render_graph_dry_run(
    expr_src: &str,
    params: &GraphParams,
    points: &[(i32, i32)],
    skipped: usize,
) -> String {
    let mut out = format!(
        "[ok] mouse_graph（dry_run=true，只解析和采样，没有移动鼠标）\n\
         表达式：{expr_src}\n\
         定义域：x ∈ [{}, {}]\n\
         采样数：{}（跳过 NaN/inf：{}）\n\
         起点：({}, {})\n\
         终点：({}, {})\n\
         原点：({}, {})，scale=({}, {})\n\
         dry_run：true\n",
        params.x_min,
        params.x_max,
        params.samples,
        skipped,
        points[0].0,
        points[0].1,
        points[points.len() - 1].0,
        points[points.len() - 1].1,
        params.origin.0,
        params.origin.1,
        params.scale.0,
        params.scale.1
    );
    if points.len() > 1 {
        let first = points[0];
        let last = points[points.len() - 1];
        out.push_str(&format!(
            "位移：dx={} dy={}\n",
            last.0 - first.0,
            last.1 - first.1
        ));
    }
    out.push_str("坐标列表：\n");
    let limit = points.len().min(100);
    for (index, (x, y)) in points.iter().take(limit).enumerate() {
        out.push_str(&format!("{}. ({x}, {y})\n", index + 1));
    }
    if points.len() > limit {
        out.push_str(&format!(
            "…（还有 {} 个点未列出）\n",
            points.len() - limit
        ));
    }
    out
}

async fn mouse_graph(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    if let Err(e) = input_allowed(ctx) {
        return e;
    }
    let expr_src = match arg_str_req(&args, "expr") {
        Ok(value) => value,
        Err(e) => return e,
    };
    let expr = match parse_graph_expr(&expr_src) {
        Ok(expr) => expr,
        Err(e) => return ToolOutcome::err(e),
    };
    // origin 缺省 = 当前鼠标位置（只读查询；dry_run 也允许缺省，但测试会显式传）
    let default_origin = match parse_xy_pair(args.get("origin"), "origin") {
        Ok(Some((x, y))) => (x.round() as i32, y.round() as i32),
        Ok(None) => match current_cursor_position().await {
            Ok(position) => position,
            Err(e) => return ToolOutcome::err(e),
        },
        Err(e) => return ToolOutcome::err(e),
    };
    let params = match parse_graph_params(&args, default_origin) {
        Ok(params) => params,
        Err(e) => return ToolOutcome::err(e),
    };
    let (points, skipped) = sample_graph_points(&expr, &params);
    if points.is_empty() {
        return ToolOutcome::err(
            "曲线在定义域内没有有效采样点（全部为 NaN/inf）；请检查表达式、定义域或 scale。"
                .to_string(),
        );
    }
    if params.dry_run {
        let content = render_graph_dry_run(&expr_src, &params, &points, skipped);
        return ToolOutcome::ok(content)
            .with_display(format!("dry_run：{} 个点", points.len()));
    }
    if ctx.is_cancelled() {
        return ToolOutcome::err("已取消：用户中断，未移动鼠标。".to_string());
    }

    let point_count = points.len();
    let duration_ms = params.duration_ms;
    let button = params.button.to_enigo();
    let per_step = Duration::from_millis((duration_ms / point_count.max(1) as u64).max(1));
    let ctx_for_loop = ctx.clone();
    let display = format!("曲线移动 {point_count} 个点");
    let result = enigo_blocking(move |enigo| {
        let mut pressed = false;
        let mut was_held = false;
        let inner = (|| -> Result<String, String> {
            if let Some(mouse_button) = button {
                was_held = held_lock().mouse_is_held(mouse_button);
                enigo
                    .button(mouse_button, Direction::Press)
                    .map_err(|e| format!("按下鼠标按键失败：{e}"))?;
                pressed = true;
                if !was_held {
                    held_lock().press_mouse(mouse_button);
                }
            }
            for (index, (x, y)) in points.iter().enumerate() {
                if ctx_for_loop.is_cancelled() {
                    return Err(format!(
                        "已取消：鼠标曲线移动到第 {}/{point_count} 个采样点时被用户中断。",
                        index + 1
                    ));
                }
                enigo
                    .move_mouse(*x, *y, Coordinate::Abs)
                    .map_err(|e| format!("移动到 ({x}, {y}) 失败：{e}"))?;
                std::thread::sleep(per_step);
            }
            Ok(format!(
                "[ok] 鼠标曲线移动完成\n表达式：{expr_src}\n定义域：x ∈ [{}, {}]\n采样数：{point_count}（跳过 NaN/inf：{skipped}）\n起点：({}, {})\n终点：({}, {})\n按键：{}\n时长：{duration_ms}ms",
                params.x_min,
                params.x_max,
                points[0].0,
                points[0].1,
                points[point_count - 1].0,
                points[point_count - 1].1,
                params.button.label()
            ))
        })();
        if pressed {
            if let Some(mouse_button) = button {
                let _ = enigo.button(mouse_button, Direction::Release);
                if !was_held {
                    held_lock().release_mouse(mouse_button);
                }
            }
        }
        inner
    })
    .await;
    match result {
        Ok(content) => ToolOutcome::ok(content).with_display(display),
        Err(e) => ToolOutcome::err(e),
    }
}

async fn mouse_drag(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    if let Err(e) = input_allowed(ctx) {
        return e;
    }
    let Some(from_x) = args.get("from_x").map(|_| arg_i64(&args, "from_x", 0)) else {
        return ToolOutcome::err("缺少必填参数 from_x。".to_string());
    };
    let Some(from_y) = args.get("from_y").map(|_| arg_i64(&args, "from_y", 0)) else {
        return ToolOutcome::err("缺少必填参数 from_y。".to_string());
    };
    let Some(to_x) = args.get("to_x").map(|_| arg_i64(&args, "to_x", 0)) else {
        return ToolOutcome::err("缺少必填参数 to_x。".to_string());
    };
    let Some(to_y) = args.get("to_y").map(|_| arg_i64(&args, "to_y", 0)) else {
        return ToolOutcome::err("缺少必填参数 to_y。".to_string());
    };
    let button_name = arg_str(&args, "button").unwrap_or_else(|| "left".to_string());
    let button = match parse_button(&button_name) {
        Ok(b) => b,
        Err(e) => return ToolOutcome::err(e),
    };
    let logical = arg_bool(&args, "logical", false);
    let coord_space = match parse_coord_space(&args) {
        Ok(space) => space,
        Err(e) => return ToolOutcome::err(e),
    };
    let (points, coord_note) = match convert_coords(
        coord_space,
        logical,
        &[(from_x, from_y), (to_x, to_y)],
    ) {
        Ok(value) => value,
        Err(e) => return ToolOutcome::err(e),
    };
    let ((from_x, from_y), (to_x, to_y)) = (points[0], points[1]);
    let coord_note_text = coord_note.clone();
    run_input(
        format!(
            "拖拽 ({from_x},{from_y}) -> ({to_x},{to_y}){}{}",
            if logical { "（逻辑坐标）" } else { "" },
            if coord_space == CoordSpace::Image {
                "（图片坐标）"
            } else {
                ""
            }
        ),
        move |enigo| {
            let (fx, fy) = resolve_point(logical, from_x, from_y);
            let (tx, ty) = resolve_point(logical, to_x, to_y);
            enigo
                .move_mouse(fx, fy, Coordinate::Abs)
                .map_err(|e| format!("移动到起点失败：{e}"))?;
            std::thread::sleep(Duration::from_millis(40));
            enigo
                .button(button, Direction::Press)
                .map_err(|e| format!("按下鼠标失败：{e}"))?;
            smooth_move(enigo, tx, ty, 20, Duration::from_millis(8));
            std::thread::sleep(Duration::from_millis(40));
            enigo
                .button(button, Direction::Release)
                .map_err(|e| format!("释放鼠标失败：{e}"))?;
            Ok(format!(
                "[ok] 已从 ({fx}, {fy}) 拖拽到 ({tx}, {ty}){}",
                coord_note_text
                    .as_ref()
                    .map(|note| format!("\n坐标换算：{note}"))
                    .unwrap_or_default()
            ))
        },
    )
    .await
}

/// 按下一个鼠标键并保持不松（手动拖拽：mouse_press → mouse_move → mouse_release）。
async fn mouse_press(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    if let Err(e) = input_allowed(ctx) {
        return e;
    }
    let has_x = args.get("x").is_some();
    let has_y = args.get("y").is_some();
    if has_x != has_y {
        return ToolOutcome::err("x 和 y 必须同时提供，或者都不提供。".to_string());
    }
    let logical = arg_bool(&args, "logical", false);
    let coord_space = match parse_coord_space(&args) {
        Ok(space) => space,
        Err(e) => return ToolOutcome::err(e),
    };
    let mut coord_note: Option<String> = None;
    let target: Option<(i64, i64)> = if has_x {
        let raw = (arg_i64(&args, "x", 0), arg_i64(&args, "y", 0));
        match convert_coords(coord_space, logical, &[raw]) {
            Ok((converted, note)) => {
                coord_note = note;
                Some(converted[0])
            }
            Err(e) => return ToolOutcome::err(e),
        }
    } else {
        if coord_space == CoordSpace::Image {
            return ToolOutcome::err(
                "coord_space=\"image\" 需要同时提供 x 和 y（图片像素坐标）。".to_string(),
            );
        }
        None
    };
    let button_name = arg_str(&args, "button").unwrap_or_else(|| "left".to_string());
    let button = match parse_button(&button_name) {
        Ok(button) => button,
        Err(e) => return ToolOutcome::err(e),
    };
    if held_lock().mouse_is_held(button) {
        return ToolOutcome::err(format!(
            "{}当前已经是按下状态，无需重复按下。\n\
             要继续拖拽请用 mouse_move / mouse_drag_path 移动，结束时用 mouse_release（不传 button 可松开全部）。",
            button_label(button)
        ));
    }
    ensure_release_on_exit();
    let label = button_label(button);
    let display = format!("按下{label}");
    let coord_note_text = coord_note.clone();
    let result = enigo_blocking(move |enigo| {
        if let Some((x, y)) = target {
            let (px, py) = resolve_point(logical, x, y);
            enigo
                .move_mouse(px, py, Coordinate::Abs)
                .map_err(|e| format!("移动鼠标到 ({px}, {py}) 失败：{e}"))?;
            std::thread::sleep(Duration::from_millis(30));
        }
        enigo
            .button(button, Direction::Press)
            .map_err(|e| format!("按下鼠标失败：{e}"))?;
        Ok(format!(
            "[ok] 已按下{label}并保持不松{}\n现在可以用 mouse_move 移动到目标，再用 mouse_release 松开（或 mouse_release 带 x/y 先移动再松）。{}",
            target
                .map(|(x, y)| format!("，位置 ({x}, {y}){}", if logical { "（逻辑坐标）" } else { "" }))
                .unwrap_or_default(),
            coord_note_text
                .as_ref()
                .map(|note| format!("\n坐标换算：{note}"))
                .unwrap_or_default()
        ))
    })
    .await;
    match result {
        Ok(msg) => {
            held_lock().press_mouse(button);
            ToolOutcome::ok(msg).with_display(display)
        }
        Err(e) => ToolOutcome::err(e),
    }
}

/// 松开鼠标键；不传 button 时松开所有按住的键（安全兜底）。
async fn mouse_release(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    if let Err(e) = input_allowed(ctx) {
        return e;
    }
    let has_x = args.get("x").is_some();
    let has_y = args.get("y").is_some();
    if has_x != has_y {
        return ToolOutcome::err("x 和 y 必须同时提供，或者都不提供。".to_string());
    }
    let logical = arg_bool(&args, "logical", false);
    let coord_space = match parse_coord_space(&args) {
        Ok(space) => space,
        Err(e) => return ToolOutcome::err(e),
    };
    let mut coord_note: Option<String> = None;
    let target: Option<(i64, i64)> = if has_x {
        let raw = (arg_i64(&args, "x", 0), arg_i64(&args, "y", 0));
        match convert_coords(coord_space, logical, &[raw]) {
            Ok((converted, note)) => {
                coord_note = note;
                Some(converted[0])
            }
            Err(e) => return ToolOutcome::err(e),
        }
    } else {
        if coord_space == CoordSpace::Image {
            return ToolOutcome::err(
                "coord_space=\"image\" 需要同时提供 x 和 y（图片像素坐标）。".to_string(),
            );
        }
        None
    };
    let explicit_button = match arg_str(&args, "button") {
        Some(name) => match parse_button(&name) {
            Ok(button) => Some(button),
            Err(e) => return ToolOutcome::err(e),
        },
        None => None,
    };
    let held_now: Vec<Button> = held_lock().mouse.clone();
    let targets = release_targets(explicit_button, &held_now);
    let was_held = match explicit_button {
        Some(button) => held_now.contains(&button),
        // 不传 button 时永远尝试松开全部（哪怕当前记录为空也走后续"没有按住"提示）
        None => !held_now.is_empty(),
    };
    if targets.is_empty() {
        return ToolOutcome::ok("当前没有按住的鼠标键，无需松开。")
            .with_display("未按住".to_string());
    }
    let logical = arg_bool(&args, "logical", false);
    let labels: Vec<&'static str> = targets.iter().map(|button| button_label(*button)).collect();
    let display = format!("松开{}", labels.join("+"));
    let release_targets = targets.clone();
    let result = enigo_blocking(move |enigo| {
        if let Some((x, y)) = target {
            let (px, py) = resolve_point(logical, x, y);
            enigo
                .move_mouse(px, py, Coordinate::Abs)
                .map_err(|e| format!("移动鼠标到 ({px}, {py}) 失败：{e}"))?;
            std::thread::sleep(Duration::from_millis(40));
        }
        for button in release_targets.iter().rev() {
            enigo
                .button(*button, Direction::Release)
                .map_err(|e| format!("松开鼠标失败：{e}"))?;
        }
        Ok(format!(
            "[ok] 已松开{}",
            release_targets
                .iter()
                .map(|button| button_label(*button))
                .collect::<Vec<_>>()
                .join("+")
        ))
    })
    .await;
    match result {
        Ok(mut msg) => {
            {
                let mut held = held_lock();
                for button in &targets {
                    held.release_mouse(*button);
                }
            }
            if explicit_button.is_some() && !was_held {
                msg.push_str("\n（该键之前未记录为按下，已补发一次松开事件）");
            }
            if let Some(note) = &coord_note {
                msg.push_str(&format!("\n坐标换算：{note}"));
            }
            ToolOutcome::ok(msg).with_display(display)
        }
        Err(e) => ToolOutcome::err(e),
    }
}

/// 决定 mouse_release 要松开哪些键：显式指定时只松该键；不指定时松开全部按住的键。
/// （保证"release 不传 button 能撤销所有按住状态"，不会漏松。）
fn release_targets(explicit: Option<Button>, held: &[Button]) -> Vec<Button> {
    match explicit {
        Some(button) => vec![button],
        None => held.to_vec(),
    }
}

async fn mouse_drag_path(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    if let Err(e) = input_allowed(ctx) {
        return e;
    }
    let raw_points = match parse_points(args.get("points")) {
        Ok(points) => points,
        Err(e) => return ToolOutcome::err(e),
    };
    let button_name = arg_str(&args, "button").unwrap_or_else(|| "left".to_string());
    let button = match parse_button(&button_name) {
        Ok(b) => b,
        Err(e) => return ToolOutcome::err(e),
    };
    let duration_ms = arg_u64(&args, "duration_ms", 600).clamp(50, 20_000);
    let hold_ms = arg_u64(&args, "hold_ms", 0).min(60_000);
    let logical = arg_bool(&args, "logical", false);
    let coord_space = match parse_coord_space(&args) {
        Ok(space) => space,
        Err(e) => return ToolOutcome::err(e),
    };
    let raw_points_i64: Vec<(i64, i64)> = raw_points
        .iter()
        .map(|&(x, y)| (x as i64, y as i64))
        .collect();
    let (converted_points, coord_note) =
        match convert_coords(coord_space, logical, &raw_points_i64) {
            Ok(value) => value,
            Err(e) => return ToolOutcome::err(e),
        };
    let raw_points: Vec<(i64, i64)> = converted_points;
    let point_count = raw_points.len();
    let coord_note_text = coord_note.clone();
    let display = format!(
        "沿 {point_count} 个点拖拽（约 {duration_ms}ms）{}",
        if coord_space == CoordSpace::Image {
            "（图片坐标）"
        } else {
            ""
        }
    );
    run_input(display, move |enigo| {
        let points: Vec<(i32, i32)> = raw_points
            .iter()
            .map(|&(x, y)| resolve_point(logical, x, y))
            .collect();
        let steps = (duration_ms / 16).clamp(8, 200) as u32;
        let path = interpolate_path(&points, steps);
        let Some(first) = path.first().copied() else {
            return Err("拖拽路径为空。".to_string());
        };
        enigo
            .move_mouse(first.0, first.1, Coordinate::Abs)
            .map_err(|e| format!("移动到起点失败：{e}"))?;
        std::thread::sleep(Duration::from_millis(40));
        enigo
            .button(button, Direction::Press)
            .map_err(|e| format!("按下鼠标失败：{e}"))?;
        let per_step = Duration::from_millis((duration_ms / path.len().max(1) as u64).max(2));
        for point in path.iter().skip(1) {
            enigo
                .move_mouse(point.0, point.1, Coordinate::Abs)
                .map_err(|e| format!("沿路径移动失败：{e}"))?;
            std::thread::sleep(per_step);
        }
        // hold_ms：到达终点后保持按住的时间（用于"拖到位停一下再松"）
        if hold_ms > 0 {
            std::thread::sleep(Duration::from_millis(hold_ms));
        } else {
            std::thread::sleep(Duration::from_millis(40));
        }
        enigo
            .button(button, Direction::Release)
            .map_err(|e| format!("释放鼠标失败：{e}"))?;
        Ok(format!(
            "[ok] 已沿 {point_count} 个点拖拽（实际 {} 步，约 {}ms，终点按住 {}ms）{}{}",
            path.len(),
            duration_ms,
            hold_ms,
            if logical { "，坐标为逻辑坐标" } else { "" },
            coord_note_text
                .as_ref()
                .map(|note| format!("\n坐标换算：{note}"))
                .unwrap_or_default()
        ))
    })
    .await
}

/// 计算滚轮最终格数：正数=向上、负数=向下。
/// 有 `direction`（up/down）时 `amount` 视为格数绝对值；没有则沿用 amount 正负语义。
fn scroll_plan(amount: i64, direction: Option<&str>) -> Result<i64, String> {
    let magnitude = amount.unsigned_abs().clamp(1, 500) as i64;
    match direction.map(|d| d.trim().to_ascii_lowercase()) {
        None => Ok(amount.clamp(-500, 500)),
        Some(d) if d.is_empty() => Ok(amount.clamp(-500, 500)),
        Some(d) => match d.as_str() {
            "up" | "上" | "upward" | "向上" => Ok(magnitude),
            "down" | "下" | "downward" | "向下" => Ok(-magnitude),
            other => Err(format!(
                "direction 只能是 up / down，收到 {other:?}。"
            )),
        },
    }
}

async fn mouse_scroll(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    if let Err(e) = input_allowed(ctx) {
        return e;
    }
    let raw_amount = args.get("amount").map(|_| arg_i64(&args, "amount", 0));
    let direction = arg_str(&args, "direction");
    if raw_amount.is_none() && direction.is_none() {
        return ToolOutcome::err(
            "缺少参数：给 amount（滚轮格数，正=上/负=下）或 direction（up/down）+ amount。"
                .to_string(),
        );
    }
    let amount = match scroll_plan(raw_amount.unwrap_or(1), direction.as_deref()) {
        Ok(value) => value,
        Err(e) => return ToolOutcome::err(e),
    };
    if amount == 0 {
        return ToolOutcome::ok("[ok] amount=0，没有滚动。").with_display("未滚动".to_string());
    }
    let has_x = args.get("x").is_some();
    let has_y = args.get("y").is_some();
    if has_x != has_y {
        return ToolOutcome::err("x 和 y 必须同时提供，或者都不提供。".to_string());
    }
    let target = if has_x {
        Some((arg_i64(&args, "x", 0), arg_i64(&args, "y", 0)))
    } else {
        None
    };
    let logical = arg_bool(&args, "logical", false);
    run_input(format!("滚动 {amount}"), move |enigo| {
        if let Some((x, y)) = target {
            let (px, py) = resolve_point(logical, x, y);
            enigo
                .move_mouse(px, py, Coordinate::Abs)
                .map_err(|e| format!("移动鼠标到 ({px}, {py}) 失败：{e}"))?;
            std::thread::sleep(Duration::from_millis(30));
        }
        // enigo：正数向下滚，所以工具语义（正数=上）取负号。
        let mut remaining = amount;
        while remaining != 0 {
            let step = remaining.clamp(-20, 20) as i32;
            enigo
                .scroll(-step, Axis::Vertical)
                .map_err(|e| format!("滚动失败：{e}"))?;
            remaining -= step as i64;
            if remaining != 0 {
                std::thread::sleep(Duration::from_millis(15));
            }
        }
        Ok(format!(
            "[ok] 已滚动 {} 格（{}）",
            amount,
            if amount > 0 { "向上" } else { "向下" }
        ))
    })
    .await
}

// ============================================================
// 键盘
// ============================================================

async fn keyboard_type(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    if let Err(e) = input_allowed(ctx) {
        return e;
    }
    let text = match arg_str_req(&args, "text") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let interval_ms = arg_u64(&args, "interval_ms", 0).min(5000);
    let char_count = text.chars().count();
    run_input(
        format!("输入 {char_count} 字符"),
        move |enigo| {
            if interval_ms > 0 {
                for ch in text.chars() {
                    enigo
                        .key(Key::Unicode(ch), Direction::Click)
                        .map_err(|e| format!("输入字符 {ch:?} 失败：{e}"))?;
                    std::thread::sleep(Duration::from_millis(interval_ms));
                }
                Ok(format!(
                    "[ok] 已逐字输入 {char_count} 个字符（间隔 {interval_ms}ms）。"
                ))
            } else {
                match enigo.text(&text) {
                    Ok(()) => Ok(format!("[ok] 已输入 {char_count} 个字符。")),
                    Err(err) => {
                        // 某些应用/输入法对 SendInput 不友好：用剪贴板粘贴兜底。
                        clipboard_paste(enigo, &text).map_err(|e| {
                            format!("enigo.text 失败（{err}），剪贴板兜底也失败（{e}）。")
                        })?;
                        Ok(format!(
                            "[ok] 已通过剪贴板粘贴输入 {char_count} 个字符（enigo.text 失败后兜底）。"
                        ))
                    }
                }
            }
        },
    )
    .await
}

fn clipboard_paste(enigo: &mut Enigo, text: &str) -> Result<(), String> {
    let mut clipboard = arboard::Clipboard::new().map_err(|e| format!("剪贴板不可用：{e}"))?;
    let old = clipboard.get_text().ok();
    clipboard
        .set_text(text.to_string())
        .map_err(|e| format!("写剪贴板失败：{e}"))?;
    std::thread::sleep(Duration::from_millis(80));
    enigo
        .key(Key::Control, Direction::Press)
        .map_err(|e| format!("按下 Ctrl 失败：{e}"))?;
    let paste = enigo.key(Key::Unicode('v'), Direction::Click);
    let release = enigo.key(Key::Control, Direction::Release);
    paste.map_err(|e| format!("粘贴失败：{e}"))?;
    release.map_err(|e| format!("释放 Ctrl 失败：{e}"))?;
    std::thread::sleep(Duration::from_millis(150));
    if let Some(old) = old {
        let _ = clipboard.set_text(old);
    }
    Ok(())
}

async fn keyboard_key(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    if let Err(e) = input_allowed(ctx) {
        return e;
    }
    let raw_key = match arg_str_req(&args, "key") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let repeat = arg_u64(&args, "repeat", 1).clamp(1, 50) as u32;

    // 支持 "ctrl+shift+t" 这种写法
    let mut modifier_names: Vec<String> = arg_string_list(&args, "modifiers");
    let mut key_name = raw_key.clone();
    if raw_key != "+" && raw_key.contains('+') {
        let parts: Vec<String> = raw_key
            .split('+')
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect();
        if parts.len() > 1 {
            for p in &parts[..parts.len() - 1] {
                modifier_names.push(p.clone());
            }
            key_name = parts[parts.len() - 1].clone();
        }
    }
    let key = match parse_key(&key_name) {
        Ok(k) => k,
        Err(e) => return ToolOutcome::err(e),
    };
    let mut modifiers: Vec<Key> = Vec::new();
    for name in &modifier_names {
        match parse_modifier(name) {
            Ok(k) => {
                if !modifiers.contains(&k) {
                    modifiers.push(k);
                }
            }
            Err(e) => return ToolOutcome::err(e),
        }
    }
    let combo_desc = {
        let mut parts: Vec<String> = modifier_names.clone();
        parts.push(key_name.clone());
        parts.join("+")
    };
    run_input(
        format!("按键 {combo_desc}"),
        move |enigo| {
            press_combo(enigo, &modifiers, key, repeat)?;
            Ok(format!(
                "[ok] 已按下 {combo_desc}{}",
                if repeat > 1 {
                    format!("（重复 {repeat} 次）")
                } else {
                    String::new()
                }
            ))
        },
    )
    .await
}

async fn keyboard_combo(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    if let Err(e) = input_allowed(ctx) {
        return e;
    }
    let keys = arg_string_list(&args, "keys");
    if keys.len() < 2 {
        return ToolOutcome::err(
            "keys 至少需要 2 个键，例如 [\"ctrl\",\"shift\",\"t\"]（最后一个为主键，前面都按住）。"
                .to_string(),
        );
    }
    let mut parsed: Vec<(String, Key)> = Vec::with_capacity(keys.len());
    for name in &keys {
        match parse_key(name) {
            Ok(key) => parsed.push((name.clone(), key)),
            Err(e) => return ToolOutcome::err(e),
        }
    }
    let Some((main_name, main_key)) = parsed.pop() else {
        return ToolOutcome::err("keys 不能为空。".to_string());
    };
    let modifier_names: Vec<String> = parsed.iter().map(|(name, _)| name.clone()).collect();
    let modifier_keys: Vec<Key> = parsed.iter().map(|(_, key)| *key).collect();
    let combo_desc = keys.join("+");
    let display = format!("组合键 {combo_desc}");
    run_input(display, move |enigo| {
        for (index, key) in modifier_keys.iter().enumerate() {
            enigo
                .key(*key, Direction::Press)
                .map_err(|e| format!("按下修饰键 {} 失败：{e}", modifier_names[index]))?;
        }
        let main_result = enigo
            .key(main_key, Direction::Click)
            .map_err(|e| format!("按下 {main_name} 失败：{e}"));
        for key in modifier_keys.iter().rev() {
            let _ = enigo.key(*key, Direction::Release);
        }
        main_result?;
        Ok(format!("[ok] 已按下组合键 {combo_desc}"))
    })
    .await
}

/// 按住一组键（不松），用于长按修饰键/方向键等长时间组合操作。
async fn keyboard_press(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    if let Err(e) = input_allowed(ctx) {
        return e;
    }
    let names = arg_string_list(&args, "keys");
    if names.is_empty() {
        return ToolOutcome::err(
            "缺少必填参数 keys（要按住的键数组，例如 [\"ctrl\",\"shift\"]）。".to_string(),
        );
    }
    let mut parsed: Vec<(String, Key)> = Vec::new();
    for name in &names {
        let key = match parse_key(name) {
            Ok(key) => key,
            Err(e) => return ToolOutcome::err(e),
        };
        if !parsed.iter().any(|(_, held)| *held == key) {
            parsed.push((name.clone(), key));
        }
    }
    let (mut to_press, mut already): (Vec<(String, Key)>, Vec<String>) = (Vec::new(), Vec::new());
    {
        let held = held_lock();
        for (name, key) in &parsed {
            if held.key_is_held(*key) {
                already.push(name.clone());
            } else {
                to_press.push((name.clone(), *key));
            }
        }
    }
    if to_press.is_empty() {
        return ToolOutcome::ok(format!(
            "[ok] {}已经是按下状态，没有重复按下。",
            already.join("/")
        ))
        .with_display("已按住".to_string());
    }
    ensure_release_on_exit();
    let press_names: Vec<String> = to_press.iter().map(|(name, _)| name.clone()).collect();
    let press_keys: Vec<Key> = to_press.iter().map(|(_, key)| *key).collect();
    let display = format!("按住{}", press_names.join("+"));
    let result = enigo_blocking(move |enigo| {
        for (index, key) in press_keys.iter().enumerate() {
            enigo
                .key(*key, Direction::Press)
                .map_err(|e| format!("按下 {} 失败：{e}", press_names[index]))?;
        }
        Ok(format!(
            "[ok] 已按住 {}（保持不松；用完请 keyboard_release，不传 keys 可松开全部）",
            press_names.join("+")
        ))
    })
    .await;
    match result {
        Ok(mut msg) => {
            {
                let mut held = held_lock();
                for (name, key) in &to_press {
                    held.press_key(name, *key);
                }
            }
            if !already.is_empty() {
                msg.push_str(&format!(
                    "\n（{} 之前已在按住，未重复按下）",
                    already.join("/")
                ));
            }
            ToolOutcome::ok(msg).with_display(display)
        }
        Err(e) => ToolOutcome::err(e),
    }
}

/// 松开一组键；不传 keys 时松开所有按住的键（安全兜底）。
async fn keyboard_release(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    if let Err(e) = input_allowed(ctx) {
        return e;
    }
    let names = arg_string_list(&args, "keys");
    let targets: Vec<(String, Key)> = if names.is_empty() {
        held_lock().keys.clone()
    } else {
        let mut parsed: Vec<(String, Key)> = Vec::new();
        for name in &names {
            let key = match parse_key(name) {
                Ok(key) => key,
                Err(e) => return ToolOutcome::err(e),
            };
            if !parsed.iter().any(|(_, held)| *held == key) {
                parsed.push((name.clone(), key));
            }
        }
        parsed
    };
    if targets.is_empty() {
        return ToolOutcome::ok("当前没有按住的键盘键，无需松开。")
            .with_display("未按住".to_string());
    }
    let not_held: Vec<String> = {
        let held = held_lock();
        targets
            .iter()
            .filter(|(_, key)| !held.key_is_held(*key))
            .map(|(name, _)| name.clone())
            .collect()
    };
    let target_names: Vec<String> = targets.iter().map(|(name, _)| name.clone()).collect();
    let target_keys: Vec<Key> = targets.iter().map(|(_, key)| *key).collect();
    let display = format!("松开{}", target_names.join("+"));
    let result = enigo_blocking(move |enigo| {
        for (index, key) in target_keys.iter().enumerate().rev() {
            enigo
                .key(*key, Direction::Release)
                .map_err(|e| format!("松开 {} 失败：{e}", target_names[index]))?;
        }
        Ok(format!(
            "[ok] 已松开 {}",
            target_names.join("+")
        ))
    })
    .await;
    match result {
        Ok(mut msg) => {
            {
                let mut held = held_lock();
                for (_, key) in &targets {
                    held.release_key(*key);
                }
            }
            if !not_held.is_empty() {
                msg.push_str(&format!(
                    "\n（{} 之前未记录为按下，已补发松开事件）",
                    not_held.join("/")
                ));
            }
            ToolOutcome::ok(msg).with_display(display)
        }
        Err(e) => ToolOutcome::err(e),
    }
}

// ============================================================
// 剪贴板
// ============================================================

async fn clipboard_read(ctx: &ToolCtx, _args: Value) -> ToolOutcome {
    if let Err(e) = input_allowed(ctx) {
        return e;
    }
    let joined = tokio::task::spawn_blocking(|| -> Result<String, String> {
        let mut clipboard =
            arboard::Clipboard::new().map_err(|e| format!("剪贴板不可用：{e}"))?;
        match clipboard.get_text() {
            Ok(text) => Ok(text),
            Err(e) => Err(format!(
                "剪贴板里没有文本（可能是图片/文件或空）：{e}"
            )),
        }
    })
    .await;
    match joined {
        Ok(Ok(text)) => {
            let chars = text.chars().count();
            ToolOutcome::ok(format!(
                "[ok] 剪贴板文本（{chars} 字符）：\n{}",
                truncate_chars(&text, 20_000)
            ))
            .with_display(format!("{chars} 字符"))
        }
        Ok(Err(e)) => ToolOutcome::err(e),
        Err(e) => ToolOutcome::err(format!("读取剪贴板任务异常：{e}")),
    }
}

async fn clipboard_write(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    if let Err(e) = input_allowed(ctx) {
        return e;
    }
    let text = match arg_str(&args, "text") {
        Some(t) => t,
        None => return ToolOutcome::err("缺少必填参数 text（要写入剪贴板的文本）。".to_string()),
    };
    let chars = text.chars().count();
    let joined = tokio::task::spawn_blocking(move || -> Result<(), String> {
        let mut clipboard =
            arboard::Clipboard::new().map_err(|e| format!("剪贴板不可用：{e}"))?;
        clipboard
            .set_text(text)
            .map_err(|e| format!("写入剪贴板失败：{e}"))
    })
    .await;
    match joined {
        Ok(Ok(())) => ToolOutcome::ok(format!("[ok] 已写入剪贴板（{chars} 字符）。"))
            .with_display(format!("写入 {chars} 字符")),
        Ok(Err(e)) => ToolOutcome::err(e),
        Err(e) => ToolOutcome::err(format!("写剪贴板任务异常：{e}")),
    }
}

// ============================================================
// 工具声明
// ============================================================

tool_impl!(
    MouseMove,
    "mouse_move",
    "把鼠标移动到指定的绝对坐标（默认物理像素）。smooth=true（默认）会平滑移动，\
     对需要悬停/拖拽的界面更自然；smooth=false 直接跳转。\
     坐标是逻辑像素时传 logical=true，会按主屏缩放自动换算成物理像素。\
     从截图估算的图片坐标传 coord_space=\"image\"，会按最近一次 screenshot 的 scale 自动换算成屏幕物理像素。",
    json!({
        "type": "object",
        "properties": {
            "x": {"type": "integer", "description": "横坐标（默认物理像素）"},
            "y": {"type": "integer", "description": "纵坐标（默认物理像素）"},
            "smooth": {"type": "boolean", "description": "是否平滑移动，默认 true"},
            "logical": {"type": "boolean", "description": "x/y 是逻辑坐标时传 true（按主屏 scale 换算），默认 false"},
            "coord_space": {"type": "string", "enum": ["screen", "image"], "description": "坐标空间：screen(默认)=屏幕物理像素；image=最近一次 screenshot 的图片像素（自动乘 scale 换算）"},
        },
        "required": ["x", "y"]
    }),
    mouse_move
);

tool_impl!(
    MouseClick,
    "mouse_click",
    "点击鼠标，支持左键/右键/中键与双击。\n\
     - 左键单击：默认（button=left）；\n\
     - 右键（打开上下文菜单）：button=right；\n\
     - 中键：button=middle；\n\
     - 左键双击（选中单词/打开文件）：double=true（自动点 2 次，间隔会压到系统双击阈值内）；\n\
     - 连续多次点击：count=3（旧参数 clicks 仍兼容）。\n\
     不给 x/y 时点击当前位置；给了坐标会先移动再点击。坐标默认物理像素，逻辑坐标传 logical=true。\
     从截图估算的图片坐标传 coord_space=\"image\"（需先 screenshot，10 分钟内有效），会自动乘 scale。",
    json!({
        "type": "object",
        "properties": {
            "x": {"type": "integer", "description": "可选横坐标（默认物理像素）"},
            "y": {"type": "integer", "description": "可选纵坐标（默认物理像素）"},
            "button": {"type": "string", "enum": ["left", "right", "middle", "back", "forward"], "description": "鼠标按键：left(左键,默认) / right(右键) / middle(中键)"},
            "double": {"type": "boolean", "description": "是否双击（左键双击=选中单词/打开文件），默认 false"},
            "count": {"type": "integer", "description": "点击次数，默认 1；double=true 时至少 2"},
            "clicks": {"type": "integer", "description": "旧参数，等价 count（保留兼容）"},
            "interval_ms": {"type": "integer", "description": "多次点击间隔毫秒，默认 80"},
            "logical": {"type": "boolean", "description": "x/y 是逻辑坐标时传 true，默认 false"},
            "coord_space": {"type": "string", "enum": ["screen", "image"], "description": "坐标空间：screen(默认)=屏幕物理像素；image=最近一次 screenshot 的图片像素（自动乘 scale 换算）"},
        }
    }),
    mouse_click
);

tool_impl!(
    MouseClickSequence,
    "mouse_click_sequence",
    "一次设定多个点，依次移动+点击（最多 30 个点）。\n\
     需要连续点击多个位置时用这个：比多次调用 mouse_click 更稳定、少来回、每步可单独配按键/延迟。\n\
     points 每项：{x, y, button?: left/right/middle, double?: bool, delay_ms?, logical?}；\n\
     interval_ms 是默认步间延迟（默认 120ms，单点 delay_ms 优先）。返回实际执行点数与总耗时；\n\
     某一步失败会立即中止并返回“第几步失败”（含坐标），不会继续乱点。\n\
     coord_space=\"image\" 时所有点按最近一次 screenshot 的图片像素自动乘 scale 换算成屏幕物理像素。",
    json!({
        "type": "object",
        "properties": {
            "points": {
                "type": "array",
                "minItems": 1,
                "maxItems": 30,
                "description": "点击点数组，例如 [{\"x\":100,\"y\":200},{\"x\":400,\"y\":200,\"button\":\"right\"}]",
                "items": {
                    "type": "object",
                    "properties": {
                        "x": {"type": "integer"},
                        "y": {"type": "integer"},
                        "button": {"type": "string", "enum": ["left", "right", "middle"], "description": "默认 left；可 right（右键菜单）"},
                        "double": {"type": "boolean", "description": "是否双击，默认 false"},
                        "delay_ms": {"type": "integer", "description": "这一步之后到下一点的等待毫秒，默认取 interval_ms"},
                        "logical": {"type": "boolean", "description": "x/y 是逻辑坐标时传 true"}
                    },
                    "required": ["x", "y"]
                }
            },
            "interval_ms": {"type": "integer", "description": "默认步间延迟毫秒，默认 120，最大 5000"},
            "coord_space": {"type": "string", "enum": ["screen", "image"], "description": "坐标空间：screen(默认)=屏幕物理像素；image=最近一次 screenshot 的图片像素（自动乘 scale 换算）"}
        },
        "required": ["points"]
    }),
    mouse_click_sequence
);

tool_impl!(
    MouseGraph,
    "mouse_graph",
    "按 y=f(x) 曲线移动鼠标（用于画图、演示轨迹、沿曲线拖拽）。\n\
     expr 只含 x，支持 + - * / ^、括号、一元负号、小数、pi/e；\
     函数：sin cos tan asin acos atan sqrt abs exp ln log(以 10 为底) floor ceil round min(a,b) max(a,b)。\n\
     坐标：screen_x = origin.x + x * scale.x，screen_y = origin.y - f(x) * scale.y（y 轴向上）；\
     origin 默认当前鼠标位置，scale 默认 1.0。\n\
     采样点里 NaN/inf 会自动跳过；button=left/right 时按住到结束（取消也会释放）。\n\
     dry_run=true 只解析+采样并返回坐标列表，不移动鼠标（推荐先用它确认轨迹）。",
    json!({
        "type": "object",
        "properties": {
            "expr": {"type": "string", "description": "只含 x 的表达式，例如 \"sin(x)\"、\"x^2/10\"、\"2*pi*x\""},
            "x_min": {"type": "number", "description": "定义域起点（必须 < x_max）"},
            "x_max": {"type": "number", "description": "定义域终点（跨度 <= 10000）"},
            "samples": {"type": "integer", "description": "采样点数，默认 120，范围 2-2000"},
            "origin": {
                "type": "object",
                "description": "坐标原点（屏幕物理像素），默认当前鼠标位置",
                "properties": {"x": {"type": "integer"}, "y": {"type": "integer"}}
            },
            "scale": {
                "type": "object",
                "description": "x/y 缩放（像素/单位），默认 1.0",
                "properties": {"x": {"type": "number"}, "y": {"type": "number"}}
            },
            "duration_ms": {"type": "integer", "description": "整个曲线移动的时长毫秒，默认 800，范围 50-10000"},
            "button": {"type": "string", "enum": ["none", "left", "right"], "description": "移动时按住的鼠标键，默认 none"},
            "dry_run": {"type": "boolean", "description": "只解析/采样并返回坐标列表，不真正移动鼠标（默认 false）"}
        },
        "required": ["expr", "x_min", "x_max"]
    }),
    mouse_graph
);

tool_impl!(
    MouseDrag,
    "mouse_drag",
    "按住鼠标从起点拖到终点（默认物理像素）。常用于拖动文件、选中文本、拖滚动条。\
     逻辑坐标传 logical=true；从截图估算的图片坐标传 coord_space=\"image\"（自动乘 scale）。\n\
     如果拖拽过程需要中间停顿、或要分多步观察，改用 mouse_press → mouse_move（可多次）→ mouse_release 的手动模式。",
    json!({
        "type": "object",
        "properties": {
            "from_x": {"type": "integer", "description": "起点横坐标"},
            "from_y": {"type": "integer", "description": "起点纵坐标"},
            "to_x": {"type": "integer", "description": "终点横坐标"},
            "to_y": {"type": "integer", "description": "终点纵坐标"},
            "button": {"type": "string", "enum": ["left", "right", "middle"], "description": "按键，默认 left"},
            "logical": {"type": "boolean", "description": "坐标是逻辑像素时传 true，默认 false"},
            "coord_space": {"type": "string", "enum": ["screen", "image"], "description": "坐标空间：screen(默认)=屏幕物理像素；image=最近一次 screenshot 的图片像素（自动乘 scale 换算）"},
        },
        "required": ["from_x", "from_y", "to_x", "to_y"]
    }),
    mouse_drag
);

tool_impl!(
    MousePress,
    "mouse_press",
    "按下一个鼠标键并保持不松（可选先移动到 x/y）。\n\
     手动拖拽：mouse_press（按下）→ mouse_move（可多次移动，过程中一直按着）→ mouse_release（松开）；\n\
     需要\"按住等一会儿再松\"时，可以在 press 和 release 之间用 wait，或直接用 mouse_drag_path 的 hold_ms。\n\
     重复 press 同一个键会返回提示（先 release）。进程退出时工具会自动兜底释放所有按住的键，不会卡住鼠标。\n\
     x/y 可传图片坐标（coord_space=\"image\"，自动按最近一次 screenshot 的 scale 换算）。",
    json!({
        "type": "object",
        "properties": {
            "button": {"type": "string", "enum": ["left", "right", "middle"], "description": "鼠标键：left(默认)/right/middle"},
            "x": {"type": "integer", "description": "可选，按下前先移动到的横坐标（物理像素）"},
            "y": {"type": "integer", "description": "可选，按下前先移动到的纵坐标（物理像素）"},
            "logical": {"type": "boolean", "description": "x/y 是逻辑坐标时传 true，默认 false"},
            "coord_space": {"type": "string", "enum": ["screen", "image"], "description": "坐标空间：screen(默认)=屏幕物理像素；image=最近一次 screenshot 的图片像素（自动乘 scale 换算）"},
        }
    }),
    mouse_press
);

tool_impl!(
    MouseRelease,
    "mouse_release",
    "松开鼠标键。不传 button 时会释放所有按住的鼠标键（安全兜底）。\n\
     可选先移动到 x/y 再松开（拖到目标位置后再松手）；逻辑坐标传 logical=true。\n\
     x/y 也可传图片坐标（coord_space=\"image\"，自动按最近一次 screenshot 的 scale 换算）。\n\
     配合 mouse_press 完成\"按住 → 移动 → 松开\"的手动拖拽。",
    json!({
        "type": "object",
        "properties": {
            "button": {"type": "string", "enum": ["left", "right", "middle"], "description": "要松开的鼠标键；不传则松开全部按住的键"},
            "x": {"type": "integer", "description": "可选，松开前先移动到的横坐标（物理像素）"},
            "y": {"type": "integer", "description": "可选，松开前先移动到的纵坐标（物理像素）"},
            "logical": {"type": "boolean", "description": "x/y 是逻辑坐标时传 true，默认 false"},
            "coord_space": {"type": "string", "enum": ["screen", "image"], "description": "坐标空间：screen(默认)=屏幕物理像素；image=最近一次 screenshot 的图片像素（自动乘 scale 换算）"},
        }
    }),
    mouse_release
);


tool_impl!(
    MouseDragPath,
    "mouse_drag_path",
    "沿一条折线路径拖拽鼠标（模拟人手，分步移动）。points 至少 2 个点，按顺序连接；\
     duration_ms 控制总时长（默认 600ms）。适合拖过多个目标（如画图、拖选一大段文本、拖动滑块）。\n\
     从截图估算的图片坐标传 coord_space=\"image\"，points 会按最近一次 screenshot 的 scale 自动换算。",
    json!({
        "type": "object",
        "properties": {
            "points": {
                "type": "array",
                "description": "路径点数组，例如 [{\"x\":100,\"y\":100},{\"x\":300,\"y\":200}]",
                "items": {
                    "type": "object",
                    "properties": {
                        "x": {"type": "integer"},
                        "y": {"type": "integer"}
                    },
                    "required": ["x", "y"]
                }
            },
            "button": {"type": "string", "enum": ["left", "right", "middle"], "description": "按键，默认 left"},
            "duration_ms": {"type": "integer", "description": "整个拖拽的时长毫秒，默认 600，范围 50-20000"},
            "hold_ms": {"type": "integer", "description": "到达终点后保持按住多久再松开（毫秒，默认 0，最大 60000）"},
            "logical": {"type": "boolean", "description": "坐标是逻辑像素时传 true，默认 false"},
            "coord_space": {"type": "string", "enum": ["screen", "image"], "description": "坐标空间：screen(默认)=屏幕物理像素；image=最近一次 screenshot 的图片像素（自动乘 scale 换算）"},
        },
        "required": ["points"]
    }),
    mouse_drag_path
);

tool_impl!(
    MouseScroll,
    "mouse_scroll",
    "滚动鼠标滚轮（竖向）。\n\
     - direction=up / down + amount（amount 为格数，默认 1）：推荐写法，方向明确；\n\
     - 或只给 amount：正数=向上滚、负数=向下滚（兼容旧用法）。\n\
     可先移动到 x/y 再滚动；逻辑坐标传 logical=true。滚动后可用 screenshot 或 job_status 确认效果。",
    json!({
        "type": "object",
        "properties": {
            "direction": {"type": "string", "enum": ["up", "down"], "description": "滚动方向：up=向上、down=向下"},
            "amount": {"type": "integer", "description": "滚轮格数（正数向上、负数向下；给 direction 时只取绝对值，默认 1）"},
            "x": {"type": "integer", "description": "可选，先移动到的横坐标"},
            "y": {"type": "integer", "description": "可选，先移动到的纵坐标"},
            "logical": {"type": "boolean", "description": "x/y 是逻辑像素时传 true，默认 false"}
        }
    }),
    mouse_scroll
);

tool_impl!(
    KeyboardType,
    "keyboard_type",
    "向当前焦点窗口输入一段文本（支持中文等 Unicode；先试 enigo 直接输入，失败时自动用剪贴板粘贴兜底）。\
     注意：会真的敲进当前应用，调用前请确认焦点正确。interval_ms>0 时逐字输入（某些应用需要）。",
    json!({
        "type": "object",
        "properties": {
            "text": {"type": "string", "description": "要输入的文本"},
            "interval_ms": {"type": "integer", "description": "逐字输入时每个字符的间隔毫秒，0（默认）表示一次性输入"}
        },
        "required": ["text"]
    }),
    keyboard_type
);

tool_impl!(
    KeyboardKey,
    "keyboard_key",
    "按一个键或组合键。key 支持 enter/esc/tab/space/backspace/delete/insert/home/end/pgup/pgdn/\
     up/down/left/right/f1-f20/a-z/0-9 以及任意单字符；modifiers 传 [\"ctrl\",\"shift\"]，\
     也可以直接写 key=\"ctrl+a\"。repeat 可重复按多次。",
    json!({
        "type": "object",
        "properties": {
            "key": {"type": "string", "description": "按键名，如 \"a\"、\"enter\"、\"ctrl+a\""},
            "modifiers": {
                "type": "array",
                "items": {"type": "string"},
                "description": "修饰键数组，如 [\"ctrl\",\"shift\"]"
            },
            "repeat": {"type": "integer", "description": "重复次数，默认 1"}
        },
        "required": ["key"]
    }),
    keyboard_key
);

tool_impl!(
    KeyboardCombo,
    "keyboard_combo",
    "按一个组合键（数组形式，最后一个键是主键，前面的都按住再松开）。\
     例如 [\"ctrl\",\"shift\",\"t\"]、[\"alt\",\"f4\"]、[\"ctrl\",\"a\"]。\
     比 keyboard_key 的 modifiers 更直观，适合快捷键场景。",
    json!({
        "type": "object",
        "properties": {
            "keys": {
                "type": "array",
                "items": {"type": "string"},
                "description": "按键序列，最后一个为主键，如 [\"ctrl\",\"shift\",\"t\"]"
            }
        },
        "required": ["keys"]
    }),
    keyboard_combo
);

tool_impl!(
    KeyboardPress,
    "keyboard_press",
    "按住一组键不放（如 [\"ctrl\",\"shift\"] 按住两个修饰键，或 [\"right\"] 长按方向键）。\n\
     用于\"按住修饰键 → 中间做别的操作 → 松开\"的长时间组合场景；用完必须 keyboard_release。\n\
     重复按同一个键会提示（不会重复按下）。进程退出时工具会自动兜底释放所有按住的键。",
    json!({
        "type": "object",
        "properties": {
            "keys": {
                "type": "array",
                "items": {"type": "string"},
                "description": "要按住的键，例如 [\"ctrl\",\"shift\"]；键名同 keyboard_key"
            }
        },
        "required": ["keys"]
    }),
    keyboard_press
);

tool_impl!(
    KeyboardRelease,
    "keyboard_release",
    "松开一组键；不传 keys 时释放所有按住的键盘键（安全兜底）。\n\
     和 keyboard_press 配对使用，例如 keyboard_press([\"shift\"]) → 选中文本 → keyboard_release([\"shift\"])。",
    json!({
        "type": "object",
        "properties": {
            "keys": {
                "type": "array",
                "items": {"type": "string"},
                "description": "要松开的键；不传则松开全部按住的键"
            }
        }
    }),
    keyboard_release
);

tool_impl!(
    ClipboardRead,
    "clipboard_read",
    "读取系统剪贴板里的文本。剪贴板是图片/文件时会明确报错。",
    json!({"type": "object", "properties": {}}),
    clipboard_read
);

tool_impl!(
    ClipboardWrite,
    "clipboard_write",
    "把文本写入系统剪贴板（常用于把结果交给用户，或配合 keyboard_key 的 ctrl+v 粘贴）。",
    json!({
        "type": "object",
        "properties": {
            "text": {"type": "string", "description": "要写入的文本"}
        },
        "required": ["text"]
    }),
    clipboard_write
);

// ============================================================
// 测试（只测纯解析逻辑，不真的操作鼠标键盘）
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// 串行化会读写全局按下状态的测试，避免并行测试互相干扰。
    static HELD_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn buttons() {
        assert_eq!(parse_button("left").unwrap(), Button::Left);
        assert_eq!(parse_button("RIGHT").unwrap(), Button::Right);
        assert_eq!(parse_button("m").unwrap(), Button::Middle);
        assert_eq!(parse_button("").unwrap(), Button::Left);
        assert!(parse_button("thumb").is_err());
    }

    #[test]
    fn keys() {
        assert_eq!(parse_key("enter").unwrap(), Key::Return);
        assert_eq!(parse_key("Esc").unwrap(), Key::Escape);
        assert_eq!(parse_key("pgup").unwrap(), Key::PageUp);
        assert_eq!(parse_key("f12").unwrap(), Key::F12);
        assert_eq!(parse_key("a").unwrap(), Key::Unicode('a'));
        assert_eq!(parse_key("中").unwrap(), Key::Unicode('中'));
        assert!(parse_key("f99").is_err());
        assert!(parse_key("nonsense").is_err());
        assert!(parse_key("").is_err());
    }

    #[test]
    fn modifiers() {
        assert_eq!(parse_modifier("ctrl").unwrap(), Key::Control);
        assert_eq!(parse_modifier("win").unwrap(), Key::Meta);
        assert_eq!(parse_modifier("shift").unwrap(), Key::Shift);
        assert_eq!(parse_modifier("alt").unwrap(), Key::Alt);
        assert!(parse_modifier("hyper").is_err());
    }

    #[test]
    fn combo_split_logic() {
        // 与 keyboard_key 内部一致的拆分规则（抽出来方便测）
        fn split(raw: &str) -> (Vec<String>, String) {
            let mut mods = Vec::new();
            let mut key = raw.to_string();
            if raw != "+" && raw.contains('+') {
                let parts: Vec<String> = raw
                    .split('+')
                    .map(|p| p.trim().to_string())
                    .filter(|p| !p.is_empty())
                    .collect();
                if parts.len() > 1 {
                    for p in &parts[..parts.len() - 1] {
                        mods.push(p.clone());
                    }
                    key = parts[parts.len() - 1].clone();
                }
            }
            (mods, key)
        }
        assert_eq!(
            split("ctrl+shift+t"),
            (vec!["ctrl".to_string(), "shift".to_string()], "t".to_string())
        );
        assert_eq!(split("+").1, "+");
        assert_eq!(split("a").1, "a");
        assert!(split("ctrl+a").0.contains(&"ctrl".to_string()));
    }

    #[test]
    fn logical_coordinate_conversion() {
        assert_eq!(convert_point(false, 100, 200, 2.5), (100, 200));
        assert_eq!(convert_point(true, 100, 200, 2.5), (250, 500));
        assert_eq!(convert_point(true, 1536, 864, 2.5), (3840, 2160));
        // 非法 scale 退化为 1.0，不 panic
        assert_eq!(convert_point(true, 10, 20, 0.0), (10, 20));
    }

    #[test]
    fn drag_path_interpolation() {
        let path = interpolate_path(&[(0, 0), (100, 100)], 10);
        assert_eq!(path.first(), Some(&(0, 0)));
        assert_eq!(path.last(), Some(&(100, 100)));
        assert!(path.len() >= 10, "步数不足: {}", path.len());
        let mut prev = path[0];
        for point in &path[1..] {
            assert!(point.0 >= prev.0 && point.1 >= prev.1);
            prev = *point;
        }
        // 多段折线：拐点必须被经过
        let multi = interpolate_path(&[(0, 0), (10, 0), (10, 10)], 20);
        assert!(multi.contains(&(10, 0)), "{multi:?}");
        assert_eq!(multi.last(), Some(&(10, 10)));
        // 少于 2 点原样返回
        assert_eq!(interpolate_path(&[(5, 5)], 10), vec![(5, 5)]);
    }

    #[test]
    fn points_parsing() {
        let ok = parse_points(Some(&json!([{"x": 1, "y": 2}, {"x": "3", "y": 4}]))).unwrap();
        assert_eq!(ok, vec![(1, 2), (3, 4)]);
        assert!(parse_points(None).is_err());
        assert!(parse_points(Some(&json!([{"x": 1, "y": 2}]))).is_err());
        assert!(parse_points(Some(&json!([{"x": 1}, {"y": 2}]))).is_err());
        assert!(parse_points(Some(&json!("nope"))).is_err());
    }

    #[test]
    fn keyboard_combo_parsing() {
        let keys = ["ctrl", "shift", "t"];
        let parsed: Vec<Key> = keys.iter().map(|k| parse_key(k).unwrap()).collect();
        assert_eq!(parsed[0], Key::Control);
        assert_eq!(parsed[1], Key::Shift);
        assert_eq!(parsed[2], Key::Unicode('t'));
        assert!(parse_key("alt").is_ok());
    }

    #[test]
    fn click_plan_parsing() {
        // 默认单击
        assert_eq!(click_plan(false, None, None, 80), (1, 80));
        // double=true 自动至少两次，并把间隔压到双击阈值内
        assert_eq!(click_plan(true, None, None, 80), (2, 80));
        assert_eq!(click_plan(true, None, None, 900), (2, 180));
        // count 优先于旧 clicks
        assert_eq!(click_plan(false, Some(3), Some(5), 80), (3, 80));
        assert_eq!(click_plan(false, None, Some(4), 80), (4, 80));
        // double=true 时 count=1 也升到 2
        assert_eq!(click_plan(true, Some(1), None, 60), (2, 60));
        // 上限保护
        assert_eq!(click_plan(false, Some(99), None, 80), (10, 80));
        // 不同按键的中文名（右键/中键文案）
        assert_eq!(button_label(Button::Right), "右键");
        assert_eq!(button_label(Button::Middle), "中键");
        assert_eq!(button_label(Button::Left), "左键");
        assert_eq!(parse_button("right").unwrap(), Button::Right);
        assert_eq!(parse_button("middle").unwrap(), Button::Middle);
    }

    #[test]
    fn scroll_plan_parsing() {
        // 兼容旧语义：正数向上、负数向下
        assert_eq!(scroll_plan(3, None).unwrap(), 3);
        assert_eq!(scroll_plan(-3, None).unwrap(), -3);
        // direction 明确方向，amount 取绝对值
        assert_eq!(scroll_plan(3, Some("up")).unwrap(), 3);
        assert_eq!(scroll_plan(3, Some("down")).unwrap(), -3);
        assert_eq!(scroll_plan(-3, Some("down")).unwrap(), -3);
        assert_eq!(scroll_plan(-3, Some("UP")).unwrap(), 3);
        // 缺 amount 且有方向时默认 1 格
        assert_eq!(scroll_plan(1, Some("down")).unwrap(), -1);
        // 非法方向
        assert!(scroll_plan(1, Some("left")).is_err());
    }

    #[test]
    fn click_sequence_parsing() {
        // 正常 3 个点：默认 left、默认 interval、按需覆盖
        let value = json!([
            {"x": 10, "y": 20},
            {"x": "300", "y": 400, "button": "right", "delay_ms": 80},
            {"x": 500, "y": 600, "button": "left", "double": true, "logical": true}
        ]);
        let steps = parse_click_sequence(Some(&value), 200).unwrap();
        assert_eq!(steps.len(), 3);
        assert_eq!((steps[0].x, steps[0].y), (10, 20));
        assert_eq!(steps[0].button, Button::Left);
        assert_eq!(steps[0].delay_ms, 200, "未指定 delay_ms 时用 interval_ms");
        assert!(!steps[0].double && !steps[0].logical);
        assert_eq!((steps[1].x, steps[1].y), (300, 400));
        assert_eq!(steps[1].button, Button::Right);
        assert_eq!(steps[1].delay_ms, 80);
        assert!(steps[2].double && steps[2].logical);
        // 边界：interval_ms=0 时缺省 120
        let steps = parse_click_sequence(Some(&json!([{"x": 1, "y": 2}])), 0).unwrap();
        assert_eq!(steps[0].delay_ms, 120);
        // 上限 30 个点
        let points: Vec<Value> = (0..30).map(|i| json!({"x": i, "y": i})).collect();
        assert!(parse_click_sequence(Some(&json!(points)), 120).is_ok());
        let points: Vec<Value> = (0..31).map(|i| json!({"x": i, "y": i})).collect();
        assert!(parse_click_sequence(Some(&json!(points)), 120).is_err());
        // 错误路径
        assert!(parse_click_sequence(None, 120).is_err(), "缺 points 应报错");
        assert!(parse_click_sequence(Some(&json!([])), 120).is_err(), "空数组应报错");
        assert!(parse_click_sequence(Some(&json!([{"y": 2}])), 120).is_err(), "缺 x");
        assert!(parse_click_sequence(Some(&json!([{"x": 1}])), 120).is_err(), "缺 y");
        assert!(parse_click_sequence(Some(&json!([{"x": 1, "y": 2, "button": "thumb"}])), 120).is_err());
        assert!(parse_click_sequence(Some(&json!([1, 2, 3])), 120).is_err());
        // delay_ms 上限保护
        let steps = parse_click_sequence(
            Some(&json!([{"x": 1, "y": 2, "delay_ms": 999_999}])),
            120,
        )
        .unwrap();
        assert_eq!(steps[0].delay_ms, 5000);
    }

    #[test]
    fn held_state_machine() {
        let mut state = HeldState::default();
        // 鼠标：按 -> 重复按 -> 松 -> 重复松
        assert!(state.press_mouse(Button::Left));
        assert!(!state.press_mouse(Button::Left), "重复 press 应返回失败");
        assert!(state.mouse_is_held(Button::Left));
        assert!(state.release_mouse(Button::Left));
        assert!(!state.release_mouse(Button::Left), "未按就 release 应返回失败");
        assert!(!state.mouse_is_held(Button::Left));
        // 多个键可同时按住
        assert!(state.press_mouse(Button::Left));
        assert!(state.press_mouse(Button::Right));
        assert_eq!(state.mouse.len(), 2);
        let (mouse, keys) = state.snapshot();
        assert_eq!(mouse, vec!["左键".to_string(), "右键".to_string()]);
        assert!(keys.is_empty());
        // 键盘：按 -> 重复按 -> 松（返回键名）-> 重复松
        assert!(state.press_key("ctrl", Key::Control));
        assert!(!state.press_key("ctrl", Key::Control), "重复 press 应返回失败");
        assert!(state.key_is_held(Key::Control));
        assert_eq!(state.release_key(Key::Control), Some("ctrl".to_string()));
        assert_eq!(state.release_key(Key::Control), None, "未按就 release 应返回 None");
        // take_all 清空一切
        let _ = state.take_all();
        assert!(state.press_mouse(Button::Middle));
        assert!(state.press_key("shift", Key::Shift));
        let (mouse, keys) = state.take_all();
        assert_eq!(mouse, vec![Button::Middle]);
        assert_eq!(keys, vec![("shift".to_string(), Key::Shift)]);
        assert!(state.mouse.is_empty() && state.keys.is_empty());
    }

    #[test]
    fn held_state_duplicate_key_by_alias() {
        let mut state = HeldState::default();
        assert!(state.press_key("ctrl", Key::Control));
        // 同一物理键不同别名（control）也应判为已按下
        assert!(!state.press_key("control", Key::Control));
        assert_eq!(state.keys.len(), 1);
    }

    #[test]
    fn hold_release_guard_clears_held_state() {
        let _serial = HELD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // 准备全局状态（不真的按键，只写状态表）
        {
            let mut state = held_lock();
            state.take_all();
            state.press_mouse(Button::Left);
            state.press_key("ctrl", Key::Control);
        }
        let (mouse, keys) = held_snapshot();
        assert_eq!(mouse.len(), 1);
        assert_eq!(keys, vec!["ctrl".to_string()]);
        // Drop 兜底：会尝试向系统补发 release（当前其实没按下，无害），并清空状态
        drop(hold_release_guard());
        let (mouse, keys) = held_snapshot();
        assert!(mouse.is_empty(), "Drop 后不应还有按住的鼠标键");
        assert!(keys.is_empty(), "Drop 后不应还有按住的键盘键");
    }

    #[tokio::test]
    async fn press_duplicate_and_unheld_release_paths() {
        let _serial = HELD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // 清空全局状态，保证测试不依赖顺序
        {
            let mut state = held_lock();
            state.take_all();
        }
        let root = crate::test_support::temp_root("input-held");
        let (ctx, _host) = crate::test_support::fake_ctx(&root);

        // 非法按键直接报中文错误，不触发真实输入
        let bad_button = mouse_press(&ctx, json!({"button": "thumb"})).await;
        assert!(!bad_button.ok);
        assert!(
            bad_button.content.contains("不认识的鼠标按键"),
            "{}",
            bad_button.content
        );
        let bad_key = keyboard_press(&ctx, json!({"keys": ["not-a-key"]})).await;
        assert!(!bad_key.ok);
        let empty_keys = keyboard_press(&ctx, json!({"keys": []})).await;
        assert!(!empty_keys.ok, "keys 为空应报错");

        // 重复 press：先把左键标成已按下，再 mouse_press 应返回提示且不重复按
        {
            let mut state = held_lock();
            state.press_mouse(Button::Left);
        }
        let dup = mouse_press(&ctx, json!({"button": "left"})).await;
        assert!(!dup.ok, "重复 press 应失败: {}", dup.content);
        assert!(
            dup.content.contains("已经是按下状态"),
            "{}",
            dup.content
        );

        // 未按下时 mouse_release（指定键）应补发松开并提示
        let release = mouse_release(&ctx, json!({"button": "right"})).await;
        assert!(release.ok, "{}", release.content);
        assert!(release.content.contains("未记录为按下"), "{}", release.content);

        // 未按住任何键时 mouse_release 全部 / keyboard_release 全部：直接 ok，不触碰系统
        {
            let mut state = held_lock();
            state.take_all();
        }
        let release_all = mouse_release(&ctx, json!({})).await;
        assert!(release_all.ok, "{}", release_all.content);
        assert!(release_all.content.contains("没有按住"));
        let key_release_all = keyboard_release(&ctx, json!({})).await;
        assert!(key_release_all.ok, "{}", key_release_all.content);
        assert!(key_release_all.content.contains("没有按住"));

        // 收尾：清理全局状态，避免影响其他测试
        {
            let mut state = held_lock();
            state.take_all();
        }
    }

    #[test]
    fn release_targets_releases_all_when_button_omitted() {
        // 不传 button：松开所有按住的键（这是工具描述里承诺的行为）
        assert_eq!(release_targets(None, &[]), Vec::<Button>::new());
        assert_eq!(
            release_targets(None, &[Button::Left]),
            vec![Button::Left]
        );
        assert_eq!(
            release_targets(None, &[Button::Left, Button::Right, Button::Middle]),
            vec![Button::Left, Button::Right, Button::Middle]
        );
        // 传了 button：只松开指定的那个，其它保持按住
        assert_eq!(
            release_targets(Some(Button::Right), &[Button::Left, Button::Right]),
            vec![Button::Right]
        );
        assert_eq!(
            release_targets(Some(Button::Left), &[]),
            vec![Button::Left]
        );
    }

    #[test]
    fn graph_expression_parser() {
        fn eval(src: &str, x: f64) -> f64 {
            let expr = parse_graph_expr(src).expect("表达式应能解析");
            eval_graph_expr(&expr, x)
        }
        // 基本运算与优先级
        assert!((eval("x^2", 3.0) - 9.0).abs() < 1e-12);
        assert!((eval("-x^2", 2.0) + 4.0).abs() < 1e-12, "-x^2 应为 -(x^2)");
        assert!((eval("2+3*4", 0.0) - 14.0).abs() < 1e-12);
        assert!((eval("(2+3)*4", 0.0) - 20.0).abs() < 1e-12);
        assert!((eval("2^3^2", 0.0) - 512.0).abs() < 1e-12, "^ 应右结合");
        assert!((eval("2^-2", 0.0) - 0.25).abs() < 1e-12);
        assert!((eval("x/2", 9.0) - 4.5).abs() < 1e-12);
        // 常量与函数
        assert!((eval("sin(x)", std::f64::consts::FRAC_PI_2) - 1.0).abs() < 1e-12);
        assert!((eval("2*pi", 0.0) - std::f64::consts::TAU).abs() < 1e-12);
        assert!((eval("e", 0.0) - std::f64::consts::E).abs() < 1e-12);
        assert!((eval("cos(0)+tan(0)", 0.0) - 1.0).abs() < 1e-12);
        assert!((eval("asin(1)+acos(0)+atan(0)", 0.0) - std::f64::consts::PI).abs() < 1e-9);
        assert!((eval("sqrt(9)+exp(0)+ln(1)+log(100)", 0.0) - 6.0).abs() < 1e-12);
        assert!((eval("abs(-5)+floor(2.7)+ceil(2.1)+round(2.5)", 0.0) - 13.0).abs() < 1e-12);
        assert!((eval("min(3,4)+max(1,5)", 0.0) - 8.0).abs() < 1e-12);
        assert!((eval("min(x, 10)", 3.0) - 3.0).abs() < 1e-12);
        // 除零 → inf；负数开方 → NaN
        assert!(eval("1/x", 0.0).is_infinite(), "除零应得到 inf");
        assert!(eval("sqrt(x)", -1.0).is_nan(), "sqrt(-1) 应是 NaN");
        assert!(eval("ln(x)", -1.0).is_nan(), "ln(-1) 应是 NaN");
        // 非法表达式
        for bad in [
            "",
            "   ",
            "x+",
            "sin(",
            "sin()",
            "sin(1,2)",
            "foo(x)",
            "x @ 2",
            "2..3",
            "x 2",
            "()",
        ] {
            assert!(parse_graph_expr(bad).is_err(), "非法表达式应报错: {bad:?}");
        }
    }

    #[test]
    fn graph_params_validation() {
        let base = |extra: Value| {
            let mut value = json!({"x_min": 0.0, "x_max": 10.0});
            if let (Some(target), Some(source)) = (value.as_object_mut(), extra.as_object()) {
                for (key, item) in source {
                    target.insert(key.clone(), item.clone());
                }
            }
            value
        };
        let params = parse_graph_params(&base(json!({})), (100, 100)).unwrap();
        assert_eq!(params.samples, 120);
        assert_eq!(params.duration_ms, 800);
        assert_eq!(params.button, GraphButton::None);
        assert_eq!(params.origin, (100, 100));
        assert_eq!(params.scale, (1.0, 1.0));
        assert!(!params.dry_run);

        // samples / duration 边界 clamp
        let params = parse_graph_params(&base(json!({"samples": 1, "duration_ms": 1})), (0, 0)).unwrap();
        assert_eq!(params.samples, 2);
        assert_eq!(params.duration_ms, 50);
        let params = parse_graph_params(
            &base(json!({"samples": 99999, "duration_ms": 999999})),
            (0, 0),
        )
        .unwrap();
        assert_eq!(params.samples, 2000);
        assert_eq!(params.duration_ms, 10000);

        // origin/scale/button 解析
        let params = parse_graph_params(
            &base(json!({
                "origin": {"x": 50, "y": 60},
                "scale": {"x": 2.5, "y": 0.5},
                "button": "LEFT",
                "dry_run": true
            })),
            (0, 0),
        )
        .unwrap();
        assert_eq!(params.origin, (50, 60));
        assert_eq!(params.scale, (2.5, 0.5));
        assert_eq!(params.button, GraphButton::Left);
        assert!(params.dry_run);

        // 校验失败
        assert!(parse_graph_params(&json!({"x_max": 10.0}), (0, 0)).is_err(), "缺 x_min");
        assert!(parse_graph_params(&json!({"x_min": 0.0}), (0, 0)).is_err(), "缺 x_max");
        assert!(
            parse_graph_params(&json!({"x_min": 10.0, "x_max": 10.0}), (0, 0)).is_err(),
            "x_min 不能等于 x_max"
        );
        assert!(parse_graph_params(&json!({"x_min": 10.0, "x_max": 0.0}), (0, 0)).is_err());
        assert!(
            parse_graph_params(&json!({"x_min": 0.0, "x_max": 10001.0}), (0, 0)).is_err(),
            "跨度超过 10000 应报错"
        );
        assert!(
            parse_graph_params(&json!({"x_min": 0.0, "x_max": 10000.0}), (0, 0)).is_ok(),
            "跨度正好 10000 应允许"
        );
        assert!(
            parse_graph_params(&base(json!({"button": "middle"})), (0, 0)).is_err(),
            "未知 button 应报错"
        );
        assert!(
            parse_graph_params(&base(json!({"origin": {"x": 1}})), (0, 0)).is_err(),
            "origin 缺 y 应报错"
        );
    }

    #[test]
    fn graph_sampling_axis_and_skips() {
        let expr = parse_graph_expr("x").unwrap();
        let params = GraphParams {
            x_min: 0.0,
            x_max: 10.0,
            samples: 3,
            origin: (100, 200),
            scale: (2.0, 3.0),
            duration_ms: 800,
            button: GraphButton::None,
            dry_run: true,
        };
        let (points, skipped) = sample_graph_points(&expr, &params);
        // x=0/5/10；screen_x=100+x*2，screen_y=200-f(x)*3（y 轴向上）
        assert_eq!(points, vec![(100, 200), (110, 185), (120, 170)]);
        assert_eq!(skipped, 0);

        // NaN（sqrt 负区间）应跳过
        let expr = parse_graph_expr("sqrt(x)").unwrap();
        let params = GraphParams {
            x_min: -1.0,
            x_max: 1.0,
            samples: 5,
            origin: (0, 0),
            scale: (1.0, 1.0),
            duration_ms: 100,
            button: GraphButton::None,
            dry_run: true,
        };
        let (points, skipped) = sample_graph_points(&expr, &params);
        assert_eq!(skipped, 2, "x<0 的 sqrt 是 NaN，应跳过");
        assert_eq!(points.len(), 3);

        // inf（1/x 在 x=0）应跳过
        let expr = parse_graph_expr("1/x").unwrap();
        let params = GraphParams {
            x_min: -1.0,
            x_max: 1.0,
            samples: 3,
            origin: (0, 0),
            scale: (1.0, 1.0),
            duration_ms: 100,
            button: GraphButton::None,
            dry_run: true,
        };
        let (points, skipped) = sample_graph_points(&expr, &params);
        assert_eq!(skipped, 1, "x=0 时 1/x 是 inf，应跳过");
        assert_eq!(points.len(), 2);
    }

    #[tokio::test]
    async fn mouse_graph_dry_run_only() {
        let root = crate::test_support::temp_root("mouse-graph");
        let (ctx, _host) = crate::test_support::fake_ctx(&root);
        // dry_run：不移动、不按键，只返回坐标列表
        let out = mouse_graph(
            &ctx,
            json!({
                "expr": "x",
                "x_min": 0,
                "x_max": 10,
                "samples": 3,
                "origin": {"x": 100, "y": 200},
                "dry_run": true
            }),
        )
        .await;
        assert!(out.ok, "{}", out.content);
        println!("mouse_graph dry_run:\n{}", out.content);
        assert!(out.content.contains("dry_run：true"), "{}", out.content);
        assert!(out.content.contains("表达式：x"), "{}", out.content);
        assert!(out.content.contains("采样数：3"), "{}", out.content);
        assert!(out.content.contains("起点：(100, 200)"), "{}", out.content);
        assert!(out.content.contains("终点：(110, 190)"), "{}", out.content);
        assert!(out.content.contains("跳过 NaN/inf：0"), "{}", out.content);
        assert!(out.content.contains("(100, 200)") && out.content.contains("(110, 190)"), "{}", out.content);
        // 1/x 除零 inf 点应跳过并体现在输出
        let out = mouse_graph(
            &ctx,
            json!({
                "expr": "1/x", "x_min": -1, "x_max": 1, "samples": 3,
                "origin": {"x": 0, "y": 0}, "dry_run": true
            }),
        )
        .await;
        assert!(out.ok, "{}", out.content);
        assert!(out.content.contains("跳过 NaN/inf：1"), "{}", out.content);
        // 未知 button / 非法定义域 / 非法 expr
        let bad_button = mouse_graph(
            &ctx,
            json!({"expr": "x", "x_min": 0, "x_max": 1, "origin": {"x": 0, "y": 0}, "button": "middle", "dry_run": true}),
        )
        .await;
        assert!(!bad_button.ok);
        let bad_domain = mouse_graph(
            &ctx,
            json!({"expr": "x", "x_min": 2, "x_max": 1, "origin": {"x": 0, "y": 0}, "dry_run": true}),
        )
        .await;
        assert!(!bad_domain.ok);
        let bad_expr = mouse_graph(&ctx, json!({"expr": "sin(", "x_min": 0, "x_max": 1, "dry_run": true})).await;
        assert!(!bad_expr.ok);
    }

    // ============================================================
    // 自适应坐标：coord_space="image" 的换算与错误路径
    // ============================================================

    /// 串行化会读写全局截图契约的测试（与 screen 测试共用同一把锁）。

    #[test]
    fn coord_space_conversion() {
        let _serial = crate::screen::CONTRACT_TEST_LOCK.lock();
        crate::screen::clear_screenshot_contract_for_test();
        // screen（默认）：原样返回，无回显
        let (points, note) = convert_coords(CoordSpace::Screen, false, &[(600, 400)]).unwrap();
        assert_eq!(points, vec![(600, 400)]);
        assert!(note.is_none());
        // image 但没有截图契约 → 清晰中文错误
        let error = convert_coords(CoordSpace::Image, false, &[(600, 400)])
            .expect_err("未截图时应报错");
        assert!(error.contains("还没有可用的截图坐标契约"), "{error}");
        assert!(error.contains("请先调用 screenshot"), "{error}");
        // 写入 3840x2160 → 1536x864（scale 2.5）的契约
        crate::screen::set_screenshot_contract_for_test(1536, 864, 3840, 2160, 0, 0);
        let (points, note) = convert_coords(CoordSpace::Image, false, &[(600, 400)]).unwrap();
        assert_eq!(points, vec![(1500, 1000)]);
        let note = note.expect("image 模式应有回显");
        assert!(note.contains("图片(600,400) → 屏幕(1500,1000)"), "{note}");
        // 边界：左上 (0,0)、右下（取整）
        let (points, _) =
            convert_coords(CoordSpace::Image, false, &[(0, 0), (1535, 863)]).unwrap();
        assert_eq!(points, vec![(0, 0), (3838, 2158)]);
        // image + logical 冲突
        let error = convert_coords(CoordSpace::Image, true, &[(1, 1)]).expect_err("不能同时用");
        assert!(error.contains("不能同时使用"), "{error}");
        // 非等比缩放按 x/y 分别算
        crate::screen::set_screenshot_contract_for_test(1600, 1000, 3840, 2160, 0, 0);
        let (points, _) = convert_coords(CoordSpace::Image, false, &[(500, 500)]).unwrap();
        assert_eq!(points, vec![(1200, 1080)]);
        crate::screen::clear_screenshot_contract_for_test();
    }

    #[test]
    fn coord_space_arg_parsing() {
        assert_eq!(parse_coord_space(&json!({})).unwrap(), CoordSpace::Screen);
        assert_eq!(
            parse_coord_space(&json!({"coord_space": "screen"})).unwrap(),
            CoordSpace::Screen
        );
        assert_eq!(
            parse_coord_space(&json!({"coord_space": "IMAGE"})).unwrap(),
            CoordSpace::Image
        );
        assert!(parse_coord_space(&json!({"coord_space": "bogus"})).is_err());
    }

    #[tokio::test]
    async fn coord_space_image_errors_without_screenshot() {
        let _serial = crate::screen::CONTRACT_TEST_LOCK.lock();
        crate::screen::clear_screenshot_contract_for_test();
        let root = crate::test_support::temp_root("coord-space");
        let (ctx, _host) = crate::test_support::fake_ctx(&root);
        // 未截图：mouse_move(coord_space=image) 应在移动鼠标前报错
        let out = mouse_move(
            &ctx,
            json!({"x": 600, "y": 400, "coord_space": "image"}),
        )
        .await;
        assert!(!out.ok, "{}", out.content);
        assert!(
            out.content.contains("请先调用 screenshot"),
            "{}",
            out.content
        );
        // 非法 coord_space
        let out = mouse_move(&ctx, json!({"x": 1, "y": 2, "coord_space": "bogus"})).await;
        assert!(!out.ok);
        assert!(out.content.contains("coord_space 只能是"), "{}", out.content);
        // image + logical 冲突（不会触碰鼠标）
        let out = mouse_move(
            &ctx,
            json!({"x": 1, "y": 2, "coord_space": "image", "logical": true}),
        )
        .await;
        assert!(!out.ok);
        assert!(out.content.contains("不能同时使用"), "{}", out.content);
        // mouse_click 只给一个坐标时 image 模式需要 x/y
        let out = mouse_click(&ctx, json!({"coord_space": "image"})).await;
        assert!(!out.ok);
        assert!(out.content.contains("需要同时提供 x 和 y"), "{}", out.content);
        // mouse_click_sequence：无契约时报错（在点击前）
        let out = mouse_click_sequence(
            &ctx,
            json!({"points": [{"x": 10, "y": 20}], "coord_space": "image"}),
        )
        .await;
        assert!(!out.ok, "{}", out.content);
        assert!(
            out.content.contains("请先调用 screenshot"),
            "{}",
            out.content
        );
    }
}
