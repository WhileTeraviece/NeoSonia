//! MIME / HTML 解析，全部是纯函数，不碰网络，便于单测。
//!
//! - 邮件结构用 `mail-parser` 解析（它会处理 RFC2047 编码头、多段 MIME、附件、字符集）；
//! - `Message-ID` / `References` / `In-Reply-To` / `Date` 这几项额外用原始头提取，
//!   保证回复时能原样带回 `<...>` 形式的 id；
//! - HTML 正文用自带的 [`strip_html`] 转纯文本（没有 html2text 依赖）。

use mail_parser::MessageParser;

use crate::{MailFull, MailSummary, truncate_chars};

/// 摘要预览的正文截断长度（字节）。
pub const SNIPPET_CHARS: usize = 180;

/// 把列表用的「完整头 + 正文前 N 字节」拼成一封可解析的（截断）邮件。
///
/// 列表只下载 `BODY[HEADER]` + `BODY[TEXT]<0.4096>`，body 可能被截断，
/// 但对摘要（from/subject/date/snippet）足够；不要把结果用于完整正文。
pub fn compose_partial_message(header: &[u8], text: &[u8]) -> Vec<u8> {
    let mut raw = Vec::with_capacity(header.len() + text.len() + 4);
    raw.extend_from_slice(header);
    if !header.ends_with(b"\r\n\r\n") {
        raw.extend_from_slice(b"\r\n\r\n");
    }
    raw.extend_from_slice(text);
    raw
}

/// 把一封原始邮件解析成列表摘要。解析失败时不 panic，返回带占位内容的摘要。
pub fn parse_summary(uid: u32, raw: &[u8], unread: bool) -> MailSummary {
    let header = HeaderView::new(raw);
    let parser = MessageParser::default();
    let Some(msg) = parser.parse(raw) else {
        // 连头部都解析不出来：至少把原始头信息给出来，别让整个列表失败。
        let (from, from_name) = split_one_address(header.value("From").unwrap_or(""));
        return MailSummary {
            uid,
            from,
            from_name,
            subject: header.value("Subject").unwrap_or("(无法解析的邮件)").to_string(),
            date: header.value("Date").unwrap_or("").to_string(),
            snippet: String::new(),
            unread,
            has_attachments: false,
        };
    };

    let (from, from_name) = first_address(msg.from());
    let subject = clean_subject(msg.subject());
    let date = format_date(msg.date(), header.value("Date"));
    let body = message_body_text(&msg);
    let snippet = snippet_of(&body, SNIPPET_CHARS);
    let has_attachments = msg.attachment_count() > 0;

    MailSummary { uid, from, from_name, subject, date, snippet, unread, has_attachments }
}

/// 把一封原始邮件解析成完整邮件。
pub fn parse_full(uid: u32, raw: &[u8]) -> MailFull {
    let header = HeaderView::new(raw);
    let parser = MessageParser::default();
    let Some(msg) = parser.parse(raw) else {
        let (from, _) = split_one_address(header.value("From").unwrap_or(""));
        return MailFull {
            uid,
            from,
            to: split_loose(header.value("To").unwrap_or("")),
            cc: split_loose(header.value("Cc").unwrap_or("")),
            subject: header.value("Subject").unwrap_or("(无法解析的邮件)").to_string(),
            date: header.value("Date").unwrap_or("").to_string(),
            body_text: String::new(),
            has_attachments: false,
            message_id: first_message_id(header.value("Message-ID").unwrap_or("")),
            references: message_ids(header.value("References").unwrap_or("")),
        };
    };

    let (from, _) = first_address(msg.from());
    let subject = clean_subject(msg.subject());
    let date = format_date(msg.date(), header.value("Date"));
    let body_text = message_body_text(&msg);
    let message_id = {
        let from_header = first_message_id(header.value("Message-ID").unwrap_or(""));
        if from_header.is_empty() {
            first_message_id(msg.message_id().unwrap_or(""))
        } else {
            from_header
        }
    };
    // References 优先取原始头（保留 `<...>` 形式），没有再退回 mail-parser 的解析结果。
    let mut references = message_ids(header.value("References").unwrap_or(""));
    if references.is_empty() {
        references = message_ids(msg.header_raw("References").unwrap_or(""));
    }

    MailFull {
        uid,
        from,
        to: address_strings(msg.to()),
        cc: address_strings(msg.cc()),
        subject,
        date,
        body_text,
        has_attachments: msg.attachment_count() > 0,
        message_id,
        references,
    }
}

