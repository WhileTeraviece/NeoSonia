//! 设置面板：滚动表单 + 分组卡片 + 底部固定保存条。
//!
//! 所有输入都直接读写 `AppState::draft`（JSON Pointer），保存由 app 层负责落盘。
//! 布尔用 switch，枚举用 select，数字用 TextInput + 本地解析（解析失败时保持原值，
//! 避免每帧把用户正在输入的字符覆盖掉）。
//!
//! 视觉对齐 `docs/ui-design.md`：单列、卡片 `surface_raised` + 1px 发丝边、
//! 圆角 12、间距 4/8/12/16，零 emoji，状态一律"色点 + 文字"。

use fenestra::prelude::*;
use neosonia_core::config::{McpServerConfig, McpTransport};

use crate::state::{AppState, Msg, NavPage, TestTarget};
use crate::theme::{space, UiColors, R_CARD, R_CHIP};

/// 设置页根容器 id（固定 footer + 内部滚动区）。
const PANEL_ID: &str = "panel-settings";
/// 可滚动的表单区 id（必须稳定，滚动位置按它持久化）。
const FORM_ID: &str = "panel-settings-scroll";

// ============================================================
// 纯函数（可单测，不依赖 Element）
// ============================================================

/// 数字的显示形式：整数不带小数点，小数去掉尾随 0。
///
/// 草稿里的 f32（例如 temperature 0.6）经 serde_json 会变成
/// `0.6000000238418579`，所以这里统一四舍五入到 6 位小数再裁剪。
fn fmt_num(v: f64) -> String {
    if !v.is_finite() {
        return "0".to_string();
    }
    if (v.fract()).abs() < 1e-9 {
        return format!("{}", v as i64);
    }
    let s = format!("{v:.6}");
    let s = s.trim_end_matches('0').trim_end_matches('.').to_string();
    if s.is_empty() { "0".to_string() } else { s }
}

/// 从草稿读取数字并格式化（表单行的"显示值"就由它构造）。
fn number_text(st: &AppState, path: &str) -> String {
    fmt_num(st.draft_num(path))
}

/// 当前值在选项里的下标；找不到时回到第一项。
fn select_index(value: &str, values: &[&str]) -> usize {
    values.iter().position(|v| *v == value).unwrap_or(0)
}

/// 白名单输入解析：按逗号/分号/换行/空白拆分，小写、去重、丢弃空项。
fn parse_whitelist_input(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in text.split([',', ';', '\n', '\r', '\t', ' ']) {
        let item = raw.trim().to_ascii_lowercase();
        if item.is_empty() {
            continue;
        }
        if !out.iter().any(|x| x == &item) {
            out.push(item);
        }
    }
    out
}

/// 单条白名单地址的格式校验：`user@host` 精确匹配，或 `@domain` 后缀匹配。
fn valid_whitelist_entry(addr: &str) -> bool {
    let s = addr.trim();
    if s.is_empty() || s.chars().any(char::is_whitespace) {
        return false;
    }
    if let Some(domain) = s.strip_prefix('@') {
        return !domain.is_empty() && !domain.contains('@');
    }
    match s.split_once('@') {
        Some((user, host)) => !user.is_empty() && !host.is_empty() && !host.contains('@'),
        None => false,
    }
}

/// 索引根目录输入解析：一行一个路径，去空白/空行/注释/重复。
fn parse_root_lines(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !out.iter().any(|x| x == line) {
            out.push(line.to_string());
        }
    }
    out
}

/// MCP 传输方式的协议标签。
fn mcp_transport_label(t: McpTransport) -> &'static str {
    match t {
        McpTransport::Stdio => "stdio",
        McpTransport::Http => "http",
    }
}

/// 把字符串裁到 `max` 个字符，超出补省略号。
fn short_chars(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// MCP 服务器卡片的第二行：stdio 显示命令+参数，http 显示 URL。
fn mcp_summary(cfg: &McpServerConfig) -> String {
    match cfg.transport {
        McpTransport::Http => {
            let url = cfg.url.trim();
            if url.is_empty() {
                "(未配置 URL)".to_string()
            } else {
                url.to_string()
            }
        }
        McpTransport::Stdio => {
            let command = cfg.command.trim();
            if command.is_empty() {
                return "(未配置命令)".to_string();
            }
            let mut s = command.to_string();
            // 只拼前 16 个参数，避免每帧对超长参数列表做 O(n) 大串拼接。
            for arg in cfg.args.iter().take(16) {
                s.push(' ');
                s.push_str(arg);
                if s.chars().count() > 96 {
                    break;
                }
            }
            short_chars(&s, 96)
        }
    }
}

/// 工作模式 id -> 中文名。
fn mode_label(id: &str) -> &'static str {
    match id.trim() {
        "coding" => "编程",
        "computer_use" => "计算机使用",
        _ => "日常",
    }
}

/// reasoning_effort 字符串 -> 滑块下标（0 不思考 / 1 low / 2 high / 3 max）。
///
/// 空串 = 不发送该参数（不思考）；旧配置的 `medium` 兼容映射到 high 档显示。
fn reasoning_index(value: &str) -> usize {
    match value.trim() {
        "" => 0,
        "low" => 1,
        "high" | "medium" => 2,
        "max" => 3,
        // 未知值按默认 high 显示，避免滑块落到意外档位。
        _ => 2,
    }
}

/// 滑块值 -> reasoning_effort 字符串（四舍五入到四档；0 = 空字符串表示不发送）。
fn reasoning_value(v: f32) -> &'static str {
    match v.round() as i32 {
        i32::MIN..=0 => "",
        1 => "low",
        2 => "high",
        _ => "max",
    }
}

/// 秒数 -> 人类可读有效期（用于只读展示审查 TTL）。
fn ttl_label(secs: u64) -> String {
    if secs < 60 {
        format!("{secs} 秒")
    } else if secs < 3_600 {
        format!("{} 分钟", secs / 60)
    } else if secs < 86_400 {
        format!("{} 小时", secs / 3_600)
    } else {
        format!("{} 天", secs / 86_400)
    }
}

/// token 数 -> 中文可读（“约 70 万 tokens”）。
fn tokens_label(n: u64) -> String {
    if n >= 10_000 {
        format!("约 {:.0} 万 tokens", n as f64 / 10_000.0)
    } else if n >= 1_000 {
        format!("约 {:.0} 千 tokens", n as f64 / 1_000.0)
    } else {
        format!("{n} tokens")
    }
}

/// 只读的设置展示行（用于上下文阈值、上下文长度等不可编辑项）。
fn readonly_field(c: &UiColors, label: &str, value: impl Into<String>, hint: &str) -> Element<Msg> {
    let value = value.into();
    let mut kids: Vec<Element<Msg>> = vec![
        text(label)
            .size(TextSize::Xs)
            .weight(Weight::Medium)
            .color(c.text_muted)
            .into(),
        row()
            .w_full()
            .items_center()
            .p(space::SM)
            .bg(c.bg)
            .border(1.0, c.border_subtle)
            .rounded(R_CHIP)
            .children((
                text(value)
                    .size(TextSize::Sm)
                    .mono()
                    .color(c.text)
                    .grow()
                    .truncate(),
            ))
            .into(),
    ];
    if !hint.is_empty() {
        kids.push(text(hint).size(TextSize::Xs).color(c.text_subtle).into());
    }
    col().w_full().gap(space::XS + 2.0).children(kids)
}

// ---- 折叠面板标题摘要（常数级字符串，不扫描大列表） ----

fn skills_summary(st: &AppState) -> String {
    let n = st.skills.len();
    if n == 0 {
        "未安装技能".to_string()
    } else {
        format!("已安装 {n} 个 · 当前模式：{}", mode_label(&st.agent_mode))
    }
}

fn index_summary(st: &AppState) -> String {
    let s = &st.settings.index;
    if !s.enabled {
        return "未启用".to_string();
    }
    format!(
        "已索引 {} 条 · {}",
        st.index_count,
        if st.index_busy { "扫描中" } else { "空闲" }
    )
}

fn mail_summary(st: &AppState) -> String {
    let oauth = &st.settings.mail.oauth;
    if oauth.enabled || !st.mail_oauth_status.trim().is_empty() {
        let status = if st.mail_oauth_status.trim().is_empty() {
            "未登录".to_string()
        } else {
            short_chars(st.mail_oauth_status.trim(), 40)
        };
        format!("OAuth：{status}")
    } else if !st.settings.mail.imap.host.trim().is_empty() {
        format!(
            "密码登录：{}",
            short_chars(st.settings.mail.imap.host.trim(), 28)
        )
    } else {
        "密码登录：未配置".to_string()
    }
}

fn remote_summary(st: &AppState) -> String {
    if !st.remote.enabled {
        return "未启用".to_string();
    }
    let url = st.remote.url.trim();
    if url.is_empty() {
        "已启用 · 地址未生成".to_string()
    } else {
        format!("已启用 · {}", short_chars(url, 36))
    }
}

fn persona_memory_summary(st: &AppState) -> String {
    let name = st
        .personas
        .iter()
        .find(|p| p.id == st.active_persona)
        .map(|p| p.name.clone())
        .unwrap_or_else(|| st.active_persona.clone());
    let name = if name.trim().is_empty() {
        "默认".to_string()
    } else {
        short_chars(name.trim(), 20)
    };
    format!("人设：{name} · 记忆 {} 条", st.memory_items.len())
}

// ============================================================
// 小部件
// ============================================================

fn dot(color: Color) -> Element<Msg> {
    col().w(6.0).h(6.0).rounded_full().bg(color).shrink0()
}

/// 小号信息标签（技能来源等）。
fn tag(c: &UiColors, label: impl Into<String>) -> Element<Msg> {
    row()
        .h(18.0)
        .items_center()
        .px(space::XS + 2.0)
        .rounded(R_CHIP)
        .bg(c.selection)
        .shrink0()
        .children((text(label).size(TextSize::Xs).weight(Weight::Medium).color(c.text_muted),))
}

/// 卡片内的分组小标题（IMAP 收信 / SMTP 发信 / 白名单 …）。
fn subhead(c: &UiColors, label: &str) -> Element<Msg> {
    text(label)
        .size(TextSize::Xs)
        .weight(Weight::Semibold)
        .color(c.text_subtle)
}

/// 一个分组卡片：标题 + 说明 + 一组行。
///
/// `id` 用于稳定 `.id(...)`，避免滚动/重排时丢失交互状态。
fn section(
    c: &UiColors,
    id: &str,
    title: &str,
    hint: &str,
    rows: Vec<Element<Msg>>,
) -> Element<Msg> {
    let mut head = col()
        .gap(2.0)
        .child(text(title).size(TextSize::Sm).weight(Weight::Semibold).color(c.text));
    if !hint.is_empty() {
        head = head.child(text(hint).size(TextSize::Xs).color(c.text_subtle));
    }
    col()
        .w_full()
        .gap(space::SM)
        .id(&format!("settings-section-{id}"))
        .children([
            head,
            col()
                .w_full()
                .p(space::MD)
                .gap(space::MD)
                .bg(c.surface_raised)
                .border(1.0, c.border_subtle)
                .rounded(R_CARD)
                .id(&format!("settings-section-{id}-card"))
                .children(rows),
        ])
}

