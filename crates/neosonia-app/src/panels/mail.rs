//! 邮件面板：工具条 / 未配置引导 / 收件箱 / 详情 / 回复。
//!
//! 只读 `AppState::mail` 与 `AppState::settings`，所有网络操作通过 `Msg` 交给 app 层。
//! 零 emoji：未读用色点、白名单/外部用文字小标签、状态用"色点 + 文字"。

use fenestra::prelude::*;
use neosonia_mail::{MailFull, MailSummary};

use crate::state::{AppState, Msg, NavPage};
use crate::theme::{space, UiColors, R_CARD, R_CHIP};

const PANEL_ID: &str = "panel-mail";

/// 超过这个字符数的正文默认折叠。
const BODY_FOLD_CHARS: usize = 1600;

// ============================================================
// 纯函数（可单测）
// ============================================================

/// 邮件日期压缩成列表里用的短格式：`MM-DD HH:MM`。
///
/// 依次尝试 RFC3339 / RFC2822；都不是时保留原始字符串的前 16 个字符
/// （邮件服务端通常已经格式化过）。
fn short_mail_date(raw: &str) -> String {
    let s = raw.trim();
    if s.is_empty() {
        return "—".to_string();
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return dt
            .with_timezone(&chrono::Local)
            .format("%m-%d %H:%M")
            .to_string();
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(s) {
        return dt
            .with_timezone(&chrono::Local)
            .format("%m-%d %H:%M")
            .to_string();
    }
    s.chars().take(16).collect()
}

/// 发件人显示名：有名字用名字，否则用地址。
fn sender_label(from_name: &str, from: &str) -> String {
    let name = from_name.trim();
    if name.is_empty() {
        from.trim().to_string()
    } else {
        name.to_string()
    }
}

/// 地址是否命中白名单（同时看已保存设置与当前草稿，避免设置没保存时误判）。
fn whitelisted(st: &AppState, from: &str) -> bool {
    let addr = neosonia_mail::address_of(from).to_ascii_lowercase();
    if addr.is_empty() {
        return false;
    }
    if st.settings.is_whitelisted(&addr) {
        return true;
    }
    st.draft_str_list("/mail/whitelist").iter().any(|w| {
        let w = w.trim().to_ascii_lowercase();
        if w.is_empty() {
            false
        } else if w.starts_with('@') {
            addr.ends_with(&w)
        } else {
            addr == w
        }
    })
}

// ============================================================
// 小部件
// ============================================================

fn dot(color: Color) -> Element<Msg> {
    col().w(6.0).h(6.0).rounded_full().bg(color).shrink0()
}

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

fn source_chip(st: &AppState, c: &UiColors, from: &str) -> Element<Msg> {
    if whitelisted(st, from) {
        chip("白名单", c.success, c.success_bg)
    } else {
        chip("外部", c.text_subtle, c.selection)
    }
}

fn card(c: &UiColors, rows: Vec<Element<Msg>>) -> Element<Msg> {
    col()
        .w_full()
        // 滚动容器里的卡片不能被 flex shrink 压成视口高度，否则 scroll_y
        // 会把内容高度算成等于容器高度，滚轮永远滚不动。
        .shrink0()
        .p(space::MD)
        .gap(space::MD)
        .bg(c.surface_raised)
        .border(1.0, c.border_subtle)
        .rounded(R_CARD)
        .children(rows)
}

fn toolbar(st: &AppState, c: &UiColors) -> Element<Msg> {
    let status = st
        .mail
        .status
        .clone()
        .unwrap_or_else(|| {
            if st.mail.loaded {
                "收件箱已同步".to_string()
            } else {
                "尚未同步".to_string()
            }
        });
    let busy = st.mail.loading;

    let left = col().grow().gap(2.0).children((
        row().items_center().gap(space::SM).children((
            text("邮件")
                .size(TextSize::Lg)
                .weight(Weight::Semibold)
                .color(c.text),
            if st.mail.unread > 0 {
                chip(format!("{} 未读", st.mail.unread), c.on_accent, c.accent)
            } else {
                chip("无未读", c.text_subtle, c.selection)
            },
        )),
        row().items_center().gap(space::XS).children((
            dot(if busy { c.warning } else { c.text_subtle }),
            text(neosonia_core::util::truncate_chars(&status, 42))
                .size(TextSize::Xs)
                .color(c.text_subtle),
        )),
    ));

    let busy_indicator: Element<Msg> = if busy {
        row().children((spinner(),))
    } else {
        col().w(0.0)
    };

    row()
        .w_full()
        .items_center()
        .gap(space::SM)
        .children((
            left,
            busy_indicator,
            button("刷新")
                .variant(ButtonVariant::Secondary)
                .size(ControlSize::Sm)
                .on_click(Msg::MailRefresh)
                .id("mail-refresh"),
        ))
}

fn setup_guide(c: &UiColors) -> Element<Msg> {
    card(
        c,
        vec![
            row().items_center().gap(space::SM).children((
                dot(c.warning),
                text("还没有配置 IMAP 收信")
                    .size(TextSize::Base)
                    .weight(Weight::Semibold)
                    .color(c.text),
            )).into(),
            text("在设置页填好 IMAP / SMTP 账号后，这里就能收信、读正文并直接回复。邮箱密码只存本机配置，不会写进日志。")
                .size(TextSize::Xs)
                .color(c.text_muted)
                .leading(1.6)
                .into(),
            row().w_full().items_center().gap(space::SM).children((
                button("去设置页填写")
                    .variant(ButtonVariant::Primary)
                    .size(ControlSize::Sm)
                    .on_click(Msg::Nav(NavPage::Settings))
                    .id("mail-go-settings"),
                button("刷新收件箱")
                    .variant(ButtonVariant::Secondary)
                    .size(ControlSize::Sm)
                    .on_click(Msg::MailRefresh)
                    .id("mail-refresh-guide"),
            )).into(),
            divider(),
            text("白名单说明：只有匹配 user@host（精确）或 @domain（整个域）的来信，才会自动触发代理处理与回复；给白名单外的人主动发信默认被拒绝，可在设置里打开。")
                .size(TextSize::Xs)
                .color(c.text_subtle)
                .leading(1.6)
                .into(),
        ],
    )
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
                .children((icons::lucide::mail().w(20.0).h(20.0).color(c.text_subtle),)),
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
// 收件箱
// ============================================================

fn inbox_row(st: &AppState, c: &UiColors, m: &MailSummary) -> Element<Msg> {
    let unread_dot = if m.unread { c.accent } else { Color::TRANSPARENT };
    let sender = sender_label(&m.from_name, &m.from);
    let subject_weight = if m.unread {
        Weight::Semibold
    } else {
        Weight::Regular
    };

    let mut trail: Vec<Element<Msg>> = vec![source_chip(st, c, &m.from)];
    if m.has_attachments {
        trail.push(chip("附件", c.text_subtle, c.selection));
    }

    let mut lines: Vec<Element<Msg>> = vec![
        row().w_full().items_center().gap(space::SM).children((
            text(sender)
                .size(TextSize::Sm)
                .weight(subject_weight)
                .color(c.text)
                .grow()
                .truncate(),
            text(short_mail_date(&m.date))
                .size(TextSize::Xs)
                .tabular()
                .color(c.text_subtle),
        )),
        row().w_full().items_center().gap(space::SM).children((
            text(if m.subject.trim().is_empty() {
                "（无主题）".to_string()
            } else {
                m.subject.clone()
            })
            .size(TextSize::Sm)
            .weight(subject_weight)
            .color(if m.unread { c.text } else { c.text_muted })
            .grow()
            .truncate(),
            row().items_center().gap(space::XS).shrink0().children(trail),
        )),
    ];
    if !m.snippet.trim().is_empty() {
        lines.push(
            text(neosonia_core::util::truncate_chars(&m.snippet, 72))
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
        .px(space::MD)
        .py(space::SM)
        .on_click(Msg::MailOpen(m.uid))
        .cursor(Cursor::Pointer)
        .state_layer(|t| t.text)
        .id(&format!("mail-row-{}", m.uid))
        .children((
            col().pt(6.0).shrink0().children((dot(unread_dot),)),
            col().grow().gap(3.0).children(lines),
        ))
}

fn inbox_card(st: &AppState, c: &UiColors) -> Element<Msg> {
    let mut rows: Vec<Element<Msg>> = Vec::new();
    for (i, m) in st.mail.summaries.iter().enumerate() {
        if i > 0 {
            rows.push(divider());
        }
        rows.push(inbox_row(st, c, m));
    }
    col()
        .w_full()
        // 同上：收件箱长列表必须保持自然高度，交给外层 scroll_y 滚动。
        .shrink0()
        .bg(c.surface_raised)
        .border(1.0, c.border_subtle)
        .rounded(R_CARD)
        .overflow_hidden()
        .id("mail-inbox-card")
        .children(rows)
}

// ============================================================
// 详情
// ============================================================

fn meta_line(c: &UiColors, label: &str, value: impl Into<String>) -> Element<Msg> {
    row()
        .w_full()
        .items_start()
        .gap(space::SM)
        .children((
            text(label).size(TextSize::Xs).color(c.text_subtle).w(36.0).shrink0(),
            // 超长邮箱/收件人列表单行截断，避免把详情卡撑出面板右边界。
            text(value)
                .size(TextSize::Xs)
                .color(c.text_muted)
                .grow()
                .min_w(0.0)
                .leading(1.5)
                .truncate(),
        ))
}

/// 正文 markdown（只在需要展示时构建，避免每帧解析长文）。
fn markdown_element(source: &str) -> Element<Msg> {
    let el: Element<Msg> = fenestra_markdown::markdown(source.to_owned())
        .on_link(|url| Msg::OpenLink(url.to_owned()))
        .into();
    // 强制占满可用宽度：长句/长 URL 在窄栏里换行，不把卡片撑出右边界。
    el.w_full().overflow_hidden()
}

fn mail_body(full: &MailFull, expanded: bool, c: &UiColors) -> Element<Msg> {
    let chars = full.body_text.chars().count();
    let long = chars > BODY_FOLD_CHARS;

    if !long {
        markdown_element(&full.body_text)
    } else if !expanded {
        let preview: String = full.body_text.chars().take(BODY_FOLD_CHARS).collect();
        col()
            .w_full()
            .gap(space::SM)
            .children((
                text(format!("{preview}…"))
                    .size(TextSize::Sm)
                    .color(c.text_muted)
                    .leading(1.6),
                button(format!("展开全文（共 {chars} 字）"))
                    .variant(ButtonVariant::Ghost)
                    .size(ControlSize::Xs)
                    .on_click(Msg::MailToggleBody)
                    .id("mail-body-toggle"),
            ))
            .into()
    } else {
        col()
            .w_full()
            .gap(space::SM)
            .children((
                markdown_element(&full.body_text),
                button("收起长文")
                    .variant(ButtonVariant::Ghost)
                    .size(ControlSize::Xs)
                    .on_click(Msg::MailToggleBody)
                    .id("mail-body-collapse"),
            ))
            .into()
    }
}

fn reply_card(st: &AppState, c: &UiColors, full: &MailFull) -> Element<Msg> {
    let trusted = whitelisted(st, &full.from);
    let area: Element<Msg> = text_area(st.mail.reply_text.clone())
        .placeholder("写回复…（Enter 换行，Ctrl+Enter 也会换行；用按钮发送）")
        .width(320.0)
        .min_height(96.0)
        .on_input(Msg::MailReplyChanged)
        .id("mail-reply")
        .into();

    let hint = if trusted {
        "白名单来信，可直接回复"
    } else {
        "外部来信：默认只允许回复到原地址"
    };
    let status = st.mail.status.clone();

    col()
        .w_full()
        .p(space::MD)
        .gap(space::SM)
        .bg(c.surface_raised)
        .border(1.0, c.border_subtle)
        .rounded(R_CARD)
        .children((
            row().w_full().items_center().justify_between().gap(space::SM).children((
                text("回复")
                    .size(TextSize::Sm)
                    .weight(Weight::Semibold)
                    .color(c.text),
                text(hint)
                    .size(TextSize::Xs)
                    .color(c.text_subtle)
                    .grow()
                    .min_w(0.0)
                    .truncate(),
            )),
            area.w_full(),
            // mail_send 支持 attachments（文件路径）；发送结果里会带上附件数量。
            text("需要附件时让代理在发送时指定文件路径；发送结果会注明附件数量。")
                .size(TextSize::Xs)
                .color(c.text_subtle)
                .leading(1.4),
            row().w_full().items_center().justify_between().gap(space::SM).children((
                col().grow().min_w(0.0).children((
                    row().items_center().gap(space::XS).children((
                        dot(if status.is_some() {
                            c.accent
                        } else {
                            c.text_subtle.with_alpha(0.5)
                        }),
                        text(status.unwrap_or_else(|| "发送前会按白名单校验".to_string()))
                            .size(TextSize::Xs)
                            .color(c.text_subtle)
                            .truncate(),
                    )),
                )),
                button("发送回复")
                    .variant(ButtonVariant::Primary)
                    .size(ControlSize::Sm)
                    .on_click(Msg::MailSendReply)
                    .id("mail-send-reply"),
            )),
        ))
}

fn detail_card(st: &AppState, c: &UiColors) -> Element<Msg> {
    let Some(full) = st.mail.open.as_ref() else {
        return card(
            c,
            vec![
                row().items_center().gap(space::SM).children((
                    spinner(),
                    text("正在读取邮件…").size(TextSize::Xs).color(c.text_muted),
                )).into(),
            ],
        );
    };

    let to = if full.to.is_empty() {
        "—".to_string()
    } else {
        full.to.join("，")
    };
    let cc = if full.cc.is_empty() {
        None
    } else {
        Some(full.cc.join("，"))
    };

    let mut rows: Vec<Element<Msg>> = vec![
        // 返回是图标按钮（无文字，tooltip 用 label），放在详情标题左侧。
        row().w_full().items_center().gap(space::SM).children((
            icon_button(icons::lucide::arrow_left())
                .variant(ButtonVariant::Ghost)
                .size(ControlSize::Sm)
                .label("返回收件箱")
                .on_click(Msg::MailClose)
                .id("mail-back"),
            text(if full.subject.trim().is_empty() {
                "（无主题）".to_string()
            } else {
                full.subject.clone()
            })
            .size(TextSize::Base)
            .weight(Weight::Semibold)
            .color(c.text)
            .grow()
            .min_w(0.0)
            .truncate(),
        )).into(),
        row().w_full().items_center().gap(space::SM).wrap().children((
            source_chip(st, c, &full.from),
            if full.has_attachments {
                chip("含附件", c.text_subtle, c.selection)
            } else {
                col().w(0.0)
            },
            text(short_mail_date(&full.date))
                .size(TextSize::Xs)
                .tabular()
                .color(c.text_subtle),
        )).into(),
        meta_line(c, "发件人", &full.from).into(),
        meta_line(c, "收件人", to).into(),
    ];
    if let Some(cc) = cc {
        rows.push(meta_line(c, "抄送", cc).into());
    }
    rows.push(divider());
    rows.push(mail_body(full, st.mail.body_expanded, c));
    rows.push(divider());
    rows.push(
        text("回复会带上原邮件主题与 Message-ID，尽量保持会话线程。")
            .size(TextSize::Xs)
            .color(c.text_subtle)
            .into(),
    );

    // 溢出兜底：长附件名/长地址/异常 markdown 最多被裁在卡片内，不再顶出面板。
    card(c, rows)
        .overflow_hidden()
        .id("mail-detail-card")
        .into()
}

// ============================================================
// 面板入口
// ============================================================

/// 独立滚动区：列表页/详情页各用自己的滚动容器，避免嵌套滚动。
fn scroll_area(
    id: &'static str,
    bottom_pad: f32,
    children: Vec<Element<Msg>>,
) -> Element<Msg> {
    col()
        .w_full()
        .grow()
        .min_h(0.0)
        .scroll_y()
        .px(space::LG)
        .pt(space::SM)
        .pb(bottom_pad)
        .gap(space::LG)
        .id(id)
        .children(children)
        .into()
}

pub fn view(st: &AppState, c: &UiColors) -> Element<Msg> {
    let configured = !st.settings.mail.imap.host.trim().is_empty();

    // 工具栏固定在顶部（不随列表/详情滚动）：放在根 col 里、滚动区之外。
    let toolbar_el = toolbar(st, c)
        .w_full()
        .shrink0()
        .id("mail-toolbar")
        .px(space::LG)
        .pt(space::LG)
        .pb(space::SM);

    let body: Element<Msg> = if !configured {
        scroll_area("mail-guide-scroll", space::XL, vec![setup_guide(c)])
    } else if st.mail.open_uid.is_some() {
        // 详情页：独立滚动，正文/长邮件地址不会撑出面板。
        let mut kids: Vec<Element<Msg>> = vec![detail_card(st, c)];
        if let Some(full) = st.mail.open.as_ref() {
            kids.push(reply_card(st, c, full));
        }
        scroll_area("mail-detail-scroll", space::XXL, kids)
    } else if st.mail.loading && !st.mail.loaded {
        scroll_area(
            "mail-list-scroll",
            space::XXL,
            vec![card(
                c,
                vec![
                    row().items_center().gap(space::SM).children((
                        spinner(),
                        text("正在收取邮件…")
                            .size(TextSize::Xs)
                            .color(c.text_muted),
                    )).into(),
                    progress_indeterminate(),
                ],
            )],
        )
    } else if st.mail.summaries.is_empty() {
        scroll_area(
            "mail-list-scroll",
            space::XXL,
            vec![empty_state(
                c,
                "收件箱为空",
                "点右上角刷新检查新邮件；后台轮询也会按设置里的间隔自动拉取。",
            )],
        )
    } else {
        // 列表底部多留 32px，最后一条不会贴住面板底边。
        scroll_area("mail-list-scroll", space::XXL, vec![inbox_card(st, c)])
    };

    col()
        .w_full()
        .grow()
        .min_h(0.0)
        .id(PANEL_ID)
        .children((toolbar_el, body))
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mail_dates_are_compact() {
        let expected = chrono::DateTime::parse_from_rfc3339("2026-10-31T19:51:18+08:00")
            .unwrap()
            .with_timezone(&chrono::Local)
            .format("%m-%d %H:%M")
            .to_string();
        assert_eq!(short_mail_date("2026-10-31T19:51:18+08:00"), expected);

        let expected_rfc = chrono::DateTime::parse_from_rfc2822("Sat, 31 Oct 2026 19:51:18 +0800")
            .unwrap()
            .with_timezone(&chrono::Local)
            .format("%m-%d %H:%M")
            .to_string();
        assert_eq!(
            short_mail_date("Sat, 31 Oct 2026 19:51:18 +0800"),
            expected_rfc
        );

        // 已经是格式化好的字符串：原样（截到 16 字符）。
        assert_eq!(short_mail_date("2026-10-31 19:51"), "2026-10-31 19:51");
        assert_eq!(short_mail_date("   "), "—");
    }

    #[test]
    fn sender_label_prefers_display_name() {
        assert_eq!(sender_label("张三", "zhang@corp.com"), "张三");
        assert_eq!(sender_label("  ", "zhang@corp.com"), "zhang@corp.com");
        assert_eq!(sender_label("", ""), "");
        // 白名单匹配走 Settings 的同一套规则。
        let mut settings = neosonia_core::config::Settings::default();
        settings.mail.whitelist = vec!["boss@corp.com".into(), "@vip.com".into()];
        assert!(settings.is_whitelisted("boss@corp.com"));
        assert!(settings.is_whitelisted("anyone@vip.com"));
        assert!(!settings.is_whitelisted("nobody@corp.com"));
    }

    /// 构造 26 封邮件的状态（用于验证列表/详情滚动容器有确定高度）。
    fn scroll_test_state(count: usize) -> AppState {
        use neosonia_core::config::{Paths, Settings};
        let mut st = AppState::new(
            Paths::from_root("C:/tmp/neosonia-mail-layout"),
            Settings::default(),
        );
        st.settings.mail.imap.host = "imap.example.com".into();
        st.mail.loaded = true;
        st.mail.summaries = (0..count)
            .map(|i| MailSummary {
                uid: i as u32 + 1,
                from: format!("sender{i}@example.com"),
                from_name: format!("发件人{i}"),
                subject: format!("第 {i} 封邮件主题"),
                date: "2026-11-04 10:00".into(),
                snippet: "这是摘要".into(),
                unread: i % 2 == 0,
                has_attachments: i % 5 == 0,
            })
            .collect();
        st
    }

    #[test]
    fn mail_scroll_layout_is_bounded_and_wheel_routes() {
        let theme = crate::theme::nova_theme(neosonia_core::config::ThemeMode::Dark, true, 1.0);
        let c = crate::theme::colors(&theme);
        let mut fonts = Fonts::embedded();
        let mut state = FrameState::new();

        // 列表页：内容应高于视口，滚轮命中 mail-list-scroll，且能滚到底。
        let st = scroll_test_state(26);
        let root = view(&st, &c);
        let frame = build_frame(&root, &theme, &mut fonts, &mut state, (424.0, 760.0), 1.0);
        let scroll = frame.get(&by::id("mail-list-scroll"));
        let card = frame.get(&by::id("mail-inbox-card"));
        assert!(
            card.rect.height() > scroll.rect.height(),
            "列表内容({:.1}) 应高于视口({:.1})，否则 scroll_y 不会生效",
            card.rect.height(),
            scroll.rect.height()
        );
        assert!(frame.is_scrollable(scroll.id), "mail-list-scroll 应可滚动");
        assert_eq!(
            frame.scrollable_y_at(scroll.rect.center()),
            Some(scroll.id),
            "滚轮应路由到列表滚动容器"
        );
        state.scroll_by(scroll.id, 1_000_000.0);
        let frame2 = build_frame(&root, &theme, &mut fonts, &mut state, (424.0, 760.0), 1.0);
        let scroll2 = frame2.get(&by::id("mail-list-scroll"));
        let card2 = frame2.get(&by::id("mail-inbox-card"));
        assert!(
            card2.rect.y0 < card.rect.y0 - 100.0,
            "滚轮后内容应上移（{:.1} -> {:.1}）",
            card.rect.y0,
            card2.rect.y0
        );
        assert!(
            card2.rect.y1 <= scroll2.rect.y1 + 1.0,
            "滚到底后卡片底部({:.1}) 应不超出视口底部({:.1})",
            card2.rect.y1,
            scroll2.rect.y1
        );

        // 详情页：长正文也要能独立滚动。
        let mut st2 = scroll_test_state(26);
        st2.mail.open_uid = Some(1);
        st2.mail.open = Some(MailFull {
            uid: 1,
            from: "boss@corp.com".into(),
            to: vec!["me@corp.com".into()],
            cc: Vec::new(),
            subject: "长邮件：一条很长的 URL".into(),
            date: "2026-11-04 10:00".into(),
            body_text: format!(
                "https://example.com/{}正文{}",
                "very-long-path/".repeat(200),
                "内容".repeat(2000)
            ),
            has_attachments: true,
            message_id: "<x@y>".into(),
            references: Vec::new(),
        });
        let root2 = view(&st2, &c);
        let frame3 = build_frame(&root2, &theme, &mut fonts, &mut state, (424.0, 760.0), 1.0);
        let dscroll = frame3.get(&by::id("mail-detail-scroll"));
        let dcard = frame3.get(&by::id("mail-detail-card"));
        assert!(
            dcard.rect.height() > dscroll.rect.height(),
            "详情内容({:.1}) 应高于视口({:.1})",
            dcard.rect.height(),
            dscroll.rect.height()
        );
        assert!(frame3.is_scrollable(dscroll.id), "mail-detail-scroll 应可滚动");
        assert_eq!(
            frame3.scrollable_y_at(dscroll.rect.center()),
            Some(dscroll.id),
            "滚轮应路由到详情滚动容器"
        );
    }
}






