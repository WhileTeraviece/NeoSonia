//! 屏幕工具：枚举显示器、截图（xcap）、主屏尺寸、屏幕度量。
//!
//! 坐标约定：对外一律使用**物理像素**。`screenshot` 返回 `scale`，
//! 逻辑坐标 = 物理坐标 / scale；`mouse_*` 支持 `logical: true` 时内部换算。
//! `screenshot` 结果里带 `[screenshot:<绝对路径>]` 标记，core 的视觉输入靠它找图。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use base64::Engine as _;
use neosonia_core::model::{ToolCtx, ToolOutcome};
use neosonia_core::registry::ToolRegistry;
use neosonia_core::util::{human_bytes, truncate_chars};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde_json::{json, Value};
use xcap::Monitor;

use crate::{arg_bool, arg_str, arg_u64};

/// 抓屏串行锁。
///
/// 线程安全结论：xcap 0.0.14 在 Windows 上通过 GDI（`BitBlt`/`GetDIBits`）抓屏，GDI 句柄是
/// 每线程独立的，**在后台线程调用是安全的**（我们本来就在 `spawn_blocking` 里调用，不占 tokio
/// 工作线程）；但并发抓同一块屏幕可能受 DWM/显卡驱动影响出现黑图或偶发失败，所以这里
/// 用一把进程级锁把「枚举显示器 + 抓图」串行化，避免多个并发 screenshot 互相干扰。
///
/// parking_lot 的 Mutex 不会中毒（线程 panic 后锁仍可用），满足"锁毒化不 panic"的要求。
static CAPTURE_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

pub fn register(reg: &ToolRegistry) {
    register_info(reg);
    reg.register(Arc::new(Screenshot));
    reg.register(Arc::new(ScreenMark));
}

/// 只注册只读的屏幕信息工具（不允许截图/输入模拟时仍保留）。
pub fn register_info(reg: &ToolRegistry) {
    reg.register(Arc::new(ListDisplays));
    reg.register(Arc::new(ScreenSize));
    reg.register(Arc::new(ScreenMetrics));
}

// ============================================================
// 显示器信息
// ============================================================

#[derive(Clone, Debug)]
struct MonitorMeta {
    index: usize,
    id: u32,
    name: String,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    scale: f32,
    primary: bool,
    rotation: f32,
    frequency: f32,
}

impl MonitorMeta {
    fn logical_size(&self) -> (u32, u32) {
        let s = if self.scale > 0.0 { self.scale } else { 1.0 };
        (
            (self.width as f32 / s).round() as u32,
            (self.height as f32 / s).round() as u32,
        )
    }
}

fn collect_monitors_blocking() -> Result<Vec<MonitorMeta>, String> {
    let _capture_guard = CAPTURE_LOCK.lock();
    let monitors = Monitor::all().map_err(|e| format!("枚举显示器失败：{e}"))?;
    let mut out = Vec::with_capacity(monitors.len());
    for (index, m) in monitors.iter().enumerate() {
        out.push(MonitorMeta {
            index,
            id: m.id(),
            name: m.name().to_string(),
            x: m.x(),
            y: m.y(),
            width: m.width(),
            height: m.height(),
            scale: m.scale_factor(),
            primary: m.is_primary(),
            rotation: m.rotation(),
            frequency: m.frequency(),
        });
    }
    Ok(out)
}

async fn collect_monitors() -> Result<Vec<MonitorMeta>, String> {
    tokio::task::spawn_blocking(collect_monitors_blocking)
        .await
        .map_err(|e| format!("显示器枚举任务异常：{e}"))?
}

fn fmt_scale(scale: f32) -> String {
    if scale > 0.0 {
        format!("{scale:.2}")
    } else {
        "1.00".to_string()
    }
}

async fn list_displays(_ctx: &ToolCtx, _args: Value) -> ToolOutcome {
    let monitors = match collect_monitors().await {
        Ok(m) => m,
        Err(e) => return ToolOutcome::err(e),
    };
    if monitors.is_empty() {
        return ToolOutcome::err("没有检测到显示器（可能是无头环境）。".to_string());
    }
    let mut out = format!("共 {} 个显示器：\n", monitors.len());
    for m in &monitors {
        let (lw, lh) = m.logical_size();
        out.push_str(&format!(
            "#{}  {}  {}  {}x{} 物理像素（缩放 {}，逻辑 {}x{}）  位置 ({},{})  刷新 {:.0}Hz{}\n",
            m.index,
            if m.primary { "主屏" } else { "扩展" },
            m.name,
            m.width,
            m.height,
            fmt_scale(m.scale),
            lw,
            lh,
            m.x,
            m.y,
            m.frequency,
            if m.rotation != 0.0 {
                format!("  旋转 {:.0} 度", m.rotation)
            } else {
                String::new()
            }
        ));
    }
    out.push_str("\n坐标约定：screenshot / mouse_* 工具使用物理像素。");
    ToolOutcome::ok(out).with_display(format!("{} 个显示器", monitors.len()))
}

async fn screen_size(_ctx: &ToolCtx, _args: Value) -> ToolOutcome {
    let monitors = match collect_monitors().await {
        Ok(m) => m,
        Err(e) => return ToolOutcome::err(e),
    };
    let Some(m) = monitors
        .iter()
        .find(|m| m.primary)
        .or_else(|| monitors.first())
    else {
        return ToolOutcome::err("没有检测到显示器（可能是无头环境）。".to_string());
    };
    let (lw, lh) = m.logical_size();
    ToolOutcome::ok(format!(
        "主屏：{}\n物理尺寸：{}x{}\n逻辑尺寸：{}x{}（缩放 {}）\n位置：({}, {})\n\n\
         坐标约定：screenshot / mouse_* 使用物理像素；逻辑坐标 = 物理坐标 / {}。",
        m.name,
        m.width,
        m.height,
        lw,
        lh,
        fmt_scale(m.scale),
        m.x,
        m.y,
        fmt_scale(m.scale)
    ))
    .with_display(format!("{}x{}", m.width, m.height))
}

// ============================================================
// 屏幕度量（虚拟屏 / 各显示器 / 光标位置）
// ============================================================

/// 所有显示器合起来的虚拟屏幕矩形（物理像素）：(x, y, w, h)。
fn virtual_screen_rect(monitors: &[MonitorMeta]) -> Option<(i32, i32, u32, u32)> {
    let mut iter = monitors.iter();
    let first = iter.next()?;
    let mut min_x = first.x;
    let mut min_y = first.y;
    let mut max_x = first.x.saturating_add(first.width as i32);
    let mut max_y = first.y.saturating_add(first.height as i32);
    for monitor in iter {
        min_x = min_x.min(monitor.x);
        min_y = min_y.min(monitor.y);
        max_x = max_x.max(monitor.x.saturating_add(monitor.width as i32));
        max_y = max_y.max(monitor.y.saturating_add(monitor.height as i32));
    }
    Some((
        min_x,
        min_y,
        (max_x - min_x).max(0) as u32,
        (max_y - min_y).max(0) as u32,
    ))
}

/// 读取当前光标位置（物理像素）；失败返回 None，不让工具失败。
async fn cursor_position() -> Option<(i32, i32)> {
    tokio::task::spawn_blocking(|| {
        use enigo::{Enigo, Mouse, Settings as EnigoSettings};
        let enigo = Enigo::new(&EnigoSettings::default()).ok()?;
        enigo.location().ok()
    })
    .await
    .ok()
    .flatten()
}

async fn screen_metrics(_ctx: &ToolCtx, _args: Value) -> ToolOutcome {
    let monitors = match collect_monitors().await {
        Ok(m) => m,
        Err(e) => return ToolOutcome::err(e),
    };
    if monitors.is_empty() {
        return ToolOutcome::err("没有检测到显示器（可能是无头环境）。".to_string());
    }
    let cursor = cursor_position().await;
    let mut out = String::from("[ok] 屏幕度量\n");
    match virtual_screen_rect(&monitors) {
        Some((x, y, w, h)) => out.push_str(&format!(
            "虚拟屏幕：x={x} y={y} 宽={w} 高={h}（物理像素，覆盖所有显示器）\n"
        )),
        None => out.push_str("虚拟屏幕：（无法计算）\n"),
    }
    match cursor {
        Some((x, y)) => out.push_str(&format!("光标位置：({x}, {y}) 物理像素\n")),
        None => out.push_str("光标位置：（读取失败，可能没有可交互桌面）\n"),
    }
    out.push_str(&format!("显示器：{} 个\n", monitors.len()));
    for m in &monitors {
        let (lw, lh) = m.logical_size();
        out.push_str(&format!(
            "#{}  {}  {}  物理 {}x{}  逻辑 {}x{}  缩放 {}  位置 ({},{}){}\n",
            m.index,
            if m.primary { "主屏" } else { "扩展屏" },
            m.name,
            m.width,
            m.height,
            lw,
            lh,
            fmt_scale(m.scale),
            m.x,
            m.y,
            if m.rotation != 0.0 {
                format!("  旋转 {:.0} 度", m.rotation)
            } else {
                String::new()
            }
        ));
    }
    out.push_str(
        "提示：screenshot / mouse_* 默认物理像素；mouse_* 传 logical=true 时按主屏缩放自动换算。",
    );
    ToolOutcome::ok(out).with_display(format!(
        "{} 显示器，光标 {}",
        monitors.len(),
        cursor
            .map(|(x, y)| format!("({x}, {y})"))
            .unwrap_or_else(|| "未知".to_string())
    ))
}

// ============================================================
// 截图
// ============================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Region {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
}