// ============================================================
// 正文 / 预览
// ============================================================

/// 取正文纯文本：优先 text/plain，回退 html（去标签）。
pub fn message_body_text(msg: &mail_parser::Message<'_>) -> String {
    if let Some(text) = msg.body_text(0) {
        let s = text.into_owned();
        if !s.trim().is_empty() {
            return s;
        }
    }
    if let Some(html) = msg.body_html(0) {
        let s = strip_html(&html);
        if !s.trim().is_empty() {
            return s;
        }
    }
    String::new()
}

/// 正文预览：压缩空白后截断。
pub fn snippet_of(text: &str, max_chars: usize) -> String {
    let mut compact = String::with_capacity(text.len().min(4096));
    let mut last_was_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !last_was_space && !compact.is_empty() {
                compact.push(' ');
            }
            last_was_space = true;
        } else {
            compact.push(ch);
            last_was_space = false;
            if compact.chars().count() > max_chars * 4 {
                break;
            }
        }
    }
    truncate_chars(compact.trim(), max_chars)
}

/// 极简 HTML 转纯文本：
/// 1. 先删 `<script>` / `<style>` / 注释；
/// 2. 块级标签换成换行，其它标签换成空格；
/// 3. 还原常见实体（`&amp;` `&nbsp;` `&#xxxx;` 等）；
/// 4. 压缩空白。
pub fn strip_html(html: &str) -> String {
    let lowered = html.to_ascii_lowercase();

    // 1) 删掉 script / style / 注释 区间。
    let mut without_blocks = String::with_capacity(html.len());
    let mut i = 0usize;
    while i < html.len() {
        if !html.is_char_boundary(i) {
            i += 1;
            continue;
        }
        let rest = &lowered[i..];
        let mut skip_to: Option<usize> = None;
        for (open, close) in [
            ("<script", "</script>"),
            ("<style", "</style>"),
            ("<!--", "-->"),
        ] {
            if rest.starts_with(open) {
                if let Some(pos) = lowered[i..].find(close) {
                    skip_to = Some(i + pos + close.len());
                } else {
                    skip_to = Some(html.len());
                }
                break;
            }
        }
        if let Some(end) = skip_to {
            i = end;
            without_blocks.push(' ');
            continue;
        }
        let ch = html[i..].chars().next().unwrap_or(' ');
        without_blocks.push(ch);
        i += ch.len_utf8();
    }

    // 2) 去标签，块级标签换行。
    let mut text = String::with_capacity(without_blocks.len());
    let mut in_tag = false;
    let mut tag = String::new();
    for ch in without_blocks.chars() {
        if in_tag {
            if ch == '>' {
                let name = tag
                    .trim_start_matches('/')
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let is_block = matches!(
                    name.as_str(),
                    "p" | "div"
                        | "br"
                        | "tr"
                        | "li"
                        | "ul"
                        | "ol"
                        | "table"
                        | "h1"
                        | "h2"
                        | "h3"
                        | "h4"
                        | "h5"
                        | "h6"
                        | "blockquote"
                        | "section"
                        | "article"
                        | "header"
                        | "footer"
                        | "pre"
                        | "hr"
                );
                text.push(if is_block { '\n' } else { ' ' });
                in_tag = false;
                tag.clear();
            } else {
                tag.push(ch);
            }
        } else if ch == '<' {
            in_tag = true;
        } else {
            text.push(ch);
        }
    }

    // 3) 还原实体。
    let text = decode_entities(&text);

    // 4) 压缩空白：行内多空格合一，连续空行最多两行。
    let mut out = String::with_capacity(text.len());
    let mut pending_newlines = 0usize;
    let mut last_space = false;
    for ch in text.chars() {
        match ch {
            '\r' => {}
            '\n' => {
                pending_newlines += 1;
                last_space = false;
            }
            c if c.is_whitespace() => {
                if !last_space && pending_newlines == 0 {
                    out.push(' ');
                }
                last_space = true;
            }
            c => {
                if pending_newlines > 0 {
                    let n = pending_newlines.min(2);
                    for _ in 0..n {
                        out.push('\n');
                    }
                    pending_newlines = 0;
                } else if last_space {
                    // 前面的空格已经压过一个，这里不再补
                }
                out.push(c);
                last_space = false;
            }
        }
    }
    // 去掉行首尾空白
    out.lines().map(str::trim).collect::<Vec<_>>().join("\n").trim().to_string()
}

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        let after = &rest[pos..];
        if let Some(semi) = after.find(';') {
            let entity = &after[1..semi];
            let decoded = match entity {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some(' '),
                _ => decode_numeric_entity(entity),
            };
            match decoded {
                Some(ch) => {
                    out.push(ch);
                    rest = &after[semi + 1..];
                    continue;
                }
                None => {
                    // 不是已知实体，原样保留
                    out.push('&');
                    rest = &after[1..];
                }
            }
        } else {
            out.push('&');
            rest = &after[1..];
        }
    }
    out.push_str(rest);
    out
}

