//! 人设面板：人设卡片列表 + 可视化编辑 / 新建 + 重新扫描。
//!
//! 人设文件放在 `personas/*.md`，YAML front-matter 声明 name / description，
//! 正文即系统提示词。编辑/新建通过 `Msg::PersonaEdit*` 交给主循环落盘；
//! 本面板只维护表单与校验，不做 IO。

use fenestra::prelude::*;
use neosonia_core::model::Persona;

use crate::state::{AppState, Msg, PersonaEditState};
use crate::theme::{space, UiColors, R_CARD, R_CHIP, R_CONTROL};

const PANEL_ID: &str = "panel-persona";

/// 内置人设 id：不允许删除。
const BUILTIN_PERSONA_IDS: &[&str] = &["default", "dev", "office"];

/// 人设文件格式示例（逐行渲染，保证等宽排版不被折行破坏）。
const SAMPLE_LINES: &[&str] = &[
    "---",
    "name: 分析师",
    "description: 数据与逻辑优先，先给结论",
    "---",
    "你是一位严谨的分析师。回答先给结论，再列证据与步骤。",
];

// ============================================================
// 纯函数（可单测）
// ============================================================

/// 头像上的首字符：拉丁取首字母大写，中文取第一个字，空名回退到 "?"。
fn initial(name: &str) -> String {
    match name.trim().chars().next() {
        Some(ch) => ch.to_uppercase().collect(),
        None => "?".to_string(),
    }
}

/// 取路径的文件名并去掉 `.md` 后缀（兼容 `/` 与 `\`）。
fn file_stem(path: &str) -> String {
    let base = path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
        .trim();
    base.strip_suffix(".md").unwrap_or(base).to_string()
}

/// 卡片右下角的来源标签。
fn source_label(p: &Persona) -> String {
    match p.source_path.as_deref() {
        Some(path) if !path.trim().is_empty() => format!("personas/{}.md", file_stem(path)),
        _ => "内置人设".to_string(),
    }
}

/// 人设 id 是否合法：非空、仅小写字母/数字/`-`/`_`，长度不超过 40。
fn valid_persona_id(id: &str) -> bool {
    let id = id.trim();
    !id.is_empty()
        && id.len() <= 40
        && id
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' || ch == '_')
}

/// 是否是内置人设 id（禁止删除）。
fn builtin_persona_id(id: &str) -> bool {
    BUILTIN_PERSONA_IDS.contains(&id.trim())
}

/// 新建时 id 是否与现有人设重名（编辑时排除自己）。
fn persona_id_conflict(id: &str, personas: &[Persona], editing_id: &str) -> bool {
    let id = id.trim();
    !id.is_empty() && id != editing_id.trim() && personas.iter().any(|p| p.id == id)
}

/// 人设展示名：空 name 回退到 id，再回退到"未命名"。
fn display_name(name: &str, id: &str) -> String {
    let name = name.trim();
    if !name.is_empty() {
        return name.to_string();
    }
    let id = id.trim();
    if id.is_empty() {
        "未命名人设".to_string()
    } else {
        id.to_string()
    }
}

// ============================================================
// 小部件
// ============================================================

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

fn page_header(st: &AppState, c: &UiColors) -> Element<Msg> {
    let editing = st.persona_edit.is_some();
    row()
        .w_full()
        .items_end()
        .justify_between()
        .gap(space::MD)
        .children((
            col().grow().min_w(0.0).gap(2.0).children((
                text("人设")
                    .size(TextSize::Lg)
                    .weight(Weight::Semibold)
                    .color(c.text),
                text("人设决定代理的身份、语气与做事方式；可可视化编辑并新建。")
                    .size(TextSize::Xs)
                    .color(c.text_muted),
            )),
            row().items_center().gap(space::SM).shrink0().children((
                badge(format!("{} 个", st.personas.len()), Status::Accent),
                button("新建人设")
                    .variant(ButtonVariant::Primary)
                    .size(ControlSize::Sm)
                    .disabled(editing)
                    .on_click(Msg::PersonaEditStart(None))
                    .id("persona-new"),
                button("重新扫描")
                    .variant(ButtonVariant::Secondary)
                    .size(ControlSize::Sm)
                    .on_click(Msg::PersonaReload)
                    .id("persona-reload"),
            )),
        ))
}

