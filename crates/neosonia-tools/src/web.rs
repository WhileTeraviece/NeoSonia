//! 网页工具：国内可用的聚合搜索（百度/搜狗/必应中国/360，可选 DDG/SearXNG）、
//! 抓取正文、提取链接、流式下载。
//!
//! 聚合搜索默认并行打多个引擎，按"跨引擎命中数 + rank 加权"合并排序并标注来源；
//! 单个引擎失败会静默跳过并在结果末尾统计（不 mock、不依赖 SearXNG）。
//! `render_js=true` 需要 Chromium 系应用开 `--remote-debugging-port`，
//! 本 crate 不内置浏览器，遇到会给出明确降级建议（用 cdp_* 工具或截图）。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use neosonia_core::config::{SearchProvider, Settings, WebSettings};
use neosonia_core::model::{ToolCtx, ToolOutcome};
use neosonia_core::registry::ToolRegistry;
use neosonia_core::util::{human_bytes, human_duration_ms, middle_truncate, truncate_chars};
use once_cell::sync::Lazy;
use regex::Regex;
use scraper::{Html, Selector};
use serde_json::{json, Value};
use url::Url;

use crate::{
    arg_bool, arg_str, arg_str_req, arg_u64, io_error_hint, resolve_write_path,
};

pub fn register(reg: &ToolRegistry) {
    reg.register(Arc::new(WebSearch));
    reg.register(Arc::new(WebFetch));
    reg.register(Arc::new(WebLinks));
    reg.register(Arc::new(DownloadFile));
}

// ============================================================
// HTTP 客户端与解码
// ============================================================

fn build_client(web: &WebSettings) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(web.timeout_secs.clamp(5, 300)))
        .user_agent(web.user_agent.clone())
        .redirect(reqwest::redirect::Policy::limited(10))
        .connect_timeout(Duration::from_secs(20));
    let proxy = web.proxy.trim();
    if !proxy.is_empty() {
        let p = reqwest::Proxy::all(proxy)
            .map_err(|e| format!("代理地址非法（{proxy}）：{e}"))?;
        builder = builder.proxy(p);
    }
    builder
        .build()
        .map_err(|e| format!("创建 HTTP 客户端失败：{e}"))
}

/// `Accept-Language` 头：默认给中文优先。
fn accept_language(lang: &str) -> String {
    let l = lang.trim();
    if l.is_empty() {
        "zh-CN,zh;q=0.9,en;q=0.8".to_string()
    } else {
        let base = l.split('-').next().unwrap_or(l);
        format!("{l},{base};q=0.9,en;q=0.7")
    }
}

static META_CHARSET: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?i)<meta[^>]+charset\s*=\s*["']?([A-Za-z0-9_\-]+)"#)
        .expect("静态正则 META_CHARSET 合法")
});

static TITLE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?is)<title[^>]*>(.*?)</title>").expect("静态正则 TITLE_RE 合法")
});

static SCRIPT_STYLE_RE: Lazy<Regex> = Lazy::new(|| {
    // 注意：Rust 的 regex crate 不支持反向引用（\1），所以用并列分支而不是 <(tag)>.*?</\1>。
    Regex::new(
        r"(?is)<script[^>]*>.*?</script>|<style[^>]*>.*?</style>|<noscript[^>]*>.*?</noscript>|<svg[^>]*>.*?</svg>",
    )
    .expect("静态正则 SCRIPT_STYLE_RE 合法")
});

fn header_charset(content_type: Option<&str>) -> Option<String> {
    let ct = content_type?;
    let lower = ct.to_ascii_lowercase();
    let pos = lower.find("charset=")?;
    let rest = &ct[pos + "charset=".len()..];
    let value = rest
        .split([';', ','])
        .next()
        .unwrap_or(rest)
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_ascii_lowercase())
    }
}

/// 解码响应体：BOM > HTTP charset > meta charset > UTF-8 探测 > GBK 兜底。
fn decode_body(bytes: &[u8], content_type: Option<&str>) -> (String, String) {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return (
            String::from_utf8_lossy(&bytes[3..]).into_owned(),
            "UTF-8 (BOM)".to_string(),
        );
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        return (String::from_utf16_lossy(&units), "UTF-16 LE".to_string());
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        return (String::from_utf16_lossy(&units), "UTF-16 BE".to_string());
    }
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(8192)]).into_owned();
    let label = header_charset(content_type).or_else(|| {
        META_CHARSET
            .captures(&head)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_ascii_lowercase())
    });
    if let Some(label) = label {
        if let Some(enc) = encoding_rs::Encoding::for_label(label.as_bytes()) {
            let (cow, _) = enc.decode_with_bom_removal(bytes);
            return (cow.into_owned(), enc.name().to_string());
        }
    }
    if let Ok(s) = std::str::from_utf8(bytes) {
        return (s.to_string(), "UTF-8".to_string());
    }
    let (cow, _, had_errors) = encoding_rs::GBK.decode(bytes);
    if !had_errors {
        (cow.into_owned(), "GBK".to_string())
    } else {
        (String::from_utf8_lossy(bytes).into_owned(), "UTF-8 (lossy)".to_string())
    }
}

fn looks_textual(content_type: Option<&str>) -> bool {
    let Some(ct) = content_type else {
        return true;
    };
    let ct = ct.to_ascii_lowercase();
    if ct.starts_with("text/") {
        return true;
    }
    [
        "application/json",
        "application/xml",
        "application/xhtml",
        "application/javascript",
        "application/rss",
        "application/atom",
        "application/ld+json",
        "image/svg",
    ]
    .iter()
    .any(|p| ct.contains(p))
}

fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn strip_tags(html: &str) -> String {
    let no_scripts = SCRIPT_STYLE_RE.replace_all(html, " ");
    let mut text = String::with_capacity(no_scripts.len());
    let mut in_tag = false;
    for ch in no_scripts.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                text.push(' ');
            }
            c if !in_tag => text.push(c),
            _ => {}
        }
    }
    collapse_whitespace(&text)
}

fn cleanup_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank_run = 0;
    for line in s.replace("\r\n", "\n").replace('\r', "\n").lines() {
        let trimmed = line.trim_end();
        if trimmed.trim().is_empty() {
            blank_run += 1;
            if blank_run <= 2 {
                out.push('\n');
            }
        } else {
            blank_run = 0;
            out.push_str(trimmed);
            out.push('\n');
        }
    }
    out.trim().to_string()
}

fn html_to_text(html: &str, width: usize) -> String {
    match html2text::config::plain().string_from_read(html.as_bytes(), width) {
        Ok(text) => cleanup_text(&text),
        Err(_) => cleanup_text(&strip_tags(html)),
    }
}

fn extract_title(html: &str) -> Option<String> {
    TITLE_RE
        .captures(html)
        .and_then(|c| c.get(1))
        .map(|m| collapse_whitespace(&strip_tags(m.as_str())))
        .filter(|t| !t.is_empty())
}

// ============================================================
// 搜索结果模型 / URL 规范化 / 各引擎 HTML 解析（纯函数，可单测）
// ============================================================

/// 单个引擎解析出来的一条原始结果。
#[derive(Clone, Debug, PartialEq, Eq)]
struct RawHit {
    title: String,
    url: String,
    snippet: String,
}

/// 多引擎合并后的一条结果。
#[derive(Clone, Debug)]
struct MergedHit {
    title: String,
    url: String,
    snippet: String,
    /// 命中该结果的引擎显示名（按首次出现顺序）。
    engines: Vec<&'static str>,
    /// 跨引擎加权分数：Σ(1/(rank+3)) + 0.5 × 命中引擎数。
    score: f64,
}

/// 内部/反爬跳转页地址：不算有效搜索结果。
fn is_internal_search_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    [
        "wappass.baidu.com",
        "antispider",
        "javascript:",
        "about:blank",
        // 搜狗的站内搜索/跳转页本身不是搜索结果
        "/web/searchlist.jsp",
        "m.sogou.com/web/",
        "m.sogou.com/from=",
    ]
    .iter()
    .any(|bad| lower.contains(bad))
}

/// 把一条结果加入列表；超出 limit 时返回 false 让调用方停止。
fn push_raw(hits: &mut Vec<RawHit>, title: &str, url: &str, snippet: &str, limit: usize) -> bool {
    let title = collapse_whitespace(title);
    let url = url.trim().to_string();
    if title.is_empty()
        || url.is_empty()
        || !(url.starts_with("http://") || url.starts_with("https://"))
        || is_internal_search_url(&url)
    {
        return hits.len() < limit;
    }
    let key = normalize_url_key(&url);
    if hits.iter().any(|h| normalize_url_key(&h.url) == key) {
        return hits.len() < limit;
    }
    hits.push(RawHit {
        title: truncate_chars(&title, 200),
        url,
        snippet: truncate_chars(&collapse_whitespace(snippet), 400),
    });
    hits.len() < limit
}

/// 追踪类参数：去重时忽略。
fn is_tracking_query_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.starts_with("utm_")
        || matches!(
            key.as_str(),
            "fbclid"
                | "gclid"
                | "msclkid"
                | "yclid"
                | "spm"
                | "vd_source"
                | "_hsenc"
                | "_hsmi"
                | "ref_src"
                | "ref_url"
        )
}

/// 归一化 URL，用于跨引擎去重：去 fragment、host 去 www.、去尾斜杠、去追踪参数、参数排序。
fn normalize_url_key(raw: &str) -> String {
    let raw = raw.trim();
    let Ok(mut parsed) = Url::parse(raw) else {
        return raw.trim_end_matches('/').to_ascii_lowercase();
    };
    parsed.set_fragment(None);
    let host = parsed
        .host_str()
        .unwrap_or("")
        .trim_start_matches("www.")
        .to_ascii_lowercase();
    let path = parsed.path().trim_end_matches('/').to_string();
    let mut params: Vec<(String, String)> = parsed
        .query_pairs()
        .filter(|(k, _)| !is_tracking_query_key(k))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    params.sort();
    if params.is_empty() {
        format!("{host}{path}")
    } else {
        let query = params
            .iter()
            .map(|(k, v)| {
                if v.is_empty() {
                    k.clone()
                } else {
                    format!("{k}={v}")
                }
            })
            .collect::<Vec<_>>()
            .join("&");
        format!("{host}{path}?{query}")
    }
}

/// 从跳转链接（搜狗/360/百度）的 query 里取出真实 URL。
/// 兼容绝对 URL、`//host/...`、`/path?...` 以及搜狗移动版这种 `./id=.../tc?...` 相对形式。
fn query_param_of(url: &str, key: &str) -> Option<String> {
    let absolute = if url.starts_with("//") {
        format!("https:{url}")
    } else if url.starts_with('/') {
        format!("https://example.invalid{url}")
    } else {
        url.to_string()
    };
    if let Ok(parsed) = Url::parse(&absolute) {
        if let Some(value) = parsed
            .query_pairs()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.into_owned())
            .filter(|v| v.starts_with("http://") || v.starts_with("https://"))
        {
            return Some(value);
        }
    }
    // 相对路径无法被 Url::parse 接受时，直接解析 query 串。
    let query = url.split_once('?')?.1;
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.into_owned())
        .filter(|v| v.starts_with("http://") || v.starts_with("https://"))
}

fn is_redirect_link(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.contains("baidu.com/link?url=")
        || lower.contains("sogou.com/link?url=")
        || lower.contains("/link?url=")
        || lower.contains("/link?m=")
}

fn looks_like_captcha_page(html: &str, final_url: &str) -> bool {
    final_url.contains("wappass")
        || html.contains("wappass.baidu.com/static/captcha")
        || html.contains("请输入验证码")
        || html.contains("antispider")
}

/// 百度移动版把摘要放在 SSR 的 `<!--s-data:{...}-->` 注释里，字段大致是 summaryData.generalLines[].data[].text。
static BAIDU_SUMMARY_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#""text":"((?:[^"\\]|\\.){5,600})""#).expect("静态正则 BAIDU_SUMMARY_RE 合法")
});

fn unescape_json_string(raw: &str) -> String {
    let quoted = format!("\"{raw}\"");
    serde_json::from_str::<String>(&quoted).unwrap_or_else(|_| raw.to_string())
}

fn extract_baidu_mobile_summary(inner_html: &str) -> String {
    for cap in BAIDU_SUMMARY_RE.captures_iter(inner_html) {
        let text = unescape_json_string(&cap[1]);
        let clean = collapse_whitespace(&strip_tags(&text));
        if clean.chars().count() >= 8 {
            return truncate_chars(&clean, 400);
        }
    }
    String::new()
}

// ---------------- 必应中国 ----------------