fn decode_numeric_entity(entity: &str) -> Option<char> {
    let body = entity.strip_prefix('#')?;
    let code = if let Some(hex) = body.strip_prefix(['x', 'X']) {
        u32::from_str_radix(hex, 16).ok()?
    } else {
        body.parse::<u32>().ok()?
    };
    char::from_u32(code)
}

// ============================================================
// 地址 / 头字段
// ============================================================

fn first_address(addr: Option<&mail_parser::Address<'_>>) -> (String, String) {
    let Some(addr) = addr else {
        return (String::new(), String::new());
    };
    for item in addr.iter() {
        let email = item.address.as_deref().unwrap_or("").trim().to_string();
        let name = item.name.as_deref().unwrap_or("").trim().to_string();
        if !email.is_empty() {
            return (email, name);
        }
        if !name.is_empty() {
            return (String::new(), name);
        }
    }
    (String::new(), String::new())
}

fn address_strings(addr: Option<&mail_parser::Address<'_>>) -> Vec<String> {
    let Some(addr) = addr else {
        return Vec::new();
    };
    addr.iter()
        .filter_map(|item| {
            let email = item.address.as_deref().unwrap_or("").trim();
            if email.is_empty() {
                None
            } else {
                Some(email.to_string())
            }
        })
        .collect()
}

/// 只做最粗糙的地址拆分（解析失败时的兜底），不保证正确处理带逗号的显示名。
fn split_loose(raw: &str) -> Vec<String> {
    crate::split_addresses(raw)
        .into_iter()
        .map(|s| crate::address_of(&s))
        .filter(|s| !s.is_empty())
        .collect()
}

fn split_one_address(raw: &str) -> (String, String) {
    let first = crate::split_addresses(raw).into_iter().next().unwrap_or_default();
    (crate::address_of(&first), crate::display_name_of(&first))
}

/// 主题清洗：去掉折行，压缩空白。
pub fn clean_subject(raw: Option<&str>) -> String {
    let raw = raw.unwrap_or("");
    let mut out = String::with_capacity(raw.len());
    let mut last_space = false;
    for ch in raw.chars() {
        if ch.is_whitespace() {
            if !last_space && !out.is_empty() {
                out.push(' ');
            }
            last_space = true;
        } else {
            out.push(ch);
            last_space = false;
        }
    }
    let trimmed = out.trim();
    if trimmed.is_empty() {
        "(无主题)".to_string()
    } else {
        trimmed.to_string()
    }
}

fn format_date(dt: Option<&mail_parser::DateTime>, raw_header: Option<&str>) -> String {
    if let Some(d) = dt {
        return format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            d.year, d.month, d.day, d.hour, d.minute
        );
    }
    let raw = raw_header.unwrap_or("").trim();
    if raw.is_empty() { "(未知日期)".to_string() } else { raw.to_string() }
}

/// 提取该头字段里的所有 `<...>` 消息 id（保留尖括号）。
pub fn message_ids(raw_value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = raw_value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '<' {
            let mut id = String::from('<');
            let mut closed = false;
            for c in chars.by_ref() {
                id.push(c);
                if c == '>' {
                    closed = true;
                    break;
                }
                if id.len() > 512 {
                    break;
                }
            }
            if closed {
                out.push(id);
            }
        }
    }
    out
}

/// 取第一个消息 id；没有尖括号则返回去空白后的原文。
pub fn first_message_id(raw_value: &str) -> String {
    let ids = message_ids(raw_value);
    if let Some(id) = ids.into_iter().next() {
        return id;
    }
    raw_value.trim().to_string()
}

