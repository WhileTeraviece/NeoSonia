//! 设备面板：局域网 Web 服务状态、二维码配对、已信任设备管理。
//!
//! 数据来自 `AppState::remote`（Lead 的 `RemoteState`）。二维码像素由 app 层
//! 生成后放在 `st.remote.qr_pixels`，这里只负责用 `image_rgba8` 渲染。

use fenestra::prelude::*;

use crate::state::{AppState, Msg, NavPage};
use crate::theme::{space, UiColors, R_CARD, R_CHIP};

const PANEL_ID: &str = "panel-remote";

// ============================================================
// 纯函数（可单测）
// ============================================================

/// 配对码剩余时间：`mm:ss`，过期后显示"已过期"。
fn countdown_label(expires_at_ms: i64, now_ms: i64) -> String {
    let remain = expires_at_ms - now_ms;
    if remain <= 0 {
        return "已过期".to_string();
    }
    let secs = remain / 1000;
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

/// 整数显示（端口等），避免出现 `8787.0`。
fn int_label(v: f64) -> String {
    if !v.is_finite() {
        "0".to_string()
    } else {
        format!("{}", v as i64)
    }
}

/// 相对时间（7 天以前只显示日期）。
fn age_label(now_ms: i64, then_ms: i64) -> String {
    let secs = (now_ms - then_ms).max(0) / 1000;
    if then_ms <= 0 {
        "从未".to_string()
    } else if secs < 60 {
        "刚刚".to_string()
    } else if secs < 3_600 {
        format!("{} 分钟前", secs / 60)
    } else if secs < 86_400 {
        format!("{} 小时前", secs / 3_600)
    } else if secs < 7 * 86_400 {
        format!("{} 天前", secs / 86_400)
    } else {
        chrono::DateTime::from_timestamp_millis(then_ms)
            .map(|dt| {
                dt.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d")
                    .to_string()
            })
            .unwrap_or_default()
    }
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
                text("设备")
                    .size(TextSize::Lg)
                    .weight(Weight::Semibold)
                    .color(c.text),
                text("手机、平板连同一个局域网，扫码配对后就能远程和 NeoSonia 聊天。")
                    .size(TextSize::Xs)
                    .color(c.text_muted),
            )),
            row().items_center().gap(space::SM).shrink0().children((
                if st.remote.busy {
                    row().items_center().gap(space::XS).children((
                        spinner(),
                        text("处理中").size(TextSize::Xs).color(c.text_subtle),
                    ))
                } else {
                    col().w(0.0)
                },
                button("刷新")
                    .variant(ButtonVariant::Secondary)
                    .size(ControlSize::Sm)
                    .on_click(Msg::RemoteRefresh)
                    .id("remote-refresh"),
                button("返回设置")
                    .variant(ButtonVariant::Ghost)
                    .size(ControlSize::Sm)
                    .on_click(Msg::Nav(NavPage::Settings))
                    .id("remote-back"),
            )),
        ))
}

fn status_row(st: &AppState, c: &UiColors) -> Element<Msg> {
    let text_ = if st.remote.status.trim().is_empty() {
        if st.remote.enabled {
            "局域网服务已启用".to_string()
        } else {
            "局域网服务未启用".to_string()
        }
    } else {
        st.remote.status.clone()
    };
    row()
        .w_full()
        .items_center()
        .gap(space::XS)
        .children((
            dot(if st.remote.enabled {
                c.success
            } else {
                c.text_subtle
            }),
            text(neosonia_core::util::truncate_chars(&text_, 60))
                .size(TextSize::Xs)
                .color(c.text_muted),
        ))
}

