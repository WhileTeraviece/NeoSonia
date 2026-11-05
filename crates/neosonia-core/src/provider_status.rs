//! 模型提供商状态页检测（Statuspage 标准）。
//!
//! 用 `Settings::effective_server().base_url` 判断厂商，拉官方状态页：
//! `https://<status host>/api/v2/status.json`（必须）与 `summary.json`（可选，补充事件/故障组件名）。
//! 60 秒缓存 + 单飞（并发调用只打一次网络）；失败返回 `unknown`，**不当作故障**，避免误报。
//!
//! OWNER: core-llm 队友。

use std::time::Duration;

use parking_lot::Mutex;
use serde_json::Value;

use crate::config::Settings;
use crate::util::now_ms;

/// 支持的厂商（未知厂商不显示状态小球）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderKind {
    DeepSeek,
    OpenAI,
    Anthropic,
    Groq,
    Mistral,
    Cohere,
}

impl ProviderKind {
    /// 根据 base_url 判断厂商；未知返回 None。
    pub fn from_base_url(base_url: &str) -> Option<Self> {
        let lower = base_url.trim().to_ascii_lowercase();
        if lower.is_empty() {
            return None;
        }
        if lower.contains("api.deepseek.com") {
            Some(ProviderKind::DeepSeek)
        } else if lower.contains("api.openai.com") {
            Some(ProviderKind::OpenAI)
        } else if lower.contains("api.anthropic.com") {
            Some(ProviderKind::Anthropic)
        } else if lower.contains("api.groq.com") {
            Some(ProviderKind::Groq)
        } else if lower.contains("api.mistral.ai") {
            Some(ProviderKind::Mistral)
        } else if lower.contains("api.cohere.com") || lower.contains("api.cohere.ai") {
            Some(ProviderKind::Cohere)
        } else {
            None
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            ProviderKind::DeepSeek => "DeepSeek",
            ProviderKind::OpenAI => "OpenAI",
            ProviderKind::Anthropic => "Anthropic",
            ProviderKind::Groq => "Groq",
            ProviderKind::Mistral => "Mistral",
            ProviderKind::Cohere => "Cohere",
        }
    }

    /// 给人看的状态页地址。
    pub fn status_url(&self) -> &'static str {
        match self {
            ProviderKind::DeepSeek => "https://status.deepseek.com",
            ProviderKind::OpenAI => "https://status.openai.com",
            ProviderKind::Anthropic => "https://status.anthropic.com",
            ProviderKind::Groq => "https://groqstatus.com",
            ProviderKind::Mistral => "https://status.mistral.ai",
            ProviderKind::Cohere => "https://status.cohere.com",
        }
    }
}

/// 一次状态检测结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderStatus {
    /// 厂商名，例如 "DeepSeek"。
    pub provider: String,
    /// `green` / `yellow` / `orange` / `red` / `unknown`。
    pub indicator: String,
    /// 中文状态："正常" / "轻微故障" / "部分中断" / "严重中断" / "未知"。
    pub label: String,
    /// 人看的 status 页面。
    pub url: String,
    pub checked_at_ms: i64,
    /// 细节：故障事件名/组件名；失败时为中文原因；可为空。
    pub detail: String,
}

impl ProviderStatus {
    pub fn unknown(provider: &str, url: &str, detail: impl Into<String>) -> Self {
        Self {
            provider: provider.to_string(),
            indicator: "unknown".to_string(),
            label: "未知".to_string(),
            url: url.to_string(),
            checked_at_ms: now_ms(),
            detail: detail.into(),
        }
    }

    /// 是否需要注意（yellow/orange/red）。
    pub fn requires_attention(&self) -> bool {
        requires_attention(&self.indicator)
    }

    /// 是否严重中断（critical）。
    pub fn is_down(&self) -> bool {
        self.indicator == "red"
    }

    /// 顶栏小球用的 (provider, label, indicator)。
    pub fn short(&self) -> (String, String, String) {
        (
            self.provider.clone(),
            self.label.clone(),
            self.indicator.clone(),
        )
    }
}

