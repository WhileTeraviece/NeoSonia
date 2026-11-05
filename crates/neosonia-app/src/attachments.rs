//! 附件条：聊天输入框上方的横向 chip 列表。
//!
//! 只负责渲染与删除消息（`Msg::AttachmentRemove`），附件的选择/读取由 app 层
//! （`Msg::AttachClick` / `Msg::AttachFolderClick` / `Msg::AttachmentsPicked`）完成。

use fenestra::prelude::*;
use neosonia_core::Attachment;

use crate::state::{AppState, Msg};
use crate::theme::{space, UiColors, R_CHIP};

/// 缩略图边长（逻辑像素）。
const THUMB: f32 = 44.0;
/// 单张附件卡片的固定宽度：左侧 44 缩略图 + 中间两行文本 + 右侧独立 × 按钮。
const CARD_W: f32 = 236.0;
/// 只有像素量不超过这个值的图片才直接做缩略图。
///
/// `Attachment.pixels` 是整张解码后的 RGBA，`view` 每帧都会重建；如果原图
/// 是 4000x3000，直接 `clone()` 就是几十 MB。超过阈值时退化成类型图标，
/// 保证界面流畅（app 层会在读取附件时存一份缩略图）。
const MAX_THUMB_PIXELS: usize = 256 * 256;

/// 长文件名中间省略：保留前 `head` 字符 + "…" + 后 `tail` 字符（含扩展名）。
fn short_filename(name: &str, head: usize, tail: usize) -> String {
    let count = name.chars().count();
    if count <= head + tail + 1 {
        return name.to_string();
    }
    let head_part: String = name.chars().take(head).collect();
    let tail_part: String = name
        .chars()
        .rev()
        .take(tail)
        .collect::<Vec<char>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head_part}…{tail_part}")
}

/// 附件类型的中文标签。
fn kind_label(kind: neosonia_core::AttachmentKind) -> &'static str {
    use neosonia_core::AttachmentKind as K;
    match kind {
        K::Image => "图片",
        K::Text => "文本",
        K::File => "文件",
        K::Folder => "文件夹",
        K::Screenshot => "截图",
    }
}

/// 像素数据是否适合直接做缩略图（维度、长度都要合法）。
fn can_thumbnail(width: u32, height: u32, len: usize) -> bool {
    let Some(pixels) = (width as usize).checked_mul(height as usize) else {
        return false;
    };
    pixels > 0 && pixels <= MAX_THUMB_PIXELS && len >= pixels * 4
}

fn kind_icon(kind: neosonia_core::AttachmentKind, c: &UiColors) -> Element<Msg> {
    use neosonia_core::AttachmentKind as K;
    let icon: Element<Msg> = match kind {
        K::Folder => icons::lucide::folder().w(20.0).h(20.0).color(c.text_muted).into(),
        K::Screenshot => icons::lucide::eye().w(20.0).h(20.0).color(c.text_muted).into(),
        _ => icons::lucide::file().w(20.0).h(20.0).color(c.text_muted).into(),
    };
    row()
        .w(THUMB)
        .h(THUMB)
        .items_center()
        .justify_center()
        .bg(c.bg)
        .border(1.0, c.border_subtle)
        .rounded(6.0)
        .shrink0()
        .children((icon,))
}

fn chip(index: usize, att: &Attachment, c: &UiColors) -> Element<Msg> {
    let preview: Element<Msg> = match &att.pixels {
        Some((w, h, pixels)) if can_thumbnail(*w, *h, pixels.len()) => {
            image_rgba8(*w, *h, pixels.clone())
                .w(THUMB)
                .h(THUMB)
                .rounded(6.0)
        }
        _ => kind_icon(att.kind, c),
    };
    let display_name = short_filename(&att.name, 12, 8);

    row()
        .w(CARD_W)
        .h(60.0)
        .items_center()
        .gap(space::SM)
        .p(space::SM)
        .bg(c.surface_raised)
        .border(1.0, c.border_subtle)
        .rounded(R_CHIP)
        .shrink0()
        .id(&format!("attachment-{index}"))
        .children((
            preview,
            // 中间文本列可收缩：文件名中间省略，第二行固定“类型 · 大小”。
            col().grow().min_w(0.0).gap(2.0).children((
                text(display_name)
                    .size(TextSize::Xs)
                    .color(c.text)
                    .truncate(),
                text(format!("{} · {}", kind_label(att.kind), att.human_size()))
                    .size(TextSize::Xs)
                    .tabular()
                    .color(c.text_subtle)
                    .truncate(),
            )),
            // 独立的 × 按钮在右侧，不再绝对定位盖在缩略图上。
            icon_button(icons::x())
                .size(ControlSize::Xs)
                .label(format!("移除附件 {}", att.name))
                .on_click(Msg::AttachmentRemove(index))
                .id(&format!("attachment-remove-{index}")),
        ))
}

