//! 历史面板：会话列表 + 消息命中 + 新建/删除。
//!
//! 数据来自 `AppState::history`（Lead 的 `HistoryState`）；所有动作通过
//! `Msg::History*` 交给主循环，`view` 里不做 IO。

use fenestra::prelude::*;
use neosonia_core::{ConversationHit, ConversationSummary};

use crate::state::{AppState, Msg, NavPage};
use crate::theme::{space, UiColors, R_CARD, R_CHIP};

const PANEL_ID: &str = "panel-history";

// ============================================================
// 纯函数（可单测）
// ============================================================

/// 相对时间文案（7 天以前显示日期）。
fn age_label(now_ms: i64, then_ms: i64) -> String {
    let secs = (now_ms - then_ms).max(0) / 1000;
    if secs < 60 {
        "刚刚".to_string()
    } else if secs < 3_600 {
        format!("{} 分钟前", secs / 60)
    } else if secs < 86_400 {
        format!("{} 小时前", secs / 3_600)
    } else if secs < 7 * 86_400 {
        format!("{} 天前", secs / 86_400)
    } else {
        fmt_date(then_ms)
    }
}

fn fmt_date(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%Y-%m-%d")
                .to_string()
        })
        .unwrap_or_default()
}

/// 消息角色的中文标签。
fn role_label(role: &str) -> &'static str {
    match role.trim().to_ascii_lowercase().as_str() {
        "user" => "用户",
        "assistant" => "助手",
        "tool" => "工具",
        "system" => "系统",
        _ => "消息",
    }
}

fn time_label(ms: i64) -> String {
    age_label(neosonia_core::util::now_ms(), ms)
}

// ============================================================
// 小部件
// ============================================================

fn chip(label: impl Into<String>, fg: Color, bg: Color) -> Element<Msg> {
    row()
        .h(18.0)
        .items_center()
        .px(space::XS + 2.0)
        .rounded(R_CHIP)
        .bg(bg)
        .shrink0()
        .children((text(label).size(TextSize::Xs).weight(Weight::Medium).color(fg),))
}

fn header(st: &AppState, c: &UiColors) -> Element<Msg> {
    // 两种入口都要能回对话：
    // - 从对话页打开覆盖层（page == Chat）：HistoryToggle 关掉覆盖层；
    // - 独立历史页（page == History）：OpenPage(Chat) 切回对话页。
    let back_msg = if st.page == NavPage::History {
        Msg::OpenPage(NavPage::Chat)
    } else {
        Msg::HistoryToggle
    };
    row()
        .w_full()
        .items_end()
        .justify_between()
        .gap(space::MD)
        .children((
            col().grow().gap(2.0).children((
                row().items_center().gap(space::SM).children((
                    text("历史")
                        .size(TextSize::Lg)
                        .weight(Weight::Semibold)
                        .color(c.text),
                    if st.history.loading {
                        row().items_center().gap(space::XS).children((
                            spinner(),
                            text("搜索中").size(TextSize::Xs).color(c.text_subtle),
                        ))
                    } else {
                        col().w(0.0)
                    },
                )),
                text("搜索过去的会话与消息；打开会话会载入到对话页继续聊。")
                    .size(TextSize::Xs)
                    .color(c.text_muted),
            )),
            row().gap(space::SM).items_center().shrink0().children((
                button("新建对话")
                    .variant(ButtonVariant::Secondary)
                    .size(ControlSize::Sm)
                    .on_click(Msg::HistoryNew)
                    .id("history-new"),
                button("返回对话")
                    .variant(ButtonVariant::Ghost)
                    .size(ControlSize::Sm)
                    .on_click(back_msg)
                    .id("history-back"),
            )),
        ))
}