/// 原始头字段的只读视图：支持折行（continuation line），大小写不敏感。
///
/// 构造时一次性把头部解析成 `(小写字段名, 值)` 列表，之后 `value` 只是查表，
/// 不涉及生命周期技巧。
pub struct HeaderView {
    fields: Vec<(String, String)>,
}

impl HeaderView {
    pub fn new(raw: &[u8]) -> Self {
        let header_bytes = header_section(raw);
        let text = String::from_utf8_lossy(header_bytes);
        let mut fields: Vec<(String, String)> = Vec::new();
        let mut current_name: Option<String> = None;
        let mut current_value = String::new();
        let mut last_was_field = false;

        let flush = |name: Option<String>, value: String, fields: &mut Vec<(String, String)>| {
            if let Some(name) = name {
                fields.push((name.to_ascii_lowercase(), value));
            }
        };

        for line in text.lines() {
            let line = line.trim_end_matches('\r');
            if line.starts_with(' ') || line.starts_with('\t') {
                if last_was_field {
                    let extra = line.trim();
                    if !extra.is_empty() {
                        if !current_value.is_empty() {
                            current_value.push(' ');
                        }
                        current_value.push_str(extra);
                    }
                }
                continue;
            }
            let Some(colon) = line.find(':') else {
                continue;
            };
            // 结算上一个字段
            let name = current_name.take().or_else(|| {
                // 第一次进入循环时没有上一个字段
                None
            });
            flush(name, std::mem::take(&mut current_value), &mut fields);
            current_name = Some(line[..colon].trim().to_string());
            current_value = line[colon + 1..].trim().to_string();
            last_was_field = true;
        }
        flush(current_name.take(), current_value, &mut fields);
        Self { fields }
    }

    /// 取某个头字段的值（折行已拼成一行）；不存在返回 None。同名取第一个。
    pub fn value(&self, name: &str) -> Option<&str> {
        let key = name.to_ascii_lowercase();
        self.fields
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.as_str())
    }

    /// 所有头字段（小写名, 值）。
    pub fn all(&self) -> &[(String, String)] {
        &self.fields
    }
}