fn parse_bing_html(html: &str, limit: usize) -> Vec<RawHit> {
    let doc = Html::parse_document(html);
    let mut hits: Vec<RawHit> = Vec::new();
    let Ok(result_sel) = Selector::parse("li.b_algo") else {
        return hits;
    };
    let Ok(title_sel) = Selector::parse("h2 a") else {
        return hits;
    };
    let Ok(snippet_sel) = Selector::parse(".b_caption p, p") else {
        return hits;
    };
    for item in doc.select(&result_sel) {
        let Some(anchor) = item.select(&title_sel).next() else {
            continue;
        };
        let title = anchor.text().collect::<String>();
        let Some(href) = anchor.value().attr("href") else {
            continue;
        };
        // Bing 有时用 bing.com/ck/a 跳转；这种拿不到真实 URL，直接跳过。
        if href.contains("bing.com/ck/a") {
            continue;
        }
        let snippet = item
            .select(&snippet_sel)
            .next()
            .map(|e| e.text().collect::<String>())
            .unwrap_or_default();
        if !push_raw(&mut hits, &title, href, &snippet, limit) {
            break;
        }
    }
    hits
}

// ---------------- 360（so.com） ----------------

fn parse_so360_html(html: &str, limit: usize) -> Vec<RawHit> {
    let doc = Html::parse_document(html);
    let mut hits: Vec<RawHit> = Vec::new();
    let Ok(card_sel) = Selector::parse("li.res-list") else {
        return hits;
    };
    let Ok(title_sel) = Selector::parse("h3.res-title a, h3 a") else {
        return hits;
    };
    let Ok(snippet_sel) = Selector::parse("p.res-desc, .res-desc") else {
        return hits;
    };
    for card in doc.select(&card_sel) {
        let Some(anchor) = card.select(&title_sel).next() else {
            continue;
        };
        let title = anchor.text().collect::<String>();
        let url = anchor
            .value()
            .attr("data-mdurl")
            .map(str::to_string)
            .or_else(|| anchor.value().attr("href").map(str::to_string))
            .unwrap_or_default();
        let snippet = card
            .select(&snippet_sel)
            .next()
            .map(|e| e.text().collect::<String>())
            .unwrap_or_default();
        if !push_raw(&mut hits, &title, &url, &snippet, limit) {
            break;
        }
    }
    hits
}

// ---------------- 搜狗（移动版 + 桌面版） ----------------

fn parse_sogou_html(html: &str, limit: usize) -> Vec<RawHit> {
    let doc = Html::parse_document(html);
    let mut hits: Vec<RawHit> = Vec::new();
    let Ok(link_sel) = Selector::parse("a.resultLink") else {
        return hits;
    };
    let Ok(snippet_sel) = Selector::parse("a.txt-summary, .space-txt, .str-text, .str_info") else {
        return hits;
    };
    let Ok(container_sel) = Selector::parse("div.structure190319, div.reactResult, div.vrwrap") else {
        return hits;
    };
    // 优先按结果容器解析（能拿到摘要）
    for container in doc.select(&container_sel) {
        let Some(anchor) = container.select(&link_sel).next() else {
            continue;
        };
        let title = anchor.text().collect::<String>();
        let href = anchor.value().attr("href").unwrap_or("");
        let url = query_param_of(href, "url")
            .or_else(|| query_param_of(href, "pcurl"))
            .unwrap_or_else(|| href.to_string());
        let snippet = container
            .select(&snippet_sel)
            .next()
            .map(|e| e.text().collect::<String>())
            .unwrap_or_default();
        if !push_raw(&mut hits, &title, &url, &snippet, limit) {
            return hits;
        }
    }
    if !hits.is_empty() {
        return hits;
    }
    // 兜底：直接抓所有 resultLink（没有摘要）
    for anchor in doc.select(&link_sel) {
        let title = anchor.text().collect::<String>();
        let href = anchor.value().attr("href").unwrap_or("");
        let url = query_param_of(href, "url")
            .or_else(|| query_param_of(href, "pcurl"))
            .unwrap_or_else(|| href.to_string());
        if !push_raw(&mut hits, &title, &url, "", limit) {
            break;
        }
    }
    hits
}

// ---------------- 百度（桌面版 + 移动版） ----------------

fn parse_baidu_html(html: &str, limit: usize) -> Vec<RawHit> {
    let doc = Html::parse_document(html);
    let mut hits: Vec<RawHit> = Vec::new();
    let Ok(card_sel) = Selector::parse("div.result.c-container, div.c-container") else {
        return hits;
    };
    let Ok(title_sel) = Selector::parse("h3 a") else {
        return hits;
    };
    let Ok(snippet_sel) = Selector::parse(".c-abstract, .c-span-last, .content-right") else {
        return hits;
    };
    for card in doc.select(&card_sel) {
        let Some(anchor) = card.select(&title_sel).next() else {
            continue;
        };
        let title = anchor.text().collect::<String>();
        let Some(href) = anchor.value().attr("href") else {
            continue;
        };
        let snippet = card
            .select(&snippet_sel)
            .next()
            .map(|e| e.text().collect::<String>())
            .unwrap_or_default();
        if !push_raw(&mut hits, &title, href, &snippet, limit) {
            break;
        }
    }
    hits
}

/// 百度移动版（m.baidu.com）：结果卡片是 `div.c-result`，真实 URL 在 data-log 的 mu 字段，
/// 标题在 h3，摘要在 SSR 注释里。
fn parse_baidu_mobile_html(html: &str, limit: usize) -> Vec<RawHit> {
    let doc = Html::parse_document(html);
    let mut hits: Vec<RawHit> = Vec::new();
    let Ok(card_sel) = Selector::parse("div.c-result") else {
        return hits;
    };
    let Ok(title_sel) = Selector::parse("h3") else {
        return hits;
    };
    for card in doc.select(&card_sel) {
        let srcid = card.value().attr("srcid").unwrap_or("");
        if !srcid.is_empty() && srcid != "www_index" {
            continue;
        }
        let Some(data_log) = card.value().attr("data-log") else {
            continue;
        };
        let Ok(log) = serde_json::from_str::<Value>(data_log) else {
            continue;
        };
        let Some(url) = log.get("mu").and_then(|v| v.as_str()) else {
            continue;
        };
        let title = card
            .select(&title_sel)
            .next()
            .map(|e| e.text().collect::<String>())
            .unwrap_or_default();
        let snippet = extract_baidu_mobile_summary(&card.html());
        if !push_raw(&mut hits, &title, url, &snippet, limit) {
            break;
        }
    }
    hits
}

// ---------------- DuckDuckGo（国内常不可达，作为可选引擎） ----------------

fn unwrap_ddg_url(href: &str) -> String {
    let href = href.trim();
    if href.contains("duckduckgo.com/l/") {
        if let Some(target) = query_param_of(href, "uddg") {
            return target;
        }
    }
    if let Some(rest) = href.strip_prefix("//") {
        return format!("https://{rest}");
    }
    href.to_string()
}

fn parse_ddg_html(html: &str, limit: usize) -> Vec<RawHit> {
    let doc = Html::parse_document(html);
    let mut hits: Vec<RawHit> = Vec::new();
    let selectors = [
        ("div.result", "a.result__a"),
        ("div.web-result", "a.result__a"),
        ("div", "a.result__a"),
    ];
    for (container_css, link_css) in selectors {
        let Ok(container) = Selector::parse(container_css) else {
            continue;
        };
        let Ok(link_sel) = Selector::parse(link_css) else {
            continue;
        };
        let Ok(snippet_sel) = Selector::parse(".result__snippet") else {
            continue;
        };
        for item in doc.select(&container) {
            let Some(anchor) = item.select(&link_sel).next() else {
                continue;
            };
            let title = anchor.text().collect::<String>();
            let Some(href) = anchor.value().attr("href") else {
                continue;
            };
            let url = unwrap_ddg_url(href);
            let snippet = item
                .select(&snippet_sel)
                .next()
                .map(|e| e.text().collect::<String>())
                .unwrap_or_default();
            if !push_raw(&mut hits, &title, &url, &snippet, limit) {
                return hits;
            }
        }
        if !hits.is_empty() {
            return hits;
        }
    }
    hits
}

// ============================================================
// 搜索引擎：并行聚合 / 单引擎 / 合并排序
// ============================================================

/// 移动版 UA：搜狗/百度的移动页比桌面页更少反爬。
const MOBILE_UA: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) \
     AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Engine {
    Baidu,
    Sogou,
    BingCn,
    So360,
    DuckDuckGo,
    Searxng,
}

impl Engine {
    /// 默认聚合的国内引擎（不含国内常不可达的 DDG）。
    const AGGREGATED: [Engine; 4] = [Engine::Baidu, Engine::Sogou, Engine::BingCn, Engine::So360];

