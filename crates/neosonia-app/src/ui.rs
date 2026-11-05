//! 浮窗整体布局：标题栏（拖拽区）/ 导航 / 内容区 / 输入区。
//!
//! 布局对齐 `docs/ui-design.md`：面板 424 宽、r16、标题栏 48 + 导航 40 + 单列内容 + 输入区。
//! 动效克制：导航/按钮用颜色过渡，新消息与历史面板用 enter 过渡，Toast 淡入。

use fenestra::prelude::*;

use crate::chat;
use crate::panels;
use crate::state::{AppState, Msg, NavPage, NoticeLevel};
use crate::theme::{UiColors, dims, space, R_CARD, R_CHIP, R_PANEL};

// ============================================================
// 品牌标记
// ============================================================

/// 项目 logo 像素（编译期嵌入，只解码一次）。
fn logo_pixels() -> Option<(u32, u32, Vec<u8>)> {
    static LOGO: std::sync::OnceLock<Option<(u32, u32, Vec<u8>)>> = std::sync::OnceLock::new();
    LOGO.get_or_init(|| {
        static PNG: &[u8] = include_bytes!("../../../assets/icon-64.png");
        image::load_from_memory(PNG).ok().map(|img| {
            let rgba = img.to_rgba8();
            let (w, h) = rgba.dimensions();
            (w, h, rgba.into_raw())
        })
    })
    .clone()
}

fn brand_mark(c: &UiColors) -> Element<Msg> {
    match logo_pixels() {
        Some((w, h, px)) => col()
            .w(22.0)
            .h(22.0)
            .rounded(6.0)
            .overflow_hidden()
            .children((image_rgba8(w, h, px).w(22.0).h(22.0),))
            .into(),
        None => col()
            .w(22.0)
            .h(22.0)
            .rounded(6.0)
            .bg(c.accent)
            .items_center()
            .justify_center()
            .children((text("N").size(TextSize::Sm).weight(Weight::Semibold).color(c.on_accent),))
            .into(),
    }
}

// ============================================================
// 标题栏
// ============================================================

fn header(st: &AppState, c: &UiColors) -> Element<Msg> {
    let busy = st.status.busy;
    let state_color = if busy { c.warning } else { c.success };
    let phase = if busy && st.status.phase.is_empty() {
        "执行中".to_string()
    } else if st.status.phase.is_empty() {
        "就绪".to_string()
    } else {
        st.status.phase.clone()
    };

    let mut right: Vec<Element<Msg>> = Vec::new();
    if busy {
        right.push(
            button("停止")
                .size(ControlSize::Sm)
                .variant(ButtonVariant::Danger)
                .on_click(Msg::CancelTurn)
                .into(),
        );
    }
    if st.pending_risk.is_some() {
        right.push(
            row()
                .px(space::SM)
                .h(24.0)
                .rounded(R_CHIP)
                .bg(c.warning.with_alpha(0.18))
                .items_center()
                .children((text("待确认").size(TextSize::Xs).color(c.warning),))
                .into(),
        );
    }
    if st.subagent_total > 0 {
        right.push(
            row()
                .px(space::SM)
                .h(24.0)
                .gap(4.0)
                .rounded(R_CHIP)
                .bg(c.accent.with_alpha(0.16))
                .items_center()
                .children((
                    {
                        let accent = c.accent;
                        icons::lucide::by_name("user")
                            .map(move |e| {
                                e.w(13.0)
                                    .h(13.0)
                                    .themed(move |_t, s| s.color(accent))
                            })
                            .unwrap_or_else(|| col().w(0.0).into())
                    },
                    text(format!(
                        "{}",
                        if st.subagent_active > 0 {
                            st.subagent_active
                        } else {
                            st.subagent_total
                        }
                    ))
                    .size(TextSize::Xs)
                    .color(c.accent),
                ))
                .into(),
        );
    }
    right.push(
        icon_button(icons::lucide::share_2())
            .size(ControlSize::Sm)
            .variant(ButtonVariant::Ghost)
            .label("连接其他设备")
            .on_click(Msg::OpenPage(NavPage::Remote))
            .into(),
    );
    right.push(
        icon_button(icons::lucide::clock())
            .size(ControlSize::Sm)
            .variant(ButtonVariant::Ghost)
            .label("对话历史")
            .on_click(Msg::HistoryToggle)
            .into(),
    );
    right.push(
        icon_button(icons::lucide::eye())
            .size(ControlSize::Sm)
            .variant(ButtonVariant::Ghost)
            .label("截图发给模型")
            .on_click(Msg::ScreenshotHotkey)
            .into(),
    );
    right.push(
        icon_button(icons::x())
            .size(ControlSize::Sm)
            .variant(ButtonVariant::Ghost)
            .label("隐藏窗口")
            .on_click(Msg::HideWindow)
            .into(),
    );

    row()
        .w_full()
        .h(dims::HEADER_H)
        .px(space::MD)
        .gap(space::SM)
        .items_center()
        .bg(c.surface)
        .border_bottom(1.0, c.border_subtle)
        .on_drag(|_, _| Some(Msg::DragWindow))
        .children((
            brand_mark(c),
            col()
                .grow()
                .min_w(0.0)
                .min_h(0.0)
                .overflow_hidden()
                .gap(1.0)
                .children((
                    row().gap(space::XS).items_center().children((
                        text("NeoSonia")
                            .size(TextSize::Lg)
                            .weight(Weight::Semibold)
                            .color(c.text),
                        if busy {
                            // 执行中：脉冲点（keyframes 让帧持续刷新，有明显的"在工作"反馈）
                            col()
                                .w(6.0)
                                .h(6.0)
                                .rounded_full()
                                .bg(state_color)
                                .keyframes(
                                    Keyframes::new(1100.0)
                                        .stop(0.0, |s| s.opacity(0.25))
                                        .stop(0.5, |s| s.opacity(1.0))
                                        .stop(1.0, |s| s.opacity(0.25)),
                                )
                                .into()
                        } else {
                            let dot: Element<Msg> =
                                col().w(6.0).h(6.0).rounded_full().bg(state_color).into();
                            dot
                        },
                    )),
                    // 执行命令时 phase 可能很长（如 "正在执行命令: $roots = @(...)"）：
                    // 窄窗口下会折成两行、溢出固定高度 header 压到导航上（用户截图）。
                    // 按可显示宽度保守截断为 14 个字符，保持单行。
                    text(neosonia_core::util::truncate_chars(&phase, 14))
                        .size(TextSize::Xs)
                        .color(c.text_subtle),                )),
            row().gap(space::XS).items_center().children(right),
        ))
        .into()
}

