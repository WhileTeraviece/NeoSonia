//! Single-line text editing on parley's `PlainEditor`: per-widget editor
//! state, keyboard/pointer handling, and painting (selection, caret,
//! placeholder, horizontal follow-scroll).

use kurbo::{Affine, Rect};
use parley::PositionedLayoutItem;
use peniko::{Color, Fill};
use vello::Scene;

use crate::clipboard::Clipboard;
use crate::events::{Key, KeyInput};
use crate::text::{Fonts, LayoutBrush, ResolvedText};

/// Caret blink period: 530ms visible, 530ms hidden.
const BLINK_HALF_PERIOD: f64 = 0.53;
/// Caret stroke width in logical px.
const CARET_WIDTH: f64 = 1.5;

/// Retained editing state for one input widget.
pub(crate) struct EditorState {
    pub editor: parley::PlainEditor<LayoutBrush>,
    /// Horizontal scroll keeping the caret visible in a narrow box.
    pub scroll_x: f64,
    /// Clock time of the last edit or caret move (restarts the blink).
    pub last_activity: f64,
    /// Frame stamp for garbage collection.
    pub seen: u64,
    /// Multiline mode: wraps, accepts newlines, moves by line.
    pub multiline: bool,
    /// Undo/redo history (QUndoStack semantics: coalesced runs,
    /// boundaries on caret moves, redo cleared by new edits).
    pub undo: UndoStack,
}

/// What kind of edit a coalescing run holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditRun {
    Insert,
    Delete,
}

/// One restorable editor moment: text plus the selected byte range.
#[derive(Debug, Clone)]
struct Snapshot {
    text: String,
    selection: (usize, usize),
}

/// Bounded undo/redo stacks with typing-run coalescing.
#[derive(Debug, Default)]
pub(crate) struct UndoStack {
    undos: Vec<Snapshot>,
    redos: Vec<Snapshot>,
    /// The open coalescing run; edits of the same kind merge into it.
    run: Option<EditRun>,
}

/// History depth: plenty for a field, bounded so editors never grow
/// without limit.
const UNDO_LIMIT: usize = 100;

fn snapshot(editor: &parley::PlainEditor<LayoutBrush>) -> Snapshot {
    let range = editor.raw_selection().text_range();
    Snapshot {
        // 用 `text()` 而不是 `raw_text()`：后者包含 IME 预编辑区，
        // 不应写进撤销栈（撤销会恢复出不存在的"组合中"文本）。
        text: editor.text().to_string(),
        selection: (range.start, range.end),
    }
}

impl UndoStack {
    /// Opens (or continues) a coalescing run of `kind`: the first edit
    /// of a run snapshots the pre-edit state and clears redos.
    fn begin(&mut self, editor: &parley::PlainEditor<LayoutBrush>, kind: EditRun) {
        if self.run != Some(kind) {
            self.undos.push(snapshot(editor));
            if self.undos.len() > UNDO_LIMIT {
                self.undos.remove(0);
            }
            self.redos.clear();
            self.run = Some(kind);
        }
    }

    /// Ends the open run: the next edit starts a fresh undo unit.
    /// Called on caret/selection moves and external value changes.
    pub(crate) fn break_run(&mut self) {
        self.run = None;
    }
}

impl EditorState {
    pub(crate) fn new(style: &ResolvedText, now: f64, multiline: bool) -> Self {
        let mut editor = parley::PlainEditor::new(style.px);
        apply_style(&mut editor, style);
        editor.set_width(None); // multiline editors get their width at paint
        Self {
            editor,
            scroll_x: 0.0,
            last_activity: now,
            seen: 0,
            multiline,
            undo: UndoStack::default(),
        }
    }

    /// Syncs the editor with the app-provided value and text style. The app
    /// is the source of truth: external changes reset the buffer.
    ///
    /// 比较用 `text()`（排除 IME 预编辑区）而不是 `raw_text()`：后者在中文
    /// 拼音组合期间包含预编辑文本，会与 app 的"已提交值"永远不相等 →
    /// 每帧 `set_text` 清掉组合并把光标重置到末尾（输入位置错乱）。
    pub(crate) fn sync(&mut self, value: &str, style: &ResolvedText) {
        if self.editor.text() != value {
            let fresh = self.editor.text() == ""
                && self.undo.undos.is_empty()
                && self.undo.redos.is_empty();
            if fresh {
                // First fill of a brand-new editor: nothing to undo.
                self.editor.set_text(value);
                apply_style(&mut self.editor, style);
                return;
            }
            // External (programmatic) change: its own undo unit, so
            // undo can step back across it.
            self.undo.undos.push(snapshot(&self.editor));
            if self.undo.undos.len() > UNDO_LIMIT {
                self.undo.undos.remove(0);
            }
            self.undo.redos.clear();
            self.undo.break_run();
            self.editor.set_text(value);
        }
        apply_style(&mut self.editor, style);
    }
}