/// 输入框上方的附件条；没有附件时收缩成 0 高度。
pub fn strip(st: &AppState, c: &UiColors) -> Element<Msg> {
    if st.attachments.is_empty() {
        return col().w_full().h(0.0).into();
    }
    let chips: Vec<Element<Msg>> = st
        .attachments
        .iter()
        .enumerate()
        .map(|(i, att)| chip(i, att, c))
        .collect();
    row()
        .w_full()
        .items_center()
        .gap(space::SM)
        .scroll_x()
        .id("attachments-strip")
        .children(chips)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_chinese() {
        use neosonia_core::AttachmentKind as K;
        assert_eq!(kind_label(K::Image), "图片");
        assert_eq!(kind_label(K::Text), "文本");
        assert_eq!(kind_label(K::File), "文件");
        assert_eq!(kind_label(K::Folder), "文件夹");
        assert_eq!(kind_label(K::Screenshot), "截图");
    }

    #[test]
    fn thumbnail_bounds_are_safe() {
        assert!(can_thumbnail(44, 44, 44 * 44 * 4));
        assert!(can_thumbnail(256, 256, 256 * 256 * 4));
        assert!(!can_thumbnail(257, 256, 257 * 256 * 4));
        assert!(!can_thumbnail(0, 40, 0));
        // 长度不足（像素被截断）时不做缩略图，交给类型图标。
        assert!(!can_thumbnail(40, 40, 40 * 40 * 4 - 1));
        // 超大尺寸不会溢出 usize（checked_mul 保护）。
        assert!(!can_thumbnail(u32::MAX, u32::MAX, 0));
    }

    #[test]
    fn long_filenames_collapse_in_the_middle() {
        // 短名原样返回，不动扩展名。
        assert_eq!(short_filename("a.png", 12, 8), "a.png");
        assert_eq!(short_filename("report.docx", 12, 8), "report.docx");
        // 长英文名保留头 12 + 尾 8（含扩展名）。
        let long = "2026-Q3-architecture-review-final-signed.pdf";
        let short = short_filename(long, 12, 8);
        assert!(short.contains('…'), "{short}");
        assert!(short.ends_with(".pdf"), "{short}");
        assert_eq!(short.chars().count(), 12 + 1 + 8);
        // 超长英文名（多段扩展名）。
        let huge = "a-very-long-english-filename-that-goes-on-and-on-archive.tar.gz";
        let huge_short = short_filename(huge, 12, 8);
        assert!(huge_short.contains('…'), "{huge_short}");
        assert!(huge_short.ends_with(".tar.gz"), "{huge_short}");
        // 中文长名同样按字符折叠，不破坏 UTF-8。
        let cn = "这是一个特别长的项目设计文档最终版本.pdf";
        let short_cn = short_filename(cn, 12, 8);
        assert!(short_cn.contains('…'), "{short_cn}");
        assert!(short_cn.ends_with("版本.pdf"), "{short_cn}");
        // emoji 文件名：按 Unicode 标量折叠，不能 panic 也不能折出半个字符。
        let emoji = format!("{}.png", "😀".repeat(30));
        let short_emoji = short_filename(&emoji, 12, 8);
        assert!(short_emoji.contains('…'), "{short_emoji}");
        assert!(short_emoji.ends_with(".png"), "{short_emoji}");
        assert!(short_emoji.is_char_boundary(short_emoji.len()));
        assert_eq!(short_emoji.chars().count(), 12 + 1 + 8);
        // 无扩展名：尾部省略，不 panic。
        let no_ext = "a-very-long-file-name-without-any-extension";
        let no_ext_short = short_filename(no_ext, 12, 8);
        assert!(no_ext_short.contains('…'), "{no_ext_short}");
        assert_eq!(no_ext_short.chars().count(), 12 + 1 + 8);
    }
}
