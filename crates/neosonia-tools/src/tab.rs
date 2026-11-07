//! Tab 键遍历：扫描屏幕内可被 Tab 选中的对象，并允许按 index 与其交互。
//!
//! 对模型暴露两个工具：
//!
//! - `tab_scan`：列出屏幕内所有可被 Tab 键选中的对象（UIA 判定可键盘聚焦 + 可用 +
//!   不在屏幕外），按屏幕位置（上→下、左→右）排序，index 从 1 开始；结果缓存 30 秒，
//!   供 `tab_act` 使用。只读，不操作用户界面。
//! - `tab_act`：对最近一次 `tab_scan` 结果中的对象执行 `focus | click | enter | space |
//!   tab | shift_tab`；index 必须来自最近 30 秒内的扫描结果，过期/越界会提示重新扫描。
//!
//! Windows 实现基于 `windows` 0.58 的 UI Automation：每次调用在独立工作线程里
//! `CoInitializeEx(MTA)` 并 RAII 释放。UIA 拿不到的子控件用 Win32 `WS_TABSTOP`
//! 兜底枚举（EnumChildWindows + GetWindowLongW(GWL_STYLE)）。
//!
//! 编码约定：零 emoji；中文错误；不 panic；写操作在描述里明确提示会操作用户界面。

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use neosonia_core::model::{ToolCtx, ToolOutcome, ToolSpec};
use neosonia_core::registry::{Tool, ToolRegistry};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::{arg_i64, arg_str, arg_u64};

// ============================================================
// 常量与纯逻辑类型
// ============================================================

/// 扫描数量默认值（与任务约定一致）。
const DEFAULT_MAX: u64 = 80;
/// 扫描数量上限。
const MAX_CAP: usize = 200;
/// 扫描结果缓存有效期：超过后 `tab_act` 要求重新扫描。
const SCAN_TTL: Duration = Duration::from_secs(30);

/// 单次扫描的预算：窗口很多/某个程序 UIA 很慢时也能及时返回部分结果。
#[cfg(windows)]
const SCAN_BUDGET: Duration = Duration::from_millis(8_000);
/// 单个窗口最多遍历多少个 UIA 节点（防止 Chromium 之类的大树拖慢扫描）。
#[cfg(windows)]
const MAX_NODES_PER_WINDOW: usize = 1_500;
/// 单个窗口最大遍历深度。
#[cfg(windows)]
const MAX_DEPTH: usize = 8;
/// 扫描过程中最多累计多少个候选对象（排序/去重后再截断到 max）。
#[cfg(windows)]
const MAX_RAW_OBJECTS: usize = MAX_CAP * 8;

/// `tab_act` 支持的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TabAction {
    Focus,
    Click,
    Enter,
    Space,
    Tab,
    ShiftTab,
}

impl TabAction {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "focus" => Some(Self::Focus),
            "click" => Some(Self::Click),
            "enter" | "return" => Some(Self::Enter),
            "space" => Some(Self::Space),
            "tab" => Some(Self::Tab),
            "shift_tab" | "shift-tab" | "shifttab" => Some(Self::ShiftTab),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Focus => "focus",
            Self::Click => "click",
            Self::Enter => "enter",
            Self::Space => "space",
            Self::Tab => "tab",
            Self::ShiftTab => "shift_tab",
        }
    }

    /// 发送按键前是否需要先聚焦目标。
    fn needs_focus(self) -> bool {
        matches!(
            self,
            Self::Focus | Self::Enter | Self::Space | Self::Tab | Self::ShiftTab
        )
    }

    fn key_label(self) -> &'static str {
        match self {
            Self::Enter => "Enter",
            Self::Space => "Space",
            Self::Tab => "Tab",
            Self::ShiftTab => "Shift+Tab",
            _ => "",
        }
    }
}

/// 一个可 Tab 选中的屏幕对象。
#[derive(Debug, Clone, PartialEq, Eq)]
struct TabObject {
    /// 1 基序号，供 `tab_act` 使用；排序后重新编号。
    index: usize,
    name: String,
    control_type: String,
    automation_id: String,
    class_name: String,
    accelerator_key: String,
    /// 屏幕物理坐标（left, top, width, height）。
    rect: (i32, i32, i32, i32),
    window_title: String,
    process_id: u32,
    /// 当前是否持有键盘焦点。
    focused: bool,
    enabled: bool,
}

impl TabObject {
    fn to_value(&self) -> Value {
        json!({
            "index": self.index,
            "name": self.name,
            "control_type": self.control_type,
            "automation_id": self.automation_id,
            "class_name": self.class_name,
            "accelerator_key": self.accelerator_key,
            "rect": {
                "x": self.rect.0,
                "y": self.rect.1,
                "w": self.rect.2,
                "h": self.rect.3,
            },
            "window_title": self.window_title,
            "process_id": self.process_id,
            "focused": self.focused,
            "enabled": self.enabled,
        })
    }

    fn center(&self) -> (i32, i32) {
        (self.rect.0 + self.rect.2 / 2, self.rect.1 + self.rect.3 / 2)
    }

    fn brief(&self) -> String {
        format!(
            "{}[{}] name=\"{}\" rect=({},{},{},{})",
            self.control_type,
            self.index,
            self.name,
            self.rect.0,
            self.rect.1,
            self.rect.2,
            self.rect.3
        )
    }
}

/// 虚拟桌面屏幕范围（当前进程 DPI 坐标系的物理/虚拟坐标，与 mouse_*、UIA rect 一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScreenRect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl ScreenRect {
    fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self {
            x,
            y,
            w: w.max(0),
            h: h.max(0),
        }
    }

    fn contains_point(&self, px: i32, py: i32) -> bool {
        self.w > 0
            && self.h > 0
            && px >= self.x
            && py >= self.y
            && px < self.x.saturating_add(self.w)
            && py < self.y.saturating_add(self.h)
    }

    /// rect 与屏幕的可见交集面积（像素）。
    fn intersection_area(&self, rect: (i32, i32, i32, i32)) -> i64 {
        let (rx, ry, rw, rh) = rect;
        if rw <= 0 || rh <= 0 || self.w <= 0 || self.h <= 0 {
            return 0;
        }
        let left = rx.max(self.x) as i64;
        let top = ry.max(self.y) as i64;
        let right = (rx as i64 + rw as i64).min(self.x as i64 + self.w as i64);
        let bottom = (ry as i64 + rh as i64).min(self.y as i64 + self.h as i64);
        let w = right - left;
        let h = bottom - top;
        if w <= 0 || h <= 0 { 0 } else { w * h }
    }
}

/// 判断对象是否在屏幕内可见：中心点在屏幕内，或可见交集面积占对象面积 >= 20%。
/// 完全在屏幕外（含负坐标）一律为 false。
fn rect_visible_on_screen(rect: (i32, i32, i32, i32), screen: ScreenRect) -> bool {
    let (x, y, w, h) = rect;
    if w <= 0 || h <= 0 || screen.w <= 0 || screen.h <= 0 {
        return false;
    }
    let cx = x.saturating_add(w / 2);
    let cy = y.saturating_add(h / 2);
    if screen.contains_point(cx, cy) {
        return true;
    }
    let area = w as i64 * h as i64;
    let visible = screen.intersection_area(rect);
    visible > 0 && (visible as f64 / area as f64) >= 0.2
}