fn apply_style(editor: &mut parley::PlainEditor<LayoutBrush>, style: &ResolvedText) {
    use parley::{FontFamily, FontWeight, GenericFamily, LineHeight, StyleProperty};
    let styles = editor.edit_styles();
    styles.insert(StyleProperty::FontSize(style.px));
    styles.insert(StyleProperty::FontWeight(FontWeight::new(style.weight)));
    styles.insert(StyleProperty::LineHeight(LineHeight::FontSizeRelative(
        style.line_height,
    )));
    styles.insert(StyleProperty::LetterSpacing(style.letter_spacing));
    // Features and optical sizing are insert-or-*remove*: re-applying a style
    // that turned a feature (or the opsz axis) off must clear the prior property,
    // not leave it stuck on a persistent editor (the 0.16 known limitation).
    // `edit_styles` is a discriminant-keyed set, so removal by variant is exact.
    match style.features.feature_string() {
        Some(s) => {
            styles.insert(StyleProperty::FontFeatures(parley::FontFeatures::Source(
                std::borrow::Cow::Owned(s),
            )));
        }
        None => {
            styles.remove(std::mem::discriminant(&StyleProperty::FontFeatures(
                parley::FontFeatures::empty(),
            )));
        }
    }
    // Optical sizing (no-op on the editor's static Inter; correct if an app
    // registers a variable face).
    match style.optical.opsz_at(style.px) {
        Some(opsz) => {
            styles.insert(StyleProperty::FontVariations(
                parley::FontVariations::Source(std::borrow::Cow::Owned(format!("\"opsz\" {opsz}"))),
            ));
        }
        None => {
            styles.remove(std::mem::discriminant(&StyleProperty::FontVariations(
                parley::FontVariations::empty(),
            )));
        }
    }
    styles.insert(StyleProperty::FontFamily(match style.family {
        crate::tokens::FamilyRole::Sans => FontFamily::named("Inter"),
        crate::tokens::FamilyRole::Mono => FontFamily::Single(GenericFamily::Monospace.into()),
        // Editors keep the body face; display/serif faces are for static
        // text (registered names are not plumbed into editors yet).
        crate::tokens::FamilyRole::Display | crate::tokens::FamilyRole::Serif => {
            FontFamily::named("Inter")
        }
    }));
}

/// The outcome of one editor event.
pub(crate) struct EditOutcome {
    /// The buffer changed (emit `on_input`).
    pub changed: bool,
    /// The event was consumed by the editor (do not forward to `on_key`).
    pub consumed: bool,
}

const HANDLED: EditOutcome = EditOutcome {
    changed: true,
    consumed: true,
};
const MOVED: EditOutcome = EditOutcome {
    changed: false,
    consumed: true,
};
const IGNORED: EditOutcome = EditOutcome {
    changed: false,
    consumed: false,
};

/// Applies a key press to the editor. Word jumps follow the common
/// conventions: Alt+arrow (mac) or Ctrl+arrow jump words; Cmd/Ctrl+arrow or
/// Home/End jump to the line edges; Cmd/Ctrl+A/C/X/V are the selection and
/// clipboard shortcuts.
pub(crate) fn handle_key(
    state: &mut EditorState,
    fonts: &mut Fonts,
    clipboard: &mut dyn Clipboard,
    key: &KeyInput,
) -> EditOutcome {
    let outcome = handle_key_inner(state, fonts, clipboard, key);
    if !outcome.changed {
        // Caret/selection moves are coalescing boundaries: the next
        // edit starts a fresh undo unit.
        state.undo.break_run();
    }
    outcome
}