fn format_card(c: &UiColors) -> Element<Msg> {
    let code_lines: Vec<Element<Msg>> = SAMPLE_LINES
        .iter()
        .map(|line| {
            let is_rule = *line == "---";
            text(*line)
                .size(TextSize::Xs)
                .mono()
                .color(if is_rule { c.text_subtle } else { c.text_muted })
                .into()
        })
        .collect();

    col()
        .w_full()
        .p(space::MD)
        .gap(space::MD)
        .bg(c.surface_raised)
        .border(1.0, c.border_subtle)
        .rounded(R_CARD)
        .children((
            row().items_center().gap(space::SM).children((
                icons::lucide::file().w(16.0).h(16.0).color(c.text_subtle),
                text("人设文件格式（也可以只用可视化编辑）")
                    .size(TextSize::Sm)
                    .weight(Weight::Semibold)
                    .color(c.text),
            )),
            text("保存后写入 personas/<id>.md：YAML front-matter 用 --- 包住，正文就是系统提示词。文件名即人设 id。")
                .size(TextSize::Xs)
                .color(c.text_muted)
                .leading(1.6),
            col()
                .w_full()
                .p(space::SM + 2.0)
                .gap(2.0)
                .bg(c.bg)
                .border(1.0, c.border_subtle)
                .rounded(R_CONTROL)
                .children(code_lines),
        ))
}