/// indicator 是否为需要注意的故障级别。
pub fn requires_attention(indicator: &str) -> bool {
    matches!(indicator, "yellow" | "orange" | "red")
}

/// Statuspage indicator → (颜色, 中文标签)。
pub(crate) fn map_indicator(raw: &str) -> (&'static str, &'static str) {
    match raw.trim().to_ascii_lowercase().as_str() {
        "none" | "operational" => ("green", "正常"),
        "minor" | "degraded_performance" | "maintenance" => ("yellow", "轻微故障"),
        "major" | "partial_outage" => ("orange", "部分中断"),
        "critical" | "major_outage" => ("red", "严重中断"),
        _ => ("unknown", "未知"),
    }
}

// ============================================================
// 解析（纯函数，可单测）
// ============================================================

/// 解析 status.json，返回 Statuspage 原始 indicator（如 "none"）。
pub(crate) fn parse_status_json(value: &Value) -> Option<String> {
    value
        .get("status")
        .and_then(|status| status.get("indicator"))
        .and_then(|indicator| indicator.as_str())
        .map(|indicator| indicator.trim().to_string())
        .filter(|indicator| !indicator.is_empty())
}

/// 解析 summary.json：优先 incidents[0].name，其次非 operational 的组件名。
pub(crate) fn detail_from_summary(value: &Value) -> Option<String> {
    if let Some(name) = value
        .get("incidents")
        .and_then(|incidents| incidents.as_array())
        .and_then(|incidents| incidents.first())
        .and_then(|incident| incident.get("name"))
        .and_then(|name| name.as_str())
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        return Some(name.to_string());
    }
    let degraded: Vec<String> = value
        .get("components")
        .and_then(|components| components.as_array())
        .map(|components| {
            components
                .iter()
                .filter(|component| {
                    component
                        .get("status")
                        .and_then(|status| status.as_str())
                        .map(|status| !status.eq_ignore_ascii_case("operational"))
                        .unwrap_or(false)
                })
                .filter_map(|component| component.get("name").and_then(|name| name.as_str()))
                .map(|name| name.trim().to_string())
                .filter(|name| !name.is_empty())
                .take(3)
                .collect()
        })
        .unwrap_or_default();
    if degraded.is_empty() {
        None
    } else {
        Some(degraded.join("、"))
    }
}

/// status.json 里的 status.description（可空的英文描述）。
pub(crate) fn description_from_status_json(value: &Value) -> Option<String> {
    value
        .get("status")
        .and_then(|status| status.get("description"))
        .and_then(|description| description.as_str())
        .map(str::trim)
        .filter(|description| !description.is_empty())
        .map(|description| description.to_string())
}

// ============================================================
// 缓存 + 单飞
// ============================================================

struct CachedStatus {
    key: String,
    status: ProviderStatus,
    fetched_at_ms: i64,
}

static CACHE: Mutex<Option<CachedStatus>> = Mutex::new(None);
static FETCH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const CACHE_TTL_MS: i64 = 60_000;
const STATUS_TIMEOUT: Duration = Duration::from_secs(8);
const SUMMARY_TIMEOUT: Duration = Duration::from_secs(4);

fn cache_key(kind: ProviderKind) -> String {
    kind.label().to_string()
}

fn cached_status(key: &str) -> Option<ProviderStatus> {
    let guard = CACHE.lock();
    let cached = guard.as_ref()?;
    if cached.key != key || now_ms().saturating_sub(cached.fetched_at_ms) >= CACHE_TTL_MS {
        return None;
    }
    Some(cached.status.clone())
}

fn store_status(key: &str, status: ProviderStatus) {
    *CACHE.lock() = Some(CachedStatus {
        key: key.to_string(),
        status,
        fetched_at_ms: now_ms(),
    });
}

