# 第二阶段契约（v0.2 增量）

> 与 `docs/contracts.md` 叠加生效。第二阶段目标：视觉输入、国内可用搜索、本地文件索引、
> 局域网远程、技能系统、对话历史、附件上传、数学公式、自启与性能/动画。

## 0. 已经改好的公共契约（Lead 已提交，其他人只读）

### 0.1 视觉输入（`neosonia_core::model`）
```rust
pub struct ImagePart { pub mime: String, pub data_base64: String, pub width: u32, pub height: u32, pub source: Option<String> }
impl ImagePart { pub fn png(base64:String,w:u32,h:u32,source:Option<String>)->Self; pub fn data_uri(&self)->String; pub fn approx_bytes(&self)->usize }

pub struct ChatMessage { /* 原有字段 */ pub images: Vec<ImagePart> }
impl ChatMessage {
    pub fn user_with_images(text: impl Into<String>, images: Vec<ImagePart>) -> Self;
    pub fn with_image(self, image: ImagePart) -> Self;
    pub fn has_images(&self) -> bool;
}
```
- `ChatMessage::to_wire()` 在带图片时会把 `content` 展开成 OpenAI 内容数组
  （`{type:"text"}` + `{type:"image_url",image_url:{url:"data:image/png;base64,..."}}`）。
- `llm.rs` **不用改**（它就是拿 `to_wire()` 发的），但流式/非流式都要保证图片消息原样带上。

### 0.2 事件（`neosonia_core::model::AgentEvent`）
- 现在实现了 `serde::Serialize/Deserialize`，带 `#[serde(tag="type", content="data", rename_all="snake_case")]`（相邻标记：`{"type":"text_delta","data":"..."}`）
  —— **web crate 直接 `serde_json::to_string(&event)` 推给浏览器**。
- 新增变体：`ConversationStarted { conversation_id, title }`、`ImagesAttached { count, source }`。

### 0.3 共享类型（`neosonia_core::shared`，已 re-export 到 core 根）
```rust
pub struct ConversationSummary { id, title, created_at_ms, updated_at_ms, message_count: u32, preview, persona }
pub struct ConversationHit { conversation_id, conversation_title, role, snippet, at_ms }
pub struct SkillSummary { id, name, description, when_to_use, source, installed_at_ms, path }
pub struct SkillDetail { summary, body, files: Vec<String> }
pub struct TrustedDevice { id, name, user_agent, last_addr, created_at_ms, last_seen_ms, online }
pub enum AttachmentKind { Image, Text, File, Folder, Screenshot }
pub struct Attachment { path: PathBuf, name, kind, size: u64, text_preview: Option<String>, pixels: Option<(u32,u32,Vec<u8>)> }
```

### 0.4 配置（`neosonia_core::config`）
- `SearchProvider`：新增 `Aggregated`（默认），枚举变为
  `Aggregated | Baidu | Sogou | Bing | Duckduckgo | Searxng | None`。**`Auto` 已删除**。
- `UiSettings` 新增：`autostart: bool`、`screenshot_hotkey: String`（默认 `Ctrl+Alt+A`）。
- `AgentSettings` 新增：`vision: bool`（默认 true）、`vision_max_edge: u32`（默认 1536）。
- 新增 `RemoteSettings { enabled, bind, port(8787), require_pairing, max_devices, lan_only, pair_ttl_secs }`。
- 新增 `IndexSettings { enabled, roots: Vec<String>, exclude: Vec<String>, max_entries, autoscan }`。
- `Settings` 新增 `remote`、`index` 两个块。

### 0.5 路径（`neosonia_core::config::Paths`）
新增：`skills`、`uploads`、`qrcodes`、`file_index_db`、`devices_file`、`conversations_db`
（`ensure()` 已自动创建目录）。