    fn name(self) -> &'static str {
        match self {
            Engine::Baidu => "百度",
            Engine::Sogou => "搜狗",
            Engine::BingCn => "必应",
            Engine::So360 => "360",
            Engine::DuckDuckGo => "DuckDuckGo",
            Engine::Searxng => "SearXNG",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Provider {
    Aggregated,
    Single(Engine),
    /// DeepSeek 官方 Search API（`/v1/tools/search`，需要 DeepSeek 的 key）。
    DeepSeek,
    None,
}

fn provider_from_arg(raw: &str) -> Result<Provider, String> {
    let lower = raw.trim().to_ascii_lowercase();
    Ok(match lower.as_str() {
        "" | "auto" | "aggregated" | "aggregate" | "all" => Provider::Aggregated,
        "baidu" | "baidu.com" => Provider::Single(Engine::Baidu),
        "sogou" | "sogou.com" => Provider::Single(Engine::Sogou),
        "bing" | "bingcn" | "cn_bing" | "bing_cn" => Provider::Single(Engine::BingCn),
        "360" | "so" | "so360" | "so.com" => Provider::Single(Engine::So360),
        "duckduckgo" | "ddg" => Provider::Single(Engine::DuckDuckGo),
        "searxng" | "searx" => Provider::Single(Engine::Searxng),
        "deepseek" | "ds" | "deepseek-search" | "deepseek_search" => Provider::DeepSeek,
        "none" | "off" => Provider::None,
        other => {
            return Err(format!(
                "不认识的 provider：{other:?}。可用 auto/aggregated、baidu、sogou、bing、360、duckduckgo、searxng、deepseek、none。"
            ))
        }
    })
}

fn provider_from_settings(settings: SearchProvider) -> Provider {
    match settings {
        SearchProvider::Aggregated => Provider::Aggregated,
        SearchProvider::Baidu => Provider::Single(Engine::Baidu),
        SearchProvider::Sogou => Provider::Single(Engine::Sogou),
        SearchProvider::Bing => Provider::Single(Engine::BingCn),
        SearchProvider::Duckduckgo => Provider::Single(Engine::DuckDuckGo),
        SearchProvider::Searxng => Provider::Single(Engine::Searxng),
        SearchProvider::DeepSeek => Provider::DeepSeek,
        SearchProvider::None => Provider::None,
    }
}

// ============================================================
// DeepSeek 官方 Search / Fetch API
// ------------------------------------------------------------
// 文档：https://api-docs.deepseek.com/guides/search
//   Search：POST {base}/v1/tools/search  {"query","count","need_content"}
//           响应 {"results":[{"rerank_score","title","url","snippet","summary",...}]}
//   Fetch ：POST {base}/v1/tools/fetch   {"url"}
//           响应 {"url","title","content"(Markdown),...}
// 鉴权与 chat 相同：Authorization: Bearer <api_key>。
// ============================================================

/// 归一化 base_url：去掉结尾的 `/v1`、`/chat/completions` 等后缀，得到 API 根地址。
fn deepseek_api_root(base_url: &str) -> String {
    let mut root = base_url.trim().trim_end_matches('/').to_string();
    loop {
        let before = root.clone();
        for suffix in ["/chat/completions", "/v1", "/beta"] {
            if let Some(stripped) = root.strip_suffix(suffix) {
                root = stripped.trim_end_matches('/').to_string();
            }
        }
        if root == before {
            break;
        }
    }
    root
}

fn deepseek_search_url(base_url: &str) -> String {
    let root = deepseek_api_root(base_url);
    let root = if root.is_empty() {
        "https://api.deepseek.com".to_string()
    } else {
        root
    };
    format!("{root}/v1/tools/search")
}

fn deepseek_fetch_url(base_url: &str) -> String {
    let root = deepseek_api_root(base_url);
    let root = if root.is_empty() {
        "https://api.deepseek.com".to_string()
    } else {
        root
    };
    format!("{root}/v1/tools/fetch")
}

/// 从 settings 里取 DeepSeek API key（也支持 `env:DEEPSEEK_API_KEY` 形式，由 core 展开）。
fn deepseek_key(settings: &Settings) -> Option<String> {
    let key = settings.server.api_key.trim();
    if key.is_empty() {
        None
    } else {
        Some(key.to_string())
    }
}

/// 把 DeepSeek 的错误响应整理成中文错误（兼容 OpenAI 风格 {"error":{"message":...}}）。
fn deepseek_error_message(status: u16, body: &str) -> String {
    let detail = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|payload| {
            payload
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(|message| message.as_str())
                .map(|message| message.to_string())
                .or_else(|| {
                    payload
                        .get("message")
                        .and_then(|message| message.as_str())
                        .map(|message| message.to_string())
                })
        })
        .unwrap_or_else(|| truncate_chars(body.trim(), 300));
    let hint = match status {
        402 => "余额不足",
        403 => "触发安全策略",
        404 => "资源不存在",
        408 => "请求超时（可重试）",
        429 => "请求过多（可重试）",
        s if s >= 500 => "服务端错误（可重试）",
        _ => "请求失败",
    };
    if detail.is_empty() {
        format!("HTTP {status}（{hint}）")
    } else {
        format!("HTTP {status}（{hint}）：{detail}")
    }
}

/// 带 Bearer 鉴权 POST 一段 JSON，返回解析后的响应。
async fn deepseek_post_json(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    body: Value,
) -> Result<Value, String> {
    let response = client
        .post(url)
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {api_key}"),
        )
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .json(&body)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("请求 DeepSeek API 失败：{e}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| format!("读取 DeepSeek 响应失败：{e}"))?;
    if !status.is_success() {
        return Err(deepseek_error_message(status.as_u16(), &text));
    }
    serde_json::from_str::<Value>(&text)
        .map_err(|e| format!("DeepSeek 响应不是合法 JSON：{e}；前 300 字符：{}", truncate_chars(&text, 300)))
}

/// 解析 `/v1/tools/search` 的响应：按 rerank_score 降序，摘要优先 summary。
fn parse_deepseek_search_results(payload: &Value, limit: usize) -> Result<Vec<RawHit>, String> {
    let results = payload
        .get("results")
        .and_then(|results| results.as_array())
        .ok_or_else(|| "DeepSeek 搜索响应缺少 results 数组。".to_string())?;
    let mut scored: Vec<(f64, RawHit)> = Vec::new();
    for item in results {
        let url = item
            .get("url")
            .and_then(|url| url.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if url.is_empty() || !(url.starts_with("http://") || url.starts_with("https://")) {
            continue;
        }
        let title = item
            .get("title")
            .and_then(|title| title.as_str())
            .map(|title| collapse_whitespace(title))
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| url.clone());
        let summary = item
            .get("summary")
            .and_then(|summary| summary.as_str())
            .unwrap_or("");
        let snippet = item
            .get("snippet")
            .and_then(|snippet| snippet.as_str())
            .unwrap_or("");
        let text = if !summary.trim().is_empty() {
            summary
        } else {
            snippet
        };
        let snippet = truncate_chars(&collapse_whitespace(text), 500);
        let score = item
            .get("rerank_score")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        scored.push((
            score,
            RawHit {
                title,
                url,
                snippet,
            },
        ));
    }
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    Ok(scored
        .into_iter()
        .take(limit.max(1))
        .map(|(_, hit)| hit)
        .collect())
}

/// 调 DeepSeek 官方搜索；失败时返回中文原因（由 web_search 决定是否回退聚合）。
async fn search_deepseek(
    client: &reqwest::Client,
    settings: &Settings,
    query: &str,
    count: usize,
) -> Result<Vec<RawHit>, String> {
    let api_key = deepseek_key(settings).ok_or_else(|| {
        "未配置 DeepSeek API key（settings.server.api_key 或环境变量 DEEPSEEK_API_KEY）。".to_string()
    })?;
    let url = deepseek_search_url(&settings.server.base_url);
    let body = json!({
        "query": truncate_chars(query, 128),
        "count": count.clamp(1, 50),
        "need_content": false
    });
    let payload = deepseek_post_json(client, &url, &api_key, body).await?;
    parse_deepseek_search_results(&payload, count)
}

/// DeepSeek Fetch API 返回的正文。
#[derive(Debug)]
struct DeepSeekFetched {
    url: String,
    title: Option<String>,
    content: String,
}

fn parse_deepseek_fetch(payload: &Value) -> Result<DeepSeekFetched, String> {
    let content = payload
        .get("content")
        .and_then(|content| content.as_str())
        .unwrap_or("")
        .to_string();
    if content.trim().is_empty() {
        return Err("DeepSeek Fetch 返回的正文为空。".to_string());
    }
    let url = payload
        .get("url")
        .and_then(|url| url.as_str())
        .unwrap_or("")
        .to_string();
    let title = payload
        .get("title")
        .and_then(|title| title.as_str())
        .map(|title| collapse_whitespace(title))
        .filter(|title| !title.is_empty());
    Ok(DeepSeekFetched {
        url,
        title,
        content,
    })
}

/// 用 DeepSeek Fetch API 抓正文（Markdown）。失败返回中文原因。
async fn fetch_deepseek_markdown(
    client: &reqwest::Client,
    settings: &Settings,
    url: &str,
) -> Result<DeepSeekFetched, String> {
    let api_key = deepseek_key(settings).ok_or_else(|| {
        "未配置 DeepSeek API key（settings.server.api_key 或环境变量 DEEPSEEK_API_KEY）。".to_string()
    })?;
    let endpoint = deepseek_fetch_url(&settings.server.base_url);
    let body = json!({ "url": url });
    let payload = deepseek_post_json(client, &endpoint, &api_key, body).await?;
    parse_deepseek_fetch(&payload)
}

fn encode_query(q: &str) -> String {
    url::form_urlencoded::byte_serialize(q.as_bytes()).collect()
}

fn ddg_kl(lang: &str) -> &'static str {
    if lang.to_ascii_lowercase().starts_with("zh") {
        "cn-zh"
    } else if lang.to_ascii_lowercase().starts_with("en") {
        "us-en"
    } else {
        "wt-wt"
    }
}

fn bing_setlang(lang: &str) -> &'static str {
    if lang.to_ascii_lowercase().starts_with("zh") {
        "zh-hans"
    } else if lang.to_ascii_lowercase().starts_with("en") {
        "en-us"
    } else {
        "zh-hans"
    }
}

/// 抓一个搜索页，统一加 UA/Accept-Language/Referer，按响应头/页面 meta 解码。
async fn fetch_search_html(
    client: &reqwest::Client,
    url: &str,
    referer: Option<&str>,
    ua_override: Option<&str>,
    lang: &str,
) -> Result<(String, String), String> {
    let mut request = client
        .get(url)
        .header(
            reqwest::header::ACCEPT,
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .header(reqwest::header::ACCEPT_LANGUAGE, accept_language(lang));
    if let Some(referer) = referer {
        request = request.header(reqwest::header::REFERER, referer);
    }
    if let Some(ua) = ua_override {
        request = request.header(reqwest::header::USER_AGENT, ua);
    }
    let response = request.send().await.map_err(|e| format!("请求失败：{e}"))?;
    let status = response.status();
    let final_url = response.url().to_string();
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("读取响应失败：{e}"))?;
    let (html, _) = decode_body(&bytes, Some("text/html"));
    Ok((html, final_url))
}

async fn resolve_redirect_url(client: &reqwest::Client, url: &str) -> Option<String> {
    let response = tokio::time::timeout(
        Duration::from_secs(4),
        client
            .get(url)
            .header(reqwest::header::REFERER, "https://www.baidu.com/")
            .send(),
    )
    .await
    .ok()?
    .ok()?;
    let final_url = response.url().to_string();
    if is_redirect_link(&final_url) {
        None
    } else {
        Some(final_url)
    }
}

/// 把一批跳转链接（前 5 条）并行解析成真实 URL；解析失败保留原链接。
async fn resolve_redirect_hits(client: &reqwest::Client, hits: Vec<RawHit>) -> Vec<RawHit> {
    let candidates: Vec<(usize, String)> = hits
        .iter()
        .enumerate()
        .filter(|(_, hit)| is_redirect_link(&hit.url))
        .take(5)
        .map(|(index, hit)| (index, hit.url.clone()))
        .collect();
    if candidates.is_empty() {
        return hits;
    }
    let futures = candidates.iter().map(|(index, url)| async move {
        let resolved = resolve_redirect_url(client, url).await;
        (*index, resolved)
    });
    let resolved = futures_util::future::join_all(futures).await;
    let mut hits = hits;
    for (index, resolved) in resolved {
        if let Some(url) = resolved {
            hits[index].url = url;
        }
    }
    hits
}

async fn search_baidu(
    client: &reqwest::Client,
    query: &str,
    limit: usize,
    lang: &str,
) -> Result<Vec<RawHit>, String> {
    // 1) 桌面主站（正常网络可用；反爬时返回验证码页）
    let desktop_url = format!(
        "https://www.baidu.com/s?wd={}&rn=20&ie=utf-8",
        encode_query(query)
    );
    let desktop_reason = match fetch_search_html(
        client,
        &desktop_url,
        Some("https://www.baidu.com/"),
        None,
        lang,
    )
    .await
    {
        Ok((html, final_url)) => {
            if looks_like_captcha_page(&html, &final_url) {
                "桌面版返回验证码页".to_string()
            } else {
                let hits = parse_baidu_html(&html, limit);
                if !hits.is_empty() {
                    return Ok(resolve_redirect_hits(client, hits).await);
                }
                "桌面版没有可解析的结果".to_string()
            }
        }
        Err(e) => format!("桌面版失败：{e}"),
    };
    // 2) 移动版（SSR 页面，反爬更少）
    let mobile_url = format!(
        "https://m.baidu.com/s?word={}&from=844b&sa=tb",
        encode_query(query)
    );
    let (html, final_url) = fetch_search_html(
        client,
        &mobile_url,
        Some("https://m.baidu.com/"),
        Some(MOBILE_UA),
        lang,
    )
    .await
    .map_err(|e| format!("百度不可用（{desktop_reason}；移动版失败：{e}）"))?;
    if looks_like_captcha_page(&html, &final_url) {
        return Err(format!("百度被验证码拦截（{desktop_reason}）"));
    }
    let hits = parse_baidu_mobile_html(&html, limit);
    if hits.is_empty() {
        return Err(format!("百度没有可解析的结果（{desktop_reason}）"));
    }
    Ok(hits)
}

async fn search_sogou(
    client: &reqwest::Client,
    query: &str,
    limit: usize,
    lang: &str,
) -> Result<Vec<RawHit>, String> {
    // 1) 移动版（a.resultLink 带真实 url 参数，最稳）
    let mobile_url = format!(
        "https://m.sogou.com/web/searchList.jsp?keyword={}",
        encode_query(query)
    );
    let mobile_reason = match fetch_search_html(
        client,
        &mobile_url,
        Some("https://m.sogou.com/"),
        Some(MOBILE_UA),
        lang,
    )
    .await
    {
        Ok((html, _)) => {
            let hits = parse_sogou_html(&html, limit);
            if !hits.is_empty() {
                return Ok(resolve_redirect_hits(client, hits).await);
            }
            "移动版没有可解析的结果".to_string()
        }
        Err(e) => format!("移动版失败：{e}"),
    };
    // 2) 桌面版
    let desktop_url = format!("https://www.sogou.com/web?query={}", encode_query(query));
    let (html, final_url) = fetch_search_html(
        client,
        &desktop_url,
        Some("https://www.sogou.com/"),
        None,
        lang,
    )
    .await
    .map_err(|e| format!("搜狗不可用（{mobile_reason}；桌面版失败：{e}）"))?;
    if looks_like_captcha_page(&html, &final_url) || html.contains("antispider") {
        return Err(format!("搜狗被反爬拦截（{mobile_reason}）"));
    }
    let hits = parse_sogou_html(&html, limit);
    if hits.is_empty() {
        return Err(format!("搜狗没有可解析的结果（{mobile_reason}）"));
    }
    Ok(resolve_redirect_hits(client, hits).await)
}

async fn search_bing_cn(
    client: &reqwest::Client,
    query: &str,
    limit: usize,
    lang: &str,
) -> Result<Vec<RawHit>, String> {
    let url = format!(
        "https://cn.bing.com/search?q={}&ensearch=0&count={}&setlang={}",
        encode_query(query),
        limit.clamp(1, 30),
        bing_setlang(lang)
    );
    let (html, _) = fetch_search_html(
        client,
        &url,
        Some("https://cn.bing.com/"),
        None,
        lang,
    )
    .await?;
    let hits = parse_bing_html(&html, limit);
    if hits.is_empty() {
        return Err("必应没有解析到结果（页面结构可能变化）".to_string());
    }
    Ok(hits)
}

