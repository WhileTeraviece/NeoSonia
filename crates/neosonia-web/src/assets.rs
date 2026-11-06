//! 内嵌前端资源（离线可用，不引用任何 CDN）。
//!
//! 四个文件在编译期用 `include_str!` 打进二进制，运行时不需要读磁盘，
//! 也就不存在被篡改的风险。所有响应都带安全头（见 [`crate::server`]）。
//!
//! 缓存策略：
//! - `index.html` → `Cache-Control: no-cache`（每次进页面重新验证，配对状态/版本及时更新）；
//! - `app.js` / `style.css` / `spa-core.js` → `immutable` 长缓存，
//!   URL 带 `?v=<APP_VERSION>` 做版本击穿（升级后 URL 变化，自动重新下载）。

use std::sync::OnceLock;

use base64::Engine as _;
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};

const INDEX_HTML: &str = include_str!("../assets/web/index.html");
const APP_JS: &str = include_str!("../assets/web/app.js");
const STYLE_CSS: &str = include_str!("../assets/web/style.css");
const SPA_CORE_JS: &str = include_str!("../assets/web/spa-core.js");
/// v0.7：Markdown 表格/公式/代码块渲染核心（纯逻辑，可 Node 单测）。
const MD_CORE_JS: &str = include_str!("../assets/web/md-core.js");

/// HTML 缓存头：每次进页面都要重新验证，避免设备配对状态/版本过期。
pub const HTML_CACHE: &str = "no-cache";
/// 静态资源缓存头：URL 带版本号，可以长期缓存。
pub const ASSET_CACHE: &str = "public, max-age=31536000, immutable";

/// 项目 logo（64x64 PNG），内嵌成 `data:` URI 作为 favicon，避免额外请求/外部路径。
const ICON_PNG: &[u8] = include_bytes!("../../../assets/icon-64.png");
/// 同一张 logo 放到 SPA 资源目录，配对页顶部品牌位用 `<img>` 显示（可缓存）。
const LOGO_PNG: &[u8] = include_bytes!("../assets/web/logo-64.png");

/// favicon 的 data URI（编译期把 PNG 打进二进制，运行时 base64 一次）。
fn favicon_data_uri() -> String {
    static URI: OnceLock<String> = OnceLock::new();
    URI.get_or_init(|| {
        let encoded = base64::engine::general_purpose::STANDARD.encode(ICON_PNG);
        format!("data:image/png;base64,{encoded}")
    })
    .clone()
}

/// 静态资源版本号：`<APP_VERSION>-<资源内容哈希前 12 位>`。
///
/// 资源用 `immutable` 长缓存，靠 URL 里的 `?v=` 做缓存击穿；版本号带内容哈希后，
/// 即使 app 版本号没变（开发期重建），SPA/样式变化也会自动换 URL，避免用户拿到旧文件。
fn asset_version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION.get_or_init(|| {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(APP_JS.as_bytes());
        hasher.update(STYLE_CSS.as_bytes());
        hasher.update(SPA_CORE_JS.as_bytes());
        hasher.update(MD_CORE_JS.as_bytes());
        hasher.update(LOGO_PNG);
        let digest = hex::encode(hasher.finalize());
        format!("{}-{}", crate::VERSION, &digest[..12])
    })
}

/// index.html 里用占位符替换：favicon data URI + 静态资源版本号（做缓存击穿）。
fn page_html() -> &'static str {
    static PAGE: OnceLock<String> = OnceLock::new();
    PAGE.get_or_init(|| {
        INDEX_HTML
            .replace("{{FAVICON_DATA_URI}}", &favicon_data_uri())
            .replace("{{APP_VERSION}}", asset_version())
    })
}

fn resource(content_type: &'static str, body: &'static str, cache: &'static str) -> Response {
    let mut res = body.into_response();
    res.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    res
}

/// `GET /` 与 `GET /pair`：同一个 SPA（favicon 已替换成 data URI、资源 URL 带版本号）。
pub async fn index() -> Response {
    resource("text/html; charset=utf-8", page_html(), HTML_CACHE)
}

/// `GET /app.js?v=`
pub async fn app_js() -> Response {
    resource("application/javascript; charset=utf-8", APP_JS, ASSET_CACHE)
}

