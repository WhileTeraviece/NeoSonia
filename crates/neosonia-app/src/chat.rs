//! 对话页：消息流、工具调用卡片、子代理卡片、输入区。
//!
//! 设计约束（docs/ui-design.md）：单列、零 emoji、状态用 6px 圆点 + 文字，
//! 卡片 r12，padding 16（紧凑 12），分隔线只用 border_subtle。

use fenestra::prelude::*;
use fenestra::kit::icons;

use crate::state::{AppState, ChatItem, Msg, NoticeLevel, ToolCard, ToolState};
use crate::theme::{self, UiColors, dims, space, R_CARD, R_CHIP, R_CONTROL};

// ============================================================
// 状态点
// ============================================================

pub fn status_dot(c: &UiColors, state: &ToolState) -> Element<Msg> {
    let color = match state {
        ToolState::Running => c.warning,
        ToolState::Ok => c.success,
        ToolState::Failed => c.danger,
        ToolState::Cancelled => c.text_subtle,
    };
    col().w(6.0).h(6.0).rounded_full().bg(color).into()
}

fn small_meta(label: impl Into<String>, c: &UiColors) -> Element<Msg> {
    text(label)
        .size(TextSize::Xs)
        .color(c.text_subtle)
        .into()
}

fn pill(label: impl Into<String>, c: &UiColors, bg: Color, fg: Color) -> Element<Msg> {
    row()
        .px(6.0)
        .py(2.0)
        .rounded(R_CHIP)
        .bg(bg)
        .items_center()
        .children((text(label).size(TextSize::Xs).color(fg),))
        .into()
}

// ============================================================
// 消息
// ============================================================

fn user_bubble(body: &str, thumbs: &[(u32, u32, Vec<u8>)], c: &UiColors) -> Element<Msg> {
    let mut content: Vec<Element<Msg>> = Vec::new();
    if !thumbs.is_empty() {
        let mut imgs: Vec<Element<Msg>> = Vec::new();
        for (w, h, px) in thumbs.iter().take(4) {
            let ratio = if *w > 0 { *h as f32 / *w as f32 } else { 0.6 };
            let tw = 132.0f32;
            imgs.push(
                image_rgba8(*w, *h, px.clone())
                    .w(tw)
                    .h((tw * ratio).clamp(48.0, 200.0))
                    .rounded(R_CONTROL),
            );
        }
        content.push(row().gap(space::XS).wrap().children(imgs).into());
    }
    // 气泡宽度按内容估算并 clamp：之前用 max_w(300)+text(w_full)，父容器宽度不确定，
    // fenestra 会按单行测高、文字实际却绘制多行 → 文字"出框"并压到下面的模型消息上。
    // 同时给超长无空格串（LaTeX/URL）插入零宽空格，保证能换行。
    let text_w = if body.trim().is_empty() {
        0.0
    } else {
        let chars = body.chars().count() as f32;
        (chars * 15.0).clamp(120.0, 280.0)
    };
    if !body.trim().is_empty() {
        // 用户气泡保持纯文本（公式只在助手回复里渲染）。
        let wrapped = soft_wrap_breaks(body);
        content.push(
            text(wrapped)
                .size(TextSize::Base)
                .color(c.text)
                .leading(1.5)
                .w((text_w - 24.0).max(96.0))
                .into(),
        );
    }
    let bubble_w = if thumbs.is_empty() {
        (text_w + 24.0).max(80.0)
    } else {
        // 有缩略图时气泡至少能放下 132px 图
        (text_w + 24.0).max(180.0)
    };
    row().w_full().justify_end().children((
        col()
            .w(bubble_w)
            .min_w(0.0)
            .p(space::MD)
            .gap(space::XS)
            .rounded(R_CARD)
            .bg(c.surface_raised)
            .border(1.0, c.border_subtle)
            .children(content),
    ))
    .into()
}

/// 解析模型给出的本地路径时尝试的候选根：app home、工程 workspace、进程 cwd。
///
/// 模型经常输出 `shots/play-test.png` 这类相对路径，单看 cwd 不一定命中；
/// 依次尝试 home / workspace / cwd，找到第一个存在的文件。
#[derive(Clone, Debug, Default)]
pub(crate) struct PathRoots {
    pub home: std::path::PathBuf,
    pub workspace: std::path::PathBuf,
    pub cwd: std::path::PathBuf,
}

impl PathRoots {
    pub fn from_state(st: &AppState) -> Self {
        Self {
            home: st.paths.root.clone(),
            workspace: st.paths.workspace.clone(),
            cwd: std::env::current_dir().unwrap_or_default(),
        }
    }

    /// 测试用：三个根都取当前目录。
    pub fn for_tests() -> Self {
        let cwd = std::env::current_dir().unwrap_or_default();
        Self {
            home: cwd.clone(),
            workspace: cwd.clone(),
            cwd,
        }
    }

    pub fn resolve(&self, raw: &str) -> Option<std::path::PathBuf> {
        let raw = raw.trim();
        if raw.is_empty() {
            return None;
        }
        let path = std::path::Path::new(raw);
        if path.is_absolute() {
            return path.exists().then(|| path.to_path_buf());
        }
        for root in [&self.home, &self.workspace, &self.cwd] {
            if root.as_os_str().is_empty() {
                continue;
            }
            let candidate = root.join(path);
            if candidate.exists() {
                return Some(candidate);
            }
        }
        None
    }
}

/// 正文里的本地媒体/文件段：`![alt](path)` 与指向本地文件的 `[文字](path)`。
#[derive(Debug)]
enum MediaChunk {
    Text(String),
    Image { alt: String, raw: String },
    LocalFile { text: String, raw: String },
}

fn ext_of(raw: &str) -> String {
    std::path::Path::new(raw)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn is_image_ext(ext: &str) -> bool {
    matches!(ext, "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" | "svg" | "ico")
}

fn is_file_ext(ext: &str) -> bool {
    is_image_ext(ext)
        || matches!(
            ext,
            "lnk"
                | "url"
                | "pdf"
                | "txt"
                | "md"
                | "json"
                | "csv"
                | "doc"
                | "docx"
                | "xls"
                | "xlsx"
                | "ppt"
                | "pptx"
                | "zip"
                | "7z"
                | "rar"
                | "mp4"
                | "mov"
                | "webm"
                | "avi"
                | "mp3"
                | "wav"
                | "exe"
                | "rs"
                | "py"
                | "js"
                | "ts"
                | "html"
                | "css"
                | "toml"
    )
}

/// 把一段 markdown 里"指向本地文件"的图片/链接切出来，交给自定义渲染。
/// 非本地（http/https/锚点/mailto/data）保持原样走 markdown。
fn split_local_media(text: &str, roots: &PathRoots) -> Vec<MediaChunk> {
    let mut out: Vec<MediaChunk> = Vec::new();
    let mut pending = String::new();
    let mut i = 0usize;
    while i < text.len() {
        if text[i..].starts_with('!') || text[i..].starts_with('[') {
            if let Some(parsed) = parse_md_link_at(text, i) {
                let (is_image, alt, dest, end) = parsed;
                let dest_trim = dest.trim().trim_matches(['"', '\'']);
                let local = !dest_trim.contains("://")
                    && !dest_trim.starts_with('#')
                    && !dest_trim.starts_with("mailto:")
                    && !dest_trim.starts_with("data:");
                let ext = ext_of(dest_trim);
                let resolve_ok = local && roots.resolve(dest_trim).is_some();
                let take = is_image || (local && (resolve_ok || is_file_ext(&ext)));
                if take {
                    if !pending.is_empty() {
                        out.push(MediaChunk::Text(std::mem::take(&mut pending)));
                    }
                    if is_image && local {
                        out.push(MediaChunk::Image {
                            alt,
                            raw: dest_trim.to_string(),
                        });
                    } else {
                        out.push(MediaChunk::LocalFile {
                            text: alt,
                            raw: dest_trim.to_string(),
                        });
                    }
                    i = end;
                    continue;
                }
                // 非本地（或不像本地文件）：原样保留，继续扫
                pending.push_str(&text[i..end]);
                i = end;
                continue;
            }
        }
        let ch = text[i..].chars().next().expect("in-bounds char");
        pending.push(ch);
        i += ch.len_utf8();
    }
    if !pending.is_empty() {
        out.push(MediaChunk::Text(pending));
    }
    out
}

/// 解析 `[alt](dest)` / `![alt](dest)`，返回 (是否图片, alt, dest, 结束字节)。
fn parse_md_link_at(text: &str, start: usize) -> Option<(bool, String, String, usize)> {
    let is_image = text[start..].starts_with("![");
    let open = if is_image { start + 2 } else { start + 1 };
    let bytes = text.as_bytes();
    let mut br = open;
    while br < bytes.len() && bytes[br] != b']' && bytes[br] != b'\n' {
        br += 1;
    }
    if br >= bytes.len() || bytes[br] != b']' {
        return None;
    }
    let mut k = br + 1;
    while k < bytes.len() && (bytes[k] == b' ' || bytes[k] == b'\t') {
        k += 1;
    }
    if k >= bytes.len() || bytes[k] != b'(' {
        return None;
    }
    let dest_start = k + 1;
    let mut m = dest_start;
    let mut depth = 0usize;
    while m < bytes.len() {
        match bytes[m] {
            b'(' => depth += 1,
            b')' => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            b'\n' => return None,
            _ => {}
        }
        m += 1;
    }
    if m >= bytes.len() {
        return None;
    }
    let alt = text[open..br].to_string();
    let raw_dest = text[dest_start..m].trim();
    // 去掉可选 title：`path "title"` / `path 'title'`
    let dest = if let Some(first) = raw_dest.split_whitespace().next() {
        first.to_string()
    } else {
        raw_dest.to_string()
    };
    Some((is_image, alt, dest, m + 1))
}

/// markdown 文本渲染：断行安全 + 链接可点击（`[文字](url)` 不显示原始语法）。
fn markdown_element(text: &str) -> Element<Msg> {
    let wrapped = wrap_markdown_text(text);
    let md = fenestra_markdown::markdown(wrapped).on_link(|url| Msg::OpenLink(url.to_string()));
    Element::<Msg>::from(md).w_full().min_w(0.0).into()
}

/// 行内文本断行预处理：
/// - 超过 24 字符的无空白 token 插零宽空格（长 URL / 长单词 / 长 LaTeX 都能折行）；
/// - 裸 `http(s)://…` 转成 `[短显示](完整 URL)`，可点击且不撑破窗口；
/// - markdown 链接的 `](dest)` 目标区保持原样（不能往 URL 里插零宽空格）。
fn wrap_markdown_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 16);
    let mut i = 0usize;
    let mut run = 0usize;
    let mut in_code = false;
    let mut in_dest = false;
    while i < text.len() {
        let ch = text[i..].chars().next().expect("in-bounds char");
        let ch_len = ch.len_utf8();
        if ch == '`' {
            in_code = !in_code;
            out.push(ch);
            i += ch_len;
            run = 0;
            continue;
        }
        if !in_code {
            if text[i..].starts_with("](") {
                in_dest = true;
                out.push_str("](");
                i += 2;
                run = 0;
                continue;
            }
            if in_dest {
                if ch == ')' {
                    in_dest = false;
                }
                out.push(ch);
                i += ch_len;
                run = 0;
                continue;
            }
            // markdown 链接：显示文字过长时缩短（fenestra_markdown 把链接文字渲染成
            // 不可断行的 shrink0 片段，长文字会直接撑出面板；目标 URL 保持完整）。
            if run == 0 && text[i..].starts_with('[') && !text[i..].starts_with("![") {
                if let Some((is_image, alt, dest, end)) = parse_md_link_at(text, i) {
                    if !is_image {
                        let display = if alt.chars().count() > 18 {
                            let mut short: String = alt.chars().take(16).collect();
                            short.push('…');
                            short
                        } else {
                            alt
                        };
                        out.push('[');
                        out.push_str(&display);
                        out.push_str("](");
                        out.push_str(&dest);
                        out.push(')');
                        i = end;
                        run = 0;
                        continue;
                    }
                }
            }
        }
        if ch.is_whitespace() {
            run = 0;
        } else {
            run += 1;
            if run >= 24 && ch != '\u{200B}' {
                out.push('\u{200B}');
                run = 0;
            }
        }
        out.push(ch);
        i += ch_len;
    }
    out
}