/// 可折叠分组：默认折叠（`st.settings_open` 里没有该 id 时不构建 body），
/// 点击标题行发 `Msg::ToggleSection(id)`。
///
/// - `summary`：标题行第二行摘要（不展开也知道里面有什么，成本是常数级字符串）；
/// - `lead`：折叠时也常显的行（例如邮件段的 OAuth 登录入口）；
/// - `body`：展开时才调用的惰性构建闭包。
fn collapsible_section(
    st: &AppState,
    c: &UiColors,
    id: &'static str,
    title: &str,
    summary: &str,
    lead: Vec<Element<Msg>>,
    body: impl FnOnce() -> Vec<Element<Msg>>,
) -> Element<Msg> {
    let open = st.settings_open.iter().any(|x| x.as_str() == id);
    let chevron: Element<Msg> = if open {
        icons::chevron_down().w(16.0).h(16.0).color(c.text_muted)
    } else {
        icons::chevron_right().w(16.0).h(16.0).color(c.text_muted)
    };
    let header = row()
        .w_full()
        .items_center()
        .gap(space::SM)
        .p(space::MD)
        .on_click(Msg::ToggleSection(id.to_string()))
        .cursor(Cursor::Pointer)
        .id(&format!("settings-section-{id}-header"))
        .children((
            chevron,
            col().grow().min_w(0.0).gap(2.0).children((
                text(title)
                    .size(TextSize::Sm)
                    .weight(Weight::Semibold)
                    .color(c.text),
                if summary.is_empty() {
                    col().h(0.0)
                } else {
                    text(summary.to_string())
                        .size(TextSize::Xs)
                        .color(c.text_subtle)
                        .truncate()
                },
            )),
            tag(c, if open { "收起" } else { "展开" }),
        ));

    let mut card = col()
        .w_full()
        .bg(c.surface_raised)
        .border(1.0, c.border_subtle)
        .rounded(R_CARD)
        .id(&format!("settings-section-{id}"))
        .child(header);

    let has_lead = !lead.is_empty();
    if has_lead || open {
        let mut content = lead;
        if open {
            content.extend(body());
        }
        card = card.child(divider()).child(
            col()
                .w_full()
                .p(space::MD)
                .gap(space::MD)
                .children(content),
        );
    }
    card
}

/// 轻量内嵌折叠（OAuth 诊断 / 高级排障等段内小节）。
///
/// 与 `collapsible_section` 共用 `st.settings_open` 与 `Msg::ToggleSection`，
/// 但没有独立卡片外观，只多一行标题 + 展开后的 body。
fn fold_section(
    st: &AppState,
    c: &UiColors,
    id: &'static str,
    title: &str,
    summary: &str,
    body: impl FnOnce() -> Vec<Element<Msg>>,
) -> Element<Msg> {
    let open = st.settings_open.iter().any(|x| x.as_str() == id);
    let chevron: Element<Msg> = if open {
        icons::chevron_down().w(14.0).h(14.0).color(c.text_muted)
    } else {
        icons::chevron_right().w(14.0).h(14.0).color(c.text_muted)
    };
    let mut rows: Vec<Element<Msg>> = vec![row()
        .w_full()
        .items_center()
        .gap(space::XS)
        .py(space::XS)
        .on_click(Msg::ToggleSection(id.to_string()))
        .cursor(Cursor::Pointer)
        .id(&format!("settings-fold-{id}-header"))
        .children((
            chevron,
            text(title)
                .size(TextSize::Xs)
                .weight(Weight::Medium)
                .color(c.text_muted),
            col().grow().min_w(0.0).children((
                text(summary)
                    .size(TextSize::Xs)
                    .color(c.text_subtle)
                    .truncate(),
            )),
        ))
        .into()];
    if open {
        rows.push(
            col()
                .w_full()
                .pl(space::MD)
                .gap(space::SM)
                .id(&format!("settings-fold-{id}-body"))
                .children(body())
                .into(),
        );
    }
    col()
        .w_full()
        .gap(2.0)
        .id(&format!("settings-fold-{id}"))
        .children(rows)
}

/// OAuth 权限项展示文案；`unknown` 表示 token 不是 JWT / 还没登录，无法判断。
fn oauth_perm_label(has: bool, unknown: bool) -> &'static str {
    if unknown {
        "未知"
    } else if has {
        "有"
    } else {
        "缺失"
    }
}

/// 列表拼接；空列表显示“未知”。超长截断，避免撑破窄栏。
fn joined_or_unknown(items: &[String]) -> String {
    if items.is_empty() {
        return "未知".to_string();
    }
    short_chars(&items.join("，"), 160)
}

/// OAuth 登录态文案：重启后 access_token 可能为空，但 refresh_token 还在。
///
/// 返回值 `(文案, 是否健康)`；needs_reauth 优先于其它状态。
fn oauth_login_state(
    needs_reauth: bool,
    refresh_token: &str,
    access_token: &str,
    enabled: bool,
) -> (&'static str, bool) {
    let has_refresh = !refresh_token.trim().is_empty();
    let has_access = !access_token.trim().is_empty();
    if needs_reauth {
        ("需要重新授权", false)
    } else if has_refresh && !has_access {
        ("已登录（token 待刷新）", true)
    } else if has_refresh && has_access {
        ("已登录", true)
    } else if enabled {
        ("已启用，尚未登录", false)
    } else {
        ("未登录", false)
    }
}

/// 设置页顶部的“设备入口”提示条（只在局域网服务未启用时出现）。
fn device_entry_hint(c: &UiColors) -> Element<Msg> {
    row()
        .w_full()
        .items_center()
        .gap(space::SM)
        .p(space::MD)
        .bg(c.accent.with_alpha(0.10))
        .border(1.0, c.accent.with_alpha(0.35))
        .rounded(R_CARD)
        .on_click(Msg::OpenPage(NavPage::Remote))
        .cursor(Cursor::Pointer)
        .id("settings-device-hint")
        .children((
            dot(c.accent),
            col().grow().min_w(0.0).gap(2.0).children((
                text("没有找到设备入口？")
                    .size(TextSize::Xs)
                    .weight(Weight::Medium)
                    .color(c.text),
                text("在「设备」页开启局域网访问，手机扫码即可连接。")
                    .size(TextSize::Xs)
                    .color(c.text_muted)
                    .truncate(),
            )),
            icons::chevron_right().w(16.0).h(16.0).color(c.accent),
        ))
}

/// 标签 + 控件 + 说明的纵向表单行。
fn labeled(c: &UiColors, label: &str, hint: &str, control: Element<Msg>) -> Element<Msg> {
    let mut kids: Vec<Element<Msg>> = vec![
        text(label)
            .size(TextSize::Xs)
            .weight(Weight::Medium)
            .color(c.text_muted)
            .into(),
        control,
    ];
    if !hint.is_empty() {
        kids.push(text(hint).size(TextSize::Xs).color(c.text_subtle).into());
    }
    col().w_full().gap(space::XS + 2.0).children(kids)
}

fn page_header(c: &UiColors) -> Element<Msg> {
    col()
        .w_full()
        .gap(2.0)
        .children((
            text("设置")
                .size(TextSize::Lg)
                .weight(Weight::Semibold)
                .color(c.text),
            text("模型、工具与邮件都在这里配置。改完点底部保存，带重启提示的项下次启动生效。")
                .size(TextSize::Xs)
                .color(c.text_muted),
        ))
}

// ---- 表单行 ----

fn text_field(
    st: &AppState,
    c: &UiColors,
    path: &'static str,
    label: &str,
    hint: &str,
    placeholder: &str,
) -> Element<Msg> {
    let value = st.draft_str(path);
    let id = format!("settings{path}");
    let input: Element<Msg> = text_input(value)
        .placeholder(placeholder)
        .width(320.0)
        .on_input(move |s| Msg::set(path, s))
        .id(&id)
        .into();
    labeled(c, label, hint, input.w_full().min_w(0.0))
}

fn num_field(
    st: &AppState,
    c: &UiColors,
    path: &'static str,
    label: &str,
    hint: &str,
    unit: &str,
) -> Element<Msg> {
    let shown = number_text(st, path);
    let current = st.draft_num(path);
    let id = format!("settings{path}");
    let mut builder = text_input(shown)
        .placeholder("0")
        .width(320.0)
        .on_input(move |s| {
            let t = s.trim();
            match t.parse::<f64>() {
                Ok(v) => Msg::set(path, v),
                // 空串或半截输入（例如 "0."）：保持原值，不打断正在输入的内容。
                Err(_) => Msg::set(path, current),
            }
        })
        .id(&id);
    if !unit.is_empty() {
        builder = builder.suffix(text(unit).size(TextSize::Xs).color(c.text_subtle));
    }
    let input: Element<Msg> = builder.into();
    // min_w(0) + w_full：放进两列布局时允许收缩，不会把右列顶出卡片。
    labeled(c, label, hint, input.w_full().min_w(0.0))
}

fn switch_field(
    st: &AppState,
    c: &UiColors,
    path: &'static str,
    label: &str,
    desc: &str,
) -> Element<Msg> {
    let on = st.draft_bool(path);
    let msg = Msg::set(path, !on);
    let id = format!("settings{path}");
    let mut left = col()
        .gap(2.0)
        .grow()
        .child(text(label).size(TextSize::Sm).color(c.text));
    if !desc.is_empty() {
        left = left.child(text(desc).size(TextSize::Xs).color(c.text_subtle));
    }
    row()
        .w_full()
        .min_h(44.0)
        .items_center()
        .justify_between()
        .gap(space::MD)
        .children((left, switch(on).on_toggle(msg).id(&id)))
}

fn select_field(
    st: &AppState,
    c: &UiColors,
    path: &'static str,
    label: &str,
    hint: &str,
    options: &'static [(&'static str, &'static str)],
) -> Element<Msg> {
    let current = st.draft_str(path);
    let values: Vec<&'static str> = options.iter().map(|(v, _)| *v).collect();
    let index = select_index(&current, &values);
    let labels: Vec<String> = options.iter().map(|(_, l)| (*l).to_string()).collect();
    let ids = values.clone();
    let id = format!("settings{path}");
    let sel: Element<Msg> = select(index, labels)
        .width(320.0)
        .on_change(move |i| Msg::set(path, ids.get(i).copied().unwrap_or_default()))
        .id(&id)
        .into();
    labeled(c, label, hint, sel.w_full())
}

/// 思考强度：4 档滑块（不思考 / 低 / 高 / 最高），写 `/server/reasoning_effort`。
fn thinking_field(st: &AppState, c: &UiColors) -> Element<Msg> {
    let current = st.draft_str("/server/reasoning_effort");
    let index = reasoning_index(&current);
    let label = match index {
        0 => "不思考",
        1 => "低",
        2 => "高",
        _ => "最高（max）",
    };
    let slider: Element<Msg> = slider(index as f32)
        .range(0.0, 3.0)
        .step(1.0)
        .marks(true)
        .width(300.0)
        .on_change(|v| Msg::set("/server/reasoning_effort", reasoning_value(v)))
        .id("settings-thinking-slider")
        .into();

    let control = row()
        .w_full()
        .items_center()
        .gap(space::MD)
        .children((
            col().grow().min_w(0.0).children((slider,)),
            col()
                .items_end()
                .gap(space::XS)
                .shrink0()
                .children((
                    text(format!("当前：{label}"))
                        .size(TextSize::Xs)
                        .weight(Weight::Medium)
                        .color(c.accent)
                        .truncate(),
                    button("不思考")
                        .variant(ButtonVariant::Ghost)
                        .size(ControlSize::Xs)
                        .on_click(Msg::set("/server/reasoning_effort", ""))
                        .id("settings-thinking-clear"),
                )),
        ));
    labeled(
        c,
        "思考强度",
        "对应 reasoning_effort 的 none/low/high/max；不思考=不发送该参数（默认 high）；只对支持该参数的模型生效，点「不思考」清空。",
        control,
    )
}

// ---- 连通性测试 ----