fn search_row(st: &AppState, c: &UiColors) -> Element<Msg> {
    let input: Element<Msg> = text_input(st.history.query.clone())
        .placeholder("搜索会话标题或消息内容，回车搜索")
        .width(280.0)
        .prefix(icons::search().w(16.0).h(16.0).color(c.text_subtle))
        .on_input(Msg::HistoryQueryChanged)
        .id("history-search")
        .into();
    let input = input.on_key(|k| (k.key == Key::Enter).then_some(Msg::HistorySearch));

    row()
        .w_full()
        .items_center()
        .gap(space::SM)
        .children((
            col().grow().children((input.w_full(),)),
            button("搜索")
                .variant(ButtonVariant::Secondary)
                .size(ControlSize::Sm)
                .on_click(Msg::HistorySearch)
                .id("history-search-go"),
        ))
}

fn section_title(c: &UiColors, title: &str, count: usize) -> Element<Msg> {
    row()
        .w_full()
        .items_center()
        .gap(space::SM)
        .children((
            text(title)
                .size(TextSize::Sm)
                .weight(Weight::Semibold)
                .color(c.text),
            chip(format!("{count}"), c.text_subtle, c.selection),
        ))
}

fn session_card(st: &AppState, c: &UiColors, s: &ConversationSummary) -> Element<Msg> {
    let id = s.id.clone();
    let delete_id = s.id.clone();
    let is_current = st.history.active_id.as_deref() == Some(s.id.as_str())
        || st.history.current_id.as_deref() == Some(s.id.as_str());
    let title = if s.title.trim().is_empty() {
        "（无标题会话）".to_string()
    } else {
        s.title.clone()
    };

    let mut meta: Vec<Element<Msg>> = vec![
        text(format!("{} 条消息 · {}", s.message_count, time_label(s.updated_at_ms)))
            .size(TextSize::Xs)
            .tabular()
            .color(c.text_subtle)
            .into(),
    ];
    if !s.persona.trim().is_empty() {
        meta.push(
            text(format!("人设 {}", s.persona))
                .size(TextSize::Xs)
                .color(c.text_subtle)
                .truncate()
                .into(),
        );
    }

    let mut body: Vec<Element<Msg>> = vec![
        row().w_full().items_center().gap(space::SM).children((
            text(title)
                .size(TextSize::Base)
                .weight(Weight::Semibold)
                .color(c.text)
                .grow()
                .truncate(),
            if is_current {
                chip("当前", c.on_accent, c.accent)
            } else {
                col().w(0.0)
            },
        )).into(),
        row().w_full().items_center().gap(space::SM).wrap().children(meta).into(),
    ];
    if !s.preview.trim().is_empty() {
        body.push(
            text(neosonia_core::util::truncate_chars(&s.preview, 90))
                .size(TextSize::Xs)
                .color(c.text_muted)
                .truncate()
                .into(),
        );
    }

    // 左侧内容整块可点；删除按钮是它的兄弟节点，避免点删除时同时触发打开。
    let open = col()
        .grow()
        .min_w(0.0)
        .gap(space::XS)
        .on_click(Msg::HistorySelect(id))
        .cursor(Cursor::Pointer)
        .id(&format!("history-open-{}", s.id))
        .children(body);

    row()
        .w_full()
        .items_center()
        .gap(space::SM)
        .p(space::MD)
        .bg(c.surface_raised)
        .border(1.0, c.border_subtle)
        .rounded(R_CARD)
        .children((
            open,
            icon_button(icons::lucide::trash())
                .size(ControlSize::Xs)
                .label("删除这个会话")
                .on_click(Msg::HistoryDelete(delete_id))
                .id(&format!("history-delete-{}", s.id)),
        ))
}

fn hit_card(c: &UiColors, hit: &ConversationHit, index: usize) -> Element<Msg> {
    let title = if hit.conversation_title.trim().is_empty() {
        "（无标题会话）".to_string()
    } else {
        hit.conversation_title.clone()
    };
    col()
        .w_full()
        .p(space::MD)
        .gap(space::XS)
        .bg(c.surface_raised)
        .border(1.0, c.border_subtle)
        .rounded(R_CARD)
        .on_click(Msg::HistorySelect(hit.conversation_id.clone()))
        .cursor(Cursor::Pointer)
        .id(&format!("history-hit-{index}"))
        .children((
            row().w_full().items_center().gap(space::SM).children((
                chip(role_label(&hit.role), c.text_subtle, c.selection),
                text(title)
                    .size(TextSize::Xs)
                    .color(c.text_subtle)
                    .grow()
                    .truncate(),
                text(time_label(hit.at_ms))
                    .size(TextSize::Xs)
                    .tabular()
                    .color(c.text_subtle),
            )),
            text(hit.snippet.clone())
                .size(TextSize::Sm)
                .color(c.text)
                .leading(1.5),
        ))
}