fn handle_key_inner(
    state: &mut EditorState,
    fonts: &mut Fonts,
    clipboard: &mut dyn Clipboard,
    key: &KeyInput,
) -> EditOutcome {
    // IME 组合期间：左右方向键只移动"预编辑光标"，其余按键交给 IME
    // （这里只消费事件，绝不落到编辑已提交文本的分支，否则会删改/插入到错误位置）。
    if state.editor.raw_compose().is_some() {
        return match key.key {
            Key::ArrowLeft if !key.shift => compose_move(state, fonts, -1),
            Key::ArrowRight if !key.shift => compose_move(state, fonts, 1),
            _ => MOVED,
        };
    }
    let multiline = state.multiline;
    let (font_cx, layout_cx) = fonts.editor_contexts();
    let mut drv = state.editor.driver(font_cx, layout_cx);
    let shortcut = key.meta || key.ctrl;
    let word = key.alt || (key.ctrl && !key.meta);

    match key.key {
        Key::Char(c) if shortcut => match c.to_ascii_lowercase() {
            'a' => {
                drv.select_all();
                MOVED
            }
            'c' => {
                if let Some(text) = drv.editor.selected_text() {
                    clipboard.set(text.to_owned());
                }
                MOVED
            }
            'x' => {
                if let Some(text) = drv.editor.selected_text() {
                    clipboard.set(text.to_owned());
                    // Cut is its own undo unit, never coalesced.
                    state.undo.break_run();
                    state.undo.begin(&state.editor, EditRun::Delete);
                    let (font_cx, layout_cx) = fonts.editor_contexts();
                    let mut drv = state.editor.driver(font_cx, layout_cx);
                    drv.delete_selection();
                    state.undo.break_run();
                    return HANDLED;
                }
                MOVED
            }
            'v' => {
                if let Some(text) = clipboard.get() {
                    // Paste is its own undo unit, never coalesced.
                    state.undo.break_run();
                    state.undo.begin(&state.editor, EditRun::Insert);
                    let (font_cx, layout_cx) = fonts.editor_contexts();
                    let mut drv = state.editor.driver(font_cx, layout_cx);
                    drv.insert_or_replace_selection(&sanitize(&text, multiline));
                    state.undo.break_run();
                    return HANDLED;
                }
                MOVED
            }
            'z' => {
                let applied = if key.shift {
                    redo(state, fonts)
                } else {
                    undo(state, fonts)
                };
                if applied { HANDLED } else { MOVED }
            }
            'y' if key.ctrl => {
                if redo(state, fonts) {
                    HANDLED
                } else {
                    MOVED
                }
            }
            _ => IGNORED,
        },
        // Control characters (Enter arriving as '\r', Tab, DEL, ...) never
        // become text, matching the text-commit and paste path filters.
        Key::Char(c) if c.is_control() => IGNORED,
        Key::Char(c) if !key.ctrl && !key.meta => {
            state.undo.begin(&state.editor, EditRun::Insert);
            let (font_cx, layout_cx) = fonts.editor_contexts();
            let mut drv = state.editor.driver(font_cx, layout_cx);
            drv.insert_or_replace_selection(&c.to_string());
            HANDLED
        }
        Key::ArrowLeft => {
            match (key.shift, word, key.meta) {
                (true, _, true) => drv.select_to_line_start(),
                (false, _, true) => drv.move_to_line_start(),
                (true, true, _) => drv.select_word_left(),
                (false, true, _) => drv.move_word_left(),
                (true, false, _) => drv.select_left(),
                (false, false, _) => drv.move_left(),
            }
            MOVED
        }
        Key::ArrowRight => {
            match (key.shift, word, key.meta) {
                (true, _, true) => drv.select_to_line_end(),
                (false, _, true) => drv.move_to_line_end(),
                (true, true, _) => drv.select_word_right(),
                (false, true, _) => drv.move_word_right(),
                (true, false, _) => drv.select_right(),
                (false, false, _) => drv.move_right(),
            }
            MOVED
        }
        Key::Home => {
            if key.shift {
                drv.select_to_line_start();
            } else {
                drv.move_to_line_start();
            }
            MOVED
        }
        Key::End => {
            if key.shift {
                drv.select_to_line_end();
            } else {
                drv.move_to_line_end();
            }
            MOVED
        }
        Key::Enter if multiline => {
            state.undo.begin(&state.editor, EditRun::Insert);
            let (font_cx, layout_cx) = fonts.editor_contexts();
            let mut drv = state.editor.driver(font_cx, layout_cx);
            drv.insert_or_replace_selection("\n");
            HANDLED
        }
        Key::ArrowUp if multiline => {
            if key.shift {
                drv.select_up();
            } else {
                drv.move_up();
            }
            MOVED
        }
        Key::ArrowDown if multiline => {
            if key.shift {
                drv.select_down();
            } else {
                drv.move_down();
            }
            MOVED
        }
        Key::Backspace => {
            state.undo.begin(&state.editor, EditRun::Delete);
            let (font_cx, layout_cx) = fonts.editor_contexts();
            let mut drv = state.editor.driver(font_cx, layout_cx);
            if word {
                drv.backdelete_word();
            } else {
                drv.backdelete();
            }
            HANDLED
        }
        Key::Delete => {
            state.undo.begin(&state.editor, EditRun::Delete);
            let (font_cx, layout_cx) = fonts.editor_contexts();
            let mut drv = state.editor.driver(font_cx, layout_cx);
            if word {
                drv.delete_word();
            } else {
                drv.delete();
            }
            HANDLED
        }
        _ => IGNORED,
    }
}

