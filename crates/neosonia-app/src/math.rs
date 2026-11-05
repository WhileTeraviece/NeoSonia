//! LaTeX 子集渲染：把模型输出里的 `$...$` / `$$...$$` 变成 fenestra 元素。
//!
//! 目标不是完整排版，而是聊天里常见的中学/大学公式：分数、上下标、根号、求和/连乘/积分、
//! 希腊字母与常用运算符、`\text{}`、`\left(\right)`，以及多行环境
//! `aligned` / `cases` / `matrix` / `pmatrix` / `bmatrix` / `vmatrix` / `array` / `gathered`。
//!
//! # 两条硬约束（来自真机事故）
//!
//! 1. **不能输出原始 LaTeX 源码**：以前多行环境直接 `.mono()` 显示 `\begin{matrix}`，用户看到的是源码。
//!    现在所有环境都会解析成多行可读文本（或对齐网格）。
//! 2. **不能出现缺字形方框**：app 的 windowed 字体是内嵌 Inter + 系统回退。实测内嵌 Inter
//!    缺少 `∈ ∉ ⊂ ⊃ ∪ ∩ ∇ ℝ ℕ ℤ ℚ ℂ ℒ ⋯ ⌊ ⌋ ⟨ ⟩ ∀ ∃` 等字符；本版本 fenestra 的缺字回退
//!    在布局阶段有递归爆栈风险（见 `docs/verification.md`）。因此数学符号统一走 [`glyphs`]：
//!    Inter 一定有的字符保留 Unicode，Inter 缺的字符替换成 ASCII 可读写法（`∈`→` in `、`ℝ`→`R`），
//!    保证永远不会出现方框。设置 `NEOSONIA_MATH_UNICODE=1` 可以强制保留完整 Unicode
//!    （等 fenestra 回退稳定后可用）。
//!
//! 另外：分数堆叠 / 上下标小字这类"嵌套小元素"在窄栏 + 滚动容器里会触发布局递归爆栈
//! （`docs/verification.md` 已记录），所以这里坚持**平面文本**方案：公式压成一行清晰可读的
//! Unicode 数学文本；多行环境（aligned/cases/matrix）用多行文本元素/对齐网格表达。
//! 这与契约第 6 节"至少把 `$...$` 原样以等宽显示并加公式样式"一致，但观感更好、更稳。
//!
//! 三个公开 API（`docs/contracts-phase2.md` 第 6 节）：
//! - [`split`]：把一段 markdown 文本切成 普通文本 / 行内公式 / 块级公式；
//! - [`inline`]：渲染一个行内公式元素；
//! - [`block`]：渲染一个居中、带底色的块级公式元素。

use fenestra::prelude::*;

use crate::state::Msg;
use crate::theme::{space, R_CONTROL, UiColors};

// ============================================================
// 公开类型
// ============================================================

/// 一段文本里的公式片段。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MathSegment {
    Text(String),
    Inline(String),
    Block(String),
}

// ============================================================
// 字体安全层
// ============================================================

/// 是否允许使用系统字体回退才能显示的 Unicode 数学符号。
///
/// 默认关闭：windowed 下虽然有系统回退，但本版本 fenestra 的缺字回退在 layout 阶段
/// 有递归爆栈记录（`docs/verification.md`），为保证聊天界面绝不崩、绝不出现方框，
/// 默认把 Inter 缺的符号替换成 ASCII。等回退修好后可以 `NEOSONIA_MATH_UNICODE=1` 打开。
fn unicode_preferred() -> bool {
    std::env::var("NEOSONIA_MATH_UNICODE")
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// 内嵌 Inter 实测覆盖的上标字符（`ᵀ` 实测缺失，故不在此表）。
const SUP_CHARS: &str = "⁰¹²³⁴⁵⁶⁷⁸⁹⁺⁻⁼⁽⁾ⁿⁱˣᵃᵇᶜᵈᵉᶠᵍʰʲᵏˡᵐᵒᵖʳˢᵗᵘᵛʷʸᶻ";

/// 内嵌 Inter 实测覆盖的下标字符。
const SUB_CHARS: &str = "₀₁₂₃₄₅₆₇₈₉₊₋₌₍₎ₐₑₕᵢⱼₖₗₘₙₒₚᵣₛₜᵤᵥₓ";

/// 内嵌 Inter 一定有的数学符号（实测：这些字符不需要系统回退）。
fn inter_has(ch: char) -> bool {
    if ch.is_ascii() {
        return true;
    }
    // 希腊字母（Inter 覆盖完整基础希腊区）。
    if ('\u{0370}'..='\u{03FF}').contains(&ch) {
        return true;
    }
    if SUP_CHARS.contains(ch) || SUB_CHARS.contains(ch) {
        return true;
    }
    matches!(
        ch,
        '∞' | '∂'
            | '∑'
            | '∏'
            | '√'
            | '≈'
            | '≠'
            | '≤'
            | '≥'
            | '±'
            | '×'
            | '÷'
            | '·'
            | '°'
            | '′'
            | '″'
            | '∫'
            | '→'
            | '←'
            | '↔'
            | '⇒'
            | '⇐'
            | '⇔'
            | '¬'
            | '∓'
            | '…'
    )
}

/// 中文 / 全角标点（走系统 CJK 回退，windowed 渲染正常）。
fn is_cjk(ch: char) -> bool {
    matches!(ch,
        '\u{3000}'..='\u{303F}' | '\u{3400}'..='\u{4DBF}' | '\u{4E00}'..='\u{9FFF}'
        | '\u{F900}'..='\u{FAFF}' | '\u{FF00}'..='\u{FFEF}'
    )
}

/// Inter 缺失字符的 ASCII 可读替代（保证不会出现方框）。
fn ascii_fallback(ch: char) -> Option<&'static str> {
    Some(match ch {
        // 集合 / 逻辑 / 微积分
        '∇' => "grad",
        '∈' => " in ",
        '∉' => " notin ",
        '⊂' => " subset ",
        '⊆' => " subseteq ",
        '⊃' => " superset ",
        '⊇' => " superseteq ",
        '∪' => " union ",
        '∩' => " cap ",
        '∖' => " minus ",
        '∅' => " empty ",
        '∀' => "forall ",
        '∃' => "exists ",
        '∧' => " and ",
        '∨' => " or ",
        '∼' => "~",
        '∝' => "~",
        '≡' => "==",
        '≅' => "~=",
        '∥' => "||",
        '‖' => "||",
        '⊥' => "_|_",
        '∠' => "angle ",
        '∴' => "therefore ",
        '∵' => "because ",
        '∛' => "cbrt",
        '∜' => "root4",
        // 黑板体 / 花体字母（Inter 缺失；渲染时会配合粗体/斜体做视觉区分）
        'ℝ' => "R",
        'ℕ' => "N",
        'ℤ' => "Z",
        'ℚ' => "Q",
        'ℂ' => "C",
        'ℍ' => "H",
        'ℙ' => "P",
        'ℒ' => "L",
        'ℳ' => "M",
        'ℬ' => "B",
        'ℰ' => "E",
        'ℱ' => "F",
        'ℋ' => "H",
        'ℐ' => "I",
        'ℛ' => "R",
        // 括号 / 省略号 / 上标 T
        '⌊' => "[",
        '⌋' => "]",
        '⌈' => "[",
        '⌉' => "]",
        '⟨' => "<",
        '⟩' => ">",
        '⋯' => "...",
        '⋮' => ":",
        '⋱' => "...",
        'ᵀ' => "T",
        _ => return None,
    })
}

/// 零宽字符：`chat.rs` 的 `soft_wrap_breaks` 会往超长无空格串里插 U+200B 帮换行，
/// 但插进 LaTeX 命令中间会把 `\frac` 拆成 `\fr` + `ac`（渲染成 `fr?ac`）。
/// 数学解析前必须全部剔除。
fn is_zero_width(c: char) -> bool {
    matches!(
        c,
        '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{2060}' | '\u{FEFF}' | '\u{00AD}'
    )
}

fn strip_zero_width(s: &str) -> String {
    s.chars().filter(|c| !is_zero_width(*c)).collect()
}

/// 把一个字符转成"字体安全"的显示形态：
/// - ASCII / 内嵌 Inter 覆盖的字符：原样；
/// - `NEOSONIA_MATH_UNICODE=1`：原样（依赖系统回退）；
/// - 其它：ASCII 可读替代；再不行用 `?`（宁可有损也不出方框）。
fn glyph(ch: char) -> String {
    if is_zero_width(ch) {
        // 零宽格式字符直接丢弃，绝不能变成 `?`
        return String::new();
    }
    if ch.is_ascii() || inter_has(ch) {
        return ch.to_string();
    }
    if unicode_preferred() {
        return ch.to_string();
    }
    if let Some(s) = ascii_fallback(ch) {
        return s.to_string();
    }
    if is_cjk(ch) {
        return ch.to_string();
    }
    "?".to_string()
}

/// 整串字符过一遍字体安全层（公式里的符号/字母/数字走这里）。
fn glyphs(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        // 控制字符直接丢掉，避免换行/制表符把公式打乱
        if ch.is_control() {
            continue;
        }
        out.push_str(&glyph(ch));
    }
    out
}

/// `\text{}` 这类正文内容：只去掉控制字符（中文/标点原样保留，走系统 CJK 回退）。
fn plain_text(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// 粗略文本宽度（用于矩阵/对齐网格的列宽）：CJK 记 2，其它记 1。
fn text_units(s: &str) -> f32 {
    let mut units = 0.0f32;
    for ch in s.chars() {
        units += if is_cjk(ch) { 2.0 } else { 1.0 };
    }
    units
}

fn approx_width(s: &str, size: TextSize) -> f32 {
    (text_units(s) * size.px() * 0.58 + 8.0).clamp(12.0, 340.0)
}

/// 不设上限的宽度估算（长公式判断是否需要横向滚动用）。
fn est_width(s: &str, size: TextSize) -> f32 {
    text_units(s) * size.px() * 0.58 + 8.0
}

/// 块级公式卡片内部可用的逻辑宽度（面板 424 - 卡片左右内边距 12*2 - 边框）。
const BLOCK_FIT_UNITS: f32 = 34.0;

/// `NEOSONIA_MATH_WRAP=1`：长公式强制按宽度折行，不启用横向滚动（排障/兜底用）。
fn wrap_mode() -> bool {
    std::env::var("NEOSONIA_MATH_WRAP")
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// 一行文本放进块级公式卡片会不会超出可用宽度（超了就走横向滚动）。
fn needs_hscroll(text: &str) -> bool {
    !wrap_mode() && text_units(text) > BLOCK_FIT_UNITS
}

/// FNV-1a：给可横向滚动的公式生成稳定 id（不同公式各自保持滚动位置）。
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// 把超宽内容放进横向滚动容器：不撑破窗口，用户可横向滚动/拖滚动条。
///
/// `min_height`：显式给容器一个最小高度，避免滚动容器在"内容比容器宽"时
/// 高度被算成 0，导致后面的正文和公式卡片叠在一起。
fn h_scroll(element: Element<Msg>, width: f32, min_height: f32, seed: &str) -> Element<Msg> {
    let id = format!("math-hscroll-{:016x}", fnv1a(seed.as_bytes()));
    col()
        .w_full()
        .min_w(0.0)
        .min_h(min_height.max(16.0))
        .scroll_x()
        .id(&id)
        .children((col().w(width.max(120.0)).shrink0().children((element,)),))
}

// ============================================================
// 表达式（内部）
// ============================================================

/// 行内/块级的字体样式（`\mathbf` / `\mathbb` / `\mathcal` 等）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RunStyle {
    bold: bool,
    italic: bool,
}

impl RunStyle {
    fn plain() -> Self {
        Self { bold: false, italic: false }
    }
    fn bold() -> Self {
        Self { bold: true, italic: false }
    }
    fn italic() -> Self {
        Self { bold: false, italic: true }
    }
}

/// 多行环境的种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EnvKind {
    /// `aligned` / `align` / `split`：按 `&` 分列、列交替右/左对齐。
    Aligned,
    /// `cases` / `dcases`：左边一个大括号 + “表达式, 条件”两列。
    Cases,
    /// `matrix` / `array`：无括号网格。
    Grid,
    /// `pmatrix`：圆括号网格。
    Paren,
    /// `bmatrix`：方括号网格。
    Bracket,
    /// `Bmatrix`：花括号网格。
    Brace,
    /// `vmatrix` / `Vmatrix`：竖线网格。
    Bar,
    /// `gathered` / `equation` 等：单列居中多行。
    Gathered,
}