fn test_status(st: &AppState, c: &UiColors, target: TestTarget) -> Element<Msg> {
    if st.test.target == Some(target) && st.test.running {
        return row()
            .items_center()
            .gap(space::XS)
            .children((
                spinner(),
                text("正在测试…").size(TextSize::Xs).color(c.text_muted),
            ))
            .into();
    }
    if st.test.target == Some(target) {
        let ok = st.test.ok;
        let head_color = if ok { c.success } else { c.danger };
        let summary = if !st.test.summary.trim().is_empty() {
            st.test.summary.clone()
        } else if !st.test.message.trim().is_empty() {
            st.test.message.clone()
        } else if ok {
            "连接成功".to_string()
        } else {
            "连接失败".to_string()
        };
        let mut lines: Vec<Element<Msg>> = vec![row()
            .w_full()
            .items_start()
            .gap(space::XS)
            .children((
                col().pt(5.0).children((dot(head_color),)),
                text(neosonia_core::util::truncate_chars(&summary, 96))
                    .size(TextSize::Xs)
                    .color(if ok { c.text_muted } else { c.danger })
                    .leading(1.4)
                    .grow()
                    .truncate(),
            ))
            .into()];

        // IMAP / SMTP 各自的结果（成功绿点、失败红点）。
        if target != TestTarget::Llm {
            for (name, ok_one, message) in [
                ("IMAP", st.test.imap_ok, st.test.imap_message.as_str()),
                ("SMTP", st.test.smtp_ok, st.test.smtp_message.as_str()),
            ] {
                if message.trim().is_empty() {
                    continue;
                }
                lines.push(
                    row()
                        .w_full()
                        .items_start()
                        .gap(space::XS)
                        .children((
                            col().pt(5.0).children((
                                dot(if ok_one { c.success } else { c.danger }),
                            )),
                            text(format!(
                                "{name}：{}",
                                neosonia_core::util::truncate_chars(message.trim(), 88)
                            ))
                            .size(TextSize::Xs)
                            .color(if ok_one { c.text_muted } else { c.danger })
                            .leading(1.4)
                            .grow()
                            .truncate(),
                        ))
                        .into(),
                );
            }

            // OAuth 诊断：needs_reauth 优先，其次 audience / scopes。
            if st.mail_oauth_needs_reauth {
                lines.push(
                    text(format!(
                        "需要重新授权：{}",
                        neosonia_core::util::truncate_chars(
                            if st.mail_oauth_status_message.trim().is_empty() {
                                "授权已失效或权限不足，请重新登录。"
                            } else {
                                st.mail_oauth_status_message.trim()
                            },
                            100
                        )
                    ))
                    .size(TextSize::Xs)
                    .color(c.danger)
                    .leading(1.4)
                    .truncate()
                    .into(),
                );
            }
            let audience = joined_or_unknown(&st.mail_oauth_audience);
            let scopes = joined_or_unknown(&st.mail_oauth_scopes);
            if audience != "未知" || scopes != "未知" {
                lines.push(
                    text(format!("受众：{audience}"))
                        .size(TextSize::Xs)
                        .color(c.text_subtle)
                        .truncate()
                        .into(),
                );
                lines.push(
                    text(format!("权限：{scopes}"))
                        .size(TextSize::Xs)
                        .color(c.text_subtle)
                        .truncate()
                        .into(),
                );
                lines.push(
                    text(format!(
                        "IMAP 权限：{} · SMTP 权限：{}",
                        oauth_perm_label(
                            st.mail_oauth_has_imap_scope,
                            st.mail_oauth_scopes.is_empty()
                        ),
                        oauth_perm_label(
                            st.mail_oauth_has_smtp_scope,
                            st.mail_oauth_scopes.is_empty()
                        ),
                    ))
                    .size(TextSize::Xs)
                    .color(c.text_subtle)
                    .truncate()
                    .into(),
                );
            }
        }

        return col()
            .w_full()
            .gap(2.0)
            .children(lines)
            .into();
    }
    row()
        .items_center()
        .gap(space::XS)
        .children((
            dot(c.text_subtle.with_alpha(0.5)),
            text("尚未测试").size(TextSize::Xs).color(c.text_subtle),
        ))
        .into()
}

fn test_row(st: &AppState, c: &UiColors, target: TestTarget) -> Element<Msg> {
    let id = format!("settings-test-{}", target.label());
    row()
        .w_full()
        .items_start()
        .gap(space::SM)
        .children((
            button(format!("测试{}", target.label()))
                .variant(ButtonVariant::Secondary)
                .size(ControlSize::Sm)
                .on_click(Msg::TestConnection(target))
                .id(&id),
            // 状态文本可能很长（服务端返回的模型列表/错误），放在可收缩的列里截断，
            // 避免把整行撑出面板右侧。
            col()
                .grow()
                .min_w(0.0)
                .children((test_status(st, c, target),)),
        ))
}

// ============================================================
// 分组内容
// ============================================================

fn model_section(st: &AppState, c: &UiColors) -> Element<Msg> {
    let rows: Vec<Element<Msg>> = vec![
        text_field(
            st,
            c,
            "/server/base_url",
            "接口地址",
            "OpenAI 兼容端点，写到 /v1 为止，不要带 /chat/completions。",
            "https://api.deepseek.com/v1",
        ),
        text_field(
            st,
            c,
            "/server/model",
            "模型名",
            "由服务端识别，例如 deepseek-chat、gpt-4.1、qwen3。",
            "deepseek-chat",
        ),
        text_field(
            st,
            c,
            "/server/api_key",
            "API 密钥",
            "密钥仅存本机配置，不会写入日志或聊天记录；也支持 env:DEEPSEEK_API_KEY 写法。",
            "sk-...",
        ),
        row()
            .w_full()
            .items_start()
            .gap(space::MD)
            .children((
                // 两列都 min_w(0)：窄栏下可收缩，避免右列输入框顶出卡片。
                col().grow().min_w(0.0).children((num_field(
                    st,
                    c,
                    "/server/temperature",
                    "温度",
                    "0 – 2，越高越发散；思考模型可能忽略。",
                    "",
                ),)),
                col().grow().min_w(0.0).children((num_field(
                    st,
                    c,
                    "/server/max_tokens",
                    "最大输出 token",
                    "默认 32768；DeepSeek 单次输出上限 384K（393216 tokens）。",
                    "",
                ),)),
            )),
        readonly_field(
            c,
            "上下文长度",
            tokens_label(st.settings.server.context_window as u64),
            "上下文窗口，用于裁剪历史；只读（在 TOML 的 [server] 段修改）。",
        ),
        thinking_field(st, c),
        subhead(c, "上下文压缩"),
        switch_field(
            st,
            c,
            "/agent/context_compress",
            "超长对话自动压缩",
            "对话超过阈值时，旧消息会被自动摘要，保留最近若干条原文，避免上下文溢出。",
        ),
        readonly_field(
            c,
            "压缩阈值",
            tokens_label(st.settings.agent.context_threshold_tokens),
            "超过这个 token 数触发摘要。",
        ),
        readonly_field(
            c,
            "保留最近消息",
            format!("{} 条", st.settings.agent.keep_recent_messages),
            "压缩时永远保留最近这些条消息原文。",
        ),
        test_row(st, c, TestTarget::Llm),
    ];
    section(
        c,
        "model",
        "模型服务",
        "任何 OpenAI 兼容端点：DeepSeek / OpenAI / Ollama / vLLM / OpenRouter。",
        rows,
    )
}

fn ui_section(st: &AppState, c: &UiColors) -> Element<Msg> {
    let hotkey = st.draft_str("/ui/hotkey");
    let hotkey_input: Element<Msg> = text_input(hotkey)
        .placeholder("Alt+Space")
        .width(220.0)
        .on_input(|s| Msg::set("/ui/hotkey", s))
        .id("settings-hotkey")
        .into();
    let apply_row = row()
        .w_full()
        .items_center()
        .gap(space::SM)
        .children((
            col().grow().children((hotkey_input.w_full(),)),
            button("应用快捷键")
                .variant(ButtonVariant::Secondary)
                .size(ControlSize::Sm)
                .on_click(Msg::HotkeyApply)
                .id("settings-hotkey-apply"),
        ));

    let rows: Vec<Element<Msg>> = vec![
        select_field(
            st,
            c,
            "/ui/theme",
            "主题",
            "跟随系统会在系统深浅色切换时自动换主题。",
            &[("dark", "深色"), ("light", "浅色"), ("system", "跟随系统")],
        ),
        labeled(
            c,
            "全局快捷键",
            "支持的写法：Ctrl / Alt / Shift / Win（或 Cmd）+ 键名，例如 Alt+Space、Ctrl+Shift+Space、F9。",
            apply_row,
        ),
        num_field(
            st,
            c,
            "/ui/width",
            "浮窗宽度",
            "逻辑像素，默认 424；改动重启后生效。",
            "px",
        ),
        num_field(
            st,
            c,
            "/ui/height_ratio",
            "屏占比",
            "浮窗高度占工作区比例，0.5 – 1.0；重启后生效。",
            "",
        ),
        num_field(
            st,
            c,
            "/ui/margin",
            "贴边距",
            "距屏幕边缘的间距（逻辑像素）；重启后生效。",
            "px",
        ),
        num_field(
            st,
            c,
            "/ui/opacity",
            "背景不透明度",
            "0.6 – 1.0；小于 1 需要透明窗口支持。",
            "",
        ),
        text_field(
            st,
            c,
            "/ui/screenshot_hotkey",
            "快捷截图",
            "按这个组合键把当前屏幕发给模型，例如 Ctrl+Alt+A；重启后生效，写法与全局快捷键相同。",
            "Ctrl+Alt+A",
        ),
        switch_field(st, c, "/ui/always_on_top", "窗口置顶", "保持浮窗始终在最前面。"),
        switch_field(
            st,
            c,
            "/ui/show_tray",
            "显示托盘图标",
            "开启后可从托盘唤出窗口（受全局快捷键设置影响）；关闭后只能靠快捷键/托盘外的入口唤出。",
        ),
        switch_field(
            st,
            c,
            "/ui/autostart",
            "开机自启",
            "登录后自动在后台启动 NeoSonia（只起服务，不弹窗）；需要保存并由系统下次登录时生效。",
        ),
        // 静态说明即可：当前没有 elevated 状态字段，不引入新依赖/命令。
        text("Windows 限制：以管理员身份运行时，系统会阻止从资源管理器拖放文件到本窗口（鼠标显示禁止）。此时请用输入框的「附件」按钮、Ctrl+C 复制文件后在本窗口 Ctrl+V，或改用普通权限启动。")
            .size(TextSize::Xs)
            .color(c.text_subtle)
            .leading(1.5),
        switch_field(
            st,
            c,
            "/ui/computer_activity_indicator",
            "计算机操作时屏幕提示",
            "调用截图、点击、键盘类工具时，屏幕边缘会亮起提示，便于知道代理正在操作电脑。",
        ),
        // 用户反馈“找不到连接设备入口”，这里做成显眼的一行。
        row()
            .w_full()
            .items_center()
            .justify_between()
            .gap(space::SM)
            .p(space::SM)
            .bg(c.bg)
            .border(1.0, c.border_subtle)
            .rounded(R_CHIP)
            .children((
                col().grow().min_w(0.0).gap(2.0).children((
                    text("连接其他设备（手机/平板）")
                        .size(TextSize::Sm)
                        .color(c.text),
                    text("同一局域网、扫码配对；入口在「设备」页。")
                        .size(TextSize::Xs)
                        .color(c.text_subtle)
                        .truncate(),
                )),
                button("打开设备页")
                    .variant(ButtonVariant::Secondary)
                    .size(ControlSize::Sm)
                    .on_click(Msg::OpenPage(NavPage::Remote))
                    .id("settings-open-remote-devices"),
            ))
            .into(),
    ];
    section(c, "ui", "界面", "外观与浮窗行为。", rows)
}

/// 工作模式的一个单选卡片。
fn mode_option(
    c: &UiColors,
    id: &'static str,
    label: &str,
    desc: &str,
    active: bool,
) -> Element<Msg> {
    let radio: Element<Msg> = if active {
        col()
            .w(14.0)
            .h(14.0)
            .rounded_full()
            .border(2.0, c.accent)
            .items_center()
            .justify_center()
            .shrink0()
            .children((dot(c.accent),))
    } else {
        col()
            .w(14.0)
            .h(14.0)
            .rounded_full()
            .border(2.0, c.border_strong)
            .items_center()
            .justify_center()
            .shrink0()
            .children((col().w(6.0).h(6.0),))
    };
    row()
        .w_full()
        .items_start()
        .gap(space::SM)
        .p(space::SM)
        .bg(if active {
            c.accent.with_alpha(0.10)
        } else {
            c.bg
        })
        .border(1.0, if active { c.accent } else { c.border_subtle })
        .rounded(R_CHIP)
        .on_click(Msg::SetAgentMode(id.to_string()))
        .cursor(Cursor::Pointer)
        .id(&format!("settings-mode-{id}"))
        .children((
            radio,
            col().grow().min_w(0.0).gap(2.0).children((
                text(label)
                    .size(TextSize::Sm)
                    .weight(if active {
                        Weight::Semibold
                    } else {
                        Weight::Medium
                    })
                    .color(c.text),
                text(desc)
                    .size(TextSize::Xs)
                    .color(c.text_subtle)
                    .leading(1.4),
            )),
            if active {
                tag(c, "使用中")
            } else {
                col().w(0.0)
            },
        ))
}