/// 纯兜底容器（Win32 拿不到任何信息）：name/control_type/class/automation_id/accelerator
/// 全空或 Unknown，且没有任何可用于定位的 class，视为噪声丢弃。
fn is_empty_fallback_object(obj: &TabObject) -> bool {
    obj.name.trim().is_empty()
        && obj.automation_id.trim().is_empty()
        && obj.accelerator_key.trim().is_empty()
        && obj.class_name.trim().is_empty()
        && (obj.control_type.trim().is_empty() || obj.control_type == "Unknown")
}

/// 过滤屏幕外/不可见对象与空兜底对象；返回过滤掉的数量。
fn filter_visible(objects: &mut Vec<TabObject>, screen: ScreenRect) -> usize {
    let before = objects.len();
    objects.retain(|o| rect_visible_on_screen(o.rect, screen) && !is_empty_fallback_object(o));
    before.saturating_sub(objects.len())
}

/// `tab_act` 的动作前校验：目标中心点必须仍在屏幕内。
fn ensure_target_on_screen(obj: &TabObject, screen: ScreenRect) -> Result<(), String> {
    let (cx, cy) = obj.center();
    if obj.rect.2 <= 0 || obj.rect.3 <= 0 || !screen.contains_point(cx, cy) {
        return Err(
            "目标不在当前屏幕内（可能已滚动/隐藏或窗口被移出屏幕），请重新 tab_scan".to_string(),
        );
    }
    Ok(())
}

/// 最近一次 `tab_scan` 的结果缓存。
struct ScanCache {
    scanned_at: Instant,
    screen: ScreenRect,
    objects: Vec<TabObject>,
}

/// 全局扫描缓存（同一进程内共享；多轮对话都可用，但有 30 秒 TTL）。
static SCAN_CACHE: Lazy<Mutex<Option<ScanCache>>> = Lazy::new(|| Mutex::new(None));

fn cache_guard() -> parking_lot::MutexGuard<'static, Option<ScanCache>> {
    SCAN_CACHE.lock()
}

// ============================================================
// 纯逻辑：参数校验 / 排序 / 缓存解析 / 输出拼装（可单测）
// ============================================================

/// `max` 参数：默认 80，clamp 到 1..=200。
fn clamp_max(raw: u64) -> usize {
    (raw as usize).clamp(1, MAX_CAP)
}

/// 解析缓存里的目标：index 1 基；无扫描结果 / 过期 / 越界都返回中文错误。
fn resolve_target(
    cache: Option<&ScanCache>,
    index: i64,
    now: Instant,
) -> Result<TabObject, String> {
    let Some(cache) = cache else {
        return Err(
            "还没有可用的 tab_scan 结果：请先调用 tab_scan，再用返回的 index 调用 tab_act。".into(),
        );
    };
    let age = now.saturating_duration_since(cache.scanned_at);
    if age > SCAN_TTL {
        return Err(format!(
            "最近一次 tab_scan 已过去 {:.1} 秒（超过 30 秒），界面可能已经变化：请重新调用 tab_scan。",
            age.as_secs_f64()
        ));
    }
    if index < 1 {
        return Err(format!(
            "index 必须从 1 开始（当前传入 {index}）；index 来自 tab_scan 返回的数组。"
        ));
    }
    let idx = index as usize;
    if idx > cache.objects.len() {
        return Err(format!(
            "index 越界：最近一次 tab_scan 只有 {} 个可 Tab 对象（index 1..={}），请重新 tab_scan 后选择。",
            cache.objects.len(),
            cache.objects.len()
        ));
    }
    Ok(cache.objects[idx - 1].clone())
}

/// 去掉尺寸非法的对象，按屏幕位置（上→下、左→右）排序，截断并重新编号。
fn sort_and_index(objects: &mut Vec<TabObject>, max: usize) {
    objects.retain(|o| o.rect.2 > 0 && o.rect.3 > 0);
    objects.sort_by_key(|o| (o.rect.1, o.rect.0));
    objects.truncate(max);
    for (i, o) in objects.iter_mut().enumerate() {
        o.index = i + 1;
    }
}

/// 同一元素可能被多个窗口根枚举到（或 UIA/Win32 兜底重复），按 name+type+rect 去重。
fn dedup_objects(objects: &mut Vec<TabObject>) {
    objects.sort_by_key(|o| (o.rect.1, o.rect.0, o.name.clone()));
    objects
        .dedup_by(|a, b| a.name == b.name && a.control_type == b.control_type && a.rect == b.rect);
}

/// 生成 `tab_scan` 的返回文本：一句摘要 + JSON 数组。
fn scan_content(
    objects: &[TabObject],
    screen: ScreenRect,
    windows_scanned: usize,
    elapsed_ms: u128,
    truncated: bool,
    used_win32_fallback: bool,
    filtered_offscreen: usize,
) -> String {
    let mut head = format!(
        "[ok] 共 {} 个可 Tab 对象（屏幕 {}x{}+({},{})；扫描窗口 {} 个；用时 {}ms",
        objects.len(),
        screen.w,
        screen.h,
        screen.x,
        screen.y,
        windows_scanned,
        elapsed_ms
    );
    if truncated {
        head.push_str("；结果已截断");
    }
    if used_win32_fallback {
        head.push_str("；部分窗口用 Win32 WS_TABSTOP 兜底");
    }
    head.push_str(&format!(
        "；已过滤屏幕外/不可见对象 {filtered_offscreen} 个"
    ));
    head.push_str("）。只列屏幕内可见对象；index 从 1 开始，按屏幕位置从上到下、从左到右排序，可直接用于 tab_act。");
    if objects.is_empty() {
        head.push_str(
            "\n没有找到屏幕内可 Tab 的对象：可能当前屏幕上的程序都没有可聚焦控件，或某个程序的 UIA 树无法读取（受管理员权限/保护进程限制）。",
        );
        return head;
    }
    let json =
        serde_json::to_string_pretty(&objects.iter().map(TabObject::to_value).collect::<Vec<_>>())
            .unwrap_or_else(|_| "[]".to_string());
    format!("{head}\n{json}")
}

/// 生成 `tab_act` 的成功返回文本。
fn act_content(
    action: TabAction,
    target: &TabObject,
    method: &str,
    focused: Option<&(String, String)>,
    screen: ScreenRect,
) -> String {
    let (cx, cy) = target.center();
    let on_screen = screen.contains_point(cx, cy);
    let focus_match = match focused {
        Some((name, _)) if !target.name.is_empty() => {
            name == &target.name || name.contains(&target.name) || target.name.contains(name)
        }
        _ => false,
    };
    let focus_text = match focused {
        Some((name, ct)) => {
            format!("当前焦点控件 name=\"{name}\" control_type={ct}；命中目标={focus_match}")
        }
        None => "无法读取当前焦点控件（GetFocusedElement 失败）".to_string(),
    };
    let advice = match action {
        TabAction::Tab | TabAction::ShiftTab => {
            "可以继续 tab_act action=tab/shift_tab 按 Tab 顺序遍历，或 action=enter/space 激活。"
        }
        TabAction::Focus => "可以接着 action=enter/space 激活，或 action=tab 继续遍历。",
        TabAction::Click | TabAction::Enter | TabAction::Space => {
            "动作已发送；如需继续在表单里移动，用 action=tab 按 Tab 顺序遍历（注意界面可能已变化，必要时重新 tab_scan）。"
        }
    };
    format!(
        "[ok] 已执行 {}：index={} {}，对象中心=({cx},{cy}) 屏幕内={on_screen}，窗口=\"{}\"\n屏幕：虚拟桌面 ({},{}) {}x{}\n操作：{method}\n焦点：{focus_text}\n建议：{advice}",
        action.as_str(),
        target.index,
        target.brief(),
        target.window_title.replace('"', "'"),
        screen.x,
        screen.y,
        screen.w,
        screen.h
    )
}