/// 编辑区：新建 / 编辑人设。字段通过 `Msg::PersonaEditField` 实时写回 AppState，
/// 保存/取消/删除分别发 `PersonaSaveEdit` / `PersonaCancelEdit` / `PersonaDeleteRequest`。
fn persona_editor(st: &AppState, c: &UiColors, edit: &PersonaEditState) -> Element<Msg> {
    let title = if edit.is_new {
        "新建人设".to_string()
    } else {
        format!("编辑人设：{}", display_name(&edit.name, &edit.id))
    };

    // ---- id ----
    let id_ok = !edit.is_new
        || (valid_persona_id(&edit.id) && !persona_id_conflict(&edit.id, &st.personas, ""));
    let id_hint = if !edit.is_new {
        String::new()
    } else if edit.id.trim().is_empty() {
        "id 必填：只允许小写字母、数字、- 和 _（例如 analyst-2）。".to_string()
    } else if !valid_persona_id(&edit.id) {
        "id 格式不对：只允许小写字母、数字、- 和 _，最长 40 字符。".to_string()
    } else if persona_id_conflict(&edit.id, &st.personas, "") {
        format!("id \"{}\" 已存在，换一个。", edit.id.trim())
    } else {
        "保存后写入 personas/<id>.md；id 之后不可更改。".to_string()
    };
    let id_block: Element<Msg> = if edit.is_new {
        let input: Element<Msg> = text_input(edit.id.clone())
            .placeholder("例如 analyst")
            .width(320.0)
            .on_input(|s| Msg::PersonaEditField {
                field: "id".to_string(),
                value: s,
            })
            .id("persona-edit-id")
            .into();
        let input = input.w_full().min_w(0.0).invalid(!id_ok);
        col()
            .w_full()
            .gap(space::XS + 2.0)
            .children((
                text("id（文件名）")
                    .size(TextSize::Xs)
                    .weight(Weight::Medium)
                    .color(c.text_muted),
                input,
                text(id_hint)
                    .size(TextSize::Xs)
                    .color(if id_ok { c.text_subtle } else { c.danger }),
            ))
            .into()
    } else {
        col()
            .w_full()
            .gap(space::XS + 2.0)
            .children((
                text("id（文件名，不可修改）")
                    .size(TextSize::Xs)
                    .weight(Weight::Medium)
                    .color(c.text_muted),
                row()
                    .w_full()
                    .p(space::SM)
                    .bg(c.bg)
                    .border(1.0, c.border_subtle)
                    .rounded(R_CHIP)
                    .children((
                        text(edit.id.clone())
                            .size(TextSize::Sm)
                            .mono()
                            .color(c.text)
                            .grow()
                            .truncate(),
                    )),
                text(source_hint(edit))
                    .size(TextSize::Xs)
                    .color(c.text_subtle),
            ))
            .into()
    };

    // ---- name / description ----
    let name_input: Element<Msg> = text_input(edit.name.clone())
        .placeholder("例如 分析师")
        .width(320.0)
        .on_input(|s| Msg::PersonaEditField {
            field: "name".to_string(),
            value: s,
        })
        .id("persona-edit-name")
        .into();
    let desc_input: Element<Msg> = text_input(edit.description.clone())
        .placeholder("一句话说明这个人设（显示在卡片上）")
        .width(320.0)
        .on_input(|s| Msg::PersonaEditField {
            field: "description".to_string(),
            value: s,
        })
        .id("persona-edit-description")
        .into();

    // ---- body ----
    let body_area: Element<Msg> = text_area(edit.body.clone())
        .placeholder("这里写系统提示词：身份、语气、做事方式、边界……")
        .width(320.0)
        .min_height(420.0)
        .on_input(|s| Msg::PersonaEditField {
            field: "body".to_string(),
            value: s,
        })
        .id("persona-edit-body")
        .into();
    // 正文编辑区固定高度 + 内部滚动：
    // text_area 自身会随内容长高，长提示词会把整张编辑卡撑高，鼠标停在正文上滚轮
    // 会被输入框吃掉、外层页面滚不动（用户报"人设正文处无法滚动"）。
    // 用固定高度的滚动容器包住它，正文超出 420px 后滚轮在正文上也能滚动。
    let body_area = body_area.w_full().mono();

    let builtin = !edit.is_new && builtin_persona_id(&edit.id);
    let can_delete = !edit.is_new && !builtin;
    let can_save = !edit.is_new || id_ok;
    let delete_id = edit.id.clone();
    let save_hint = if builtin {
        "内置人设可以编辑并另存，但不能删除。"
    } else {
        "保存后会立即重新扫描人设列表。"
    };

    let mut rows: Vec<Element<Msg>> = vec![
        row().w_full().items_center().gap(space::SM).children((
            text(title)
                .size(TextSize::Base)
                .weight(Weight::Semibold)
                .color(c.text)
                .grow()
                .min_w(0.0)
                .truncate(),
            chip(c, if edit.is_new { "新建" } else { "编辑中" }),
            if builtin {
                chip(c, "内置人设")
            } else {
                col().w(0.0)
            },
        )).into(),
        id_block,
        col().w_full().gap(space::XS + 2.0).children((
            text("名称")
                .size(TextSize::Xs)
                .weight(Weight::Medium)
                .color(c.text_muted),
            name_input.w_full().min_w(0.0),
        )).into(),
        col().w_full().gap(space::XS + 2.0).children((
            text("描述")
                .size(TextSize::Xs)
                .weight(Weight::Medium)
                .color(c.text_muted),
            desc_input.w_full().min_w(0.0),
        )).into(),
        col().w_full().gap(space::XS + 2.0).children((
            text("人设正文（系统提示词，等宽显示）")
                .size(TextSize::Xs)
                .weight(Weight::Medium)
                .color(c.text_muted),
            body_area,
            text("建议 300–2000 字：写清身份、语气、执行习惯和禁止事项。")
                .size(TextSize::Xs)
                .color(c.text_subtle),
        )).into(),
    ];

    if !edit.error.trim().is_empty() {
        rows.push(
            text(neosonia_core::util::truncate_chars(edit.error.trim(), 240))
                .size(TextSize::Xs)
                .color(c.danger)
                .leading(1.4)
                .into(),
        );
    }

    rows.push(divider());
    rows.push(
        row()
            .w_full()
            .items_center()
            .gap(space::SM)
            .wrap()
            .children((
                button("保存人设")
                    .variant(ButtonVariant::Primary)
                    .size(ControlSize::Sm)
                    .disabled(!can_save)
                    .on_click(Msg::PersonaSaveEdit)
                    .id("persona-edit-save"),
                button("取消")
                    .variant(ButtonVariant::Secondary)
                    .size(ControlSize::Sm)
                    .on_click(Msg::PersonaCancelEdit)
                    .id("persona-edit-cancel"),
                button("删除人设")
                    .variant(ButtonVariant::Danger)
                    .size(ControlSize::Sm)
                    .disabled(!can_delete)
                    .on_click(Msg::PersonaDeleteRequest(delete_id))
                    .id("persona-edit-delete"),
            ))
            .into(),
    );
    rows.push(
        text(if builtin {
            "内置人设不可删除；可修改后保存为自己的副本。"
        } else {
            save_hint
        })
        .size(TextSize::Xs)
        .color(c.text_subtle)
        .leading(1.5)
        .into(),
    );

    col()
        .w_full()
        .min_h(0.0)
        .shrink0()
        .p(space::LG)
        .gap(space::MD)
        .bg(c.surface_raised)
        .border(1.0, c.accent)
        .rounded(R_CARD)
        .id("persona-editor")
        .children(rows)
}