### 0.6 UI 状态（`neosonia-app/src/state.rs`，Lead 维护）
`NavPage` 新增 `History` / `Remote`（不在底栏，从标题栏/设置进入）。
`AppState` 新增字段：`attachments: Vec<Attachment>`、`history: HistoryState`、`remote: RemoteState`、
`skills: Vec<SkillSummary>`、`scroll_bottom_pending: bool`。
`Msg` 新增（UI 面板直接用，**名字不可改**）：
```
AttachClick, AttachFolderClick, AttachmentsPicked(Vec<PathBuf>), AttachmentRemove(usize), AttachmentClear,
ScreenshotHotkey, ScreenshotDone(Result<String,String>),
HistoryToggle, HistoryQueryChanged(String), HistorySearch, HistoryLoaded(Vec<ConversationSummary>),
HistoryHits(Vec<ConversationHit>), HistorySelect(String), HistoryDelete(String), HistoryNew,
RemoteToggle, RemoteRefresh, RemotePairingReady{url,code,expires_at_ms,qr_png}, RemoteDevices(Vec<TrustedDevice>),
RemoteRevoke(String), RemoteStatus(String),
SkillsLoaded(Vec<SkillSummary>), SkillInstallRequest, SkillInstallPath(String), SkillInstallUrl(String),
SkillUninstall(String), SkillOpenDir
```

---

## 1. core-llm（新文件：`conversation.rs`、`skill.rs`；修改：`agent.rs`、`lib.rs`）

### 1.1 视觉输入（`agent.rs`）
- 工具结果里如果出现标记 `[screenshot:/abs/path.png]` 或 `[image:/abs/path]`：
  1) 读文件（`image` crate 已加进 core 依赖）；2) 超过 `agent.vision_max_edge` 就等比缩小；
  3) PNG 编码 → base64 → `ImagePart::png(...)`；
  4) 在下一次请求前追加一条 `ChatMessage::user_with_images("（这是刚才截图的画面，坐标以截图像素为准）", vec![img])`；
  5) `emit(AgentEvent::ImagesAttached{count, source})`。
- 只在 `settings.agent.vision == true` 且模型可能支持图片时做；失败只 warn，不影响轮次。
- 图片消息**不进 UI 消息流**（UI 只显示工具卡片），但要进历史与会话库。

### 1.2 对话历史（`conversation.rs`）
```rust
pub struct ConversationStore { /* SQLite: data/conversations.sqlite3 */ }
impl ConversationStore {
    pub fn open(path: &Path) -> anyhow::Result<Self>;
    pub fn ensure(&self, conv_id: Option<&str>, first_user_text: &str, persona: &str) -> anyhow::Result<String>;
    pub fn append(&self, conv_id: &str, msg: &ChatMessage) -> anyhow::Result<()>;
    pub fn messages(&self, conv_id: &str, limit: usize) -> anyhow::Result<Vec<ChatMessage>>;
    pub fn list(&self, query: Option<&str>, limit: usize) -> anyhow::Result<Vec<ConversationSummary>>;
    pub fn search_messages(&self, q: &str, limit: usize) -> anyhow::Result<Vec<ConversationHit>>;
    pub fn rename(&self, conv_id: &str, title: &str) -> anyhow::Result<()>;
    pub fn delete(&self, conv_id: &str) -> anyhow::Result<bool>;
    pub fn stats(&self) -> anyhow::Result<(i64, i64)>; // (会话数, 消息数)
}
```
- 表：`conversations(id TEXT PK, title, created_at_ms, updated_at_ms, message_count, preview, persona)`
  和 `messages(id INTEGER PK AUTOINCREMENT, conv_id, role, content, reasoning, images_json, tool_calls_json, created_at_ms, seq)`。
  搜索用 `LIKE '%q%'`（标题 + 消息内容），大小写不敏感。
- 标题：首条用户消息前 30 字（去掉换行）。
- 引擎行为：`EngineConfig` 增加 `conversations: Arc<ConversationStore>`、`skills: SkillStore`；
  每轮用户消息落库前 `ensure` 拿到 conv_id，emit `ConversationStarted`（第一次时）；
  user/assistant/tool 消息都 append。