/// `GET /spa-core.js?v=`：首屏必要的纯逻辑（心跳/重连/配对文案），可被 Node 单测。
pub async fn spa_core_js() -> Response {
    resource(
        "application/javascript; charset=utf-8",
        SPA_CORE_JS,
        ASSET_CACHE,
    )
}

/// `GET /md-core.js?v=`：Markdown/表格/公式渲染核心（无 DOM，可 Node 单测）。
pub async fn md_core_js() -> Response {
    resource(
        "application/javascript; charset=utf-8",
        MD_CORE_JS,
        ASSET_CACHE,
    )
}

/// `GET /style.css?v=`
pub async fn style_css() -> Response {
    resource("text/css; charset=utf-8", STYLE_CSS, ASSET_CACHE)
}

/// `GET /logo-64.png?v=`：配对页品牌位用的真实 logo（PNG 二进制，immutable）。
pub async fn logo_png() -> Response {
    let mut res = LOGO_PNG.into_response();
    res.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static(ASSET_CACHE));
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brand_logo_replaces_placeholder() {
        // v0.7.2：配对页左上角用真实 logo，不再用渐变色圆角矩形占位。
        assert!(INDEX_HTML.contains("class=\"brand-logo\""), "缺少 logo 图片");
        assert!(
            INDEX_HTML.contains("/logo-64.png?v={{APP_VERSION}}"),
            "logo 应带内容哈希版本号"
        );
        assert!(INDEX_HTML.contains("alt=\"NeoSonia\""), "logo 需要 alt");
        assert!(
            !INDEX_HTML.contains("brand-mark"),
            "不应残留旧的占位块"
        );
        assert!(!STYLE_CSS.contains(".brand-mark"), "样式不应残留占位块");
        assert!(STYLE_CSS.contains(".brand-logo"));
        assert!(LOGO_PNG.len() > 100, "logo 体积异常");
        assert_eq!(&LOGO_PNG[..8], b"\x89PNG\r\n\x1a\n", "logo 应为 PNG");
    }

    #[test]
    fn assets_are_not_empty() {
        assert!(INDEX_HTML.contains("NeoSonia"));
        assert!(APP_JS.len() > 1000);
        assert!(STYLE_CSS.len() > 1000);
        assert!(SPA_CORE_JS.len() > 1000, "spa-core.js 应非空");
        assert!(MD_CORE_JS.len() > 1000, "md-core.js 应非空");
        assert!(LOGO_PNG.starts_with(b"\x89PNG"), "logo-64.png 应是合法 PNG");
    }

    #[test]
    fn no_external_resources() {
        // 离线要求：不许引用任何 CDN / 外网资源。
        for (name, body) in [
            ("index.html", INDEX_HTML),
            ("style.css", STYLE_CSS),
            ("spa-core.js", SPA_CORE_JS),
            ("md-core.js", MD_CORE_JS),
        ] {
            // 排障文案里会示例 `http://192.168.x.x`，所以只断言没有真实的外部资源依赖。
            for bad in ["//cdn", "fonts.googleapis", "unpkg", "jsdelivr", "googleapis.com"] {
                assert!(
                    !body.contains(bad),
                    "{name} 不应引用外部资源（发现 {bad}）"
                );
            }
        }
        // 所有 script/link/style 引用必须是同源相对路径（favicon 是 data URI 占位）。
        assert!(!INDEX_HTML.contains("src=\"http"), "不允许外链脚本");
        assert!(!INDEX_HTML.contains("href=\"http"), "不允许外链样式");
        assert!(!STYLE_CSS.contains("url(http"), "CSS 不允许外链资源");
        // index.html 只引用同源的 /app.js、/spa-core.js 与 /style.css（带版本号）。
        assert!(INDEX_HTML.contains("src=\"/app.js?v="));
        assert!(INDEX_HTML.contains("src=\"/spa-core.js?v="));
        assert!(INDEX_HTML.contains("href=\"/style.css?v="));
        assert!(!INDEX_HTML.contains("src=\"//"));
    }

    #[test]
    fn static_cache_headers_and_versioning() {
        // HTML 每次重新验证；assets 长期缓存 + immutable；URL 带版本号做缓存击穿。
        assert_eq!(HTML_CACHE, "no-cache");
        assert!(ASSET_CACHE.contains("immutable"), "静态资源应为 immutable");
        assert!(ASSET_CACHE.contains("max-age=31536000"));
        let page = page_html();
        assert!(!page.contains("{{APP_VERSION}}"), "版本占位符应被替换");
        assert!(page.contains("?v="), "静态资源 URL 应带版本号");
        // 版本号 = app 版本 + 内容哈希前 12 位（重建后自动变化，避免 immutable 缓存旧 SPA）。
        let version = asset_version();
        assert!(version.starts_with(crate::VERSION), "{version}");
        assert_eq!(version.rsplit('-').next().unwrap().len(), 12, "{version}");
        assert!(page.contains(version));
        assert!(page.contains("?v="));
    }

    #[test]
    fn pair_auto_and_connection_ui_hooks_exist() {
        // v0.5：扫码自动配对 + 断线横幅 + 设备页连接状态/断开按钮。
        assert!(INDEX_HTML.contains("id=\"pair-auto\""), "缺少自动配对状态提示");
        assert!(APP_JS.contains("autoPair"), "缺少自动配对逻辑");
        assert!(APP_JS.contains("pairFailureMessage"), "缺少配对失败分类文案");
        assert!(INDEX_HTML.contains("id=\"conn-banner\""), "缺少断线重连横幅");
        assert!(INDEX_HTML.contains("id=\"conn-banner-text\""));
        assert!(APP_JS.contains("HEARTBEAT_INTERVAL_MS") || SPA_CORE_JS.contains("HEARTBEAT_INTERVAL_MS"));
        assert!(APP_JS.contains("nextReconnectDelay"), "缺少退避重连");
        assert!(INDEX_HTML.contains("id=\"dev-conn\""), "设备页缺少连接状态");
        assert!(INDEX_HTML.contains("断开本设备"), "设备页缺少断开本设备按钮");
        assert!(APP_JS.contains("设备授权已失效，请在桌面端查看新配对码"), "401 文案不符合要求");
    }

    #[test]
    fn spa_core_heartbeat_state_machine_present() {
        assert!(SPA_CORE_JS.contains("RECONNECT_STEPS"));
        assert!(SPA_CORE_JS.contains("1000, 2000, 5000, 10000, 15000"));
        assert!(SPA_CORE_JS.contains("HEARTBEAT_INTERVAL_MS = 25000"));
        assert!(SPA_CORE_JS.contains("MAX_MISSED_PONGS = 3"));
        assert!(SPA_CORE_JS.contains("SILENT_RETRIES = 2"));
        assert!(SPA_CORE_JS.contains("pairFailureMessage"));
        // 纯逻辑文件不碰 DOM，方便 Node 单测。
        assert!(!SPA_CORE_JS.contains("document."));
    }

    #[test]
    fn index_has_viewport_and_charset() {
        assert!(INDEX_HTML.contains("name=\"viewport\""));
        assert!(INDEX_HTML.contains("charset=\"utf-8\""));
        assert!(INDEX_HTML.contains("lang=\"zh-CN\""));
    }

    #[test]
    fn favicon_is_embedded_data_uri() {
        let page = page_html();
        assert!(page.contains("data:image/png;base64,"), "favicon 应为内嵌 data URI");
        assert!(
            !page.contains("{{FAVICON_DATA_URI}}"),
            "favicon 占位符应被替换"
        );
        assert!(ICON_PNG.starts_with(b"\x89PNG"), "内嵌 logo 应是合法 PNG");
    }

    #[test]
    fn first_screen_skeleton_markup_exists() {
        // 骨架屏是静态 HTML：JS 还没跑（或网络慢）时也能立刻显示，避免白屏。
        assert!(INDEX_HTML.contains("id=\"list-skeleton\""), "缺少列表骨架屏");
        assert!(INDEX_HTML.contains("id=\"chat-skeleton\""), "缺少聊天骨架屏");
        assert_eq!(INDEX_HTML.matches("sk-session").count(), 3, "列表骨架应有 3 条");
        assert_eq!(INDEX_HTML.matches("sk-bubble").count(), 2, "聊天骨架应有 2 个气泡");
        // 首屏关键 CSS 内联，不依赖外部样式表先下载完。
        assert!(INDEX_HTML.contains("<style>"), "首屏关键 CSS 应内联");
        assert!(INDEX_HTML.contains("sk-shimmer"), "骨架屏动画应内联");
    }

    #[test]
    fn mode_and_review_ui_hooks_exist() {
        assert!(INDEX_HTML.contains("id=\"mode-chip\""), "缺少模式 chip");
        assert!(INDEX_HTML.contains("id=\"review-bar\""), "缺少审查警示条");
        assert!(INDEX_HTML.contains("id=\"review-list\""), "缺少审查列表");
        assert!(INDEX_HTML.contains("id=\"review-toggle\""));
        assert!(INDEX_HTML.contains("远程只能查看，不能批准") || APP_JS.contains("不能批准"));
        assert!(!INDEX_HTML.contains("批准"), "页面不应出现任何批准按钮/文案");
    }

    #[test]
    fn performance_hooks_exist() {
        assert!(INDEX_HTML.contains("id=\"perf-first-paint\""));
        assert!(INDEX_HTML.contains("id=\"perf-app\""));
        assert!(INDEX_HTML.contains("id=\"perf-sessions\""));
        assert!(INDEX_HTML.contains("id=\"perf-fetch\""));
        assert!(APP_JS.contains("__neosoniaPerf"), "应暴露 Performance 计时对象");
        assert!(APP_JS.contains("t_first_paint"));
        assert!(APP_JS.contains("t_app_ready"));
        assert!(APP_JS.contains("t_sessions_loaded"));
        assert!(APP_JS.contains("t_sessions_fetch_ms"));
    }

    #[test]
    fn spa_perf_hooks_in_js() {
        // 分页 / 合批渲染 / 骨架屏的关键实现必须存在（防止回退）。
        assert!(APP_JS.contains("DocumentFragment"));
        assert!(APP_JS.contains("createDocumentFragment"));
        assert!(APP_JS.contains("requestAnimationFrame"));
        assert!(APP_JS.contains("STREAM_FRAME_MS"));
        assert!(APP_JS.contains("MAX_RENDERED"));
        assert!(APP_JS.contains("before=") || APP_JS.contains("?before="));
        assert!(APP_JS.contains("list-skeleton"));
        assert!(APP_JS.contains("chat-skeleton"));
        assert!(APP_JS.contains("加载更早的消息"));
        assert!(APP_JS.contains("readonly") || APP_JS.contains("不能批准") || APP_JS.contains("只读"));
    }

    #[test]
    fn lan_bind_and_troubleshooting_hooks_exist() {
        // v0.6：手机连不上 —— 配对页排障 + 设备页绑定/访问地址/客户端 IP/心跳。
        assert!(INDEX_HTML.contains("id=\"pair-help\""), "缺少排障折叠");
        assert!(INDEX_HTML.contains("id=\"pair-origin\""), "缺少当前访问地址");
        assert!(INDEX_HTML.contains("id=\"pair-loopback-warn\""), "缺少回环地址警告");
        assert!(INDEX_HTML.contains("netsh advfirewall firewall add rule"), "缺少防火墙放行命令");
        assert!(INDEX_HTML.contains("AP 隔离"));
        assert!(INDEX_HTML.contains("id=\"srv-bind\""), "设备页缺少服务监听字段");
        assert!(INDEX_HTML.contains("id=\"srv-advertise\""), "设备页缺少访问地址字段");
        assert!(INDEX_HTML.contains("id=\"dev-client\""), "设备页缺少客户端 IP");
        assert!(INDEX_HTML.contains("id=\"dev-heartbeat\""), "设备页缺少最后心跳");
        assert!(APP_JS.contains("pair-origin"));
        assert!(APP_JS.contains("pair-loopback-warn"));
        assert!(APP_JS.contains("srv-bind"));
        assert!(APP_JS.contains("srv-advertise"));
        assert!(APP_JS.contains("lastHeartbeatMs"));
        // 移动端：viewport + standalone。
        assert!(INDEX_HTML.contains("name=\"viewport\""));
        assert!(INDEX_HTML.contains("apple-mobile-web-app-capable"));
        // 不能把 127.0.0.1 写死成访问地址（QR、pair 跳转、API base 都必须同源/相对）；
        // 说明文案/回环判断里出现 127.0.0.1 是允许的。
        assert!(!INDEX_HTML.contains("http://127.0.0.1"), "index.html 不应硬编码回环 URL");
        assert!(!APP_JS.contains("http://127.0.0.1"), "app.js 不应硬编码回环 URL");
        assert!(!APP_JS.contains("ws://127.0.0.1"), "app.js 不应硬编码回环 WS URL");
        assert!(!SPA_CORE_JS.contains("127.0.0.1"));
    }

    #[test]
    fn markdown_math_code_and_upload_hooks_exist() {
        // v0.7：表格/公式/代码块渲染核心与上传/重命名 UI。
        assert!(INDEX_HTML.contains("src=\"/md-core.js?v="), "index 应引用 md-core.js");
        for token in [
            "renderMarkdown",
            "renderMath",
            "splitAttachments",
            "m-frac",
            "table-wrap",
            "code-copy",
            "code-lang",
        ] {
            assert!(MD_CORE_JS.contains(token), "md-core.js 缺少 {token}");
        }
        for token in [
            ".table-wrap",
            ".code-block",
            ".math-block",
            "overflow-x: auto",
            "overflow-wrap: anywhere",
            "min-width: 0",
        ] {
            assert!(STYLE_CSS.contains(token), "style.css 缺少 {token}");
        }
        assert!(INDEX_HTML.contains("id=\"attach-btn\""), "缺少附件按钮");
        assert!(INDEX_HTML.contains("id=\"file-input\""), "缺少文件选择框");
        assert!(INDEX_HTML.contains("id=\"attach-strip\""), "缺少待发送附件条");
        assert!(APP_JS.contains("uploadFiles"), "缺少上传逻辑");
        assert!(APP_JS.contains("renderUploadStrip"), "缺少上传进度条");
        assert!(APP_JS.contains("splitAttachments"), "附件卡解析未接入");
        assert!(APP_JS.contains("renameDevice"), "缺少设备重命名");
        assert!(APP_JS.contains("code-copy"), "缺少代码复制");
        assert!(APP_JS.contains("drag-over"), "缺少拖拽上传");
    }

    #[test]
    fn provider_status_ball_hooks_exist() {
        // v0.7 收尾：顶栏 API 状态小球（绿/黄/橙/红，unknown 隐藏）+ 图例。
        assert!(INDEX_HTML.contains("id=\"provider-ball\""), "缺少顶栏状态小球");
        assert!(INDEX_HTML.contains("id=\"srv-provider\""), "设备页缺少 API 状态行");
        for legend in ["绿=正常", "黄=轻微故障", "橙=部分中断", "红=严重中断"] {
            assert!(INDEX_HTML.contains(legend), "缺少图例：{legend}");
        }
        assert!(APP_JS.contains("renderProviderStatus"));
        assert!(APP_JS.contains("provider_status"));
        // 绿色时设备页也要显示 detail（最近事件）——文案由 spa-core 统一生成。
        assert!(SPA_CORE_JS.contains("providerStatusText"));
        assert!(APP_JS.contains("core.providerStatusText"));
        for class_name in ["ind-green", "ind-yellow", "ind-orange", "ind-red"] {
            assert!(STYLE_CSS.contains(class_name), "style.css 缺少 {class_name}");
        }
    }

    #[test]
    fn composer_toolbar_links_and_lists_hooks_exist() {
        // v0.7.1：输入框上方横栏（上传按钮 + 进度占位 + API 小球），顶栏只留连接点。
        assert!(INDEX_HTML.contains("id=\"composer-tools\""), "缺少输入框横栏");
        let topbar = INDEX_HTML
            .split("id=\"conn-banner\"")
            .next()
            .unwrap_or_default();
        assert!(
            !topbar.contains("provider-ball"),
            "顶栏不应再放 API 小球（会与连接点混淆）"
        );
        let tools = INDEX_HTML
            .split("id=\"composer-tools\"")
            .nth(1)
            .unwrap_or_default();
        assert!(tools.contains("id=\"provider-ball\""), "横栏应包含 API 小球");
        assert!(tools.contains("id=\"attach-btn\""), "横栏应包含上传按钮");
        assert!(
            tools.contains("id=\"attach-progress\""),
            "横栏应包含上传进度占位"
        );
        assert!(APP_JS.contains("attach-progress"), "上传进度未接线");
        // Markdown 链接/有序列表：file:/// 代码样式、长链接换行、列表缩进。
        assert!(MD_CORE_JS.contains("file-link"));
        assert!(STYLE_CSS.contains(".composer-tools"));
        assert!(STYLE_CSS.contains(".file-link"));
        assert!(STYLE_CSS.contains(".md ol"));
        assert!(STYLE_CSS.contains("overflow-wrap: anywhere"));
    }

    #[test]
    fn drag_drop_and_remote_sync_hooks_exist() {
        // v0.7.2：拖拽上传浮层 + 桌面端处理审批后的远程同步。
        assert!(INDEX_HTML.contains("id=\"drop-overlay\""), "缺少拖拽浮层");
        assert!(INDEX_HTML.contains("id=\"drop-count\""), "缺少拖拽文件数");
        assert!(APP_JS.contains("dragHasFiles"), "缺少拖拽文件判定");
        assert!(APP_JS.contains("collectDragFiles"), "缺少 drop 文件收集");
        assert!(APP_JS.contains("dropEffect"), "dragover 未设置 dropEffect");
        assert!(APP_JS.contains("review_resolved"), "未处理 review_resolved");
        assert!(APP_JS.contains("review_cleared"), "未处理 review_cleared");
        assert!(APP_JS.contains("visibilitychange"), "后台恢复未补同步");
        assert!(STYLE_CSS.contains(".drop-overlay"), "缺少浮层样式");
    }

    #[test]
    fn wide_layout_two_column_hooks_exist() {
        // v0.7.3：宽屏双栏（左列表固定 340px + 右对话/设备），手机单栏不变。
        assert!(INDEX_HTML.contains("id=\"list-brand\""), "宽屏左栏缺少品牌位");
        assert!(
            STYLE_CSS.contains("grid-template-columns: 340px minmax(0, 1fr)"),
            "缺少双栏网格"
        );
        assert!(
            STYLE_CSS.contains("@media (min-width: 900px), (orientation: landscape) and (min-width: 768px)"),
            "缺少宽屏媒体查询"
        );
        assert!(
            STYLE_CSS.contains(".app > #list-view.hidden { display: flex !important; }"),
            "宽屏左栏应常驻"
        );
        assert!(APP_JS.contains("isWideLayout"), "缺少宽度判定");
        assert!(APP_JS.contains("matchMedia"), "缺少断点切换监听");
        assert!(SPA_CORE_JS.contains("function isWideLayout"));
    }

    #[test]
    fn icon_copy_and_new_conversation_hooks_exist() {
        // v0.7.4：复制按钮图标化 + 助手消息「复制完整回复」+ 新建对话加号图标。
        assert!(
            INDEX_HTML.contains("id=\"new-btn\"") && INDEX_HTML.contains("title=\"新建对话\""),
            "新建对话应为 + 图标按钮"
        );
        assert!(
            !INDEX_HTML.contains(">新对话<"),
            "列表页不应再显示「新对话」文字按钮"
        );
        assert!(APP_JS.contains("msg-copy"), "缺少复制完整回复按钮");
        assert!(APP_JS.contains("复制完整回复"), "缺少 tooltip 文案");
        assert!(APP_JS.contains("copyWithFeedback"), "缺少复制反馈");
        assert!(APP_JS.contains("ICON_CHECK"), "复制成功应短暂变勾");
        assert!(MD_CORE_JS.contains("aria-label=\"复制代码\""), "代码块复制应有 aria-label");
        assert!(MD_CORE_JS.contains("ic-copy"), "代码块复制应为图标");
        assert!(STYLE_CSS.contains(".msg-copy"), "缺少完整回复按钮样式");
        assert!(STYLE_CSS.contains(".msg-actions"), "缺少消息底部操作行样式");
    }

    #[test]
    fn no_emoji_in_assets() {
        // 零 emoji：检查常见 emoji 区段（代理对与符号区）。
        fn has_emoji(s: &str) -> bool {
            s.chars().any(|c| {
                let u = c as u32;
                (0x1F300..=0x1FAFF).contains(&u)      // 各类 emoji
                    || (0x2600..=0x27BF).contains(&u) // 杂项符号/装饰符号
                    || (0x1F000..=0x1F2FF).contains(&u)
            })
        }
        for (name, body) in [
            ("index.html", INDEX_HTML),
            ("app.js", APP_JS),
            ("style.css", STYLE_CSS),
            ("spa-core.js", SPA_CORE_JS),
            ("md-core.js", MD_CORE_JS),
        ] {
            assert!(!has_emoji(body), "{name} 不应包含 emoji");
        }
    }
}