fn enable_rows(st: &AppState, c: &UiColors) -> Vec<Element<Msg>> {
    let on = st.draft_bool("/remote/enabled");
    let port = int_label(st.draft_num("/remote/port"));
    let lan_only = st.draft_bool("/remote/lan_only");
    let mut rows: Vec<Element<Msg>> = Vec::new();
    rows.push(
        row()
            .w_full()
            .items_center()
            .justify_between()
            .gap(space::MD)
            .children((
                col().gap(2.0).grow().children((
                    text("启用局域网访问")
                        .size(TextSize::Sm)
                        .color(c.text),
                    text("开启后本机起一个只有局域网能访问的 Web 服务，端口可到设置页修改。")
                        .size(TextSize::Xs)
                        .color(c.text_subtle),
                )),
                switch(on).on_toggle(Msg::RemoteToggle).id("remote-enabled"),
            ))
            .into(),
    );
    rows.push(
        row()
            .w_full()
            .items_center()
            .gap(space::SM)
            .children((
                chip(format!("端口 {port}"), c.text_muted, c.selection),
                chip(
                    if lan_only { "仅局域网" } else { "允许公网" },
                    if lan_only { c.success } else { c.warning },
                    if lan_only { c.success_bg } else { c.warning_bg },
                ),
            ))
            .into(),
    );
    if !on {
        rows.push(
            text("当前关闭：手机无法访问。上面的开关只是本地配置，保存或重启后才能完全生效。")
                .size(TextSize::Xs)
                .color(c.text_subtle)
                .leading(1.5)
                .into(),
        );
    }
    rows
}

fn address_rows(st: &AppState, c: &UiColors) -> Vec<Element<Msg>> {
    let url = st.remote.url.trim().to_string();
    let mut rows: Vec<Element<Msg>> = Vec::new();
    rows.push(
        text("访问地址")
            .size(TextSize::Xs)
            .weight(Weight::Medium)
            .color(c.text_muted)
            .into(),
    );
    if url.is_empty() {
        rows.push(
            text("还没有生成地址。启用后点右上角“刷新”，会在这里显示局域网地址和配对二维码。")
                .size(TextSize::Xs)
                .color(c.text_subtle)
                .leading(1.5)
                .into(),
        );
    } else {
        let copy_url = url.clone();
        rows.push(
            row()
                .w_full()
                .items_center()
                .gap(space::SM)
                .children((
                    text(url)
                        .size(TextSize::Sm)
                        .mono()
                        .color(c.accent)
                        .grow()
                        .truncate(),
                    icon_button(icons::lucide::copy())
                        .variant(ButtonVariant::Ghost)
                        .size(ControlSize::Xs)
                        .label("复制访问地址")
                        .on_click(Msg::Copy(copy_url))
                        .id("remote-copy-url"),
                ))
                .into(),
        );
    }
    rows
}