- 新 API：
```rust
impl AgentEngine {
    pub fn conversation_id(&self) -> Option<String>;
    pub fn load_conversation(&self, id: String);   // 清空当前历史并从库载入，emit Notice
    pub fn new_conversation(&self);                // 只清空当前历史，下一轮建新会话
    pub fn conversations(&self) -> Arc<ConversationStore>;
    pub fn skills(&self) -> SkillStore;
    pub fn reload_skills(&self);
}
```
  `reset()` 保留（等价 new_conversation）。

### 1.3 技能（`skill.rs`）
```rust
pub struct SkillStore { /* dir: Paths.skills */ }
impl SkillStore {
    pub fn load(root: &Path) -> Self;
    pub fn reload(&self);
    pub fn list(&self) -> Vec<SkillSummary>;
    pub fn get(&self, id: &str) -> Option<SkillDetail>;
    pub fn install_from_path(&self, path: &Path) -> anyhow::Result<SkillSummary>;
    pub async fn install_from_url(&self, url: &str) -> anyhow::Result<SkillSummary>;
    pub fn uninstall(&self, id: &str) -> anyhow::Result<bool>;
}
```
- 布局：`skills/<id>/SKILL.md`，YAML front-matter 字段 `id/name/description/when_to_use`，正文=技能说明。
  同目录其它文件列进 `SkillDetail.files`。
- `install_from_url`：只接受 http(s)，下载后按内容判断（单 md 文件或 zip 不支持）→ 存成 `skills/<id>/SKILL.md`。
- 内置 2 个示例技能（core 首次启动时写盘）：`screen-review`（看截图定位界面问题）、`mail-draft`（按人设写邮件草稿）。
- 内置工具（引擎注册）：`skill_list {}`、`skill_load {name}`、`skill_install {source}`、`skill_uninstall {id}`。
- 系统提示里加"可用技能"清单（最多 20 条：`name — when_to_use`），并提示"需要时先 skill_load 再照做"。

---

## 2. tools（修改 `web.rs`、`screen.rs`、`lib.rs`，新增 `index.rs` 由 mail 负责）

### 2.1 国内可用的聚合搜索（`web.rs`，仿 SearXNG）
- `search_provider = aggregated`（默认）：**并行**请求以下引擎，各自解析 HTML（`scraper`）：
  - 百度 `https://www.baidu.com/s?wd={q}&rn=20`
  - 搜狗 `https://www.sogou.com/web?query={q}`
  - 必应中国 `https://cn.bing.com/search?q={q}&ensearch=0`
  - 360 `https://www.so.com/s?q={q}`
  - 可选 DuckDuckGo 作为补充（国内常不可达，失败静默跳过）
- 合并：按规范化 URL（去掉 utm/tracking 参数、统一小写 host、去尾斜杠）去重；
  打分 = Σ(1/(rank+3)) + 0.5×命中引擎数；排序取前 `count`。
- 输出格式（给模型）：每条形如
  `1. 标题\n   URL\n   摘要\n   来源: 百度, 搜狗` ，末尾注明总耗时与各引擎命中数。
- 单引擎模式（baidu/sogou/bing/duckduckgo/searxng）保持可切换。
- 网络要求：必须真实可用；带 `Accept-Language: zh-CN`、正常 UA、`Referer` 按引擎设置，
  失败自动换下一个引擎；**不许 mock**，并在完成汇报里贴真实结果。
- 额外：`web_search` 结果里如果抓不到摘要，用 `web_fetch` 补齐前 2 条（可选）。

### 2.2 计算机使用增强（`screen.rs` / `input.rs` / `system.rs`）
- `screenshot`：无论是否 `return_base64`，返回文本里**必须**包含 `[screenshot:<绝对路径>]` 标记
  （core 的 vision 靠它找图）；`return_base64=true` 时同时返回 base64（供远端 web 显示）。