fn agent_mode_section(st: &AppState, c: &UiColors) -> Element<Msg> {
    let current = st.agent_mode.trim().to_string();
    let label = mode_label(&current);
    let mut rows: Vec<Element<Msg>> = vec![
        row().w_full().items_center().gap(space::SM).wrap().children((
            dot(c.accent),
            text(format!("当前运行模式：{label}"))
                .size(TextSize::Sm)
                .weight(Weight::Medium)
                .color(c.text),
            tag(c, label),
        )).into(),
        text("工作模式决定系统提示、工具优先级与风险策略；切换后立即生效并写入 /agent/mode。")
            .size(TextSize::Xs)
            .color(c.text_subtle)
            .leading(1.5)
            .into(),
    ];
    rows.push(mode_option(
        c,
        "daily",
        "日常",
        "先对话后执行；人设与长期记忆完整注入，工具提示精简。",
        current == "daily",
    ));
    rows.push(mode_option(
        c,
        "coding",
        "编程",
        "面向代码库：先读再改、命令优先 pwsh、强调 diff 与验证；人设注入。",
        current == "coding",
    ));
    rows.push(mode_option(
        c,
        "computer_use",
        "计算机使用",
        "截图/鼠标键盘/UIA 提示词展开，优先看屏幕再操作；每步风险更谨慎。",
        current == "computer_use",
    ));
    section(
        c,
        "agent-mode",
        "工作模式",
        "选一个当前场景；模式会改变代理的默认行为。",
        rows,
    )
}

/// 命令审查的 1-4 分规则（逐行小字展示）。
const REVIEW_RULES: &[&str] = &[
    "1 分：只读、无副作用，直接执行。",
    "2 分：可逆副作用（写文件、装依赖），授权一次后同类自动执行（有效期见下）。",
    "3 分：不可逆或敏感（删除、发邮件、执行命令），每次都要你确认。",
    "4 分：高危（关机、改系统、大范围删除），拒绝；同类在有效期内自动拒绝。",
];

fn review_section(st: &AppState, c: &UiColors) -> Element<Msg> {
    let mut rows: Vec<Element<Msg>> = vec![
        switch_field(
            st,
            c,
            "/review/enabled",
            "启用命令审查",
            "默认开启；关闭 = 完全权限（所有工具调用直接执行，风险自负）。",
        ),
        switch_field(
            st,
            c,
            "/review/review_all_tools",
            "连只读工具也审查",
            "默认只审查有副作用的工具；开启后所有调用都先打分。",
        ),
        subhead(c, "1-4 分规则"),
    ];
    for rule in REVIEW_RULES {
        rows.push(
            text(*rule)
                .size(TextSize::Xs)
                .color(c.text_subtle)
                .leading(1.5)
                .into(),
        );
    }
    let allow = ttl_label(st.settings.review.allow_ttl_secs);
    let deny = ttl_label(st.settings.review.deny_ttl_secs);
    rows.push(
        row()
            .w_full()
            .items_center()
            .gap(space::SM)
            .wrap()
            .children((
                tag(c, format!("2 分授权后同类自动执行：{allow}")),
                tag(c, format!("4 分拒绝后同类自动拒绝：{deny}")),
            ))
            .into(),
    );
    rows.push(subhead(c, "最近审查记录"));
    if st.review_log.is_empty() {
        rows.push(
            text("暂无审查记录")
                .size(TextSize::Xs)
                .color(c.text_subtle)
                .into(),
        );
    } else {
        let total = st.review_log.len();
        for line in st.review_log.iter().rev().take(8) {
            rows.push(
                row()
                    .w_full()
                    .items_start()
                    .gap(space::SM)
                    .children((
                        text("·").size(TextSize::Xs).color(c.text_subtle).shrink0(),
                        text(neosonia_core::util::truncate_chars(line, 100))
                            .size(TextSize::Xs)
                            .mono()
                            .color(c.text_muted)
                            .grow()
                            .truncate(),
                    ))
                    .into(),
            );
        }
        if total > 8 {
            rows.push(
                text(format!("还有 {} 条…", total - 8))
                    .size(TextSize::Xs)
                    .color(c.text_subtle)
                    .into(),
            );
        }
    }
    section(
        c,
        "review",
        "命令审查",
        "有副作用的工具调用前由评分模型打 1-4 分，决定自动执行 / 确认 / 拒绝。",
        rows,
    )
}

fn tools_section(st: &AppState, c: &UiColors) -> Element<Msg> {
    let rows: Vec<Element<Msg>> = vec![
        switch_field(
            st,
            c,
            "/tools/allow_shell",
            "允许执行命令",
            "关掉后代理只能读写文件，不能跑命令。",
        ),
        num_field(
            st,
            c,
            "/tools/command_timeout_secs",
            "命令超时",
            "0 = 不限制。长任务会一直等，只受你的停止操作约束。",
            "秒",
        ),
        switch_field(
            st,
            c,
            "/tools/allow_input_simulation",
            "允许模拟鼠标键盘",
            "截图、点击、输入文本都需要它。",
        ),
        switch_field(
            st,
            c,
            "/tools/uia_enabled",
            "允许读取其它程序控件树（UIA）",
            "Windows 上用于定位按钮、输入框等控件。",
        ),
        switch_field(
            st,
            c,
            "/tools/cdp_enabled",
            "允许通过 CDP 操作 Chromium 应用",
            "需要目标应用以 --remote-debugging-port 启动。",
        ),
        num_field(
            st,
            c,
            "/tools/cdp_port",
            "CDP 调试端口",
            "默认 9222，与浏览器启动参数保持一致。",
            "",
        ),
    ];
    section(c, "tools", "工具权限", "决定代理能对这台机器做什么。", rows)
}

fn web_section(st: &AppState, c: &UiColors) -> Element<Msg> {
    let rows: Vec<Element<Msg>> = vec![
        select_field(
            st,
            c,
            "/web/search_provider",
            "联网搜索服务商",
            "DeepSeek 官方搜索用服务商的联网能力；失败会自动回退到聚合搜索。",
            &[
                ("deepseek", "DeepSeek 官方搜索"),
                ("aggregated", "聚合搜索（百度/搜狗/必应/360）"),
                ("baidu", "百度"),
                ("sogou", "搜狗"),
                ("bing", "必应（中国）"),
                ("duckduckgo", "DuckDuckGo"),
                ("searxng", "自建 SearXNG"),
                ("none", "不用搜索引擎（仅抓 URL）"),
            ],
        ),
        text("聚合搜索会并行查询多个国内引擎并按命中/排名去重合并；DeepSeek 官方搜索不可用时自动回退。")
            .size(TextSize::Xs)
            .color(c.text_subtle)
            .leading(1.5)
            .into(),
    ];
    section(
        c,
        "web",
        "联网搜索",
        "联网搜索的默认后端；可切换到 DeepSeek 官方搜索或单一引擎。",
        rows,
    )
}

/// MCP 小节底部的 TOML 示例（逐行渲染，等宽）。
const MCP_TOML_SAMPLE: &[&str] = &[
    "[[mcp.servers]]",
    "name = \"fs\"",
    "transport = \"stdio\"",
    "command = \"npx\"",
    "args = [\"-y\", \"@modelcontextprotocol/server-filesystem\", \"E:\\\\\"]",
];

/// 单台 MCP 服务器卡片：配置来自已保存的 settings，运行态来自 `st.mcp.servers`。
fn mcp_server_card(
    st: &AppState,
    c: &UiColors,
    cfg: &McpServerConfig,
    index: usize,
) -> Element<Msg> {
    let runtime = st.mcp.servers.iter().find(|r| r.name == cfg.name);
    let ok = runtime.map(|r| r.ok).unwrap_or(false);
    let error = runtime.map(|r| r.error.clone()).unwrap_or_default();
    let tools_text = match runtime.map(|r| r.tools) {
        Some(n) => format!("{n} 个工具"),
        None => "未运行".to_string(),
    };
    let status_color = if !cfg.enabled {
        c.text_subtle
    } else if runtime.is_none() {
        c.text_subtle.with_alpha(0.6)
    } else if ok {
        c.success
    } else {
        c.danger
    };
    let status_text = if !cfg.enabled {
        "已停用".to_string()
    } else if runtime.is_none() {
        "未运行".to_string()
    } else if ok {
        tools_text
    } else {
        "运行失败".to_string()
    };

    let mut body: Vec<Element<Msg>> = vec![
        row()
            .w_full()
            .items_center()
            .gap(space::XS)
            .children((
                col().grow().min_w(0.0).children((
                    text(cfg.name.clone())
                        .size(TextSize::Sm)
                        .weight(Weight::Semibold)
                        .color(c.text)
                        .truncate(),
                )),
                tag(c, mcp_transport_label(cfg.transport)),
                tag(c, status_text),
            ))
            .into(),
        text(mcp_summary(cfg))
            .size(TextSize::Xs)
            .mono()
            .color(c.text_subtle)
            .truncate()
            .into(),
    ];
    if !error.trim().is_empty() {
        body.push(
            text(format!(
                "错误：{}",
                neosonia_core::util::truncate_chars(error.trim(), 90)
            ))
            .size(TextSize::Xs)
            .color(c.danger)
            .truncate()
            .into(),
        );
    }

    row()
        .w_full()
        .items_start()
        .gap(space::SM)
        .p(space::SM)
        .bg(c.bg)
        .border(1.0, c.border_subtle)
        .rounded(R_CHIP)
        .id(&format!("settings-mcp-server-{index}"))
        .children((
            dot(status_color),
            col().grow().min_w(0.0).gap(2.0).children(body),
            col()
                .items_end()
                .gap(space::XS)
                .shrink0()
                .children((
                    // 单台启用状态只读展示：JSON Pointer 不支持数组下标，编辑走 TOML。
                    switch(cfg.enabled)
                        .disabled(true)
                        .id(&format!("settings-mcp-enabled-{index}")),
                    button("重启")
                        .variant(ButtonVariant::Secondary)
                        .size(ControlSize::Xs)
                        .on_click(Msg::McpRestart(cfg.name.clone()))
                        .id(&format!("settings-mcp-restart-{index}")),
                )),
        ))
}

fn mcp_section(st: &AppState, c: &UiColors) -> Element<Msg> {
    let mut rows: Vec<Element<Msg>> = Vec::new();
    rows.push(switch_field(
        st,
        c,
        "/mcp/enabled",
        "启用 MCP 服务器",
        "保存后重启生效（bridge 会重启 MCP）。",
    ));
    rows.push(
        text("Model Context Protocol：把外部工具服务器接入代理；配置在 config/settings.toml 的 [mcp] 段。")
            .size(TextSize::Xs)
            .color(c.text_subtle)
            .leading(1.5)
            .into(),
    );

    if st.settings.mcp.servers.is_empty() {
        rows.push(
            text("还没有配置 MCP 服务器。在 TOML 里加上 [[mcp.servers]] 并保存后，回到这里点“重启”。")
                .size(TextSize::Xs)
                .color(c.text_subtle)
                .leading(1.5)
                .into(),
        );
    } else {
        // MCP 服务器通常 1-3 台；仍限制 8 台，防止异常配置撑大首帧。
        for (i, cfg) in st.settings.mcp.servers.iter().take(8).enumerate() {
            rows.push(mcp_server_card(st, c, cfg, i));
        }
        if st.settings.mcp.servers.len() > 8 {
            rows.push(
                text(format!(
                    "还有 {} 台服务器未显示（列表只显示前 8 台）",
                    st.settings.mcp.servers.len() - 8
                ))
                .size(TextSize::Xs)
                .color(c.text_subtle)
                .into(),
            );
        }
        rows.push(
            text("单台服务器的启用/停用暂时在 TOML 里改（数组下标不支持 JSON Pointer 编辑）；保存后点“重启”应用。")
                .size(TextSize::Xs)
                .color(c.text_subtle)
                .leading(1.5)
                .into(),
        );
    }

    rows.push(divider());
    rows.push(subhead(c, "TOML 示例"));
    let code_lines: Vec<Element<Msg>> = MCP_TOML_SAMPLE
        .iter()
        .map(|line| {
            text(*line)
                .size(TextSize::Xs)
                .mono()
                .color(if *line == "[[mcp.servers]]" {
                    c.text_subtle
                } else {
                    c.text_muted
                })
                .into()
        })
        .collect();
    rows.push(
        col()
            .w_full()
            .p(space::SM + 2.0)
            .gap(2.0)
            .bg(c.bg)
            .border(1.0, c.border_subtle)
            .rounded(R_CHIP)
            .children(code_lines),
    );
    rows.push(
        text("命令可以是 npx / uvx / node / python 或绝对路径；http 传输则填 url = \"http://127.0.0.1:3000/mcp\"。")
            .size(TextSize::Xs)
            .color(c.text_subtle)
            .leading(1.5)
            .into(),
    );

    section(
        c,
        "mcp",
        "MCP 服务器",
        "Model Context Protocol：把外部工具服务器接入代理。",
        rows,
    )
}