impl EnvKind {
    fn from_name(name: &str) -> Option<Self> {
        let base = name.trim().trim_end_matches('*');
        Some(match base {
            "aligned" | "align" | "alignat" | "split" | "flalign" => EnvKind::Aligned,
            "cases" | "dcases" | "rcases" => EnvKind::Cases,
            "matrix" | "array" => EnvKind::Grid,
            "pmatrix" => EnvKind::Paren,
            "bmatrix" => EnvKind::Bracket,
            "Bmatrix" => EnvKind::Brace,
            "vmatrix" | "Vmatrix" => EnvKind::Bar,
            "gathered" | "gather" | "equation" | "multline" | "subequations" => EnvKind::Gathered,
            _ => return None,
        })
    }

    fn needs_column_spec(self) -> bool {
        // `\begin{array}{cc}` 在环境名后多一个列格式参数，需要跳过。
        matches!(self, EnvKind::Grid)
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Expr {
    /// 顺序拼接。
    Row(Vec<Expr>),
    /// 数学符号/字母/数字（按字体安全层输出）。
    Sym(String),
    /// `\text{}` 里的普通文本（中文原样）。
    Text(String),
    /// 带样式的片段（`\mathbf` 等）。
    Styled { style: RunStyle, inner: Box<Expr> },
    Frac(Box<Expr>, Box<Expr>),
    /// `\sqrt[n]{...}`（`index` 为 `None` 表示平方根）。
    Sqrt { index: Option<Box<Expr>>, body: Box<Expr> },
    Script {
        base: Box<Expr>,
        sub: Option<Box<Expr>>,
        sup: Option<Box<Expr>>,
    },
    /// 多行环境：`rows` 是「行 -> 单元格」。
    Env { kind: EnvKind, rows: Vec<Vec<Expr>> },
}

/// 希腊字母与常用运算符的 Unicode 映射。
///
/// 注意：这里返回的是"理想 Unicode"，最终显示还要过 [`glyphs`]（Inter 缺的会转 ASCII）。
fn symbol(name: &str) -> Option<&'static str> {
    Some(match name {
        // 小写希腊字母
        "alpha" => "α",
        "beta" => "β",
        "gamma" => "γ",
        "delta" => "δ",
        "epsilon" => "ε",
        "varepsilon" => "ϵ",
        "zeta" => "ζ",
        "eta" => "η",
        "theta" => "θ",
        "vartheta" => "ϑ",
        "iota" => "ι",
        "kappa" => "κ",
        "lambda" => "λ",
        "mu" => "μ",
        "nu" => "ν",
        "xi" => "ξ",
        "pi" => "π",
        "varpi" => "ϖ",
        "rho" => "ρ",
        "varrho" => "ϱ",
        "sigma" => "σ",
        "varsigma" => "ς",
        "tau" => "τ",
        "upsilon" => "υ",
        "phi" => "φ",
        "varphi" => "ϕ",
        "chi" => "χ",
        "psi" => "ψ",
        "omega" => "ω",
        // 大写希腊字母
        "Gamma" => "Γ",
        "Delta" => "Δ",
        "Theta" => "Θ",
        "Lambda" => "Λ",
        "Xi" => "Ξ",
        "Pi" => "Π",
        "Sigma" => "Σ",
        "Upsilon" => "Υ",
        "Phi" => "Φ",
        "Psi" => "Ψ",
        "Omega" => "Ω",
        // 不常用的大写希腊字母也补全（Inter 覆盖希腊区，无需退化）
        "Alpha" => "Α",
        "Beta" => "Β",
        "Epsilon" => "Ε",
        "Zeta" => "Ζ",
        "Eta" => "Η",
        "Iota" => "Ι",
        "Kappa" => "Κ",
        "Mu" => "Μ",
        "Nu" => "Ν",
        "Omicron" => "Ο",
        "Rho" => "Ρ",
        "Tau" => "Τ",
        "Chi" => "Χ",
        "varkappa" => "ϰ",
        "digamma" => "ϝ",
        // 运算符
        "times" => "×",
        "cdot" => "·",
        "div" => "÷",
        "pm" => "±",
        "mp" => "∓",
        "le" | "leq" => "≤",
        "ge" | "geq" => "≥",
        "ne" | "neq" => "≠",
        "approx" => "≈",
        "equiv" => "≡",
        "sim" => "∼",
        "simeq" => "≅",
        "cong" => "≅",
        "propto" => "∝",
        "infty" => "∞",
        "to" | "rightarrow" => "→",
        "leftarrow" => "←",
        "leftrightarrow" => "↔",
        "Rightarrow" | "implies" => "⇒",
        "Leftarrow" => "⇐",
        "Leftrightarrow" | "iff" => "⇔",
        "mapsto" => "→",
        "uparrow" => "↑",
        "downarrow" => "↓",
        "partial" => "∂",
        "nabla" => "∇",
        "in" => "∈",
        "notin" => "∉",
        "subset" => "⊂",
        "subseteq" => "⊆",
        "supset" => "⊃",
        "supseteq" => "⊇",
        "cup" | "bigcup" => "∪",
        "cap" | "bigcap" => "∩",
        "setminus" => "∖",
        "emptyset" | "varnothing" => "∅",
        "forall" => "∀",
        "exists" => "∃",
        "neg" | "lnot" => "¬",
        "land" | "wedge" => "∧",
        "lor" | "vee" => "∨",
        "cdots" => "⋯",
        "ldots" | "dots" => "…",
        "vdots" => "⋮",
        "ddots" => "⋱",
        "prime" => "′",
        "dprime" => "″",
        "circ" => "°",
        "degree" => "°",
        "angle" => "∠",
        "perp" => "⊥",
        "parallel" => "∥",
        "therefore" => "∴",
        "because" => "∵",
        "sum" => "∑",
        "prod" => "∏",
        "int" => "∫",
        "iint" => "∬",
        "iiint" => "∭",
        "oint" => "∮",
        "sqrt" => "√",
        "cbrt" => "∛",
        "lfloor" => "⌊",
        "rfloor" => "⌋",
        "lceil" => "⌈",
        "rceil" => "⌉",
        "langle" => "⟨",
        "rangle" => "⟩",
        "vert" | "mid" => "|",
        "Vert" => "‖",
        "lbrace" => "{",
        "rbrace" => "}",
        _ => return None,
    })
}

/// 常见"函数名"命令：输出正体英文名（不倾斜，符合数学排版习惯）。
fn function_name(name: &str) -> Option<&'static str> {
    Some(match name {
        "sin" => "sin",
        "cos" => "cos",
        "tan" => "tan",
        "cot" => "cot",
        "sec" => "sec",
        "csc" => "csc",
        "arcsin" => "arcsin",
        "arccos" => "arccos",
        "arctan" => "arctan",
        "sinh" => "sinh",
        "cosh" => "cosh",
        "tanh" => "tanh",
        "log" => "log",
        "ln" => "ln",
        "lg" => "lg",
        "exp" => "exp",
        "lim" => "lim",
        "limsup" => "limsup",
        "liminf" => "liminf",
        "max" => "max",
        "min" => "min",
        "sup" => "sup",
        "inf" => "inf",
        "det" => "det",
        "gcd" => "gcd",
        "deg" => "deg",
        "dim" => "dim",
        "ker" => "ker",
        "Pr" => "Pr",
        "arg" => "arg",
        "mod" => "mod",
        "bmod" => "mod",
        _ => return None,
    })
}

// ============================================================
// 解析
// ============================================================

#[derive(Clone, Copy, PartialEq, Eq)]
enum StopHit {
    /// 输入读完了。
    Eof,
    /// 读到了分组右括号（`}`）。
    CloseBrace,
    /// 多行环境里的列分隔 `&`。
    Column,
    /// 多行环境里的行分隔 `\\`。
    Row,
    /// 多行环境结束 `\end{...}`。
    EnvEnd,
}