- 新增 `screen_metrics {}`：返回虚拟屏幕矩形、每个显示器的物理/逻辑尺寸与 scale、光标当前位置。
- 新增 `mouse_drag`… 已有；新增 `mouse_drag_path {points:[{x,y}], button?, duration_ms?}`（分步移动，模拟人手）。
- 新增 `keyboard_combo {keys:["ctrl","shift","t"]}`。
- 新增 `wait_for_window {title_contains, timeout_secs?, interval_ms?}`（轮询前台窗口，等到返回）。
- `mouse_*` 参数统一支持 `physical`（默认 true）与可选 `logical: true` 换算（用 window scale）。
- 保留原有工具名/参数不变（模型 schema 已固化）。

### 2.3 文件索引（`index.rs`，owner: mail；tools 只负责在 `lib.rs` 里 `pub mod index;` 与注册）
见第 3 节。

---

## 3. mail（新文件 `crates/neosonia-tools/src/index.rs`）

类 Everything 的本地文件索引。
```rust
pub struct FileIndex { /* data/file_index.sqlite3 */ }
impl FileIndex {
    pub fn global(paths: &Paths) -> &'static FileIndex;         // OnceLock
    pub fn start_background(&'static self, settings: Arc<RwLock<Settings>>); // 启动扫描线程
    pub fn search(&self, query: &str, path_filter: Option<&Path>, glob: Option<&str>, limit: usize) -> anyhow::Result<Vec<IndexEntry>>;
    pub fn scan_now(&self, roots: &[PathBuf], exclude: &[String], max_entries: u32) -> anyhow::Result<ScanStats>;
    pub fn stats(&self) -> anyhow::Result<IndexStats>;
    pub fn clear(&self) -> anyhow::Result<()>;
}
pub struct IndexEntry { path: String, name: String, size: u64, modified_ms: i64, is_dir: bool }
```
- 表：`files(path TEXT PRIMARY KEY, name TEXT, name_lower TEXT, size, modified_ms, is_dir, root)`；
  索引 `name_lower`（前缀/包含查询都走 LIKE + 前缀优先排序）。
- 扫描：`walkdir` + `IndexSettings.exclude`（目录名小写比较）+ `max_entries` 截断；
  跳过系统目录（`C:\Windows`、`$Recycle.Bin`、`System Volume Information`、`node_modules`、`.git`、`target`）。
- 后台：`start_background` 用独立线程，先全量扫描，之后每 10 分钟增量（按 mtime 差异）或
  `notify` 监听（有就更好，没有就定时）。
- 工具（`pub fn register(reg)`）：`find_files {query, path?, glob?, limit?=50, dirs_only?}`、
  `reindex_files {paths?}`、`index_stats {}`。
- 搜索返回按"名字完全匹配 > 前缀匹配 > 包含匹配"排序，最多 200 条，返回 `绝对路径 / 大小 / 修改时间`。

---

## 4. automation（修改 `uia.rs`、`cdp.rs`；真机验证）

1. **真机输入验证**（Black Screen 已关闭）：用记事本/计算器做非破坏性验证
   —— `ui_focus_window` → `keyboard_type "6*7"` → `keyboard_key enter` → `ui_tree`/`screenshot` 看结果；
   `mouse_drag` 拖选文本；把证据贴给 Lead。
2. 新增 `ui_act {action, name?, role?, index?, value?, window?, keys?}`：
   复合动作：`focus_window` → 遍历树 → 按 name/role/index 定位 → 执行
   `click|type|set_value|invoke|toggle|expand|select|scroll`，失败重试 1 次（重新遍历）；
   `type` 时若定位到 Edit 就 SetValue，否则聚焦后 enigo 输入。
3. 新增 `ui_drag {from, to}`：`from`/`to` 可以是 `{name}`/`{x,y}`，用 UIA 拿坐标后 enigo 拖拽。
4. `ui_tree` 默认参数改为 `depth=8, max_nodes=400`（和 `ui_click` 一致），避免 index 对不上。
5. 继续保证 Linux 分支编译通过（降级错误）。

---

## 5. web-remote（新成员，crate `crates/neosonia-web`）