fn pairing_rows(st: &AppState, c: &UiColors) -> Vec<Element<Msg>> {
    let now = neosonia_core::util::now_ms();
    let mut rows: Vec<Element<Msg>> = Vec::new();
    rows.push(
        text("扫码配对")
            .size(TextSize::Xs)
            .weight(Weight::Medium)
            .color(c.text_muted)
            .into(),
    );

    let qr: Element<Msg> = match &st.remote.qr_pixels {
        Some((w, h, pixels))
            if *w > 0
                && *h > 0
                && (*w as usize)
                    .checked_mul(*h as usize)
                    .and_then(|n| n.checked_mul(4))
                    .is_some_and(|need| pixels.len() >= need) =>
        {
            image_rgba8(*w, *h, pixels.clone())
                .w(168.0)
                .h(168.0)
                .rounded(8.0)
                .id("remote-qr")
        }
        _ => col()
            .w(168.0)
            .h(168.0)
            .items_center()
            .justify_center()
            .bg(c.bg)
            .border(1.0, c.border_subtle)
            .rounded(8.0)
            .children((
                text("二维码待生成").size(TextSize::Xs).color(c.text_subtle),
            )),
    };
    rows.push(row().w_full().justify_center().children((qr,)).into());

    if st.remote.code.trim().is_empty() {
        rows.push(
            text("还没有配对码：点右上角“刷新”生成一个一次性配对码（默认 10 分钟有效）。")
                .size(TextSize::Xs)
                .color(c.text_subtle)
                .leading(1.5)
                .into(),
        );
    } else {
        let code = st.remote.code.trim().to_string();
        let remain = countdown_label(st.remote.expires_at_ms, now);
        let expired = st.remote.expires_at_ms > 0 && st.remote.expires_at_ms <= now;
        let copy_code = code.clone();
        rows.push(
            row()
                .w_full()
                .items_center()
                .gap(space::SM)
                .children((
                    col().gap(2.0).grow().children((
                        text("配对码")
                            .size(TextSize::Xs)
                            .color(c.text_subtle),
                        text(code.clone())
                            .size(TextSize::Xl)
                            .weight(Weight::Semibold)
                            .mono()
                            .tracking(0.08)
                            .color(c.text),
                    )),
                    col().gap(2.0).items_end().children((
                        chip(
                            format!("剩余 {remain}"),
                            if expired { c.danger } else { c.success },
                            if expired { c.danger_bg } else { c.success_bg },
                        ),
                        icon_button(icons::lucide::copy())
                            .variant(ButtonVariant::Ghost)
                            .size(ControlSize::Xs)
                            .label("复制配对码")
                            .on_click(Msg::Copy(copy_code))
                            .id("remote-copy-code"),
                    )),
                ))
                .into(),
        );
        if st.remote.expires_at_ms > 0 && st.remote.expires_at_ms <= now {
            rows.push(
                button("重新生成配对码")
                    .variant(ButtonVariant::Secondary)
                    .size(ControlSize::Sm)
                    .on_click(Msg::RemoteRefresh)
                    .id("remote-refresh-pairing")
                    .into(),
            );
        }
    }
    rows
}