/// Filters control characters out of committed or pasted text. Multiline
/// editors keep newlines (normalized to `\n`); single-line editors strip
/// them with everything else.
fn sanitize(text: &str, multiline: bool) -> String {
    if multiline {
        text.replace("\r\n", "\n")
            .chars()
            .map(|c| if c == '\r' { '\n' } else { c })
            .filter(|c| *c == '\n' || !c.is_control())
            .collect()
    } else {
        text.chars().filter(|c| !c.is_control()).collect()
    }
}

/// Inserts committed text (typing or IME commit).
/// Applies one undo step. Returns whether anything changed.
pub(crate) fn undo(state: &mut EditorState, fonts: &mut Fonts) -> bool {
    let Some(snap) = state.undo.undos.pop() else {
        return false;
    };
    state.undo.redos.push(snapshot(&state.editor));
    apply_snapshot(state, fonts, &snap);
    true
}

/// Applies one redo step. Returns whether anything changed.
pub(crate) fn redo(state: &mut EditorState, fonts: &mut Fonts) -> bool {
    let Some(snap) = state.undo.redos.pop() else {
        return false;
    };
    state.undo.undos.push(snapshot(&state.editor));
    apply_snapshot(state, fonts, &snap);
    true
}

fn apply_snapshot(state: &mut EditorState, fonts: &mut Fonts, snap: &Snapshot) {
    state.editor.set_text(&snap.text);
    let (font_cx, layout_cx) = fonts.editor_contexts();
    let mut drv = state.editor.driver(font_cx, layout_cx);
    drv.select_byte_range(snap.selection.0, snap.selection.1);
    state.undo.break_run();
}

pub(crate) fn handle_text(state: &mut EditorState, fonts: &mut Fonts, text: &str) -> EditOutcome {
    // IME 组合中收到的 Text 事件是拼音按键（不是提交文本，提交走 ImeCommit）。
    // 直接忽略：否则拼音会被当已提交文本写进 buffer 和 app 的 value，
    // 表现为"每输入 N 个字母光标就往后跳 N 个字符、中文插在移动后的位置"。
    if state.editor.raw_compose().is_some() {
        return IGNORED;
    }
    state.undo.begin(&state.editor, EditRun::Insert);
    let sanitized = sanitize(text, state.multiline);
    if sanitized.is_empty() {
        return IGNORED;
    }
    let (font_cx, layout_cx) = fonts.editor_contexts();
    let mut drv = state.editor.driver(font_cx, layout_cx);
    drv.insert_or_replace_selection(&sanitized);
    HANDLED
}

/// Commits IME text: replaces the active preedit (if any) with the committed
/// text and leaves the caret after it. Without an active preedit this inserts
/// at the selection like normal text.
///
/// 必须先 `clear_compose` 再插入：直接 `insert_or_replace_selection` 会把
/// 提交文本插进"预编辑文本仍然存在"的缓冲区里，产生拼音残留/重复。
pub(crate) fn handle_commit(
    state: &mut EditorState,
    fonts: &mut Fonts,
    text: &str,
) -> EditOutcome {
    let sanitized = sanitize(text, state.multiline);
    let had_compose = state.editor.raw_compose().is_some();
    if sanitized.is_empty() && !had_compose {
        return IGNORED;
    }
    // 组合中：先移除预编辑（光标回到预编辑起点），空提交=取消组合。
    state.undo.begin(&state.editor, EditRun::Delete);
    let (font_cx, layout_cx) = fonts.editor_contexts();
    let mut drv = state.editor.driver(font_cx, layout_cx);
    if had_compose {
        drv.clear_compose();
    }
    if sanitized.is_empty() {
        state.undo.break_run();
        return HANDLED;
    }
    drv.insert_or_replace_selection(&sanitized);
    HANDLED
}

/// IME 组合期间移动预编辑光标（不改变已提交文本的 selection）。
///
/// parley 的 `set_compose` 会在预编辑区起点重新插入同样的文本并把 selection
/// 移到 `cursor` 指示的位置，所以用"同一段预编辑文本 + 新光标"重设即可实现
/// 组合内的左右移动（这正是中文输入法里"组合期间方向键无效"要修的行为）。
fn compose_move(state: &mut EditorState, fonts: &mut Fonts, delta: isize) -> EditOutcome {
    let Some(range) = state.editor.raw_compose().clone() else {
        return MOVED;
    };
    let preedit = state.editor.raw_text()[range.clone()].to_string();
    if preedit.is_empty() {
        return MOVED;
    }
    let selection = state.editor.raw_selection().text_range();
    let mut rel = selection.start.saturating_sub(range.start).min(preedit.len());
    while !preedit.is_char_boundary(rel) {
        rel -= 1;
    }
    let target = if delta > 0 {
        (rel + 1..=preedit.len())
            .find(|i| preedit.is_char_boundary(*i))
            .unwrap_or(preedit.len())
    } else if delta < 0 {
        (0..rel)
            .rev()
            .find(|i| preedit.is_char_boundary(*i))
            .unwrap_or(0)
    } else {
        rel
    };
    if target == rel {
        return MOVED;
    }
    let (font_cx, layout_cx) = fonts.editor_contexts();
    let mut drv = state.editor.driver(font_cx, layout_cx);
    drv.set_compose(&preedit, Some((target, target)));
    MOVED
}