fn shorten_url_display(url: &str) -> String {
    let no_scheme = url
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let mut display: String = no_scheme.chars().take(24).collect();
    if display.chars().count() < no_scheme.chars().count() {
        display.push('…');
    }
    display
}

/// 把 assistant 文本按数学公式切段渲染：普通段走 markdown，公式段走 math 渲染。
fn rich_body(body: &str, c: &UiColors, roots: &PathRoots) -> Vec<Element<Msg>> {
    // 排障开关：NEOSONIA_NO_MATH=1 时全部按 markdown 渲染（用于定位公式渲染问题）
    if std::env::var("NEOSONIA_NO_MATH").map(|v| v == "1").unwrap_or(false) {
        return vec![markdown_element(body)];
    }
    // 表格与数学公式可能出现在同一条消息里：
    // - 表格块必须整段交给 markdown（数学分段会把表格行切开 → 表体变成原始 "| ... |"）；
    // - 其它段落仍要做数学分段，否则同一条消息里的 $$ 公式会变回原始 LaTeX。
    let has_table = body
        .lines()
        .any(|l| l.trim_start().starts_with('|') && l.matches('|').count() >= 2);
    if has_table {
        let mut out: Vec<Element<Msg>> = Vec::new();
        for chunk in split_table_chunks(body) {
            if chunk.is_table {
                out.push(markdown_element(&chunk.text));
            } else {
                out.extend(render_math_segments(&chunk.text, c, roots));
            }
        }
        return out;
    }
    // 代码块要自定义渲染（横向滚动 + 语言标签），先按围栏切段；
    // 非代码段仍走 markdown/公式渲染（递归调用本函数）。
    let chunks = split_code_blocks(body);
    if chunks.iter().any(|chunk| matches!(chunk, BodyChunk::Code { .. })) {
        let mut out: Vec<Element<Msg>> = Vec::new();
        for chunk in chunks {
            match chunk {
                BodyChunk::Code { lang, code } => out.push(code_block(&lang, &code, c)),
                BodyChunk::Text(t) => {
                    if !t.trim().is_empty() {
                        out.extend(rich_body(&t, c, roots));
                    }
                }
            }
        }
        return out;
    }
    let segments = crate::math::split(body);
    let has_math = segments
        .iter()
        .any(|s| matches!(s, crate::math::MathSegment::Inline(_) | crate::math::MathSegment::Block(_)));
    if !has_math {
        let mut out = Vec::new();
        flush_text_chunk(&mut body.to_string(), c, roots, &mut out);
        return out;
    }
    let out = render_math_segments(body, c, roots);
    if out.is_empty() {
        let mut out = Vec::new();
        flush_text_chunk(&mut body.to_string(), c, roots, &mut out);
        out
    } else {
        out
    }
}

/// 行内图片/文件把文本切成多段后，`**` 可能被拆成两段各自不配对。
/// 按顺序统计未闭合的加粗：给前段补 `**` 收尾、给后段补 `**` 重开，保证每段自身成对。
fn balance_bold_across_media(chunks: &mut [MediaChunk]) {
    let mut bold = false;
    for chunk in chunks.iter_mut() {
        if let MediaChunk::Text(t) = chunk {
            if bold {
                t.insert_str(0, "**");
            }
            let odd = t.matches("**").count() % 2 == 1;
            if odd {
                t.push_str("**");
            }
            bold = odd;
        }
    }
}

/// 解析一行 blockquote：返回 (嵌套层级, 去掉 `>` 标记后的内容)。
fn parse_quote_line(line: &str) -> Option<(usize, String)> {
    let trimmed = line.trim_start();
    if !trimmed.starts_with('>') {
        return None;
    }
    let mut depth = 0usize;
    let mut rest = trimmed;
    while let Some(after) = rest.strip_prefix('>') {
        depth += 1;
        rest = after.strip_prefix(' ').unwrap_or(after);
    }
    Some((depth, rest.trim_end().to_string()))
}

/// 引用块渲染：左侧 2-3px 竖线 + 可换行内容列（fenestra_markdown 的引用行
/// 会把内容放进无宽度约束的 row，长行会撑出面板）。
fn push_quote_block(lines: &[(usize, String)], c: &UiColors, out: &mut Vec<Element<Msg>>) {
    if lines.is_empty() {
        return;
    }
    let depth = lines.iter().map(|(d, _)| *d).max().unwrap_or(1).max(1);
    let content = lines
        .iter()
        .map(|(_, text)| text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut inner: Vec<Element<Msg>> = Vec::new();
    push_markdown_blocks(&content, c, &mut inner);
    if inner.is_empty() {
        return;
    }
    let indent = (depth.saturating_sub(1) as f32) * 10.0;
    let bar_color = c.border_subtle;
    out.push(
        row()
            .w_full()
            .min_w(0.0)
            .gap(space::XS)
            .items_start()
            .children((
                div().w(3.0).shrink0().rounded(1.5).bg(bar_color),
                col()
                    .grow()
                    .min_w(0.0)
                    .pl(indent)
                    .gap(space::XS)
                    .children(inner),
            ))
            .into(),
    );
}
/// 解析一行 markdown 列表项：返回 (marker 文本, 内容, 缩进层级)。
fn parse_list_line(line: &str) -> Option<(String, String, f32)> {
    let indent_chars = line.chars().take_while(|c| *c == ' ' || *c == '\t').count();
    let level = (indent_chars / 2) as f32;
    let trimmed = line.trim_start();
    for prefix in ["- ", "* ", "+ "] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return Some((
                prefix.trim().to_string(),
                rest.trim_start().to_string(),
                level,
            ));
        }
    }
    let bytes = trimmed.as_bytes();
    let mut idx = 0usize;
    while idx < bytes.len() && bytes[idx].is_ascii_digit() {
        idx += 1;
    }
    if idx > 0
        && idx < bytes.len()
        && (bytes[idx] == b'.' || bytes[idx] == b')')
        && bytes.get(idx + 1) == Some(&b' ')
    {
        let marker = format!("{}.", &trimmed[..idx]);
        let content = trimmed[idx + 2..].trim_start().to_string();
        return Some((marker, content, level));
    }
    None
}

/// 列表项渲染：marker + 可换行的内容列（fenestra_markdown 的列表行不带宽度
/// 约束，长文本/长 URL 会撑出面板，所以这里自己排版）。
fn push_list_items(items: &[(String, String, f32)], c: &UiColors, out: &mut Vec<Element<Msg>>) {
    for (marker, content, level) in items {
        let indent = (level * 16.0).clamp(0.0, 48.0);
        let marker_el: Element<Msg> = text(marker.clone())
            .size(TextSize::Sm)
            .color(c.text_muted)
            .into();
        let body = markdown_element(content);
        out.push(
            row()
                .w_full()
                .min_w(0.0)
                .pl(indent)
                .gap(6.0)
                .items_start()
                .children((marker_el, col().grow().min_w(0.0).children((body,))))
                .into(),
        );
    }
}

/// 把一段普通文本按"列表项 / 普通段落"渲染；列表自己排版，其余交给 markdown。
fn push_markdown_blocks(text: &str, c: &UiColors, out: &mut Vec<Element<Msg>>) {
    let mut para = String::new();
    let mut items: Vec<(String, String, f32)> = Vec::new();
    let mut quote: Vec<(usize, String)> = Vec::new();
    for line in text.lines() {
        if let Some(item) = parse_quote_line(line) {
            if !para.trim().is_empty() {
                out.push(markdown_element(&std::mem::take(&mut para)));
            }
            if !items.is_empty() {
                push_list_items(&items, c, out);
                items.clear();
            }
            quote.push(item);
            continue;
        }
        if !quote.is_empty() {
            push_quote_block(&quote, c, out);
            quote.clear();
        }
        if let Some(item) = parse_list_line(line) {
            if !para.trim().is_empty() {
                out.push(markdown_element(&std::mem::take(&mut para)));
            }
            items.push(item);
            continue;
        }
        if !items.is_empty() {
            push_list_items(&items, c, out);
            items.clear();
        }
        para.push_str(line);
        para.push('\n');
    }
    if !items.is_empty() {
        push_list_items(&items, c, out);
    }
    if !quote.is_empty() {
        push_quote_block(&quote, c, out);
    }
    if !para.trim().is_empty() {
        out.push(markdown_element(&para));
    }
}
/// 把待渲染的普通文本（可能含本地图片/文件链接）切成元素。
fn flush_text_chunk(
    pending: &mut String,
    c: &UiColors,
    roots: &PathRoots,
    out: &mut Vec<Element<Msg>>,
) {
    if pending.trim().is_empty() {
        pending.clear();
        return;
    }
    let text = std::mem::take(pending);
    let mut chunks = split_local_media(&text, roots);
    balance_bold_across_media(&mut chunks);
    for chunk in chunks {
        match chunk {
            MediaChunk::Text(t) => {
                if !t.trim().is_empty() {
                    push_markdown_blocks(&t, c, out);
                }
            }
            MediaChunk::Image { alt, raw } => out.push(local_image_block(&alt, &raw, c, roots)),
            MediaChunk::LocalFile { text, raw } => {
                out.push(local_file_block(&text, &raw, c, roots))
            }
        }
    }
}