fn header_section(raw: &[u8]) -> &[u8] {
    // 找第一个空行（\r\n\r\n 或 \n\n）
    let mut i = 0usize;
    while i < raw.len() {
        if i + 3 < raw.len() && &raw[i..i + 4] == b"\r\n\r\n" {
            return &raw[..i];
        }
        if i + 1 < raw.len() && &raw[i..i + 2] == b"\n\n" {
            return &raw[..i];
        }
        i += 1;
    }
    raw
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_partial_message_supports_list_summary() {
        let header = b"From: Alice <alice@example.com>\r\nSubject: hello\r\nDate: Sat, 31 Oct 2026 19:51:18 +0800\r\n\r\n";
        let text = b"This is the beginning of the body...";
        let raw = compose_partial_message(header, text);
        let summary = parse_summary(7, &raw, true);
        assert_eq!(summary.uid, 7);
        assert_eq!(summary.from, "alice@example.com");
        assert_eq!(summary.subject, "hello");
        assert!(summary.snippet.contains("beginning"), "snippet: {}", summary.snippet);
        // 头本身已以空行结尾时不重复插入
        let raw2 = compose_partial_message(header, b"body");
        assert_eq!(&raw2[..header.len()], header);
    }

    fn sample_multipart() -> Vec<u8> {
        let raw = concat!(
            "From: =?UTF-8?B?5byg5LiJ?= <zhang@example.com>\r\n",
            "To: a@b.com, \"B\" <b@c.com>\r\n",
            "Cc: c@d.com\r\n",
            "Date: Mon, 02 Jun 2025 10:30:00 +0800\r\n",
            "Subject: =?UTF-8?B?5rWL6K+V5Li76aKY?=\r\n",
            "Message-ID: <abc123@example.com>\r\n",
            "References: <prev1@example.com> <prev2@example.com>\r\n",
            "In-Reply-To: <prev2@example.com>\r\n",
            "MIME-Version: 1.0\r\n",
            "Content-Type: multipart/mixed; boundary=\"BOUND\"\r\n",
            "\r\n",
            "--BOUND\r\n",
            "Content-Type: text/plain; charset=\"utf-8\"\r\n",
            "Content-Transfer-Encoding: 8bit\r\n",
            "\r\n",
            "你好，这是正文。\r\n",
            "--BOUND\r\n",
            "Content-Type: application/octet-stream; name=\"a.txt\"\r\n",
            "Content-Disposition: attachment; filename=\"a.txt\"\r\n",
            "Content-Transfer-Encoding: base64\r\n",
            "\r\n",
            "aGVsbG8=\r\n",
            "--BOUND--\r\n"
        );
        raw.as_bytes().to_vec()
    }

    #[test]
    fn parses_summary_with_encoded_headers() {
        let raw = sample_multipart();
        let s = parse_summary(42, &raw, true);
        assert_eq!(s.uid, 42);
        assert!(s.unread);
        assert_eq!(s.from, "zhang@example.com");
        assert_eq!(s.from_name, "张三");
        assert_eq!(s.subject, "测试主题");
        assert!(s.has_attachments);
        assert_eq!(s.date, "2025-06-02 10:30");
        assert!(s.snippet.contains("你好"));
        assert!(!s.snippet.contains('\n'));
    }

    #[test]
    fn parses_full_with_references_and_attachment() {
        let raw = sample_multipart();
        let f = parse_full(7, &raw);
        assert_eq!(f.uid, 7);
        assert_eq!(f.from, "zhang@example.com");
        assert_eq!(f.to, vec!["a@b.com".to_string(), "b@c.com".to_string()]);
        assert_eq!(f.cc, vec!["c@d.com".to_string()]);
        assert_eq!(f.subject, "测试主题");
        assert!(f.body_text.contains("你好，这是正文。"));
        assert!(f.has_attachments);
        assert_eq!(f.message_id, "<abc123@example.com>");
        assert_eq!(
            f.references,
            vec!["<prev1@example.com>".to_string(), "<prev2@example.com>".to_string()]
        );
    }

    #[test]
    fn html_body_falls_back_to_stripped_text() {
        let raw = concat!(
            "From: a@b.com\r\n",
            "Subject: html only\r\n",
            "MIME-Version: 1.0\r\n",
            "Content-Type: text/html; charset=\"utf-8\"\r\n",
            "\r\n",
            "<html><body><p>第一段</p><p>第二段 &amp; 更多</p></body></html>\r\n"
        );
        let f = parse_full(1, raw.as_bytes());
        assert!(f.body_text.contains("第一段"));
        assert!(f.body_text.contains("第二段"));
        assert!(f.body_text.contains("&"));
        assert!(!f.body_text.contains('<'));
    }

    #[test]
    fn strip_html_removes_script_and_style() {
        let html = "<style>body{color:red}</style><script>alert('x')</script><p>hello<br>world</p>";
        let text = strip_html(html);
        assert!(!text.contains("alert"));
        assert!(!text.contains("color:red"));
        assert!(text.contains("hello"));
        assert!(text.contains("world"));
    }

    #[test]
    fn strip_html_decodes_entities() {
        assert_eq!(strip_html("a &amp; b &lt; c &gt; d &quot;e&quot; &nbsp;f"), "a & b < c > d \"e\" f");
        assert_eq!(strip_html("&#20320;&#x597d;"), "你好");
        assert_eq!(strip_html("未知 &foo; 实体"), "未知 &foo; 实体");
    }

    #[test]
    fn snippet_is_compact() {
        let s = snippet_of("  第一行\n\n第二行\t第三行  ", 100);
        assert!(!s.contains('\n'));
        assert!(!s.contains('\t'));
        assert!(s.starts_with("第一行"));
    }

    #[test]
    fn message_ids_only_takes_angle_brackets() {
        let ids = message_ids("  <a@x> \r\n <b@y> garbage c@z ");
        assert_eq!(ids, vec!["<a@x>".to_string(), "<b@y>".to_string()]);
        assert_eq!(first_message_id("<only@x>"), "<only@x>");
        assert_eq!(first_message_id("plain@x"), "plain@x");
    }

    #[test]
    fn header_view_handles_folding() {
        let raw = b"From: a@b.com\r\nSubject: hello\r\n world\r\nX-Long: 1\r\n\r\nbody";
        let h = HeaderView::new(raw);
        assert_eq!(h.value("From"), Some("a@b.com"));
        assert_eq!(h.value("Subject"), Some("hello world"));
        assert_eq!(h.value("missing"), None);
    }
}
