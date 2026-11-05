//! Firefox Nova 设计令牌 -> fenestra `Theme`。
//!
//! 色板来自 `docs/ui-design-palette.json`（调研自 Mozilla 的 Nova tokens），
//! 编译期嵌入，运行时解析一次。
//!
//! 约定：本文件只做"令牌 -> 像素"的映射，不含业务逻辑。

use std::collections::BTreeMap;
use std::sync::OnceLock;

use fenestra::prelude::*;
use serde::Deserialize;

use neosonia_core::config::ThemeMode;

/// 一套色板（dark / light 各一份）。
#[derive(Clone, Debug, Deserialize)]
pub struct Palette {
    pub bg: String,
    pub surface: String,
    pub surface_raised: String,
    pub overlay: String,
    pub border_subtle: String,
    pub border_strong: String,
    pub text_primary: String,
    pub text_secondary: String,
    pub text_tertiary: String,
    pub accent: String,
    pub accent_hover: String,
    pub accent_pressed: String,
    pub on_accent: String,
    pub brand_from: String,
    pub brand_to: String,
    pub success: String,
    pub success_bg: String,
    pub warning: String,
    pub warning_bg: String,
    pub danger: String,
    pub danger_bg: String,
    pub selection: String,
    pub focus_ring: String,
    pub scrollbar_thumb: String,
    pub scrollbar_track: String,
    pub scrim: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Motion {
    pub instant: u64,
    pub fast: u64,
    pub base: u64,
    pub slow: u64,
    pub sheet: u64,
    pub stream: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RadiusTokens {
    pub none: f32,
    pub xs: f32,
    pub sm: f32,
    pub md: f32,
    pub lg: f32,
    pub xl: f32,
    pub pill: f32,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Typography {
    pub caption: f32,
    pub label: f32,
    pub secondary: f32,
    pub body: f32,
    pub title: f32,
    pub heading: f32,
    pub display: f32,
}

#[derive(Clone, Debug, Deserialize)]
pub struct NovaTokens {
    pub dark: Palette,
    pub light: Palette,
    pub spacing: Vec<f32>,
    pub radius: RadiusTokens,
    pub motion_ms: Motion,
    pub typography_px: Typography,
}

const PALETTE_JSON: &str = include_str!("../../../docs/ui-design-palette.json");

static TOKENS: OnceLock<NovaTokens> = OnceLock::new();

/// 全局令牌（首次访问时解析；JSON 是编译期嵌入的，不会失败）。
pub fn tokens() -> &'static NovaTokens {
    TOKENS.get_or_init(|| {
        serde_json::from_str(PALETTE_JSON).expect("docs/ui-design-palette.json 解析失败")
    })
}

/// 间距：`sp(4)` = 16px。索引超出时取最后一档。
pub fn sp(i: usize) -> f32 {
    let t = tokens();
    t.spacing.get(i).copied().unwrap_or_else(|| *t.spacing.last().unwrap_or(&32.0))
}

pub fn radius(name: &str) -> f32 {
    let r = &tokens().radius;
    match name {
        "xs" => r.xs,
        "sm" => r.sm,
        "md" => r.md,
        "lg" => r.lg,
        "xl" => r.xl,
        "pill" => r.pill,
        _ => r.none,
    }
}

/// 常用半径常量。
pub const R_CARD: f32 = 12.0;
pub const R_PANEL: f32 = 16.0;
pub const R_CONTROL: f32 = 8.0;
pub const R_CHIP: f32 = 6.0;

/// 十六进制颜色（`#rrggbb` / `#rrggbbaa`）。
pub fn hex(s: &str) -> Color {
    let t = s.trim().trim_start_matches('#');
    let parse = |i: usize| u8::from_str_radix(&t[i..i + 2], 16).unwrap_or(0);
    match t.len() {
        6 => Color::from_rgba8(parse(0), parse(2), parse(4), 255),
        8 => Color::from_rgba8(parse(0), parse(2), parse(4), parse(6)),
        _ => Color::from_rgba8(0, 0, 0, 255),
    }
}

pub fn palette(mode: ThemeMode, system_dark: bool) -> &'static Palette {
    let dark = match mode {
        ThemeMode::Dark => true,
        ThemeMode::Light => false,
        ThemeMode::System => system_dark,
    };
    let t = tokens();
    if dark { &t.dark } else { &t.light }
}

/// 构造 Nova 主题：以品牌紫的 OKLCH 色相为基础生成 fenestra 的色阶，
/// 然后用 Nova 精确色值覆盖所有"看得见的"表面色。
pub fn nova_theme(mode: ThemeMode, system_dark: bool, font_scale: f64) -> Theme {
    let p = palette(mode, system_dark);
    let dark = matches!(mode, ThemeMode::Dark)
        || (matches!(mode, ThemeMode::System) && system_dark);
    let accent_color = hex(&p.accent);
    let [_l, _c, hue] = oklch_of(accent_color);

    let mut t = if dark { Theme::dark() } else { Theme::light() };
    t = Theme::from_accent(hue, if dark { Mode::Dark } else { Mode::Light });

    t.bg = hex(&p.bg);
    t.surface = hex(&p.surface);
    t.surface_raised = hex(&p.surface_raised);
    t.element = hex(&p.overlay);
    t.element_hover = hex(&p.selection);
    t.element_active = hex(&p.border_strong);

    t.border_subtle = hex(&p.border_subtle);
    t.border = hex(&p.border_subtle);
    t.border_strong = hex(&p.border_strong);

    t.text = hex(&p.text_primary);
    t.text_muted = hex(&p.text_secondary);
    t.text_subtle = hex(&p.text_tertiary);
    t.text_disabled = hex(&p.text_tertiary).with_alpha(0.55);

    t.accent = accent_color;
    t.accent_hover = hex(&p.accent_hover);
    t.accent_active = hex(&p.accent_pressed);
    t.accent_bg = accent_color.with_alpha(if dark { 0.18 } else { 0.12 });
    t.accent_border = accent_color.with_alpha(if dark { 0.45 } else { 0.35 });
    t.accent_text = accent_color;
    t.on_accent = hex(&p.on_accent);

    t.with_text_scale(font_scale.clamp(0.75, 1.6) as f32)
}

// ============================================================
// 语义色快捷访问
// ============================================================

/// 当前模式下的语义色集合，避免各处反复解析 JSON。
#[derive(Clone, Copy, Debug)]
pub struct UiColors {
    pub bg: Color,
    pub surface: Color,
    pub surface_raised: Color,
    pub overlay: Color,
    pub border_subtle: Color,
    pub border_strong: Color,
    pub text: Color,
    pub text_muted: Color,
    pub text_subtle: Color,
    pub accent: Color,
    pub accent_hover: Color,
    pub on_accent: Color,
    pub success: Color,
    pub success_bg: Color,
    pub warning: Color,
    pub warning_bg: Color,
    pub danger: Color,
    pub danger_bg: Color,
    pub selection: Color,
    pub focus_ring: Color,
    pub scrollbar_thumb: Color,
    pub scrim: Color,
    pub brand_from: Color,
    pub brand_to: Color,
}

pub fn colors(theme: &Theme) -> UiColors {
    let dark = matches!(theme.mode, Mode::Dark);
    let p = palette(if dark { ThemeMode::Dark } else { ThemeMode::Light }, dark);
    UiColors {
        bg: hex(&p.bg),
        surface: hex(&p.surface),
        surface_raised: hex(&p.surface_raised),
        overlay: hex(&p.overlay),
        border_subtle: hex(&p.border_subtle),
        border_strong: hex(&p.border_strong),
        text: hex(&p.text_primary),
        text_muted: hex(&p.text_secondary),
        text_subtle: hex(&p.text_tertiary),
        accent: hex(&p.accent),
        accent_hover: hex(&p.accent_hover),
        on_accent: hex(&p.on_accent),
        success: hex(&p.success),
        success_bg: hex(&p.success_bg),
        warning: hex(&p.warning),
        warning_bg: hex(&p.warning_bg),
        danger: hex(&p.danger),
        danger_bg: hex(&p.danger_bg),
        selection: hex(&p.selection),
        focus_ring: hex(&p.focus_ring),
        scrollbar_thumb: hex(&p.scrollbar_thumb),
        scrim: hex(&p.scrim),
        brand_from: hex(&p.brand_from),
        brand_to: hex(&p.brand_to),
    }
}

/// 半透明版本（用于浮窗半透明、分隔层）。
pub fn alpha(c: Color, a: f32) -> Color {
    c.with_alpha(a)
}

/// 两色按比例混合（t = 0 取 a，1 取 b），sRGB 空间线性插值。
pub fn mix(a: Color, b: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    let ca = a.components;
    let cb = b.components;
    let f = |i: usize| (ca[i] * (1.0 - t) + cb[i] * t) as f32;
    Color::new([f(0), f(1), f(2), f(3)])
}

/// 设计稿里的 2px 品牌发丝：紫 -> 橙。只允许用在这一处（导航下划线/头像描边）。
pub fn brand_line_width() -> f32 {
    2.0
}

/// 字号阶梯（px -> TextSize）。fenestra 的 TextSize 只有固定档位，这里做映射。
pub fn text_size(px: f32) -> TextSize {
    match px {
        p if p <= 11.0 => TextSize::Xs,
        p if p <= 12.0 => TextSize::Sm,
        p if p <= 13.0 => TextSize::Sm,
        p if p <= 14.0 => TextSize::Base,
        p if p <= 16.0 => TextSize::Lg,
        p if p <= 20.0 => TextSize::Xl,
        p if p <= 24.0 => TextSize::Xl2,
        _ => TextSize::Xl3,
    }
}

/// 键盘/动效时长。
pub fn motion_ms(name: &str) -> u64 {
    let m = &tokens().motion_ms;
    match name {
        "instant" => m.instant,
        "fast" => m.fast,
        "base" => m.base,
        "slow" => m.slow,
        "sheet" => m.sheet,
        "stream" => m.stream,
        _ => m.base,
    }
}

/// 间距常量（逻辑像素），与设计稿一致。
pub mod space {
    pub const XS: f32 = 4.0;
    pub const SM: f32 = 8.0;
    pub const MD: f32 = 12.0;
    pub const LG: f32 = 16.0;
    pub const XL: f32 = 24.0;
    pub const XXL: f32 = 32.0;
}

/// 浮窗布局常量。
pub mod dims {
    /// 面板宽度（设计稿 424）。
    pub const PANEL_WIDTH: f64 = 424.0;
    /// 标题栏高度。
    pub const HEADER_H: f32 = 48.0;
    /// 导航条高度。
    pub const NAV_H: f32 = 40.0;
    /// 底部输入区高度。
    pub const COMPOSER_H: f32 = 112.0;
    /// 控件高度。
    pub const CONTROL_H: f32 = 32.0;
    /// 屏边留白。
    pub const EDGE_MARGIN: f64 = 12.0;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_parses() {
        let t = tokens();
        assert!(t.dark.bg.starts_with('#'));
        assert!(t.spacing.len() >= 5);
        assert!(t.radius.lg >= 12.0);
    }

    #[test]
    fn hex_roundtrip() {
        let c = hex("#b89cff");
        let rgba = c.to_rgba8();
        assert_eq!((rgba.r, rgba.g, rgba.b, rgba.a), (184, 156, 255, 255));
        assert_eq!(hex("#00000080").to_rgba8().a, 128);
    }

    #[test]
    fn theme_uses_nova_surfaces() {
        let th = nova_theme(ThemeMode::Dark, true, 1.0);
        let c = colors(&th);
        assert_eq!(c.bg, hex("#121114"));
        assert_eq!(c.accent, hex("#b89cff"));
    }

    #[test]
    fn mix_endpoints() {
        let a = hex("#000000");
        let b = hex("#ffffff");
        assert_eq!(mix(a, b, 0.0), a);
        assert_eq!(mix(a, b, 1.0), b);
    }
}