// ============================================================
// 导航
// ============================================================

fn nav(st: &AppState, c: &UiColors) -> Element<Msg> {
    let mut tabs: Vec<Element<Msg>> = Vec::new();
    for page in NavPage::all() {
        let active = st.page == page;
        let fg = if active { c.text } else { c.text_muted };
        let mut parts: Vec<Element<Msg>> = vec![text(page.label()).size(TextSize::Sm).color(fg).into()];
        if page == NavPage::Mail && st.mail.unread > 0 {
            parts.push(
                row()
                    .px(5.0)
                    .rounded(R_CHIP)
                    .bg(c.accent)
                    .items_center()
                    .children((text(st.mail.unread.to_string())
                        .size(TextSize::Xs)
                        .color(c.on_accent),))
                    .into(),
            );
        }
        let content = row()
            .px(space::SM)
            .h(32.0)
            .gap(space::XS)
            .items_center()
            .rounded(6.0)
            .transition(Transition::colors())
            .on_click(Msg::Nav(page))
            .children(parts);
        let content = if active { content.bg(c.selection) } else { content };
        let underline: Element<Msg> = if active {
            col()
                .w_full()
                .h(2.0)
                .bg(c.accent)
                .rounded_full()
                .transition(Transition::colors())
                .into()
        } else {
            col().w_full().h(2.0).into()
        };
        tabs.push(col().gap(0.0).items_center().children((content, underline)).into());
    }

    // 模式 chip：一眼看到当前是 日常 / 编程 / 计算机使用（点击去设置页切换）
    let mode = match st.agent_mode.as_str() {
        "coding" => "编程",
        "computer_use" => "计算机使用",
        _ => "日常",
    };
    let mode_chip: Element<Msg> = row()
        .px(space::SM)
        .h(22.0)
        .rounded(R_CHIP)
        .bg(c.accent.with_alpha(0.14))
        .items_center()
        .on_click(Msg::OpenPage(NavPage::Settings))
        .children((text(mode).size(TextSize::Xs).color(c.accent),))
        .into();

    row()
        .w_full()
        .h(dims::NAV_H)
        .px(8.0)
        .gap(space::XS)
        .items_center()
        .bg(c.surface)
        .border_bottom(1.0, c.border_subtle)
        .children({
            let mut all = tabs;
            all.push(spacer());
            all.push(mode_chip);
            all
        })
        .into()
}