fn subagent_section(st: &AppState, c: &UiColors) -> Element<Msg> {
    let rows: Vec<Element<Msg>> = vec![
        switch_field(
            st,
            c,
            "/agent/subagents_enabled",
            "启用子代理",
            "复杂任务可以拆给多个子代理并行处理。",
        ),
        num_field(
            st,
            c,
            "/agent/max_subagents",
            "并发子代理上限",
            "同时运行的子代理数量，建议 2 – 4。",
            "个",
        ),
        num_field(
            st,
            c,
            "/agent/max_iterations",
            "单轮最多工具调用",
            "达到上限后代理会先汇报进展再停下。",
            "轮",
        ),
        num_field(
            st,
            c,
            "/agent/max_tool_output_chars",
            "单条工具输出上限",
            "超出会保留头尾并截断，避免撑爆上下文。",
            "字符",
        ),
    ];
    section(c, "subagent", "子代理", "并行拆解任务的上限与预算。", rows)
}

/// 白名单备注在 draft 里的 JSON Pointer（地址作为 map key）。
fn whitelist_note_path(addr: &str) -> String {
    format!("/mail/whitelist_notes/{}", addr.trim())
}

/// 读取 draft 里的字符串 map（`/mail/whitelist_notes` 这类 BTreeMap）。
fn draft_str_pairs(st: &AppState, path: &str) -> Vec<(String, String)> {
    st.draft
        .pointer(path)
        .and_then(|v| v.as_object())
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

fn whitelist_rows(st: &AppState, c: &UiColors) -> Vec<Element<Msg>> {
    let list = st.draft_str_list("/mail/whitelist");
    let notes = draft_str_pairs(st, "/mail/whitelist_notes");
    let mut rows: Vec<Element<Msg>> = Vec::new();

    rows.push(subhead(
        c,
        "发信白名单（只有匹配的来信才会自动处理与回复）",
    ));
    rows.push(
        text("支持 user@host 精确匹配和 @domain 后缀匹配；给白名单外的人主动发信默认被拒绝，可在下方「允许给白名单外的人发信」打开。")
            .size(TextSize::Xs)
            .color(c.text_subtle)
            .leading(1.5)
            .into(),
    );

    if list.is_empty() {
        rows.push(
            text("还没有白名单地址。在下面输入一个地址并点「添加白名单」。")
                .size(TextSize::Xs)
                .color(c.text_subtle)
                .into(),
        );
    } else {
        for addr in list.iter().take(12) {
            let addr = addr.clone();
            let remove = addr.clone();
            let note = notes
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(&addr))
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            let note_path = whitelist_note_path(&addr);
            let note_input: Element<Msg> = text_input(note)
                .placeholder("备注（可选，例如 老板 / 客户 / 家人）")
                .width(280.0)
                .on_input(move |s| Msg::set(&note_path, s))
                .id(&format!("settings-whitelist-note-{addr}"))
                .into();
            rows.push(
                col()
                    .w_full()
                    .p(space::SM)
                    .gap(space::XS + 2.0)
                    .bg(c.bg)
                    .border(1.0, c.border_subtle)
                    .rounded(R_CHIP)
                    .children((
                        row().w_full().items_center().gap(space::SM).children((
                            text(&addr)
                                .size(TextSize::Xs)
                                .mono()
                                .color(c.text)
                                .grow()
                                .truncate(),
                            icon_button(icons::x())
                                .size(ControlSize::Xs)
                                .label(format!("移除白名单 {addr}"))
                                .on_click(Msg::WhitelistRemove(remove))
                                .id(&format!("settings-whitelist-remove-{addr}")),
                        )),
                        note_input.w_full().min_w(0.0),
                    ))
                    .into(),
            );
        }
        if list.len() > 12 {
            rows.push(
                text(format!("还有 {} 个地址未显示（列表只显示前 12 个）", list.len() - 12))
                    .size(TextSize::Xs)
                    .color(c.text_subtle)
                    .into(),
            );
        }
    }

    // 输入 + 添加
    let parsed = parse_whitelist_input(&st.whitelist_input);
    let single = parsed.len() == 1 && valid_whitelist_entry(&parsed[0]);
    let too_many = parsed.len() > 1;
    let add_addr = parsed.first().cloned().unwrap_or_default();
    let add_msg = Msg::WhitelistAdd(add_addr.clone());

    let add_input: Element<Msg> = text_input(st.whitelist_input.clone())
        .placeholder("user@host 或 @domain")
        .width(220.0)
        .on_input(Msg::WhitelistInputChanged)
        .id("settings-whitelist-input")
        .into();
    let mut add_input = add_input.invalid(too_many);
    let enter_addr = add_addr.clone();
    let enter_ok = single;
    add_input = add_input.on_key(move |k| {
        if k.key == Key::Enter && enter_ok {
            Some(Msg::WhitelistAdd(enter_addr.clone()))
        } else {
            None
        }
    });

    rows.push(
        row()
            .w_full()
            .items_center()
            .gap(space::SM)
            .children((
                col().grow().min_w(0.0).children((add_input.w_full().min_w(0.0),)),
                button("添加白名单")
                    .variant(ButtonVariant::Secondary)
                    .size(ControlSize::Sm)
                    .disabled(!single)
                    .on_click(add_msg)
                    .id("settings-whitelist-add"),
            ))
            .into(),
    );

    if too_many {
        rows.push(
            text("一次添加一个地址：输入里识别到多个地址。")
                .size(TextSize::Xs)
                .color(c.danger)
                .into(),
        );
    } else if !st.whitelist_input.trim().is_empty() && !single {
        rows.push(
            text("地址格式不对：应为 user@host 或 @domain。")
                .size(TextSize::Xs)
                .color(c.danger)
                .into(),
        );
    } else {
        rows.push(
            text("添加后可在列表里给每个地址写备注（备注修改点底部「保存」后生效）；删除条目会一并移除备注。")
                .size(TextSize::Xs)
                .color(c.text_subtle)
                .into(),
        );
    }

    rows
}

/// OAuth2 登录入口（邮件段折叠时也常显：用户反馈“找不到入口”）。
///
/// `st.mail_oauth_status` 里可能包含 Microsoft 的 8 位授权码 / 设备码说明，
/// 这里按行等宽展示并给复制按钮（`Msg::CopyText`）。
/// 诊断信息的一行：标签 + 值；`good=Some(true/false)` 时前面加成功/失败色点。
fn diag_line(
    c: &UiColors,
    label: &str,
    value: impl Into<String>,
    good: Option<bool>,
) -> Element<Msg> {
    let value = value.into();
    let mut kids: Vec<Element<Msg>> = Vec::new();
    if let Some(ok) = good {
        kids.push(
            row()
                .pt(5.0)
                .children((dot(if ok { c.success } else { c.danger }),))
                .into(),
        );
    }
    kids.push(
        text(label)
            .size(TextSize::Xs)
            .color(c.text_subtle)
            .shrink0(),
    );
    kids.push(
        text(value)
            .size(TextSize::Xs)
            .mono()
            .color(if good == Some(false) { c.danger } else { c.text_muted })
            .grow()
            .truncate(),
    );
    row().w_full().items_start().gap(space::XS).children(kids)
}