struct Parser<'a> {
    chars: &'a [char],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn parse_into(&mut self) -> Expr {
        let (items, _) = self.items(false, None);
        Expr::Row(items)
    }

    /// 读取一串表达式，直到遇到停止条件。
    ///
    /// `stop_group`：遇到 `}` 停（解析 `{...}` 分组用）；
    /// `env`：多行环境名，遇到 `&` / `\\` / `\end{env}` 停。
    fn items(&mut self, stop_group: bool, env: Option<&str>) -> (Vec<Expr>, StopHit) {
        let mut out: Vec<Expr> = Vec::new();
        while self.pos < self.chars.len() {
            let ch = self.chars[self.pos];
            match ch {
                '}' if stop_group => {
                    self.pos += 1;
                    return (out, StopHit::CloseBrace);
                }
                '&' if env.is_some() => {
                    self.pos += 1;
                    return (out, StopHit::Column);
                }
                '\\' => {
                    // 环境里的 `\\`：换行
                    if env.is_some()
                        && self.pos + 1 < self.chars.len()
                        && self.chars[self.pos + 1] == '\\'
                    {
                        self.pos += 2;
                        self.skip_row_spacing();
                        return (out, StopHit::Row);
                    }
                    // 环境里的 `\end{env}`：结束当前环境
                    if let Some(env_name) = env {
                        if self.consume_env_end(env_name) {
                            return (out, StopHit::EnvEnd);
                        }
                    }
                    self.pos += 1;
                    if let Some(e) = self.command() {
                        out.push(self.attach_scripts(e));
                    }
                }
                '{' => {
                    self.pos += 1;
                    let (inner, _) = self.items(true, None);
                    out.push(self.attach_scripts(Expr::Row(inner)));
                }
                '^' | '_' => {
                    let is_sup = ch == '^';
                    self.pos += 1;
                    let base = match out.pop() {
                        Some(e) => trim_trailing_space(e),
                        None => Expr::Sym(String::new()),
                    };
                    let arg = self.arg();
                    out.push(merge_script(base, is_sup, arg));
                }
                '}' => {
                    // 多余的右括号：当普通字符处理，避免吞掉后续内容。
                    self.pos += 1;
                    out.push(Expr::Sym("}".to_string()));
                }
                _ => {
                    let start = self.pos;
                    while self.pos < self.chars.len() {
                        let c = self.chars[self.pos];
                        let special = matches!(c, '\\' | '{' | '}' | '^' | '_')
                            || (c == '&' && env.is_some());
                        if special {
                            break;
                        }
                        self.pos += 1;
                    }
                    let s: String = self.chars[start..self.pos].iter().collect();
                    if !s.is_empty() {
                        out.push(Expr::Sym(s));
                    }
                }
            }
        }
        (out, StopHit::Eof)
    }

    /// 跳过 `\\[2pt]` 这种行间距可选项。
    fn skip_row_spacing(&mut self) {
        self.skip_spaces();
        if self.pos < self.chars.len() && self.chars[self.pos] == '[' {
            while self.pos < self.chars.len() && self.chars[self.pos] != ']' {
                self.pos += 1;
            }
            if self.pos < self.chars.len() {
                self.pos += 1;
            }
        }
    }

    /// 如果当前位置是 `\end{env}`，消费它并返回 `true`（环境结束）。
    ///
    /// 遇到不匹配的 `\end{其它}` 也消费（防止死循环），返回 `true`。
    fn consume_env_end(&mut self, env: &str) -> bool {
        let save = self.pos;
        if self.pos >= self.chars.len() || self.chars[self.pos] != '\\' {
            return false;
        }
        let mut p = self.pos + 1;
        let start = p;
        while p < self.chars.len() && self.chars[p].is_ascii_alphabetic() {
            p += 1;
        }
        let word: String = self.chars[start..p].iter().collect();
        if word != "end" {
            return false;
        }
        self.pos = p;
        self.skip_spaces();
        if self.pos >= self.chars.len() || self.chars[self.pos] != '{' {
            self.pos = save;
            return false;
        }
        let name = self.raw_group();
        if name.trim() == env {
            return true;
        }
        // 不匹配的 \end：也当作本环境结束，避免出现无穷循环（坏输入容错）。
        true
    }

    /// 解析反斜杠命令；调用前 `self.pos` 已越过反斜杠。
    fn command(&mut self) -> Option<Expr> {
        if self.pos >= self.chars.len() {
            return Some(Expr::Sym("\\".to_string()));
        }
        let ch = self.chars[self.pos];
        if !ch.is_ascii_alphabetic() {
            self.pos += 1;
            return Some(match ch {
                ',' | ';' | ':' | ' ' | '!' => Expr::Sym(" ".to_string()),
                '\\' => Expr::Sym(" ".to_string()),
                '%' | '$' | '#' | '&' | '_' | '{' | '}' => Expr::Sym(ch.to_string()),
                _ => Expr::Sym(ch.to_string()),
            });
        }
        let start = self.pos;
        while self.pos < self.chars.len() && self.chars[self.pos].is_ascii_alphabetic() {
            self.pos += 1;
        }
        let name: String = self.chars[start..self.pos].iter().collect();
        match name.as_str() {
            "frac" | "dfrac" | "tfrac" | "cfrac" => {
                let a = self.arg();
                let b = self.arg();
                Some(Expr::Frac(Box::new(a), Box::new(b)))
            }
            "sqrt" => {
                self.skip_spaces();
                // `\sqrt[3]{x}` 的方括号索引：直接扫到 `]` 为止（不能用 items()，
                // 否则会把后面的 `{x}` 一起吞掉）。
                let index = if self.pos < self.chars.len() && self.chars[self.pos] == '[' {
                    self.pos += 1;
                    let start = self.pos;
                    while self.pos < self.chars.len() && self.chars[self.pos] != ']' {
                        self.pos += 1;
                    }
                    let raw: String = self.chars[start..self.pos].iter().collect();
                    if self.pos < self.chars.len() {
                        self.pos += 1; // 跳过 `]`
                    }
                    let raw = raw.trim().to_string();
                    if raw.is_empty() {
                        None
                    } else {
                        Some(Box::new(parse_latex(&raw)))
                    }
                } else {
                    None
                };
                let body = self.arg();
                Some(Expr::Sqrt { index, body: Box::new(body) })
            }
            "text" | "textrm" | "textnormal" | "mbox" => Some(Expr::Text(self.raw_group())),
            "mathrm" | "mathsf" | "mathtt" | "operatorname" | "mathnormal" => {
                Some(Expr::Text(self.raw_group()))
            }
            "mathbf" | "boldsymbol" | "bm" | "mathbold" => Some(Expr::Styled {
                style: RunStyle::bold(),
                inner: Box::new(self.arg()),
            }),
            "mathit" | "mathsl" => Some(Expr::Styled {
                style: RunStyle::italic(),
                inner: Box::new(self.arg()),
            }),
            "mathbb" => Some(Self::styled_letters(RunStyle::bold(), &self.raw_group(), true)),
            "mathcal" | "mathscr" => {
                Some(Self::styled_letters(RunStyle::italic(), &self.raw_group(), false))
            }
            "mathfrak" => Some(Expr::Text(self.raw_group())),
            "left" | "right" => Some(Expr::Sym(self.delim())),
            "quad" => Some(Expr::Sym("  ".to_string())),
            "qquad" => Some(Expr::Sym("    ".to_string())),
            "begin" => {
                let env_name = self.raw_group();
                let name = env_name.trim();
                if name.is_empty() {
                    return None;
                }
                Some(self.env_body(name))
            }
            "end" => {
                // 顶层多余的 \end{...}：吞掉，不显示原始 LaTeX。
                let _ = self.raw_group();
                None
            }
            // 重音/上划线：忽略命令本身，保留内容（不引入缺字形的组合符）。
            "hat" | "bar" | "vec" | "dot" | "ddot" | "tilde" | "widehat" | "widetilde"
            | "overline" | "underline" | "overbrace" | "underbrace" | "not" => Some(self.arg()),
            other => {
                if let Some(f) = function_name(other) {
                    return Some(Expr::Text(f.to_string()));
                }
                Some(match symbol(other) {
                    Some(s) => Expr::Sym(s.to_string()),
                    // 未知命令：去掉反斜杠，按正体文字显示（绝不出原始 LaTeX）。
                    None => Expr::Text(other.to_string()),
                })
            }
        }
    }

    /// `\mathbb{R}` / `\mathcal{L}`：把字母映射成对应 Unicode 字母。
    /// 最终显示由 [`glyphs`] 决定（Inter 缺的会退化成普通字母，并用样式做视觉区分）。
    fn styled_letters(style: RunStyle, raw: &str, double_struck: bool) -> Expr {
        let mut items: Vec<Expr> = Vec::new();
        for ch in raw.chars() {
            if let Some(mapped) = if double_struck {
                double_struck_letter(ch)
            } else {
                script_letter(ch)
            } {
                items.push(Expr::Sym(mapped.to_string()));
            } else {
                // 数字/标点等保持原样
                items.push(Expr::Sym(ch.to_string()));
            }
        }
        Expr::Styled { style, inner: Box::new(Expr::Row(items)) }
    }

    /// 解析 `\begin{env}` 到 `\end{env}` 之间的内容。
    fn env_body(&mut self, name: &str) -> Expr {
        let kind = EnvKind::from_name(name);
        if kind.map(EnvKind::needs_column_spec).unwrap_or(false) {
            // `\begin{array}{cc}`：跳过列格式参数
            self.skip_spaces();
            if self.pos < self.chars.len() && self.chars[self.pos] == '{' {
                let _ = self.raw_group();
            }
        }
        let mut rows: Vec<Vec<Expr>> = Vec::new();
        let mut cells: Vec<Expr> = Vec::new();
        loop {
            let (items, hit) = self.items(false, Some(name));
            cells.push(Expr::Row(items));
            match hit {
                StopHit::Column => continue,
                StopHit::Row => {
                    rows.push(std::mem::take(&mut cells));
                }
                StopHit::EnvEnd | StopHit::Eof => {
                    rows.push(std::mem::take(&mut cells));
                    break;
                }
                StopHit::CloseBrace => break,
            }
        }
        Expr::Env { kind: kind.unwrap_or(EnvKind::Grid), rows }
    }

    fn skip_spaces(&mut self) {
        while self.pos < self.chars.len() && self.chars[self.pos].is_whitespace() {
            self.pos += 1;
        }
    }

    /// 解析一个参数：`{...}`、单个命令、或单个字符。
    fn arg(&mut self) -> Expr {
        self.skip_spaces();
        if self.pos >= self.chars.len() {
            return Expr::Row(Vec::new());
        }
        match self.chars[self.pos] {
            '{' => {
                self.pos += 1;
                let (items, _) = self.items(true, None);
                Expr::Row(items)
            }
            '\\' => {
                self.pos += 1;
                self.command().unwrap_or(Expr::Row(Vec::new()))
            }
            ']' => {
                self.pos += 1;
                Expr::Sym("]".to_string())
            }
            ch => {
                self.pos += 1;
                Expr::Sym(ch.to_string())
            }
        }
    }

    /// 花括号内容（支持嵌套），调用前 `self.pos` 指向 `{`（或非 `{` 时退化成单字符）。
    fn raw_group(&mut self) -> String {
        self.skip_spaces();
        if self.pos >= self.chars.len() {
            return String::new();
        }
        if self.chars[self.pos] != '{' {
            let ch = self.chars[self.pos];
            self.pos += 1;
            return ch.to_string();
        }
        self.pos += 1;
        let mut depth = 1usize;
        let mut s = String::new();
        while self.pos < self.chars.len() {
            let ch = self.chars[self.pos];
            self.pos += 1;
            match ch {
                '{' => {
                    depth += 1;
                    s.push(ch);
                }
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    s.push(ch);
                }
                _ => s.push(ch),
            }
        }
        s
    }

    /// `\left` / `\right` 后面的定界符。
    fn delim(&mut self) -> String {
        self.skip_spaces();
        if self.pos >= self.chars.len() {
            return String::new();
        }
        let ch = self.chars[self.pos];
        self.pos += 1;
        match ch {
            '.' => String::new(),
            '\\' => {
                let start = self.pos;
                while self.pos < self.chars.len() && self.chars[self.pos].is_ascii_alphabetic() {
                    self.pos += 1;
                }
                let name: String = self.chars[start..self.pos].iter().collect();
                if name == "{" {
                    return "{".to_string();
                }
                if name == "}" {
                    return "}".to_string();
                }
                symbol(&name).map(|s| s.to_string()).unwrap_or_default()
            }
            c => c.to_string(),
        }
    }

    /// 命令或分组之后紧跟的上下标。
    fn attach_scripts(&mut self, mut base: Expr) -> Expr {
        loop {
            let save = self.pos;
            self.skip_spaces();
            if self.pos >= self.chars.len() {
                self.pos = save;
                break;
            }
            let ch = self.chars[self.pos];
            if ch != '^' && ch != '_' {
                self.pos = save;
                break;
            }
            self.pos += 1;
            let arg = self.arg();
            base = merge_script(base, ch == '^', arg);
        }
        base
    }
}

/// `\mathbb` 字母映射（`\mathbb{R}` -> `ℝ`；`\mathbb{Z}` -> `ℤ` ...）。
fn double_struck_letter(ch: char) -> Option<char> {
    Some(match ch {
        'A' => '𝔸',
        'B' => '𝔹',
        'C' => 'ℂ',
        'D' => '𝔻',
        'E' => '𝔼',
        'F' => '𝔽',
        'G' => '𝔾',
        'H' => 'ℍ',
        'I' => '𝕀',
        'J' => '𝕁',
        'K' => '𝕂',
        'L' => '𝕃',
        'M' => '𝕄',
        'N' => 'ℕ',
        'O' => '𝕆',
        'P' => 'ℙ',
        'Q' => 'ℚ',
        'R' => 'ℝ',
        'S' => '𝕊',
        'T' => '𝕋',
        'U' => '𝕌',
        'V' => '𝕍',
        'W' => '𝕎',
        'X' => '𝕏',
        'Y' => '𝕐',
        'Z' => 'ℤ',
        _ => return None,
    })
}

/// `\mathcal` 字母映射（只映射常见大写；未映射的保持普通字母）。
fn script_letter(ch: char) -> Option<char> {
    Some(match ch {
        'B' => 'ℬ',
        'E' => 'ℰ',
        'F' => 'ℱ',
        'H' => 'ℋ',
        'I' => 'ℐ',
        'L' => 'ℒ',
        'M' => 'ℳ',
        'R' => 'ℛ',
        _ => return None,
    })
}