// ============================================================
// 通知条
// ============================================================

fn toast(st: &AppState, c: &UiColors) -> Element<Msg> {
    let Some((text_, level, _)) = &st.toast else {
        return col().h(0.0).into();
    };
    let color = match level {
        NoticeLevel::Info => c.accent,
        NoticeLevel::Success => c.success,
        NoticeLevel::Warning => c.warning,
        NoticeLevel::Error => c.danger,
    };
    row()
        .w_full()
        .px(space::MD)
        .py(6.0)
        .bg(color.with_alpha(0.10))
        .border_bottom(1.0, color.with_alpha(0.35))
        .enter(Transition::all())
        .id("toast")
        .children((text(neosonia_core::util::truncate_chars(text_, 80))
            .size(TextSize::Xs)
            .color(color),))
        .into()
}

// ============================================================
// 内容区
// ============================================================

fn content(st: &AppState, c: &UiColors) -> Element<Msg> {
    // 对话页里的"历史"覆盖层
    if st.page == NavPage::Chat && st.history.open {
        return col()
            .w_full()
            .grow()
            .overflow_hidden()
            .enter(Transition::all())
            .id("history-overlay")
            .children((panels::history::view(st, c),))
            .into();
    }
    let body: Element<Msg> = match st.page {
        NavPage::Chat => chat::view(st, c),
        NavPage::Mail => panels::mail::view(st, c),
        NavPage::Memory => panels::memory::view(st, c),
        NavPage::Persona => panels::persona::view(st, c),
        NavPage::Settings => panels::settings::view(st, c),
        NavPage::History => panels::history::view(st, c),
        NavPage::Remote => panels::remote::view(st, c),
    };
    col().w_full().grow().overflow_hidden().children((body,)).into()
}

// ============================================================
// 审查确认弹窗（风险 1-4 分）
// ============================================================

fn risk_modal(st: &AppState, c: &UiColors) -> Element<Msg> {
    let Some(p) = st.pending_risk.as_ref() else {
        return col().h(0.0).into();
    };
    let (color, level_text) = match p.level {
        1 => (c.success, "低风险"),
        2 => (c.accent, "需确认"),
        3 => (c.warning, "较高风险"),
        _ => (c.danger, "高风险"),
    };
    let call_id = p.call_id.clone();

    let mut buttons: Vec<Element<Msg>> = vec![
        button("允许这次")
            .size(ControlSize::Sm)
            .on_click(Msg::RiskDecision {
                call_id: call_id.clone(),
                decision: crate::state::RiskChoice::Approve,
            })
            .into(),
        button("拒绝")
            .size(ControlSize::Sm)
            .variant(ButtonVariant::Ghost)
            .on_click(Msg::RiskDecision {
                call_id: call_id.clone(),
                decision: crate::state::RiskChoice::Deny,
            })
            .into(),
    ];
    if p.level >= 3 {
        buttons.push(
            button("拒绝并 1 天内拒绝同类")
                .size(ControlSize::Sm)
                .variant(ButtonVariant::Danger)
                .on_click(Msg::RiskDecision {
                    call_id: call_id.clone(),
                    decision: crate::state::RiskChoice::DenyClass,
                })
                .into(),
        );
    }

    let card = col()
        .w_full()
        .p(space::LG)
        .gap(space::SM)
        .rounded(R_PANEL)
        .bg(c.surface_raised)
        .border(1.0, color)
        .children((
            row()
                .gap(space::XS)
                .items_center()
                .children((
                    col().w(8.0).h(8.0).rounded_full().bg(color),
                    text(format!("需要你确认 · 风险 {}/4 · {level_text}", p.level))
                        .size(TextSize::Sm)
                        .weight(Weight::Semibold)
                        .color(c.text),
                )),
            text(format!("工具: {}", p.tool)).size(TextSize::Xs).mono().color(c.text_muted),
            text(neosonia_core::util::truncate_chars(&p.args, 260))
                .size(TextSize::Xs)
                .mono()
                .color(c.text_subtle),
            text(neosonia_core::util::truncate_chars(&p.reason, 200))
                .size(TextSize::Sm)
                .color(c.text),
            text(format!("同类操作: {}", neosonia_core::util::truncate_chars(&p.class, 80)))
                .size(TextSize::Xs)
                .color(c.text_subtle),
            if p.emphasized {
                let warn: Element<Msg> = row()
                    .w_full()
                    .p(space::SM)
                    .rounded(R_CARD)
                    .bg(c.danger.with_alpha(0.14))
                    .border(1.0, c.danger.with_alpha(0.45))
                    .children((
                        text("这一操作影响面较大，请确认你了解它的后果。").size(TextSize::Xs).color(c.danger),
                    ))
                    .into();
                warn
            } else {
                col().h(0.0).into()
            },
            row().w_full().gap(space::XS).items_center().children(buttons),
            text("可在 设置 → 命令审查 里关闭审查（关闭 = 完全权限）")
                .size(TextSize::Xs)
                .color(c.text_subtle),
        ));

    col()
        .absolute()
        .top(0.0)
        .left(0.0)
        .w_full()
        .h_full()
        .bg(c.bg.with_alpha(0.82))
        .items_center()
        .justify_center()
        .px(space::LG)
        .on_click(Msg::Noop)
        .children((card,))
        .into()
}