/// Updates the IME preedit text.
pub(crate) fn handle_preedit(
    state: &mut EditorState,
    fonts: &mut Fonts,
    text: &str,
    cursor: Option<(usize, usize)>,
) {
    let (font_cx, layout_cx) = fonts.editor_contexts();
    let mut drv = state.editor.driver(font_cx, layout_cx);
    if text.is_empty() {
        drv.clear_compose();
    } else {
        // Platform IMEs are not trusted to keep offsets in range, and
        // parley debug-asserts on out-of-range compose cursors.
        let cursor = cursor.map(|(a, b)| (a.min(text.len()), b.min(text.len())));
        drv.set_compose(text, cursor);
    }
}

/// Moves the caret (or extends the selection) to a pointer position given in
/// coordinates local to the text origin.
pub(crate) fn pointer_down(
    state: &mut EditorState,
    fonts: &mut Fonts,
    x: f64,
    y: f64,
    shift: bool,
) {
    state.undo.break_run();
    let (font_cx, layout_cx) = fonts.editor_contexts();
    let mut drv = state.editor.driver(font_cx, layout_cx);
    #[expect(clippy::cast_possible_truncation, reason = "text coords fit in f32")]
    if shift {
        drv.shift_click_extension(x as f32, y as f32);
    } else {
        drv.move_to_point(x as f32, y as f32);
    }
}

/// Selects the word at a pointer position (double-click).
pub(crate) fn select_word_at(state: &mut EditorState, fonts: &mut Fonts, x: f64, y: f64) {
    state.undo.break_run();
    let (font_cx, layout_cx) = fonts.editor_contexts();
    let mut drv = state.editor.driver(font_cx, layout_cx);
    #[expect(clippy::cast_possible_truncation, reason = "text coords fit in f32")]
    drv.select_word_at_point(x as f32, y as f32);
}

/// Selects the whole line at a pointer position (triple-click).
pub(crate) fn select_line_at(state: &mut EditorState, fonts: &mut Fonts, x: f64, y: f64) {
    state.undo.break_run();
    let (font_cx, layout_cx) = fonts.editor_contexts();
    let mut drv = state.editor.driver(font_cx, layout_cx);
    #[expect(clippy::cast_possible_truncation, reason = "text coords fit in f32")]
    drv.select_line_at_point(x as f32, y as f32);
}

/// Extends the selection during a drag.
pub(crate) fn pointer_drag(state: &mut EditorState, fonts: &mut Fonts, x: f64, y: f64) {
    state.undo.break_run();
    let (font_cx, layout_cx) = fonts.editor_contexts();
    let mut drv = state.editor.driver(font_cx, layout_cx);
    #[expect(clippy::cast_possible_truncation, reason = "text coords fit in f32")]
    drv.extend_selection_to_point(x as f32, y as f32);
}

/// Everything the painter needs for one input, resolved at build time.
pub(crate) struct InputPaint {
    pub placeholder: String,
    pub style: ResolvedText,
    pub placeholder_color: Color,
    pub caret_color: Color,
    pub selection_color: Color,
    pub focused: bool,
    /// Left/right padding inside the box, logical px.
    pub pad_x: f64,
    /// Top padding inside the box (multiline is top-aligned), logical px.
    pub pad_y: f64,
    /// Multiline mode: wrapped, top-aligned, no horizontal follow-scroll.
    pub multiline: bool,
}