/// 本地图片：能解码就显示缩略图（复用 delivered_block 的 `image_rgba8` 路径），失败退化为文件卡。
fn local_image_block(
    alt: &str,
    raw: &str,
    c: &UiColors,
    roots: &PathRoots,
) -> Element<Msg> {
    let Some(path) = roots.resolve(raw) else {
        return file_row(
            if alt.is_empty() { raw } else { alt },
            "图片（文件不存在）",
            raw,
            c,
            false,
        );
    };
    let Some((w, h, px)) = crate::app::load_png_pixels_public(&path, 640) else {
        return local_file_card(&path, c);
    };
    if w == 0 || h == 0 {
        return local_file_card(&path, c);
    }
    let max_w = 240.0f32;
    let scale = (max_w / w as f32).min(1.0);
    let disp_w = (w as f32 * scale).max(40.0);
    let disp_h = (h as f32 * scale).max(30.0);
    let open_path = path.display().to_string();
    let mut items: Vec<Element<Msg>> = vec![
        image_rgba8(w, h, px)
            .w(disp_w)
            .h(disp_h)
            .rounded(R_CONTROL)
            .on_click(Msg::OpenPath(open_path))
            .into(),
    ];
    if !alt.trim().is_empty() {
        items.push(
            text(soft_wrap_breaks(alt))
                .size(TextSize::Xs)
                .color(c.text_subtle)
                .into(),
        );
    }
    col().w_full().min_w(0.0).gap(2.0).items_start().children(items).into()
}

/// 本地文件链接（含 .lnk / .url）：文件卡，点击用系统默认方式打开。
fn local_file_block(
    text_: &str,
    raw: &str,
    c: &UiColors,
    roots: &PathRoots,
) -> Element<Msg> {
    let Some(path) = roots.resolve(raw) else {
        return file_row(
            if text_.is_empty() { raw } else { text_ },
            "文件（不存在）",
            raw,
            c,
            false,
        );
    };
    local_file_card(&path, c)
}

fn local_file_card(path: &std::path::Path, c: &UiColors) -> Element<Msg> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_else(|| path.to_str().unwrap_or("文件"));
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let hint = match ext.as_str() {
        "lnk" | "url" => "快捷方式",
        _ => "文件",
    };
    file_row(name, hint, &path.display().to_string(), c, matches!(ext.as_str(), "lnk" | "url"))
}

/// 通用文件行卡片：图标 + 名称 + 提示 + 打开；整行可点（系统默认方式）。
fn file_row(name: &str, hint: &str, path: &str, c: &UiColors, shortcut: bool) -> Element<Msg> {
    let accent = c.accent;
    let icon_name = if shortcut { "external-link" } else { "file" };
    let icon: Element<Msg> = icons::lucide::by_name(icon_name)
        .map(|e| e.w(16.0).h(16.0).themed(move |_t, s| s.color(accent)))
        .unwrap_or_else(|| col().w(0.0).into());
    let path_owned = path.to_string();
    row()
        .w_full()
        .min_w(0.0)
        .gap(space::XS)
        .p(space::XS)
        .rounded(R_CONTROL)
        .bg(c.surface)
        .border(1.0, c.border_subtle)
        .items_center()
        .on_click(Msg::OpenPath(path_owned))
        .children((
            icon,
            col()
                .grow()
                .min_w(0.0)
                .gap(1.0)
                .children((
                    text(soft_wrap_breaks(name)).size(TextSize::Sm).color(c.text),
                    text(hint).size(TextSize::Xs).color(c.text_subtle),
                )),
            text("打开").size(TextSize::Xs).color(c.accent),
        ))
        .into()
}

/// 正文分段：普通文本 / 围栏代码块。
#[derive(Debug)]
enum BodyChunk {
    Text(String),
    Code {
        lang: String,
        code: String,
    },
}

/// 按围栏代码块（``` 或 ~~~，可带语言标注）把正文切段。
/// 未闭合的围栏按 CommonMark 语义吞到文末、当作代码块。
fn split_code_blocks(body: &str) -> Vec<BodyChunk> {
    let mut out: Vec<BodyChunk> = Vec::new();
    let mut text = String::new();
    let mut lines = body.split_inclusive('\n');
    while let Some(line) = lines.next() {
        let stripped = line.trim_start();
        let fence = stripped.chars().next().filter(|c| *c == '`' || *c == '~');
        let marker = fence
            .map(|f| stripped.chars().take_while(|c| *c == f).count())
            .unwrap_or(0);
        if fence.is_none() || marker < 3 {
            text.push_str(line);
            continue;
        }
        // 语言标注：围栏后到行尾的第一个 token（兼容 ```{rust} 写法）
        let info = stripped[marker..].trim();
        let lang = info
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_start_matches('{')
            .trim_end_matches('}')
            .to_string();
        if !text.is_empty() {
            out.push(BodyChunk::Text(std::mem::take(&mut text)));
        }
        let fence_char = fence.expect("fence checked above");
        let mut code = String::new();
        for inner in lines.by_ref() {
            let s = inner.trim_start();
            let close = s.chars().take_while(|c| *c == fence_char).count();
            if close >= marker && s.chars().skip(close).all(|c| c.is_whitespace()) {
                break;
            }
            code.push_str(inner);
        }
        out.push(BodyChunk::Code { lang, code });
    }
    if !text.is_empty() {
        out.push(BodyChunk::Text(text));
    }
    out
}

/// 代码块渲染：右上角语言标签；内容等宽、不换行，放进 `scroll_x()` 横向滚动容器。
///
/// 注意：`assistant_block` 会先给正文插零宽空格帮长串换行，代码里必须剔除，
/// 否则会改变代码内容、也会让宽度估算和横向滚动失效。
fn code_block<Msg>(lang: &str, code: &str, c: &UiColors) -> Element<Msg> {
    let code: String = code
        .chars()
        .filter(|ch| {
            !matches!(
                ch,
                '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{2060}' | '\u{FEFF}' | '\u{00AD}'
            )
        })
        .collect();
    let code = code.trim_end_matches(['\n', '\r']);
    if code.is_empty() {
        return col().h(0.0).into();
    }
    let size = TextSize::Xs;
    let line_h = size.px() * 1.55;
    let lines = code.lines().count().max(1);
    let max_chars = code
        .lines()
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(0)
        .max(1);
    // 等宽字形约 0.6em，按 0.72em 估宽度留余量：宁可多一点横向滚动，也不让代码折行。
    let content_w = (max_chars as f32 * size.px() * 0.72).max(120.0) + 24.0;

    let label = if lang.is_empty() { "text" } else { lang };
    let header = row()
        .w_full()
        .justify_end()
        .children((text(label).size(TextSize::Xs).mono().color(c.text_subtle),));

    let code_text = text(code.to_string())
        .size(size)
        .mono()
        .color(c.text)
        .leading(1.55)
        .w(content_w);
    let scroll_id = format!("code-block-{:016x}", fnv1a(code.as_bytes()));
    let scroll = col()
        .w_full()
        .min_w(0.0)
        .min_h((lines as f32 * line_h + 16.0).clamp(24.0, 620.0))
        .scroll_x()
        .id(&scroll_id)
        .children((col().w(content_w).shrink0().children((code_text,)),));

    col()
        .w_full()
        .min_w(0.0)
        .label("代码块")
        .rounded(R_CONTROL)
        .bg(c.surface)
        .border(1.0, c.border_subtle)
        .px(space::SM)
        .pt(space::XS)
        .pb(space::SM)
        .gap(space::XS)
        .children((header, scroll))
        .into()
}

/// FNV-1a：给每个代码块的滚动容器生成稳定 id（不同代码块各自保持横向滚动位置）。
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// 把文字段与行内公式合并成一段 markdown（同一行流动），块级公式单独出卡片。
fn render_math_segments(body: &str, c: &UiColors, roots: &PathRoots) -> Vec<Element<Msg>> {
    let mut out: Vec<Element<Msg>> = Vec::new();
    let mut pending = String::new();
    for seg in crate::math::split(body) {
        match seg {
            crate::math::MathSegment::Text(t) => pending.push_str(&t),
            crate::math::MathSegment::Inline(latex) => {
                pending.push_str(&crate::math::inline_plain(&latex))
            }
            crate::math::MathSegment::Block(latex) => {
                flush_text_chunk(&mut pending, c, roots, &mut out);
                out.push(crate::math::block(&latex, c));
            }
        }
    }
    flush_text_chunk(&mut pending, c, roots, &mut out);
    out
}

fn assistant_block(
    body: &str,
    reasoning: &str,
    reasoning_open: bool,
    streaming: bool,
    error: Option<&str>,
    idx: usize,
    c: &UiColors,
    roots: &PathRoots,
) -> Element<Msg> {
    let mut col_items: Vec<Element<Msg>> = Vec::new();
    // 不再对整条正文插零宽空格：那会把 markdown 链接目标 `](url)` 和代码块内容弄坏。
    // 断行交给 rich_body 里的 markdown 感知预处理（长 token 折行、URL 链接化）。
    let body = body;

    if !reasoning.is_empty() {
        let label = if reasoning_open { "思考中（点击收起）" } else { "思考过程（点击展开）" };
        col_items.push(
            col()
                .w_full()
                .rounded(R_CARD)
                .bg(c.surface)
                .border(1.0, c.border_subtle)
                .p(space::SM)
                .gap(space::XS)
                .on_click(Msg::ToggleReasoning(idx))
                .children((
                    row()
                        .gap(space::XS)
                        .items_center()
                        .children((text(label).size(TextSize::Xs).color(c.text_subtle),)),
                    if reasoning_open {
                        let el: Element<Msg> =
                            text(reasoning).size(TextSize::Sm).color(c.text_muted).into();
                        el
                    } else {
                        col().h(0.0).into()
                    },
                )),
        );
    }

    if !body.is_empty() {
        col_items.extend(rich_body(body, c, roots));
    } else if streaming {
        col_items.push(
            row()
                .gap(6.0)
                .items_center()
                .children((
                    col().w(6.0).h(6.0).rounded_full().bg(c.accent),
                    text("正在思考").size(TextSize::Sm).color(c.text_subtle),
                ))
                .into(),
        );
    }

    if let Some(e) = error {
        col_items.push(
            row()
                .w_full()
                .p(space::SM)
                .rounded(R_CARD)
                .bg(c.danger_bg.with_alpha(0.35))
                .border(1.0, c.danger.with_alpha(0.5))
                .children((text(e).size(TextSize::Sm).color(c.danger),)),
        );
    }

    // 注意：不要 items_start（列方向会让子元素按 max-content 收缩，长段落不换行被裁切）
    col()
        .w_full()
        .min_w(0.0)
        .gap(space::SM)
        .children(col_items)
        .into()
}