fn mail_oauth_rows(st: &AppState, c: &UiColors) -> Vec<Element<Msg>> {
    let raw = st.mail_oauth_status.trim().to_string();
    let lines: Vec<Element<Msg>> = if raw.is_empty() {
        vec![text("尚未登录")
            .size(TextSize::Xs)
            .color(c.text_subtle)
            .into()]
    } else {
        raw.lines()
            .take(8)
            .map(|line| {
                text(neosonia_core::util::truncate_chars(line.trim(), 120))
                    .size(TextSize::Xs)
                    .mono()
                    .color(c.text_muted)
                    .truncate()
                    .into()
            })
            .collect()
    };
    let copy_status = Msg::CopyText(raw.clone());
    let copy_disabled = raw.is_empty();
    let audience_unknown = st.mail_oauth_audience.is_empty();
    let scopes_unknown = st.mail_oauth_scopes.is_empty();
    // 有授权/失效授权/状态文字时才显示「退出登录」；保留 imap/smtp 地址与账号。
    let can_logout = st.mail_oauth_needs_reauth
        || !st.settings.mail.oauth.refresh_token.trim().is_empty()
        || !raw.is_empty();
    // 重启后常见状态：refresh_token 还在、access_token 为空（待刷新），不应空白或报错。
    let (login_label, login_ok) = oauth_login_state(
        st.mail_oauth_needs_reauth,
        &st.settings.mail.oauth.refresh_token,
        &st.settings.mail.oauth.access_token,
        st.settings.mail.oauth.enabled,
    );

    let mut rows: Vec<Element<Msg>> = vec![
        switch_field(
            st,
            c,
            "/mail/oauth/enabled",
            "使用 OAuth2 登录",
            "替代密码登录（Outlook / Microsoft 365 / Gmail）；开启后点下面按钮走浏览器授权。",
        ),
        // 登录态：已登录 / 已登录（token 待刷新）/ 需要重新授权 / 未登录。
        row()
            .w_full()
            .items_center()
            .gap(space::XS)
            .children((
                dot(if login_ok { c.success } else { c.danger }),
                text(format!("登录状态：{login_label}"))
                    .size(TextSize::Xs)
                    .weight(Weight::Medium)
                    .color(if login_ok { c.text } else { c.danger })
                    .truncate(),
            ))
            .into(),
        row()
            .w_full()
            .items_center()
            .gap(space::SM)
            .wrap()
            .children((
                button("用 Microsoft 登录")
                    .variant(ButtonVariant::Secondary)
                    .size(ControlSize::Sm)
                    .on_click(Msg::MailOAuthStart("microsoft".to_string()))
                    .id("settings-mail-oauth-microsoft"),
                button("用 Google 登录")
                    .variant(ButtonVariant::Secondary)
                    .size(ControlSize::Sm)
                    .on_click(Msg::MailOAuthStart("google".to_string()))
                    .id("settings-mail-oauth-google"),
            ))
            .into(),
        // 退出登录：清除 oauth 凭证（保留服务器地址/账号/client_id）。
        if can_logout {
            row()
                .w_full()
                .items_center()
                .gap(space::SM)
                .wrap()
                .children((
                    button("退出登录（清除授权）")
                        .variant(ButtonVariant::Danger)
                        .size(ControlSize::Sm)
                        .on_click(Msg::MailLogout)
                        .id("settings-mail-oauth-logout"),
                    text("清除后会退出登录，但保留服务器地址与账号，可继续用密码/授权码登录。")
                        .size(TextSize::Xs)
                        .color(c.text_subtle)
                        .truncate(),
                ))
                .into()
        } else {
            col().h(0.0)
        },
        // 状态/授权码：多行等宽 + 复制（OAuth 需要用户把代码填到浏览器里）。
        row()
            .w_full()
            .items_center()
            .justify_between()
            .gap(space::SM)
            .children((
                text("OAuth 状态 / 授权码")
                    .size(TextSize::Xs)
                    .weight(Weight::Medium)
                    .color(c.text_muted),
                button("复制代码")
                    .variant(ButtonVariant::Secondary)
                    .size(ControlSize::Xs)
                    .disabled(copy_disabled)
                    .on_click(copy_status)
                    .id("settings-mail-oauth-copy"),
            ))
            .into(),
        col()
            .w_full()
            .p(space::SM)
            .gap(2.0)
            .bg(c.bg)
            .border(1.0, c.border_subtle)
            .rounded(R_CHIP)
            .children(lines),
        text("会打开浏览器完成授权，无需在本机保存邮箱密码；如果显示 8 位代码，请在浏览器里输入它。")
            .size(TextSize::Xs)
            .color(c.text_subtle)
            .into(),
    ];

    // 授权后仍不可用：needs_reauth 红色提示 + 一键重新授权。
    if st.mail_oauth_needs_reauth {
        let message = if st.mail_oauth_status_message.trim().is_empty() {
            "授权已失效或权限不足（缺少 IMAP/SMTP 权限），请重新登录。".to_string()
        } else {
            neosonia_core::util::truncate_chars(st.mail_oauth_status_message.trim(), 220)
        };
        rows.push(
            col()
                .w_full()
                .p(space::SM)
                .gap(space::XS + 2.0)
                .bg(c.danger.with_alpha(0.10))
                .border(1.0, c.danger.with_alpha(0.35))
                .rounded(R_CHIP)
                .children((
                    row().items_center().gap(space::XS).children((
                        dot(c.danger),
                        text("需要重新授权")
                            .size(TextSize::Xs)
                            .weight(Weight::Semibold)
                            .color(c.danger),
                    )),
                    text(message)
                        .size(TextSize::Xs)
                        .color(c.danger)
                        .leading(1.4),
                    button("重新授权")
                        .variant(ButtonVariant::Danger)
                        .size(ControlSize::Xs)
                        .on_click(Msg::MailOAuthStart("microsoft".to_string()))
                        .id("settings-mail-oauth-reauth"),
                ))
                .into(),
        );
    }

    // 诊断信息折叠：受众 / 权限 / IMAP / SMTP 权限。
    let diag_summary = format!(
        "受众 {} 项 · 权限 {} 项",
        st.mail_oauth_audience.len(),
        st.mail_oauth_scopes.len()
    );
    rows.push(fold_section(
        st,
        c,
        "mail-oauth-diagnostics",
        "诊断信息",
        &diag_summary,
        || {
            vec![
                diag_line(
                    c,
                    "受众",
                    joined_or_unknown(&st.mail_oauth_audience),
                    None,
                ),
                diag_line(
                    c,
                    "权限",
                    joined_or_unknown(&st.mail_oauth_scopes),
                    None,
                ),
                diag_line(
                    c,
                    "IMAP 权限",
                    oauth_perm_label(st.mail_oauth_has_imap_scope, scopes_unknown),
                    if scopes_unknown {
                        None
                    } else {
                        Some(st.mail_oauth_has_imap_scope)
                    },
                ),
                diag_line(
                    c,
                    "SMTP 权限",
                    oauth_perm_label(st.mail_oauth_has_smtp_scope, scopes_unknown),
                    if scopes_unknown {
                        None
                    } else {
                        Some(st.mail_oauth_has_smtp_scope)
                    },
                ),
                text(if audience_unknown && scopes_unknown {
                    "当前还没有可解析的 access token（未登录，或 Google token 不是 JWT），诊断项显示“未知”。"
                } else {
                    "受众/权限来自 access_token 的 JWT claims；缺少 IMAP 权限时收信会报“User is authenticated but not connected”。"
                })
                .size(TextSize::Xs)
                .color(c.text_subtle)
                .leading(1.4)
                .into(),
            ]
        },
    ));

    rows
}

fn mail_section(st: &AppState, c: &UiColors) -> Vec<Element<Msg>> {
    let mut rows: Vec<Element<Msg>> = Vec::new();
    rows.push(switch_field(
        st,
        c,
        "/mail/enabled",
        "启用邮件",
        "开启后后台轮询收件箱，白名单来信会触发代理。",
    ));
    rows.push(divider());
    rows.push(subhead(c, "IMAP 收信"));
    rows.push(text_field(
        st,
        c,
        "/mail/imap/host",
        "IMAP 主机",
        "例如 imap.gmail.com、imap.qq.com。",
        "imap.example.com",
    ));
    rows.push(num_field(
        st,
        c,
        "/mail/imap/port",
        "IMAP 端口",
        "直连 TLS 一般是 993，明文是 143。",
        "",
    ));
    rows.push(switch_field(
        st,
        c,
        "/mail/imap/tls",
        "IMAP 直连 TLS",
        "993 端口保持开启；只有 143 + STARTTLS 的服务器才关掉。",
    ));
    rows.push(text_field(
        st,
        c,
        "/mail/imap/user",
        "IMAP 用户",
        "通常是完整邮箱地址。",
        "you@example.com",
    ));
    rows.push(text_field(
        st,
        c,
        "/mail/imap/password",
        "IMAP 密码",
        "建议用应用专用密码；同样仅存本机配置。",
        "",
    ));
    rows.push(text_field(
        st,
        c,
        "/mail/imap/mailbox",
        "默认收件箱目录",
        "一般是 INBOX。",
        "INBOX",
    ));
    rows.push(test_row(st, c, TestTarget::Imap));

    rows.push(divider());
    rows.push(subhead(c, "SMTP 发信"));
    rows.push(text_field(
        st,
        c,
        "/mail/smtp/host",
        "SMTP 主机",
        "例如 smtp.gmail.com、smtp.qq.com。",
        "smtp.example.com",
    ));
    rows.push(num_field(
        st,
        c,
        "/mail/smtp/port",
        "SMTP 端口",
        "587 = STARTTLS，465 = 隐式 TLS。",
        "",
    ));
    rows.push(switch_field(
        st,
        c,
        "/mail/smtp/starttls",
        "SMTP STARTTLS",
        "587 端口一般开启。",
    ));
    rows.push(switch_field(
        st,
        c,
        "/mail/smtp/implicit_tls",
        "SMTP 隐式 TLS",
        "465 端口开启；与 STARTTLS 二选一。",
    ));
    rows.push(text_field(
        st,
        c,
        "/mail/smtp/user",
        "SMTP 用户",
        "一般与 IMAP 用户相同。",
        "you@example.com",
    ));
    rows.push(text_field(
        st,
        c,
        "/mail/smtp/password",
        "SMTP 密码",
        "留空则回退到环境变量 NEOSONIA_SMTP_PASSWORD。",
        "",
    ));
    rows.push(text_field(
        st,
        c,
        "/mail/smtp/from",
        "发件人地址",
        "留空则使用 SMTP 用户。",
        "you@example.com",
    ));
    rows.push(text_field(
        st,
        c,
        "/mail/smtp/from_name",
        "发件人名称",
        "收件人看到的显示名。",
        "NeoSonia",
    ));
    rows.push(test_row(st, c, TestTarget::Smtp));

    rows.push(divider());
    rows.extend(whitelist_rows(st, c));

    rows.push(divider());
    rows.push(subhead(c, "行为"));
    rows.push(num_field(
        st,
        c,
        "/mail/poll_interval_secs",
        "轮询间隔",
        "后台检查新邮件的间隔，太短可能被服务器限流。",
        "秒",
    ));
    rows.push(switch_field(
        st,
        c,
        "/mail/auto_reply",
        "自动回复白名单来信",
        "关闭后只通知，不会替你回信。",
    ));
    rows.push(switch_field(
        st,
        c,
        "/mail/allow_outbound_to_anyone",
        "允许给白名单外的人发信",
        "建议保持关闭，避免代理自作主张发信。",
    ));
    rows.push(text_field(
        st,
        c,
        "/mail/signature",
        "自动回复签名",
        "会追加在自动回复正文末尾。",
        "—— NeoSonia",
    ));

    rows.push(divider());
    rows.push(fold_section(
        st,
        c,
        "mail-advanced",
        "高级 / 排障",
        "TLS 证书校验、不安全连接开关",
        || {
            vec![
                switch_field(
                    st,
                    c,
                    "/mail/oauth/allow_insecure_tls",
                    "允许不安全的 TLS（排障）",
                    "仅在 HTTPS 证书被拦截（如 Kaspersky 等安全软件 MITM）时打开；打开后本次重试不校验证书，存在风险。",
                ),
                text("打开后只用于本次重试；保存后开关状态会保留，排障完建议关掉。")
                    .size(TextSize::Xs)
                    .color(c.text_subtle)
                    .leading(1.4)
                    .into(),
            ]
        },
    ));

    rows
}

fn persona_memory_section(st: &AppState, c: &UiColors) -> Vec<Element<Msg>> {
    let mut rows: Vec<Element<Msg>> = Vec::new();

    if st.personas.is_empty() {
        let sel: Element<Msg> = select(0, ["默认人设（未发现 personas/*.md）".to_string()])
            .width(320.0)
            .disabled(true)
            .id("settings-persona")
            .into();
        rows.push(labeled(c, "当前人设", "人设文件放在 personas/ 目录。", sel.w_full()));
    } else {
        let ids: Vec<String> = st.personas.iter().map(|p| p.id.clone()).collect();
        let names: Vec<String> = st.personas.iter().map(|p| p.name.clone()).collect();
        let index = ids
            .iter()
            .position(|id| id == &st.active_persona)
            .unwrap_or(0);
        let pick = ids.clone();
        let sel: Element<Msg> = select(index, names)
            .width(320.0)
            .on_change(move |i| {
                Msg::PersonaSelect(pick.get(i).cloned().unwrap_or_else(|| "default".into()))
            })
            .id("settings-persona")
            .into();
        rows.push(labeled(
            c,
            "当前人设",
            "也可以在“人设”页切换；这里跟随同一份配置。",
            sel.w_full(),
        ));
    }

    rows.push(switch_field(
        st,
        c,
        "/agent/auto_memory",
        "自动写入长期记忆",
        "代理会在合适的时候把值得记的事写进记忆库。",
    ));
    rows.push(num_field(
        st,
        c,
        "/agent/memory_inject",
        "每次注入条数",
        "每次请求前从长期记忆里注入多少条相关事实。",
        "条",
    ));
    rows.push(divider());
    rows.push(switch_field(
        st,
        c,
        "/agent/show_reasoning",
        "显示思考过程",
        "对话里折叠展示模型的 reasoning 内容。",
    ));

    rows
}