/// 把字符串裁到 `max` 个字符，超出补省略号。
fn clip(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// 从 User-Agent 里提取“原机型”摘要（新设备配对时记录）。
///
/// Android/iOS 的 UA 一般形如 `Mozilla/5.0 (iPhone; CPU iPhone OS ...)`，
/// 取第一个括号里的第一段；其余情况退化成 UA 前 36 字。
fn ua_model(user_agent: &str) -> String {
    let ua = user_agent.trim();
    if ua.is_empty() {
        return "未知机型".to_string();
    }
    if let Some(start) = ua.find('(') {
        if let Some(rest) = ua.get(start + 1..) {
            let segment = rest.split([';', ')']).next().unwrap_or("").trim();
            if !segment.is_empty() {
                return clip(segment, 32);
            }
        }
    }
    clip(ua, 36)
}

/// 设备名长度上限：必须与 web 端 `DeviceStore::MAX_NAME_CHARS = 32` 保持一致。
const DEVICE_NAME_MAX_CHARS: usize = 32;

/// 设备自定义名：1-32 字符、不能含换行/制表符。
fn valid_device_name(name: &str) -> bool {
    let name = name.trim();
    !name.is_empty()
        && name.chars().count() <= DEVICE_NAME_MAX_CHARS
        && !name.chars().any(|ch| ch == '\n' || ch == '\r' || ch == '\t')
}

fn device_row(
    st: &AppState,
    c: &UiColors,
    d: &neosonia_core::TrustedDevice,
    index: usize,
) -> Element<Msg> {
    let now = neosonia_core::util::now_ms();
    let name = d.display();
    let seen = age_label(now, d.last_seen_ms);
    let model = ua_model(&d.user_agent);
    let mut meta = format!("最近在线 {seen}");
    if !d.last_addr.trim().is_empty() {
        meta.push_str(&format!(" · {}", d.last_addr));
    }
    meta.push_str(&format!(" · 原机型：{model}"));
    let id = d.id.clone();
    let editing = st.device_rename_id == d.id;

    let mut body: Vec<Element<Msg>> = vec![
        row().w_full().items_center().gap(space::SM).children((
            text(name)
                .size(TextSize::Sm)
                .color(c.text)
                .grow()
                .truncate(),
            chip(
                if d.online { "在线" } else { "离线" },
                if d.online { c.success } else { c.text_subtle },
                if d.online { c.success_bg } else { c.selection },
            ),
        ))
        .into(),
        text(meta)
            .size(TextSize::Xs)
            .color(c.text_subtle)
            .truncate()
            .into(),
    ];

    if editing {
        let input: Element<Msg> = text_input(st.device_rename_input.clone())
            .placeholder("设备名，例如 客厅 iPad")
            .width(220.0)
            .on_input(Msg::DeviceRenameChanged)
            .id(&format!("remote-rename-input-{index}"))
            .into();
        let valid = valid_device_name(&st.device_rename_input);
        body.push(input.w_full().min_w(0.0));
        body.push(
            row()
                .w_full()
                .items_center()
                .gap(space::SM)
                .wrap()
                .children((
                    button("保存名称")
                        .variant(ButtonVariant::Primary)
                        .size(ControlSize::Xs)
                        .disabled(!valid)
                        .on_click(Msg::DeviceRename {
                            id: id.clone(),
                            name: st.device_rename_input.clone(),
                        })
                        .id(&format!("remote-rename-save-{index}")),
                    button("取消")
                        .variant(ButtonVariant::Ghost)
                        .size(ControlSize::Xs)
                        .on_click(Msg::DeviceRenameCancel)
                        .id(&format!("remote-rename-cancel-{index}")),
                    text(if valid {
                        "1-32 个字符，常见中文/英文/emoji 均可。"
                    } else {
                        "名称需 1-32 字符，且不能含换行/制表符。"
                    })
                    .size(TextSize::Xs)
                    .color(if valid { c.text_subtle } else { c.danger })
                    .truncate(),
                    text(format!(
                        "{}/{DEVICE_NAME_MAX_CHARS}",
                        st.device_rename_input.trim().chars().count()
                    ))
                    .size(TextSize::Xs)
                    .tabular()
                    .color(if valid { c.text_subtle } else { c.danger })
                    .shrink0(),
                ))
                .into(),
        );
    }

    let mut actions: Vec<Element<Msg>> = Vec::new();
    if !editing {
        actions.push(
            button("重命名")
                .variant(ButtonVariant::Ghost)
                .size(ControlSize::Xs)
                .on_click(Msg::DeviceRenameStart(id.clone()))
                .id(&format!("remote-rename-{index}"))
                .into(),
        );
    }
    actions.push(
        button("撤销")
            .variant(ButtonVariant::Ghost)
            .size(ControlSize::Xs)
            .on_click(Msg::RemoteRevoke(id))
            .id(&format!("remote-revoke-{index}"))
            .into(),
    );

    row()
        .w_full()
        .items_start()
        .gap(space::SM)
        .py(space::SM)
        .children((
            col().pt(6.0).children((dot(if d.online { c.success } else { c.text_subtle }),)),
            col().grow().min_w(0.0).gap(2.0).children(body),
            col().items_end().gap(space::XS).shrink0().children(actions),
        ))
}

fn devices_rows(st: &AppState, c: &UiColors) -> Vec<Element<Msg>> {
    let mut rows: Vec<Element<Msg>> = Vec::new();
    rows.push(
        row().w_full().items_center().gap(space::SM).children((
            text("已配对设备")
                .size(TextSize::Xs)
                .weight(Weight::Medium)
                .color(c.text_muted),
            chip(format!("{}", st.remote.devices.len()), c.text_subtle, c.selection),
        )).into(),
    );
    if st.remote.devices.is_empty() {
        rows.push(
            text("还没有设备。手机扫码配对后会出现在这里，可随时撤销。")
                .size(TextSize::Xs)
                .color(c.text_subtle)
                .leading(1.5)
                .into(),
        );
    } else {
        for (i, d) in st.remote.devices.iter().enumerate() {
            rows.push(device_row(st, c, d, i));
            if i + 1 < st.remote.devices.len() {
                rows.push(divider());
            }
        }
    }
    rows
}

// ============================================================
// 面板入口
// ============================================================

pub fn view(st: &AppState, c: &UiColors) -> Element<Msg> {
    let mut children: Vec<Element<Msg>> = vec![header(st, c), card(c, enable_rows(st, c))];

    if !st.remote.enabled && !st.draft_bool("/remote/enabled") {
        children.push(
            card(
                c,
                vec![
                    row().items_center().gap(space::SM).children((
                        dot(c.text_subtle),
                        text("局域网访问未开启")
                            .size(TextSize::Sm)
                            .weight(Weight::Semibold)
                            .color(c.text),
                    )).into(),
                    text("打开上面的开关并保存后，NeoSonia 会在本机监听一个局域网端口；手机连同一个 Wi-Fi 即可访问。")
                        .size(TextSize::Xs)
                        .color(c.text_muted)
                        .leading(1.6)
                        .into(),
                ],
            ),
        );
    } else {
        let mut status = vec![status_row(st, c), divider()];
        status.extend(address_rows(st, c));
        status.push(divider());
        status.extend(pairing_rows(st, c));
        status.push(
            text("手机和电脑必须连同一个局域网；配对码一次性有效，过期后重新生成。")
                .size(TextSize::Xs)
                .color(c.text_subtle)
                .leading(1.5)
                .into(),
        );
        children.push(card(c, status));
        children.push(card(c, devices_rows(st, c)));
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
    fn countdown_formats_and_expires() {
        assert_eq!(countdown_label(10_000, 0), "00:10");
        assert_eq!(countdown_label(600_000, 0), "10:00");
        assert_eq!(countdown_label(65_000, 1_000), "01:04");
        assert_eq!(countdown_label(1_000, 1_000), "已过期");
        assert_eq!(countdown_label(0, 5_000), "已过期");
    }

    #[test]
    fn labels_are_stable() {
        assert_eq!(int_label(8787.0), "8787");
        assert_eq!(int_label(-1.0), "-1");
        assert_eq!(int_label(f64::INFINITY), "0");
        assert_eq!(age_label(1_000_000_000_000, 0), "从未");
        assert_eq!(age_label(1_000_000_000_000, 1_000_000_000_000 - 61_000), "1 分钟前");
        assert_eq!(age_label(1_000_000_000_000, 1_000_000_000_000 - 2 * 3_600_000), "2 小时前");
    }

    #[test]
    fn device_model_and_name_validation() {
        // 原机型摘要：iPhone / Android / Windows 桌面。
        assert_eq!(
            ua_model("Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1"),
            "iPhone"
        );
        assert_eq!(
            ua_model("Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36"),
            "Linux"
        );
        assert_eq!(
            ua_model("Mozilla/5.0 (Windows NT 10.0; Win64; x64) Chrome/126.0"),
            "Windows NT 10.0"
        );
        assert_eq!(ua_model(""), "未知机型");
        // 超长 UA 会截断，不会撑破窄栏。
        let long_ua = format!("Mozilla/5.0 ({})", "x".repeat(120));
        assert!(ua_model(&long_ua).chars().count() <= 32);

        // 设备名：1-32 字符（与 web DeviceStore::MAX_NAME_CHARS 一致），
        // 允许中英文/emoji，拒绝空/超长/换行。
        assert!(valid_device_name("客厅 iPad"));
        assert!(valid_device_name("iPhone 15 Pro"));
        assert!(valid_device_name("📱 手机"));
        // 边界：32 字符合法，33 字符不合法（旧上限 40 会让 store 拒绝）。
        assert!(valid_device_name(&"x".repeat(DEVICE_NAME_MAX_CHARS)));
        assert!(!valid_device_name(&"x".repeat(DEVICE_NAME_MAX_CHARS + 1)));
        assert!(!valid_device_name(&"x".repeat(41)));
        assert!(!valid_device_name(""));
        assert!(!valid_device_name("   "));
        assert!(!valid_device_name("a\nb"));
        assert!(!valid_device_name("a\tb"));
    }
}