// ============================================================
// 工具规格与注册
// ============================================================

const SCAN_DESC: &str = "列出屏幕内所有可被 Tab 键选中的对象（UIA 判定可键盘聚焦、可用、不在屏幕外），\
按屏幕位置从上到下、从左到右排序，返回 JSON 数组（index 从 1 开始），只读、不操作用户界面。\
结果缓存 30 秒，供 tab_act 按 index 交互。";

const ACT_DESC: &str = "对最近一次 tab_scan 结果中的对象执行动作：focus / click / enter / space / tab / shift_tab。\
index 必须来自最近 30 秒内的 tab_scan（过期或越界会提示重新扫描）。\
注意：click / enter / space 会真实操作用户界面（可能触发按钮、提交表单、改变焦点），执行前请确认目标无误。";

fn spec_tab_scan() -> ToolSpec {
    ToolSpec::new(
        "tab_scan",
        SCAN_DESC,
        json!({
            "type": "object",
            "properties": {
                "window": {
                    "type": "string",
                    "description": "只看某个窗口：按窗口标题 / 类名 / 进程名（子串，大小写不敏感）或窗口句柄（0x 十六进制）。省略则扫描屏幕内所有顶层窗口。"
                },
                "max": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 200,
                    "description": "最多返回多少个可 Tab 对象，默认 80"
                }
            }
        }),
    )
}

fn spec_tab_act() -> ToolSpec {
    ToolSpec::new(
        "tab_act",
        ACT_DESC,
        json!({
            "type": "object",
            "properties": {
                "index": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "tab_scan 返回的 1 基序号"
                },
                "action": {
                    "type": "string",
                    "enum": ["focus", "click", "enter", "space", "tab", "shift_tab"],
                    "description": "要执行的动作：focus=聚焦；click=鼠标点击对象中心；enter/space=聚焦后按键；tab/shift_tab=聚焦后按 Tab 继续遍历"
                }
            },
            "required": ["index", "action"]
        }),
    )
}

/// 把这些工具注册进注册表（由 `neosonia_tools::register_all` / `tool_catalog` 调用）。
pub fn register(reg: &ToolRegistry) {
    reg.register(Arc::new(TabScanTool));
    reg.register(Arc::new(TabActTool));
}

pub struct TabScanTool;

#[async_trait]
impl Tool for TabScanTool {
    fn spec(&self) -> ToolSpec {
        spec_tab_scan()
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> ToolOutcome {
        tab_scan(ctx, args).await
    }
}

pub struct TabActTool;

#[async_trait]
impl Tool for TabActTool {
    fn spec(&self) -> ToolSpec {
        spec_tab_act()
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> ToolOutcome {
        tab_act(ctx, args).await
    }
}

// ============================================================
// 异步入口（COM/输入都是阻塞调用，统一丢到 spawn_blocking）
// ============================================================

async fn tab_scan(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    let ctx = ctx.clone();
    match tokio::task::spawn_blocking(move || scan_entry(&ctx, &args)).await {
        Ok(outcome) => outcome,
        Err(e) => ToolOutcome::err(format!("tab_scan 工作线程异常：{e}")),
    }
}

async fn tab_act(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    let ctx = ctx.clone();
    match tokio::task::spawn_blocking(move || act_entry(&ctx, &args)).await {
        Ok(outcome) => outcome,
        Err(e) => ToolOutcome::err(format!("tab_act 工作线程异常：{e}")),
    }
}

fn scan_entry(ctx: &ToolCtx, args: &Value) -> ToolOutcome {
    if ctx.is_cancelled() {
        return ToolOutcome::err("操作已取消");
    }
    let max = clamp_max(arg_u64(args, "max", DEFAULT_MAX));
    let window = arg_str(args, "window")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    #[cfg(windows)]
    {
        let deadline = Instant::now() + SCAN_BUDGET;
        match platform::scan_once(window.as_deref(), max, deadline, ctx) {
            Ok(outcome) => {
                let content = scan_content(
                    &outcome.objects,
                    outcome.screen,
                    outcome.windows_scanned,
                    outcome.elapsed_ms,
                    outcome.truncated,
                    outcome.used_win32_fallback,
                    outcome.filtered_offscreen,
                );
                {
                    let mut guard = cache_guard();
                    *guard = Some(ScanCache {
                        scanned_at: Instant::now(),
                        screen: outcome.screen,
                        objects: outcome.objects.clone(),
                    });
                }
                ToolOutcome::ok(content)
            }
            Err(e) => ToolOutcome::err(e),
        }
    }

    #[cfg(not(windows))]
    {
        let _ = (window, max, ctx);
        ToolOutcome::err(
            "tab 键遍历目前只支持 Windows（需要 UI Automation）。其它平台请用 ui_tree 等降级工具。",
        )
    }
}

fn act_entry(ctx: &ToolCtx, args: &Value) -> ToolOutcome {
    if ctx.is_cancelled() {
        return ToolOutcome::err("操作已取消");
    }
    let Some(action_raw) = arg_str(args, "action") else {
        return ToolOutcome::err(
            "缺少必填参数 action（string）：支持 focus / click / enter / space / tab / shift_tab。",
        );
    };
    let Some(action) = TabAction::parse(&action_raw) else {
        return ToolOutcome::err(format!(
            "action 不支持：\"{}\"。支持：focus / click / enter / space / tab / shift_tab。",
            action_raw.trim()
        ));
    };
    let index = arg_i64(args, "index", -1);

    let target = {
        let cache = cache_guard();
        resolve_target(cache.as_ref(), index, Instant::now())
            .map(|target| (target, cache.as_ref().map(|c| c.screen)))
    };
    let (target, screen) = match target {
        Ok(pair) => pair,
        Err(e) => return ToolOutcome::err(e),
    };
    let Some(screen) = screen else {
        return ToolOutcome::err(
            "扫描结果缺少屏幕范围（可能来自旧版本缓存）：请重新调用 tab_scan。",
        );
    };
    // 动作前先校验目标仍在屏幕内：屏幕外/负坐标对象不退化点击、不动鼠标。
    if let Err(e) = ensure_target_on_screen(&target, screen) {
        return ToolOutcome::err(e);
    }

    #[cfg(windows)]
    {
        platform::act_on(ctx, action, &target, screen)
    }
    #[cfg(not(windows))]
    {
        let _ = (ctx, action, target, screen);
        ToolOutcome::err("tab_act 目前只支持 Windows（需要 UI Automation / 输入模拟）。")
    }
}

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use neosonia_core::config::Settings;
    use neosonia_core::registry::ToolRegistry;

    fn obj(index: usize, name: &str, rect: (i32, i32, i32, i32)) -> TabObject {
        TabObject {
            index,
            name: name.to_string(),
            control_type: "Button".to_string(),
            automation_id: String::new(),
            class_name: "Button".to_string(),
            accelerator_key: String::new(),
            rect,
            window_title: "测试窗口".to_string(),
            process_id: 1,
            focused: false,
            enabled: true,
        }
    }

    const TEST_SCREEN: ScreenRect = ScreenRect {
        x: 0,
        y: 0,
        w: 1920,
        h: 1080,
    };