fn tool_card(card: &ToolCard, c: &UiColors) -> Element<Msg> {
    let state_color = match card.state {
        ToolState::Running => c.warning,
        ToolState::Ok => c.success,
        ToolState::Failed => c.danger,
        ToolState::Cancelled => c.text_subtle,
    };
    let title = card
        .preview
        .clone()
        .filter(|p| !p.trim().is_empty())
        .unwrap_or_else(|| card.name.clone());
    let elapsed = if card.elapsed_ms > 0 {
        format!(" {}", neosonia_core::util::human_duration_ms(card.elapsed_ms))
    } else {
        String::new()
    };

    let mut head: Vec<Element<Msg>> = vec![
        col().w(6.0).h(6.0).rounded_full().bg(state_color).into(),
        text(&card.name).size(TextSize::Sm).weight(Weight::Medium).color(c.text).into(),
        spacer().into(),
        text(card.state.label()).size(TextSize::Xs).color(state_color).into(),
        text(elapsed).size(TextSize::Xs).color(c.text_subtle).into(),
    ];
    head.push(text(if card.expanded { "收起" } else { "详情" }).size(TextSize::Xs).color(c.text_subtle).into());

    let mut body: Vec<Element<Msg>> = Vec::new();
    if !title.trim().is_empty() && title != card.name {
        body.push(
            col()
                .w_full()
                .p(space::SM)
                .rounded(R_CONTROL)
                .bg(c.bg)
                .children((text(title).size(TextSize::Xs).mono().color(c.text_muted),)),
        );
    }
    if card.expanded {
        if !card.args.trim().is_empty() {
            body.push(small_meta("参数", c));
            body.push(
                col().w_full().p(space::SM).rounded(R_CONTROL).bg(c.bg).overflow_hidden().children((
                    text(neosonia_core::util::truncate_chars(&card.args, 1500))
                        .size(TextSize::Xs)
                        .mono()
                        .color(c.text_muted),
                )),
            );
        }
        if !card.output.trim().is_empty() {
            body.push(small_meta("输出", c));
            body.push(
                col().w_full().p(space::SM).rounded(R_CONTROL).bg(c.bg).overflow_hidden().children((
                    text(neosonia_core::util::truncate_chars(&card.output, 4000))
                        .size(TextSize::Xs)
                        .mono()
                        .color(c.text_muted),
                )),
            );
        }
    }

    // 截图类工具：直接把"看到了什么"缩略图显示在卡片里
    let thumb_el: Element<Msg> = if let Some((w, h, px)) = &card.thumb {
        let ratio = if *w > 0 { *h as f32 / *w as f32 } else { 0.6 };
        image_rgba8(*w, *h, px.clone())
            .w(176.0)
            .h((176.0 * ratio).clamp(60.0, 240.0))
            .rounded(R_CONTROL)
            .into()
    } else {
        col().h(0.0).into()
    };

    col()
        .w_full()
        .rounded(R_CARD)
        .bg(c.surface)
        .border(1.0, c.border_subtle)
        .overflow_hidden()
        .children((
            row()
                .w_full()
                .px(space::MD)
                .py(space::SM)
                .gap(space::SM)
                .items_center()
                .on_click(Msg::ToggleTool(card.call_id.clone()))
                .children(head),
            col()
                .w_full()
                .px(space::MD)
                .pb(space::MD)
                .gap(space::XS)
                .children((
                    thumb_el,
                    if body.is_empty() {
                        let empty: Element<Msg> = col().h(0.0).into();
                        empty
                    } else {
                        let inner: Element<Msg> =
                            col().w_full().gap(space::XS).children(body).into();
                        inner
                    },
                )),
        ))
        .into()
}

fn subagent_card(card: &crate::state::SubagentCard, c: &UiColors) -> Element<Msg> {
    let state_color = match card.state {
        ToolState::Running => c.warning,
        ToolState::Ok => c.success,
        ToolState::Failed => c.danger,
        ToolState::Cancelled => c.text_subtle,
    };
    let mut lines: Vec<Element<Msg>> = Vec::new();
    for line in card.activity.iter().rev().take(3).rev() {
        lines.push(text(neosonia_core::util::truncate_chars(line, 200)).size(TextSize::Xs).color(c.text_subtle).into());
    }
    let summary_el: Element<Msg> = if card.summary.is_empty() {
        col().h(0.0)
    } else {
        text(neosonia_core::util::truncate_chars(&card.summary, 500))
            .size(TextSize::Sm)
            .color(c.text)
            .into()
    };
    col()
        .w_full()
        .rounded(R_CARD)
        .bg(c.surface)
        .border(1.0, c.border_subtle)
        .p(space::MD)
        .gap(space::XS)
        .children((
            row()
                .w_full()
                .gap(space::SM)
                .items_center()
                .children((
                    status_dot(c, &card.state),
                    text("子代理").size(TextSize::Xs).color(c.text_subtle),
                    text(&card.label).size(TextSize::Sm).weight(Weight::Medium).color(c.text),
                    spacer(),
                    text(card.state.label()).size(TextSize::Xs).color(state_color),
                )),
            text(neosonia_core::util::truncate_chars(&card.task, 240))
                .size(TextSize::Xs)
                .color(c.text_muted),
            col().w_full().gap(0.0).children(lines),
            summary_el,
        ))
        .into()
}

fn notice_block(text_: &str, level: NoticeLevel, c: &UiColors) -> Element<Msg> {
    let color = match level {
        NoticeLevel::Info => c.text_subtle,
        NoticeLevel::Success => c.success,
        NoticeLevel::Warning => c.warning,
        NoticeLevel::Error => c.danger,
    };
    row()
        .w_full()
        .justify_center()
        .children((text(text_).size(TextSize::Xs).color(color),))
        .into()
}

// ============================================================
// 空态
// ============================================================

fn empty_state(c: &UiColors, personas: &[String]) -> Element<Msg> {
    let mut hints: Vec<Element<Msg>> = vec![
        text("NeoSonia").size(TextSize::Xl2).weight(Weight::Semibold).color(c.text).into(),
        text("常驻桌面的 agent，可以读写文件、执行命令、截屏、操作界面、收发邮件。")
            .size(TextSize::Sm)
            .color(c.text_muted)
            .into(),
    ];
    let sample = [
        "把 D 盘下载目录里上周的 PDF 按日期整理到 E 盘",
        "打开 Chrome，在搜索框里输入今天天气",
        "看一下屏幕上这个报错窗口，告诉我是哪个程序崩了",
        "给白名单里的张三回邮件，说明项目延期到周五",
    ];
    for s in sample {
        hints.push(
            row()
                .w_full()
                .p(space::MD)
                .rounded(R_CARD)
                .bg(c.surface)
                .border(1.0, c.border_subtle)
                .children((text(s).size(TextSize::Sm).color(c.text_muted),)),
        );
    }
    if !personas.is_empty() {
        hints.push(small_meta(format!("人设: {}", personas.join(" / ")), c));
    }
    col()
        .w_full()
        .p(space::XL)
        .gap(space::MD)
        .items_start()
        .justify_center()
        .children(hints)
        .into()
}

// ============================================================
// 对话主视图
// ============================================================

pub fn view(st: &AppState, c: &UiColors) -> Element<Msg> {
    // 只有"真正的对话内容"为空时才显示引导；notice 不算。
    let has_conversation = st
        .items
        .iter()
        .any(|i| matches!(i, ChatItem::User { .. } | ChatItem::Assistant { .. }));

    let scroll = col()
        .w_full()
        .grow()
        .min_w(0.0)
        .scroll_y()
        .id("chat-scroll")
        .stick_to_bottom()
        .p(space::LG)
        .gap(space::MD)
        .on_file_drop(|p| Msg::AttachmentsPicked(vec![p.to_path_buf()]));

    if !has_conversation {
        let empty = empty_state(
            c,
            &st.personas.iter().map(|p| p.name.clone()).collect::<Vec<_>>(),
        );
        return scroll.children((empty,)).into();
    }

    // 性能关键：虚拟列表。只有视口内的消息会被构建（布局 + 文本排版），
    // 长会话（几十上百条）滚动也不会掉帧。
    // 快照只在 items 变化时重建（否则每帧克隆 80 条消息的文本太浪费）。
    //
    // ⚠️ 契约：**任何修改 `st.items` 的 UI 交互都必须在 app.rs 里
    // `st.items_version += 1`**（例如 ToggleReasoning / ToggleTool）。
    // 否则这里会继续复用旧 `Rc<Vec<ChatItem>>` 快照，桌面端点"思考过程/详情"
    // 完全没反应（远程 web 是另一套渲染所以看起来正常）。守护测试：
    // `toggle_handlers_bump_items_version...` 与
    // `chat_snapshot_rebuilds_only_after_items_version_bump`。
    let snapshot = {
        let mut slot = st.chat_snapshot.borrow_mut();
        match slot.as_ref() {
            Some((ver, rc)) if *ver == st.items_version => rc.clone(),
            _ => {
                let rc = std::rc::Rc::new(st.items.clone());
                *slot = Some((st.items_version, rc.clone()));
                rc
            }
        }
    };
    let colors = *c;
    let roots = PathRoots::from_state(st);
    let count = snapshot.len();
    let builder_snapshot = snapshot.clone();
    scroll
        .virtual_rows_variable(count, 96.0, move |i| {
            row_item(&builder_snapshot, i, &colors, &roots)
        })
        .into()
}

/// 构建单条消息（供虚拟列表的 builder 调用）。不做进场动画：
/// 虚拟行会随滚动反复进出，动画反而会闪。
fn row_item(items: &[ChatItem], idx: usize, c: &UiColors, roots: &PathRoots) -> Element<Msg> {
    let Some(item) = items.get(idx) else {
        return col().h(0.0).into();
    };
    let el: Element<Msg> = match item {
        ChatItem::User { text: t, thumbs } => user_bubble(t, thumbs, c),
        ChatItem::Assistant {
            text: t,
            reasoning,
            reasoning_open,
            streaming,
            error,
        } => assistant_block(
            t,
            reasoning,
            *reasoning_open,
            *streaming,
            error.as_deref(),
            idx,
            c,
            roots,
        ),
        ChatItem::Tool(card) => tool_card(card, c),
        ChatItem::Subagent(card) => subagent_card(card, c),
        ChatItem::Notice { text: t, level } => notice_block(t, *level, c),
        ChatItem::Delivered {
            files,
            links,
            caption,
        } => delivered_block(files, links, caption, c, roots),
    };
    el.id(&format!("msg-{idx}"))
}
// ============================================================
// 输入区
// ============================================================