fn skills_section(st: &AppState, c: &UiColors) -> Vec<Element<Msg>> {
    let mut rows: Vec<Element<Msg>> = Vec::new();
    let dir = st.paths.skills.display().to_string();
    rows.push(
        row()
            .w_full()
            .items_center()
            .gap(space::SM)
            .children((
                col().grow().min_w(0.0).gap(2.0).children((
                    text("技能目录")
                        .size(TextSize::Xs)
                        .weight(Weight::Medium)
                        .color(c.text_muted),
                    text(dir)
                        .size(TextSize::Xs)
                        .mono()
                        .color(c.text_subtle)
                        .truncate(),
                )),
                button("打开目录")
                    .variant(ButtonVariant::Secondary)
                    .size(ControlSize::Xs)
                    .on_click(Msg::SkillOpenDir)
                    .id("settings-skill-dir"),
            ))
            .into(),
    );
    rows.push(subhead(c, "已安装"));
    if st.skills.is_empty() {
        rows.push(
            text("还没有安装技能。技能是给代理看的操作手册，需要时它会自己用 skill_load 读取。")
                .size(TextSize::Xs)
                .color(c.text_subtle)
                .leading(1.5)
                .into(),
        );
    } else {
        // 长列表只渲染前 8 条，避免首帧/展开帧一次构建过多节点。
        for s in st.skills.iter().take(8) {
            let id = s.id.clone();
            let source = match s.source.as_str() {
                "builtin" => "内置",
                "url" => "远程",
                _ => "本地",
            };
            let desc = if s.description.trim().is_empty() {
                s.when_to_use.clone()
            } else {
                s.description.clone()
            };
            rows.push(
                row()
                    .w_full()
                    .items_center()
                    .gap(space::SM)
                    .p(space::SM)
                    .bg(c.bg)
                    .border(1.0, c.border_subtle)
                    .rounded(R_CHIP)
                    .children((
                        col().grow().min_w(0.0).gap(2.0).children((
                            row().items_center().gap(space::XS).children((
                                col().grow().min_w(0.0).children((
                                    text(s.name.clone())
                                        .size(TextSize::Sm)
                                        .color(c.text)
                                        .truncate(),
                                )),
                                tag(c, source),
                            )),
                            text(neosonia_core::util::truncate_chars(&desc, 80))
                                .size(TextSize::Xs)
                                .color(c.text_subtle)
                                .truncate(),
                            text(s.path.clone())
                                .size(TextSize::Xs)
                                .mono()
                                .color(c.text_subtle)
                                .truncate(),
                        )),
                        icon_button(icons::lucide::trash())
                            .size(ControlSize::Xs)
                            .label(format!("卸载技能 {}", s.name))
                            .on_click(Msg::SkillUninstall(id))
                            .id(&format!("settings-skill-remove-{}", s.id)),
                    ))
                    .into(),
            );
        }
        if st.skills.len() > 8 {
            rows.push(
                text(format!(
                    "还有 {} 个技能未显示（列表只显示前 8 个）",
                    st.skills.len() - 8
                ))
                .size(TextSize::Xs)
                .color(c.text_subtle)
                .into(),
            );
        }
    }
    rows.push(divider());
    rows.push(subhead(c, "安装"));
    rows.push(
        row()
            .w_full()
            .items_center()
            .gap(space::SM)
            .children((
                button("从文件安装…")
                    .variant(ButtonVariant::Secondary)
                    .size(ControlSize::Sm)
                    .on_click(Msg::SkillInstallRequest)
                    .id("settings-skill-install-file"),
                col().grow().min_w(0.0).children((
                    text("选择 SKILL.md 或包含它的目录")
                        .size(TextSize::Xs)
                        .color(c.text_subtle)
                        .truncate(),
                )),
            ))
            .into(),
    );
    let url_input: Element<Msg> = text_input(st.skill_install_input.clone())
        .placeholder("https://…/SKILL.md")
        .width(220.0)
        .on_input(Msg::SkillInstallInputChanged)
        .id("settings-skill-url")
        .into();
    rows.push(
        row()
            .w_full()
            .items_center()
            .gap(space::SM)
            .children((
                col().grow().min_w(0.0).children((url_input.w_full(),)),
                button("从 URL 安装")
                    .variant(ButtonVariant::Secondary)
                    .size(ControlSize::Sm)
                    .on_click(Msg::SkillInstallUrl(st.skill_install_input.clone()))
                    .id("settings-skill-install-url"),
            ))
            .into(),
    );
    rows
}

fn index_section(st: &AppState, c: &UiColors) -> Vec<Element<Msg>> {
    let roots = parse_root_lines(&st.index_roots_input);
    let mut rows: Vec<Element<Msg>> = Vec::new();
    rows.push(switch_field(
        st,
        c,
        "/index/enabled",
        "启用本地文件索引",
        "开启后代理能按文件名极速搜索（类 Everything），只建索引、不读文件内容。",
    ));
    rows.push(switch_field(
        st,
        c,
        "/index/autoscan",
        "启动时自动扫描",
        "关闭后只在点“重建索引”时扫描。",
    ));
    rows.push(subhead(c, "索引根目录"));
    rows.push(
        text("一行一个路径，支持 ~；保存后点“重建索引”生效。")
            .size(TextSize::Xs)
            .color(c.text_subtle)
            .into(),
    );
    let area: Element<Msg> = text_area(st.index_roots_input.clone())
        .placeholder("~/Desktop\n~/Documents\nD:\\projects")
        .width(320.0)
        .min_height(72.0)
        .on_input(Msg::IndexRootsChanged)
        .id("settings-index-roots")
        .into();
    rows.push(area.w_full());
    rows.push(
        row()
            .w_full()
            .items_center()
            .gap(space::SM)
            .wrap()
            .children((
                button("保存根目录")
                    .variant(ButtonVariant::Secondary)
                    .size(ControlSize::Sm)
                    .on_click(Msg::set("/index/roots", roots.clone()))
                    .id("settings-index-roots-save"),
                button("重建索引")
                    .variant(ButtonVariant::Secondary)
                    .size(ControlSize::Sm)
                    .on_click(Msg::IndexRebuild)
                    .id("settings-index-rebuild"),
                text(format!("{} 个根目录", roots.len()))
                    .size(TextSize::Xs)
                    .color(c.text_subtle),
            ))
            .into(),
    );
    rows.push(num_field(
        st,
        c,
        "/index/max_entries",
        "索引条目上限",
        "超过上限后不再加入新条目，避免占用太多磁盘。",
        "条",
    ));
    rows
}

fn remote_section(st: &AppState, c: &UiColors) -> Vec<Element<Msg>> {
    // 开关读草稿（RemoteToggle 的 handler 会把 bool 写进 draft），状态点读运行时。
    let enabled = st.draft_bool("/remote/enabled");
    let running = st.remote.enabled;
    let url = st.remote.url.trim().to_string();
    let url_el: Element<Msg> = if url.is_empty() {
        col().w(0.0)
    } else {
        text(url)
            .size(TextSize::Xs)
            .mono()
            .color(c.accent)
            .truncate()
    };

    let mut rows: Vec<Element<Msg>> = Vec::new();
    rows.push(
        row()
            .w_full()
            .items_center()
            .justify_between()
            .gap(space::MD)
            .children((
                col().grow().min_w(0.0).gap(2.0).children((
                    text("启用局域网访问")
                        .size(TextSize::Sm)
                        .color(c.text),
                    text("手机/平板连同一个 Wi-Fi 后，用浏览器远程和 NeoSonia 聊天；新设备必须扫码配对。")
                        .size(TextSize::Xs)
                        .color(c.text_subtle),
                )),
                switch(enabled)
                    .on_toggle(Msg::RemoteToggle)
                    .id("settings-remote-enabled"),
            ))
            .into(),
    );
    rows.push(
        row()
            .w_full()
            .items_center()
            .gap(space::SM)
            .wrap()
            .children((
                dot(if running { c.success } else { c.text_subtle }),
                text(if running { "服务已启用" } else { "服务未启用" })
                    .size(TextSize::Xs)
                    .color(c.text_muted),
                url_el,
            ))
            .into(),
    );
    rows.push(num_field(
        st,
        c,
        "/remote/port",
        "监听端口",
        "默认 8787；改端口后需要重启 Web 服务（保存后到设备页刷新）。",
        "",
    ));
    rows.push(switch_field(
        st,
        c,
        "/remote/lan_only",
        "只允许局域网连接",
        "关闭会允许公网访问，不推荐。",
    ));
    rows.push(
        row()
            .w_full()
            .items_center()
            .justify_between()
            .gap(space::SM)
            .children((
                col().grow().min_w(0.0).children((
                    text("二维码、配对码与已信任设备在“设备”页管理。")
                        .size(TextSize::Xs)
                        .color(c.text_subtle)
                        .truncate(),
                )),
                button("打开设备页")
                    .variant(ButtonVariant::Secondary)
                    .size(ControlSize::Sm)
                    .on_click(Msg::OpenPage(NavPage::Remote))
                    .id("settings-open-remote"),
            ))
            .into(),
    );
    rows
}

/// 固定在底部的保存条（不在滚动区内，绝不超出面板宽度）。
fn footer(st: &AppState, c: &UiColors) -> Element<Msg> {
    let dirty = st.is_dirty();
    let state_color = if dirty { c.warning } else { c.success };
    let state_text = if dirty { "有未保存的修改" } else { "已与磁盘一致" };
    col()
        .w_full()
        .px(space::LG)
        .py(space::MD)
        .gap(space::XS)
        .bg(c.surface)
        .border_top(1.0, c.border_subtle)
        .id("settings-footer")
        .children((
            row()
                .w_full()
                .items_center()
                .gap(space::SM)
                .children((
                    dot(state_color),
                    text(state_text)
                        .size(TextSize::Xs)
                        .weight(Weight::Medium)
                        .color(c.text)
                        .truncate(),
                    spacer(),
                    button("重新载入")
                        .variant(ButtonVariant::Secondary)
                        .size(ControlSize::Sm)
                        .on_click(Msg::SettingsReload)
                        .id("settings-reload"),
                    button("保存")
                        .variant(if dirty {
                            ButtonVariant::Primary
                        } else {
                            ButtonVariant::Secondary
                        })
                        .size(ControlSize::Sm)
                        .on_click(Msg::SettingsSave)
                        .id("settings-save"),
                )),
            text("重启后生效：宽度 / 屏占比 / 贴边距 / 透明度 / 托盘 / 快捷键 / 开机自启。")
                .size(TextSize::Xs)
                .color(c.text_subtle)
                .truncate(),
            // 设置页最下方：版本信息（不硬编码，跟随 Cargo.toml）。
            text(version_text())
                .size(TextSize::Xs)
                .color(c.text_subtle)
                .truncate(),
        ))
}

/// 设置页/关于段共用的版本行（`CARGO_PKG_VERSION`，不硬编码）。
fn version_text() -> String {
    format!(
        "NeoSonia v{} · 本机原生 GUI（fenestra）",
        env!("CARGO_PKG_VERSION")
    )
}

/// 「关于」折叠标题摘要。
fn about_summary() -> String {
    format!("NeoSonia v{} · fenestra 0.41 · MIT", env!("CARGO_PKG_VERSION"))
}

/// 「关于 / 致谢」段（默认折叠，不依赖网络）。
fn about_section(c: &UiColors) -> Vec<Element<Msg>> {
    vec![
        text(format!(
            "NeoSonia v{}：本机原生桌面 AI 助手。",
            env!("CARGO_PKG_VERSION")
        ))
        .size(TextSize::Xs)
        .color(c.text_muted)
        .leading(1.5)
        .into(),
        text("前端使用 fenestra 0.41（winit + wgpu + vello 原生渲染，无 Web 内核）。")
            .size(TextSize::Xs)
            .color(c.text_subtle)
            .leading(1.5)
            .into(),
        text("NeoSonia 以 MIT 许可发布；第三方依赖各自遵循其许可证，详见项目根目录 README.md 与 LICENSE。")
            .size(TextSize::Xs)
            .color(c.text_subtle)
            .leading(1.5)
            .into(),
        text("构建、配置、数据与隐私说明见项目根目录的 README.md。")
            .size(TextSize::Xs)
            .color(c.text_subtle)
            .leading(1.5)
            .into(),
    ]
}

// ============================================================
// 面板入口
// ============================================================