/// 按当前生效配置查厂商状态；未知厂商返回 None；网络失败返回 `unknown`（不误报）。
///
/// 60 秒缓存 + 单飞：并发调用最多打一次状态页。
pub async fn current_status(settings: &Settings) -> Option<ProviderStatus> {
    let kind = ProviderKind::from_base_url(&settings.effective_server().base_url)?;
    let key = cache_key(kind);
    if let Some(status) = cached_status(&key) {
        return Some(status);
    }
    let _guard = FETCH_LOCK.lock().await;
    // 等到锁之后再看一次缓存（别人可能刚拉过）。
    if let Some(status) = cached_status(&key) {
        return Some(status);
    }
    let status = fetch_status(kind).await;
    store_status(&key, status.clone());
    Some(status)
}

fn http_client() -> Option<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(STATUS_TIMEOUT)
        .user_agent("NeoSonia/0.7 (+provider-status)")
        .build()
        .ok()
}

async fn fetch_status(kind: ProviderKind) -> ProviderStatus {
    let provider = kind.label().to_string();
    let page_url = kind.status_url().to_string();
    let Some(client) = http_client() else {
        return ProviderStatus::unknown(&provider, &page_url, "创建 HTTP 客户端失败");
    };
    // DeepSeek 不是 Statuspage v2：改用 Atom/RSS + HTML 关键词适配器。
    if kind == ProviderKind::DeepSeek {
        return fetch_deepseek_status(&client, &provider, &page_url).await;
    }
    let status_api = format!("{page_url}/api/v2/status.json");
    let response = match client.get(&status_api).send().await {
        Ok(response) => response,
        Err(error) => {
            let reason = if error.is_timeout() {
                "状态页请求超时（8 秒）"
            } else if error.is_connect() {
                "无法连接状态页（网络/代理问题）"
            } else {
                "状态页请求失败"
            };
            return ProviderStatus::unknown(&provider, &page_url, reason);
        }
    };
    if !response.status().is_success() {
        return ProviderStatus::unknown(
            &provider,
            &page_url,
            format!("状态页返回 HTTP {}", response.status().as_u16()),
        );
    }
    let raw = match response.text().await {
        Ok(raw) => raw,
        Err(_) => return ProviderStatus::unknown(&provider, &page_url, "读取状态页响应失败"),
    };
    let value: Value = match serde_json::from_str(&raw) {
        Ok(value) => value,
        Err(_) => {
            return ProviderStatus::unknown(&provider, &page_url, "状态页返回的不是合法 JSON");
        }
    };
    let Some(raw_indicator) = parse_status_json(&value) else {
        return ProviderStatus::unknown(&provider, &page_url, "状态页缺少 status.indicator");
    };
    let (indicator, label) = map_indicator(&raw_indicator);

    // 可选 summary.json：拿故障事件名/异常组件名，失败不影响结论。
    let mut detail = description_from_status_json(&value).unwrap_or_default();
    if requires_attention(indicator) {
        let summary_api = format!("{page_url}/api/v2/summary.json");
        let summary = tokio::time::timeout(SUMMARY_TIMEOUT, client.get(&summary_api).send()).await;
        if let Ok(Ok(response)) = summary
            && response.status().is_success()
        {
            if let Ok(text) = response.text().await {
                if let Ok(summary_value) = serde_json::from_str::<Value>(&text) {
                    if let Some(name) = detail_from_summary(&summary_value) {
                        detail = name;
                    }
                }
            }
        }
        if detail.is_empty() {
            detail = label.to_string();
        }
    }

    ProviderStatus {
        provider,
        indicator: indicator.to_string(),
        label: label.to_string(),
        url: page_url,
        checked_at_ms: now_ms(),
        detail,
    }
}

// ============================================================
// DeepSeek 专用适配器（Statuspage v2 不可用）
// ============================================================

/// DeepSeek 官方状态页的 Atom/RSS 路径（按可靠性排序）。
const DEEPSEEK_FEED_PATHS: [&str; 3] = ["/feed.atom", "/atom.xml", "/rss.xml"];

/// DeepSeek 的 HTML 当前状态区域只看开头这么多字符，避免把历史事件标题当成当前状态。
const HTML_STATUS_WINDOW_CHARS: usize = 4000;