fn parse_region(value: Option<&Value>) -> Result<Option<Region>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let obj = value
        .as_object()
        .ok_or_else(|| "region 必须是对象，例如 {\"x\":0,\"y\":0,\"w\":800,\"h\":600}。".to_string())?;
    let get = |key: &str| -> Option<i64> {
        match obj.get(key) {
            Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
            Some(Value::String(s)) => s.trim().parse::<f64>().ok().map(|f| f as i64),
            _ => None,
        }
    };
    let x = get("x").ok_or_else(|| "region.x 缺失或不是数字。".to_string())?;
    let y = get("y").ok_or_else(|| "region.y 缺失或不是数字。".to_string())?;
    let w = get("w")
        .or_else(|| get("width"))
        .ok_or_else(|| "region.w 缺失或不是数字。".to_string())?;
    let h = get("h")
        .or_else(|| get("height"))
        .ok_or_else(|| "region.h 缺失或不是数字。".to_string())?;
    if w <= 0 || h <= 0 {
        return Err("region 的宽高必须大于 0。".to_string());
    }
    Ok(Some(Region {
        x: x.clamp(-1_000_000, 1_000_000) as i32,
        y: y.clamp(-1_000_000, 1_000_000) as i32,
        w: (w as u32).min(100_000),
        h: (h as u32).min(100_000),
    }))
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DisplaySelector {
    Primary,
    Index(usize),
    Id(u32),
    Name(String),
}

fn parse_display_selector(raw: Option<&str>) -> DisplaySelector {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return DisplaySelector::Primary;
    };
    let lower = raw.to_ascii_lowercase();
    if lower == "primary" || lower == "main" || raw == "主屏" {
        return DisplaySelector::Primary;
    }
    if let Some(rest) = lower.strip_prefix("id:") {
        if let Ok(id) = rest.parse::<u32>() {
            return DisplaySelector::Id(id);
        }
    }
    if let Ok(idx) = raw.parse::<usize>() {
        return DisplaySelector::Index(idx);
    }
    DisplaySelector::Name(raw.to_string())
}

fn pick_monitor(monitors: &[MonitorMeta], selector: &DisplaySelector) -> Result<usize, String> {
    if monitors.is_empty() {
        return Err("没有检测到显示器（可能是无头环境）。".to_string());
    }
    match selector {
        DisplaySelector::Primary => Ok(monitors
            .iter()
            .position(|m| m.primary)
            .unwrap_or(0)),
        DisplaySelector::Index(i) => {
            if *i < monitors.len() {
                Ok(*i)
            } else {
                Err(format!(
                    "显示器序号 {i} 越界；当前有 {} 个显示器（从 0 开始），可用 list_displays 查看。",
                    monitors.len()
                ))
            }
        }
        DisplaySelector::Id(id) => monitors
            .iter()
            .position(|m| m.id == *id)
            .ok_or_else(|| format!("没有 id={id} 的显示器；可用 list_displays 查看。")),
        DisplaySelector::Name(name) => monitors
            .iter()
            .position(|m| m.name.to_ascii_lowercase().contains(&name.to_ascii_lowercase()))
            .ok_or_else(|| format!("没有名字包含 {name:?} 的显示器；可用 list_displays 查看。")),
    }
}

// ============================================================
// 坐标网格（自绘 3x5 点阵数字，不新增依赖）
// ============================================================

/// 3x5 点阵字形：5 行，每行低 3 位有效（bit2 = 最左）。
fn glyph_3x5(ch: char) -> Option<[u8; 5]> {
    Some(match ch {
        '0' => [0b111, 0b101, 0b101, 0b101, 0b111],
        '1' => [0b010, 0b110, 0b010, 0b010, 0b111],
        '2' => [0b111, 0b001, 0b111, 0b100, 0b111],
        '3' => [0b111, 0b001, 0b111, 0b001, 0b111],
        '4' => [0b101, 0b101, 0b111, 0b001, 0b001],
        '5' => [0b111, 0b100, 0b111, 0b001, 0b111],
        '6' => [0b111, 0b100, 0b111, 0b101, 0b111],
        '7' => [0b111, 0b001, 0b010, 0b100, 0b100],
        '8' => [0b111, 0b101, 0b111, 0b101, 0b111],
        '9' => [0b111, 0b101, 0b111, 0b001, 0b111],
        '-' => [0b000, 0b000, 0b111, 0b000, 0b000],
        ',' => [0b000, 0b000, 0b000, 0b010, 0b100],
        _ => return None,
    })
}

fn fill_rect(
    img: &mut image::RgbaImage,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    color: image::Rgba<u8>,
) {
    let (img_w, img_h) = (img.width() as i64, img.height() as i64);
    for py in y as i64..y as i64 + height as i64 {
        if py < 0 || py >= img_h {
            continue;
        }
        for px in x as i64..x as i64 + width as i64 {
            if px < 0 || px >= img_w {
                continue;
            }
            img.put_pixel(px as u32, py as u32, color);
        }
    }
}

/// 画一条纵向/横向网格线（半透明时与原像素混合，保证明暗背景都可见）。
fn draw_line(
    img: &mut image::RgbaImage,
    horizontal: bool,
    fixed: i32,
    from: i32,
    to: i32,
    color: image::Rgba<u8>,
) {
    let (img_w, img_h) = (img.width() as i64, img.height() as i64);
    let alpha = color.0[3] as u32;
    for pos in from..to {
        let (x, y) = if horizontal { (pos, fixed) } else { (fixed, pos) };
        if x < 0 || y < 0 || (x as i64) >= img_w || (y as i64) >= img_h {
            continue;
        }
        let base = img.get_pixel(x as u32, y as u32).0;
        if alpha >= 255 {
            img.put_pixel(x as u32, y as u32, color);
        } else {
            let inv = 255 - alpha;
            img.put_pixel(
                x as u32,
                y as u32,
                image::Rgba([
                    ((base[0] as u32 * inv + color.0[0] as u32 * alpha) / 255) as u8,
                    ((base[1] as u32 * inv + color.0[1] as u32 * alpha) / 255) as u8,
                    ((base[2] as u32 * inv + color.0[2] as u32 * alpha) / 255) as u8,
                    255,
                ]),
            );
        }
    }
}

/// 画 3x5 点阵文本：先黑底再白字。`scale` 是每个点的像素边长。
fn draw_text_3x5(
    img: &mut image::RgbaImage,
    x: i32,
    y: i32,
    text: &str,
    scale: u32,
    color: image::Rgba<u8>,
) {
    let scale = scale.max(1);
    let pad = scale.max(1);
    let mut cursor_x = x;
    for ch in text.chars() {
        let Some(rows) = glyph_3x5(ch) else {
            cursor_x += (3 * scale + scale) as i32;
            continue;
        };
        fill_rect(
            img,
            cursor_x - pad as i32,
            y - pad as i32,
            3 * scale + 2 * pad,
            5 * scale + 2 * pad,
            image::Rgba([0, 0, 0, 190]),
        );
        for (row, bits) in rows.iter().enumerate() {
            for col in 0..3u32 {
                if bits & (1 << (2 - col)) != 0 {
                    fill_rect(
                        img,
                        cursor_x + (col * scale) as i32,
                        y + (row as u32 * scale) as i32,
                        scale,
                        scale,
                        color,
                    );
                }
            }
        }
        cursor_x += (3 * scale + scale) as i32;
    }
}

/// 网格线绝对坐标：从 `origin`（图像左上角对应的物理坐标）到 `origin + physical_len`，
/// 每 `step` 个物理像素一条（返回绝对物理坐标）。
fn grid_lines(origin: i32, physical_len: u32, step: u32) -> Vec<i32> {
    let step = step.max(1) as i32;
    let end = origin.saturating_add(physical_len as i32);
    let mut start = (origin as f64 / step as f64).ceil() as i32 * step;
    if start < origin {
        start = origin;
    }
    let mut out = Vec::new();
    let mut value = start;
    while value <= end {
        out.push(value);
        value = value.saturating_add(step);
    }
    out
}

/// 在截图上画坐标网格：每 100 物理像素一条细线，每 500 像素一条粗线，
/// 上边标 x 刻度、左边标 y 刻度、左上角标图像起点坐标（均为物理像素）。
fn draw_grid(
    img: &mut image::RgbaImage,
    origin_x: i32,
    origin_y: i32,
    physical_width: u32,
    physical_height: u32,
) {
    if physical_width == 0 || physical_height == 0 || img.width() == 0 || img.height() == 0 {
        return;
    }
    let img_w = img.width() as i32;
    let img_h = img.height() as i32;
    let factor = img.width() as f32 / physical_width as f32;
    let coarse = image::Rgba([255, 122, 26, 220]);
    let fine = image::Rgba([255, 255, 255, 70]);
    let label_color = image::Rgba([255, 255, 255, 255]);
    let scale = ((img.width() as f32 / 800.0).round() as u32).clamp(1, 3);

    // 竖线（x 坐标）
    for ax in grid_lines(origin_x, physical_width, 100) {
        let ix = (((ax - origin_x) as f32) * factor).round() as i32;
        let is_coarse = ax % 500 == 0;
        draw_line(
            img,
            false,
            ix,
            0,
            img_h,
            if is_coarse { coarse } else { fine },
        );
        if is_coarse {
            let text = ax.to_string();
            draw_text_3x5(img, ix + 2, 2, &text, scale, label_color);
        }
    }
    // 横线（y 坐标）
    for ay in grid_lines(origin_y, physical_height, 100) {
        let iy = (((ay - origin_y) as f32) * factor).round() as i32;
        let is_coarse = ay % 500 == 0;
        draw_line(
            img,
            true,
            iy,
            0,
            img_w,
            if is_coarse { coarse } else { fine },
        );
        if is_coarse {
            let text = ay.to_string();
            draw_text_3x5(img, 2, iy + 2, &text, scale, label_color);
        }
    }
    // 左上角标注图像起点的物理坐标（多屏时可能不是 0,0）
    let origin_label = format!("{origin_x},{origin_y}");
    draw_text_3x5(img, 2, 2, &origin_label, scale, label_color);
}