/// 入口：解析整段 LaTeX。
fn parse_latex(src: &str) -> Expr {
    // 防御：调用方可能已经过 chat.rs 的零宽换行处理，先剔除再解析。
    let cleaned = strip_zero_width(src);
    let chars: Vec<char> = cleaned.chars().collect();
    let mut p = Parser { chars: chars.as_slice(), pos: 0 };
    p.parse_into()
}

fn merge_script(base: Expr, is_sup: bool, arg: Expr) -> Expr {
    match base {
        Expr::Script { base: b, sub, sup } => {
            if is_sup {
                Expr::Script { base: b, sub, sup: Some(Box::new(arg)) }
            } else {
                Expr::Script { base: b, sub: Some(Box::new(arg)), sup }
            }
        }
        other => {
            if is_sup {
                Expr::Script { base: Box::new(other), sub: None, sup: Some(Box::new(arg)) }
            } else {
                Expr::Script { base: Box::new(other), sub: Some(Box::new(arg)), sup: None }
            }
        }
    }
}

fn trim_trailing_space(e: Expr) -> Expr {
    match e {
        Expr::Sym(s) => Expr::Sym(s.trim_end().to_string()),
        other => other,
    }
}

// ============================================================
// 压平为可读文本
// ============================================================

/// 子式在 `/`、`^`、`_` 旁边是否需要括号：
/// 只有含运算符/空格/逗号（可能歧义）才加，像 `xⁿ`、`n!`、`123` 保持简洁。
fn wrap_if_multi(s: &str) -> String {
    let ambiguous = s.chars().any(|c| {
        " +-−×÷·=/<>(),≤≥≠⇒⇔→←↔".contains(c)
    });
    if text_units(s) > 1.0 && ambiguous {
        format!("({s})")
    } else {
        s.to_string()
    }
}

/// 上标字符映射（只使用内嵌 Inter 一定有的字符）。
fn unicode_sup(s: &str) -> Option<String> {
    let mut out = String::new();
    for ch in s.chars() {
        let mapped = match ch {
            '0' => '⁰',
            '1' => '¹',
            '2' => '²',
            '3' => '³',
            '4' => '⁴',
            '5' => '⁵',
            '6' => '⁶',
            '7' => '⁷',
            '8' => '⁸',
            '9' => '⁹',
            '+' => '⁺',
            '-' => '⁻',
            '=' => '⁼',
            '(' => '⁽',
            ')' => '⁾',
            'n' => 'ⁿ',
            'i' => 'ⁱ',
            'x' => 'ˣ',
            'a' => 'ᵃ',
            'b' => 'ᵇ',
            'c' => 'ᶜ',
            'd' => 'ᵈ',
            'e' => 'ᵉ',
            'f' => 'ᶠ',
            'g' => 'ᵍ',
            'h' => 'ʰ',
            'j' => 'ʲ',
            'k' => 'ᵏ',
            'l' => 'ˡ',
            'm' => 'ᵐ',
            'o' => 'ᵒ',
            'p' => 'ᵖ',
            'r' => 'ʳ',
            's' => 'ˢ',
            't' => 'ᵗ',
            'u' => 'ᵘ',
            'v' => 'ᵛ',
            'w' => 'ʷ',
            'y' => 'ʸ',
            'z' => 'ᶻ',
            // 已是上标字符：保持（支持 e^{x^2} 这种嵌套）
            _ if SUP_CHARS.contains(ch) => ch,
            _ => return None,
        };
        out.push(mapped);
    }
    Some(out)
}

/// 下标字符映射。
fn unicode_sub(s: &str) -> Option<String> {
    let mut out = String::new();
    for ch in s.chars() {
        let mapped = match ch {
            '0' => '₀',
            '1' => '₁',
            '2' => '₂',
            '3' => '₃',
            '4' => '₄',
            '5' => '₅',
            '6' => '₆',
            '7' => '₇',
            '8' => '₈',
            '9' => '₉',
            '+' => '₊',
            '-' => '₋',
            '=' => '₌',
            '(' => '₍',
            ')' => '₎',
            'a' => 'ₐ',
            'e' => 'ₑ',
            'h' => 'ₕ',
            'i' => 'ᵢ',
            'j' => 'ⱼ',
            'k' => 'ₖ',
            'l' => 'ₗ',
            'm' => 'ₘ',
            'n' => 'ₙ',
            'o' => 'ₒ',
            'p' => 'ₚ',
            'r' => 'ᵣ',
            's' => 'ₛ',
            't' => 'ₜ',
            'u' => 'ᵤ',
            'v' => 'ᵥ',
            'x' => 'ₓ',
            _ if SUB_CHARS.contains(ch) => ch,
            _ => return None,
        };
        out.push(mapped);
    }
    Some(out)
}

/// 压平整个表达式（`multiline = true` 时多行环境用换行分隔）。
fn flatten_raw(expr: &Expr, multiline: bool) -> String {
    match expr {
        Expr::Row(items) => items.iter().map(|e| flatten_raw(e, multiline)).collect(),
        Expr::Sym(s) => glyphs(s),
        Expr::Text(s) => plain_text(s),
        Expr::Styled { inner, .. } => flatten_raw(inner, multiline),
        Expr::Frac(num, den) => {
            let num = flatten_raw(num, false);
            let den = flatten_raw(den, false);
            format!("{}/{}", wrap_if_multi(&num), wrap_if_multi(&den))
        }
        Expr::Sqrt { index, body } => {
            let body = flatten_raw(body, false);
            match index {
                Some(idx) => {
                    let idx = flatten_raw(idx, false);
                    format!("√[{idx}]({body})")
                }
                None => format!("√({body})"),
            }
        }
        Expr::Script { base, sub, sup } => {
            let mut out = flatten_raw(base, false);
            if let Some(s) = sub {
                let inner = flatten_raw(s, false);
                match unicode_sub(&inner) {
                    Some(u) => out.push_str(&u),
                    None => {
                        out.push('_');
                        out.push_str(&wrap_if_multi(&inner));
                    }
                }
            }
            if let Some(s) = sup {
                let inner = flatten_raw(s, false);
                match unicode_sup(&inner) {
                    Some(u) => out.push_str(&u),
                    None => {
                        out.push('^');
                        out.push_str(&wrap_if_multi(&inner));
                    }
                }
            }
            out
        }
        Expr::Env { kind, rows } => flatten_env(*kind, rows, multiline),
    }
}