pub fn composer(st: &AppState, c: &UiColors) -> Element<Msg> {
    let busy = st.status.busy;

    // 输入框宽度跟着面板宽度走，避免把"发送"按钮挤出窗口。
    let input_w = (st.settings.ui.width - 24.0 - 8.0 - 92.0).clamp(160.0, 600.0) as f32;

    let mut input: Element<Msg> = text_area(&st.composer)
        .placeholder(if busy { "正在执行，可继续输入" } else { "输入指令，回车发送（Shift+Enter 换行）" })
        .width(input_w)
        .min_height({
            let lines = st.composer.lines().count().max(1);
            (44.0 + (lines.saturating_sub(1)) as f32 * 18.0).clamp(44.0, 220.0)
        })
        .on_input(Msg::ComposerChanged)
        .id("composer")
        .into();
    // 不再 autofocus：编辑框聚焦时 fenestra 的光标闪烁会让 frame.animating 永久为真，
    // 整个窗口按最大帧率不停重绘（用户看到"一直闪动"，同时白耗 CPU）。
    // 需要输入时点一下输入框即可获得焦点。
    input = input
        .on_key(|k| {
        if k.key == Key::Enter {
            Some(Msg::Submit)
        } else {
            None
        }
    });

    let hint_left = if busy {
        let secs = (neosonia_core::util::now_ms() - st.status.turn_started_ms).max(0) / 1000;
        format!("执行中 {}:{:02}", secs / 60, secs % 60)
    } else {
        st.status.phase.clone()
    };

    let right_button: Element<Msg> = if busy {
        button("停止").size(ControlSize::Sm).variant(ButtonVariant::Danger).on_click(Msg::CancelTurn).into()
    } else {
        button("发送").size(ControlSize::Sm).on_click(Msg::Submit).into()
    };

    let tokens_el: Element<Msg> = if st.status.tokens_in + st.status.tokens_out > 0 {
        text(format!(
            "{} / {} tok",
            st.status.tokens_in, st.status.tokens_out
        ))
        .size(TextSize::Xs)
        .color(c.text_subtle)
        .into()
    } else {
        col().h(0.0).into()
    };

    // 附件条（仅在有附件时显示）
    let attach_el: Element<Msg> = if st.attachments.is_empty() {
        col().h(0.0).into()
    } else {
        crate::attachments::strip(st, c)
    };

    // 工具栏：附件 / 文件夹 / 截图
    let tools_row = row().w_full().gap(space::XS).items_center().children((
        icon_button(icons::lucide::upload())
            .size(ControlSize::Sm)
            .variant(ButtonVariant::Ghost)
            .label("添加文件")
            .on_click(Msg::AttachClick),
        icon_button(icons::lucide::folder())
            .size(ControlSize::Sm)
            .variant(ButtonVariant::Ghost)
            .label("添加文件夹")
            .on_click(Msg::AttachFolderClick),
        icon_button(icons::lucide::eye())
            .size(ControlSize::Sm)
            .variant(ButtonVariant::Ghost)
            .label("截图发给模型")
            .on_click(Msg::ScreenshotHotkey),

        spacer(),
        text(hint_left).size(TextSize::Xs).color(c.text_subtle),
        tokens_el,
        status_ball(st, c),
    ));

    col()
        .w_full()
        .p(space::MD)
        .gap(space::SM)
        .bg(c.bg)
        .border_top(1.0, c.border_subtle)
        .on_file_drop(|p| Msg::AttachmentsPicked(vec![p.to_path_buf()]))
        .children((
            attach_el,
            row()
                .w_full()
                .gap(space::SM)
                .items_center()
                .children((col().grow().children((input,)), right_button)),
            tools_row,
        ))
        .into()
}


/// 模型交付物：图片/视频预览（有本地文件就显示缩略图占位卡）+ 文件快捷方式 + 链接。
/// 图片/视频按扩展名给出预览卡（图片直接解码显示，视频用图标+文件名），
/// 每个文件都有「打开」与「打开路径」两个按钮，链接点击用默认浏览器打开。
fn delivered_block(
    files: &[crate::state::DeliverItem],
    links: &[String],
    caption: &str,
    c: &UiColors,
    roots: &PathRoots,
) -> Element<Msg> {
    let mut rows: Vec<Element<Msg>> = Vec::new();
    if !caption.is_empty() {
        rows.push(
            text(caption)
                .size(TextSize::Sm)
                .color(c.text_muted)
                .into(),
        );
    }
    for f in files {
        // 相对路径按 home/workspace/cwd 解析（deliver 工具要求绝对路径，但历史数据/模型输出
        // 可能有相对路径）；解析不到再回退原样。
        let resolved = roots
            .resolve(&f.path)
            .unwrap_or_else(|| std::path::PathBuf::from(&f.path));
        let ext = resolved
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        // .lnk / .url：文件快捷方式卡（图标 + 整行点击打开），不要当图片。
        if matches!(ext.as_str(), "lnk" | "url") {
            rows.push(file_row(
                &f.name,
                "快捷方式",
                &resolved.display().to_string(),
                c,
                true,
            ));
            continue;
        }
        let is_image = f.kind == "image";
        let is_video = f.kind == "video";
        let path_str = resolved.display().to_string();
        if is_image {
            if let Some((w, h, px)) = crate::app::load_png_pixels_public(&resolved, 320) {
                let scale = (140.0f32 / w.max(1) as f32).min(1.0);
                let disp_w = (w as f32 * scale).max(40.0);
                let disp_h = (h as f32 * scale).max(28.0);
                let path_click = path_str.clone();
                rows.push(
                    row()
                        .w_full()
                        .min_w(0.0)
                        .gap(space::XS)
                        .p(space::XS)
                        .rounded(R_CONTROL)
                        .bg(c.surface)
                        .border(1.0, c.border_subtle)
                        .items_center()
                        .on_click(Msg::OpenPath(path_click))
                        .children((
                            image_rgba8(w, h, px)
                                .w(disp_w)
                                .h(disp_h)
                                .rounded(6.0),
                            col()
                                .grow()
                                .min_w(0.0)
                                .gap(1.0)
                                .children((
                                    text(soft_wrap_breaks(&f.name))
                                        .size(TextSize::Sm)
                                        .color(c.text),
                                    text(format_bytes(f.size))
                                        .size(TextSize::Xs)
                                        .color(c.text_subtle),
                                    text("点击打开")
                                        .size(TextSize::Xs)
                                        .color(c.accent),
                                )),
                        ))
                        .into(),
                );
                continue;
            }
        }
        // 非图片/视频/快捷方式：统一文件卡（整行点击打开）
        let hint = if is_video { "视频" } else { "文件" };
        rows.push(file_row(&f.name, hint, &path_str, c, false));
    }
    for link in links {
        let l = link.clone();
        let display = shorten_url_display(link);
        rows.push(
            row()
                .w_full()
                .min_w(0.0)
                .gap(6.0)
                .p(space::XS)
                .rounded(R_CONTROL)
                .items_center()
                .children((
                    {
                        let accent = c.accent;
                        icons::lucide::by_name("link")
                            .map(move |e| {
                                e.w(14.0)
                                    .h(14.0)
                                    .themed(move |_t, s| s.color(accent))
                            })
                            .unwrap_or_else(|| col().w(0.0).into())
                    },
                    text(soft_wrap_breaks(&display))
                        .size(TextSize::Sm)
                        .color(c.accent),
                ))
                .on_click(Msg::OpenLink(l.clone()))
                .into(),
        );
    }
    col()
        .w_full()
        .gap(8.0)
        .p(10.0)
        .rounded(R_CARD)
        .bg(c.surface_raised)
        .border(1.0, c.border_subtle)
        .children((text("交付内容").size(TextSize::Xs).color(c.text_muted),))
        .children(rows)
        .into()
}

fn format_bytes(n: u64) -> String {
    if n >= 1024 * 1024 {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    } else if n >= 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{n} B")
    }
}

/// 把一段 markdown 按"表格块 / 非表格块"切开（表格 = 连续以 `|` 开头的行）。
fn split_table_chunks(body: &str) -> Vec<TableChunk> {
    struct ChunkAcc {
        text: String,
        is_table: bool,
    }
    let mut out: Vec<TableChunk> = Vec::new();
    let mut cur = ChunkAcc {
        text: String::new(),
        is_table: false,
    };
    for line in body.lines() {
        let is_table_line = {
            let t = line.trim_start();
            t.starts_with('|') && t.matches('|').count() >= 2
        };
        if is_table_line != cur.is_table && !cur.text.is_empty() {
            out.push(TableChunk {
                text: std::mem::take(&mut cur.text),
                is_table: cur.is_table,
            });
        }
        cur.is_table = is_table_line;
        cur.text.push_str(line);
        cur.text.push('\n');
    }
    if !cur.text.is_empty() {
        out.push(TableChunk {
            text: cur.text,
            is_table: cur.is_table,
        });
    }
    out
}

struct TableChunk {
    text: String,
    is_table: bool,
}

/// 在超长"无空白"片段里插入零宽空格，避免长消息/LaTeX 撑破右侧边界。
fn soft_wrap_breaks(text: &str) -> String {
    const MAX_RUN: usize = 24;
    let mut out = String::with_capacity(text.len() + text.len() / MAX_RUN);
    let mut run = 0usize;
    for ch in text.chars() {
        out.push(ch);
        if ch.is_whitespace() {
            run = 0;
            continue;
        }
        run += 1;
        if run >= MAX_RUN && ch != '\u{200B}' {
            out.push('\u{200B}');
            run = 0;
        }
    }
    out
}

/// 输入框右下角的模型 API 状态小球（绿/黄/橙/红；unknown/未配置时不显示）。
fn status_ball(st: &AppState, c: &UiColors) -> Element<Msg> {
    let Some((provider, indicator, label, url)) = &st.provider_status else {
        return col().h(0.0).into();
    };
    let color = match indicator.as_str() {
        "green" => c.success,
        "yellow" => c.warning,
        "orange" => Color::from_rgba8(255, 150, 60, 255),
        "red" => c.danger,
        _ => return col().h(0.0).into(),
    };
    row()
        .items_center()
        .gap(3.0)
        .label(format!("{provider} API：{label}（点击查看状态页）"))
        .on_click(Msg::OpenLink(url.clone()))
        .children((col().w(7.0).h(7.0).rounded_full().bg(color),))
        .into()
}
#[cfg(test)]
mod tests {
    use super::*;
    use neosonia_core::config::ThemeMode;

    fn colors() -> UiColors {
        let theme = crate::theme::nova_theme(ThemeMode::Dark, true, 1.0);
        crate::theme::colors(&theme)
    }

    #[test]
    fn split_code_blocks_recognizes_fences_and_language() {
        let body = "说明文字\n\n```rust\nlet x = 1;\nlet y = 2;\n```\n\n结尾";
        let chunks = split_code_blocks(body);
        assert_eq!(chunks.len(), 3, "文本/代码/文本");
        assert!(matches!(&chunks[0], BodyChunk::Text(t) if t.contains("说明文字")));
        match &chunks[1] {
            BodyChunk::Code { lang, code } => {
                assert_eq!(lang, "rust");
                assert_eq!(code, "let x = 1;\nlet y = 2;\n");
            }
            other => panic!("应识别为代码块: {other:?}"),
        }
        assert!(matches!(&chunks[2], BodyChunk::Text(t) if t.contains("结尾")));
    }