struct ScreenshotResult {
    path: PathBuf,
    width: u32,
    height: u32,
    monitor: MonitorMeta,
    region: Option<Region>,
    original_width: u32,
    original_height: u32,
    /// 裁剪后（未缩放前）的物理尺寸，用于换算网格/坐标。
    physical_width: u32,
    physical_height: u32,
    grid: bool,
    /// 抓屏方式（"GDI" 快速路径 / "xcap" 兜底）。
    capture_method: &'static str,
    /// 抓屏 / 预处理 / PNG 编码 / 总耗时（毫秒）。
    capture_ms: u64,
    prepare_ms: u64,
    encode_ms: u64,
    total_ms: u64,
    file_bytes: u64,
    png: Option<Vec<u8>>,
}

// ============================================================
// Windows GDI 快速抓屏（BitBlt + GetDIBits），xcap 作为兜底
// ============================================================

/// 线程 DPI 上下文守卫：GDI 的屏幕 DC 坐标是"本线程 DPI 上下文"下的像素。
/// 进程/线程不是 per-monitor 感知时，GetDC 拿到的是被系统缩放过的坐标（会截错/黑边），
/// 所以抓屏线程临时切到 PER_MONITOR_AWARE_V2（-4），结束后恢复原上下文。
/// 用 user32 的 SetThreadDpiAwarenessContext 直接 FFI，避免给 Cargo.toml 加 HiDpi feature。
#[cfg(windows)]
struct ThreadDpiGuard(isize);

#[cfg(windows)]
impl Drop for ThreadDpiGuard {
    fn drop(&mut self) {
        set_thread_dpi_awareness_context(self.0);
    }
}

#[cfg(windows)]
fn set_thread_dpi_awareness_context(context: isize) -> isize {
    unsafe extern "system" {
        fn SetThreadDpiAwarenessContext(value: isize) -> isize;
    }
    // SAFETY: user32 的 SetThreadDpiAwarenessContext 接受 DPI_AWARENESS_CONTEXT 句柄；
    // -4 = PER_MONITOR_AWARE_V2，0/-1 等特殊值也原样传回即可（仅用于恢复）。
    unsafe { SetThreadDpiAwarenessContext(context) }
}

#[cfg(windows)]
fn set_physical_dpi_for_capture() -> ThreadDpiGuard {
    const DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2: isize = -4;
    let previous = set_thread_dpi_awareness_context(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    ThreadDpiGuard(previous)
}

/// 用 GDI 抓某个显示器的整屏（物理像素），返回 RGBA 图。
#[cfg(windows)]
fn capture_gdi_scaled(
    meta: &MonitorMeta,
    region: Option<Region>,
    out_w: u32,
    out_h: u32,
) -> Result<image::RgbaImage, String> {
    use windows::Win32::Graphics::Gdi::{
        BITMAPINFO, BITMAPINFOHEADER, CAPTUREBLT, CreateCompatibleBitmap, CreateCompatibleDC,
        DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDIBits, GetDC, HGDIOBJ, HALFTONE, ROP_CODE,
        ReleaseDC, SRCCOPY, SelectObject, SetStretchBltMode, StretchBlt,
    };

    if out_w == 0 || out_h == 0 {
        return Err(format!("目标截图尺寸非法：{out_w}x{out_h}"));
    }
    // 源矩形：显示器物理坐标 + 可选 region 裁剪（绝对屏幕坐标，GDI StretchBlt 用）
    let (src_x, src_y, src_w, src_h) = match region {
        Some(r) => {
            let x = r.x.max(0) as u32;
            let y = r.y.max(0) as u32;
            if x >= meta.width || y >= meta.height {
                return Err(format!(
                    "region 起点 ({}, {}) 超出截图范围 {}x{}。",
                    r.x, r.y, meta.width, meta.height
                ));
            }
            let w = r.w.min(meta.width - x);
            let h = r.h.min(meta.height - y);
            if w == 0 || h == 0 {
                return Err("region 裁剪后尺寸为 0，请检查参数。".to_string());
            }
            (meta.x + x as i32, meta.y + y as i32, w, h)
        }
        None => (meta.x, meta.y, meta.width, meta.height),
    };

    // 整个抓屏过程都在本线程完成；置 DPI 上下文，保证坐标是物理像素。
    let _dpi = set_physical_dpi_for_capture();

    let screen_dc = unsafe { GetDC(None) };
    if screen_dc.is_invalid() {
        return Err("GetDC 失败（拿不到屏幕 DC）。".to_string());
    }
    // 用一个小守卫保证所有 GDI 句柄都被释放（提前 return 也不泄漏）。
    struct GdiGuard {
        screen_dc: windows::Win32::Graphics::Gdi::HDC,
        mem_dc: windows::Win32::Graphics::Gdi::HDC,
        bitmap: windows::Win32::Graphics::Gdi::HBITMAP,
    }
    impl Drop for GdiGuard {
        fn drop(&mut self) {
            unsafe {
                if !self.bitmap.is_invalid() {
                    let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
                }
                if !self.mem_dc.is_invalid() {
                    let _ = DeleteDC(self.mem_dc);
                }
                if !self.screen_dc.is_invalid() {
                    let _ = ReleaseDC(None, self.screen_dc);
                }
            }
        }
    }

    let mem_dc = unsafe { CreateCompatibleDC(screen_dc) };
    if mem_dc.is_invalid() {
        return Err("CreateCompatibleDC 失败（内存 DC 不可用）。".to_string());
    }
    // 注意：这里直接按"最终输出尺寸"建位图，StretchBlt 一次完成抓屏+缩放，
    // 避免 4K 原图 + image resize（后者在 debug/低配机上要几百 ms）。
    let bitmap = unsafe { CreateCompatibleBitmap(screen_dc, out_w as i32, out_h as i32) };
    if bitmap.is_invalid() {
        return Err("CreateCompatibleBitmap 失败（无法分配位图）。".to_string());
    }
    let guard = GdiGuard {
        screen_dc,
        mem_dc,
        bitmap,
    };
    let old = unsafe { SelectObject(guard.mem_dc, HGDIOBJ(guard.bitmap.0)) };

    unsafe {
        // HALFTONE：缩放质量好（视觉模型读小字更清楚）；COLORONCOLOR 更快但锯齿明显。
        SetStretchBltMode(guard.mem_dc, HALFTONE);
        // CAPTUREBLT：把分层窗口（HUD/透明窗口）也抓进来。
        let ok = StretchBlt(
            guard.mem_dc,
            0,
            0,
            out_w as i32,
            out_h as i32,
            screen_dc,
            src_x,
            src_y,
            src_w as i32,
            src_h as i32,
            ROP_CODE(SRCCOPY.0 | CAPTUREBLT.0),
        );
        if !ok.as_bool() {
            return Err("StretchBlt 抓屏失败（可能被安全软件/受限桌面拦截）。".to_string());
        }
    }

    // 读回 32bpp 顶向下的像素（负高度 = top-down，省一次翻转）。
    let mut bgra = vec![0u8; (out_w as usize) * (out_h as usize) * 4];
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: out_w as i32,
            biHeight: -(out_h as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: 0, // BI_RGB
            ..Default::default()
        },
        ..Default::default()
    };
    let lines = unsafe {
        GetDIBits(
            guard.mem_dc,
            guard.bitmap,
            0,
            out_h,
            Some(bgra.as_mut_ptr().cast()),
            &mut info,
            DIB_RGB_COLORS,
        )
    };
    if lines == 0 {
        return Err("GetDIBits 读取像素失败。".to_string());
    }
    // 释放选择到 DC 里的旧对象
    unsafe {
        let _ = SelectObject(guard.mem_dc, old);
    }
    // GDI 是 BGRA（32bpp BI_RGB 的 alpha 字节未定义，通常是 0），image 需要 RGBA。
    // 注意必须把 alpha 强制设为 255，否则 PNG 全透明（读图工具会显示白屏）。
    for pixel in bgra.chunks_exact_mut(4) {
        pixel.swap(0, 2);
        pixel[3] = 255;
    }
    image::RgbaImage::from_raw(out_w, out_h, bgra)
        .ok_or_else(|| "构建 RGBA 图像失败（尺寸不匹配）。".to_string())
}

/// 用 PNG 快速压缩写文件（no filter + Fast 压缩级别），比默认压缩快一个数量级。
fn write_png_fast(img: &image::RgbaImage, path: &std::path::Path) -> Result<(), String> {
    use image::ImageEncoder;
    use image::codecs::png::{CompressionType, FilterType, PngEncoder};
    let file = std::fs::File::create(path)
        .map_err(|e| format!("创建 PNG 文件 {} 失败：{e}", path.display()))?;
    let writer = std::io::BufWriter::new(file);
    let encoder = PngEncoder::new_with_quality(writer, CompressionType::Fast, FilterType::NoFilter);
    encoder
        .write_image(
            img.as_raw(),
            img.width(),
            img.height(),
            image::ExtendedColorType::Rgba8,
        )
        .map_err(|e| format!("写 PNG 失败：{e}"))
}