/// 多行环境压平：行内用 `; ` 连接，块级用换行。
fn flatten_env(kind: EnvKind, rows: &[Vec<Expr>], multiline: bool) -> String {
    let sep = if multiline { "\n" } else { "; " };
    let lines: Vec<String> = rows
        .iter()
        .map(|cells| {
            // 每个单元格先 trim：LaTeX 源码里 `&` 两边的空格不应进入显示文本。
            let parts: Vec<String> = cells
                .iter()
                .map(|c| flatten_raw(c, false).trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            match kind {
                EnvKind::Cases => {
                    let expr = parts.first().cloned().unwrap_or_default();
                    let cond = parts.get(1).cloned().unwrap_or_default();
                    if cond.is_empty() {
                        expr
                    } else if expr.ends_with(',') || expr.ends_with('，') {
                        // 源码常见写法 `x, & x>0`：表达式自带逗号，不要再补一个。
                        format!("{expr} {cond}")
                    } else {
                        format!("{expr}, {cond}")
                    }
                }
                _ => parts.join(" "),
            }
        })
        .collect();
    let body = lines.join(sep);
    match kind {
        EnvKind::Paren => format!("({body})"),
        EnvKind::Bracket => format!("[{body}]"),
        EnvKind::Brace => format!("{{{body}}}"),
        EnvKind::Bar => format!("|{body}|"),
        // 无括号矩阵也加方括号，单行文本里更容易读。
        EnvKind::Grid => {
            if multiline {
                body
            } else {
                format!("[{body}]")
            }
        }
        _ => body,
    }
}

/// 压平 + 规整空白（公式里大量空格/`\,` 会留下连续空格）。
fn flatten_text(expr: &Expr, multiline: bool) -> String {
    let raw = flatten_raw(expr, multiline);
    tidy(&raw)
}

fn tidy(s: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    for line in s.split('\n') {
        let mut out = String::new();
        let mut last_space = false;
        for ch in line.chars() {
            if ch == ' ' {
                if last_space {
                    continue;
                }
                last_space = true;
                out.push(' ');
            } else {
                last_space = false;
                out.push(ch);
            }
        }
        lines.push(out.trim().to_string());
    }
    lines.join("\n").trim().to_string()
}

// ============================================================
// 渲染
// ============================================================

/// 行内的一个文本 run（保留 `\mathbf` 的粗体等样式）。
fn collect_runs(expr: &Expr, out: &mut Vec<(String, RunStyle)>) {
    match expr {
        Expr::Row(items) => {
            for item in items {
                collect_runs(item, out);
            }
        }
        Expr::Sym(s) => out.push((glyphs(s), RunStyle::plain())),
        Expr::Text(s) => out.push((plain_text(s), RunStyle::plain())),
        Expr::Styled { style, inner } => {
            // 只保留一层样式：内层再嵌套样式时按外层处理（少见，够用）。
            let text = flatten_text(inner, false);
            out.push((text, *style));
        }
        other => out.push((flatten_text(other, false), RunStyle::plain())),
    }
}

fn styled_text(s: String, size: TextSize, c: &UiColors, style: RunStyle) -> Element<Msg> {
    let mut t = text(s).size(size).color(c.text);
    // fenestra 的 Element 只有三档字重（Regular/Medium/Semibold），没有 italic API；
    // 所以 \mathbf 用 Semibold，\mathcal 这类"斜体"退化成 Medium 做轻微视觉区分。
    if style.bold {
        t = t.weight(Weight::Semibold);
    } else if style.italic {
        t = t.weight(Weight::Medium);
    }
    t.into()
}

fn inline_text(s: String, size: TextSize, c: &UiColors) -> Element<Msg> {
    text(s).size(size).color(c.text).leading(1.5).into()
}

/// 块级卡片内部可用宽度（逻辑像素）：面板 424 - 卡片左右内边距 12*2 - 边框余量。
const BLOCK_AVAILABLE_WIDTH: f32 = 396.0;

/// 内容宽度超过卡片可用宽度时，块级公式改用横向滚动（不撑破窗口）。
fn needs_hscroll_width(width: f32) -> bool {
    !wrap_mode() && width > BLOCK_AVAILABLE_WIDTH
}

fn block_text(s: String, c: &UiColors) -> Element<Msg> {
    if s.is_empty() {
        return col().h(0.0).into();
    }
    // 环境嵌在中间等情况下会带换行：按行居中显示。
    if s.contains('\n') {
        let lines: Vec<Element<Msg>> = s
            .lines()
            .map(|line| {
                row()
                    .justify_center()
                    .w_full()
                    .children((text(line.to_string()).size(TextSize::Lg).color(c.text),))
                    .into()
            })
            .collect();
        return col().w_full().items_center().gap(3.0).children(lines).into();
    }
    // 长公式：放进横向滚动容器（fenestra 的 scroll_x），可拖滚动条/横向滚动。
    if needs_hscroll(&s) {
        let size = TextSize::Lg;
        let width = est_width(&s, size) * 1.15 + 12.0;
        let line = row()
            .w(width)
            .items_center()
            .children((text(s.clone()).size(size).color(c.text),));
        let line_height = size.px() * 1.6;
        return h_scroll(line.into(), width, line_height, &s);
    }
    text(s)
        .size(TextSize::Lg)
        .color(c.text)
        .leading(1.5)
        .text_align(TextAlign::Center)
        .w_full()
        .into()
}

fn render_env_block(kind: EnvKind, rows: &[Vec<Expr>], c: &UiColors, seed: &str) -> Element<Msg> {
    let (element, width) = render_env(kind, rows, c);
    if needs_hscroll_width(width) {
        // 多行环境：按行数估一个最小高度，避免滚动容器高度算成 0 导致叠字。
        let row_h = TextSize::Base.px() * 1.6;
        let min_height = rows.len() as f32 * row_h + 4.0 * rows.len().saturating_sub(1) as f32 + 8.0;
        h_scroll(element, width, min_height, seed)
    } else {
        col().w_full().items_center().children((element,)).into()
    }
}

/// 渲染多行环境：
/// - `Aligned`：每行按列拆开，偶数列右对齐、奇数列左对齐（LaTeX 的 `&` 语义）；
/// - `Cases`：左边一个大括号 + “表达式 / 条件”两列；
/// - 矩阵：行列对齐的网格 + 对应括号；
/// - `Gathered`：单列居中。
fn render_env(kind: EnvKind, rows: &[Vec<Expr>], c: &UiColors) -> (Element<Msg>, f32) {
    if rows.is_empty() {
        return (col().h(0.0).into(), 0.0);
    }
    // 每行压平成单元格文本
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|row_cells| row_cells.iter().map(|cell| flatten_text(cell, false)).collect())
        .collect();
    let ncols = cells.iter().map(|r| r.len()).max().unwrap_or(1).max(1);
    let mut widths = vec![0f32; ncols];
    let size = TextSize::Base;
    for row_cells in &cells {
        for (j, cell) in row_cells.iter().enumerate() {
            widths[j] = widths[j].max(approx_width(cell, size));
        }
    }
    let cols_width: f32 = widths.iter().sum();
    let gap_aligned = 6.0f32;
    let gap_matrix = 10.0f32;
    match kind {
        EnvKind::Aligned => {
            let width = cols_width + gap_aligned * (ncols.saturating_sub(1)) as f32;
            let mut lines: Vec<Element<Msg>> = Vec::new();
            for row_cells in &cells {
                let mut kids: Vec<Element<Msg>> = Vec::new();
                for (j, cell) in row_cells.iter().enumerate() {
                    let align = if j % 2 == 0 { TextAlign::End } else { TextAlign::Start };
                    kids.push(
                        text(cell.clone())
                            .size(size)
                            .color(c.text)
                            .w(widths[j])
                            .text_align(align)
                            .into(),
                    );
                }
                lines.push(
                    row()
                        .items_center()
                        .justify_center()
                        .gap(6.0)
                        .children(kids)
                        .into(),
                );
            }
            (col().w(width).items_center().gap(4.0).children(lines).into(), width)
        }
        EnvKind::Cases => {
            let width = widths.first().copied().unwrap_or(0.0)
                + widths.get(1).copied().unwrap_or(0.0)
                + 22.0;
            let brace = text("{").size(TextSize::Xl2).color(c.text_muted);
            let mut lines: Vec<Element<Msg>> = Vec::new();
            for row_cells in &cells {
                let a = row_cells.first().cloned().unwrap_or_default();
                let b = row_cells.get(1).cloned().unwrap_or_default();
                let mut kids: Vec<Element<Msg>> = Vec::new();
                kids.push(
                    text(a)
                        .size(size)
                        .color(c.text)
                        .w(widths[0])
                        .text_align(TextAlign::End)
                        .into(),
                );
                if !b.is_empty() {
                    kids.push(text(b).size(size).color(c.text_muted).into());
                }
                lines.push(
                    row()
                        .items_center()
                        .justify_center()
                        .gap(8.0)
                        .children(kids)
                        .into(),
                );
            }
            (
                row()
                    .items_center()
                    .justify_center()
                    .gap(2.0)
                    .children((brace, col().items_center().gap(4.0).children(lines)))
                    .into(),
                width,
            )
        }
        EnvKind::Gathered => {
            let width = widths.iter().copied().fold(0.0f32, f32::max);
            let lines: Vec<Element<Msg>> = cells
                .iter()
                .map(|row_cells| {
                    let line = row_cells.join("  ");
                    row().justify_center().w_full().children((text(line).size(size).color(c.text),)).into()
                })
                .collect();
            (col().w(width).items_center().gap(4.0).children(lines).into(), width)
        }
        _ => {
            // 矩阵类：左右定界符 + 网格
            let has_delim = !matches!(kind, EnvKind::Grid);
            let width = cols_width
                + gap_matrix * (ncols.saturating_sub(1)) as f32
                + if has_delim { 34.0 } else { 4.0 };
            let open = match kind {
                EnvKind::Paren => "(",
                EnvKind::Bracket | EnvKind::Grid => "[",
                EnvKind::Brace => "{",
                EnvKind::Bar => "|",
                _ => "",
            };
            let close = match kind {
                EnvKind::Paren => ")",
                EnvKind::Bracket | EnvKind::Grid => "]",
                EnvKind::Brace => "}",
                EnvKind::Bar => "|",
                _ => "",
            };
            let mut lines: Vec<Element<Msg>> = Vec::new();
            for row_cells in &cells {
                let mut kids: Vec<Element<Msg>> = Vec::new();
                for (j, cell) in row_cells.iter().enumerate() {
                    kids.push(
                        text(cell.clone())
                            .size(size)
                            .color(c.text)
                            .w(widths[j])
                            .text_align(TextAlign::Center)
                            .into(),
                    );
                }
                lines.push(
                    row()
                        .items_center()
                        .justify_center()
                        .gap(10.0)
                        .children(kids)
                        .into(),
                );
            }
            let grid_width = cols_width + gap_matrix * (ncols.saturating_sub(1)) as f32;
            let grid = col().w(grid_width).items_center().gap(4.0).children(lines);
            if open.is_empty() {
                return (col().w(width).items_center().children((grid,)).into(), width);
            }
            let delim_size = match rows.len() {
                0..=2 => TextSize::Lg,
                3 => TextSize::Xl,
                _ => TextSize::Xl2,
            };
            let left = text(open).size(delim_size).color(c.text_muted);
            let right = text(close).size(delim_size).color(c.text_muted);
            (
                row()
                    .items_center()
                    .justify_center()
                    .gap(3.0)
                    .children((left, grid, right))
                    .into(),
                width,
            )
        }
    }
}

// ============================================================
// 公开 API
// ============================================================

/// 裸 `\begin{...}` 环境（模型经常不加 `$` 直接输出，例如 `\begin{aligned}...\end{aligned}`）。
const KNOWN_ENVS: &[&str] = &[
    "aligned",
    "align",
    "alignat",
    "flalign",
    "split",
    "cases",
    "dcases",
    "matrix",
    "pmatrix",
    "bmatrix",
    "Bmatrix",
    "vmatrix",
    "Vmatrix",
    "array",
    "gathered",
    "gather",
    "equation",
    "multline",
];

/// 把一段 markdown/普通文本切成 文本 / 行内公式 / 块级公式 片段。
///
/// 支持的定界符：
/// - `$...$`（行内）、`$$...$$`（块级）；
/// - `\(...\)`（行内）、`\[...\]`（块级）；
/// - **裸环境**：`\begin{aligned}...\end{aligned}` 等已知环境即使没有定界符也按块级公式渲染
///   （模型经常这样输出，以前会原样显示 LaTeX 源码）；`\begin` 后跟 `array` 的列格式参数也支持。
///
/// 未闭合的定界符原样留在文本里；`\$` 表示字面美元符号。
pub fn split(text: &str) -> Vec<MathSegment> {
    // 先剔除零宽字符：它们可能被 chat.rs 的 soft_wrap_breaks 插进 `\begin` / `\frac`
    // 中间，导致定界符/环境名匹配失败、命令名被截断。
    let cleaned = strip_zero_width(text);
    let chars: Vec<char> = cleaned.chars().collect();
    let mut out: Vec<MathSegment> = Vec::new();
    let mut buf = String::new();
    let mut i = 0usize;
    while i < chars.len() {
        let ch = chars[i];
        if ch == '\\' && i + 1 < chars.len() {
            let next = chars[i + 1];
            // 转义的美元符号
            if next == '$' {
                buf.push('$');
                i += 2;
                continue;
            }
            // 行内 \(...\)
            if next == '(' {
                if let Some(end) = find_str(&chars, i + 2, "\\)") {
                    if !buf.is_empty() {
                        out.push(MathSegment::Text(std::mem::take(&mut buf)));
                    }
                    let inner: String = chars[i + 2..end].iter().collect();
                    out.push(MathSegment::Inline(inner.trim().to_string()));
                    i = end + 2;
                    continue;
                }
            }
            // 块级 \[...\]
            if next == '[' {
                if let Some(end) = find_str(&chars, i + 2, "\\]") {
                    if !buf.is_empty() {
                        out.push(MathSegment::Text(std::mem::take(&mut buf)));
                    }
                    let inner: String = chars[i + 2..end].iter().collect();
                    out.push(MathSegment::Block(inner.trim().to_string()));
                    i = end + 2;
                    continue;
                }
            }
            // 裸环境 \begin{env}...\end{env}
            if next == 'b' {
                if let Some((name, body_start)) = parse_begin(&chars, i) {
                    let base = name.trim_end_matches('*');
                    if KNOWN_ENVS.contains(&base) {
                        let end_tag = format!("\\end{{{name}}}");
                        if let Some(end) = find_str(&chars, body_start, &end_tag) {
                            if !buf.is_empty() {
                                out.push(MathSegment::Text(std::mem::take(&mut buf)));
                            }
                            let end_index = end + end_tag.chars().count();
                            let inner: String = chars[i..end_index].iter().collect();
                            out.push(MathSegment::Block(inner.trim().to_string()));
                            i = end_index;
                            continue;
                        }
                    }
                }
            }
        }
        if ch == '$' {
            // 块级公式 $$...$$
            if i + 1 < chars.len() && chars[i + 1] == '$' {
                if let Some(end) = find_double(&chars, i + 2) {
                    if !buf.is_empty() {
                        out.push(MathSegment::Text(std::mem::take(&mut buf)));
                    }
                    let inner: String = chars[i + 2..end].iter().collect();
                    out.push(MathSegment::Block(inner.trim().to_string()));
                    i = end + 2;
                    continue;
                }
                // 没有闭合：把 $$ 当普通文本
                buf.push('$');
                buf.push('$');
                i += 2;
                continue;
            }
            // 行内公式 $...$
            if let Some(end) = find_single(&chars, i + 1) {
                if !buf.is_empty() {
                    out.push(MathSegment::Text(std::mem::take(&mut buf)));
                }
                let inner: String = chars[i + 1..end].iter().collect();
                out.push(MathSegment::Inline(inner.trim().to_string()));
                i = end + 1;
                continue;
            }
            buf.push('$');
            i += 1;
            continue;
        }
        buf.push(ch);
        i += 1;
    }
    if !buf.is_empty() {
        out.push(MathSegment::Text(buf));
    }
    out
}

