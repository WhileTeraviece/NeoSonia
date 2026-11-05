//! 记忆面板：搜索、统计、固定块编辑、事实列表。
//!
//! 数据全部来自 `AppState`（`memory_items` / `memory_blocks` / `memory_query`），
//! `view` 里不做 IO、不做重计算；删除与搜索通过 `Msg` 交给主循环。
//!
//! 视觉对齐 `docs/ui-design.md`：单列卡片、`surface_raised` + 1px 发丝边、
//! 圆角 12、状态"色点 + 文字"，零 emoji。

use fenestra::prelude::*;
use neosonia_core::model::MemoryItem;

use crate::state::{AppState, Msg};
use crate::theme::{space, UiColors, R_CARD, R_CHIP};

const PANEL_ID: &str = "panel-memory";

// ============================================================
// 纯函数（可单测）
// ============================================================

/// 记忆类型的中文标签。
fn kind_label(kind: &str) -> &'static str {
    match kind.trim().to_ascii_lowercase().as_str() {
        "fact" => "事实",
        "preference" => "偏好",
        "event" => "事件",
        "task" => "任务",
        "block" => "固定块",
        "conversation" => "对话",
        "" => "未分类",
        _ => "其他",
    }
}

/// 相对时间文案（纯函数，方便单测；不含时区依赖）。
fn age_label(now_ms: i64, then_ms: i64) -> String {
    let secs = (now_ms - then_ms).max(0) / 1000;
    if secs < 60 {
        "刚刚".to_string()
    } else if secs < 3_600 {
        format!("{} 分钟前", secs / 60)
    } else if secs < 86_400 {
        format!("{} 小时前", secs / 3_600)
    } else {
        format!("{} 天前", secs / 86_400)
    }
}