### 5.1 后端（axum 0.8）
```rust
pub struct WebConfig {
    pub paths: Paths,
    pub settings: Arc<RwLock<Settings>>,
    pub engine: Arc<AgentEngine>,
    pub conversations: Arc<ConversationStore>,
    pub events: tokio::sync::broadcast::Sender<AgentEvent>,
}
pub struct WebServer;
pub struct WebInfo { pub url: String, pub code: String, pub expires_at_ms: i64, pub qr_png: Option<PathBuf>, pub devices: Vec<TrustedDevice> }
impl WebServer {
    pub fn start(cfg: WebConfig) -> anyhow::Result<WebServer>;   // 需在 tokio runtime 内调用
    pub fn info(&self) -> WebInfo;
    pub fn refresh_pairing(&self) -> WebInfo;                    // 换新码
    pub fn devices(&self) -> Vec<TrustedDevice>;
    pub fn revoke(&self, device_id: &str) -> bool;
    pub fn stop(&self);
    pub fn lan_url(&self) -> String;
}
```
- 监听 `RemoteSettings.bind:port`；`lan_only` 时中间件校验 peer IP 属于私有网段
  （10/8、172.16/12、192.168/16、127/8、169.254/16、fc00::/7、::1），否则 403。
- **配对**：`POST /api/pair {code, name, name_ua?}`：校验 code（一次性、TTL）→ 生成 32 字节随机 token →
  存 `sha256(token)` 到 `data/devices.json`（结构 `{devices:[{id,name,user_agent,last_addr,created_at_ms,last_seen_ms,token_sha256}]}`）→
  返回 `{token, device: TrustedDevice}`。设备数超 `max_devices` 返回 409。
- **鉴权**：`/api/*` 与 `/ws` 需 `Authorization: Bearer <token>`（ws 可用 `?token=`），
  校验 sha256 命中设备表，并更新 `last_seen_ms/last_addr`；否则 401。
- 路由：
  - `GET /` → 内嵌 SPA（`include_str!`）
  - `GET /pair` → 同一个 SPA（读 location.hash 里的 code 调 /api/pair）
  - `GET /api/me` → `{device, server:{hostname, version, busy, model}}`
  - `GET /api/sessions?q=` → `Vec<ConversationSummary>`
  - `GET /api/sessions/:id?limit=` → `Vec<ChatMessage>`（图片只回 `has_image:bool` 不传 base64，避免大包）
  - `POST /api/sessions/:id/delete`
  - `GET /api/search?q=` → `Vec<ConversationHit>`
  - `POST /api/chat {text, conv_id?}` → 202 `{turn_id}`（内部 `engine.submit`）
  - `GET /ws?token=` → WebSocket：连接后先推 `{"type":"hello",...}`，之后把 broadcast 里的事件 JSON 原样转发；
    客户端可发 `{"type":"cancel"}` 取消当前轮。
  - `GET /api/screenshot/:name` → 从 `paths.screenshots` 安全地读 PNG（防目录穿越）
- 二维码：`qrcode` 生成 PNG 存 `paths.qrcodes/pair.png`，内容为
  `http://<lan_ip>:<port>/pair#<code>`（code 放 fragment，不落服务端日志）；`info().qr_png` 给出路径。

### 5.2 前端 SPA（`crates/neosonia-web/assets/web/{index.html,app.js,style.css}`）
- 风格与桌面端一致（Firefox Nova：`#121114` 底、`#b89cff` 紫、r12/r16、无 emoji、中文）。
- 移动端优先（手机竖屏），桌面端居中 720px。
- 功能：配对页（显示配对码/设备名输入）、会话列表 + 搜索、聊天（markdown 基础渲染：标题/粗体/行内代码/代码块/列表/链接）、
  流式输出（WS）、发送/停止、截图附件展示（`/api/screenshot/...`）、设备信息页。
- 离线可用：不引用任何 CDN/外网资源，字体用系统字体。
- 数学公式：至少把 `$...$` 原样以等宽显示并加"公式"样式（不引外部库）。