fn capture_blocking(
    selector: DisplaySelector,
    region: Option<Region>,
    save_target: PathBuf,
    max_edge: u32,
    grid: bool,
    fast: bool,
    want_png: bool,
) -> Result<ScreenshotResult, String> {
    let started = Instant::now();
    // 串行化 xcap 抓屏（见 CAPTURE_LOCK 注释），避免并发截图互相干扰。
    let _capture_guard = CAPTURE_LOCK.lock();
    let monitors = Monitor::all().map_err(|e| format!("枚举显示器失败：{e}"))?;
    if monitors.is_empty() {
        return Err("没有检测到显示器（可能是无头环境）。".to_string());
    }
    let metas: Vec<MonitorMeta> = monitors
        .iter()
        .enumerate()
        .map(|(index, m)| MonitorMeta {
            index,
            id: m.id(),
            name: m.name().to_string(),
            x: m.x(),
            y: m.y(),
            width: m.width(),
            height: m.height(),
            scale: m.scale_factor(),
            primary: m.is_primary(),
            rotation: m.rotation(),
            frequency: m.frequency(),
        })
        .collect();
    let index = pick_monitor(&metas, &selector)?;
    let meta = metas[index].clone();

    // 先按 region + max_edge 算出最终输出尺寸（GDI StretchBlt 一次完成抓屏+缩放）。
    let mut crop_origin_x = meta.x;
    let mut crop_origin_y = meta.y;
    let mut physical_width = meta.width;
    let mut physical_height = meta.height;
    if let Some(r) = region {
        let x = r.x.max(0) as u32;
        let y = r.y.max(0) as u32;
        if x >= meta.width || y >= meta.height {
            return Err(format!(
                "region 起点 ({}, {}) 超出截图范围 {}x{}。",
                r.x, r.y, meta.width, meta.height
            ));
        }
        let w = r.w.min(meta.width - x);
        let h = r.h.min(meta.height - y);
        if w == 0 || h == 0 {
            return Err("region 裁剪后尺寸为 0，请检查参数。".to_string());
        }
        crop_origin_x = meta.x.saturating_add(x as i32);
        crop_origin_y = meta.y.saturating_add(y as i32);
        physical_width = w;
        physical_height = h;
    }
    let (out_w, out_h) = if max_edge > 0 && physical_width.max(physical_height) > max_edge {
        let scale = max_edge as f32 / physical_width.max(physical_height) as f32;
        (
            ((physical_width as f32 * scale).round() as u32).max(1),
            ((physical_height as f32 * scale).round() as u32).max(1),
        )
    } else {
        (physical_width, physical_height)
    };

    // ---- 抓屏：默认 GDI 快速路径（抓屏+缩放一次完成），失败时回退 xcap ----
    let mut capture_method = "xcap";
    let capture_started = Instant::now();
    #[allow(unused_mut)]
    let mut img: Option<image::RgbaImage> = None;
    #[cfg(windows)]
    if fast {
        match capture_gdi_scaled(&meta, region, out_w, out_h) {
            Ok(image) => {
                capture_method = "GDI";
                img = Some(image);
            }
            Err(e) => {
                capture_method = "xcap";
                tracing::warn!(error = %e, "GDI 快速抓屏失败，回退 xcap");
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = fast;
        capture_method = "xcap";
    }
    let capture_ms;
    let mut img = match img {
        Some(image) => {
            capture_ms = capture_started.elapsed().as_millis() as u64;
            image
        }
        None => {
            // xcap 兜底：抓整屏后自己裁剪+缩放（慢，但兼容性好）
            let monitor = &monitors[index];
            let shot = monitor
                .capture_image()
                .map_err(|e| format!("抓屏失败（{}）：{e}", meta.name))?;
            let mut image = shot;
            if region.is_some() {
                let x = (crop_origin_x - meta.x).max(0) as u32;
                let y = (crop_origin_y - meta.y).max(0) as u32;
                image = image::imageops::crop_imm(&image, x, y, physical_width, physical_height)
                    .to_image();
            }
            if image.width() != out_w || image.height() != out_h {
                image = image::imageops::resize(
                    &image,
                    out_w,
                    out_h,
                    image::imageops::FilterType::Triangle,
                );
            }
            capture_ms = capture_started.elapsed().as_millis() as u64;
            image
        }
    };
    let original_width = meta.width;
    let original_height = meta.height;

    // 预处理计时（只剩网格绘制；GDI 路径的缩放已在抓屏里完成）
    let prepare_started = Instant::now();
    let (mut w, mut h) = (img.width(), img.height());
    // 防御：确保最终图像尺寸和预期一致（例如 region 边界被裁掉时）
    if (w, h) != (out_w, out_h) {
        img = image::imageops::resize(&img, out_w, out_h, image::imageops::FilterType::Triangle);
        w = out_w;
        h = out_h;
    }

    // 网格画在最终分辨率上（保证缩放后刻度文字仍清晰），坐标按物理像素换算。
    if grid {
        draw_grid(&mut img, crop_origin_x, crop_origin_y, physical_width, physical_height);
    }
    let prepare_ms = prepare_started.elapsed().as_millis() as u64;

    if let Some(parent) = save_target.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建截图目录 {} 失败：{e}", parent.display()))?;
        }
    }
    // PNG 编码计时：快速压缩（Fast + NoFilter），比 image 默认压缩快很多。
    let encode_started = Instant::now();
    write_png_fast(&img, &save_target)?;
    let encode_ms = encode_started.elapsed().as_millis() as u64;
    let file_bytes = std::fs::metadata(&save_target).map(|m| m.len()).unwrap_or(0);
    let png = if want_png {
        Some(std::fs::read(&save_target).map_err(|e| format!("回读 PNG 失败：{e}"))?)
    } else {
        None
    };
    Ok(ScreenshotResult {
        path: save_target,
        width: w,
        height: h,
        monitor: meta,
        region,
        original_width,
        original_height,
        physical_width,
        physical_height,
        grid,
        capture_method,
        capture_ms,
        prepare_ms,
        encode_ms,
        total_ms: started.elapsed().as_millis() as u64,
        file_bytes,
        png,
    })
}

/// 抓主显示器并保存成 PNG，返回文件路径。
///
/// 供 app 的截图快捷键直接调用（不走 `Tool` trait，不需要 `ToolCtx`）；
/// 内部复用与 `screenshot` 工具完全相同的 `capture_blocking` 实现，不重复抓屏逻辑。
/// 文件名是 `shot-<毫秒时间戳>.png`，目录不存在会自动创建。
pub fn capture_primary(out_dir: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
    if let Err(e) = std::fs::create_dir_all(out_dir) {
        return Err(anyhow::anyhow!(
            "创建截图目录 {} 失败：{e}",
            out_dir.display()
        ));
    }
    let file_name = format!("shot-{}.png", chrono::Local::now().timestamp_millis());
    let target = out_dir.join(file_name);
    match capture_blocking(DisplaySelector::Primary, None, target, 0, false, true, false) {
        Ok(result) => Ok(result.path),
        Err(e) => Err(anyhow::anyhow!("抓取主屏失败：{e}")),
    }
}

async fn screenshot(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    let settings = ctx.settings();
    if !settings.tools.allow_input_simulation {
        // screenshot 与输入模拟同属"屏幕操作"开关；tool 未注册时模型看不到，
        // 这里再兜底一次，避免被别的入口直接调用。
        return ToolOutcome::err(
            "屏幕截图已在设置中关闭（tools.allow_input_simulation=false）。".to_string(),
        );
    }
    let selector = parse_display_selector(arg_str(&args, "display").as_deref());
    let region = match parse_region(args.get("region")) {
        Ok(r) => r,
        Err(e) => return ToolOutcome::err(e),
    };
    let max_edge = arg_u64(
        &args,
        "max_edge",
        settings.tools.screenshot_max_edge as u64,
    )
    .min(20_000) as u32;
    let want_png = arg_bool(&args, "return_base64", false);
    let grid = arg_bool(&args, "grid", false);
    // fast 默认 true：走 Windows GDI 快速抓屏；失败自动回退 xcap。
    let fast = arg_bool(&args, "fast", true);

    // 目标路径
    let save_target: PathBuf = match arg_str(&args, "save_path").filter(|s| !s.trim().is_empty()) {
        Some(raw) => {
            let expanded = neosonia_core::util::expand_tilde(&raw);
            let mut p = if expanded.is_absolute() {
                neosonia_core::util::normalize_lexical(&expanded)
            } else {
                neosonia_core::util::normalize_lexical(&ctx.workspace().join(expanded))
            };
            if p.extension().is_none() {
                p.set_extension("png");
            }
            p
        }
        None => {
            let stem = format!(
                "shot-{}-{}",
                chrono::Local::now().format("%Y%m%d-%H%M%S"),
                neosonia_core::util::short_id()
            );
            ctx.paths().screenshot_path(&stem)
        }
    };

    ctx.phase("正在截屏...");
    let selector2 = selector.clone();
    let target = save_target.clone();
    let joined = tokio::task::spawn_blocking(move || {
        capture_blocking(selector2, region, target, max_edge, grid, fast, want_png)
    })
    .await;
    let result = match joined {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return ToolOutcome::err(e),
        Err(e) => return ToolOutcome::err(format!("截屏任务异常：{e}")),
    };

    let (lw, lh) = result.monitor.logical_size();
    let mut out = String::from("[ok] 截图已保存\n");
    out.push_str(&format!("路径：{}\n", result.path.display()));
    // 视觉输入的约定标记：core 会扫描工具结果里的 [screenshot:<绝对路径>] 并附图给模型。
    out.push_str(&format!("[screenshot:{}]\n", result.path.display()));
    out.push_str(&format!(
        "尺寸：{}x{} 物理像素（缩放 {}，逻辑 {}x{}）\n",
        result.width,
        result.height,
        fmt_scale(result.monitor.scale),
        lw,
        lh
    ));
    if result.width != result.original_width || result.height != result.original_height {
        out.push_str(&format!(
            "原图：{}x{}（本次有裁剪/缩放）\n",
            result.original_width, result.original_height
        ));
    }
    if let Some(r) = result.region {
        out.push_str(&format!(
            "区域：x={} y={} w={} h={}\n",
            r.x, r.y, r.w, r.h
        ));
    }
    out.push_str(&format!(
        "显示器：#{} {} {}{}\n",
        result.monitor.index,
        result.monitor.name,
        if result.monitor.primary { "主屏" } else { "扩展屏" },
        format!("  位置 ({}, {})", result.monitor.x, result.monitor.y)
    ));
    out.push_str(&format!("文件：{}\n", human_bytes(result.file_bytes)));
    out.push_str(&format!(
        "耗时：抓屏 {}ms（{}）+ 预处理 {}ms + PNG 编码 {}ms = {}ms\n",
        result.capture_ms,
        result.capture_method,
        result.prepare_ms,
        result.encode_ms,
        result.total_ms
    ));
    if result.grid {
        let factor = if result.physical_width > 0 {
            result.width as f32 / result.physical_width as f32
        } else {
            1.0
        };
        out.push_str(&format!(
            "网格：间距 100px（细线）/ 500px（粗线），刻度为物理像素坐标，坐标原点为屏幕左上角（虚拟桌面）；\
             图像覆盖物理范围 {}x{}，图可能被缩放，图上坐标 = 物理坐标 × {factor:.3}，显示器 scale={}。\n",
            result.physical_width,
            result.physical_height,
            fmt_scale(result.monitor.scale)
        ));
    }
    out.push_str("提示：鼠标/键盘工具使用物理像素坐标。\n");
    if let Some(png) = result.png {
        let encoded = base64::engine::general_purpose::STANDARD.encode(&png);
        out.push_str(&format!(
            "\nbase64（PNG，{} 字节）：\n{}\n",
            png.len(),
            encoded
        ));
    }
    let display = format!(
        "{}x{} {}",
        result.width,
        result.height,
        truncate_chars(
            result
                .path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default()
                .as_str(),
            40
        )
    );
    ToolOutcome::ok(out).with_display(display)
}