pub fn view(st: &AppState, c: &UiColors) -> Element<Msg> {
    // 首帧只构建常用段；技能/索引/邮件/远程/人设记忆默认折叠（settings_open 里没有时才构建 body）。
    let mut children: Vec<Element<Msg>> = vec![page_header(c)];
    if !st.remote.enabled {
        children.push(device_entry_hint(c));
    }
    children.push(model_section(st, c));
    children.push(ui_section(st, c));
    children.push(agent_mode_section(st, c));
    children.push(review_section(st, c));
    children.push(tools_section(st, c));
    children.push(web_section(st, c));
    children.push(mcp_section(st, c));
    children.push(subagent_section(st, c));
    let skills_sum = skills_summary(st);
    children.push(collapsible_section(
        st,
        c,
        "skills",
        "技能",
        &skills_sum,
        Vec::new(),
        || skills_section(st, c),
    ));
    let index_sum = index_summary(st);
    children.push(collapsible_section(
        st,
        c,
        "index",
        "本地索引",
        &index_sum,
        Vec::new(),
        || index_section(st, c),
    ));
    let mail_sum = mail_summary(st);
    children.push(collapsible_section(
        st,
        c,
        "mail",
        "邮件",
        &mail_sum,
        // OAuth 是用户主路径：折叠时也常显开关/两个登录按钮/状态。
        mail_oauth_rows(st, c),
        || mail_section(st, c),
    ));
    let remote_sum = remote_summary(st);
    children.push(collapsible_section(
        st,
        c,
        "remote",
        "远程访问",
        &remote_sum,
        Vec::new(),
        || remote_section(st, c),
    ));
    let persona_memory_sum = persona_memory_summary(st);
    children.push(collapsible_section(
        st,
        c,
        "persona_memory",
        "人设与记忆",
        &persona_memory_sum,
        Vec::new(),
        || persona_memory_section(st, c),
    ));
    // 关于/致谢：默认折叠，放在页面最末，不引入外链。
    let about_sum = about_summary();
    children.push(collapsible_section(
        st,
        c,
        "about",
        "关于",
        &about_sum,
        Vec::new(),
        || about_section(c),
    ));

    // 两段式：上面是可滚动的表单区（grow 占满剩余高度），下面是固定 footer。
    let form = col()
        .w_full()
        .grow()
        // flex + overflow 的关键：不给 min-height 0 时，滚动区会被内容撑开，footer 被顶出屏幕
        .min_h(0.0)
        .scroll_y()
        .p(space::LG)
        .gap(space::LG)
        .id(FORM_ID)
        .children(children);

    col()
        .w_full()
        .grow()
        .min_h(0.0)
        .id(PANEL_ID)
        .children((form, footer(st, c)))
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use neosonia_core::config::{Paths, Settings};

    fn state() -> AppState {
        AppState::new(Paths::from_root("C:/tmp/neosonia-panel-test"), Settings::default())
    }

    #[test]
    fn form_rows_read_and_format_draft_values() {
        let st = state();
        // 表单行的显示值由草稿构造。
        assert_eq!(number_text(&st, "/ui/width"), "424");
        assert_eq!(number_text(&st, "/server/max_tokens"), "32768");
        assert_eq!(number_text(&st, "/server/temperature"), "0.6");
        assert_eq!(number_text(&st, "/tools/command_timeout_secs"), "1800");
        assert_eq!(fmt_num(0.98), "0.98");
        assert_eq!(fmt_num(131_072.0), "131072");
        assert_eq!(fmt_num(0.0), "0");
        // select 的当前下标映射。
        assert_eq!(select_index("light", &["dark", "light", "system"]), 1);
        assert_eq!(select_index("", &["dark", "light"]), 0);
        // 主题默认 dark。
        assert_eq!(st.draft_str("/ui/theme"), "dark");
    }

    #[test]
    fn whitelist_text_parsing_and_validation() {
        assert_eq!(
            parse_whitelist_input(" Boss@Corp.com, @vip.com;  \n\t"),
            vec!["boss@corp.com".to_string(), "@vip.com".to_string()]
        );
        assert_eq!(
            parse_whitelist_input("boss@corp.com, BOSS@corp.com"),
            vec!["boss@corp.com".to_string()]
        );
        assert!(parse_whitelist_input(" , ; \n ").is_empty());
        assert!(valid_whitelist_entry("boss@corp.com"));
        assert!(valid_whitelist_entry("@vip.com"));
        assert!(!valid_whitelist_entry("boss@corp.com @x.com"));
        assert!(!valid_whitelist_entry("@"));
        assert!(!valid_whitelist_entry("no-at-sign"));
        assert!(!valid_whitelist_entry("a@b@c"));
    }

    #[test]
    fn whitelist_notes_are_read_from_draft() {
        let mut st = state();
        st.draft = serde_json::json!({
            "mail": {
                "whitelist": ["boss@corp.com", "@vip.com"],
                "whitelist_notes": {"boss@corp.com": "老板", "@vip.com": "VIP 客户"}
            }
        });
        assert_eq!(
            whitelist_note_path(" boss@corp.com "),
            "/mail/whitelist_notes/boss@corp.com"
        );
        let pairs = draft_str_pairs(&st, "/mail/whitelist_notes");
        assert_eq!(pairs.len(), 2);
        assert!(pairs
            .iter()
            .any(|(k, v)| k == "boss@corp.com" && v == "老板"));
        assert!(pairs.iter().any(|(k, v)| k == "@vip.com" && v == "VIP 客户"));
        // 没有 notes 时返回空列表，不 panic。
        assert!(draft_str_pairs(&st, "/mail/not_exists").is_empty());
    }

    #[test]
    fn index_roots_parse_one_per_line() {
        assert_eq!(
            parse_root_lines("~/Desktop\n  ~/Documents  \n\n# 注释\nD:\\projects\n~/Desktop\n"),
            vec![
                "~/Desktop".to_string(),
                "~/Documents".to_string(),
                "D:\\projects".to_string()
            ]
        );
        assert!(parse_root_lines("   \n\t\n").is_empty());
        // 新增的小节路径在默认配置里都能取到值。
        let st = state();
        assert_eq!(number_text(&st, "/remote/port"), "8787");
        assert_eq!(number_text(&st, "/index/max_entries"), "2000000");
        assert_eq!(st.draft_bool("/ui/autostart"), false);
        assert_eq!(st.draft_str("/ui/screenshot_hotkey"), "Ctrl+Alt+A");
    }

    #[test]
    fn mcp_rows_read_config_and_draft_paths() {
        let st = state();
        // 新增路径都能在默认配置里读到。
        assert_eq!(st.draft_bool("/mcp/enabled"), false);
        assert!(st.draft_bool("/ui/computer_activity_indicator"));

        // 传输方式与摘要。
        let mut cfg = McpServerConfig::default();
        assert_eq!(mcp_transport_label(cfg.transport), "stdio");
        assert_eq!(mcp_summary(&cfg), "(未配置命令)");
        cfg.command = "npx".into();
        cfg.args = vec![
            "-y".into(),
            "@modelcontextprotocol/server-filesystem".into(),
            "E:\\".into(),
        ];
        let summary = mcp_summary(&cfg);
        assert!(summary.starts_with("npx -y"), "{summary}");
        assert!(summary.contains("server-filesystem"), "{summary}");

        let http = McpServerConfig {
            transport: McpTransport::Http,
            url: "http://127.0.0.1:3000/mcp".into(),
            ..Default::default()
        };
        assert_eq!(mcp_transport_label(http.transport), "http");
        assert_eq!(mcp_summary(&http), "http://127.0.0.1:3000/mcp");
        let empty_http = McpServerConfig {
            transport: McpTransport::Http,
            ..Default::default()
        };
        assert_eq!(mcp_summary(&empty_http), "(未配置 URL)");

        // 超长命令摘要截断（不超过 96 字符）。
        let long = McpServerConfig {
            command: "x".repeat(200),
            ..Default::default()
        };
        assert!(mcp_summary(&long).chars().count() <= 96);
        assert_eq!(short_chars("abc", 10), "abc");
        assert!(short_chars("abcdef", 4).chars().count() <= 4);
    }

    #[test]
    fn mode_thinking_and_review_helpers() {
        // 工作模式 label/默认值。
        assert_eq!(mode_label("daily"), "日常");
        assert_eq!(mode_label("coding"), "编程");
        assert_eq!(mode_label("computer_use"), "计算机使用");
        assert_eq!(mode_label(""), "日常");
        let st = state();
        assert_eq!(st.draft_str("/agent/mode"), "daily");
        assert!(st.draft_bool("/review/enabled"));
        assert!(!st.draft_bool("/review/review_all_tools"));
        assert_eq!(st.draft_bool("/mail/oauth/enabled"), false);

        // 思考强度 4 档映射：不思考 / low / high / max。
        assert_eq!(reasoning_index(""), 0);
        assert_eq!(reasoning_index("low"), 1);
        assert_eq!(reasoning_index("high"), 2);
        assert_eq!(reasoning_index("max"), 3);
        // 旧配置的 medium 兼容映射到 high 档显示。
        assert_eq!(reasoning_index("medium"), 2);
        // 未知值按默认 high 显示。
        assert_eq!(reasoning_index("whatever"), 2);
        assert_eq!(reasoning_value(0.0), "");
        assert_eq!(reasoning_value(1.0), "low");
        assert_eq!(reasoning_value(2.0), "high");
        assert_eq!(reasoning_value(3.0), "max");
        assert_eq!(reasoning_value(2.6), "max");
        // 默认配置的思考强度是 high（4 档里的第 3 档）。
        assert_eq!(st.draft_str("/server/reasoning_effort"), "high");
        assert_eq!(reasoning_index(&st.draft_str("/server/reasoning_effort")), 2);

        // 审查 TTL 文案。
        assert_eq!(ttl_label(30), "30 秒");
        assert_eq!(ttl_label(90), "1 分钟");
        assert_eq!(ttl_label(7_200), "2 小时");
        assert_eq!(ttl_label(86_400), "1 天");

        // 折叠面板摘要使用默认数据即可得到稳定文案。
        assert_eq!(skills_summary(&st), "未安装技能");
        assert_eq!(index_summary(&st), "已索引 0 条 · 空闲");
        assert_eq!(mail_summary(&st), "密码登录：未配置");
        assert_eq!(remote_summary(&st), "未启用");
        assert!(persona_memory_summary(&st).contains("记忆 0 条"));
    }

    #[test]
    fn model_context_and_web_defaults() {
        let st = state();
        // token 相关默认值（v0.5 起 max_tokens 默认 32768）。
        assert_eq!(number_text(&st, "/server/max_tokens"), "32768");
        assert_eq!(number_text(&st, "/server/context_window"), "1048576");
        assert!(st.draft_bool("/agent/context_compress"));
        assert_eq!(number_text(&st, "/agent/context_threshold_tokens"), "700000");
        assert_eq!(number_text(&st, "/agent/keep_recent_messages"), "40");
        // 联网搜索默认聚合。
        assert_eq!(st.draft_str("/web/search_provider"), "aggregated");
        // token 中文文案。
        assert_eq!(tokens_label(512), "512 tokens");
        assert_eq!(tokens_label(8_192), "约 8 千 tokens");
        assert_eq!(tokens_label(700_000), "约 70 万 tokens");
        assert_eq!(tokens_label(1_048_576), "约 105 万 tokens");
    }

    #[test]
    fn oauth_diag_labels() {
        assert_eq!(oauth_perm_label(true, false), "有");
        assert_eq!(oauth_perm_label(false, false), "缺失");
        assert_eq!(oauth_perm_label(true, true), "未知");
        assert_eq!(oauth_perm_label(false, true), "未知");
        assert_eq!(joined_or_unknown(&[]), "未知");
        assert_eq!(
            joined_or_unknown(&["imap".to_string(), "smtp".to_string()]),
            "imap，smtp"
        );
        // 超长 scopes 会截断，避免撑破窄栏。
        let long: Vec<String> = (0..40).map(|i| format!("scope-{i}")).collect();
        assert!(joined_or_unknown(&long).chars().count() <= 160);

        // 重启后的 OAuth 登录态：refresh_token 在、access_token 空 → “token 待刷新”。
        assert_eq!(
            oauth_login_state(false, "refresh", "", false),
            ("已登录（token 待刷新）", true)
        );
        assert_eq!(
            oauth_login_state(false, "refresh", "access", true),
            ("已登录", true)
        );
        assert_eq!(
            oauth_login_state(true, "refresh", "", true),
            ("需要重新授权", false)
        );
        assert_eq!(
            oauth_login_state(false, "", "", true),
            ("已启用，尚未登录", false)
        );
        assert_eq!(
            oauth_login_state(false, "", "", false),
            ("未登录", false)
        );
    }

    #[test]
    fn version_and_about_texts_follow_cargo_version() {
        let v = env!("CARGO_PKG_VERSION");
        assert_eq!(
            version_text(),
            format!("NeoSonia v{v} · 本机原生 GUI（fenestra）")
        );
        assert_eq!(
            about_summary(),
            format!("NeoSonia v{v} · fenestra 0.41 · MIT")
        );
        // 版本号来自 Cargo.toml，不能是硬编码旧版本。
        assert!(version_text().contains("NeoSonia v"));
        assert!(!version_text().contains("v0.1.0"));
        assert!(about_summary().contains("MIT"));
        assert!(about_summary().contains("fenestra 0.41"));
    }
}