    #[test]
    fn split_code_blocks_handles_tilde_braces_and_unclosed() {
        // ~~~ 围栏 + ```{rust} 语言标注
        let chunks = split_code_blocks("```{rust}\nfn main() {}\n```");
        match &chunks[0] {
            BodyChunk::Code { lang, code } => {
                assert_eq!(lang, "rust");
                assert_eq!(code, "fn main() {}\n");
            }
            other => panic!("{other:?}"),
        }
        let chunks = split_code_blocks("~~~python\nprint(1)\n~~~");
        match &chunks[0] {
            BodyChunk::Code { lang, code } => {
                assert_eq!(lang, "python");
                assert_eq!(code, "print(1)\n");
            }
            other => panic!("{other:?}"),
        }
        // 未闭合：剩余内容都当代码
        let chunks = split_code_blocks("前\n```\nlet a = 1;\n");
        assert_eq!(chunks.len(), 2);
        assert!(matches!(&chunks[1], BodyChunk::Code { code, .. } if code.contains("let a = 1;")));
        // 行内反引号不是围栏
        let chunks = split_code_blocks("用 `code` 表示行内代码");
        assert!(matches!(&chunks[0], BodyChunk::Text(_)));
        assert!(!chunks.iter().any(|c| matches!(c, BodyChunk::Code { .. })));
    }

    #[test]
    fn code_block_strips_zero_width_and_builds() {
        let c = colors();
        // 模拟 soft_wrap_breaks：代码里插入零宽空格，渲染前应剔除
        let long = "abcdefghij".repeat(16);
        let with_zwsp = format!("let value = \"{}{long}\";", "\u{200B}");
        let _: Element<Msg> = code_block("rust", &with_zwsp, &c);
        let _: Element<Msg> = code_block("", "a\nb\nc", &c);
        let _: Element<Msg> = code_block("rust", "", &c);
        // 走完整 rich_body：代码块 + 说明文字都能构建
        let body = format!("说明\n```rust\n{}\n```\n结尾", long);
        let els: Vec<Element<Msg>> = rich_body(&body, &c, &PathRoots::for_tests());
        assert!(!els.is_empty());
    }

    /// 真机截图前的自检：headless 渲染代码块卡片（无 GPU 适配器时跳过）。
    #[test]
    fn code_block_headless_preview_png() {
        let c = colors();
        let theme = crate::theme::nova_theme(ThemeMode::Dark, true, 1.0);
        let long_line = format!("let value = \"{}\";", "abcdefghij".repeat(14));
        let body = format!("Below is code:\n\n```rust\n{}\nlet short = 1;\n```\n", long_line);
        let el: Element<Msg> = col()
            .w(440.0)
            .p(space::MD)
            .gap(space::XS)
            .children(rich_body(&body, &c, &PathRoots::for_tests()))
            .into();
        let img = match fenestra::shell::try_render_element(el, &theme, (480, 260)) {
            Ok(img) => img,
            Err(e) => {
                eprintln!("跳过 headless 预览（无 GPU 适配器）: {e}");
                return;
            }
        };
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../logs/ui/code-block-headless.png");
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        img.save(&path).expect("save code block preview png");
        eprintln!("code block preview: {}", path.display());
    }
}
#[cfg(test)]
mod ime_tests {
    use super::*;
    use fenestra::{Frame, FrameState, InputEvent, Key, KeyInput, build_frame, dispatch, prelude::*};
    use neosonia_core::config::ThemeMode;

    #[derive(Clone, Debug, PartialEq)]
    enum TestMsg {
        Changed(String),
    }

    fn composer_element(value: &str) -> Element<TestMsg> {
        text_area(value)
            .width(320.0)
            .on_input(TestMsg::Changed)
            .id("composer")
            .into()
    }

    fn pump(
        event: InputEvent,
        state: &mut FrameState,
        fonts: &mut Fonts,
        el: &Element<TestMsg>,
        frame: &Frame,
        value: &mut String,
        messages: &mut Vec<String>,
    ) {
        let d = dispatch(el, frame, state, fonts, event);
        for m in d.msgs {
            if let TestMsg::Changed(v) = m {
                messages.push(v.clone());
                *value = v;
            }
        }
    }

    /// app 层集成回归：真实 `text_area` 元素 + 帧 + IME 事件序列，
    /// 断言中文拼音组合期间 app 的 composer 值只收到"已提交文本"，
    /// 不会被拼音/预编辑污染（用户报的"光标每 N 个字母跳 N 格"根因）。
    #[test]
    fn ime_composition_never_pollutes_composer_value() {
        let theme = crate::theme::nova_theme(ThemeMode::Dark, true, 1.0);
        let mut fonts = Fonts::embedded();
        let mut state = FrameState::new();
        let mut value = "abc".to_string();
        let mut messages: Vec<String> = Vec::new();
        let mut el = composer_element(&value);
        let mut frame = build_frame(&el, &theme, &mut fonts, &mut state, (400.0, 160.0), 1.0);

        // 聚焦输入框，把 caret 移到中间（Home 后右移一格）。
        pump(InputEvent::Tab, &mut state, &mut fonts, &el, &frame, &mut value, &mut messages);
        pump(
            InputEvent::Key(KeyInput::plain(Key::Home)),
            &mut state,
            &mut fonts,
            &el,
            &frame,
            &mut value,
            &mut messages,
        );
        pump(
            InputEvent::Key(KeyInput::plain(Key::ArrowRight)),
            &mut state,
            &mut fonts,
            &el,
            &frame,
            &mut value,
            &mut messages,
        );
        assert!(messages.is_empty(), "移动 caret 不应触发 on_input");

        // 拼音组合 "ni"：预编辑期间不应产生任何 on_input。
        pump(
            InputEvent::ImePreedit {
                text: "ni".to_string(),
                cursor: Some((2, 2)),
            },
            &mut state,
            &mut fonts,
            &el,
            &frame,
            &mut value,
            &mut messages,
        );
        assert!(messages.is_empty(), "预编辑不应触发 on_input");

        // 组合期间漏进来的拼音 Text 事件：必须被忽略，不能写进 app 的值。
        for letter in ["n", "i"] {
            pump(
                InputEvent::Text(letter.to_string()),
                &mut state,
                &mut fonts,
                &el,
                &frame,
                &mut value,
                &mut messages,
            );
        }
        assert!(
            messages.is_empty(),
            "组合期间拼音 Text 事件不应触发 on_input（否则光标按字符数前移）"
        );
        assert_eq!(value, "abc", "app 的 composer 值不能被拼音污染");

        // 组合期间左右键移动的是预编辑光标，不应触发 on_input。
        pump(
            InputEvent::Key(KeyInput::plain(Key::ArrowLeft)),
            &mut state,
            &mut fonts,
            &el,
            &frame,
            &mut value,
            &mut messages,
        );
        assert!(messages.is_empty(), "组合中的方向键不应触发 on_input");

        // 提交：app 只应收到 "a你bc"。
        pump(
            InputEvent::ImeCommit("你".to_string()),
            &mut state,
            &mut fonts,
            &el,
            &frame,
            &mut value,
            &mut messages,
        );
        assert_eq!(messages, vec!["a你bc".to_string()], "提交应只写入已提交文本");
        assert_eq!(value, "a你bc");
    }
}
#[cfg(test)]
mod harness_tests {
    use super::*;
    use fenestra::prelude::*;
    use fenestra::shell::Harness;
    use neosonia_core::config::ThemeMode;

    #[derive(Clone, Debug)]
    enum HMsg {
        Noop,
    }

    struct CodeBlockApp {
        code: String,
    }

    impl App for CodeBlockApp {
        type Msg = HMsg;
        fn update(&mut self, _msg: HMsg) {}
        fn view(&self) -> Element<HMsg> {
            let c = crate::theme::colors(&crate::theme::nova_theme(ThemeMode::Dark, true, 1.0));
            col()
                .w(424.0)
                .p(space::MD)
                .gap(space::XS)
                .children((code_block::<HMsg>("rust", &self.code, &c),))
                .into()
        }
    }

    /// 用 fenestra 官方 Harness 驱动真实渲染 + 合成横向滚轮：
    /// 断言代码块在 `scroll_x` 容器里可以横向滚动（滚动前后像素明显变化），
    /// 并输出两张 PNG 供人工核对（语言标签、裁切/滚动条、不溢出）。
    #[test]
    fn code_block_scrolls_horizontally_in_harness() {
        let long_line = format!("let value = \"{}\";", "abcdefghij".repeat(14));
        let app = CodeBlockApp {
            code: format!("{long_line}\nlet short = 1;"),
        };
        let theme = crate::theme::nova_theme(ThemeMode::Dark, true, 1.0);
        let mut harness = Harness::new(app, theme, (424, 360));
        let before = harness.render();

        let code_query = by::label("代码块");
        harness.hover(&code_query);
        harness.wheel_xy(&code_query, -300.0, 0.0);
        harness.pump(200.0);
        let after = harness.render();

        let diff = before
            .pixels()
            .zip(after.pixels())
            .filter(|(a, b)| a != b)
            .count();
        assert!(
            diff > 500,
            "横向滚动后代码块区域应有明显像素变化（diff={diff}），说明没滚动起来"
        );

        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../logs/ui");
        std::fs::create_dir_all(&dir).ok();
        before
            .save(dir.join("code-block-scroll-0.png"))
            .expect("save before png");
        after
            .save(dir.join("code-block-scroll-1.png"))
            .expect("save after png");
        eprintln!("code-block horizontal scroll diff={diff} px");
    }
}

#[cfg(test)]
mod v08_tests {
    use super::*;
    use fenestra::prelude::*;
    use neosonia_core::config::ThemeMode;