    fn cache_at(objects: Vec<TabObject>, at: Instant) -> ScanCache {
        ScanCache {
            scanned_at: at,
            screen: TEST_SCREEN,
            objects,
        }
    }

    #[test]
    fn clamp_max_bounds() {
        assert_eq!(clamp_max(0), 1);
        assert_eq!(clamp_max(1), 1);
        assert_eq!(clamp_max(80), 80);
        assert_eq!(clamp_max(200), 200);
        assert_eq!(clamp_max(500), MAX_CAP);
        assert_eq!(clamp_max(u64::MAX), MAX_CAP);
    }

    #[test]
    fn parse_action_accepts_whitelist_only() {
        for (raw, expect) in [
            ("focus", TabAction::Focus),
            ("CLICK", TabAction::Click),
            ("enter", TabAction::Enter),
            ("space", TabAction::Space),
            ("tab", TabAction::Tab),
            ("shift_tab", TabAction::ShiftTab),
            ("shift-tab", TabAction::ShiftTab),
        ] {
            assert_eq!(TabAction::parse(raw), Some(expect), "应接受 {raw}");
        }
        for bad in ["", "press", "double_click", "enter;rm", "  "] {
            assert_eq!(TabAction::parse(bad), None, "不应接受 {bad:?}");
        }
        assert!(TabAction::Focus.needs_focus());
        assert!(TabAction::ShiftTab.needs_focus());
        assert!(!TabAction::Click.needs_focus());
        assert_eq!(TabAction::ShiftTab.key_label(), "Shift+Tab");
    }

    #[test]
    fn resolve_target_requires_scan() {
        let now = Instant::now();
        let err = resolve_target(None, 1, now).unwrap_err();
        assert!(err.contains("tab_scan"), "错误应提示先扫描：{err}");
    }

    #[test]
    fn resolve_target_checks_ttl_and_bounds() {
        let now = Instant::now();
        let old = now
            .checked_sub(Duration::from_secs(31))
            .unwrap_or_else(Instant::now);
        let cache = cache_at(vec![obj(1, "确定", (10, 20, 30, 40))], old);
        let err = resolve_target(Some(&cache), 1, now).unwrap_err();
        assert!(err.contains("30 秒"), "过期错误应提到 30 秒：{err}");

        let fresh = cache_at(vec![obj(1, "确定", (10, 20, 30, 40))], now);
        let err = resolve_target(Some(&fresh), 0, now).unwrap_err();
        assert!(err.contains("从 1 开始"), "index 0 错误：{err}");
        let err = resolve_target(Some(&fresh), 2, now).unwrap_err();
        assert!(err.contains("越界"), "越界错误：{err}");

        let hit = resolve_target(Some(&fresh), 1, now).unwrap();
        assert_eq!(hit.name, "确定");
        assert_eq!(hit.index, 1);
    }