async fn fetch_deepseek_status(
    client: &reqwest::Client,
    provider: &str,
    page_url: &str,
) -> ProviderStatus {
    // 1) 优先读 HTML 页面的"当前状态"headline（中文/英文都支持）。
    match client.get(page_url).send().await {
        Ok(response) if response.status().is_success() => {
            if let Ok(html) = response.text().await {
                if let Some(parsed) = parse_html_status(&html) {
                    return parsed.into_status(provider, page_url);
                }
            }
        }
        _ => {}
    }
    // 2) HTML 没解析出来：Atom/RSS 兜底，只看最新一条 entry/item。
    for path in DEEPSEEK_FEED_PATHS {
        let feed_url = format!("{page_url}{path}");
        let Ok(response) = client.get(&feed_url).send().await else {
            continue;
        };
        if !response.status().is_success() {
            continue;
        }
        let Ok(text) = response.text().await else {
            continue;
        };
        if let Some(parsed) = parse_atom_status(&text) {
            return parsed.into_status(provider, page_url);
        }
    }
    ProviderStatus::unknown(provider, page_url, "无法从状态页/Atom 判断当前状态")
}

/// 解析出来的状态（用于 DeepSeek 适配器内部传递）。
pub(crate) struct ParsedStatus {
    indicator: &'static str,
    label: &'static str,
    /// 细节：当前状态短语或最新事件标题。
    detail: String,
}

impl ParsedStatus {
    fn into_status(self, provider: &str, page_url: &str) -> ProviderStatus {
        let mut detail = self.detail.trim().to_string();
        if detail.is_empty() {
            detail = self.label.to_string();
        }
        ProviderStatus {
            provider: provider.to_string(),
            indicator: self.indicator.to_string(),
            label: self.label.to_string(),
            url: page_url.to_string(),
            checked_at_ms: now_ms(),
            detail,
        }
    }
}

/// 状态词表：(匹配串, indicator, 中文标签, 展示短语)。顺序用于同等位置时的优先级。
const STATUS_PHRASES: &[(&str, &str, &str, &str)] = &[
    ("major outage", "red", "严重中断", "Major Outage"),
    ("critical", "red", "严重中断", "Critical"),
    ("严重中断", "red", "严重中断", "严重中断"),
    ("重大故障", "red", "严重中断", "重大故障"),
    ("partial outage", "orange", "部分中断", "Partial Outage"),
    ("部分中断", "orange", "部分中断", "部分中断"),
    (
        "partially unavailable",
        "orange",
        "部分中断",
        "Partially Unavailable",
    ),
    (
        "degraded performance",
        "yellow",
        "轻微故障",
        "Degraded Performance",
    ),
    ("性能下降", "yellow", "轻微故障", "性能下降"),
    (
        "all systems are operating normally",
        "green",
        "正常",
        "All systems are operating normally",
    ),
    (
        "all systems operational",
        "green",
        "正常",
        "All Systems Operational",
    ),
    ("一切运行正常", "green", "正常", "一切运行正常"),
    (
        "所有系统均按预期运行",
        "green",
        "正常",
        "所有系统均按预期运行",
    ),
    ("所有系统运行正常", "green", "正常", "所有系统运行正常"),
    ("operational", "green", "正常", "Operational"),
];

/// 在"当前状态区域"文本里找位置最靠前的状态词（headline 优先，历史事件在后面）。
fn first_status_phrase(text: &str) -> Option<(&'static str, &'static str, String)> {
    let lower = text.to_ascii_lowercase();
    let mut best: Option<(usize, &'static str, &'static str, &str)> = None;
    for (needle, indicator, label, phrase) in STATUS_PHRASES {
        if let Some(index) = lower.find(&needle.to_ascii_lowercase()) {
            let better = match best {
                Some((best_index, ..)) => index < best_index,
                None => true,
            };
            if better {
                best = Some((index, indicator, label, phrase));
            }
        }
    }
    best.map(|(_, indicator, label, phrase)| (indicator, label, phrase.to_string()))
}