fn source_hint(edit: &PersonaEditState) -> String {
    if builtin_persona_id(&edit.id) {
        "内置人设：编辑后会生成 personas/<id>.md 覆盖默认内容。".to_string()
    } else {
        "来源：personas/<id>.md；保存会覆盖该文件。".to_string()
    }
}

fn persona_card(p: &Persona, active: bool, c: &UiColors) -> Element<Msg> {
    let id = p.id.clone();
    let select_id = id.clone();
    let edit_id = id.clone();
    let switch_id = id.clone();
    let name = display_name(&p.name, &p.id);
    let description = if p.description.trim().is_empty() {
        "（没有填写 description）".to_string()
    } else {
        p.description.clone()
    };

    let head = row()
        .w_full()
        .items_center()
        .gap(space::SM)
        .on_click(Msg::PersonaSelect(select_id))
        .cursor(Cursor::Pointer)
        .id(&format!("persona-head-{id}"))
        .children((
            avatar(initial(&name)),
            col().grow().min_w(0.0).gap(2.0).children((
                row().items_center().gap(space::SM).children((
                    text(name)
                        .size(TextSize::Base)
                        .weight(Weight::Semibold)
                        .color(c.text)
                        .grow()
                        .truncate(),
                    if active {
                        badge("使用中", Status::Success)
                    } else {
                        col().w(0.0)
                    },
                )),
                text(description)
                    .size(TextSize::Xs)
                    .color(c.text_muted)
                    .leading(1.5),
            )),
        ));

    let mut foot = row()
        .w_full()
        .items_center()
        .gap(space::SM)
        .children((
            icons::lucide::file().w(14.0).h(14.0).color(c.text_subtle),
            text(source_label(p))
                .size(TextSize::Xs)
                .mono()
                .color(c.text_subtle)
                .grow()
                .truncate(),
        ));
    if !active {
        foot = foot.child(
            button("切换到此")
                .variant(ButtonVariant::Secondary)
                .size(ControlSize::Xs)
                .on_click(Msg::PersonaSelect(switch_id))
                .id(&format!("persona-use-{id}")),
        );
    }
    foot = foot.child(
        button("编辑")
            .variant(ButtonVariant::Ghost)
            .size(ControlSize::Xs)
            .on_click(Msg::PersonaEditStart(Some(edit_id)))
            .id(&format!("persona-edit-{id}")),
    );

    col()
        .w_full()
        .p(space::MD)
        .gap(space::SM)
        .bg(c.surface_raised)
        .border(1.0, if active { c.accent } else { c.border_subtle })
        .rounded(R_CARD)
        .id(&format!("persona-card-{id}"))
        .children((head, foot))
}