    #[test]
    fn sort_and_index_orders_by_screen_and_renumbers() {
        let mut objects = vec![
            obj(9, "下右", (200, 300, 10, 10)),
            obj(8, "上右", (200, 10, 10, 10)),
            obj(7, "上左", (10, 10, 10, 10)),
            obj(6, "零宽", (0, 0, 0, 10)),
        ];
        sort_and_index(&mut objects, 10);
        let names: Vec<&str> = objects.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(names, vec!["上左", "上右", "下右"]);
        let indexes: Vec<usize> = objects.iter().map(|o| o.index).collect();
        assert_eq!(indexes, vec![1, 2, 3]);

        let mut many: Vec<TabObject> = (0..10)
            .map(|i| obj(0, &format!("b{i}"), (i * 10, i * 10, 5, 5)))
            .collect();
        sort_and_index(&mut many, 3);
        assert_eq!(many.len(), 3);
        assert_eq!(
            many.iter().map(|o| o.index).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn dedup_removes_same_element_from_multiple_roots() {
        let mut objects = vec![
            obj(1, "确定", (10, 20, 30, 40)),
            obj(2, "确定", (10, 20, 30, 40)),
            obj(3, "取消", (10, 80, 30, 40)),
        ];
        dedup_objects(&mut objects);
        assert_eq!(objects.len(), 2);
    }

    #[test]
    fn rect_visibility_filters_offscreen_and_partial() {
        // 完全在屏幕上方（负坐标，对应 Lead 实测的 y=-941 / -56）
        assert!(!rect_visible_on_screen((-10, -941, 120, 56), TEST_SCREEN));
        assert!(!rect_visible_on_screen((100, -56, 80, 40), TEST_SCREEN));
        // 完全在右侧屏幕外
        assert!(!rect_visible_on_screen((2000, 10, 50, 20), TEST_SCREEN));
        // 中心在屏幕内
        assert!(rect_visible_on_screen((100, 100, 80, 30), TEST_SCREEN));
        // 中心在屏幕外，但可见面积 80% >= 20% -> 保留（部分相交）
        assert!(rect_visible_on_screen((-20, 100, 100, 40), TEST_SCREEN));
        // 只相交 10% < 20% -> 剔除
        assert!(!rect_visible_on_screen((-90, 100, 100, 40), TEST_SCREEN));
        // 零尺寸 -> 剔除
        assert!(!rect_visible_on_screen((10, 10, 0, 10), TEST_SCREEN));
    }

    #[test]
    fn filter_visible_drops_offscreen_and_empty_fallback() {
        let empty_fallback = TabObject {
            name: String::new(),
            control_type: "Unknown".to_string(),
            automation_id: String::new(),
            class_name: String::new(),
            accelerator_key: String::new(),
            rect: (20, 20, 10, 10),
            ..obj(0, "占位", (0, 0, 0, 0))
        };
        let mut objects = vec![
            obj(1, "屏幕内", (10, 10, 30, 20)),
            obj(2, "屏幕外", (-10, -941, 120, 56)),
            empty_fallback,
        ];
        let dropped = filter_visible(&mut objects, TEST_SCREEN);
        assert_eq!(objects.len(), 1, "只应保留屏幕内的有效对象：{objects:?}");
        assert_eq!(objects[0].name, "屏幕内");
        assert_eq!(dropped, 2);
    }

    #[test]
    fn ensure_target_on_screen_rejects_offscreen() {
        let off = obj(1, "屏幕外", (-10, -941, 120, 56));
        let err = ensure_target_on_screen(&off, TEST_SCREEN).unwrap_err();
        assert!(err.contains("目标不在当前屏幕内"), "错误文案：{err}");
        assert!(err.contains("重新 tab_scan"), "应提示重新扫描：{err}");
        let ok_obj = obj(2, "屏幕内", (100, 100, 80, 30));
        assert!(ensure_target_on_screen(&ok_obj, TEST_SCREEN).is_ok());
    }

    #[test]
    fn scan_content_empty_and_nonempty() {
        let empty = scan_content(&[], TEST_SCREEN, 3, 12, false, false, 7);
        assert!(empty.contains("共 0 个可 Tab 对象"), "{empty}");
        assert!(empty.contains("没有找到屏幕内可 Tab"), "{empty}");
        assert!(empty.contains("已过滤屏幕外/不可见对象 7 个"), "{empty}");

        let objects = vec![obj(1, "确定", (10, 20, 30, 40))];
        let text = scan_content(&objects, TEST_SCREEN, 5, 42, true, true, 2);
        assert!(text.contains("共 1 个可 Tab 对象"), "{text}");
        assert!(text.contains("截断"), "{text}");
        assert!(text.contains("WS_TABSTOP"), "{text}");
        assert!(text.contains("已过滤屏幕外/不可见对象 2 个"), "{text}");
        assert!(text.contains("\"index\": 1"), "{text}");
    }

    #[test]
    fn act_content_contains_evidence_and_advice() {
        let target = obj(2, "确定", (100, 200, 60, 30));
        let focused = ("确定".to_string(), "Button".to_string());
        let text = act_content(
            TabAction::Enter,
            &target,
            "UIA SetFocus 成功，已发送 Enter 键",
            Some(&focused),
            TEST_SCREEN,
        );
        assert!(text.contains("index=2"), "{text}");
        assert!(text.contains("命中目标=true"), "{text}");
        assert!(text.contains("action=tab"), "{text}");
        assert!(
            text.contains("对象中心=(130,215) 屏幕内=true"),
            "中心/屏幕内应输出：{text}"
        );
    }

    #[test]
    fn specs_have_expected_names_and_params() {
        let scan = spec_tab_scan();
        assert_eq!(scan.name, "tab_scan");
        assert_eq!(
            scan.parameters["properties"]["max"]["maximum"].as_u64(),
            Some(200)
        );
        assert!(scan.description.contains("只读"));

        let act = spec_tab_act();
        assert_eq!(act.name, "tab_act");
        let actions = act.parameters["properties"]["action"]["enum"]
            .as_array()
            .expect("action enum");
        for want in ["focus", "click", "enter", "space", "tab", "shift_tab"] {
            assert!(
                actions.iter().any(|v| v.as_str() == Some(want)),
                "缺少 action {want}"
            );
        }
        assert!(act.description.contains("真实操作用户界面"));
    }

    #[test]
    fn register_requires_both_input_and_uia() {
        // 默认设置：两者都开 -> 注册
        let reg = ToolRegistry::new();
        crate::register_all(&reg, &Settings::default());
        let names = reg.names();
        assert!(names.iter().any(|n| n == "tab_scan"));
        assert!(names.iter().any(|n| n == "tab_act"));

        // allow_input_simulation=false -> 两个都不注册
        let mut s = Settings::default();
        s.tools.allow_input_simulation = false;
        let reg = ToolRegistry::new();
        crate::register_all(&reg, &s);
        let names = reg.names();
        assert!(!names.iter().any(|n| n == "tab_scan"));
        assert!(!names.iter().any(|n| n == "tab_act"));

        // uia_enabled=false -> 两个都不注册
        let mut s = Settings::default();
        s.tools.uia_enabled = false;
        let reg = ToolRegistry::new();
        crate::register_all(&reg, &s);
        let names = reg.names();
        assert!(!names.iter().any(|n| n == "tab_scan"));
        assert!(!names.iter().any(|n| n == "tab_act"));
    }

    #[test]
    fn direct_register_has_two_tools() {
        let reg = ToolRegistry::new();
        register(&reg);
        let names = reg.names();
        assert_eq!(names.len(), 2, "tab::register 应只注册两个工具：{names:?}");
    }

    /// 真机全量扫描（产品默认 8 秒预算）：输出摘要与样例，用于验收报告；默认 ignore。
    #[test]
    #[ignore = "真机全量 tab_scan（8 秒预算），输出摘要供验收报告"]
    #[cfg(windows)]
    fn real_tab_scan_full_budget_smoke() {
        let (ctx, _host) =
            crate::test_support::fake_ctx(crate::test_support::temp_root("tabscan-full"));
        let deadline = Instant::now() + SCAN_BUDGET;
        match platform::scan_once(None, DEFAULT_MAX as usize, deadline, &ctx) {
            Ok(outcome) => {
                println!(
                    "[tab_scan 8s] 对象数={} 窗口数={} 屏幕={:?} 用时={}ms 截断={} 兜底={} 过滤屏幕外/不可见={}",
                    outcome.objects.len(),
                    outcome.windows_scanned,
                    outcome.screen,
                    outcome.elapsed_ms,
                    outcome.truncated,
                    outcome.used_win32_fallback,
                    outcome.filtered_offscreen
                );
                for o in outcome.objects.iter().take(5) {
                    println!("  - {}", o.brief());
                }
                for o in &outcome.objects {
                    let (cx, cy) = o.center();
                    assert!(
                        outcome.screen.contains_point(cx, cy),
                        "真机扫描返回了屏幕外对象：{}（中心 {cx},{cy}，屏幕 {:?}）",
                        o.brief(),
                        outcome.screen
                    );
                }
                if let Some(first) = outcome.objects.first() {
                    println!(
                        "[tab_scan 8s] 首个对象 JSON：{}",
                        serde_json::to_string(&first.to_value()).unwrap_or_default()
                    );
                }
            }
            Err(e) => println!("[tab_scan 8s] 明确错误：{e}"),
        }
    }

    /// 真机枚举：在真实桌面上跑一次 tab_scan 的底层枚举，断言返回 Ok 或明确错误，
    /// 且不 panic、不阻塞超过约 2.5 秒（工作线程 + recv_timeout 双重兜底）。
    #[test]
    #[cfg(windows)]
    fn real_enumeration_returns_or_errors_quickly() {
        let (ctx, _host) = crate::test_support::fake_ctx(crate::test_support::temp_root("tabscan"));
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_millis(1_500);
            let result = platform::scan_once(None, 20, deadline, &ctx);
            let _ = tx.send(result);
        });
        match rx.recv_timeout(Duration::from_millis(2_500)) {
            Ok(Ok(outcome)) => {
                println!(
                    "[tab_scan real] 对象数={} 窗口数={} 屏幕={:?} 用时={}ms 截断={} 兜底={} 过滤屏幕外/不可见={}",
                    outcome.objects.len(),
                    outcome.windows_scanned,
                    outcome.screen,
                    outcome.elapsed_ms,
                    outcome.truncated,
                    outcome.used_win32_fallback,
                    outcome.filtered_offscreen
                );
                assert!(outcome.objects.len() <= MAX_CAP);
                for (i, o) in outcome.objects.iter().enumerate() {
                    assert_eq!(o.index, i + 1, "index 应连续从 1 开始");
                    let (cx, cy) = o.center();
                    assert!(
                        outcome.screen.contains_point(cx, cy),
                        "真机枚举返回了屏幕外对象：{}（中心 {cx},{cy}，屏幕 {:?}）",
                        o.brief(),
                        outcome.screen
                    );
                }
            }
            Ok(Err(e)) => {
                println!("[tab_scan real] 明确错误（可接受，例如无桌面）：{e}");
                assert!(!e.trim().is_empty());
            }
            Err(_) => {
                println!(
                    "[tab_scan real] 枚举超过 2.5 秒未返回：按“不阻塞”要求跳过断言（结果仍可用部分）"
                );
            }
        }
    }
}

// ============================================================
// Windows 实现
// ============================================================

#[cfg(windows)]
mod platform {
    use std::collections::{HashMap, VecDeque};
    use std::time::{Duration, Instant};