/// 把 HTML 变成可见文本（去掉 script/style/注释/标签），避免在翻译字典里误命中。
fn visible_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 3);
    let lower = html.to_ascii_lowercase();
    let mut index = 0usize;
    while index < html.len() {
        let rest = &lower[index..];
        let next_tag = rest.find('<').map(|offset| index + offset);
        let Some(tag_start) = next_tag else {
            out.push_str(&html[index..]);
            break;
        };
        // 普通文本先收下。
        out.push_str(&html[index..tag_start]);
        let tag_rest = &lower[tag_start..];
        let tag_end = match tag_rest.find('>') {
            Some(offset) => tag_start + offset + 1,
            None => html.len(),
        };
        // 跳过 script/style/注释整块。
        let skip_until = if tag_rest.starts_with("<script") {
            lower[tag_end..]
                .find("</script>")
                .map(|offset| tag_end + offset + "</script>".len())
        } else if tag_rest.starts_with("<style") {
            lower[tag_end..]
                .find("</style>")
                .map(|offset| tag_end + offset + "</style>".len())
        } else if tag_rest.starts_with("<!--") {
            lower[tag_end..]
                .find("-->")
                .map(|offset| tag_end + offset + 3)
        } else {
            None
        };
        index = skip_until.unwrap_or(tag_end);
        out.push(' ');
    }
    decode_entities(&out)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// 从 HTML 的当前状态区域解析（只扫页面开头窗口，不全文搜索）。
pub(crate) fn parse_html_status(html: &str) -> Option<ParsedStatus> {
    let visible = visible_text(html);
    let window: String = visible.chars().take(HTML_STATUS_WINDOW_CHARS).collect();
    let (indicator, label, phrase) = first_status_phrase(&window)?;
    Some(ParsedStatus {
        indicator,
        label,
        detail: phrase,
    })
}

/// 从 Atom/RSS 解析：**只看最新一条** entry/item；已解决/恢复优先判正常。
pub(crate) fn parse_atom_status(text: &str) -> Option<ParsedStatus> {
    let Some((title, summary)) = first_feed_entry(text) else {
        // 没有条目时，只认 feed 标题里的正常状态词（不当作故障）。
        let feed_title = extract_tag_text(text, "title").unwrap_or_default();
        let lower = feed_title.to_ascii_lowercase();
        if lower.contains("all systems operational")
            || lower.contains("operational")
            || feed_title.contains("一切运行正常")
        {
            return Some(ParsedStatus {
                indicator: "green",
                label: "正常",
                detail: feed_title,
            });
        }
        return None;
    };
    let combined = format!("{title} {summary}").to_ascii_lowercase();
    // 最新一条如果写着 resolved/已恢复 → 当前正常（历史故障标题不再误报）。
    let resolved = [
        "resolved",
        "已解决",
        "已恢复",
        "服务已恢复",
        "问题已解决",
        "operational",
        "operating normally",
        "一切运行正常",
        "所有系统均按预期运行",
    ]
    .iter()
    .any(|needle| combined.contains(needle));
    let detail = if title.trim().is_empty() {
        "最新状态更新".to_string()
    } else if resolved {
        format!("{title}（已恢复）")
    } else {
        title.clone()
    };
    if resolved {
        return Some(ParsedStatus {
            indicator: "green",
            label: "正常",
            detail,
        });
    }
    // 未恢复：按最新条目里的故障词判断（严重优先）。
    let red = ["major outage", "critical", "严重中断", "重大故障"];
    let orange = ["partial outage", "partially unavailable", "部分中断"];
    let yellow = ["degraded performance", "性能下降", "degraded"];
    if red.iter().any(|needle| combined.contains(needle)) {
        return Some(ParsedStatus {
            indicator: "red",
            label: "严重中断",
            detail,
        });
    }
    if orange.iter().any(|needle| combined.contains(needle)) {
        return Some(ParsedStatus {
            indicator: "orange",
            label: "部分中断",
            detail,
        });
    }
    if yellow.iter().any(|needle| combined.contains(needle)) {
        return Some(ParsedStatus {
            indicator: "yellow",
            label: "轻微故障",
            detail,
        });
    }
    None
}