/// 绝对日期（本地时区，仅用于较旧的条目）。
fn fmt_date(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

/// 列表里的时间：近一周用相对时间，更早用日期。
fn time_label(now_ms: i64, then_ms: i64) -> String {
    let secs = (now_ms - then_ms).max(0) / 1000;
    if secs < 7 * 86_400 {
        age_label(now_ms, then_ms)
    } else {
        fmt_date(then_ms)
    }
}

/// 重要度色点：高 = success，中 = warning，其余用中性色。
fn importance_color(c: &UiColors, importance: f64) -> Color {
    if importance >= 0.75 {
        c.success
    } else if importance >= 0.45 {
        c.warning
    } else {
        c.text_subtle
    }
}

// ============================================================
// 小部件
// ============================================================

fn dot(color: Color) -> Element<Msg> {
    col().w(6.0).h(6.0).rounded_full().bg(color).shrink0()
}

fn chip(c: &UiColors, label: impl Into<String>) -> Element<Msg> {
    row()
        .h(18.0)
        .items_center()
        .px(space::XS + 2.0)
        .rounded(R_CHIP)
        .bg(c.selection)
        .shrink0()
        .children((text(label).size(TextSize::Xs).weight(Weight::Medium).color(c.text_muted),))
}

fn card(c: &UiColors, rows: Vec<Element<Msg>>) -> Element<Msg> {
    col()
        .w_full()
        .p(space::MD)
        .gap(space::MD)
        .bg(c.surface_raised)
        .border(1.0, c.border_subtle)
        .rounded(R_CARD)
        .children(rows)
}

fn header(st: &AppState, c: &UiColors) -> Element<Msg> {
    row()
        .w_full()
        .items_end()
        .justify_between()
        .gap(space::MD)
        .children((
            col().grow().gap(2.0).children((
                text("记忆")
                    .size(TextSize::Lg)
                    .weight(Weight::Semibold)
                    .color(c.text),
                text("长期事实、偏好与固定块；每轮开始前代理会从这里检索。")
                    .size(TextSize::Xs)
                    .color(c.text_muted),
            )),
            badge(format!("{} 条", st.memory_items.len()), Status::Accent),
        ))
}

fn stat(c: &UiColors, label: &str, value: impl Into<String>) -> Element<Msg> {
    col().gap(2.0).grow().children((
        text(label).size(TextSize::Xs).color(c.text_subtle),
        text(value)
            .size(TextSize::Base)
            .weight(Weight::Semibold)
            .tabular()
            .color(c.text),
    ))
}

fn vline(c: &UiColors) -> Element<Msg> {
    col().w(1.0).h(28.0).bg(c.border_subtle).shrink0()
}

fn stats_card(st: &AppState, c: &UiColors, now: i64) -> Element<Msg> {
    let newest = st
        .memory_items
        .iter()
        .map(|i| i.created_at_ms)
        .max()
        .map(|ms| time_label(now, ms))
        .unwrap_or_else(|| "—".to_string());
    let blocks = st.memory_blocks.len();
    card(
        c,
        vec![
            row()
                .w_full()
                .items_center()
                .children((
                    stat(c, "记忆条数", st.memory_items.len().to_string()),
                    vline(c),
                    stat(c, "固定块", blocks.to_string()),
                    vline(c),
                    stat(c, "最近更新", newest),
                ))
                .into(),
        ],
    )
}

fn search_row(st: &AppState, c: &UiColors) -> Element<Msg> {
    let input: Element<Msg> = text_input(st.memory_query.clone())
        .placeholder("搜索记忆内容或标签，回车搜索")
        .width(280.0)
        .prefix(icons::search().w(16.0).h(16.0).color(c.text_subtle))
        .on_input(Msg::MemoryQueryChanged)
        .id("memory-search")
        .into();
    let input = input.on_key(|k| (k.key == Key::Enter).then_some(Msg::MemorySearch));

    row()
        .w_full()
        .items_center()
        .gap(space::SM)
        .children((
            col().grow().children((input.w_full(),)),
            button("搜索")
                .variant(ButtonVariant::Secondary)
                .size(ControlSize::Sm)
                .on_click(Msg::MemorySearch)
                .id("memory-search-go"),
        ))
}

fn blocks_card(st: &AppState, c: &UiColors) -> Element<Msg> {
    let mut rows: Vec<Element<Msg>> = vec![
        text("固定块会原样注入系统提示，修改后点“保存块”写入记忆库。")
            .size(TextSize::Xs)
            .color(c.text_subtle)
            .into(),
    ];

    for (key, label, hint, placeholder) in [
        (
            "persona",
            "人设块 persona",
            "代理自身的长期设定（比人设文件更稳定的小补充）。",
            "例如：回答保持简短、先给结论再给步骤。",
        ),
        (
            "human",
            "用户画像块 human",
            "关于你的稳定信息，代理每次都会看到。",
            "例如：Windows 开发者，偏好中文，常用 Rust / TypeScript。",
        ),
    ] {
        let value = st.memory_blocks.get(key).cloned().unwrap_or_default();
        let area: Element<Msg> = text_area(value.clone())
            .placeholder(placeholder)
            .width(320.0)
            .min_height(88.0)
            .on_input(move |v| Msg::MemoryBlockChanged {
                key: key.to_string(),
                value: v,
            })
            .id(&format!("memory-block-{key}"))
            .into();
        let save_value = value.clone();
        rows.push(
            col()
                .w_full()
                .gap(space::XS + 2.0)
                .children((
                    row()
                        .w_full()
                        .items_center()
                        .justify_between()
                        .gap(space::SM)
                        .children((
                            col().grow().gap(2.0).children((
                                text(label)
                                    .size(TextSize::Xs)
                                    .weight(Weight::Medium)
                                    .color(c.text),
                                text(hint).size(TextSize::Xs).color(c.text_subtle),
                            )),
                            button("保存块")
                                .variant(ButtonVariant::Secondary)
                                .size(ControlSize::Xs)
                                .on_click(Msg::MemoryBlockSave {
                                    key: key.to_string(),
                                    value: save_value,
                                })
                                .id(&format!("memory-block-save-{key}")),
                        )),
                    area.w_full(),
                ))
                .into(),
        );
    }

    card(c, rows)
}

fn memory_card(item: &MemoryItem, c: &UiColors, now: i64) -> Element<Msg> {
    let importance = item.importance.clamp(0.0, 1.0);
    let mut meta: Vec<Element<Msg>> = vec![
        chip(c, kind_label(&item.kind)),
        row()
            .items_center()
            .gap(space::XS)
            .children((
                dot(importance_color(c, importance)),
                text(format!("重要度 {:.1}", importance))
                    .size(TextSize::Xs)
                    .tabular()
                    .color(c.text_subtle),
            ))
            .into(),
        text(time_label(now, item.created_at_ms))
            .size(TextSize::Xs)
            .tabular()
            .color(c.text_subtle)
            .into(),
    ];
    if !item.tags.is_empty() {
        meta.push(
            text(format!("#{}", item.tags.join(" #")))
                .size(TextSize::Xs)
                .color(c.text_subtle)
                .truncate()
                .into(),
        );
    }

    row()
        .w_full()
        .items_start()
        .gap(space::SM)
        .p(space::MD)
        .bg(c.surface_raised)
        .border(1.0, c.border_subtle)
        .rounded(R_CARD)
        .children((
            col().grow().gap(space::SM).children((
                text(&item.content)
                    .size(TextSize::Sm)
                    .color(c.text)
                    .leading(1.55),
                row()
                    .w_full()
                    .items_center()
                    .gap(space::SM)
                    .wrap()
                    .children(meta),
            )),
            icon_button(icons::lucide::trash())
                .size(ControlSize::Xs)
                .label("删除这条记忆")
                .on_click(Msg::MemoryDelete(item.id))
                .id(&format!("memory-delete-{}", item.id)),
        ))
}

fn empty_state(c: &UiColors, title: &str, desc: &str) -> Element<Msg> {
    col()
        .w_full()
        .items_center()
        .py(space::XL)
        .gap(space::MD)
        .children((
            col()
                .w(44.0)
                .h(44.0)
                .rounded_full()
                .bg(c.surface_raised)
                .border(1.0, c.border_subtle)
                .items_center()
                .justify_center()
                .children((icons::search().w(20.0).h(20.0).color(c.text_subtle),)),
            text(title)
                .size(TextSize::Base)
                .weight(Weight::Semibold)
                .color(c.text),
            text(desc)
                .size(TextSize::Xs)
                .color(c.text_muted)
                .text_align(TextAlign::Center)
                .max_w(280.0),
        ))
}

// ============================================================
// 面板入口
// ============================================================

pub fn view(st: &AppState, c: &UiColors) -> Element<Msg> {
    let now = neosonia_core::util::now_ms();
    let mut children: Vec<Element<Msg>> = vec![
        header(st, c),
        stats_card(st, c, now),
        search_row(st, c),
        blocks_card(st, c),
    ];

    // 列表标题
    children.push(
        row()
            .w_full()
            .items_center()
            .justify_between()
            .gap(space::SM)
            .children((
                text("事实列表")
                    .size(TextSize::Sm)
                    .weight(Weight::Semibold)
                    .color(c.text),
                text(if st.memory_query.trim().is_empty() {
                    "按最近使用排序"
                } else {
                    "搜索结果"
                })
                .size(TextSize::Xs)
                .color(c.text_subtle),
            ))
            .into(),
    );

    if st.memory_items.is_empty() {
        let (title, desc) = if st.memory_query.trim().is_empty() {
            (
                "还没有长期记忆",
                "代理会在对话中自动记住值得记的事，也可以让它「记住……」。",
            )
        } else {
            (
                "没有匹配的记忆",
                "换个关键词试试，或清空搜索框查看全部。",
            )
        };
        children.push(empty_state(c, title, desc));
    } else {
        let cards: Vec<Element<Msg>> = st
            .memory_items
            .iter()
            .map(|item| memory_card(item, c, now))
            .collect();
        children.push(col().w_full().gap(space::SM).children(cards));
    }

    col()
        .w_full()
        .grow()
        .scroll_y()
        .p(space::LG)
        .gap(space::LG)
        .id(PANEL_ID)
        .children(children)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_labels_are_chinese_and_stable() {
        assert_eq!(kind_label("fact"), "事实");
        assert_eq!(kind_label("Preference"), "偏好");
        assert_eq!(kind_label(" event "), "事件");
        assert_eq!(kind_label("task"), "任务");
        assert_eq!(kind_label("block"), "固定块");
        assert_eq!(kind_label(""), "未分类");
        assert_eq!(kind_label("whatever"), "其他");
    }

    #[test]
    fn age_labels_cover_buckets() {
        let now = 1_000_000_000_000_i64;
        assert_eq!(age_label(now, now), "刚刚");
        assert_eq!(age_label(now, now - 30_000), "刚刚");
        assert_eq!(age_label(now, now - 5 * 60_000), "5 分钟前");
        assert_eq!(age_label(now, now - 3 * 3_600_000), "3 小时前");
        assert_eq!(age_label(now, now - 2 * 86_400_000), "2 天前");
        // 未来时间按 0 处理，不出现负数。
        assert_eq!(age_label(now, now + 10_000), "刚刚");
    }

    #[test]
    fn importance_thresholds() {
        // 色点阈值只依赖 UiColors，这里用主题构建一份来验证分支稳定。
        let theme = crate::theme::nova_theme(neosonia_core::config::ThemeMode::Dark, true, 1.0);
        let c = crate::theme::colors(&theme);
        assert_eq!(
            importance_color(&c, 0.9).to_rgba8(),
            c.success.to_rgba8()
        );
        assert_eq!(
            importance_color(&c, 0.5).to_rgba8(),
            c.warning.to_rgba8()
        );
        assert_eq!(
            importance_color(&c, 0.1).to_rgba8(),
            c.text_subtle.to_rgba8()
        );
    }
}