// ============================================================
// screen_mark：把屏幕坐标可视化标出来（Windows 透明置顶叠加层）
// ============================================================

#[derive(Clone, Debug, PartialEq, Eq)]
struct MarkPoint {
    x: i32,
    y: i32,
    label: String,
}

/// 叠加层窗口的绘制数据（窗口过程从全局状态读取）。
#[derive(Clone, Debug)]
struct MarkPayload {
    origin_x: i32,
    origin_y: i32,
    width: u32,
    height: u32,
    points: Vec<MarkPoint>,
}

fn parse_mark_points(value: Option<&Value>) -> Result<Vec<MarkPoint>, String> {
    let Some(value) = value else {
        return Err(
            "缺少必填参数 points（要标注的坐标数组，例如 [{\"x\":100,\"y\":200}]）。".to_string(),
        );
    };
    let Some(array) = value.as_array() else {
        return Err("points 必须是数组，例如 [{\"x\":100,\"y\":200}]。".to_string());
    };
    if array.is_empty() {
        return Err("points 不能为空，至少给 1 个点。".to_string());
    }
    if array.len() > 32 {
        return Err(format!(
            "points 最多 32 个点（收到 {} 个）；点太多会看不清标注。",
            array.len()
        ));
    }
    let mut points = Vec::with_capacity(array.len());
    for (index, item) in array.iter().enumerate() {
        let position = index + 1;
        let Some(object) = item.as_object() else {
            return Err(format!("第 {position} 个点必须是对象（含 x、y）。"));
        };
        let number = |key: &str| -> Option<i64> {
            match object.get(key) {
                Some(Value::Number(n)) => n.as_i64(),
                Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
                _ => None,
            }
        };
        let x = number("x").ok_or_else(|| format!("第 {position} 个点缺少整数 x。"))?;
        let y = number("y").ok_or_else(|| format!("第 {position} 个点缺少整数 y。"))?;
        let label = object
            .get("label")
            .and_then(|value| value.as_str())
            .map(|label| truncate_chars(label.trim(), 12))
            .unwrap_or_default();
        points.push(MarkPoint {
            x: x.clamp(-1_000_000, 1_000_000) as i32,
            y: y.clamp(-1_000_000, 1_000_000) as i32,
            label,
        });
    }
    Ok(points)
}

/// Windows 透明置顶叠加层：分层窗口 + GDI 绘制，按时长自动关闭。
#[cfg(windows)]
mod mark_overlay {
    use super::{MarkPayload, set_physical_dpi_for_capture};
    use parking_lot::Mutex;
    use std::sync::{OnceLock, mpsc};
    use std::time::Duration;

    type WndProcFn = unsafe extern "system" fn(isize, u32, usize, isize) -> isize;

    #[repr(C)]
    struct WndClassW {
        style: u32,
        lpfn_wnd_proc: Option<WndProcFn>,
        cb_cls_extra: i32,
        cb_wnd_extra: i32,
        h_instance: isize,
        h_icon: isize,
        h_cursor: isize,
        hbr_background: isize,
        lpsz_menu_name: *const u16,
        lpsz_class_name: *const u16,
    }

    #[repr(C)]
    #[derive(Default)]
    struct MsgRaw {
        hwnd: isize,
        message: u32,
        w_param: usize,
        l_param: isize,
        time: u32,
        pt_x: i32,
        pt_y: i32,
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn RegisterClassW(lp_wnd_class: *const WndClassW) -> u16;
        fn CreateWindowExW(
            ex_style: u32,
            class_name: *const u16,
            window_name: *const u16,
            style: u32,
            x: i32,
            y: i32,
            width: i32,
            height: i32,
            parent: isize,
            menu: isize,
            instance: isize,
            param: isize,
        ) -> isize;
        fn DefWindowProcW(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> isize;
        fn DestroyWindow(hwnd: isize) -> i32;
        fn PostQuitMessage(exit_code: i32);
        fn GetMessageW(
            msg: *mut MsgRaw,
            hwnd: isize,
            filter_min: u32,
            filter_max: u32,
        ) -> i32;
        fn TranslateMessage(msg: *const MsgRaw) -> i32;
        fn DispatchMessageW(msg: *const MsgRaw) -> isize;
        fn SetLayeredWindowAttributes(
            hwnd: isize,
            color_key: u32,
            alpha: u8,
            flags: u32,
        ) -> i32;
        fn ShowWindow(hwnd: isize, cmd_show: i32) -> i32;
        fn UpdateWindow(hwnd: isize) -> i32;
        fn SetTimer(hwnd: isize, id: usize, elapse_ms: u32, timer_proc: usize) -> usize;
        fn KillTimer(hwnd: isize, id: usize) -> i32;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetModuleHandleW(module_name: *const u16) -> isize;
        fn GetLastError() -> u32;
    }

    const WS_POPUP: u32 = 0x8000_0000;
    const WS_EX_LAYERED: u32 = 0x0008_0000;
    const WS_EX_TRANSPARENT: u32 = 0x0000_0020;
    const WS_EX_TOPMOST: u32 = 0x0000_0008;
    const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
    const LWA_COLORKEY: u32 = 0x0000_0001;
    const SW_SHOWNOACTIVATE: i32 = 4;
    const WM_PAINT: u32 = 0x000F;
    const WM_TIMER: u32 = 0x0113;
    const WM_DESTROY: u32 = 0x0002;
    const ERROR_CLASS_ALREADY_EXISTS: u32 = 1410;

    static PAYLOAD: OnceLock<Mutex<Option<MarkPayload>>> = OnceLock::new();

    fn payload() -> &'static Mutex<Option<MarkPayload>> {
        PAYLOAD.get_or_init(|| Mutex::new(None))
    }