/// 取最新一条 `<entry>` / `<item>` 的 (标题, 摘要文本)。
fn first_feed_entry(text: &str) -> Option<(String, String)> {
    let lower = text.to_ascii_lowercase();
    let (start, end_tag) = if let Some(index) = lower.find("<entry") {
        (index, "</entry>")
    } else if let Some(index) = lower.find("<item") {
        (index, "</item>")
    } else {
        return None;
    };
    let block_end = lower[start..]
        .find(end_tag)
        .map(|offset| start + offset)
        .unwrap_or(lower.len());
    let block = &text[start..block_end];
    let title = extract_tag_text(block, "title").unwrap_or_default();
    let summary = extract_tag_text(block, "summary")
        .or_else(|| extract_tag_text(block, "content"))
        .unwrap_or_default();
    if title.is_empty() && summary.is_empty() {
        return None;
    }
    Some((title, summary))
}

/// 取某个标签里的文本（不区分大小写，取第一个）。
fn extract_tag_text(text: &str, tag: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let start = lower.find(&open)?;
    let content_start = lower[start..].find('>').map(|offset| start + offset + 1)?;
    let end = lower[content_start..]
        .find(&close)
        .map(|offset| content_start + offset)?;
    let raw = text[content_start..end].trim();
    let raw = raw
        .trim_start_matches("<![CDATA[")
        .trim_end_matches("]]>")
        .trim();
    let cleaned = strip_html_tags(raw);
    let cleaned = decode_entities(&cleaned);
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

/// 去掉标签，保留文本。
fn strip_html_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_tag = false;
    for ch in text.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 解常见的 HTML 实体。
fn decode_entities(text: &str) -> String {
    text.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#xA;", " ")
        .replace("&nbsp;", " ")
}

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn detects_provider_from_base_url() {
        assert_eq!(
            ProviderKind::from_base_url("https://api.deepseek.com"),
            Some(ProviderKind::DeepSeek)
        );
        assert_eq!(
            ProviderKind::from_base_url("https://api.openai.com/v1"),
            Some(ProviderKind::OpenAI)
        );
        assert_eq!(
            ProviderKind::from_base_url("https://api.anthropic.com"),
            Some(ProviderKind::Anthropic)
        );
        assert_eq!(
            ProviderKind::from_base_url("https://api.groq.com/openai/v1"),
            Some(ProviderKind::Groq)
        );
        assert_eq!(
            ProviderKind::from_base_url("https://api.mistral.ai/v1"),
            Some(ProviderKind::Mistral)
        );
        assert_eq!(
            ProviderKind::from_base_url("https://api.cohere.ai"),
            Some(ProviderKind::Cohere)
        );
        assert_eq!(
            ProviderKind::from_base_url("http://127.0.0.1:11434/v1"),
            None
        );
        assert_eq!(ProviderKind::from_base_url(""), None);

        assert_eq!(
            ProviderKind::DeepSeek.status_url(),
            "https://status.deepseek.com"
        );
        assert_eq!(ProviderKind::Groq.status_url(), "https://groqstatus.com");
    }

    #[test]
    fn maps_statuspage_indicators_to_colors() {
        assert_eq!(map_indicator("none"), ("green", "正常"));
        assert_eq!(map_indicator("operational"), ("green", "正常"));
        assert_eq!(map_indicator("minor"), ("yellow", "轻微故障"));
        assert_eq!(
            map_indicator("degraded_performance"),
            ("yellow", "轻微故障")
        );
        assert_eq!(map_indicator("major"), ("orange", "部分中断"));
        assert_eq!(map_indicator("partial_outage"), ("orange", "部分中断"));
        assert_eq!(map_indicator("critical"), ("red", "严重中断"));
        assert_eq!(map_indicator("major_outage"), ("red", "严重中断"));
        assert_eq!(map_indicator("something-new"), ("unknown", "未知"));

        assert!(requires_attention("yellow"));
        assert!(requires_attention("orange"));
        assert!(requires_attention("red"));
        assert!(!requires_attention("green"));
        assert!(!requires_attention("unknown"));
    }

    #[test]
    fn parses_status_and_summary_json() {
        let status =
            json!({"status": {"indicator": "major", "description": "Partial System Outage"}});
        assert_eq!(parse_status_json(&status).as_deref(), Some("major"));
        assert_eq!(
            description_from_status_json(&status).as_deref(),
            Some("Partial System Outage")
        );
        assert!(parse_status_json(&json!({})).is_none());

        let summary = json!({
            "incidents": [{"name": "API latency elevated"}],
            "components": [
                {"name": "API", "status": "major_outage"},
                {"name": "Web", "status": "operational"}
            ]
        });
        assert_eq!(
            detail_from_summary(&summary).as_deref(),
            Some("API latency elevated")
        );
        let components_only = json!({
            "incidents": [],
            "components": [
                {"name": "API", "status": "degraded_performance"},
                {"name": "Web", "status": "operational"}
            ]
        });
        assert_eq!(
            detail_from_summary(&components_only).as_deref(),
            Some("API")
        );
        assert!(detail_from_summary(&json!({})).is_none());
    }

    #[test]
    fn parses_deepseek_atom_feed() {
        let atom = r#"<?xml version="1.0" encoding="utf-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>DeepSeek Status</title>
  <entry>
    <title>API latency elevated</title>
    <summary>Degraded Performance: some requests are slower than usual.</summary>
  </entry>
</feed>"#;
        let parsed = parse_atom_status(atom).expect("最新条目应识别 Degraded Performance");
        assert_eq!(parsed.indicator, "yellow");
        assert_eq!(parsed.label, "轻微故障");
        let status = parsed.into_status("DeepSeek", "https://status.deepseek.com");
        assert_eq!(status.indicator, "yellow");
        assert!(status.detail.contains("API latency elevated"));
        assert!(requires_attention(&status.indicator));

        // 没有 entry 时，feed 标题里的正常状态词判 green。
        let green = r#"<feed><title>All Systems Operational</title></feed>"#;
        let parsed = parse_atom_status(green).expect("应识别正常");
        assert_eq!(parsed.indicator, "green");
        assert_eq!(parsed.label, "正常");

        // 最新条目是故障 → 按故障级别；严重优先。
        let red = r#"<feed>
          <title>Operational</title>
          <entry><title>Elevated error rates</title><summary>Major Outage</summary></entry>
        </feed>"#;
        let parsed = parse_atom_status(red).expect("应识别 Major Outage");
        assert_eq!(parsed.indicator, "red");

        // RSS item 也能解析。
        let rss = r#"<rss><channel>
          <item><title>Partial Outage: EU region</title></item>
        </channel></rss>"#;
        let parsed = parse_atom_status(rss).expect("应识别 Partial Outage");
        assert_eq!(parsed.indicator, "orange");

        // 没有状态词 → None（不误报）。
        assert!(parse_atom_status("<feed><title>DeepSeek</title></feed>").is_none());
    }

    #[test]
    fn atom_latest_resolved_entry_is_green_despite_old_degraded_history() {
        // 重点回归：feed 里历史条目含 degraded/outage，但最新一条已 resolved → 必须 green。
        let feed = r#"<feed>
          <title>DeepSeek</title>
          <entry>
            <title>DeepSeek 网页/API 部分中断（DeepSeek Web/API Partially Unavailable）</title>
            <summary>&lt;p&gt;&lt;strong&gt;Status:&lt;/strong&gt; resolved&lt;/p&gt;
              本次问题已解决，服务已恢复。</summary>
          </entry>
          <entry>
            <title>DeepSeek API 性能下降（DeepSeek API Degraded Performance）</title>
            <summary>&lt;strong&gt;Status:&lt;/strong&gt; resolved</summary>
          </entry>
        </feed>"#;
        let parsed = parse_atom_status(feed).expect("最新条目已解决 → 应识别正常");
        assert_eq!(parsed.indicator, "green", "历史故障不能误报");
        assert_eq!(parsed.label, "正常");
        let status = parsed.into_status("DeepSeek", "https://status.deepseek.com");
        assert!(
            status.detail.contains("已恢复"),
            "detail = {}",
            status.detail
        );
        assert!(!requires_attention(&status.indicator));

        // 最新一条是进行中的故障（无 resolved）→ 仍要报故障。
        let ongoing = r#"<feed>
          <entry><title>DeepSeek API 性能下降</title>
            <summary>Status: investigating</summary></entry>
        </feed>"#;
        let parsed = parse_atom_status(ongoing).expect("进行中的故障应识别");
        assert_eq!(parsed.indicator, "yellow");
    }

    #[test]
    fn parses_deepseek_html_status_keywords() {
        let html = r#"<!doctype html><html><head><title>DeepSeek Status</title></head>
<body><main><span class="label">All Systems Operational</span></main></body></html>"#;
        let parsed = parse_html_status(html).expect("HTML 当前状态应命中");
        assert_eq!(parsed.indicator, "green");
        let status = parsed.into_status("DeepSeek", "https://status.deepseek.com");
        assert_eq!(status.label, "正常");
        assert_eq!(status.provider, "DeepSeek");

        let degraded = r#"<html><body><p>Degraded Performance</p></body></html>"#;
        let parsed = parse_html_status(degraded).expect("应识别降级");
        assert_eq!(parsed.indicator, "yellow");

        let partial = r#"<html><body><p>Partial Outage</p></body></html>"#;
        assert_eq!(parse_html_status(partial).unwrap().indicator, "orange");

        let major = r#"<html><body><p>Major Outage</p></body></html>"#;
        assert_eq!(parse_html_status(major).unwrap().indicator, "red");

        // 页面里没有关键词 → None，调用方会返回 unknown。
        assert!(parse_html_status("<html><body>Welcome</body></html>").is_none());
    }

    #[test]
    fn chinese_status_page_headline_wins_over_history() {
        // 中文当前状态页：顶部 headline 正常，历史事件在后面 → 必须 green。
        let html = r#"<!doctype html><html><head><title>DeepSeek 状态</title>
<script>self.__NEXT_DATA__={"messages":{"operational":"All Systems Operational"}}</script>
</head><body>
<header>DeepSeek 状态</header>
<section class="status-card"><h1>一切运行正常</h1>
  <p>所有系统均按预期运行</p></section>
<section class="history">
  <div>历史事件：DeepSeek API 性能下降（Degraded Performance）</div>
  <div>历史事件：DeepSeek 网页/API 部分中断（Partially Unavailable）</div>
</section>
</body></html>"#;
        let parsed = parse_html_status(html).expect("中文当前状态应识别");
        assert_eq!(parsed.indicator, "green", "历史故障不能误报");
        let status = parsed.into_status("DeepSeek", "https://status.deepseek.com");
        assert_eq!(status.label, "正常");

        // 英文"一切正常"也要识别。
        let english = r#"<html><body>
          <h1>All systems are operating normally</h1>
          <p>All Systems Operational</p>
        </body></html>"#;
        assert_eq!(parse_html_status(english).unwrap().indicator, "green");

        // 当前 headline 是黄色时，即使后面有绿色词也优先 headline（位置序）。
        let degraded = r#"<html><body>
          <h1>性能下降</h1><p>历史：All Systems Operational</p>
        </body></html>"#;
        assert_eq!(parse_html_status(degraded).unwrap().indicator, "yellow");
    }

    #[test]
    fn deepseek_routes_to_adapter() {
        // DeepSeek 仍能被识别（provider 检测用 effective_server().base_url 前缀）。
        assert_eq!(
            ProviderKind::from_base_url("https://api.deepseek.com/v1"),
            Some(ProviderKind::DeepSeek)
        );
        assert_eq!(
            ProviderKind::DeepSeek.status_url(),
            "https://status.deepseek.com"
        );
    }

    #[test]
    fn unknown_status_never_counts_as_attention() {
        let status = ProviderStatus::unknown("DeepSeek", "https://status.deepseek.com", "超时");
        assert_eq!(status.indicator, "unknown");
        assert_eq!(status.label, "未知");
        assert!(!status.requires_attention());
        assert!(!status.is_down());
        assert_eq!(
            status.short(),
            (
                "DeepSeek".to_string(),
                "未知".to_string(),
                "unknown".to_string()
            )
        );
    }
}