    fn ui_colors() -> UiColors {
        crate::theme::colors(&crate::theme::nova_theme(ThemeMode::Dark, true, 1.0))
    }

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("neosonia-v08-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create tmp home");
        dir
    }

    fn find_access<'a>(node: &'a fenestra::AccessNode, label: &str) -> Option<&'a fenestra::AccessNode> {
        if node.label.as_deref() == Some(label) {
            return Some(node);
        }
        node.children.iter().find_map(|child| find_access(child, label))
    }

    /// 面板级溢出检查：跳过"代码块"内部（scroll_x 内容本来就比视口宽，是内部滚动）。
    fn max_right_visible(node: &fenestra::AccessNode) -> f64 {
        if node.label.as_deref() == Some("代码块") {
            return node.rect.x0;
        }
        node.children
            .iter()
            .map(max_right_visible)
            .fold(node.rect.x1, f64::max)
    }

    fn build_panel(body: &str, roots: &PathRoots, height: f32) -> fenestra::Frame {
        let el: Element<Msg> = col()
            .w(424.0)
            .min_w(0.0)
            .p(space::MD)
            .gap(space::SM)
            .children(rich_body(body, &ui_colors(), roots))
            .into();
        let theme = crate::theme::nova_theme(ThemeMode::Dark, true, 1.0);
        let mut fonts = Fonts::embedded();
        let mut state = FrameState::new();
        fenestra::build_frame(&el, &theme, &mut fonts, &mut state, (424.0, height), 1.0)
    }

    /// 1. 代码块卡片宽度 = 父容器可用宽度（不是长行内容宽度），且不超出 424 面板。
    #[test]
    fn code_block_card_fits_available_width() {
        let long_line = format!("let very_long = \"{}\";", "abcdefghij".repeat(14));
        let body = format!("说明文字\n\n```rust\n{long_line}\nlet short = 1;\n```\n");
        let frame = build_panel(&body, &PathRoots::for_tests(), 700.0);
        let tree = frame.access_tree();
        let card = find_access(&tree, "代码块").expect("应能找到 label=代码块 的卡片");
        assert!(
            card.rect.width() <= 424.0 - 2.0 * 12.0 + 1.0,
            "代码块卡片宽度应等于可用宽度（424-24≈400），实际 {}（rect={:?}）",
            card.rect.width(),
            card.rect
        );
        assert!(card.rect.x0 >= -0.5, "卡片不应超出左侧：{:?}", card.rect);
        assert!(
            max_right_visible(&tree) <= 424.5,
            "面板内容（不含代码块内部滚动）不应超出 424 宽（max_right={}）",
            max_right_visible(&tree)
        );
    }

    /// 2. 有序列表 + 长 URL + markdown 链接：不溢出、不显示原始 `[文字](url)`。
    #[test]
    fn ordered_lists_and_long_links_fit_panel() {
        let long_url = format!(
            "https://example.com/very/long/path/with/many/segments/and/a/query?q={}",
            "abcdefghij".repeat(8)
        );
        let body = format!(
            "1. 第一项，后面跟长链接 {long_url}\n2. 第二项 [示例网站](https://example.com/docs)\n"
        );
        let frame = build_panel(&body, &PathRoots::for_tests(), 500.0);
        let tree = frame.access_tree();
        assert!(max_right_visible(&tree) <= 424.5, "列表/链接不应溢出 424（max_right={}）", max_right_visible(&tree));
    }

    /// 3. markdown 断行预处理：长 token 插零宽空格；链接目标 `](url)` 保持原样；
    ///    裸长 URL 转成可点击的 `[短显示](完整 URL)`。
    #[test]
    fn wrap_markdown_keeps_link_target_intact() {
        let link = "[文字](https://example.com/a/very/long/path?x=1)";
        let wrapped = wrap_markdown_text(link);
        assert!(
            wrapped.contains("](https://example.com/a/very/long/path?x=1)"),
            "链接目标不能被插零宽空格：{wrapped}"
        );
        // 超长 markdown 链接文字会缩短显示（保留完整目标），避免 shrink0 片段溢出
        let long_link = "[这是一个非常长的链接显示文字超过十八个字符](https://example.com/x)";
        let wrapped = wrap_markdown_text(long_link);
        assert!(wrapped.contains("](https://example.com/x)"), "{wrapped}");
        assert!(wrapped.contains('…'), "超长链接文字应缩短：{wrapped}");
        // 裸长 URL 保持普通文本 + 零宽空格折行（不转 markdown 链接片段，避免不可断行溢出）
        let long_url = format!("https://example.com/{}", "abcdefghij".repeat(8));
        let wrapped = wrap_markdown_text(&format!("前 {long_url} 后"));
        assert!(
            !wrapped.contains("]("),
            "裸 URL 不应被转成 markdown 链接片段：{wrapped}"
        );
        assert!(wrapped.contains('\u{200B}'), "裸长 URL 应插零宽空格折行：{wrapped}");
        // 超长普通 token 同样插零宽空格
        let wrapped = wrap_markdown_text(&"abcdefghij".repeat(8));
        assert!(wrapped.contains('\u{200B}'), "超长普通 token 应插零宽空格：{wrapped}");
        // 有序列表标记保持原样
        let wrapped = wrap_markdown_text("1. 第一项");
        assert!(wrapped.starts_with("1. 第一项"), "{wrapped}");
    }

    /// 4. 本地图片 / .lnk 快捷方式识别为自定义媒体段，走本地路径解析。
    #[test]
    fn local_media_split_recognizes_images_and_shortcuts() {
        let home = tmp_home("media");
        let img = home.join("play-test.png");
        image::RgbaImage::from_pixel(64, 40, image::Rgba([200, 120, 60, 255]))
            .save(&img)
            .expect("save test png");
        let lnk = home.join("快捷方式.lnk");
        std::fs::write(&lnk, b"shortcut").expect("write lnk");
        let mut roots = PathRoots::for_tests();
        roots.home = home.clone();

        let text = "看图 ![截图](play-test.png)，快捷方式 [打开](快捷方式.lnk)，外链 [站](https://example.com)。";
        let chunks = split_local_media(text, &roots);
        assert!(
            chunks
                .iter()
                .any(|c| matches!(c, MediaChunk::Image { raw, .. } if raw == "play-test.png")),
            "应识别本地图片：{chunks:?}"
        );
        assert!(
            chunks
                .iter()
                .any(|c| matches!(c, MediaChunk::LocalFile { raw, .. } if raw.ends_with(".lnk"))),
            "应识别 .lnk 本地文件：{chunks:?}"
        );
        // 外链保持文本（交给 markdown 渲染成可点击链接）
        assert!(
            chunks
                .iter()
                .any(|c| matches!(c, MediaChunk::Text(t) if t.contains("https://example.com"))),
            "外链应保持文本段：{chunks:?}"
        );
        // 图片路径按 home 解析成功、能读到像素（复用 load_png_pixels_public）
        let resolved = roots.resolve("play-test.png").expect("按 home 解析图片");
        assert!(crate::app::load_png_pixels_public(&resolved, 320).is_some());
    }

    /// 5. headless 预览：长代码块 + 有序列表 + 长链接 + 本地图片 + 快捷方式，
    ///    用系统字体渲染（中文可读），输出 PNG 供真机对照。
    #[test]
    fn chat_preview_png() {
        let home = tmp_home("preview");
        let img = home.join("play-test.png");
        image::RgbaImage::from_pixel(160, 100, image::Rgba([120, 170, 255, 255]))
            .save(&img)
            .expect("save preview png");
        std::fs::write(home.join("快捷方式.lnk"), b"shortcut").expect("write lnk");
        let mut roots = PathRoots::for_tests();
        roots.home = home;

        let long_url = format!(
            "https://example.com/really/long/path/that/keeps/going/and/going?q={}",
            "abcdefghij".repeat(6)
        );
        let long_line = format!("let very_long_line = \"{}\"; // 140 chars", "abcdefghij".repeat(14));
        let body = format!(
            "## 布局预览\n\n1. 第一项：有序列表 + 长链接 {long_url}\n2. 第二项：markdown 链接 [示例网站](https://example.com/docs/getting-started)\n\n![截图](play-test.png)\n\n[快捷方式](快捷方式.lnk)\n\n```rust\n{long_line}\nlet short = 1;\n```\n"
        );
        let el: Element<Msg> = col()
            .w(424.0)
            .min_w(0.0)
            .p(space::MD)
            .gap(space::SM)
            .children(rich_body(&body, &ui_colors(), &roots))
            .into();
        let theme = crate::theme::nova_theme(ThemeMode::Dark, true, 1.0);
        let mut fonts = Fonts::with_system();
        let img = match fenestra::shell::try_render_element_with(el, &theme, (424, 1100), &mut fonts) {
            Ok(img) => img,
            Err(e) => {
                eprintln!("跳过 headless 预览（无 GPU 适配器）: {e}");
                return;
            }
        };
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../logs/ui/chat-preview.png");
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        img.save(&path).expect("save chat preview png");
        eprintln!("chat preview: {}", path.display());
    }
    /// 6. 交付物预览：本地图片缩略图 + .lnk 快捷方式卡 + 链接。
    #[test]
    fn delivered_preview_png() {
        let home = tmp_home("delivered");
        let img = home.join("play-test.png");
        image::RgbaImage::from_pixel(320, 200, image::Rgba([90, 160, 240, 255]))
            .save(&img)
            .expect("save delivered png");
        let lnk = home.join("游戏快捷方式.lnk");
        std::fs::write(&lnk, b"shortcut").expect("write lnk");
        let mut roots = PathRoots::for_tests();
        roots.home = home.clone();
        let files = vec![
            crate::state::DeliverItem {
                path: img.display().to_string(),
                name: "play-test.png".to_string(),
                kind: "image".to_string(),
                size: 128_000,
            },
            crate::state::DeliverItem {
                path: lnk.display().to_string(),
                name: "游戏快捷方式.lnk".to_string(),
                kind: "file".to_string(),
                size: 1024,
            },
        ];
        let links = vec!["https://example.com/docs/getting-started".to_string()];
        let el: Element<Msg> = col()
            .w(424.0)
            .min_w(0.0)
            .p(space::MD)
            .children((delivered_block(
                &files,
                &links,
                "测试交付物：截图 + 快捷方式",
                &ui_colors(),
                &roots,
            ),))
            .into();
        let theme = crate::theme::nova_theme(ThemeMode::Dark, true, 1.0);
        let mut fonts = Fonts::with_system();
        let img = match fenestra::shell::try_render_element_with(el, &theme, (424, 520), &mut fonts) {
            Ok(img) => img,
            Err(e) => {
                eprintln!("跳过交付物预览（无 GPU 适配器）: {e}");
                return;
            }
        };
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../logs/ui/delivered-preview.png");
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        img.save(&path).expect("save delivered preview png");
        eprintln!("delivered preview: {}", path.display());
    }
    /// 7. 跨行内图片的加粗：`**` 不能被图片切段拆成字面星号。
    #[test]
    fn bold_survives_inline_image_split() {
        let home = tmp_home("bold");
        let img = home.join("intro.png");
        image::RgbaImage::from_pixel(120, 80, image::Rgba([150, 200, 120, 255]))
            .save(&img)
            .expect("save bold png");
        let mut roots = PathRoots::for_tests();
        roots.home = home.clone();

        // 原始 bug 形态：** 跨在行内图片两侧
        let text = "**图 2 · 绝对路径（![alt](intro.png)）**";
        let mut chunks = split_local_media(text, &roots);
        assert!(
            chunks.iter().any(|c| matches!(c, MediaChunk::Image { raw, .. } if raw == "intro.png")),
            "应识别行内图片：{chunks:?}"
        );
        balance_bold_across_media(&mut chunks);
        for chunk in &chunks {
            if let MediaChunk::Text(t) = chunk {
                assert_eq!(
                    t.matches("**").count() % 2,
                    0,
                    "加粗星号必须成对（否则渲染出字面 **）：{t:?}"
                );
            }
        }
        let texts: Vec<String> = chunks
            .iter()
            .filter_map(|c| match c {
                MediaChunk::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert!(
            texts.iter().any(|t| t.starts_with("**") && t.contains("图 2")),
            "图片前的文字应加粗收尾：{texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.ends_with("**") && t.contains("）")),
            "图片后的文字应重新开加粗：{texts:?}"
        );

        // **前**![img]**后**：两段各自成对
        let mut chunks = split_local_media("**前**![img](intro.png)**后**", &roots);
        balance_bold_across_media(&mut chunks);
        let texts: Vec<String> = chunks
            .iter()
            .filter_map(|c| match c {
                MediaChunk::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert!(texts.iter().any(|t| t == "**前**"), "{texts:?}");
        assert!(texts.iter().any(|t| t == "**后**"), "{texts:?}");

        // 布局：图片 + 加粗文本不溢出面板
        let frame = build_panel("**图 2 · 绝对路径（![alt](intro.png)）**\n", &roots, 520.0);
        assert!(max_right_visible(&frame.access_tree()) <= 424.5);

        // 视觉验证：加粗段的"墨水量"应明显多于普通字（无 GPU 适配器时跳过）
        let theme = crate::theme::nova_theme(ThemeMode::Dark, true, 1.0);
        let mut fonts = Fonts::with_system();
        let ink = |text: &str, fonts: &mut Fonts| -> Option<usize> {
            let el: Element<Msg> = col()
                .w(300.0)
                .p(8.0)
                .children((markdown_element(text),))
                .into();
            let img = fenestra::shell::try_render_element_with(el, &theme, (320, 60), fonts).ok()?;
            let bg = img.get_pixel(2, 2).0;
            Some(img.pixels().filter(|px| px.0 != bg).count())
        };
        if let (Some(bold_ink), Some(plain_ink)) = (
            ink("**图 2 · 绝对路径（**", &mut fonts),
            ink("图 2 · 绝对路径（", &mut fonts),
        ) {
            assert!(
                bold_ink > plain_ink,
                "跨图片的加粗段应有更多墨水量：bold={bold_ink} plain={plain_ink}"
            );
        }
    }

    /// 8. blockquote 长行/长 URL/嵌套引用不溢出（自己排版：竖线 + 可换行列）。
    #[test]
    fn blockquotes_wrap_within_panel() {
        let long = "说明：图 1~3 为本地文件，能否显示取决于前端是否允许本地路径访问，以及文件是否真实存在（这一句故意写得很长用来触发引用块换行测试）。";
        let body = format!(
            "> {long}\n>\n> 链接引用：https://example.com/very/long/path/that/keeps/going/and/going?q={}\n>> 嵌套引用：{}\n",
            "abcdefghij".repeat(6),
            "嵌套内容".repeat(20)
        );
        let frame = build_panel(&body, &PathRoots::for_tests(), 1000.0);
        assert!(
            max_right_visible(&frame.access_tree()) <= 424.5,
            "引用块不应溢出 424（max_right={}）",
            max_right_visible(&frame.access_tree())
        );
        // 解析：多级 >、单级、普通文本
        assert_eq!(parse_quote_line(">> 嵌套").map(|(d, _)| d), Some(2));
        assert_eq!(parse_quote_line("> 单层").map(|(d, _)| d), Some(1));
        assert!(parse_quote_line("普通文本").is_none());
    }

    /// 9. headless 预览 v2：跨图片加粗 + 长引用 + 嵌套引用。
    #[test]
    fn chat_preview_v2_png() {
        let home = tmp_home("preview2");
        let img = home.join("intro.png");
        image::RgbaImage::from_pixel(120, 80, image::Rgba([150, 200, 120, 255]))
            .save(&img)
            .expect("save intro png");
        let mut roots = PathRoots::for_tests();
        roots.home = home;
        let long = "说明：图 1~3 为本地文件，能否显示取决于前端是否允许本地路径访问，以及文件是否真实存在（这一句故意写得很长用来触发引用块换行测试）。";
        let body = format!(
            "**图 2 · 绝对路径（![alt](intro.png)）**\n\n> {long}\n>\n> 链接引用：https://example.com/very/long/path/that/keeps/going/and/going?q={}\n>> 嵌套引用：{}\n",
            "abcdefghij".repeat(6),
            "嵌套内容".repeat(20)
        );
        let el: Element<Msg> = col()
            .w(424.0)
            .min_w(0.0)
            .p(space::MD)
            .gap(space::SM)
            .children(rich_body(&body, &ui_colors(), &roots))
            .into();
        let theme = crate::theme::nova_theme(ThemeMode::Dark, true, 1.0);
        let mut fonts = Fonts::with_system();
        let img = match fenestra::shell::try_render_element_with(el, &theme, (424, 760), &mut fonts) {
            Ok(img) => img,
            Err(e) => {
                eprintln!("跳过 v2 预览（无 GPU 适配器）: {e}");
                return;
            }
        };
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../logs/ui/chat-preview-v2.png");
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        img.save(&path).expect("save chat preview v2 png");
        eprintln!("chat preview v2: {}", path.display());
    }
    /// 10. 文件拖放路由：完整 ui::root 渲染后 dispatch(FileDrop) 必须触发
    /// `Msg::AttachmentsPicked`（OS 层兜底把 WM_DROPFILES 变成同一个 InputEvent）。
    #[test]
    fn file_drop_routes_to_attachments_picked() {
        let root_dir = tmp_home("drop");
        let paths = neosonia_core::config::Paths::from_root(root_dir.clone());
        let st = AppState::new(paths, neosonia_core::config::Settings::default());
        let c = ui_colors();
        let theme = crate::theme::nova_theme(ThemeMode::Dark, true, 1.0);
        let el: Element<Msg> = crate::ui::root(&st, &c);
        let mut fonts = Fonts::embedded();
        let mut state = FrameState::new();
        let frame = fenestra::build_frame(&el, &theme, &mut fonts, &mut state, (424.0, 760.0), 1.0);
        let drop_path = root_dir.join("dropped.png");
        std::fs::write(&drop_path, b"fake image bytes").expect("write dropped file");
        let dispatch = fenestra::dispatch(
            &el,
            &frame,
            &mut state,
            &mut fonts,
            InputEvent::FileDrop(drop_path.clone()),
        );
        assert!(
            dispatch
                .msgs
                .iter()
                .any(|m| matches!(m, Msg::AttachmentsPicked(files) if files.iter().any(|p| p == &drop_path))),
            "FileDrop 应路由到 Msg::AttachmentsPicked（composer/root 的 on_file_drop）"
        );
    }
    /// 守护：Desktop 端"思考过程/工具详情"能否展开，依赖 app.rs 的 handler
    /// 在改完 `items` 后 `items_version += 1`（chat 快照缓存靠它失效）。
    #[test]
    fn toggle_handlers_bump_items_version_and_flip_state() {
        let paths = neosonia_core::config::Paths::from_root(tmp_home("toggle"));
        let mut app = crate::app::NeoSoniaApp::new(paths, neosonia_core::config::Settings::default());
        app.st.items.push(ChatItem::Assistant {
            text: "答案".to_string(),
            reasoning: "思考内容".to_string(),
            reasoning_open: false,
            streaming: false,
            error: None,
        });
        app.st.items.push(ChatItem::Tool(crate::state::ToolCard {
            call_id: "call-1".to_string(),
            name: "read".to_string(),
            args: "{}".to_string(),
            preview: None,
            state: crate::state::ToolState::Ok,
            output: "out".to_string(),
            elapsed_ms: 3,
            expanded: false,
            thumb: None,
        }));

        let v0 = app.st.items_version;
        App::update(&mut app, Msg::ToggleReasoning(0));
        assert_eq!(
            app.st.items_version,
            v0 + 1,
            "ToggleReasoning 改 items 后必须递增 items_version，否则 chat 快照不重建"
        );
        assert!(
            matches!(&app.st.items[0], ChatItem::Assistant { reasoning_open, .. } if *reasoning_open),
            "思考过程应展开"
        );

        App::update(&mut app, Msg::ToggleReasoning(0));
        assert_eq!(app.st.items_version, v0 + 2, "再次切换也必须递增");
        assert!(
            matches!(&app.st.items[0], ChatItem::Assistant { reasoning_open, .. } if !*reasoning_open),
            "思考过程应收起"
        );

        let v1 = app.st.items_version;
        App::update(&mut app, Msg::ToggleTool("call-1".to_string()));
        assert_eq!(
            app.st.items_version,
            v1 + 1,
            "ToggleTool 改 items 后必须递增 items_version"
        );
        assert!(
            app.st.tool_card_mut("call-1").is_some_and(|card| card.expanded),
            "工具详情应展开"
        );

        App::update(&mut app, Msg::ToggleTool("call-1".to_string()));
        assert_eq!(app.st.items_version, v1 + 2);
        assert!(
            app.st.tool_card_mut("call-1").is_some_and(|card| !card.expanded),
            "工具详情应收起"
        );
    }

    /// 守护：chat 快照缓存必须只在 `items_version` 变化时重建。
    /// - 直接改 `items` 但不 bump → 复用的旧快照，渲染不变（正是 bug 的机制）；
    /// - bump 之后 → 重建，展开状态在像素上可见变化。
    #[test]
    fn chat_snapshot_rebuilds_only_after_items_version_bump() {
        let paths = neosonia_core::config::Paths::from_root(tmp_home("snapshot"));
        let mut st = AppState::new(paths, neosonia_core::config::Settings::default());
        st.items.push(ChatItem::Assistant {
            text: "答案".to_string(),
            reasoning: "某段思考内容".to_string(),
            reasoning_open: false,
            streaming: false,
            error: None,
        });
        let c = ui_colors();
        let theme = crate::theme::nova_theme(ThemeMode::Dark, true, 1.0);
        let render = |st: &AppState| -> Option<Vec<u8>> {
            let el: Element<Msg> = crate::chat::view(st, &c);
            fenestra::shell::try_render_element(el, &theme, (424, 760))
                .ok()
                .map(|img| img.into_raw())
        };

        let Some(before) = render(&st) else {
            eprintln!("跳过快照缓存测试（无 headless GPU）");
            return;
        };
        // 模拟"漏掉版本号"的旧 bug：只改状态，不 bump
        if let ChatItem::Assistant { reasoning_open, .. } = &mut st.items[0] {
            *reasoning_open = true;
        }
        let Some(stale) = render(&st) else { return };
        assert_eq!(
            before, stale,
            "items_version 未变时应复用旧快照（这正是桌面端点开没反应的机制）"
        );

        st.items_version += 1;
        let Some(after) = render(&st) else { return };
        assert_ne!(
            before, after,
            "items_version 递增后快照必须重建，思考过程展开状态应可见变化"
        );
    }
}