    fn utf16z(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// 在窗口 DC 上画十字 + 圆环 + 编号（黑底作为 color key 透明）。
    fn paint(hwnd: isize) {
        use windows::Win32::Foundation::{COLORREF, HWND, RECT};
        use windows::Win32::Graphics::Gdi::{
            BeginPaint, CreatePen, CreateSolidBrush, DeleteObject, Ellipse, EndPaint, FillRect,
            HGDIOBJ, LineTo, MoveToEx, PAINTSTRUCT, PS_SOLID, SelectObject, SetBkMode, SetTextColor,
            TRANSPARENT, TextOutW,
        };
        use windows::Win32::UI::WindowsAndMessaging::GetClientRect;

        unsafe {
            let window = HWND(hwnd as *mut core::ffi::c_void);
            let mut paint_struct = PAINTSTRUCT::default();
            let hdc = BeginPaint(window, &mut paint_struct);
            let black = CreateSolidBrush(COLORREF(0));
            let mut client = RECT::default();
            let _ = GetClientRect(window, &mut client);
            FillRect(hdc, &client, black);

            let snapshot: Option<MarkPayload> = payload().lock().clone();
            if let Some(data) = snapshot {
                if !data.points.is_empty() {
                    // 红色十字 + 圆环（青色），黑色背景被 color key 抠成透明
                    let cross = CreatePen(PS_SOLID, 2, COLORREF(0x0000_00FF));
                    let ring = CreateSolidBrush(COLORREF(0x00FF_FF00));
                    let old_pen = SelectObject(hdc, HGDIOBJ(cross.0));
                    let old_brush = SelectObject(hdc, HGDIOBJ(ring.0));
                    SetBkMode(hdc, TRANSPARENT);
                    SetTextColor(hdc, COLORREF(0x00FF_FFFF));
                    for (index, point) in data.points.iter().enumerate() {
                        let cx = point.x - data.origin_x;
                        let cy = point.y - data.origin_y;
                        let _ = MoveToEx(hdc, cx - 28, cy, None);
                        let _ = LineTo(hdc, cx + 28, cy);
                        let _ = MoveToEx(hdc, cx, cy - 28, None);
                        let _ = LineTo(hdc, cx, cy + 28);
                        let _ = Ellipse(hdc, cx - 12, cy - 12, cx + 12, cy + 12);
                        let text = if point.label.is_empty() {
                            format!("{}", index + 1)
                        } else {
                            format!("{}.{}", index + 1, point.label)
                        };
                        let wide: Vec<u16> = text.encode_utf16().collect();
                        let _ = TextOutW(hdc, cx + 16, cy - 8, &wide);
                    }
                    let _ = SelectObject(hdc, old_brush);
                    let _ = SelectObject(hdc, old_pen);
                    let _ = DeleteObject(HGDIOBJ(cross.0));
                    let _ = DeleteObject(HGDIOBJ(ring.0));
                }
            }
            let _ = DeleteObject(HGDIOBJ(black.0));
            let _ = EndPaint(window, &paint_struct);
        }
    }

    unsafe extern "system" fn mark_wnd_proc(
        hwnd: isize,
        msg: u32,
        wparam: usize,
        lparam: isize,
    ) -> isize {
        match msg {
            WM_PAINT => {
                paint(hwnd);
                0
            }
            WM_TIMER => {
                // 到时长：杀掉定时器并关窗（消息循环随后退出）
                unsafe {
                    let _ = KillTimer(hwnd, wparam);
                    let _ = DestroyWindow(hwnd);
                }
                0
            }
            WM_DESTROY => {
                unsafe { PostQuitMessage(0) };
                0
            }
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }

    fn run_overlay(
        payload_data: MarkPayload,
        duration_ms: u32,
        handshake: mpsc::Sender<Result<(), String>>,
    ) -> Result<(), String> {
        let _dpi = set_physical_dpi_for_capture();
        // SAFETY: 下面都是标准 Win32 窗体/GDI 调用；字符串/结构体在调用期间有效；
        // 绘制数据放在进程级 Mutex 里供窗口过程读取。
        unsafe {
            let instance = GetModuleHandleW(std::ptr::null());
            if instance == 0 {
                return Err("GetModuleHandleW 失败（拿不到模块句柄）。".to_string());
            }
            let class_name = utf16z("NeoSoniaScreenMark");
            let wnd_class = WndClassW {
                style: 0x0003, // CS_HREDRAW | CS_VREDRAW
                lpfn_wnd_proc: Some(mark_wnd_proc),
                cb_cls_extra: 0,
                cb_wnd_extra: 0,
                h_instance: instance,
                h_icon: 0,
                h_cursor: 0,
                hbr_background: 0,
                lpsz_menu_name: std::ptr::null(),
                lpsz_class_name: class_name.as_ptr(),
            };
            if RegisterClassW(&wnd_class) == 0 {
                let error = GetLastError();
                if error != ERROR_CLASS_ALREADY_EXISTS {
                    return Err(format!("RegisterClassW 失败（错误码 {error}）。"));
                }
            }
            let title = utf16z("NeoSoniaScreenMark");
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                class_name.as_ptr(),
                title.as_ptr(),
                WS_POPUP,
                payload_data.origin_x,
                payload_data.origin_y,
                payload_data.width as i32,
                payload_data.height as i32,
                0,
                0,
                instance,
                0,
            );
            if hwnd == 0 {
                return Err(format!(
                    "CreateWindowExW 失败（错误码 {}）。",
                    GetLastError()
                ));
            }
            // 黑色（0）作为 color key 透明色；窗口整体不接收鼠标（WS_EX_TRANSPARENT）。
            if SetLayeredWindowAttributes(hwnd, 0, 0, LWA_COLORKEY) == 0 {
                let _ = DestroyWindow(hwnd);
                return Err(
                    "SetLayeredWindowAttributes 失败（无法创建透明叠加层，可能被安全软件拦截）。"
                        .to_string(),
                );
            }
            SetTimer(hwnd, 1, duration_ms, 0);
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            UpdateWindow(hwnd);
            // 窗口已经显示成功才向工具返回：之前要等 10s 消息循环结束才回，
            // 工具早超时，真正的建窗错误也被吞掉（用户看到"标注完成"但屏幕没标记）。
            let _ = handshake.send(Ok(()));

            let mut msg = MsgRaw::default();
            loop {
                let result = GetMessageW(&mut msg, 0, 0, 0);
                if result == 0 || result == -1 {
                    break;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        Ok(())
    }

    /// 创建叠加层并立即返回；线程在 duration_ms 后自动关闭窗口。
    pub fn show(payload_data: MarkPayload, duration_ms: u32) -> Result<(), String> {
        *payload().lock() = Some(payload_data.clone());
        let (sender, receiver) = mpsc::channel::<Result<(), String>>();
        std::thread::Builder::new()
            .name("neosonia-screen-mark".to_string())
            .spawn(move || {
                if let Err(e) = run_overlay(payload_data, duration_ms, sender.clone()) {
                    let _ = sender.send(Err(e));
                }
            })
            .map_err(|e| format!("启动标注线程失败：{e}"))?;
        // 等窗口创建结果：最多 800ms，避免工具调用被长时间阻塞；
        // 超时视为"线程仍在创建"，不让工具失败。
        match receiver.recv_timeout(Duration::from_millis(800)) {
            Ok(result) => result,
            Err(_) => Ok(()),
        }
    }
}

#[cfg(not(windows))]
mod mark_overlay {
    use super::MarkPayload;

    pub fn show(_payload: MarkPayload, _duration_ms: u32) -> Result<(), String> {
        Err(
            "screen_mark 目前只支持 Windows（需要 Win32 分层透明窗口 + GDI 绘制）；\
             非 Windows 可用 dry_run=true 只计算坐标。"
                .to_string(),
        )
    }
}

async fn screen_mark(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    let settings = ctx.settings();
    if !settings.tools.allow_input_simulation {
        return ToolOutcome::err(
            "屏幕标注已在设置中关闭（tools.allow_input_simulation=false）。".to_string(),
        );
    }
    let points = match parse_mark_points(args.get("points")) {
        Ok(points) => points,
        Err(e) => return ToolOutcome::err(e),
    };
    let duration_ms = arg_u64(&args, "duration_ms", 3000).clamp(500, 10_000) as u32;
    let dry_run = arg_bool(&args, "dry_run", false);
    let metas = match tokio::task::spawn_blocking(collect_monitors_blocking).await {
        Ok(Ok(metas)) => metas,
        Ok(Err(e)) => return ToolOutcome::err(e),
        Err(e) => return ToolOutcome::err(format!("枚举显示器任务异常：{e}")),
    };
    let Some((origin_x, origin_y, width, height)) = virtual_screen_rect(&metas) else {
        return ToolOutcome::err("没有检测到显示器（可能是无头环境）。".to_string());
    };
    if !dry_run {
        let payload = MarkPayload {
            origin_x,
            origin_y,
            width,
            height,
            points: points.clone(),
        };
        if let Err(e) = mark_overlay::show(payload, duration_ms) {
            return ToolOutcome::err(e);
        }
    }
    let mut out = format!(
        "[ok] 已标注 {} 个屏幕坐标{}\n\
         屏幕：{}x{}（虚拟桌面物理像素，原点 ({}, {})）\n\
         时长：{}ms{}\n\
         dry_run：{}\n",
        points.len(),
        if dry_run { "（dry_run，未绘制）" } else { "" },
        width,
        height,
        origin_x,
        origin_y,
        duration_ms,
        if dry_run {
            ""
        } else {
            "（到时自动关闭；叠加层是透明置顶、鼠标穿透的，不挡操作）"
        },
        dry_run
    );
    for (index, point) in points.iter().enumerate() {
        out.push_str(&format!(
            "{}. ({}, {}){}\n",
            index + 1,
            point.x,
            point.y,
            if point.label.is_empty() {
                String::new()
            } else {
                format!("  标签：{}", point.label)
            }
        ));
    }
    if !dry_run {
        out.push_str("\n提示：叠加层用十字+圆环标出每个点；配合 screenshot/grid 可精确点选。");
    }
    ToolOutcome::ok(out).with_display(format!(
        "标注 {} 个点{}",
        points.len(),
        if dry_run { "（dry_run）" } else { "" }
    ))
}

// ============================================================
// 工具声明
// ============================================================

tool_impl!(
    ListDisplays,
    "list_displays",
    "枚举所有显示器：序号、id、名称、物理/逻辑尺寸、缩放、位置、主屏标记。\n\
     坐标约定：截图和鼠标键盘工具都用物理像素。",
    json!({"type": "object", "properties": {}}),
    list_displays
);

tool_impl!(
    ScreenSize,
    "screen_size",
    "主屏尺寸：物理像素与逻辑像素、缩放比例。配合 screenshot 的坐标换算使用。",
    json!({"type": "object", "properties": {}}),
    screen_size
);

tool_impl!(
    ScreenMetrics,
    "screen_metrics",
    "屏幕度量：虚拟屏幕矩形（所有显示器合并）、每个显示器的物理/逻辑尺寸与缩放、当前光标位置。\n\
     比 screen_size 更全，做多屏/坐标换算（mouse_* 的 logical 参数）时用。",
    json!({"type": "object", "properties": {}}),
    screen_metrics
);

tool_impl!(
    Screenshot,
    "screenshot",
    "截取屏幕并保存为 PNG 文件，返回绝对路径、宽高和缩放比例（默认只回路径，不回二进制，省 token）。\n\
     可以截主屏/指定显示器/指定区域，并把最长边缩到 max_edge（默认取界面设置）。\n\
     默认走 Windows GDI 快速抓屏（BitBlt + GetDIBits，目标 <300ms）；fast=false 或 GDI 不可用时回退 xcap。\n\
     grid=true 时在图上叠加坐标网格：每 100 物理像素细线、每 500 像素粗线，上边/左边有刻度数字，\
     画面上还会标出图像左上角的物理坐标；结合刻度可以直接算出 mouse_click 的物理像素坐标。\n\
     返回文本始终包含 [screenshot:<绝对路径>] 标记（视觉输入依赖它），并给出抓屏/编码/总耗时。\n\
     需要把图像内容发给模型时用 return_base64=true（很占 token，慎用）。",
    json!({
        "type": "object",
        "properties": {
            "display": {"type": "string", "description": "显示器：primary（默认）/ 序号 0..n / id:123 / 名字片段"},
            "region": {
                "type": "object",
                "description": "裁剪区域（相对所选显示器左上角，物理像素）",
                "properties": {
                    "x": {"type": "integer"},
                    "y": {"type": "integer"},
                    "w": {"type": "integer"},
                    "h": {"type": "integer"}
                },
                "required": ["x", "y", "w", "h"]
            },
            "save_path": {"type": "string", "description": "PNG 保存路径；默认存到项目 data/screenshots/"},
            "max_edge": {"type": "integer", "description": "最长边像素上限，0 表示不缩放；默认取设置"},
            "grid": {"type": "boolean", "description": "是否叠加坐标网格（100px/500px 刻度，物理像素坐标），默认 false；想精确点坐标时用"},
            "fast": {"type": "boolean", "description": "是否使用 GDI 快速抓屏（默认 true；false 强制 xcap 兜底路径）"},
            "return_base64": {"type": "boolean", "description": "是否在结果里附带 PNG 的 base64，默认 false"}
        }
    }),
    screenshot
);

tool_impl!(
    ScreenMark,
    "screen_mark",
    "把指定屏幕坐标在真实屏幕上可视化标出（辅助精准点选）。\n\
     参数 points：1-32 个 {x, y, label?}（物理像素）；duration_ms 默认 3000（500-10000）；\n\
     dry_run=true 只返回坐标、不绘制（测试/预检用）。\n\
     Windows 下会创建一个透明、置顶、鼠标穿透的分层窗口，用十字+圆环+编号标出每个点，\
     到时间自动关闭，工具立即返回不阻塞；非 Windows 仅支持 dry_run。",
    json!({
        "type": "object",
        "properties": {
            "points": {
                "type": "array",
                "minItems": 1,
                "maxItems": 32,
                "description": "要标注的点，例如 [{\"x\":100,\"y\":200,\"label\":\"按钮\"}]",
                "items": {
                    "type": "object",
                    "properties": {
                        "x": {"type": "integer", "description": "物理像素横坐标"},
                        "y": {"type": "integer", "description": "物理像素纵坐标"},
                        "label": {"type": "string", "description": "可选短标签（<=12 字符）"}
                    },
                    "required": ["x", "y"]
                }
            },
            "duration_ms": {"type": "integer", "description": "标注显示时长毫秒，默认 3000，范围 500-10000"},
            "dry_run": {"type": "boolean", "description": "只返回坐标、不绘制叠加层（默认 false）"}
        },
        "required": ["points"]
    }),
    screen_mark
);

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_parsing() {
        assert_eq!(parse_region(None).unwrap(), None);
        let r = parse_region(Some(&json!({"x": 10, "y": "20", "w": 100, "h": 50}))).unwrap();
        assert_eq!(
            r,
            Some(Region {
                x: 10,
                y: 20,
                w: 100,
                h: 50
            })
        );
        assert!(parse_region(Some(&json!({"x": 0}))).is_err());
        assert!(parse_region(Some(&json!({"x": 0, "y": 0, "w": 0, "h": 10}))).is_err());
        assert!(parse_region(Some(&json!("nope"))).is_err());
        // width/height 别名
        let alt = parse_region(Some(&json!({"x": 0, "y": 0, "width": 20, "height": 30}))).unwrap();
        assert_eq!(alt.unwrap().w, 20);
    }

    #[test]
    fn display_selector_parsing() {
        assert_eq!(parse_display_selector(None), DisplaySelector::Primary);
        assert_eq!(
            parse_display_selector(Some("primary")),
            DisplaySelector::Primary
        );
        assert_eq!(parse_display_selector(Some("1")), DisplaySelector::Index(1));
        assert_eq!(parse_display_selector(Some("id:42")), DisplaySelector::Id(42));
        assert_eq!(
            parse_display_selector(Some("DISPLAY2")),
            DisplaySelector::Name("DISPLAY2".to_string())
        );
    }

    #[test]
    fn monitor_picking() {
        let metas = vec![
            MonitorMeta {
                index: 0,
                id: 1,
                name: "A".into(),
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
                scale: 1.0,
                primary: false,
                rotation: 0.0,
                frequency: 60.0,
            },
            MonitorMeta {
                index: 1,
                id: 2,
                name: "B".into(),
                x: 1920,
                y: 0,
                width: 2560,
                height: 1440,
                scale: 1.5,
                primary: true,
                rotation: 0.0,
                frequency: 144.0,
            },
        ];
        assert_eq!(pick_monitor(&metas, &DisplaySelector::Primary).unwrap(), 1);
        assert_eq!(pick_monitor(&metas, &DisplaySelector::Index(0)).unwrap(), 0);
        assert!(pick_monitor(&metas, &DisplaySelector::Index(9)).is_err());
        assert_eq!(pick_monitor(&metas, &DisplaySelector::Id(2)).unwrap(), 1);
        assert_eq!(
            pick_monitor(&metas, &DisplaySelector::Name("b".into())).unwrap(),
            1
        );
        assert!(pick_monitor(&metas, &DisplaySelector::Name("zzz".into())).is_err());
    }

    #[test]
    fn logical_size_math() {
        let meta = MonitorMeta {
            index: 0,
            id: 0,
            name: "x".into(),
            x: 0,
            y: 0,
            width: 2560,
            height: 1440,
            scale: 1.5,
            primary: true,
            rotation: 0.0,
            frequency: 60.0,
        };
        assert_eq!(meta.logical_size(), (1707, 960));
    }

    #[test]
    fn virtual_screen_union() {
        let meta = |index: usize, x: i32, y: i32, w: u32, h: u32| MonitorMeta {
            index,
            id: index as u32,
            name: format!("M{index}"),
            x,
            y,
            width: w,
            height: h,
            scale: 1.0,
            primary: index == 0,
            rotation: 0.0,
            frequency: 60.0,
        };
        // 左边一块 1920 宽、右边一块 2560 宽，第二块从 x=1920 开始
        let monitors = vec![meta(0, 0, 0, 1920, 1080), meta(1, 1920, 0, 2560, 1440)];
        assert_eq!(virtual_screen_rect(&monitors), Some((0, 0, 4480, 1440)));
        // 有显示器在原点的左边/上边时，起点要用最小值
        let monitors = vec![meta(0, 0, 0, 1920, 1080), meta(1, -1280, -200, 1280, 1024)];
        assert_eq!(virtual_screen_rect(&monitors), Some((-1280, -200, 3200, 1280)));
        assert_eq!(virtual_screen_rect(&[]), None);
    }

    #[test]
    fn grid_line_positions() {
        // 从 0 开始，每 500 一条：0/500/1000
        assert_eq!(grid_lines(0, 1000, 500), vec![0, 500, 1000]);
        // 起点不是整数倍时从下一个整数倍开始（显示器在 x=1920）
        assert_eq!(grid_lines(1920, 1000, 500), vec![2000, 2500]);
        // 负坐标（左侧扩展屏）也能正确取整；end=80 时 0 也应包含
        assert_eq!(grid_lines(-1920, 2000, 500), vec![-1500, -1000, -500, 0]);
        // step=0 时按 1 处理（防止死循环）：0..=200 共 201 条
        let stepped = grid_lines(0, 200, 0);
        assert_eq!(stepped.len(), 201);
        assert_eq!(stepped.first(), Some(&0));
        assert_eq!(stepped.last(), Some(&200));
        // 100px 细线数量
        assert_eq!(grid_lines(0, 500, 100).len(), 6);
    }

    #[test]
    fn glyph_rendering_changes_pixels() {
        // 数字/负号/逗号都有字形，未知字符没有
        for ch in "0123456789-,".chars() {
            assert!(glyph_3x5(ch).is_some(), "缺少字形: {ch}");
        }
        assert!(glyph_3x5('x').is_none());

        let mut img = image::RgbaImage::from_pixel(80, 30, image::Rgba([0, 0, 0, 255]));
        let before = img.clone();
        draw_text_3x5(&mut img, 4, 4, "500", 2, image::Rgba([255, 255, 255, 255]));
        assert_ne!(img, before, "画数字后像素应变化");

        // 网格绘制：细线/粗线/刻度都要改变像素
        let mut blank = image::RgbaImage::from_pixel(300, 200, image::Rgba([20, 40, 60, 255]));
        let before = blank.clone();
        draw_grid(&mut blank, 0, 0, 600, 400);
        assert_ne!(blank, before, "画网格后像素应变化");
        // 粗线（x=500 处，图上 x≈250）应是橙色系
        let mut found_coarse = false;
        for x in 240..260 {
            for y in 100..120 {
                let px = blank.get_pixel(x, y).0;
                if px[0] > 180 && px[1] < 160 && px[2] < 120 {
                    found_coarse = true;
                }
            }
        }
        assert!(found_coarse, "应能画出粗线（橙色刻度线）");
    }

    #[test]
    fn grid_edge_cases_do_not_panic() {
        // 空图：直接返回，不 panic
        let mut empty = image::RgbaImage::new(0, 0);
        draw_grid(&mut empty, 0, 0, 0, 0);
        draw_grid(&mut empty, 100, 100, 100, 100);
        // 物理尺寸为 0（region 异常）：直接返回
        let mut img = image::RgbaImage::from_pixel(10, 10, image::Rgba([0, 0, 0, 255]));
        draw_grid(&mut img, 0, 0, 0, 0);
        // 负坐标 + 超长标签也不能 panic
        let mut img = image::RgbaImage::from_pixel(50, 40, image::Rgba([0, 0, 0, 255]));
        draw_grid(&mut img, -1920, -200, 3840, 2160);
        // 网格线/文字坐标超出图像边界时跳过而不是 panic
        let mut img = image::RgbaImage::from_pixel(20, 20, image::Rgba([0, 0, 0, 255]));
        draw_text_3x5(&mut img, -100, -100, "123456", 2, image::Rgba([255, 255, 255, 255]));
        draw_text_3x5(&mut img, 15, 15, "999999", 2, image::Rgba([255, 255, 255, 255]));
    }

    #[test]
    fn capture_primary_reports_error_for_invalid_dir() {
        let root = crate::test_support::temp_root("shot-bad-dir");
        let blocker = root.join("blocked");
        std::fs::write(&blocker, b"not a dir").expect("写阻塞文件");
        // 目录位置被文件占用：应返回 Err（中文），而不是 panic
        let result = crate::screen::capture_primary(&blocker);
        assert!(result.is_err(), "非法目录应返回错误");
        let message = result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(message.contains("创建截图目录"), "错误信息应说明目录问题: {message}");
    }

    #[test]
    fn display_selector_errors() {
        let meta = |index: usize, name: &str| MonitorMeta {
            index,
            id: index as u32,
            name: name.to_string(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            scale: 1.0,
            primary: index == 0,
            rotation: 0.0,
            frequency: 60.0,
        };
        let metas = vec![meta(0, "A"), meta(1, "B")];
        // 序号越界 / id 不存在 / 名字不存在：都要给出中文错误而不是 panic
        assert!(pick_monitor(&metas, &DisplaySelector::Index(5)).is_err());
        assert!(pick_monitor(&metas, &DisplaySelector::Id(99)).is_err());
        assert!(pick_monitor(&metas, &DisplaySelector::Name("zzz".into())).is_err());
        assert!(pick_monitor(&[], &DisplaySelector::Primary).is_err());
        // region 非法参数
        assert!(parse_region(Some(&json!({"x": 1, "y": 2, "w": 0, "h": 3}))).is_err());
        assert!(parse_region(Some(&json!({"x": 1, "y": 2}))).is_err());
    }

    /// 真实截屏验证（手动运行 `cargo test -p neosonia-tools -- --ignored`）。
    #[tokio::test]
    #[ignore]
    async fn screenshot_writes_png_to_disk() {
        let root = crate::test_support::temp_root("screenshot");
        let (ctx, _host) = crate::test_support::fake_ctx(&root);
        let out = screenshot(
            &ctx,
            json!({"display": "primary", "return_base64": false}),
        )
        .await;
        assert!(out.ok, "{}", out.content);
        let marker = out
            .content
            .lines()
            .find(|l| l.starts_with("[screenshot:") && l.ends_with(']'))
            .expect("截图结果必须包含 [screenshot:<绝对路径>] 标记");
        let marked_path =
            std::path::PathBuf::from(marker.trim_start_matches("[screenshot:").trim_end_matches(']'));
        assert!(
            marked_path.is_absolute(),
            "标记里应是绝对路径: {}",
            marked_path.display()
        );
        let path_line = out
            .content
            .lines()
            .find(|l| l.starts_with("路径："))
            .expect("结果里应有路径");
        let path = std::path::PathBuf::from(path_line.trim_start_matches("路径：").trim());
        assert_eq!(path, marked_path, "标记路径应与路径行一致");
        assert!(path.is_file(), "截图文件应存在: {}", path.display());
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.len() > 1000, "PNG 太小: {}", bytes.len());
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "应为 PNG 文件");
        println!("screenshot: {} ({} 字节)", path.display(), bytes.len());

        // grid=true：必须带标记 + 网格说明，并生成另一张图
        let grid_out = screenshot(
            &ctx,
            json!({"display": "primary", "grid": true, "return_base64": false}),
        )
        .await;
        assert!(grid_out.ok, "{}", grid_out.content);
        assert!(
            grid_out.content.contains("[screenshot:"),
            "grid 截图也必须带 [screenshot:] 标记: {}",
            grid_out.content
        );
        assert!(
            grid_out.content.contains("网格：间距 100px"),
            "结果里应说明网格间距/原点/scale: {}",
            grid_out.content
        );
        let grid_path = grid_out
            .content
            .lines()
            .find(|l| l.starts_with("路径："))
            .map(|l| l.trim_start_matches("路径：").trim().to_string())
            .expect("grid 截图应有路径");
        assert!(std::path::Path::new(&grid_path).is_file());
        println!("GRID_SCREENSHOT_PATH={grid_path}");
        println!("=== grid screenshot result ===\n{}", grid_out.content);

        // app 截图快捷键用的公共函数（不经 Tool trait）
        let out_dir = root.join("data").join("screenshots");
        let app_path = crate::screen::capture_primary(&out_dir).expect("capture_primary 应成功");
        assert!(app_path.is_file(), "capture_primary 文件应存在: {}", app_path.display());
        let name = app_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        assert!(name.starts_with("shot-") && name.ends_with(".png"), "文件名应形如 shot-<ms>.png: {name}");
        let app_bytes = std::fs::read(&app_path).unwrap();
        assert_eq!(&app_bytes[..8], b"\x89PNG\r\n\x1a\n", "capture_primary 应是合法 PNG");
        println!("capture_primary: {} ({} 字节)", app_path.display(), app_bytes.len());
    }

    #[test]
    fn mark_points_parsing() {
        let points = parse_mark_points(Some(&json!([
            {"x": 10, "y": 20},
            {"x": "300", "y": 400, "label": "  按钮  "},
            {"x": -5, "y": 0, "label": "abcdefghijklmnop"}
        ])))
        .unwrap();
        assert_eq!(points.len(), 3);
        assert_eq!(points[0], MarkPoint { x: 10, y: 20, label: String::new() });
        assert_eq!(points[1].x, 300);
        assert_eq!(points[1].label, "按钮");
        assert_eq!(points[2].x, -5);
        // truncate_chars 超长时保留 12 个字符并追加省略号（共 13 字符）
        assert!(
            points[2].label.chars().count() <= 13,
            "label 应截断到 12 字符+省略号"
        );
        assert!(points[2].label.starts_with("abcdefghijkl"), "{}", points[2].label);
        // 上限 32
        let many: Vec<Value> = (0..32).map(|i| json!({"x": i, "y": i})).collect();
        assert!(parse_mark_points(Some(&json!(many))).is_ok());
        let many: Vec<Value> = (0..33).map(|i| json!({"x": i, "y": i})).collect();
        assert!(parse_mark_points(Some(&json!(many))).is_err());
        // 错误路径
        assert!(parse_mark_points(None).is_err());
        assert!(parse_mark_points(Some(&json!([]))).is_err());
        assert!(parse_mark_points(Some(&json!([{"y": 1}]))).is_err());
        assert!(parse_mark_points(Some(&json!([{"x": 1}]))).is_err());
        assert!(parse_mark_points(Some(&json!(["oops"]))).is_err());
        assert!(parse_mark_points(Some(&json!({"x": 1, "y": 2}))).is_err());
    }

    #[tokio::test]
    async fn screen_mark_dry_run_does_not_draw() {
        let root = crate::test_support::temp_root("screen-mark");
        let (ctx, _host) = crate::test_support::fake_ctx(&root);
        // 只走 dry_run：绝不创建叠加层/不碰屏幕
        let out = screen_mark(
            &ctx,
            json!({
                "points": [{"x": 100, "y": 200}, {"x": 300, "y": 400, "label": "按钮"}],
                "duration_ms": 1500,
                "dry_run": true
            }),
        )
        .await;
        assert!(out.ok, "{}", out.content);
        println!("screen_mark dry_run:\n{}", out.content);
        assert!(out.content.contains("dry_run：true"), "{}", out.content);
        assert!(out.content.contains("1. (100, 200)"), "{}", out.content);
        assert!(out.content.contains("2. (300, 400)"), "{}", out.content);
        assert!(out.content.contains("标签：按钮"), "{}", out.content);
        assert!(out.content.contains("时长：1500ms"), "{}", out.content);
        assert!(out.content.contains("屏幕："), "{}", out.content);
        // 参数校验
        let bad = screen_mark(&ctx, json!({"points": [], "dry_run": true})).await;
        assert!(!bad.ok);
        let bad = screen_mark(&ctx, json!({"points": [{"x": 1, "y": 2}], "duration_ms": 1, "dry_run": true})).await;
        assert!(bad.ok, "{}", bad.content);
        assert!(bad.content.contains("时长：500ms"), "duration 应 clamp 到 500ms: {}", bad.content);
        let bad = screen_mark(&ctx, json!({"points": [{"x": 1, "y": 2}], "duration_ms": 999999, "dry_run": true})).await;
        assert!(bad.ok, "{}", bad.content);
        assert!(bad.content.contains("时长：10000ms"), "duration 应 clamp 到 10000ms: {}", bad.content);
    }
}