/// 顶部 2px 加载条：忙碌/加载时来回滑动，避免"只是卡住"。
fn loading_bar(st: &AppState, c: &UiColors) -> Element<Msg> {
    if !st.status.busy {
        return col().h(0.0).into();
    }
    row()
        .w_full()
        .h(2.0)
        .bg(c.accent.with_alpha(0.15))
        .children((
            col()
                .h(2.0)
                .w(120.0)
                .bg(c.accent)
                .keyframes(
                    Keyframes::new(1200.0)
                        .stop(0.0, |s| s.translate(0.0, 0.0))
                        .stop(0.5, |s| s.translate(280.0, 0.0))
                        .stop(1.0, |s| s.translate(0.0, 0.0)),
                ),
        ))
        .into()
}

// ============================================================
// 根
// ============================================================

/// 切页两段式加载提示：目标页第一次构建较重时，先给用户一个"正在加载"的反馈。
fn page_loading(_st: &AppState, c: &UiColors) -> Element<Msg> {
    col()
        .absolute()
        .left(0.0)
        .top(0.0)
        .w_full()
        .h_full()
        .items_center()
        .justify_center()
        .bg(c.scrim)
        .children((
            row()
                .gap(8.0)
                .items_center()
                .rounded(12.0)
                .bg(c.surface_raised)
                .border(1.0, c.border_strong)
                .p(14.0)
                .children((text("正在加载…").size(TextSize::Sm).color(c.text),)),
        ))
        .into()
}
pub fn root(st: &AppState, c: &UiColors) -> Element<Msg> {
    let opacity = st.settings.ui.opacity.clamp(0.55, 1.0) as f32;
    let bg = if opacity >= 0.999 { c.bg } else { c.bg.with_alpha(opacity) };

    let mut children: Vec<Element<Msg>> = vec![header(st, c), nav(st, c)];
    children.push(loading_bar(st, c));
    children.push(toast(st, c));
    // 页面切换过渡：按时间戳手工插值，**只播一次**。
    // （不能用 Keyframes——它是循环动画，会一直闪；也不能用 Transition——每帧重建元素会反复重启。）
    let (fade, slide) = if st.page_anim_start_ms > 0 {
        let elapsed = (neosonia_core::util::now_ms() - st.page_anim_start_ms).max(0) as f32;
        let t = (elapsed / 180.0).clamp(0.0, 1.0);
        // ease-out：起步快、收尾稳
        let e = 1.0 - (1.0 - t) * (1.0 - t);
        (0.35 + 0.65 * e, (1.0 - e) * 6.0)
    } else {
        (1.0, 0.0)
    };
    children.push(
        col()
            .w_full()
            .grow()
            .min_h(0.0)
            .opacity(fade)
            .translate(0.0, slide)
            .children((content(st, c),)),
    );
    if st.page == NavPage::Chat && !st.history.open {
        children.push(chat::composer(st, c));
    }
    // 切页两段式加载提示（盖在页面上、弹窗之下）
    if st.page_pending.is_some() {
        children.push(page_loading(st, c));
    }
    // 审查确认弹窗（有就盖在最上层）
    if st.pending_risk.is_some() {
        children.push(risk_modal(st, c));
    }

    col()
        .w_full()
        .h_full()
        .bg(bg)
        .rounded(R_PANEL)
        .border(1.0, c.border_subtle)
        .overflow_hidden()
        .on_file_drop(|p| Msg::AttachmentsPicked(vec![p.to_path_buf()]))
        .children(children)
        .into()
}