/// Paints an input's content (selection, text or placeholder, caret) inside
/// `rect`, clipped, vertically centered, with horizontal follow-scroll.
/// Returns the focused caret rect (for IME positioning), if any.
pub(crate) fn paint(
    scene: &mut Scene,
    fonts: &mut Fonts,
    state: &mut EditorState,
    data: &InputPaint,
    rect: Rect,
    now: f64,
    reduced_motion: bool,
) -> Option<Rect> {
    let content = Rect::new(rect.x0 + data.pad_x, rect.y0, rect.x1 - data.pad_x, rect.y1);

    // Multiline editors wrap to the content width (set before layout).
    if data.multiline {
        #[expect(clippy::cast_possible_truncation, reason = "widths fit in f32")]
        state
            .editor
            .set_width(Some(content.width().max(0.0) as f32));
    }

    // Refresh the layout; single lines center vertically, multiline is
    // top-aligned under the padding.
    let (font_cx, layout_cx) = fonts.editor_contexts();
    let layout_height = f64::from(state.editor.layout(font_cx, layout_cx).height());
    let text_y = if data.multiline {
        rect.y0 + data.pad_y
    } else {
        rect.y0 + (rect.height() - layout_height) * 0.5
    };

    // Follow-scroll: keep the caret inside the content box (single line
    // only; multiline grows vertically instead).
    if data.multiline {
        state.scroll_x = 0.0;
    } else if let Some(caret) = state.editor.cursor_geometry(1.0) {
        let caret_x = caret.x0; // layout-local
        let visible_w = content.width().max(0.0);
        if caret_x - state.scroll_x > visible_w {
            state.scroll_x = caret_x - visible_w;
        }
        if caret_x - state.scroll_x < 0.0 {
            state.scroll_x = caret_x;
        }
        let max_scroll = (f64::from(
            state
                .editor
                .try_layout()
                .map_or(0.0, parley::Layout::full_width),
        ) - visible_w)
            .max(0.0);
        state.scroll_x = state.scroll_x.clamp(0.0, max_scroll + CARET_WIDTH);
    } else {
        state.scroll_x = 0.0;
    }
    let text_origin = (content.x0 - state.scroll_x, text_y);

    scene.push_clip_layer(Fill::NonZero, Affine::IDENTITY, &rect);

    // Selection behind the glyphs.
    if data.focused {
        for (bb, _) in state.editor.selection_geometry() {
            let sel = Rect::new(
                text_origin.0 + bb.x0,
                text_origin.1 + bb.y0,
                text_origin.0 + bb.x1,
                text_origin.1 + bb.y1,
            );
            scene.fill(
                Fill::NonZero,
                Affine::IDENTITY,
                data.selection_color,
                None,
                &sel,
            );
        }
    }

    // Text, or the placeholder when empty.
    if state.editor.raw_text().is_empty() {
        if !data.placeholder.is_empty() {
            let mut placeholder_style = data.style;
            placeholder_style.color = data.placeholder_color;
            let ph_bottom = if data.multiline {
                rect.y1 - data.pad_y
            } else {
                text_y + layout_height
            };
            let ph_rect = Rect::new(content.x0, text_y, content.x1, ph_bottom);
            fonts.paint(scene, &data.placeholder, &placeholder_style, ph_rect, None);
        }
    } else {
        let transform = Affine::translate(text_origin);
        let color = data.style.color;
        let (font_cx, layout_cx) = fonts.editor_contexts();
        let layout = state.editor.layout(font_cx, layout_cx);
        for line in layout.lines() {
            for item in line.items() {
                let PositionedLayoutItem::GlyphRun(glyph_run) = item else {
                    continue;
                };
                let mut x = glyph_run.offset();
                let y = glyph_run.baseline();
                let run = glyph_run.run();
                scene
                    .draw_glyphs(run.font())
                    .brush(color)
                    .hint(true)
                    .transform(transform)
                    .font_size(run.font_size())
                    .normalized_coords(run.normalized_coords())
                    .draw(
                        Fill::NonZero,
                        glyph_run.glyphs().map(|glyph| {
                            let gx = x + glyph.x;
                            let gy = y + glyph.y;
                            x += glyph.advance;
                            vello::Glyph {
                                id: glyph.id,
                                x: gx,
                                y: gy,
                            }
                        }),
                    );
            }
        }
    }

    // The caret, blinking on a 530ms half-period. The rect is computed
    // whenever focused (even while blink-hidden) so the IME popup can
    // anchor to it.
    let mut caret_rect = None;
    if data.focused {
        let phase = ((now - state.last_activity).max(0.0) / BLINK_HALF_PERIOD) as u64;
        let visible = reduced_motion || phase.is_multiple_of(2);
        if let Some(bb) = state.editor.cursor_geometry(1.0) {
            let caret = Rect::new(
                (text_origin.0 + bb.x0 - CARET_WIDTH * 0.5).max(rect.x0),
                text_origin.1 + bb.y0 + 1.0,
                (text_origin.0 + bb.x0 + CARET_WIDTH * 0.5).max(rect.x0 + CARET_WIDTH),
                text_origin.1 + bb.y1 - 1.0,
            );
            caret_rect = Some(caret);
            if visible {
                scene.fill(
                    Fill::NonZero,
                    Affine::IDENTITY,
                    data.caret_color,
                    None,
                    &caret,
                );
            }
        }
    }

    scene.pop_layer();
    caret_rect
}

#[cfg(test)]
mod tests {
    use super::{LayoutBrush, apply_style};
    use crate::style::{NumericSpacing, OpticalSizing, TextStyle};
    use crate::text::resolve_text;
    use crate::theme::Theme;
    use std::mem::discriminant;