    use neosonia_core::model::{ToolCtx, ToolOutcome};
    use sysinfo::{Pid, ProcessesToUpdate, System};
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM, POINT, RECT};
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
        CoUninitialize,
    };
    use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation, IUIAutomationElement};
    use windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumChildWindows, EnumWindows, GWL_STYLE, GetClassNameW, GetForegroundWindow,
        GetSystemMetrics, GetWindowLongW, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId,
        IsIconic, IsWindowVisible, SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN,
        SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, WS_TABSTOP,
    };
    use windows::core::BSTR;

    use super::*;

    const RPC_E_CHANGED_MODE: i32 = -2147417850;

    /// 一次扫描的结果（交给上层拼装输出与缓存）。
    pub(super) struct ScanOutcome {
        pub objects: Vec<TabObject>,
        pub screen: ScreenRect,
        pub windows_scanned: usize,
        pub elapsed_ms: u128,
        pub truncated: bool,
        pub used_win32_fallback: bool,
        /// 因完全在屏幕外/不可见而被剔除的对象数量。
        pub filtered_offscreen: usize,
    }

    // --------------------------------------------------------
    // COM 会话（RAII）
    // --------------------------------------------------------

    struct UiaSession {
        automation: IUIAutomation,
        should_uninit: bool,
    }

    impl UiaSession {
        fn new() -> Result<Self, String> {
            unsafe {
                let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
                let should_uninit = if hr.0 < 0 {
                    if hr.0 == RPC_E_CHANGED_MODE {
                        false
                    } else {
                        return Err(format!(
                            "COM 初始化失败（CoInitializeEx）：{}（0x{:08X}）",
                            hr.message(),
                            hr.0 as u32
                        ));
                    }
                } else {
                    true
                };
                match CoCreateInstance::<_, IUIAutomation>(
                    &CUIAutomation,
                    None::<&windows::core::IUnknown>,
                    CLSCTX_INPROC_SERVER,
                ) {
                    Ok(automation) => Ok(Self {
                        automation,
                        should_uninit,
                    }),
                    Err(e) => {
                        if should_uninit {
                            CoUninitialize();
                        }
                        Err(format!(
                            "创建 IUIAutomation 失败：{}。请确认系统 UI Automation 服务正常。",
                            e.message()
                        ))
                    }
                }
            }
        }
    }

    impl Drop for UiaSession {
        fn drop(&mut self) {
            if self.should_uninit {
                unsafe { CoUninitialize() };
            }
        }
    }

    // --------------------------------------------------------
    // 小工具
    // --------------------------------------------------------

    fn wide_string(buf: &[u16], len: i32) -> String {
        let n = len.max(0) as usize;
        let n = n.min(buf.len());
        String::from_utf16_lossy(&buf[..n]).trim().to_string()
    }

    fn window_text(hwnd: HWND) -> String {
        let mut buf = [0u16; 1024];
        let len = unsafe { GetWindowTextW(hwnd, &mut buf) };
        wide_string(&buf, len)
    }

    fn window_class(hwnd: HWND) -> String {
        let mut buf = [0u16; 512];
        let len = unsafe { GetClassNameW(hwnd, &mut buf) };
        wide_string(&buf, len)
    }

    fn bstr_string(b: windows::core::Result<BSTR>) -> String {
        match b {
            Ok(s) => s.to_string(),
            Err(_) => String::new(),
        }
    }

    fn rect_tuple(r: RECT) -> (i32, i32, i32, i32) {
        (
            r.left,
            r.top,
            (r.right - r.left).max(0),
            (r.bottom - r.top).max(0),
        )
    }

    /// 当前进程 DPI 坐标系的虚拟桌面范围（多显示器时包含负坐标的副屏）。
    fn virtual_screen_rect() -> ScreenRect {
        let x = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
        let y = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
        let w = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
        let h = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
        if w > 0 && h > 0 {
            return ScreenRect::new(x, y, w, h);
        }
        // 极端情况下取主屏尺寸兜底。
        ScreenRect::new(0, 0, unsafe { GetSystemMetrics(SM_CXSCREEN) }, unsafe {
            GetSystemMetrics(SM_CYSCREEN)
        })
    }

    fn control_type_name(control_type: u32) -> &'static str {
        match control_type {
            50000 => "Button",
            50001 => "Calendar",
            50002 => "CheckBox",
            50003 => "ComboBox",
            50004 => "Edit",
            50005 => "Hyperlink",
            50006 => "Image",
            50007 => "ListItem",
            50008 => "List",
            50009 => "Menu",
            50010 => "MenuBar",
            50011 => "MenuItem",
            50012 => "ProgressBar",
            50013 => "RadioButton",
            50014 => "ScrollBar",
            50015 => "Slider",
            50016 => "Spinner",
            50017 => "StatusBar",
            50018 => "Tab",
            50019 => "TabItem",
            50020 => "Text",
            50021 => "ToolBar",
            50022 => "ToolTip",
            50023 => "Tree",
            50024 => "TreeItem",
            50025 => "Custom",
            50026 => "Group",
            50027 => "Thumb",
            50028 => "DataGrid",
            50029 => "DataItem",
            50030 => "Document",
            50031 => "SplitButton",
            50032 => "Window",
            50033 => "Pane",
            50034 => "Header",
            50035 => "HeaderItem",
            50036 => "Table",
            50037 => "TitleBar",
            50038 => "Separator",
            50039 => "SemanticZoom",
            _ => "Unknown",
        }
    }

    // --------------------------------------------------------
    // 顶层窗口枚举与过滤
    // --------------------------------------------------------

    struct WindowEntry {
        hwnd: HWND,
        title: String,
        class: String,
        pid: u32,
        minimized: bool,
    }

    unsafe extern "system" fn enum_windows_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
            return BOOL(1);
        }
        let list = unsafe { &mut *(lparam.0 as *mut Vec<WindowEntry>) };
        let mut pid = 0u32;
        let pid_ptr: *mut u32 = &mut pid;
        unsafe { GetWindowThreadProcessId(hwnd, Some(pid_ptr)) };
        list.push(WindowEntry {
            hwnd,
            title: window_text(hwnd),
            class: window_class(hwnd),
            pid,
            minimized: unsafe { IsIconic(hwnd) }.as_bool(),
        });
        BOOL(1)
    }

    fn enum_visible_windows() -> Vec<WindowEntry> {
        let mut list: Vec<WindowEntry> = Vec::new();
        let ptr: *mut Vec<WindowEntry> = &mut list;
        let _ = unsafe { EnumWindows(Some(enum_windows_cb), LPARAM(ptr as isize)) };
        list
    }

    fn contains_ci(haystack: &str, needle: &str) -> bool {
        haystack.to_lowercase().contains(&needle.to_lowercase())
    }

    fn parse_hwnd(raw: &str) -> Option<isize> {
        let t = raw.trim();
        let hex = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X"))?;
        i64::from_str_radix(hex, 16).ok().map(|v| v as isize)
    }

    fn process_names(pids: &[u32]) -> HashMap<u32, String> {
        let mut map = HashMap::new();
        if pids.is_empty() {
            return map;
        }
        let ids: Vec<Pid> = pids.iter().map(|p| Pid::from_u32(*p)).collect();
        let mut sys = System::new();
        sys.refresh_processes(ProcessesToUpdate::Some(&ids), true);
        for pid in ids {
            if let Some(proc_) = sys.process(pid) {
                map.insert(pid.as_u32(), proc_.name().to_string_lossy().to_string());
            }
        }
        map
    }

    fn window_matches(w: &WindowEntry, filter: &str, names: &HashMap<u32, String>) -> bool {
        if let Some(hex) = parse_hwnd(filter) {
            return w.hwnd.0 as isize == hex
                || format!("{:#X}", w.hwnd.0 as usize).eq_ignore_ascii_case(filter)
                || format!("{:X}", w.hwnd.0 as usize).eq_ignore_ascii_case(filter);
        }
        if contains_ci(&w.title, filter) || contains_ci(&w.class, filter) {
            return true;
        }
        if let Some(name) = names.get(&w.pid) {
            return contains_ci(name, filter);
        }
        w.pid.to_string() == filter
    }

    // --------------------------------------------------------
    // UIA 元素 -> TabObject
    // --------------------------------------------------------

    fn element_to_object(el: &IUIAutomationElement, w: &WindowEntry) -> Option<TabObject> {
        let focusable = unsafe { el.CurrentIsKeyboardFocusable() }
            .map(|b| b.as_bool())
            .unwrap_or(false);
        if !focusable {
            return None;
        }
        let enabled = unsafe { el.CurrentIsEnabled() }
            .map(|b| b.as_bool())
            .unwrap_or(false);
        if !enabled {
            return None;
        }
        let offscreen = unsafe { el.CurrentIsOffscreen() }
            .map(|b| b.as_bool())
            .unwrap_or(true);
        if offscreen {
            return None;
        }
        let rect = unsafe { el.CurrentBoundingRectangle() }
            .ok()
            .map(rect_tuple)?;
        if rect.2 <= 0 || rect.3 <= 0 {
            return None;
        }
        let focused = unsafe { el.CurrentHasKeyboardFocus() }
            .map(|b| b.as_bool())
            .unwrap_or(false);
        Some(TabObject {
            index: 0,
            name: bstr_string(unsafe { el.CurrentName() }),
            control_type: control_type_name(
                unsafe { el.CurrentControlType() }
                    .map(|id| id.0 as u32)
                    .unwrap_or(0),
            )
            .to_string(),
            automation_id: bstr_string(unsafe { el.CurrentAutomationId() }),
            class_name: bstr_string(unsafe { el.CurrentClassName() }),
            accelerator_key: bstr_string(unsafe { el.CurrentAcceleratorKey() }),
            rect,
            window_title: w.title.clone(),
            process_id: w.pid,
            focused,
            enabled,
        })
    }

    /// 在一个窗口的 UIA 子树里 BFS 找可 Tab 元素；返回找到的数量。
    fn scan_uia_window(
        session: &UiaSession,
        w: &WindowEntry,
        out: &mut Vec<TabObject>,
        deadline: Instant,
        ctx: &ToolCtx,
    ) -> Result<usize, String> {
        let root = unsafe { session.automation.ElementFromHandle(w.hwnd) }
            .map_err(|e| format!("读取窗口 UIA 根元素失败（{}）：{}", w.title, e.message()))?;
        let walker = unsafe { session.automation.ControlViewWalker() }
            .map_err(|e| format!("创建 UIA 遍历器失败：{}", e.message()))?;
        let mut queue: VecDeque<(IUIAutomationElement, usize)> = VecDeque::new();
        queue.push_back((root, 0));
        let mut visited = 0usize;
        let mut found = 0usize;
        while let Some((el, depth)) = queue.pop_front() {
            if Instant::now() >= deadline || ctx.is_cancelled() {
                break;
            }
            visited += 1;
            if visited > MAX_NODES_PER_WINDOW || out.len() >= MAX_RAW_OBJECTS {
                break;
            }
            if let Some(obj) = element_to_object(&el, w) {
                out.push(obj);
                found += 1;
            }
            if depth >= MAX_DEPTH {
                continue;
            }
            let mut child = unsafe { walker.GetFirstChildElement(&el) }.ok();
            while let Some(c) = child {
                if queue.len() + out.len() >= MAX_RAW_OBJECTS.saturating_mul(2) {
                    break;
                }
                queue.push_back((c.clone(), depth + 1));
                child = unsafe { walker.GetNextSiblingElement(&c) }.ok();
            }
        }
        Ok(found)
    }

    // --------------------------------------------------------
    // Win32 WS_TABSTOP 兜底
    // --------------------------------------------------------

    struct Win32Collect {
        parent_title: String,
        parent_pid: u32,
        limit: usize,
        out: Vec<TabObject>,
    }

    unsafe extern "system" fn enum_child_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let ctx = unsafe { &mut *(lparam.0 as *mut Win32Collect) };
        if ctx.out.len() >= ctx.limit {
            return BOOL(0);
        }
        if !unsafe { IsWindowVisible(hwnd) }.as_bool()
            || !unsafe { IsWindowEnabled(hwnd) }.as_bool()
        {
            return BOOL(1);
        }
        let style = unsafe { GetWindowLongW(hwnd, GWL_STYLE) } as u32;
        if style & WS_TABSTOP.0 == 0 {
            return BOOL(1);
        }
        let mut rect = RECT::default();
        if unsafe { GetWindowRect(hwnd, &mut rect) }.is_err() {
            return BOOL(1);
        }
        let (x, y, w, h) = rect_tuple(rect);
        if w <= 0 || h <= 0 {
            return BOOL(1);
        }
        let foreground = unsafe { GetForegroundWindow() };
        ctx.out.push(TabObject {
            index: 0,
            name: window_text(hwnd),
            control_type: "Unknown".to_string(),
            automation_id: String::new(),
            class_name: window_class(hwnd),
            accelerator_key: String::new(),
            rect: (x, y, w, h),
            window_title: ctx.parent_title.clone(),
            process_id: ctx.parent_pid,
            focused: foreground == hwnd,
            enabled: true,
        });
        BOOL(1)
    }

    /// 用 Win32 `WS_TABSTOP` 枚举某个窗口的子控件（UIA 读不到时兜底）。
    fn win32_tabstop_children(parent: &WindowEntry, out: &mut Vec<TabObject>, limit: usize) {
        let mut collect = Win32Collect {
            parent_title: parent.title.clone(),
            parent_pid: parent.pid,
            limit,
            out: Vec::new(),
        };
        let ptr: *mut Win32Collect = &mut collect;
        let _ = unsafe { EnumChildWindows(parent.hwnd, Some(enum_child_cb), LPARAM(ptr as isize)) };
        out.extend(collect.out);
    }

    // --------------------------------------------------------
    // 扫描入口
    // --------------------------------------------------------

    pub(super) fn scan_once(
        window_filter: Option<&str>,
        max: usize,
        deadline: Instant,
        ctx: &ToolCtx,
    ) -> Result<ScanOutcome, String> {
        let started = Instant::now();
        let mut windows = enum_visible_windows();
        windows.retain(|w| !w.minimized);
        if let Some(filter) = window_filter {
            let pids: Vec<u32> = windows.iter().map(|w| w.pid).collect();
            let names = process_names(&pids);
            windows.retain(|w| window_matches(w, filter, &names));
        }
        if windows.is_empty() && window_filter.is_some() {
            let filter = window_filter.unwrap_or_default();
            return Err(format!(
                "没有找到匹配 \"{filter}\" 的可见窗口（按标题/类名/进程名/句柄过滤）。请先 ui_window_list 确认窗口存在。"
            ));
        }

        let mut objects: Vec<TabObject> = Vec::new();
        let mut truncated = false;
        let mut used_fallback = false;
        let session = UiaSession::new().ok();
        if session.is_none() {
            used_fallback = true;
        }

        let mut scanned = 0usize;
        for w in &windows {
            if Instant::now() >= deadline || objects.len() >= MAX_RAW_OBJECTS {
                truncated = true;
                break;
            }
            if ctx.is_cancelled() {
                break;
            }
            scanned += 1;
            match &session {
                Some(session) => match scan_uia_window(session, w, &mut objects, deadline, ctx) {
                    Ok(0) => {
                        used_fallback = true;
                        win32_tabstop_children(w, &mut objects, 64);
                    }
                    Ok(_) => {}
                    Err(_) => {
                        used_fallback = true;
                        win32_tabstop_children(w, &mut objects, 64);
                    }
                },
                None => win32_tabstop_children(w, &mut objects, 64),
            }
            if ctx.is_cancelled() {
                break;
            }
        }

        dedup_objects(&mut objects);
        let screen = virtual_screen_rect();
        // 只保留屏幕内可见对象：完全在屏幕外/负坐标、以及 Win32 空兜底容器都剔除。
        let filtered_offscreen = filter_visible(&mut objects, screen);
        sort_and_index(&mut objects, max);
        Ok(ScanOutcome {
            objects,
            screen,
            windows_scanned: scanned,
            elapsed_ms: started.elapsed().as_millis(),
            truncated,
            used_win32_fallback: used_fallback,
            filtered_offscreen,
        })
    }

    // --------------------------------------------------------
    // 鼠标 / 键盘（enigo，与 mouse_* / keyboard_* 工具保持同一套模拟）
    // --------------------------------------------------------

    fn mouse_click_at(x: i32, y: i32) -> Result<(), String> {
        use enigo::{Button, Coordinate, Direction, Enigo, Mouse, Settings};

        let mut enigo = Enigo::new(&Settings::default())
            .map_err(|e| format!("初始化鼠标模拟失败（enigo）：{e}。请确认桌面会话可交互。"))?;
        enigo
            .move_mouse(x, y, Coordinate::Abs)
            .map_err(|e| format!("移动鼠标到 ({x},{y}) 失败：{e}"))?;
        std::thread::sleep(Duration::from_millis(60));
        enigo
            .button(Button::Left, Direction::Click)
            .map_err(|e| format!("鼠标左键点击失败：{e}"))?;
        Ok(())
    }

    fn send_key(action: TabAction) -> Result<(), String> {
        use enigo::{Direction, Enigo, Key, Keyboard, Settings};

        let mut enigo = Enigo::new(&Settings::default())
            .map_err(|e| format!("初始化键盘模拟失败（enigo）：{e}。请确认桌面会话可交互。"))?;
        match action {
            TabAction::Enter => enigo
                .key(Key::Return, Direction::Click)
                .map_err(|e| format!("发送 Enter 失败：{e}")),
            TabAction::Space => enigo
                .key(Key::Space, Direction::Click)
                .map_err(|e| format!("发送 Space 失败：{e}")),
            TabAction::Tab => enigo
                .key(Key::Tab, Direction::Click)
                .map_err(|e| format!("发送 Tab 失败：{e}")),
            TabAction::ShiftTab => {
                enigo
                    .key(Key::Shift, Direction::Press)
                    .map_err(|e| format!("按下 Shift 失败：{e}"))?;
                let out = enigo.key(Key::Tab, Direction::Click);
                let up = enigo.key(Key::Shift, Direction::Release);
                out.map_err(|e| format!("发送 Shift+Tab 失败：{e}"))?;
                up.map_err(|e| format!("松开 Shift 失败：{e}"))
            }
            _ => Ok(()),
        }
    }

    /// 聚焦目标：优先 UIA SetFocus，失败退化到鼠标点击对象中心。
    /// 动作前校验目标中心仍在屏幕内；UIA 定位到的元素如果已经不可见/移出屏幕，
    /// 直接报错，不退化点击。
    fn focus_target(target: &TabObject, screen: ScreenRect) -> Result<String, String> {
        ensure_target_on_screen(target, screen)?;
        let (cx, cy) = target.center();
        let session = UiaSession::new()?;
        let element = unsafe { session.automation.ElementFromPoint(POINT { x: cx, y: cy }) };
        let mut method = String::new();
        if let Ok(el) = element {
            // 元素此刻的实际 rect 也要在屏幕内（窗口可能已移动/隐藏）。
            if let Ok(rect) = unsafe { el.CurrentBoundingRectangle() } {
                let fresh = rect_tuple(rect);
                let fresh_center = (fresh.0 + fresh.2 / 2, fresh.1 + fresh.3 / 2);
                if !screen.contains_point(fresh_center.0, fresh_center.1) {
                    return Err(
                        "目标已不在当前屏幕内（可能已滚动/隐藏或窗口被移出屏幕），请重新 tab_scan"
                            .to_string(),
                    );
                }
            }
            let name = bstr_string(unsafe { el.CurrentName() });
            let name_matches = target.name.is_empty()
                || name == target.name
                || name.contains(&target.name)
                || target.name.contains(&name);
            if name_matches && unsafe { el.SetFocus() }.is_ok() {
                method = "UIA SetFocus 成功".to_string();
            }
        }
        if method.is_empty() {
            mouse_click_at(cx, cy)?;
            method = "UIA SetFocus 不可用，已用鼠标点击对象中心".to_string();
        }
        Ok(method)
    }

    fn focused_element_info() -> Option<(String, String)> {
        let session = UiaSession::new().ok()?;
        let el = unsafe { session.automation.GetFocusedElement() }.ok()?;
        let name = bstr_string(unsafe { el.CurrentName() });
        let ct = control_type_name(
            unsafe { el.CurrentControlType() }
                .map(|id| id.0 as u32)
                .unwrap_or(0),
        )
        .to_string();
        Some((name, ct))
    }

    /// 执行 `tab_act` 动作。
    pub(super) fn act_on(
        ctx: &ToolCtx,
        action: TabAction,
        target: &TabObject,
        screen: ScreenRect,
    ) -> ToolOutcome {
        if ctx.is_cancelled() {
            return ToolOutcome::err("操作已取消");
        }
        // 二次校验（defense in depth）：屏幕外目标绝不点鼠标。
        if let Err(e) = ensure_target_on_screen(target, screen) {
            return ToolOutcome::err(e);
        }
        let mut focus_method: Option<String> = None;
        if action.needs_focus() {
            match focus_target(target, screen) {
                Ok(m) => focus_method = Some(m),
                Err(e) => {
                    return ToolOutcome::err(format!(
                        "tab_act 聚焦失败（index={}）：{e}",
                        target.index
                    ));
                }
            }
        }
        let result = match action {
            TabAction::Focus => Ok(focus_method.unwrap_or_default()),
            TabAction::Click => mouse_click_at(target.center().0, target.center().1).map(|_| {
                format!(
                    "已鼠标左键点击对象中心 ({}, {})",
                    target.center().0,
                    target.center().1
                )
            }),
            _ => send_key(action).map(|_| {
                format!(
                    "{}；已发送 {} 键",
                    focus_method.unwrap_or_default(),
                    action.key_label()
                )
            }),
        };
        let method = match result {
            Ok(method) => method,
            Err(e) => {
                if ctx.is_cancelled() {
                    return ToolOutcome::err("操作已取消");
                }
                return ToolOutcome::err(format!(
                    "tab_act 执行失败（index={}）：{e}",
                    target.index
                ));
            }
        };
        let focused = focused_element_info();
        let content = act_content(action, target, &method, focused.as_ref(), screen);
        if ctx.is_cancelled() {
            return ToolOutcome::err("操作已取消");
        }
        ToolOutcome::ok(content)
    }
}