### 5.3 交付验证
- `cargo check --offline -p neosonia-web` + 单测（配对码校验、token 哈希、LAN IP 判定、路径穿越防护、会话列表）。
- 用 `node "$env:USERPROFILE\.dsh\tools\firefox-screenshot.mjs" --url "http://127.0.0.1:<port>/pair#<code>" --out <png>` 截 SPA 截图，
  `read_image` 看过后把图给 Lead（也放到 `logs/screenshot-web.png`）。

---

## 6. ui-panels（`panels/settings.rs`、新增 `math.rs`、`panels/history.rs`、`panels/remote.rs`、`attachments.rs`）

1. **修设置页底部错位**：`settings::view` 改成两段式——可滚动的表单区（`col().grow().scroll_y()`）+
   **固定在底部的 footer**（不在滚动区内，`w_full`，内边距 16，按钮右对齐，不超出 424 宽）。
   当前 bug：保存条作为滚动内容最后一项，按钮溢出面板右侧被裁切。
2. **数学公式渲染 `crates/neosonia-app/src/math.rs`**：
   ```rust
   pub fn inline(latex: &str, c: &UiColors, size: TextSize) -> Element<Msg>;
   pub fn block(latex: &str, c: &UiColors) -> Element<Msg>;
   /// 把一行 markdown 文本切成 普通/行内公式/块级公式 片段，供 chat.rs 复用
   pub fn split(text: &str) -> Vec<MathSegment>;
   pub enum MathSegment { Text(String), Inline(String), Block(String) }
   ```
   支持：`\frac{}{}`、`^{}`、`_{}`、`\sqrt{}`、`\sum/\prod/\int`（带上下限）、希腊字母、常用运算符
   （`\times \cdot \pm \le \ge \ne \approx \infty \to \partial \nabla`）、`\text{}`、`\left( \right)`、
   矩阵/多行可降级为等宽文本。渲染用 fenestra 元素组合（分数=两行+横线、上下标=小字号行内）。
   必须有单测（解析 + 分段）。
3. **历史页 `panels/history.rs`**：搜索框（Enter 触发 `HistorySearch`）、结果分两段
   （会话列表 `st.history.sessions`、消息命中 `st.history.hits`）、点击 `HistorySelect(id)`、
   删除 `HistoryDelete(id)`、新建 `HistoryNew`；空态文案。
4. **设备页 `panels/remote.rs`**：启用开关（`Msg::set("/remote/enabled", bool)` + `RemoteToggle`）、
   局域网地址、二维码图片（`st.remote.qr_pixels` 用 `image_rgba8` 渲染，最大 200x200）、
   配对码 + 过期倒计时、设备列表（名称/最近在线/IP/撤销按钮）、"必须同一局域网"提示、刷新按钮。
5. **附件条 `crates/neosonia-app/src/attachments.rs`**：
   `pub fn strip(st: &AppState, c: &UiColors) -> Element<Msg>` —— 横向可滚动的附件 chip：
   图片显示缩略图（`pixels`），文件/文件夹显示名字+大小，右上角 × 发 `AttachmentRemove(i)`。
6. **设置页新增小节**：技能（列表 + 安装路径输入 `SkillInstallPath` / 从 URL `SkillInstallUrl` /
   打开目录 `SkillOpenDir` / 卸载 `SkillUninstall`）、远程访问（开关 + 端口 + 入口按钮到设备页）、
   本地索引（根目录多行输入 + 重建按钮，`Msg::set("/index/roots", StrList)`）、开机自启开关
   （`/ui/autostart`）。全部沿用 `Msg::set` 的 JSON Pointer。

---

## 7. 编译调度

- 所有人仍然**不要抢跑**：我（Lead）会在需要时统一开编译窗口，并逐个通知。
- 自测命令：`cd E:\NeoSonia; $env:CARGO_TARGET_DIR="E:\NeoSonia\target"; cargo check --offline -p <crate>`。
- 新依赖已加好并 fetch 完成：axum/tower/tower-http/qrcode/local-ip-address/urlencoding/rfd/rusqlite(tools)/notify(tools)/image(core)。
- 不要改根 `Cargo.toml`；子 crate 的 Cargo.toml 若缺 workspace 依赖可以加（从 `workspace = true` 引）。