    /// Toggling an OpenType feature (or the opsz axis) off on a *persistent*
    /// editor must clear the prior property, not leave it stuck (the 0.16
    /// known limitation). White-box: inspect the editor's resolved style set.
    #[test]
    fn editor_clears_toggled_off_feature_and_opsz() {
        let theme = Theme::light();
        let mut editor = parley::PlainEditor::<LayoutBrush>::new(16.0);

        let feat_key = discriminant(&parley::StyleProperty::<LayoutBrush>::FontFeatures(
            parley::FontFeatures::empty(),
        ));
        let var_key = discriminant(&parley::StyleProperty::<LayoutBrush>::FontVariations(
            parley::FontVariations::empty(),
        ));

        // Apply a style WITH tabular figures and auto optical sizing.
        let on = TextStyle {
            features: crate::style::FontFeatures {
                spacing: NumericSpacing::Tabular,
                ..Default::default()
            },
            optical: OpticalSizing::Auto,
            ..Default::default()
        };
        apply_style(&mut editor, &resolve_text(&on, &theme));
        assert!(
            editor.edit_styles().inner().contains_key(&feat_key),
            "feature property is set when enabled"
        );
        assert!(
            editor.edit_styles().inner().contains_key(&var_key),
            "opsz variation is set under Auto"
        );

        // Re-apply a cleared style (features off, opsz explicitly off): both must
        // clear. opsz is `Default` (not the new `Inherit`, which would track the
        // theme's `Auto` and keep the variation set).
        let off = TextStyle {
            optical: OpticalSizing::Default,
            ..Default::default()
        };
        apply_style(&mut editor, &resolve_text(&off, &theme));
        assert!(
            !editor.edit_styles().inner().contains_key(&feat_key),
            "feature property is removed when toggled off"
        );
        assert!(
            !editor.edit_styles().inner().contains_key(&var_key),
            "opsz variation is removed when toggled off"
        );
    }

    // ---- IME / selection 回归测试（本地 patch 新增） ----
    // 背景：parley 的 `raw_text()` 包含 IME 预编辑区，app 的受控值只含已提交
    // 文本；若 sync 用 raw_text 比较，中文拼音组合期间会每帧把组合清掉并把
    // 光标重置到末尾。下面两个测试锁住"组合期间 sync 不动 selection"和
    // "commit 用提交文本替换预编辑、光标落在提交文本之后"。

    use super::{EditorState, handle_commit, handle_key, handle_preedit, handle_text};
    use crate::clipboard::MemoryClipboard;
    use crate::events::{Key, KeyInput};
    use crate::text::Fonts;

    fn test_editor() -> (EditorState, Fonts) {
        let theme = Theme::light();
        let style = resolve_text(&TextStyle::default(), &theme);
        (EditorState::new(&style, 0.0, true), Fonts::embedded())
    }

    fn style() -> crate::text::ResolvedText {
        resolve_text(&TextStyle::default(), &Theme::light())
    }

    fn set_caret(state: &mut EditorState, fonts: &mut Fonts, byte: usize) {
        let (font_cx, layout_cx) = fonts.editor_contexts();
        let mut drv = state.editor.driver(font_cx, layout_cx);
        drv.select_byte_range(byte, byte);
    }

    #[test]
    fn sync_keeps_selection_during_ime_preedit() {
        let (mut state, mut fonts) = test_editor();
        let style = style();
        state.sync("abc", &style);
        set_caret(&mut state, &mut fonts, 1);
        // 模拟拼音组合 "ni"：buffer 变成 "a|ni|bc"，光标在组合文本末尾。
        handle_preedit(&mut state, &mut fonts, "ni", Some((2, 2)));
        assert!(state.editor.raw_compose().is_some(), "组合应处于激活状态");
        let composed = state.editor.raw_selection().text_range();
        assert_eq!(composed.start, 3, "预编辑期间光标应在组合区末尾");

        // 关键回归：每帧 app 都会用"已提交值" sync，不能清组合/移动光标。
        state.sync("abc", &style);
        assert!(state.editor.raw_compose().is_some(), "sync 不应清掉 IME 组合");
        let after = state.editor.raw_selection().text_range();
        assert_eq!(
            (after.start, after.end),
            (composed.start, composed.end),
            "sync 不应移动 IME 组合期间的光标"
        );
    }

    #[test]
    fn ime_commit_replaces_preedit_and_places_caret_after() {
        let (mut state, mut fonts) = test_editor();
        let style = style();
        state.sync("abc", &style);
        set_caret(&mut state, &mut fonts, 1);
        handle_preedit(&mut state, &mut fonts, "ni", Some((2, 2)));
        let outcome = handle_commit(&mut state, &mut fonts, "你");
        assert!(outcome.changed, "提交应改变文本");
        assert_eq!(state.editor.text().to_string(), "a你bc");
        assert!(state.editor.raw_compose().is_none(), "提交后组合应结束");
        let range = state.editor.raw_selection().text_range();
        let expected = 1 + "你".len();
        assert_eq!(
            (range.start, range.end),
            (expected, expected),
            "光标应落在提交文本之后"
        );
    }