async fn search_so360(
    client: &reqwest::Client,
    query: &str,
    limit: usize,
    lang: &str,
) -> Result<Vec<RawHit>, String> {
    let url = format!("https://www.so.com/s?q={}&src=srp", encode_query(query));
    let (html, _) = fetch_search_html(
        client,
        &url,
        Some("https://www.so.com/"),
        None,
        lang,
    )
    .await?;
    let hits = parse_so360_html(&html, limit);
    if hits.is_empty() {
        return Err("360 搜索没有解析到结果（页面结构可能变化）".to_string());
    }
    Ok(hits)
}

async fn search_ddg(
    client: &reqwest::Client,
    query: &str,
    limit: usize,
    lang: &str,
) -> Result<Vec<RawHit>, String> {
    let url = format!(
        "https://html.duckduckgo.com/html/?q={}&kl={}",
        encode_query(query),
        ddg_kl(lang)
    );
    let (html, _) = fetch_search_html(
        client,
        &url,
        Some("https://duckduckgo.com/"),
        None,
        lang,
    )
    .await
    .map_err(|e| format!("DuckDuckGo 请求失败：{e}"))?;
    let hits = parse_ddg_html(&html, limit);
    if hits.is_empty() {
        return Err("DuckDuckGo 没有解析到结果（国内常不可达/被反爬）".to_string());
    }
    Ok(hits)
}

async fn search_searxng(
    client: &reqwest::Client,
    base: &str,
    query: &str,
    limit: usize,
    lang: &str,
) -> Result<Vec<RawHit>, String> {
    if base.trim().is_empty() {
        return Err(
            "provider=searxng 但 settings.web.searxng_url 为空；请在设置里填写实例地址。".to_string(),
        );
    }
    let url = format!("{}/search", base.trim_end_matches('/'));
    let response = client
        .get(&url)
        .query(&[
            ("q", query.to_string()),
            ("format", "json".to_string()),
            ("language", lang.to_string()),
        ])
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|e| format!("SearXNG 请求失败：{e}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("SearXNG 返回 HTTP {status}"));
    }
    let value: Value = response
        .json()
        .await
        .map_err(|e| format!("SearXNG 返回的不是合法 JSON（实例可能未开启 format=json）：{e}"))?;
    let mut hits = Vec::new();
    if let Some(items) = value.get("results").and_then(|v| v.as_array()) {
        for item in items {
            let title = item.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let url = item.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let snippet = item.get("content").and_then(|v| v.as_str()).unwrap_or("");
            if !push_raw(&mut hits, title, url, snippet, limit) {
                break;
            }
        }
    }
    if hits.is_empty() {
        return Err("SearXNG 没有返回结果".to_string());
    }
    Ok(hits)
}

/// 跑单个引擎（带 12 秒独立超时，避免某个引擎拖垮聚合整体耗时）。
/// 给单个引擎的结果加独立超时，避免某个引擎拖垮聚合整体耗时。
async fn with_engine_timeout<F>(seconds: u64, future: F) -> Result<Vec<RawHit>, String>
where
    F: std::future::Future<Output = Result<Vec<RawHit>, String>>,
{
    match tokio::time::timeout(Duration::from_secs(seconds), future).await {
        Ok(result) => result,
        Err(_) => Err(format!("超时（{seconds} 秒）")),
    }
}

async fn run_engine(
    client: &reqwest::Client,
    engine: Engine,
    web: &WebSettings,
    query: &str,
    limit: usize,
    lang: &str,
) -> Result<Vec<RawHit>, String> {
    let seconds = 12;
    match engine {
        Engine::Baidu => {
            with_engine_timeout(seconds, search_baidu(client, query, limit, lang)).await
        }
        Engine::Sogou => {
            with_engine_timeout(seconds, search_sogou(client, query, limit, lang)).await
        }
        Engine::BingCn => {
            with_engine_timeout(seconds, search_bing_cn(client, query, limit, lang)).await
        }
        Engine::So360 => {
            with_engine_timeout(seconds, search_so360(client, query, limit, lang)).await
        }
        Engine::DuckDuckGo => {
            with_engine_timeout(seconds, search_ddg(client, query, limit, lang)).await
        }
        Engine::Searxng => {
            with_engine_timeout(
                seconds,
                search_searxng(client, &web.searxng_url, query, limit, lang),
            )
            .await
        }
    }
}

/// 把某个引擎的结果并入跨引擎合并列表（按归一化 URL 去重，累加 rank 分数）。
fn merge_raw_hits(merged: &mut Vec<MergedHit>, engine: &'static str, hits: &[RawHit]) {
    for (rank, hit) in hits.iter().enumerate() {
        let weight = 1.0 / ((rank + 3) as f64);
        let key = normalize_url_key(&hit.url);
        if let Some(existing) = merged
            .iter_mut()
            .find(|existing| normalize_url_key(&existing.url) == key)
        {
            existing.score += weight;
            if !existing.engines.contains(&engine) {
                existing.engines.push(engine);
            }
            if existing.snippet.is_empty() && !hit.snippet.is_empty() {
                existing.snippet = hit.snippet.clone();
            }
        } else {
            merged.push(MergedHit {
                title: hit.title.clone(),
                url: hit.url.clone(),
                snippet: hit.snippet.clone(),
                engines: vec![engine],
                score: weight,
            });
        }
    }
}

/// 排序并截断：分数 = Σ(1/(rank+3)) + 0.5 × 命中引擎数。
fn rank_merged(mut merged: Vec<MergedHit>, count: usize) -> Vec<MergedHit> {
    for item in merged.iter_mut() {
        item.score += 0.5 * item.engines.len() as f64;
    }
    merged.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.engines.len().cmp(&a.engines.len()))
            .then_with(|| a.title.cmp(&b.title))
    });
    merged.truncate(count);
    merged
}