fn empty_hint(c: &UiColors) -> Element<Msg> {
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
                .children((icons::lucide::user().w(20.0).h(20.0).color(c.text_subtle),)),
            text("还没有自定义人设")
                .size(TextSize::Base)
                .weight(Weight::Semibold)
                .color(c.text),
            text("点右上角“新建人设”可视化创建，或把 .md 文件放进 personas/ 目录后重新扫描。")
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
    let mut children: Vec<Element<Msg>> = vec![page_header(st, c)];

    // 编辑中：只显示编辑器（取消后回到列表），避免长表单和卡片堆在一起。
    if let Some(edit) = st.persona_edit.as_ref() {
        children.push(persona_editor(st, c, edit));
        return col()
            .w_full()
            .grow()
            .min_h(0.0)
            .scroll_y()
            .p(space::LG)
            .gap(space::LG)
            .id(PANEL_ID)
            .children(children)
            .into();
    }

    let mut cards: Vec<Element<Msg>> = Vec::new();

    // 内置默认人设始终可用；如果 personas 里已经有 default 就不重复。
    if !st.personas.iter().any(|p| p.id == "default") {
        let builtin = Persona::builtin_default();
        cards.push(persona_card(
            &builtin,
            st.active_persona == builtin.id,
            c,
        ));
    }
    for p in &st.personas {
        cards.push(persona_card(p, p.id == st.active_persona, c));
    }

    children.push(format_card(c));
    children.push(
        row()
            .w_full()
            .items_center()
            .justify_between()
            .gap(space::SM)
            .children((
                text("可用人设")
                    .size(TextSize::Sm)
                    .weight(Weight::Semibold)
                    .color(c.text),
                chip(c, "点名称切换 · 编辑可改内容"),
            ))
            .into(),
    );
    children.push(col().w_full().gap(space::SM).children(cards));
    if st.personas.is_empty() {
        children.push(empty_hint(c));
    }

    col()
        .w_full()
        .grow()
        .min_h(0.0)
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
    fn initials_handle_latin_and_cjk() {
        assert_eq!(initial("NeoSonia"), "N");
        assert_eq!(initial("分析师"), "分");
        assert_eq!(initial("  sonia"), "S");
        assert_eq!(initial(""), "?");
        assert_eq!(initial("   "), "?");
    }

    #[test]
    fn file_stems_and_sources_are_short() {
        assert_eq!(file_stem("personas/analyst.md"), "analyst");
        assert_eq!(file_stem("C:\\NeoSonia\\personas\\writer.md"), "writer");
        assert_eq!(file_stem("personas/plain"), "plain");
        assert_eq!(file_stem(""), "");

        let mut p = Persona::builtin_default();
        assert_eq!(source_label(&p), "内置人设");
        p.source_path = Some("personas/analyst.md".into());
        assert_eq!(source_label(&p), "personas/analyst.md");
    }

    #[test]
    fn persona_ids_are_validated() {
        assert!(valid_persona_id("analyst"));
        assert!(valid_persona_id("analyst-2"));
        assert!(valid_persona_id("a_b9"));
        assert!(!valid_persona_id(""));
        assert!(!valid_persona_id("Analyst"));
        assert!(!valid_persona_id("分析师"));
        assert!(!valid_persona_id("a b"));
        assert!(!valid_persona_id("a/b"));
        assert!(!valid_persona_id(&"x".repeat(41)));

        assert!(builtin_persona_id("default"));
        assert!(builtin_persona_id("dev"));
        assert!(builtin_persona_id("office"));
        assert!(!builtin_persona_id("analyst"));
        assert!(!builtin_persona_id("Default"));
    }

    #[test]
    fn persona_id_conflicts_and_display_names() {
        let personas = vec![
            Persona::builtin_default(),
            Persona {
                id: "analyst".into(),
                name: "分析师".into(),
                ..Persona::builtin_default()
            },
        ];
        assert!(persona_id_conflict("analyst", &personas, ""));
        assert!(!persona_id_conflict("analyst", &personas, "analyst"));
        assert!(!persona_id_conflict("writer", &personas, ""));
        assert!(!persona_id_conflict("", &personas, ""));

        assert_eq!(display_name("  分析师 ", "analyst"), "分析师");
        assert_eq!(display_name("", "analyst"), "analyst");
        assert_eq!(display_name("", "   "), "未命名人设");
    }
    /// 编辑器页必须真的能滚动：滚动容器里的 flex 子项默认 flex-shrink:1，
    /// 卡片会被压到视口高度 → content_height == viewport → scroll_y 不生效
    /// （用户报"人设正文处无法滚动"的根因）。这里断言 editor 卡片不被压缩、
    /// 页面可滚、滚轮命中页面滚动容器、滚动后正文上移（字和框一起滚）。
    #[test]
    fn persona_editor_page_scrolls_long_body() {
        use neosonia_core::config::{Paths, Settings};
        let mut st = AppState::new(
            Paths::from_root("C:/tmp/neosonia-persona-layout"),
            Settings::default(),
        );
        st.persona_edit = Some(PersonaEditState {
            is_new: false,
            id: "default".into(),
            name: "测试人设".into(),
            description: "长正文滚动测试".into(),
            body: (0..150)
                .map(|i| format!("第 {i} 行：这是一段用于测试人设正文滚动区域的系统提示词内容。"))
                .collect::<Vec<_>>()
                .join("\n"),
            error: String::new(),
        });
        let theme = crate::theme::nova_theme(neosonia_core::config::ThemeMode::Dark, true, 1.0);
        let c = crate::theme::colors(&theme);
        let mut fonts = Fonts::embedded();
        let mut state = FrameState::new();

        let root = view(&st, &c);
        let frame = build_frame(&root, &theme, &mut fonts, &mut state, (424.0, 760.0), 1.0);
        let page = frame.get(&by::id(PANEL_ID));
        let editor = frame.get(&by::id("persona-editor"));
        let body = frame.get(&by::id("persona-edit-body"));
        assert!(
            editor.rect.height() > page.rect.height(),
            "编辑器卡片({:.1}) 应高于视口({:.1})，否则 scroll_y 不生效",
            editor.rect.height(),
            page.rect.height()
        );
        assert!(
            body.rect.height() > 300.0,
            "正文编辑区应保持完整高度（不被压缩），实际 {:.1}",
            body.rect.height()
        );
        assert!(frame.is_scrollable(page.id), "人设页应可滚动");
        // 正文元素很高（可能超出视口底部），取它可见范围内的一点做滚轮命中测试。
        let probe_y = (body.rect.y0 + 40.0).min(page.rect.y1 - 20.0);
        let probe = fenestra::shell::vello::kurbo::Point::new(body.rect.center().x, probe_y);
        let got = frame.scrollable_y_at(probe);
        assert_eq!(
            got,
            Some(page.id),
            "鼠标停在正文上时滚轮应路由到页面滚动容器（字和框一起滚），实际 {got:?}；page.id={:?} body.id={:?}",
            page.id,
            body.id
        );
        state.scroll_by(page.id, 1_000_000.0);
        let frame2 = build_frame(&root, &theme, &mut fonts, &mut state, (424.0, 760.0), 1.0);
        let body2 = frame2.get(&by::id("persona-edit-body"));
        assert!(
            body2.rect.y0 < body.rect.y0 - 100.0,
            "滚动后正文应上移（{:.1} -> {:.1}）",
            body.rect.y0,
            body2.rect.y0
        );
    }
}