/// 在字符数组里从 `from` 开始找子串，返回起始下标。
fn find_str(chars: &[char], from: usize, needle: &str) -> Option<usize> {
    let needle: Vec<char> = needle.chars().collect();
    if needle.is_empty() {
        return Some(from.min(chars.len()));
    }
    if chars.len() < needle.len() {
        return None;
    }
    let mut i = from;
    while i + needle.len() <= chars.len() {
        if chars[i..i + needle.len()] == needle[..] {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// 解析 `\begin{env}`，返回 (环境名, 花括号后正文起点)。
fn parse_begin(chars: &[char], i: usize) -> Option<(String, usize)> {
    let begin: Vec<char> = "\\begin".chars().collect();
    if chars.len() < i + begin.len() || chars[i..i + begin.len()] != begin[..] {
        return None;
    }
    let mut p = i + begin.len();
    while p < chars.len() && chars[p].is_whitespace() {
        p += 1;
    }
    if p >= chars.len() || chars[p] != '{' {
        return None;
    }
    p += 1;
    let name_start = p;
    while p < chars.len() && chars[p] != '}' {
        p += 1;
    }
    if p >= chars.len() {
        return None;
    }
    let name = chars[name_start..p].iter().collect::<String>().trim().to_string();
    if name.is_empty() {
        return None;
    }
    Some((name, p + 1))
}

fn find_double(chars: &[char], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 1 < chars.len() {
        if chars[i] == '$' && chars[i + 1] == '$' {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn find_single(chars: &[char], from: usize) -> Option<usize> {
    let mut i = from;
    while i < chars.len() {
        if chars[i] == '$' {
            // 跳过 `$$`，避免把块级开头当行内结尾
            if i + 1 < chars.len() && chars[i + 1] == '$' {
                i += 2;
                continue;
            }
            return Some(i);
        }
        i += 1;
    }
    None
}

/// 行内公式的**纯文本形式**（供 chat.rs 把「文字 + 行内公式」合并成一段 markdown 渲染，
/// 避免 chat.rs 把每个 `Text`/`Inline` 段都 push 成独立纵向元素、导致"每段一行"）。
///
/// 返回单行文本，绝不返回原始 LaTeX（解析退化的坏输入会先去掉命令/花括号再输出）。
///
/// 注意：返回值会并入 markdown 源码，所以对 markdown 有语法含义的字符做了转义
/// （`\`、`` ` ``、`*`、`_`、`[`、`]`、`<`、`>`、`&`、`|`、`~`、`!`、`#`）。
/// 例如 `\sqrt[3]{x}` 返回 `√\[3\](x)`，经 markdown 渲染后显示为 `√[3](x)`，
/// 不会被当成链接；`\lim_{n\to\infty}` 返回 `lim\_(n → ∞)`，下划线不会变斜体。
pub fn inline_plain(latex: &str) -> String {
    let latex = latex.trim();
    if latex.is_empty() {
        return String::new();
    }
    let expr = parse_latex(latex);
    let mut text = flatten_text(&expr, false);
    text = text.replace('\n', " ");
    let text = tidy(&text);
    // 解析结果为空或残留反斜杠（理论上不会）时退化成可读文本，绝不让原始 LaTeX 进 markdown。
    let text = if text.is_empty() || text.contains('\\') {
        degraded_inline_text(latex)
    } else {
        text
    };
    escape_markdown_inline(&text)
}

/// 坏输入的退化文本：去掉 LaTeX 命令反斜杠/花括号/上下标记号，其余字符过字体安全层。
fn degraded_inline_text(latex: &str) -> String {
    let cleaned = strip_zero_width(latex);
    let mut out = String::new();
    for c in cleaned.chars() {
        match c {
            '\\' | '{' | '}' | '^' | '_' | '$' | '&' => {}
            '\n' => out.push(' '),
            _ => out.push_str(&glyph(c)),
        }
    }
    tidy(&out)
}

/// 行内 markdown 转义：只转义会影响行内解析的 ASCII 标点（CommonMark 反斜杠转义，
/// 渲染出来仍是原字符）。括号/斜杠/等号等保持原样，避免样本串里出现无意义反斜杠。
fn escape_markdown_inline(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if matches!(
            c,
            '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '&' | '|' | '~' | '!' | '#'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// 渲染行内公式。
///
/// 行内公式压平成一行"字体安全"的数学文本（`E=mc²`、`(a+b)/c`、`√[3](x)`）；
/// `\mathbf` 这类样式用带样式的文本 run 保留。多行环境在行内用 `; ` 串起来。
pub fn inline(latex: &str, c: &UiColors, size: TextSize) -> Element<Msg> {
    let latex = latex.trim();
    if latex.is_empty() {
        return col().h(0.0).into();
    }
    let expr = parse_latex(latex);
    let flat = flatten_text(&expr, false);
    if flat.is_empty() {
        return col().h(0.0).into();
    }
    // 太长的公式整体用单文本（可换行），避免一行 run 横向溢出。
    if text_units(&flat) > 70.0 {
        return inline_text(flat, size, c);
    }
    let mut runs: Vec<(String, RunStyle)> = Vec::new();
    collect_runs(&expr, &mut runs);
    // 没有样式差异时直接用单文本，省一层 row。
    if runs.iter().all(|(_, st)| *st == RunStyle::plain()) {
        return inline_text(flat, size, c);
    }
    let children: Vec<Element<Msg>> = runs
        .into_iter()
        .filter(|(s, _)| !s.is_empty())
        .map(|(s, style)| styled_text(s, size, c, style))
        .collect();
    if children.is_empty() {
        return inline_text(flat, size, c);
    }
    row().items_baseline().gap(0.5).children(children).into()
}

/// 渲染块级公式（居中、带底色，适合夹在消息正文中间）。
pub fn block(latex: &str, c: &UiColors) -> Element<Msg> {
    let latex = latex.trim();
    if latex.is_empty() {
        return col().h(0.0).into();
    }
    let expr = parse_latex(latex);
    let body: Element<Msg> = match &expr {
        Expr::Env { kind, rows } => render_env_block(*kind, rows, c, latex),
        Expr::Row(items) if items.len() == 1 => match &items[0] {
            Expr::Env { kind, rows } => render_env_block(*kind, rows, c, latex),
            other => block_text(flatten_text(other, true), c),
        },
        other => block_text(flatten_text(other, true), c),
    };
    col()
        .w_full()
        .items_center()
        .p(space::MD)
        .gap(space::XS)
        .bg(c.bg)
        .border(1.0, c.border_subtle)
        .rounded(R_CONTROL)
        .children((body,))
        .into()
}

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use neosonia_core::config::ThemeMode;

    fn colors() -> UiColors {
        let theme = crate::theme::nova_theme(ThemeMode::Dark, true, 1.0);
        crate::theme::colors(&theme)
    }

    /// 解析 + 压平（测试用）。
    fn flat(latex: &str, multiline: bool) -> String {
        flatten_text(&parse_latex(latex), multiline)
    }

    /// 输出字符是否"字体安全"：ASCII、希腊字母、内嵌 Inter 覆盖字符、中文/全角、或 `?`。
    fn output_char_ok(ch: char) -> bool {
        if ch == '\n' || ch == '?' || ch.is_ascii() {
            return true;
        }
        inter_has(ch) || is_cjk(ch)
    }

    /// 断言一段输出不出现原始 LaTeX / 缺字占位符 / 未映射字符。
    fn assert_clean(label: &str, s: &str) {
        assert!(!s.contains('\u{FFFD}'), "[{label}] 出现替换符 U+FFFD: {s:?}");
        assert!(!s.contains('\u{25A1}'), "[{label}] 出现方框 U+25A1: {s:?}");
        assert!(!s.contains('\u{25A0}'), "[{label}] 出现方框 U+25A0: {s:?}");
        assert!(!s.contains('\u{FFFE}') && !s.contains('\u{FFFF}'), "[{label}] 出现非法字符: {s:?}");
        assert!(!s.contains('\\'), "[{label}] 输出里残留原始 LaTeX 反斜杠: {s:?}");
        for ch in s.chars() {
            assert!(output_char_ok(ch), "[{label}] 出现未映射字符 U+{:04X}（{ch}）: {s:?}", ch as u32);
        }
    }

    #[test]
    fn split_handles_inline_and_block() {
        let segs = split("能量 $E=mc^2$，以及 $$\\int_0^1 x\\,dx$$ 结束");
        assert_eq!(segs.len(), 5);
        assert_eq!(segs[0], MathSegment::Text("能量 ".into()));
        assert_eq!(segs[1], MathSegment::Inline("E=mc^2".into()));
        assert_eq!(segs[2], MathSegment::Text("，以及 ".into()));
        assert_eq!(segs[3], MathSegment::Block("\\int_0^1 x\\,dx".into()));
        assert_eq!(segs[4], MathSegment::Text(" 结束".into()));
    }

    /// 用真机会话库里的原文复现（v0.7 用户样例，`$$...$$` 包裹）。
    #[test]
    fn repro_lead_math_v07_message() {
        let content = r#"行内 $\dfrac{a}{b}$ 和 $e^{x}$

块级
$$\sum_{n=0}^{\infty}\frac{x^n}{n!}$$

$$\begin{aligned} a&=b+c \\ d&=e-f \end{aligned}$$

$$\begin{pmatrix}1&2\\3&4\end{pmatrix}$$

$$\int_{-\infty}^{\infty} e^{-x^2}dx=\sqrt{\pi}$$"#;
        let segments = split(content);
        let mut inline = Vec::new();
        let mut blocks = Vec::new();
        for seg in &segments {
            match seg {
                MathSegment::Inline(latex) => inline.push(flat(latex, false)),
                MathSegment::Block(latex) => blocks.push(flat(latex, true)),
                MathSegment::Text(t) => eprintln!("[text] {t:?}"),
            }
        }
        eprintln!("[inline] {inline:?}");
        for b in &blocks {
            eprintln!("[block] {b:?}");
        }
        assert_eq!(inline, vec!["a/b".to_string(), "eˣ".to_string()]);
        assert_eq!(blocks.len(), 4, "应有 4 个块级公式: {blocks:?}");
        assert_eq!(blocks[0], "∑ₙ₌₀^∞xⁿ/n!", "求和卡内容: {:?}", blocks[0]);
        assert!(blocks[0].chars().all(output_char_ok), "求和卡出现未映射字符: {:?}", blocks[0]);
        assert!(!blocks[0].contains('?'), "求和卡不应出现 ? 兜底: {:?}", blocks[0]);
        assert_eq!(blocks[1].lines().count(), 2, "aligned 两行: {:?}", blocks[1]);
        assert_eq!(blocks[2].lines().count(), 2, "pmatrix 两行: {:?}", blocks[2]);
        assert!(blocks[3].contains('∫') && blocks[3].contains('∞'), "积分卡: {:?}", blocks[3]);

        // 关键回归：chat.rs 的 soft_wrap_breaks 会往超长 LaTeX 串里插零宽空格，
        // 必须剔除，否则 \frac 被拆成 fr?ac、\begin 识别失败。
        let corrupted = content
            .replace("\\frac", "\\fr\u{200B}ac")
            .replace("\\begin", "\\be\u{200B}gin")
            .replace("\\end", "\\en\u{200B}d")
            .replace("\\sqrt", "\\sq\u{200B}rt")
            .replace("\\int", "\\in\u{200B}t");
        let segments_zwsp = split(&corrupted);
        let inline_zwsp: Vec<String> = segments_zwsp
            .iter()
            .filter_map(|seg| match seg {
                MathSegment::Inline(latex) => Some(flat(latex, false)),
                _ => None,
            })
            .collect();
        let blocks_zwsp: Vec<String> = segments_zwsp
            .iter()
            .filter_map(|seg| match seg {
                MathSegment::Block(latex) => Some(flat(latex, true)),
                _ => None,
            })
            .collect();
        assert_eq!(inline_zwsp, inline, "零宽空格不应影响行内公式");
        assert_eq!(blocks_zwsp, blocks, "零宽空格不应影响块级公式");
        assert!(
            blocks_zwsp.iter().all(|b| !b.contains('?')),
            "零宽空格不应产生 ? 兜底: {blocks_zwsp:?}"
        );
    }

    /// 零宽空格直接插在命令名中间时的防御（chat.rs soft_wrap_breaks 的真实产物）。
    #[test]
    fn zero_width_chars_are_stripped_from_latex() {
        assert_eq!(flat("\\fr\u{200B}ac{x^n}{n!}", false), "xⁿ/n!");
        assert_eq!(flat("\\sq\u{200B}rt{x}", false), "√(x)");
        assert_eq!(flat("\\be\u{200B}gin{aligned} a&=b \\en\u{200B}d{aligned}", true), "a =b");
        // 正常文本里的零宽字符也不应输出成 ?
        assert!(!flat("a\u{200B}b", false).contains('?'));
    }

    /// chat.rs 合并「文字 + 行内公式」时用的纯文本形式。
    #[test]
    fn inline_plain_is_markdown_safe_single_line() {
        assert_eq!(inline_plain("\\dfrac{a}{b}"), "a/b");
        assert_eq!(inline_plain("e^{x}"), "eˣ");
        assert_eq!(inline_plain("x_{n+1}"), "xₙ₊₁");

        // 方括号/下划线必须转义，否则 markdown 会把 √[3](x) 当链接、把 lim_ 当斜体
        assert_eq!(inline_plain("\\sqrt[3]{x}"), "√\\[3\\](x)");
        assert_eq!(inline_plain("\\lim_{n\\to\\infty}"), "lim\\_(n→∞)");

        // 多行环境压成单行，不产生换行
        let aligned = inline_plain("\\begin{aligned} a &= 1 \\\\ b &= 2 \\end{aligned}");
        assert_eq!(aligned, "a = 1; b = 2");
        assert!(!aligned.contains('\n'));

        // 坏输入：不返回原始 LaTeX、不 panic
        for bad in ["\\frac{", "\\begin{pmatrix}", "}^{", "$$", "\\unknowncmd{x}", "\\\\"] {
            let out = inline_plain(bad);
            assert!(!out.contains('\u{FFFD}'), "{bad:?} -> {out:?}");
            assert!(!out.contains('\u{FFFE}'), "{bad:?} -> {out:?}");
        }
        assert_eq!(inline_plain("\\unknowncmd{x}"), "unknowncmdx");
        assert!(inline_plain("").is_empty());

        // 按 chat.rs 的方式合并进 markdown：应能正常构建、不 panic。
        let merged = format!(
            "行内 {} 和 {}，块级公式单独出卡片。",
            inline_plain("\\dfrac{a}{b}"),
            inline_plain("e^{x}")
        );
        let _: Element<Msg> = fenestra_markdown::markdown(&merged).into();
        let merged_escaped = format!(
            "{} 且 {}，{}",
            inline_plain("\\sqrt[3]{x}"),
            inline_plain("\\lim_{n\\to\\infty}"),
            inline_plain("|a b; c d|")
        );
        let _: Element<Msg> = fenestra_markdown::markdown(&merged_escaped).into();
    }

    #[test]
    fn split_keeps_unmatched_dollar_as_text() {
        assert_eq!(split("价格 $5 起"), vec![MathSegment::Text("价格 $5 起".into())]);
        assert_eq!(split("没有公式"), vec![MathSegment::Text("没有公式".into())]);
        assert!(split("").is_empty());
    }

    #[test]
    fn split_recognizes_latex_delimiters_and_bare_environments() {
        // \(...\) 行内
        let segs = split("行内 \\(a+b\\) 结束");
        assert_eq!(
            segs,
            vec![
                MathSegment::Text("行内 ".into()),
                MathSegment::Inline("a+b".into()),
                MathSegment::Text(" 结束".into()),
            ]
        );

        // \[...\] 块级
        let segs = split("块级 \\[\\frac{1}{2}\\] 结束");
        assert_eq!(segs.len(), 3, "{segs:?}");
        assert_eq!(segs[1], MathSegment::Block("\\frac{1}{2}".into()));

        // 裸环境（模型常不加 $，以前会原样显示 LaTeX 源码）
        let segs = split("前 \\begin{aligned} a &= b \\end{aligned} 后");
        assert_eq!(segs.len(), 3, "{segs:?}");
        match &segs[1] {
            MathSegment::Block(block) => {
                assert!(block.starts_with("\\begin{aligned}"), "{block:?}");
                assert!(block.ends_with("\\end{aligned}"), "{block:?}");
            }
            other => panic!("裸环境应识别成块级公式：{other:?}"),
        }

        // 裸 array 带列格式参数
        let segs = split("\\begin{array}{c|c} a & b \\end{array}");
        assert!(matches!(&segs[0], MathSegment::Block(_)), "{segs:?}");

        // 转义美元符号
        assert_eq!(split("价格 \\$5 起"), vec![MathSegment::Text("价格 $5 起".into())]);

        // 未闭合的 \[ 不当公式（例如 markdown 里的 `\\[2pt]` 换行间距）
        assert!(split(r"\\[2pt]").iter().all(|seg| matches!(seg, MathSegment::Text(_))));

        // 不认识的裸环境保持文本，避免误渲染
        let segs = split("\\begin{weird} x \\end{weird}");
        assert!(matches!(&segs[0], MathSegment::Text(_)), "{segs:?}");
    }

    /// 用户反馈里提到的所有公式类型 + 常见嵌套/坏输入。
    const CORPUS: &[&str] = &[
        // 多行环境
        "\\begin{aligned} a &= b + c \\\\ d &= e - f \\end{aligned}",
        "\\begin{cases} x, & x > 0 \\\\ -x, & x \\le 0 \\end{cases}",
        "\\begin{matrix} 1 & 2 \\\\ 3 & 4 \\end{matrix}",
        "\\begin{pmatrix} 1 & 2 \\\\ 3 & 4 \\end{pmatrix}",
        "\\begin{bmatrix} a & b \\\\ c & d \\end{bmatrix}",
        "\\begin{vmatrix} a & b \\\\ c & d \\end{vmatrix}",
        "\\begin{array}{cc} x & y \\\\ z & w \\end{array}",
        "\\begin{gathered} a = b \\\\ c = d \\end{gathered}",
        "\\begin{pmatrix} \\frac{1}{2} & 0 \\\\ 0 & \\sqrt{x} \\end{pmatrix}",
        // 字体命令
        "\\mathbf{v} = \\mathbb{R}^n",
        "\\mathcal{L}(x)",
        "\\mathbb{N} \\subset \\mathbb{Z} \\subset \\mathbb{Q} \\subset \\mathbb{R}",
        // 符号
        "\\int_0^1 x\\,dx = \\infty",
        "a \\ne b, a \\le b, a \\ge b, x \\in A, x \\notin B",
        "A \\subset B, A \\cup B, A \\cap B, p \\Rightarrow q, p \\Leftrightarrow q",
        "\\pm \\times \\div \\cdot 90^\\circ 3' 5''",
        "\\nabla f, \\partial x, \\sum_{i=1}^{n} i, \\prod_{i=1}^{n} i, \\sqrt{2}",
        // 嵌套上下标 / 分式 / 根号
        "e^{-x^2}",
        "a_{i,j}^{2}",
        "\\frac{\\frac{1}{2}}{3}",
        "\\sqrt[3]{x + 1}",
        "\\frac{a+b}{c} \\cdot \\frac{1}{2}",
        "x_{n+1} = x_n^2 + 1",
        // 其它写法
        "\\left( \\frac{a}{b} \\right)^2",
        "\\text{速度} v = \\frac{s}{t}",
        "\\sin^2 x + \\cos^2 x = 1",
        "\\lim_{n \\to \\infty} \\frac{1}{n} = 0",
        // v0.7 用户样例
        "\\dfrac{a}{b} + \\tfrac{c}{d}",
        "\\sqrt[n]{x}",
        "\\sum_{n=0}^{\\infty} \\frac{x^n}{n!}",
        "e^{x}",
        "\\det(A) \\ne 0",
        "\\operatorname{rank}(A) = 2",
        "\\mathrm{d}x \\, \\mathrm{d}y",
        "\\lim_{n\\to\\infty} \\left(1 + \\frac{1}{n}\\right)^n = e",
        "\\int_{-\\infty}^{\\infty} e^{-x^2} \\, dx = \\sqrt{\\pi}",
        "\\eta \\lambda \\sigma \\theta \\Gamma \\Omega",
        "\\begin{vmatrix} a & b \\\\ c & d \\end{vmatrix} = ad - bc",
        "\\begin{matrix} 1 & 2 \\\\ 3 & 4 \\end{matrix}",
        "\\begin{array}{c|c} x & f(x) \\\\ 1 & 2 \\end{array}",
        // 坏输入：不完整/多余/未知命令
        "\\frac{",
        "\\begin{matrix} 1 & 2",
        "$$",
        "a^",
        "}^{",
        "\\unknowncmd{x}",
        "\\begin{weird} a & b \\\\ c & d \\end{weird}",
        "\\text{中文，标点。}",
        "} ] { \\\\",
        "",
    ];

    /// v0.7 用户明确点名要覆盖的公式。
    #[test]
    fn v07_required_samples_are_readable() {
        // 分式变体
        assert_eq!(flat("\\dfrac{a}{b}", false), "a/b");
        assert_eq!(flat("\\tfrac{a}{b}", false), "a/b");

        // 根号任意次
        assert_eq!(flat("\\sqrt[3]{x}", false), "√[3](x)");
        assert_eq!(flat("\\sqrt[n]{x}", false), "√[n](x)");

        // 无穷级数
        let series = flat("\\sum_{n=0}^{\\infty} \\frac{x^n}{n!}", false);
        assert!(series.contains('∑'), "{series:?}");
        assert!(series.contains("ₙ₌₀"), "求和下标应 Unicode 化: {series:?}");
        assert!(series.contains("^∞"), "无穷上标应可读: {series:?}");
        assert!(series.contains("xⁿ"), "x^n 应 Unicode 化: {series:?}");
        assert!(!series.contains('\\'), "{series:?}");

        // 嵌套上下标
        assert_eq!(flat("e^{x}", false), "eˣ");
        assert_eq!(flat("x_{n+1}", false), "xₙ₊₁");

        // 函数名 / 矩阵行列式
        assert_eq!(flat("\\det(A) \\ne 0", false), "det(A) ≠ 0");
        assert_eq!(flat("\\operatorname{rank}(A) = 2", false), "rank(A) = 2");
        assert_eq!(flat("\\mathrm{d}x", false), "dx");

        // 极限 / 积分（含无穷上下限）
        let lim = flat("\\lim_{n\\to\\infty} \\frac{1}{n}", false);
        assert!(lim.starts_with("lim"), "{lim:?}");
        let integral = flat("\\int_{-\\infty}^{\\infty} e^{-x^2} \\, dx", false);
        assert!(integral.contains('∫') && integral.contains('∞'), "{integral:?}");
        assert!(!integral.contains('\\'), "{integral:?}");

        // 希腊字母（含不常用大写）
        let greek = flat("\\eta \\lambda \\sigma \\theta \\Gamma \\Omega \\Alpha \\Beta", false);
        for ch in ['η', 'λ', 'σ', 'θ', 'Γ', 'Ω', 'Α', 'Β'] {
            assert!(greek.contains(ch), "{ch} 未映射: {greek:?}");
        }

        // 矩阵/行列式环境仍是多行
        let vmatrix = flat("\\begin{vmatrix} a & b \\\\ c & d \\end{vmatrix}", true);
        assert_eq!(vmatrix.lines().count(), 2, "{vmatrix:?}");
        assert!(vmatrix.starts_with('|') && vmatrix.ends_with('|'), "{vmatrix:?}");
        let array = flat("\\begin{array}{c|c} x & f(x) \\\\ 1 & 2 \\end{array}", true);
        assert_eq!(array.lines().count(), 2, "{array:?}");
    }

    #[test]
    fn long_formulas_switch_to_horizontal_scroll_logic() {
        // 短公式：普通居中卡片，不滚动
        assert!(!needs_hscroll("E=mc²"));
        assert!(!needs_hscroll_width(300.0));

        // 长公式：走横向滚动（不撑破 424 宽窗口）
        let long = "x_{1}+x_{2}+x_{3}+x_{4}+x_{5}+x_{6}+x_{7}+x_{8}+x_{9}+x_{10}=y_{1}+y_{2}+y_{3}+y_{4}+y_{5}+y_{6}+y_{7}+y_{8}+y_{9}+y_{10}";
        assert!(needs_hscroll(long), "长公式应判定需要横向滚动");
        assert!(needs_hscroll_width(520.0));

        // 块级渲染构建不 panic（滚动容器 + 长文本）
        let c = colors();
        let _ = block(long, &c);
        let wide_matrix = "\\begin{pmatrix} a_{11} & a_{12} & a_{13} & a_{14} & a_{15} \\\\ a_{21} & a_{22} & a_{23} & a_{24} & a_{25} \\end{pmatrix}";
        let _ = block(wide_matrix, &c);
    }

    #[test]
    fn corpus_outputs_clean_readable_text() {
        for latex in CORPUS {
            let inline = flat(latex, false);
            let block = flat(latex, true);
            assert_clean(&format!("inline: {latex}"), &inline);
            assert_clean(&format!("block: {latex}"), &block);
        }
    }

    #[test]
    fn corpus_renders_without_panic() {
        let c = colors();
        for latex in CORPUS {
            let _ = inline(latex, &c, TextSize::Base);
            let _ = block(latex, &c);
        }
    }

    #[test]
    fn print_sample_outputs() {
        // 给 Lead 的真机截图核对用：跑 `cargo test ... math -- --nocapture` 时打印
        // 「输入 LaTeX -> 实际显示文本」。
        let samples: &[(&str, &str)] = &[
            ("行内二次公式", "E=mc^2"),
            ("分式", "\\frac{a+b}{c}"),
            ("嵌套分式", "\\frac{\\frac{1}{2}}{3}"),
            ("根号", "\\sqrt[3]{x+1}"),
            ("幂与下标", "x_{n+1} = x_n^2 + 1"),
            ("指数嵌套", "e^{-x^2}"),
            ("aligned", "\\begin{aligned} a &= b + c \\\\ d &= e - f \\end{aligned}"),
            ("cases", "\\begin{cases} x, & x > 0 \\\\ -x, & x \\le 0 \\end{cases}"),
            ("pmatrix", "\\begin{pmatrix} 1 & 2 \\\\ 3 & 4 \\end{pmatrix}"),
            ("bmatrix", "\\begin{bmatrix} a & b \\\\ c & d \\end{bmatrix}"),
            ("mathbf/mathbb", "\\mathbf{v} = \\mathbb{R}^n"),
            ("mathcal", "\\mathcal{L}(x)"),
            ("符号", "x \\in A, A \\subset B, A \\cup B, p \\Rightarrow q"),
            ("微积分", "\\int_0^1 x\\,dx = \\infty, \\nabla f, \\partial t"),
            ("逻辑", "\\forall x \\exists y, x \\ne y"),
            ("函数", "\\lim_{n \\to \\infty} \\frac{1}{n} = 0"),
            // v0.7 用户点名的样例
            ("dfrac/tfrac", "\\dfrac{a}{b} + \\tfrac{c}{d}"),
            ("任意次根", "\\sqrt[n]{x}"),
            ("无穷级数", "\\sum_{n=0}^{\\infty} \\frac{x^n}{n!}"),
            ("嵌套上标", "e^{x} = \\sum_{n=0}^{\\infty} \\frac{x^n}{n!}"),
            ("行列式", "\\det(A) \\ne 0"),
            ("operatorname", "\\operatorname{rank}(A) = 2"),
            ("极限/积分", "\\lim_{n\\to\\infty} \\int_{-\\infty}^{\\infty} e^{-x^2} \\, dx = \\sqrt{\\pi}"),
            ("希腊字母", "\\eta \\lambda \\sigma \\theta \\Gamma \\Omega"),
            ("vmatrix", "\\begin{vmatrix} a & b \\\\ c & d \\end{vmatrix} = ad - bc"),
            ("array", "\\begin{array}{c|c} x & f(x) \\\\ 1 & 2 \\end{array}"),
            (
                "长公式（块级横向滚动）",
                "x_{1}+x_{2}+x_{3}+x_{4}+x_{5}+x_{6}+x_{7}+x_{8}+x_{9}+x_{10}=y_{1}+y_{2}+y_{3}+y_{4}+y_{5}",
            ),
        ];
        for (label, src) in samples {
            eprintln!("[样例] {label}");
            eprintln!("  输入: {src}");
            eprintln!("  行内: {}", flat(src, false));
            eprintln!("  块级: {}", flat(src, true).replace('\n', " ⏎ "));
        }
    }

    #[test]
    fn environments_generate_multiple_lines() {
        let aligned = flat("\\begin{aligned} a &= 1 \\\\ b &= 2 \\end{aligned}", true);
        assert_eq!(aligned.lines().count(), 2, "aligned 应为两行: {aligned:?}");
        assert_eq!(aligned.lines().next().unwrap(), "a = 1");
        assert!(aligned.contains("b = 2"));

        let cases = flat("\\begin{cases} x & x>0 \\\\ -x & x\\le 0 \\end{cases}", true);
        assert_eq!(cases.lines().count(), 2, "cases 应为两行: {cases:?}");
        assert!(cases.contains('x'));
        // 源码里 `x, & x>0` 自带逗号时不要重复补逗号
        let cases_with_comma = flat("\\begin{cases} x, & x>0 \\\\ -x, & x\\le 0 \\end{cases}", false);
        assert_eq!(cases_with_comma, "x, x>0; -x, x≤ 0", "cases 逗号处理: {cases_with_comma:?}");

        let matrix = flat("\\begin{pmatrix} 1 & 2 \\\\ 3 & 4 \\end{pmatrix}", true);
        assert_eq!(matrix.lines().count(), 2, "matrix 应为两行: {matrix:?}");
        assert!(matrix.starts_with('(') && matrix.ends_with(')'), "pmatrix 应有括号: {matrix:?}");
        assert!(matrix.contains("1 2"), "矩阵单元格空格规整: {matrix:?}");

        let bmatrix = flat("\\begin{bmatrix} a & b \\\\ c & d \\end{bmatrix}", true);
        assert!(bmatrix.starts_with('[') && bmatrix.ends_with(']'), "bmatrix 应有方括号: {bmatrix:?}");

        // 行内环境压成一行，用 `; ` 连接
        let inline_env = flat("\\begin{aligned} a &= 1 \\\\ b &= 2 \\end{aligned}", false);
        assert_eq!(inline_env, "a = 1; b = 2");
    }

    #[test]
    fn font_commands_are_font_safe() {
        // Inter 缺 ℝ/ℒ：默认退化成普通字母（渲染时加粗体/斜体做视觉区分）
        assert_eq!(flat("\\mathbb{R}", false), "R");
        assert_eq!(flat("\\mathbb{N} \\subset \\mathbb{Z}", false), "N subset Z");
        assert_eq!(flat("\\mathcal{L}(x)", false), "L(x)");
        assert_eq!(flat("\\mathbf{v}", false), "v");
        // 不允许出现原始 LaTeX
        assert!(!flat("\\mathbf{v} = \\mathbb{R}^n", false).contains('\\'));
    }

    #[test]
    fn required_symbols_are_covered() {
        // Inter 覆盖：原样输出
        for (src, expect) in [
            ("\\infty", "∞"),
            ("\\partial", "∂"),
            ("\\sum", "∑"),
            ("\\prod", "∏"),
            ("\\sqrt{2}", "√(2)"),
            ("\\approx", "≈"),
            ("\\ne", "≠"),
            ("\\le", "≤"),
            ("\\ge", "≥"),
            ("\\pm", "±"),
            ("\\times", "×"),
            ("\\div", "÷"),
            ("\\cdot", "·"),
            ("\\circ", "°"),
            ("\\prime", "′"),
            ("\\dprime", "″"),
        ] {
            let out = flat(src, false);
            assert!(out.contains(expect), "{src} 应该输出 {expect}，实际 {out:?}");
        }
        // Inter 缺失：ASCII 可读替代（NEOSONIA_MATH_UNICODE 未设置时）
        if !unicode_preferred() {
            assert_eq!(flat("\\nabla", false), "grad");
            assert!(flat("x \\in A", false).contains(" in "));
            assert!(flat("x \\notin A", false).contains(" notin "));
            assert!(flat("A \\subset B", false).contains(" subset "));
            assert!(flat("A \\cup B", false).contains(" union "));
            assert!(flat("A \\cap B", false).contains(" cap "));
            assert!(flat("p \\Rightarrow q", false).contains('⇒'), "⇒ 在 Inter 里，应原样输出");
            assert!(flat("p \\Leftrightarrow q", false).contains('⇔'));
        }
    }

    #[test]
    fn nested_scripts_and_roots_stay_readable() {
        // e^{-x^2}：嵌套上标全部能 Unicode 化时直接输出 e⁻ˣ²
        let out = flat("e^{-x^2}", false);
        assert!(out.starts_with('e'), "{out:?}");
        assert!(out.contains('²'), "嵌套上标应保留平方: {out:?}");
        assert!(!out.contains('^') && !out.contains('_'), "能 Unicode 化时不应出现 ^/_ : {out:?}");

        assert_eq!(flat("\\sqrt[3]{x}", false), "√[3](x)");
        assert_eq!(flat("\\sqrt[4]{x+1}", false), "√[4](x+1)");
        assert_eq!(flat("\\frac{\\frac{1}{2}}{3}", false), "(1/2)/3");
        assert_eq!(flat("a_i^2", false), "aᵢ²");
        let with_comma = flat("a_{i,j}^{2}", false);
        assert!(with_comma.contains("(i,j)"), "逗号下标无法 Unicode 化时应退化成 (i,j): {with_comma:?}");
        assert!(with_comma.ends_with('²'), "{with_comma:?}");
    }

    #[test]
    fn unknown_commands_never_leak_latex() {
        let out = flat("\\unknowncmd{x}", false);
        assert!(!out.contains('\\'), "{out:?}");
        assert!(out.contains("unknowncmd"), "{out:?}");
    }
}

#[cfg(test)]
mod render_probe {
    //! 排障用：逐个渲染可疑公式，定位解析器/渲染器的无限递归。
    use super::*;
    use neosonia_core::config::ThemeMode;

    #[test]
    fn render_suspicious_formulas() {
        let cases = [
            "E=mc^2",
            "\\frac{a+b}{c}",
            "9\\times10^{13}",
            "3^4",
            "x^{n+1}",
            "\\sqrt{2}",
            "\\sum_{i=1}^{n} i",
            "\\left( \\frac{1}{2} \\right)^2",
            "a_{1} + a_{2}",
            "10^{-3}",
            "^{",
            "}^{",
            "\\frac{",
            "$$",
            "a^",
            "\\text{质量}",
            "\\int_{0}^{1} x^2 dx",
            "\\begin{aligned} a &= b \\\\ c &= d \\end{aligned}",
            "\\begin{pmatrix} 1 & 2 \\\\ 3 & 4 \\end{pmatrix}",
        ];
        let theme = crate::theme::nova_theme(ThemeMode::Dark, true, 1.0);
        let cols = crate::theme::colors(&theme);
        for latex in cases {
            eprintln!("[probe] 解析: {latex}");
            let expr = parse_latex(latex);
            eprintln!("[probe] units={}: {latex}", text_units(&flatten_text(&expr, false)));
            let _ = inline(latex, &cols, TextSize::Base);
            let _ = block(latex, &cols);
            eprintln!("[probe] 渲染完成: {latex}");
        }
    }
}