fn empty_hint(c: &UiColors, title: &str, desc: &str, icon: Element<Msg>) -> Element<Msg> {
    col()
        .w_full()
        .items_center()
        .py(space::LG)
        .gap(space::SM)
        .children((
            col()
                .w(44.0)
                .h(44.0)
                .rounded_full()
                .bg(c.surface_raised)
                .border(1.0, c.border_subtle)
                .items_center()
                .justify_center()
                .children((icon,)),
            text(title)
                .size(TextSize::Base)
                .weight(Weight::Semibold)
                .color(c.text),
            text(desc)
                .size(TextSize::Xs)
                .color(c.text_muted)
                .text_align(TextAlign::Center)
                .max_w(300.0),
        ))
}

// ============================================================
// 面板入口
// ============================================================

pub fn view(st: &AppState, c: &UiColors) -> Element<Msg> {
    let h = &st.history;
    let mut children: Vec<Element<Msg>> = vec![header(st, c), search_row(st, c)];

    // 会话列表
    children.push(section_title(c, "会话", h.sessions.len()));
    if h.sessions.is_empty() {
        let (title, desc) = if h.loading {
            ("正在搜索…", "历史库较大时第一次搜索会慢一点。")
        } else if h.query.trim().is_empty() {
            ("还没有历史会话", "从这里新建对话，或直接在对话页开始一轮。")
        } else {
            ("没有找到匹配的会话", "换个关键词试试，或清空搜索框。")
        };
        children.push(empty_hint(
            c,
            title,
            desc,
            icons::lucide::clock().w(20.0).h(20.0).color(c.text_subtle),
        ));
    } else {
        let cards: Vec<Element<Msg>> = h.sessions.iter().map(|s| session_card(st, c, s)).collect();
        children.push(col().w_full().gap(space::SM).children(cards));
    }

    // 消息命中
    children.push(section_title(c, "消息命中", h.hits.len()));
    if h.hits.is_empty() {
        let (title, desc) = if h.query.trim().is_empty() {
            ("搜索后显示消息命中", "会列出标题和消息正文里包含关键词的片段。")
        } else if h.loading {
            ("正在搜索…", "正在扫描历史消息。")
        } else {
            ("没有命中的消息", "只匹配标题时可以到上面的会话列表里找。")
        };
        children.push(empty_hint(
            c,
            title,
            desc,
            icons::search().w(20.0).h(20.0).color(c.text_subtle),
        ));
    } else {
        let cards: Vec<Element<Msg>> = h
            .hits
            .iter()
            .enumerate()
            .map(|(i, hit)| hit_card(c, hit, i))
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
    fn role_labels_are_chinese() {
        assert_eq!(role_label("user"), "用户");
        assert_eq!(role_label("Assistant"), "助手");
        assert_eq!(role_label("tool"), "工具");
        assert_eq!(role_label("system"), "系统");
        assert_eq!(role_label("other"), "消息");
        assert_eq!(role_label(""), "消息");
    }

    #[test]
    fn age_labels_cover_buckets() {
        let now = 1_700_000_000_000_i64;
        assert_eq!(age_label(now, now), "刚刚");
        assert_eq!(age_label(now, now - 61_000), "1 分钟前");
        assert_eq!(age_label(now, now - 2 * 3_600_000), "2 小时前");
        assert_eq!(age_label(now, now - 3 * 86_400_000), "3 天前");
        assert_eq!(age_label(now, now + 5_000), "刚刚");
        // 更早的显示日期（本地时区，只断言非空且含 '-'）。
        let old = age_label(now, now - 30 * 86_400_000);
        assert!(old.contains('-'), "{old}");
    }
}