struct AggregatedSearch {
    hits: Vec<MergedHit>,
    /// (引擎显示名, 命中条数)
    engine_counts: Vec<(&'static str, usize)>,
    /// (引擎显示名, 失败原因)
    engine_errors: Vec<(String, String)>,
    elapsed: Duration,
}

async fn search_aggregated(
    client: &reqwest::Client,
    web: &WebSettings,
    query: &str,
    count: usize,
    lang: &str,
) -> AggregatedSearch {
    let per_engine = count.clamp(10, 20);
    let started = Instant::now();
    // 先把引擎数组落到局部变量，避免 `Engine::AGGREGATED.iter()` 借到临时值。
    let engines = Engine::AGGREGATED;
    let futures = engines.iter().map(|&engine| async move {
        let result = run_engine(client, engine, web, query, per_engine, lang).await;
        (engine, result)
    });
    let joined = futures_util::future::join_all(futures).await;
    let elapsed = started.elapsed();

    let mut merged: Vec<MergedHit> = Vec::new();
    let mut engine_counts: Vec<(&'static str, usize)> = Vec::new();
    let mut engine_errors: Vec<(String, String)> = Vec::new();
    for (engine, result) in joined {
        match result {
            Ok(hits) => {
                engine_counts.push((engine.name(), hits.len()));
                merge_raw_hits(&mut merged, engine.name(), &hits);
            }
            Err(reason) => engine_errors.push((engine.name().to_string(), reason)),
        }
    }
    AggregatedSearch {
        hits: rank_merged(merged, count),
        engine_counts,
        engine_errors,
        elapsed,
    }
}

fn render_search_results(
    query: &str,
    provider_label: &str,
    hits: &[MergedHit],
    engine_counts: &[(&'static str, usize)],
    engine_errors: &[(String, String)],
    elapsed: Duration,
) -> String {
    let mut out = format!(
        "[ok] 搜索「{}」\n后端：{}，结果：{} 条，耗时 {}\n",
        query,
        provider_label,
        hits.len(),
        human_duration_ms(elapsed.as_millis() as u64)
    );
    if !engine_counts.is_empty() {
        out.push_str(&format!(
            "各引擎命中：{}\n",
            engine_counts
                .iter()
                .map(|(name, count)| format!("{name} {count}"))
                .collect::<Vec<_>>()
                .join("、")
        ));
    }
    if engine_errors.is_empty() {
        out.push_str("失败引擎：无\n");
    } else {
        out.push_str(&format!(
            "失败引擎：{}\n",
            engine_errors
                .iter()
                .map(|(name, reason)| format!("{name}（{reason}）"))
                .collect::<Vec<_>>()
                .join("；")
        ));
    }
    out.push('\n');
    for (index, hit) in hits.iter().enumerate() {
        out.push_str(&format!("{}. {}\n   {}\n", index + 1, hit.title, hit.url));
        if !hit.snippet.is_empty() {
            out.push_str(&format!("   {}\n", hit.snippet));
        }
        out.push_str(&format!("   来源: {}\n", hit.engines.join(", ")));
    }
    if hits.is_empty() {
        out.push_str("（没有结果）\n");
    }
    out.push_str("\n需要正文用 web_fetch(url)；想换引擎可传 provider=baidu/sogou/bing/360/duckduckgo/deepseek。");
    out
}

async fn web_search(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    let query = match arg_str_req(&args, "query") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let settings = ctx.settings();
    let web = &settings.web;
    let count = arg_u64(&args, "count", 8).clamp(1, 30) as usize;
    let lang = arg_str(&args, "lang").unwrap_or_else(|| "zh-CN".to_string());
    let explicit_provider = arg_str(&args, "provider").filter(|p| !p.trim().is_empty());
    let provider = match &explicit_provider {
        Some(raw) => match provider_from_arg(raw) {
            Ok(p) => p,
            Err(e) => return ToolOutcome::err(e),
        },
        None => provider_from_settings(web.search_provider),
    };
    if provider == Provider::None {
        return ToolOutcome::err(
            "搜索后端为 none/已关闭：请用 web_fetch 直接抓 URL，或在设置里把搜索后端改成 aggregated。"
                .to_string(),
        );
    }
    let client = match build_client(web) {
        Ok(c) => c,
        Err(e) => return ToolOutcome::err(e),
    };
    ctx.phase(format!("正在搜索：{}", truncate_chars(&query, 40)));

    match provider {
        Provider::Aggregated => {
            if ctx.is_cancelled() {
                return ToolOutcome::err("搜索已取消（用户中断）。".to_string());
            }
            let result = search_aggregated(&client, web, &query, count, &lang).await;
            if ctx.is_cancelled() {
                return ToolOutcome::err("搜索已取消（用户中断）。".to_string());
            }
            if result.hits.is_empty() {
                return ToolOutcome::err(format!(
                    "聚合搜索失败：所有引擎都没有返回可解析的结果。\n查询：{}\n各引擎：{}\n失败：{}\n\
                     建议：换关键词重试、检查网络/代理（settings.web.proxy），或用 web_fetch 抓已知 URL。",
                    query,
                    if result.engine_counts.is_empty() {
                        "无".to_string()
                    } else {
                        result
                            .engine_counts
                            .iter()
                            .map(|(name, n)| format!("{name} {n}"))
                            .collect::<Vec<_>>()
                            .join("、")
                    },
                    result
                        .engine_errors
                        .iter()
                        .map(|(name, reason)| format!("{name}（{reason}）"))
                        .collect::<Vec<_>>()
                        .join("；")
                ));
            }
            let label = format!(
                "aggregated（{} 个引擎）",
                Engine::AGGREGATED.len()
            );
            let content = render_search_results(
                &query,
                &label,
                &result.hits,
                &result.engine_counts,
                &result.engine_errors,
                result.elapsed,
            );
            ToolOutcome::ok(content).with_display(format!(
                "{} 条结果（{}）",
                result.hits.len(),
                result
                    .engine_counts
                    .iter()
                    .map(|(name, _)| *name)
                    .collect::<Vec<_>>()
                    .join("/")
            ))
        }
        Provider::DeepSeek => {
            if ctx.is_cancelled() {
                return ToolOutcome::err("搜索已取消（用户中断）。".to_string());
            }
            let started = Instant::now();
            match search_deepseek(&client, &settings, &query, count).await {
                Ok(raw_hits) => {
                    let elapsed = started.elapsed();
                    if ctx.is_cancelled() {
                        return ToolOutcome::err("搜索已取消（用户中断）。".to_string());
                    }
                    let mut merged: Vec<MergedHit> = Vec::new();
                    merge_raw_hits(&mut merged, "DeepSeek", &raw_hits);
                    let hits = rank_merged(merged, count);
                    let mut content = render_search_results(
                        &query,
                        "deepseek（官方 Search API）",
                        &hits,
                        &[("DeepSeek", raw_hits.len())],
                        &[],
                        elapsed,
                    );
                    content.push_str(
                        "\n来源说明：以上结果来自 DeepSeek 官方 Search API（实时网页搜索，非模型知识，按 rerank_score 排序）。",
                    );
                    ToolOutcome::ok(content).with_display(format!(
                        "{} 条结果（DeepSeek 官方搜索）",
                        hits.len()
                    ))
                }
                Err(reason) => {
                    // 官方搜索不可用（无 key / 网络 / 额度）时自动回退聚合，并记录一行 warn。
                    tracing::warn!(
                        target: "neosonia_tools::web",
                        reason = %reason,
                        "DeepSeek 官方搜索失败，自动回退聚合搜索"
                    );
                    if ctx.is_cancelled() {
                        return ToolOutcome::err("搜索已取消（用户中断）。".to_string());
                    }
                    let result = search_aggregated(&client, web, &query, count, &lang).await;
                    if result.hits.is_empty() {
                        return ToolOutcome::err(format!(
                            "DeepSeek 搜索失败（{reason}），回退聚合搜索也没有拿到结果。\n\
                             查询：{query}\n建议：检查 settings.server.api_key 与网络，或把搜索后端改成 auto；也可以直接用 web_fetch 抓已知 URL。"
                        ));
                    }
                    let label = format!(
                        "deepseek 不可用，已回退 aggregated（{} 个引擎）",
                        Engine::AGGREGATED.len()
                    );
                    let mut content = render_search_results(
                        &query,
                        &label,
                        &result.hits,
                        &result.engine_counts,
                        &result.engine_errors,
                        result.elapsed,
                    );
                    content.push_str(&format!(
                        "\n[提示] DeepSeek 官方搜索不可用（{reason}），本次已自动回退聚合搜索（百度/搜狗/必应/360）。"
                    ));
                    ToolOutcome::ok(content).with_display(format!(
                        "{} 条结果（DeepSeek 失败，已回退聚合）",
                        result.hits.len()
                    ))
                }
            }
        }
        Provider::Single(engine) => {
            let started = Instant::now();
            let result = run_engine(&client, engine, web, &query, count.clamp(10, 20), &lang).await;
            let elapsed = started.elapsed();
            if ctx.is_cancelled() {
                return ToolOutcome::err("搜索已取消（用户中断）。".to_string());
            }
            match result {
                Ok(raw_hits) => {
                    let mut merged: Vec<MergedHit> = Vec::new();
                    merge_raw_hits(&mut merged, engine.name(), &raw_hits);
                    let hits = rank_merged(merged, count);
                    let content = render_search_results(
                        &query,
                        engine.name(),
                        &hits,
                        &[(engine.name(), raw_hits.len())],
                        &[],
                        elapsed,
                    );
                    ToolOutcome::ok(content)
                        .with_display(format!("{} 条结果（{}）", hits.len(), engine.name()))
                }
                Err(reason) => ToolOutcome::err(format!(
                    "搜索失败（{}）：{}\n建议：换 provider（auto/baidu/sogou/bing/360）、换关键词，或检查网络。",
                    engine.name(),
                    reason
                )),
            }
        }
        Provider::None => ToolOutcome::err("搜索已关闭。".to_string()),
    }
}

// ============================================================
// 抓取
// ============================================================

struct FetchedPage {
    final_url: String,
    status: u16,
    content_type: String,
    encoding: String,
    title: Option<String>,
    html: String,
}

async fn fetch_page(
    client: &reqwest::Client,
    url: &Url,
    lang: &str,
    max_bytes: u64,
) -> Result<FetchedPage, String> {
    let resp = client
        .get(url.as_str())
        .header(
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .header("Accept-Language", accept_language(lang))
        .send()
        .await
        .map_err(|e| format!("请求失败：{e}"))?;
    let status = resp.status();
    let final_url = resp.url().to_string();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if let Some(len) = resp.content_length() {
        if len > max_bytes {
            return Err(format!(
                "响应太大（{}，超过上限 {}）。可改用 download_file 下载。",
                human_bytes(len),
                human_bytes(max_bytes)
            ));
        }
    }
    if !status.is_success() {
        let hint = if status.as_u16() == 403 {
            "（HTTP 403：可能被反爬拦截。可换 URL、稍后重试，或 open_path 打开页面后截图。）"
        } else if status.as_u16() == 404 {
            "（HTTP 404：地址不存在，检查拼写。）"
        } else {
            ""
        };
        return Err(format!("服务器返回 HTTP {status}{hint}"));
    }
    if !looks_textual(Some(content_type.as_str())) {
        return Err(format!(
            "响应不是可读文本（content-type={content_type}）。下载二进制请用 download_file。"
        ));
    }
    let bytes = resp.bytes().await.map_err(|e| format!("读取响应失败：{e}"))?;
    if bytes.len() as u64 > max_bytes {
        return Err(format!(
            "响应太大（{}，超过上限 {}）。可改用 download_file。",
            human_bytes(bytes.len() as u64),
            human_bytes(max_bytes)
        ));
    }
    let (html, encoding) = decode_body(&bytes, Some(content_type.as_str()));
    let title = extract_title(&html);
    Ok(FetchedPage {
        final_url,
        status: status.as_u16(),
        content_type,
        encoding,
        title,
        html,
    })
}

async fn web_fetch(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    let url_raw = match arg_str_req(&args, "url") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let settings = ctx.settings();
    let web = &settings.web;
    let format = arg_str(&args, "format")
        .unwrap_or_else(|| "text".to_string())
        .to_ascii_lowercase();
    if !["text", "markdown", "html"].contains(&format.as_str()) {
        return ToolOutcome::err(format!(
            "format 只能是 text / markdown / html，收到 {format:?}。"
        ));
    }
    let max_chars = arg_u64(&args, "max_chars", web.max_chars as u64)
        .clamp(500, 1_000_000) as usize;
    let render_js = arg_bool(&args, "render_js", false);
    if render_js {
        return ToolOutcome::err(
            "render_js=true 需要浏览器远程调试（CDP），本工具不内置浏览器。\n\
             替代方案：1) 用 cdp_* 工具连已开启 --remote-debugging-port 的 Chromium 应用；\
             2) 用 open_path 打开页面后 screenshot 看渲染结果；3) 找该站点的 API/JSON 接口。"
                .to_string(),
        );
    }
    let url = match Url::parse(url_raw.trim()) {
        Ok(u) => u,
        Err(e) => {
            return ToolOutcome::err(format!(
                "URL 非法（{url_raw}）：{e}。请传完整地址，例如 https://example.com/page。"
            ))
        }
    };
    if url.scheme() != "http" && url.scheme() != "https" {
        return ToolOutcome::err(format!(
            "只支持 http/https，收到 scheme={}。本地文件请用 read_file。",
            url.scheme()
        ));
    }
    if url.scheme() == "http" && !web.allow_http {
        return ToolOutcome::err(
            "设置里禁止明文 HTTP（web.allow_http=false）；请用 https。".to_string(),
        );
    }
    let client = match build_client(web) {
        Ok(c) => c,
        Err(e) => return ToolOutcome::err(e),
    };
    ctx.phase(format!("正在抓取：{}", truncate_chars(url.as_str(), 60)));
    let lang = arg_str(&args, "lang").unwrap_or_else(|| "zh-CN".to_string());
    let mut fetch_note: Option<String> = None;
    let page = match fetch_page(&client, &url, &lang, 8 * 1024 * 1024).await {
        Ok(page) => page,
        Err(direct_error) => {
            // 直连失败（反爬/超时/JS 站点）：配置了 DeepSeek key 时用官方 Fetch API 兜底。
            let Some(_) = deepseek_key(&settings) else {
                return ToolOutcome::err(direct_error);
            };
            match fetch_deepseek_markdown(&client, &settings, url.as_str()).await {
                Ok(fetched) => {
                    let body_chars = fetched.content.chars().count();
                    let truncated = body_chars > max_chars;
                    let body = middle_truncate(&fetched.content, max_chars);
                    let shown_url = if fetched.url.is_empty() {
                        url.as_str().to_string()
                    } else {
                        fetched.url.clone()
                    };
                    let mut out = format!(
                        "[ok] 抓取成功（DeepSeek Fetch API 兜底）\nURL：{shown_url}\n\
                         抓取方式：DeepSeek Fetch API（页面直连失败：{}）\n",
                        truncate_chars(&direct_error, 200)
                    );
                    if let Some(title) = &fetched.title {
                        out.push_str(&format!("标题：{title}\n"));
                    }
                    out.push_str(&format!(
                        "正文：{} 字符{}\n\n--- 正文（Markdown）---\n",
                        body_chars,
                        if truncated {
                            format!("（已截断到 max_chars={max_chars}）")
                        } else {
                            String::new()
                        }
                    ));
                    out.push_str(&body);
                    return ToolOutcome::ok(out)
                        .with_display(format!("{body_chars} 字符（DeepSeek Fetch）"));
                }
                Err(fetch_error) => {
                    return ToolOutcome::err(format!(
                        "直连抓取失败：{direct_error}\nDeepSeek Fetch API 兜底也失败：{fetch_error}\n\
                         建议：稍后重试、检查网络/代理（settings.web.proxy），或 open_path 打开后 screenshot。"
                    ))
                }
            }
        }
    };

    let mut title = page.title.clone();
    let mut final_url = page.final_url.clone();
    let mut body = if format == "html" {
        page.html.clone()
    } else {
        html_to_text(&page.html, 100)
    };
    // 正文太短（常见于 JS 渲染页面）且配置了 DeepSeek key：用官方 Fetch 提取更强正文。
    if format != "html" && body.trim().chars().count() < 200 && deepseek_key(&settings).is_some() {
        let direct_chars = body.trim().chars().count();
        if let Ok(fetched) = fetch_deepseek_markdown(&client, &settings, url.as_str()).await {
            fetch_note = Some(format!(
                "直连正文过短（{direct_chars} 字符，可能是 JS 渲染），已改用 DeepSeek Fetch API 提取"
            ));
            body = fetched.content;
            if let Some(fetched_title) = fetched.title {
                title = Some(fetched_title);
            }
            if !fetched.url.is_empty() {
                final_url = fetched.url;
            }
        }
    }
    let body_chars = body.chars().count();
    let body = middle_truncate(&body, max_chars);
    let truncated = body_chars > max_chars;

    let mut header = format!(
        "[ok] 抓取成功\nURL：{}\n状态：{}\n类型：{}（解码：{}）\n",
        final_url,
        page.status,
        if page.content_type.is_empty() {
            "（未知）"
        } else {
            page.content_type.as_str()
        },
        page.encoding
    );
    if let Some(title) = &title {
        header.push_str(&format!("标题：{}\n", title));
    }
    if let Some(note) = &fetch_note {
        header.push_str(&format!("抓取方式：{note}\n"));
    }
    header.push_str(&format!(
        "格式：{}\n正文：{} 字符{}\n\n--- 正文 ---\n",
        format,
        body_chars,
        if truncated {
            format!("（已截断到 max_chars={max_chars}；可调大 max_chars 或用 web_links 分段）")
        } else {
            String::new()
        }
    ));
    header.push_str(&body);
    if body.trim().is_empty() {
        header.push_str(
            "\n\n[注意] 正文为空：页面可能完全依赖 JS 渲染。可 open_path 打开后 screenshot，\
             或用 cdp_* 工具；也可以查看 web_links 找 API。",
        );
    }
    ToolOutcome::ok(header).with_display(format!(
        "{} 字符{}",
        body_chars,
        title
            .as_ref()
            .map(|t| format!("，{}", truncate_chars(t, 30)))
            .unwrap_or_default()
    ))
}

// ============================================================
// web_links
// ============================================================

fn extract_links(html: &str, base: &Url, filter: &str, limit: usize) -> Vec<(String, String)> {
    let doc = Html::parse_document(html);
    let Ok(sel) = Selector::parse("a[href]") else {
        return Vec::new();
    };
    let filter = filter.trim().to_ascii_lowercase();
    let mut out: Vec<(String, String)> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for anchor in doc.select(&sel) {
        let Some(href) = anchor.value().attr("href") else {
            continue;
        };
        let href = href.trim();
        if href.is_empty()
            || href.starts_with('#')
            || href.starts_with("javascript:")
            || href.starts_with("mailto:")
            || href.starts_with("tel:")
        {
            continue;
        }
        let joined = match base.join(href) {
            Ok(u) => u,
            Err(_) => continue,
        };
        if joined.scheme() != "http" && joined.scheme() != "https" {
            continue;
        }
        let url = joined.to_string();
        let text = collapse_whitespace(&anchor.text().collect::<String>());
        if !filter.is_empty()
            && !url.to_ascii_lowercase().contains(&filter)
            && !text.to_ascii_lowercase().contains(&filter)
        {
            continue;
        }
        if seen.contains(&url) {
            continue;
        }
        seen.push(url.clone());
        out.push((text, url));
        if out.len() >= limit {
            break;
        }
    }
    out
}

async fn web_links(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    let url_raw = match arg_str_req(&args, "url") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let settings = ctx.settings();
    let web = &settings.web;
    let limit = arg_u64(&args, "limit", 50).clamp(1, 500) as usize;
    let filter = arg_str(&args, "filter").unwrap_or_default();
    let url = match Url::parse(url_raw.trim()) {
        Ok(u) => u,
        Err(e) => {
            return ToolOutcome::err(format!("URL 非法（{url_raw}）：{e}。"))
        }
    };
    if url.scheme() != "http" && url.scheme() != "https" {
        return ToolOutcome::err("只支持 http/https URL。".to_string());
    }
    let client = match build_client(web) {
        Ok(c) => c,
        Err(e) => return ToolOutcome::err(e),
    };
    ctx.phase(format!("正在提取链接：{}", truncate_chars(url.as_str(), 60)));
    let lang = "zh-CN";
    let page = match fetch_page(&client, &url, lang, 8 * 1024 * 1024).await {
        Ok(p) => p,
        Err(e) => return ToolOutcome::err(e),
    };
    let base = Url::parse(&page.final_url).unwrap_or(url);
    let links = extract_links(&page.html, &base, &filter, limit);
    let mut out = format!(
        "[ok] 链接提取完成\n页面：{}\n标题：{}\n过滤：{}\n链接：{} 条{}\n\n",
        page.final_url,
        page.title.unwrap_or_else(|| "（无标题）".to_string()),
        if filter.trim().is_empty() {
            "（无）"
        } else {
            filter.as_str()
        },
        links.len(),
        if links.len() >= limit {
            format!("（达到 limit={limit}，可能还有更多）")
        } else {
            String::new()
        }
    );
    if links.is_empty() {
        out.push_str("没有匹配的链接。\n");
    } else {
        for (i, (text, url)) in links.iter().enumerate() {
            if text.is_empty() {
                out.push_str(&format!("{}. {}\n", i + 1, url));
            } else {
                out.push_str(&format!("{}. {} -- {}\n", i + 1, truncate_chars(text, 120), url));
            }
        }
    }
    ToolOutcome::ok(out).with_display(format!("{} 条链接", links.len()))
}

// ============================================================
// download_file
// ============================================================

fn decode_percent(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = bytes[i + 1];
            let lo = bytes[i + 2];
            let hex = [hi, lo];
            if let Ok(text) = std::str::from_utf8(&hex) {
                if let Ok(v) = u8::from_str_radix(text, 16) {
                    out.push(v);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn sanitize_filename(name: &str) -> String {
    let decoded = decode_percent(name);
    let mut out = String::new();
    for ch in decoded.chars() {
        if ch.is_ascii_alphanumeric() || ch == '.' || ch == '-' || ch == '_' || (ch as u32) > 0x2E80 {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches('.').trim_matches('_').to_string();
    if trimmed.is_empty() {
        "download.bin".to_string()
    } else if trimmed.chars().count() > 120 {
        let keep: String = trimmed.chars().take(120).collect();
        keep
    } else {
        trimmed
    }
}

fn filename_from_url(url: &Url) -> String {
    let last = url
        .path_segments()
        .and_then(|segments| segments.filter(|s| !s.is_empty()).next_back())
        .unwrap_or("download.bin");
    let last = if last.is_empty() { "download.bin" } else { last };
    let name = sanitize_filename(last);
    if name.contains('.') {
        name
    } else {
        format!("{name}.bin")
    }
}

async fn download_file(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    let url_raw = match arg_str_req(&args, "url") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let settings = ctx.settings();
    let web = &settings.web;
    let max_bytes = arg_u64(&args, "max_bytes", 200 * 1024 * 1024).clamp(1024, 2 * 1024 * 1024 * 1024);
    let allow_outside = arg_bool(&args, "allow_outside", false);

    let url = match Url::parse(url_raw.trim()) {
        Ok(u) => u,
        Err(e) => return ToolOutcome::err(format!("URL 非法（{url_raw}）：{e}。")),
    };
    if url.scheme() != "http" && url.scheme() != "https" {
        return ToolOutcome::err("下载只支持 http/https URL。".to_string());
    }
    let target: PathBuf = match arg_str(&args, "path").filter(|s| !s.trim().is_empty()) {
        Some(raw) => match resolve_write_path(ctx, &raw, allow_outside) {
            Ok(p) => p,
            Err(e) => return e,
        },
        None => {
            let mut p = ctx.workspace().join("downloads");
            p.push(filename_from_url(&url));
            p
        }
    };
    if let Some(parent) = target.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return ToolOutcome::err(format!(
                "创建下载目录 {} 失败：{}",
                parent.display(),
                io_error_hint(&e)
            ));
        }
    }
    let client = match build_client(web) {
        Ok(c) => c,
        Err(e) => return ToolOutcome::err(e),
    };
    ctx.phase(format!("开始下载：{}", truncate_chars(url.as_str(), 60)));
    let resp = match client
        .get(url.as_str())
        .header("Accept-Language", accept_language("zh-CN"))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return ToolOutcome::err(format!("请求失败：{e}")),
    };
    let status = resp.status();
    if !status.is_success() {
        return ToolOutcome::err(format!("服务器返回 HTTP {status}，下载失败。"));
    }
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let total = resp.content_length();
    if let Some(len) = total {
        if len > max_bytes {
            return ToolOutcome::err(format!(
                "文件太大（{}，超过 max_bytes={}）；可调大 max_bytes 或分块下载。",
                human_bytes(len),
                human_bytes(max_bytes)
            ));
        }
    }
    let mut file = match tokio::fs::File::create(&target).await {
        Ok(f) => f,
        Err(e) => {
            return ToolOutcome::err(format!(
                "创建 {} 失败：{}",
                target.display(),
                io_error_hint(&e)
            ))
        }
    };
    let mut stream = std::pin::pin!(resp.bytes_stream());
    let started = Instant::now();
    let mut written: u64 = 0;
    let mut last_report = Instant::now();
    let mut cancelled = false;
    while let Some(chunk) = stream.next().await {
        if ctx.is_cancelled() {
            cancelled = true;
            break;
        }
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                let _ = tokio::fs::remove_file(&target).await;
                return ToolOutcome::err(format!("下载中断：{e}（已删除不完整文件）"));
            }
        };
        written += chunk.len() as u64;
        if written > max_bytes {
            let _ = tokio::fs::remove_file(&target).await;
            return ToolOutcome::err(format!(
                "下载超过 max_bytes={}，已中止并删除部分文件。",
                human_bytes(max_bytes)
            ));
        }
        use tokio::io::AsyncWriteExt;
        if let Err(e) = file.write_all(&chunk).await {
            let _ = tokio::fs::remove_file(&target).await;
            return ToolOutcome::err(format!("写文件失败：{e}（已删除不完整文件）"));
        }
        if last_report.elapsed() >= Duration::from_millis(500) {
            last_report = Instant::now();
            let pct = total
                .filter(|t| *t > 0)
                .map(|t| format!(" {:.0}%", written as f64 / t as f64 * 100.0))
                .unwrap_or_default();
            ctx.emit_output(format!(
                "下载中：{}{} / {}\n",
                human_bytes(written),
                pct,
                total
                    .map(human_bytes)
                    .unwrap_or_else(|| "未知大小".to_string())
            ));
        }
    }
    use tokio::io::AsyncWriteExt;
    let _ = file.flush().await;
    drop(file);
    if cancelled {
        let _ = tokio::fs::remove_file(&target).await;
        return ToolOutcome::err(match std::fs::metadata(&target) {
            Ok(_) => "下载已取消（用户中断），已删除部分文件。".to_string(),
            Err(_) => "下载已取消（用户中断）。".to_string(),
        });
    }
    let elapsed = started.elapsed();
    ToolOutcome::ok(format!(
        "[ok] 下载完成\n路径：{}\n大小：{}（{} 字节）\n类型：{}\n耗时：{}\n",
        target.display(),
        human_bytes(written),
        written,
        if content_type.is_empty() { "（未知）" } else { content_type.as_str() },
        human_duration_ms(elapsed.as_millis() as u64)
    ))
    .with_display(format!("下载 {}", human_bytes(written)))
}

// ============================================================
// 工具声明
// ============================================================

tool_impl!(
    WebSearch,
    "web_search",
    "联网搜索（国内可用）。默认自实现聚合：并行请求 百度/搜狗/必应中国/360，\
     按“跨引擎命中数 + 排名”加权合并去重，结果标注来源引擎；某个引擎失败会跳过并统计，不影响其它引擎。\
     返回编号列表（标题/URL/摘要/来源）。拿到 URL 后用 web_fetch 读正文。\n\
     provider 可指定后端：auto/aggregated（默认）、baidu、sogou、bing、360、duckduckgo、searxng、deepseek、none。\n\
     deepseek：调用 DeepSeek 官方 Search API（实时网页搜索，按 rerank_score 排序，来源标注 DeepSeek）；\
     未配置 key 或请求失败时会自动回退聚合搜索，并在结果里注明回退原因。",
    json!({
        "type": "object",
        "properties": {
            "query": {"type": "string", "description": "搜索关键词"},
            "count": {"type": "integer", "description": "返回结果条数，默认 8"},
            "provider": {"type": "string", "enum": ["auto", "aggregated", "baidu", "sogou", "bing", "360", "duckduckgo", "searxng", "deepseek", "none"], "description": "搜索后端，默认从设置读取（aggregated）；deepseek=官方 Search API（失败自动回退聚合）"},
            "lang": {"type": "string", "description": "语言偏好，默认 zh-CN"}
        },
        "required": ["query"]
    }),
    web_search
);

tool_impl!(
    WebFetch,
    "web_fetch",
    "抓取网页并返回正文（自动识别编码、去掉脚本样式、支持 text/markdown/html 三种格式）。\
     默认返回纯文本；max_chars 控制长度。页面依赖 JS 渲染时正文可能为空，\
     可用 open_path 打开后 screenshot，或 cdp_* 工具；render_js=true 目前会明确说明限制。\n\
     配置了 DeepSeek API key 时：直连抓取失败、或正文太短（疑似 JS 渲染）会自动用 DeepSeek Fetch API 兜底，\
     结果里会注明“抓取方式：DeepSeek Fetch API”。",
    json!({
        "type": "object",
        "properties": {
            "url": {"type": "string", "description": "要抓取的 http/https URL"},
            "max_chars": {"type": "integer", "description": "正文最大字符数，默认取设置"},
            "format": {"type": "string", "enum": ["text", "markdown", "html"], "description": "返回格式，默认 text"},
            "render_js": {"type": "boolean", "description": "是否渲染 JS（需要 CDP，本工具会返回限制说明）"},
            "lang": {"type": "string", "description": "Accept-Language 偏好，默认 zh-CN"}
        },
        "required": ["url"]
    }),
    web_fetch
);

tool_impl!(
    WebLinks,
    "web_links",
    "提取网页上的所有链接（自动解析相对路径、去重），可用 filter 按 URL 或链接文本过滤。\
     适合先了解站点结构，再决定 web_fetch 哪些页面。",
    json!({
        "type": "object",
        "properties": {
            "url": {"type": "string", "description": "页面 URL"},
            "filter": {"type": "string", "description": "可选，URL 或链接文本的子串过滤（不区分大小写）"},
            "limit": {"type": "integer", "description": "最多返回多少条，默认 50"}
        },
        "required": ["url"]
    }),
    web_links
);

tool_impl!(
    DownloadFile,
    "download_file",
    "流式下载文件到工作目录（默认 workspace/downloads/<文件名>），带进度回传、大小上限和取消支持。\
     适合下载安装包、数据集、图片等二进制文件。",
    json!({
        "type": "object",
        "properties": {
            "url": {"type": "string", "description": "文件 URL"},
            "path": {"type": "string", "description": "保存路径；默认 workspace/downloads/从 URL 推导的文件名"},
            "max_bytes": {"type": "integer", "description": "最大字节数，默认 200MB"},
            "allow_outside": {"type": "boolean", "description": "允许保存到工作目录之外，默认 false"}
        },
        "required": ["url"]
    }),
    download_file
);

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    const BING_HTML: &str = r#"<!DOCTYPE html><html><head><title>rust - 搜索</title></head><body>
<ol id="b_results">
<li class="b_algo"><h2><a href="https://www.runoob.com/rust/rust-tutorial.html">Rust 教程 | 菜鸟教程</a></h2>
  <div class="b_caption"><p>Rust 语言教程，从入门到进阶。</p></div></li>
<li class="b_algo"><h2><a href="https://doc.rust-lang.org/book/">The Rust Programming Language</a></h2>
  <p>Official book for Rust.</p></li>
<li class="b_algo"><h2><a href="https://www.bing.com/ck/a?u=abc">redirect junk</a></h2><p>x</p></li>
</ol></body></html>"#;

    const DDG_HTML: &str = r##"<html><body>
<div class="result results_links">
  <h2><a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Frust%3Fa%3D1&rut=xyz">Rust 官网</a></h2>
  <a class="result__snippet" href="#">Rust 是一门系统编程语言。</a>
</div>
<div class="result results_links">
  <h2><a class="result__a" href="https://doc.rust-lang.org/book/">Rust Book</a></h2>
  <a class="result__snippet">The book.</a>
</div>
</body></html>"##;

    const SO360_HTML: &str = r##"<html><body><ul>
<li class="res-list">
  <h3 class="res-title "><a href="https://www.so.com/link?m=xxx" data-mdurl="https://www.rust-lang.org/" target="_blank"><em>Rust</em> Programming Language</a></h3>
  <p class="res-desc"><span data-result-summary>Rust is blazingly fast and memory-efficient.</span></p>
</li>
<li class="res-list">
  <h3 class="res-title "><a href="https://www.so.com/link?m=yyy" data-mdurl="https://doc.rust-lang.org/book/" target="_blank">Rust Book</a></h3>
  <p class="res-desc">The Rust Programming Language book.</p>
</li>
<li class="res-list"><div>feedback card without title</div></li>
</ul></body></html>"##;

    const SOGOU_HTML: &str = r##"<html><body>
<div class="structure190319">
  <h3 class="vr-tit clamp2 click-sugg-title"><a class="resultLink" href="./id=abc/keyword=rust/tc?rcer=x&amp;title=Rust%20Programming%20Language&amp;url=https%3A%2F%2Fwww.rust-lang.org%2F&amp;pcurl=https%3A%2F%2Fwww.rust-lang.org%2F"><em>Rust</em> Programming <em>Language</em></a></h3>
  <a class="txt-summary space-txt" href="./id=abc"><em>A language</em> empowering everyone to build reliable software.</a>
</div>
<div class="structure190319">
  <h3 class="vr-tit"><a class="resultLink" href="./id=def/keyword=rust/tc?url=https%3A%2F%2Fdoc.rust-lang.org%2Fbook%2F">Rust Book</a></h3>
  <a class="txt-summary space-txt">The Rust Programming Language book.</a>
</div>
</body></html>"##;

    const BAIDU_MOBILE_HTML: &str = r##"<html><body>
<div class="c-result result" srcid='www_index' data-log='{"fm":"alop","ensrcid":"www_index","order":3,"mu":"https://blog.csdn.net/example/article"}'>
  <div class="c-result-content"><article><h3 class="cosc-title">Rust 入门教程</h3>
  <div data-module="pure-summary"><!--s-data:{"summaryData":{"generalLines":[{"data":[{"text":"这是一篇 Rust 入门教程的摘要，介绍所有权与借用。"}]}]}}--></div></article></div>
</div>
<div class="c-result result" srcid='recommend_list_san' data-log='{"mu":"http://34689.recommend_list.baidu.com"}'>
  <div class="c-result-content"><article><h3>大家还在搜</h3></article></div>
</div>
</body></html>"##;

    const BAIDU_DESKTOP_HTML: &str = r##"<html><body>
<div class="result c-container" id="1">
  <h3 class="t"><a href="https://www.baidu.com/link?url=abc">Rust 桌面版结果</a></h3>
  <div class="c-abstract">桌面版摘要内容。</div>
</div>
</body></html>"##;

    #[test]
    fn bing_parse() {
        let hits = parse_bing_html(BING_HTML, 10);
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert_eq!(hits[0].url, "https://www.runoob.com/rust/rust-tutorial.html");
        assert!(hits[0].title.contains("Rust 教程"));
        assert!(hits[0].snippet.contains("从入门到进阶"));
        assert_eq!(hits[1].url, "https://doc.rust-lang.org/book/");
        assert!(!hits.iter().any(|h| h.url.contains("bing.com/ck/a")));
    }

    #[test]
    fn so360_parse() {
        let hits = parse_so360_html(SO360_HTML, 10);
        assert_eq!(hits.len(), 2, "{hits:?}");
        // 必须用 data-mdurl 还原真实地址，而不是 so.com/link 跳转
        assert_eq!(hits[0].url, "https://www.rust-lang.org/");
        assert!(hits[0].title.contains("Rust Programming Language"));
        assert!(hits[0].snippet.contains("memory-efficient"));
        assert_eq!(hits[1].url, "https://doc.rust-lang.org/book/");
    }

    #[test]
    fn sogou_parse() {
        let hits = parse_sogou_html(SOGOU_HTML, 10);
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert_eq!(hits[0].url, "https://www.rust-lang.org/");
        assert!(hits[0].title.contains("Rust Programming Language"), "{hits:?}");
        assert!(hits[0].snippet.contains("reliable software"), "{hits:?}");
        assert_eq!(hits[1].url, "https://doc.rust-lang.org/book/");
    }

    #[test]
    fn baidu_mobile_parse() {
        let hits = parse_baidu_mobile_html(BAIDU_MOBILE_HTML, 10);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].url, "https://blog.csdn.net/example/article");
        assert!(hits[0].title.contains("Rust 入门教程"));
        assert!(hits[0].snippet.contains("所有权与借用"), "{hits:?}");
    }

    #[test]
    fn baidu_desktop_parse() {
        let hits = parse_baidu_html(BAIDU_DESKTOP_HTML, 10);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert!(hits[0].url.contains("baidu.com/link?url=abc"));
        assert!(hits[0].title.contains("Rust 桌面版结果"));
        assert!(hits[0].snippet.contains("桌面版摘要"));
    }

    #[test]
    fn ddg_parse_and_unwrap_redirect() {
        let hits = parse_ddg_html(DDG_HTML, 10);
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert_eq!(hits[0].url, "https://example.com/rust?a=1");
        assert!(hits[0].title.contains("Rust 官网"));
        assert!(hits[0].snippet.contains("系统编程"));
        assert_eq!(hits[1].url, "https://doc.rust-lang.org/book/");
    }

    #[test]
    fn ddg_dedup() {
        let html = r##"<div class="result"><a class="result__a" href="https://a.com/x">A</a></div>
                      <div class="result"><a class="result__a" href="https://a.com/x/">A again</a></div>"##;
        let hits = parse_ddg_html(html, 10);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn ddg_limit() {
        let mut html = String::new();
        for i in 0..10 {
            html.push_str(&format!(
                r#"<div class="result"><a class="result__a" href="https://a.com/{i}">T{i}</a></div>"#
            ));
        }
        assert_eq!(parse_ddg_html(&html, 3).len(), 3);
    }

    #[test]
    fn captcha_detection() {
        assert!(looks_like_captcha_page(
            "<html>wappass.baidu.com/static/captcha/tuxing_v2.html</html>",
            "https://wappass.baidu.com/static/captcha/tuxing_v2.html"
        ));
        assert!(looks_like_captcha_page("<html>请输入验证码</html>", "https://x.com/s"));
        assert!(!looks_like_captcha_page("<html>正常结果</html>", "https://www.baidu.com/s"));
    }

    #[test]
    fn url_normalization_for_dedup() {
        assert_eq!(
            normalize_url_key("https://www.Example.com/path/?utm_source=x&b=2&a=1#frag"),
            "example.com/path?a=1&b=2"
        );
        assert_eq!(
            normalize_url_key("http://example.com/path"),
            normalize_url_key("https://www.example.com/path/")
        );
        assert_eq!(
            normalize_url_key("https://example.com/a?fbclid=1"),
            "example.com/a"
        );
    }

    #[test]
    fn merge_ranks_by_cross_engine_score() {
        let mut merged: Vec<MergedHit> = Vec::new();
        let engine_a = vec![
            RawHit {
                title: "共享结果".into(),
                url: "https://example.com/shared".into(),
                snippet: "A 的摘要".into(),
            },
            RawHit {
                title: "只有 A".into(),
                url: "https://example.com/a".into(),
                snippet: String::new(),
            },
        ];
        let engine_b = vec![
            RawHit {
                title: "只有 B".into(),
                url: "https://other.com/c".into(),
                snippet: String::new(),
            },
            RawHit {
                title: "共享结果".into(),
                url: "https://www.example.com/shared/".into(),
                snippet: "B 的摘要".into(),
            },
        ];
        merge_raw_hits(&mut merged, "百度", &engine_a);
        merge_raw_hits(&mut merged, "搜狗", &engine_b);
        assert_eq!(merged.len(), 3, "{merged:?}");
        let shared = merged
            .iter()
            .find(|m| m.url.contains("shared"))
            .expect("应有共享结果");
        assert_eq!(shared.engines, vec!["百度", "搜狗"]);
        assert!(shared.snippet.contains("A 的摘要"));

        let ranked = rank_merged(merged, 3);
        assert!(ranked[0].url.contains("shared"), "跨引擎命中应排第一: {ranked:?}");
        // 0.5×2 跨引擎加成后，共享结果的分数应最高
        assert!(ranked[0].score > ranked[1].score);
        assert_eq!(ranked.len(), 3);
        assert_eq!(rank_merged(ranked, 1).len(), 1);
    }

    #[test]
    fn provider_mapping() {
        assert_eq!(provider_from_arg("auto").unwrap(), Provider::Aggregated);
        assert_eq!(
            provider_from_arg("aggregated").unwrap(),
            Provider::Aggregated
        );
        assert_eq!(
            provider_from_arg("baidu").unwrap(),
            Provider::Single(Engine::Baidu)
        );
        assert_eq!(
            provider_from_arg("sogou").unwrap(),
            Provider::Single(Engine::Sogou)
        );
        assert_eq!(
            provider_from_arg("bing").unwrap(),
            Provider::Single(Engine::BingCn)
        );
        assert_eq!(
            provider_from_arg("360").unwrap(),
            Provider::Single(Engine::So360)
        );
        assert_eq!(provider_from_arg("none").unwrap(), Provider::None);
        assert!(provider_from_arg("google").is_err());
        assert_eq!(
            provider_from_settings(SearchProvider::Aggregated),
            Provider::Aggregated
        );
        assert_eq!(
            provider_from_settings(SearchProvider::Bing),
            Provider::Single(Engine::BingCn)
        );
    }

    #[test]
    fn render_has_engine_sources() {
        let hits = vec![
            MergedHit {
                title: "标题一".into(),
                url: "https://example.com/1".into(),
                snippet: "摘要一".into(),
                engines: vec!["百度", "搜狗"],
                score: 1.5,
            },
            MergedHit {
                title: "标题二".into(),
                url: "https://example.com/2".into(),
                snippet: String::new(),
                engines: vec!["必应"],
                score: 0.8,
            },
        ];
        let text = render_search_results(
            "rust",
            "aggregated（4 个引擎）",
            &hits,
            &[("百度", 1), ("搜狗", 1), ("必应", 1)],
            &[("360".to_string(), "超时（12 秒）".to_string())],
            Duration::from_millis(1200),
        );
        assert!(text.contains("1. 标题一"));
        assert!(text.contains("来源: 百度, 搜狗"));
        assert!(text.contains("各引擎命中：百度 1、搜狗 1、必应 1"));
        assert!(text.contains("360（超时"));
        assert!(text.contains("后端：aggregated（4 个引擎）"));
    }

    #[test]
    fn charset_detection() {
        assert_eq!(
            header_charset(Some("text/html; charset=GBK")),
            Some("gbk".to_string())
        );
        assert_eq!(header_charset(Some("text/html")), None);
        assert_eq!(header_charset(None), None);
        let (bytes, _, _) = encoding_rs::GBK.encode("中文字符");
        let mut html = b"<html><head><meta charset=\"gbk\"></head><body>".to_vec();
        html.extend_from_slice(&bytes);
        html.extend_from_slice(b"</body></html>");
        let (text, enc) = decode_body(&html, None);
        assert!(text.contains("中文字符"), "{text}");
        assert_eq!(enc, "GBK");
    }

    #[test]
    fn decode_utf8_without_header() {
        let (text, enc) = decode_body("<html>中文</html>".as_bytes(), None);
        assert_eq!(enc, "UTF-8");
        assert!(text.contains("中文"));
    }

    #[test]
    fn html2text_strips_markup() {
        let html = "<html><head><title>T</title><style>.a{color:red}</style></head>\
                    <body><h1>标题</h1><p>正文内容</p><script>alert(1)</script></body></html>";
        let text = html_to_text(html, 80);
        assert!(text.contains("标题"), "{text}");
        assert!(text.contains("正文内容"), "{text}");
        assert!(!text.contains("alert"), "{text}");
        assert!(!text.contains("color:red"), "{text}");
        assert_eq!(extract_title(html).as_deref(), Some("T"));
    }

    #[test]
    fn links_resolution() {
        let base = Url::parse("https://example.com/dir/page.html").unwrap();
        let html = r##"<a href="/abs">A</a>
                      <a href="rel.html">Rel</a>
                      <a href="https://other.com/c">Other</a>
                      <a href="javascript:void(0)">JS</a>
                      <a href="mailto:a@b.com">Mail</a>
                      <a href="#frag">Frag</a>
                      <a href="/abs">A dup</a>"##;
        let links = extract_links(html, &base, "", 50);
        assert_eq!(links.len(), 3, "{links:?}");
        assert_eq!(links[0].1, "https://example.com/abs");
        assert_eq!(links[1].1, "https://example.com/dir/rel.html");
        assert_eq!(links[2].1, "https://other.com/c");

        let filtered = extract_links(html, &base, "other", 50);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].1, "https://other.com/c");
    }

    #[test]
    fn filename_helpers() {
        let url = Url::parse("https://example.com/files/%E6%B5%8B%E8%AF%95.exe").unwrap();
        let name = filename_from_url(&url);
        assert!(name.ends_with(".exe"), "{name}");
        assert_eq!(decode_percent("%E4%B8%AD%E6%96%87"), "中文");
        assert_eq!(sanitize_filename("a b:c?.txt"), "a_b_c_.txt");
        assert_eq!(sanitize_filename("..."), "download.bin");
        let no_ext = Url::parse("https://example.com/download").unwrap();
        assert_eq!(filename_from_url(&no_ext), "download.bin");
    }

    #[test]
    fn detect_binary_content_type() {
        assert!(looks_textual(Some("text/html; charset=utf-8")));
        assert!(looks_textual(Some("text/html")));
        assert!(!looks_textual(Some("image/png")));
        assert!(!looks_textual(Some("application/pdf")));
        assert!(looks_textual(Some("application/json")));
        assert!(looks_textual(Some("application/xhtml+xml")));
        assert!(looks_textual(None));
    }

    #[tokio::test]
    async fn missing_url_returns_err() {
        let root = crate::test_support::temp_root("web-args");
        let (ctx, _host) = crate::test_support::fake_ctx(&root);
        let out = web_fetch(&ctx, json!({})).await;
        assert!(!out.ok);
        let out = web_fetch(&ctx, json!({"url": "not a url"})).await;
        assert!(!out.ok);
        let out = web_fetch(&ctx, json!({"url": "https://x.com", "format": "xml"})).await;
        assert!(!out.ok);
        let out = web_links(&ctx, json!({})).await;
        assert!(!out.ok);
        let out = download_file(&ctx, json!({})).await;
        assert!(!out.ok);
        let out = web_search(&ctx, json!({})).await;
        assert!(!out.ok);
        let out = web_search(&ctx, json!({"query": "x", "provider": "google"})).await;
        assert!(!out.ok);
    }

    /// 真实联网搜索验证（手动跑：`cargo test -p neosonia-tools web_search_live -- --ignored --nocapture`）。
    #[tokio::test]
    #[ignore]
    async fn web_search_live_aggregated() {
        let root = crate::test_support::temp_root("web-live");
        let (ctx, _host) = crate::test_support::fake_ctx(&root);
        let out = web_search(&ctx, json!({"query": "rust language", "count": 8})).await;
        assert!(out.ok, "{}", out.content);
        assert!(out.content.contains("来源:"), "{}", out.content);
        assert!(out.content.contains("http"), "{}", out.content);
        println!("{}", out.content);
    }

    #[tokio::test]
    #[ignore]
    async fn web_search_live_single_engines() {
        let root = crate::test_support::temp_root("web-live-single");
        let (ctx, _host) = crate::test_support::fake_ctx(&root);
        for provider in ["baidu", "sogou", "bing", "360"] {
            let out = web_search(
                &ctx,
                json!({"query": "rust language", "count": 5, "provider": provider}),
            )
            .await;
            println!("=== provider={provider} ok={} ===\n{}", out.ok, neosonia_core::util::truncate_chars(&out.content, 900));
        }
    }

    #[tokio::test]
    #[ignore]
    async fn web_fetch_live() {
        let root = crate::test_support::temp_root("web-fetch-live");
        let (ctx, _host) = crate::test_support::fake_ctx(&root);
        let out = web_fetch(&ctx, json!({"url": "https://cn.bing.com/search?q=rust"})).await;
        assert!(out.ok, "{}", out.content);
        println!("{}", middle_truncate(&out.content, 800));
    }

    // ---- DeepSeek 官方 Search / Fetch API ----

    #[test]
    fn deepseek_base_url_normalization() {
        assert_eq!(deepseek_api_root("https://api.deepseek.com"), "https://api.deepseek.com");
        assert_eq!(
            deepseek_api_root("https://api.deepseek.com/v1"),
            "https://api.deepseek.com"
        );
        assert_eq!(
            deepseek_api_root("https://api.deepseek.com/v1/"),
            "https://api.deepseek.com"
        );
        assert_eq!(
            deepseek_api_root("https://api.deepseek.com/v1/chat/completions"),
            "https://api.deepseek.com"
        );
        assert_eq!(
            deepseek_search_url("https://api.deepseek.com/v1"),
            "https://api.deepseek.com/v1/tools/search"
        );
        assert_eq!(
            deepseek_search_url(""),
            "https://api.deepseek.com/v1/tools/search"
        );
        assert_eq!(
            deepseek_fetch_url("https://api.deepseek.com"),
            "https://api.deepseek.com/v1/tools/fetch"
        );
        // 其它 OpenAI 兼容端点也规范成根地址 + 官方路径（失败会回退聚合）
        assert_eq!(
            deepseek_search_url("http://127.0.0.1:8080/v1"),
            "http://127.0.0.1:8080/v1/tools/search"
        );
    }

    #[test]
    fn deepseek_provider_mapping() {
        assert_eq!(
            provider_from_arg("deepseek").unwrap(),
            Provider::DeepSeek
        );
        assert_eq!(provider_from_arg("DeepSeek").unwrap(), Provider::DeepSeek);
        assert_eq!(provider_from_arg("ds").unwrap(), Provider::DeepSeek);
        assert_eq!(
            provider_from_settings(SearchProvider::DeepSeek),
            Provider::DeepSeek
        );
    }

    #[test]
    fn deepseek_search_result_parsing() {
        let payload = json!({
            "results": [
                {"rerank_score": 1.1, "title": "低分结果", "url": "https://a.example/1", "snippet": "s1", "summary": ""},
                {"rerank_score": 2.9, "title": "高分结果", "url": "https://b.example/2", "summary": "这是 summary 正文", "snippet": "被忽略"},
                {"rerank_score": 2.0, "title": "", "url": "https://c.example/3", "snippet": "标题为空用 URL"},
                {"rerank_score": 3.0, "title": "非法链接", "url": "javascript:alert(1)"},
                {"rerank_score": 2.5, "title": "没有 URL", "url": ""}
            ]
        });
        let hits = parse_deepseek_search_results(&payload, 10).expect("应能解析");
        assert_eq!(hits.len(), 3, "非法 URL/空 URL 应被过滤: {hits:?}");
        // 按 rerank_score 降序
        assert_eq!(hits[0].url, "https://b.example/2");
        assert_eq!(hits[0].title, "高分结果");
        assert_eq!(hits[0].snippet, "这是 summary 正文", "摘要应优先 summary");
        assert_eq!(hits[1].url, "https://c.example/3");
        assert_eq!(hits[1].title, "https://c.example/3", "空标题回退 URL");
        assert_eq!(hits[2].url, "https://a.example/1");
        assert!(hits[2].snippet.contains("s1"), "没有 summary 时用 snippet");
        // 截断 limit
        assert_eq!(parse_deepseek_search_results(&payload, 2).unwrap().len(), 2);
        // 缺 results 字段应报错而不是 panic
        assert!(parse_deepseek_search_results(&json!({}), 10).is_err());
        assert!(parse_deepseek_search_results(&json!({"results": "nope"}), 10).is_err());
    }

    #[test]
    fn deepseek_fetch_result_parsing() {
        let payload = json!({
            "url": "https://example.com/page",
            "title": "  Example   Page ",
            "content": "# 标题\n\n正文内容"
        });
        let fetched = parse_deepseek_fetch(&payload).expect("应能解析");
        assert_eq!(fetched.url, "https://example.com/page");
        assert_eq!(fetched.title.as_deref(), Some("Example Page"));
        assert!(fetched.content.contains("正文内容"));
        // 空正文应报错
        assert!(parse_deepseek_fetch(&json!({"content": ""})).is_err());
        assert!(parse_deepseek_fetch(&json!({})).is_err());
    }

    #[test]
    fn deepseek_error_message_chinese() {
        let message = deepseek_error_message(402, r#"{"error":{"message":"Insufficient Balance"}}"#);
        assert!(message.contains("余额不足"), "{message}");
        assert!(message.contains("Insufficient Balance"), "{message}");
        let message = deepseek_error_message(429, "rate limited");
        assert!(message.contains("请求过多"), "{message}");
        let message = deepseek_error_message(500, "");
        assert!(message.contains("服务端错误"), "{message}");
    }

    #[test]
    fn deepseek_key_reads_settings() {
        let mut settings = Settings::default();
        settings.server.api_key = "  sk-test  ".to_string();
        assert_eq!(deepseek_key(&settings), Some("sk-test".to_string()));
        settings.server.api_key.clear();
        assert_eq!(deepseek_key(&settings), None);
    }

    #[tokio::test]
    async fn deepseek_calls_without_key_error_early() {
        // 没有 api_key 时必须在发请求前就返回中文错误（web_search 据此回退聚合）
        let settings = Settings::default();
        let client = build_client(&settings.web).expect("客户端应能创建");
        let search_error = search_deepseek(&client, &settings, "rust", 5)
            .await
            .expect_err("无 key 应报错");
        assert!(
            search_error.contains("未配置 DeepSeek API key"),
            "{search_error}"
        );
        let fetch_error = fetch_deepseek_markdown(&client, &settings, "https://example.com")
            .await
            .expect_err("无 key 应报错");
        assert!(
            fetch_error.contains("未配置 DeepSeek API key"),
            "{fetch_error}"
        );
    }

    #[tokio::test]
    async fn error_paths_are_chinese_errors_not_panics() {
        let root = crate::test_support::temp_root("web-errors");
        let (ctx, _host) = crate::test_support::fake_ctx(&root);
        // 未知 provider：中文错误，不发请求
        let bad_provider = web_search(&ctx, json!({"query": "x", "provider": "nope"})).await;
        assert!(!bad_provider.ok);
        assert!(bad_provider.content.contains("不认识的 provider"), "{}", bad_provider.content);
        // 非 http/https scheme：中文错误
        let bad_scheme = web_fetch(&ctx, json!({"url": "file:///etc/passwd"})).await;
        assert!(!bad_scheme.ok);
        assert!(bad_scheme.content.contains("只支持 http/https"), "{}", bad_scheme.content);
        // 非法 URL：中文错误
        let bad_url = web_fetch(&ctx, json!({"url": "not a url"})).await;
        assert!(!bad_url.ok);
        assert!(bad_url.content.contains("URL 非法"), "{}", bad_url.content);
        // format 非法
        let bad_format = web_fetch(&ctx, json!({"url": "https://example.com", "format": "xml"})).await;
        assert!(!bad_format.ok);
        assert!(bad_format.content.contains("format 只能是"), "{}", bad_format.content);
        // provider=none：明确提示，不发请求
        let none = web_search(&ctx, json!({"query": "rust", "provider": "none"})).await;
        assert!(!none.ok, "{}", none.content);
        assert!(none.content.contains("none"), "{}", none.content);
    }
}