    #[test]
    fn ime_commit_without_preedit_inserts_at_caret() {
        let (mut state, mut fonts) = test_editor();
        let style = style();
        state.sync("abc", &style);
        set_caret(&mut state, &mut fonts, 1);
        let outcome = handle_commit(&mut state, &mut fonts, "X");
        assert!(outcome.changed);
        assert_eq!(state.editor.text().to_string(), "aXbc");
        let range = state.editor.raw_selection().text_range();
        assert_eq!((range.start, range.end), (2, 2));
    }

    /// 组合期间漏进来的 Text 事件（拼音字母）必须被忽略：
    /// 否则 app 的 value 会被写入拼音，光标按字符数前移、提交位置错乱。
    #[test]
    fn text_events_while_composing_are_ignored() {
        let (mut state, mut fonts) = test_editor();
        let style = style();
        state.sync("abc", &style);
        set_caret(&mut state, &mut fonts, 1);
        handle_preedit(&mut state, &mut fonts, "ni", Some((2, 2)));
        let before = state.editor.raw_selection().text_range();
        for letter in ["n", "i"] {
            let outcome = handle_text(&mut state, &mut fonts, letter);
            assert!(!outcome.changed, "组合期间拼音 Text 事件不应算作已提交文本改动");
        }
        assert_eq!(state.editor.text().to_string(), "abc", "已提交文本不能被拼音污染");
        assert!(state.editor.raw_compose().is_some(), "组合应仍在进行");
        let after = state.editor.raw_selection().text_range();
        assert_eq!(
            (after.start, after.end),
            (before.start, before.end),
            "组合光标不应跳动"
        );
        // 提交后中文应插在原来的 caret 位置。
        handle_commit(&mut state, &mut fonts, "你");
        assert_eq!(state.editor.text().to_string(), "a你bc");
    }

    /// 组合期间 Left/Right 移动的是预编辑光标，不是已提交文本的 selection。
    #[test]
    fn arrows_move_compose_cursor_not_committed_selection() {
        let (mut state, mut fonts) = test_editor();
        let style = style();
        state.sync("abc", &style);
        set_caret(&mut state, &mut fonts, 1);
        handle_preedit(&mut state, &mut fonts, "nihao", Some((5, 5)));
        let mut clip = MemoryClipboard::default();

        // 组合光标初始在预编辑末尾（绝对 1+5=6）。
        assert_eq!(state.editor.raw_selection().text_range().start, 6);
        let left = KeyInput::plain(Key::ArrowLeft);
        let _ = handle_key(&mut state, &mut fonts, &mut clip, &left);
        assert_eq!(
            state.editor.raw_selection().text_range().start,
            5,
            "Left 应把组合光标左移一格"
        );
        let _ = handle_key(&mut state, &mut fonts, &mut clip, &left);
        assert_eq!(state.editor.raw_selection().text_range().start, 4);
        let right = KeyInput::plain(Key::ArrowRight);
        let _ = handle_key(&mut state, &mut fonts, &mut clip, &right);
        assert_eq!(state.editor.raw_selection().text_range().start, 5);
        // 已提交文本保持不变、组合仍在。
        assert_eq!(state.editor.text().to_string(), "abc");
        assert!(state.editor.raw_compose().is_some());
        // 提交仍落在原 caret 位置。
        handle_commit(&mut state, &mut fonts, "你");
        assert_eq!(state.editor.text().to_string(), "a你bc");
    }

    /// 组合期间的其它按键（退格/删除/回车/字符键）只消费、不改已提交文本。
    #[test]
    fn keys_while_composing_do_not_edit_committed_text() {
        let (mut state, mut fonts) = test_editor();
        let style = style();
        state.sync("abc", &style);
        set_caret(&mut state, &mut fonts, 1);
        handle_preedit(&mut state, &mut fonts, "ni", Some((2, 2)));
        let mut clip = MemoryClipboard::default();
        for key in [
            Key::Backspace,
            Key::Delete,
            Key::Enter,
            Key::Escape,
            Key::Char('x'),
        ] {
            let _ = handle_key(&mut state, &mut fonts, &mut clip, &KeyInput::plain(key));
            assert_eq!(
                state.editor.text().to_string(),
                "abc",
                "{key:?} 不应改动已提交文本"
            );
            assert!(state.editor.raw_compose().is_some(), "{key:?} 不应结束组合");
        }
    }
}
