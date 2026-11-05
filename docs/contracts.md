# NeoSonia 开发契约（所有队友必读）

> 这是 4 个 crate 之间的边界。改公共签名 = 先通知 Lead。

## 1. 产品目标

Windows/Linux 桌面上的常驻 AI Agent：右侧长条浮窗（置顶、无边框、圆角），全局快捷键呼出，
能读写文件、执行命令（长任务可等待）、截屏、模拟鼠标键盘、操作其它程序的 UI 控件、
访问网页、收发邮件（IMAP/SMTP 直连，不走 Outlook），有人设与长期记忆，能开子代理。

## 2. crate 与文件归属

| crate | 文件 | 归属 |
| --- | --- | --- |
| `crates/neosonia-core` | `model.rs` `config.rs` `util.rs` `registry.rs` `lib.rs` | **Lead（已冻结，勿改公共签名）** |
| | `llm.rs` `agent.rs` `memory.rs` `persona.rs` `subagent.rs` | core-llm |
| `crates/neosonia-tools` | `fs.rs` `shell.rs` `screen.rs` `input.rs` `system.rs` `web.rs` `lib.rs` | tools |
| | `uia.rs` `cdp.rs` | automation |
| `crates/neosonia-mail` | 全部 | mail |
| `crates/neosonia-app` | `main.rs` `theme.rs` `app.rs` `chat.rs` `dock.rs` `hotkey.rs` `tray.rs` | Lead |
| | `panels/`（设置、记忆、邮件、人设页） | ui-panels |

公共类型都在 `neosonia_core::model` / `neosonia_core::config` / `neosonia_core::lib.rs` 的 re-export 里。

## 3. 构建

- 依赖已 `cargo fetch`。**统一用一个 target 目录**：`E:\NeoSonia\target`（环境变量 `CARGO_TARGET_DIR` 已由 Lead 设定方式：在仓库根直接 `cargo check` 即可，默认就是它）。
- 单人自测用 `cargo check -p neosonia-core` / `cargo test -p neosonia-core`。第一次全量编译很慢（8 核老机器 + wgpu/vello），**cargo 会自己等文件锁**，不要杀进程、不要删 target。
- 提交前必须 `cargo check -p <你的crate>` 通过 + `cargo test -p <你的crate>` 通过（没有测试也要能编译）。

## 4. 编码约定

1. **不要 emoji**。任何字符串（工具返回、UI 文案、日志）都不许出现 emoji。用文字/ASCII 标记，例如 `[ok]` / `[err]`。
2. 用户可见文案用中文；代码注释可中文；标识符用英文 snake_case。
3. 错误不要 panic：工具统一返回 `ToolOutcome::err("...")`，并在 content 里说清原因 + 修复建议。
4. 长时间操作（命令、下载、轮询）必须：
   - 周期性检查 `ctx.is_cancelled()`；
   - 用 `ctx.emit_output(chunk)` 把增量输出推给 UI；
   - 用 `ctx.phase("正在编译...")` 更新状态行。
5. 跨平台：所有 Windows 专有代码放 `#[cfg(windows)]`，Linux 放 `#[cfg(unix)]`，另一边给可用的降级实现（返回 err 并说明）。
6. 路径处理：Windows 下大小写不敏感，用 `neosonia_core::util::is_within` 判断沙箱；所有写操作默认限制在 workspace 内，除非参数 `allow_outside=true`（默认 false）。
7. 工具参数用 `serde_json::Value` 取，缺字段给默认值，不要 unwrap。
8. 每个工具有单元测试覆盖参数解析/纯函数逻辑；涉及系统的用 `#[ignore]` 或跳过。
9. 不许新增 crate 依赖（Cargo.toml 已定），确需新增先找 Lead。
10. 不要修改 `E:\NeoSonia\Cargo.toml` 的 `[workspace.dependencies]`。子 crate 的 Cargo.toml 若需加 workspace 依赖，从 `workspace = true` 引用即可（Lead 已把可能用到的都列好了）。

## 5. 工具协议

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;                       // name / description / parameters(JSON Schema)
    async fn call(&self, ctx: &ToolCtx, args: Value) -> ToolOutcome;
    fn requires_approval(&self, args: &Value) -> bool { false }
}
```

- 名字用 `snake_case`，动词开头：`read_file` `run_command` `ui_click`。
- description 用中文，写清"什么时候该用/不该用"和边界（例如"相对路径相对 workspace"）。
- 结果 content 是喂给模型的主内容：结构化、带路径/退出码/行号、限制长度（默认 20k 字符，
  `AgentSettings::max_tool_output_chars` 由 agent 侧再裁一次）。
- `ToolOutcome::with_display("生成 12 行")` 给 UI 一行摘要。

## 6. 工具清单与契约（tools / automation / mail 三个 workstream）

### 6.1 文件（tools/fs.rs）—— owner: tools

| 工具 | 参数 | 行为 |
| --- | --- | --- |
| `read_file` | `path`, `start_line?`, `end_line?`, `max_chars?` | UTF-8/UTF-16/GBK 自动识别；返回带行号内容；超长截断并提示总行数 |
| `write_file` | `path`, `content`, `create_dirs?=true`, `overwrite?=true`, `allow_outside?=false` | 原子写；返回字节数与行数；覆盖已存在文件前记录旧大小 |
| `edit_file` | `path`, `old_string`, `new_string`, `replace_all?=false` | 精确字符串替换；old_string 必须唯一（除非 replace_all）；失败返回当前文件相关片段 |
| `append_file` | `path`, `content` | 追加 + 自动补换行 |
| `list_dir` | `path`, `recursive?=false`, `max_entries?=200`, `show_hidden?=false` | 目录树/列表，标注类型与大小 |
| `search_files` | `query`, `path?`, `glob?`, `mode?=name|content`, `max_results?=50`, `case_sensitive?=false`, `regex?=false` | **自己实现**：walkdir 遍历 + globset 过滤 + 内容正则。**不依赖 Everything**。遇到 `node_modules` `.git` `target` 默认跳过；返回 `路径:行号: 内容` |
| `file_info` | `path` | 大小/时间/权限/是否目录 |
| `make_dir` | `path`, `recursive?=true` | |
| `copy_path` / `move_path` / `delete_path` | `from`,`to` / `path`,`recursive?=false` | 危险操作，`requires_approval = true` |

### 6.2 命令（tools/shell.rs）—— owner: tools

**必须支持长任务**，默认超时 `ToolsSettings::command_timeout_secs`（1800 秒），
0 = 无限（只受用户取消约束）。**不允许** Sonia 那种 30 秒断开的实现。

| 工具 | 参数 | 行为 |
| --- | --- | --- |
| `run_command` | `command`, `working_dir?`, `shell?`, `timeout_secs?`, `background?=false`, `env?` | Windows 默认 `pwsh -NoProfile -NonInteractive -Command`（无 pwsh 回退 `powershell`，再回退 `cmd /C`）；Linux 用 `bash -lc`（无 bash 用 `sh -lc`）；流式回传 stdout/stderr；返回退出码+耗时；超时才终止并说清 |
| `list_jobs` | 无 | 列出后台任务（id/状态/命令/耗时/退出码） |
| `read_job_output` | `job_id`, `wait_ms?=0`, `max_chars?` | 读增量输出；`wait_ms` 内阻塞等待新输出（"允许等待命令"） |
| `kill_job` | `job_id`, `force?=true` | 终止进程树（Windows `taskkill /T /F`，Linux 进程组 kill） |
| `wait` | `seconds`, `reason?` | 显式等待，受上限 600 秒，期间响应取消 |
| `send_job_input` | `job_id`, `input` | 给后台任务 stdin 写一行（交互式命令） |

实现要点：tokio `Command` + `Stdio::piped` + 两个读任务（stdout/stderr）+ `tokio::select!` 处理超时/取消；
后台任务存 `Arc<Mutex<HashMap<String, JobHandle>>>`，输出环形缓冲（保留最近 200KB）。
进程组：Windows 用 `CREATE_NEW_PROCESS_GROUP`（`windows` crate 或 `CommandExt`），Linux 用 `setsid`（`CommandExt::process_group(0)`）。

### 6.3 屏幕与输入（tools/screen.rs / input.rs）—— owner: tools

| 工具 | 参数 | 行为 |
| --- | --- | --- |
| `list_displays` | 无 | 显示器 id/尺寸/缩放/主屏 |
| `screenshot` | `display?=primary`, `region?={x,y,w,h}`, `save_path?`, `max_edge?`, `return_base64?=false` | `xcap` 抓屏；PNG 存 `data/screenshots/`；返回**绝对路径 + 宽高**，`return_base64=true` 时才回 base64（默认 false，省 token） |
| `mouse_move` | `x`, `y`, `smooth?=true` | 绝对坐标（物理像素） |
| `mouse_click` | `x?`,`y?`, `button?=left`, `clicks?=1`, `interval_ms?=80` | 不传坐标就点当前位置 |
| `mouse_drag` | `from_x`,`from_y`,`to_x`,`to_y`, `button?=left` | |
| `mouse_scroll` | `amount`(正=上), `x?`,`y?` | |
| `keyboard_type` | `text`, `interval_ms?=0` | 输入文本（支持 CJK：用剪贴板粘贴兜底，先试 `enigo.text`） |
| `keyboard_key` | `key`, `modifiers?=["ctrl","shift"]`, `repeat?=1` | 支持 `enter` `esc` `tab` `f1`..`f12` `a`..`z` `0`..`9` `left/right/up/down` `home/end/pgup/pgdn` `backspace/delete` `space` |
| `clipboard_read` / `clipboard_write` | — / `text` | `arboard` |
| `screen_size` | 无 | 主屏逻辑/物理尺寸（配合截图坐标换算） |

坐标约定：**一律物理像素**，工具文档里写明；`screenshot` 返回里给出 `scale` 便于换算。

### 6.4 系统集成（tools/system.rs）—— owner: tools

| 工具 | 参数 | 行为 |
| --- | --- | --- |
| `system_info` | 无 | OS 版本、主机名、CPU 核数/负载、内存、磁盘剩余、当前前台窗口标题与进程名 |
| `foreground_window` | 无 | 标题/进程/pid/窗口矩形（Windows `GetForegroundWindow` + `GetWindowTextW`；Linux 用 `xdotool` 若存在，否则 err） |
| `open_path` | `target`(路径或 URL) | `open` crate，用默认程序打开 |
| `notify` | `title`, `body` | Windows 用 toast（`windows` crate 或 PowerShell BurntToast 兜底 → 最后退化成托盘/日志）；Linux `notify-send` |
| `list_processes` | `filter?`, `limit?=30` | `sysinfo` 进程列表（名字/pid/内存） |
| `kill_process` | `pid`, `force?` | 需审批 |

### 6.5 网页（tools/web.rs）—— owner: tools

**不依赖 SearXNG**。自实现多后端。

| 工具 | 参数 | 行为 |
| --- | --- | --- |
| `web_search` | `query`, `count?=8`, `provider?=auto`, `lang?` | `auto`：DuckDuckGo HTML 端点 (`https://html.duckduckgo.com/html/?q=`) → 失败回退 Bing (`https://www.bing.com/search?q=`) → 再回退 `https://search.marginalia.nu`。用 `scraper` 解析结果（title/url/snippet），去重，返回编号列表 |
| `web_fetch` | `url`, `max_chars?`, `format?=text|markdown|html`, `render_js?=false` | `reqwest` + `encoding_rs` 解码 + `html2text`；`render_js=true` 时走 CDP（若 automation 的 `cdp` 可用），否则说明限制 |
| `web_links` | `url`, `filter?`, `limit?=50` | 提取页面链接 |
| `download_file` | `url`, `path?`, `max_bytes?` | 流式下载到 workspace，进度回传 |

反爬兜底：带 `User-Agent`、`Accept-Language: zh-CN,zh;q=0.9,en;q=0.8`；HTTP 403 时提示可换 provider。

### 6.6 UI 自动化（tools/uia.rs + cdp.rs）—— owner: automation

目标："获取前端框架的控件树并模拟点击按钮"。

**Windows（UI Automation，必须实现）**
| 工具 | 参数 | 行为 |
| --- | --- | --- |
| `ui_tree` | `window?=foreground`, `depth?=4`, `max_nodes?=120`, `name_filter?`, `role_filter?` | 用 `IUIAutomation::GetFocusedElement` / `ElementFromHandle` 遍历控件树；每个节点返回 `index / role / name / value / bounds(x,y,w,h) / enabled / patterns`。**Electron/Chromium 应用也能拿到按钮**（Chromium 暴露 UIA 树） |
| `ui_click` | `index?` 或 `name?`+`role?`, `window?` | 按 index 或可访问名找到元素，优先 `InvokePattern`，否则 `SetFocus` + 元素中心点鼠标点击（内部复用 enigo） |
| `ui_set_value` | `index`/`name`, `value` | `ValuePattern` 或点进去全选覆盖输入 |
| `ui_invoke_pattern` | `index`, `pattern=invoke|toggle|select|expand` | 高级模式 |
| `ui_window_list` | `filter?`, `limit?=30` | 枚举顶层窗口（title/class/pid/hwnd） |
| `ui_focus_window` | `title?`/`pid?`/`hwnd?` | 置前台 |

**Linux**：AT-SPI 太重，降级方案：若存在 `xdotool`/`wmctrl` 则给它俩的能力（窗口列表/激活/按坐标点），
`ui_tree` 返回 `[err] 当前平台未实现控件树，请用 screenshot + mouse_click 或安装 xdotool`。

**Chromium/CDP（两平台，需要应用开 `--remote-debugging-port=9222`）**：owner: automation
| 工具 | 参数 | 行为 |
| --- | --- | --- |
| `cdp_targets` | `port?=9222` | `GET http://127.0.0.1:port/json/list`，列出页面/标题/url/ws 地址 |
| `cdp_eval` | `expression`, `target?`（url 片段或 index）, `port?` | `Runtime.evaluate` 返回 JSON 结果（`returnByValue`），可读 DOM：`document.title`、`document.querySelector('.btn')?.innerText` |
| `cdp_click` | `selector` 或 `text`, `target?` | `Runtime.evaluate` 里 `document.querySelector(sel).click()`；`text` 模式遍历可点击元素按 innerText 匹配 |
| `cdp_screenshot` | `target?`, `full_page?=false` | `Page.captureScreenshot` 存 PNG，返回路径 |

用 `tungstenite` 连 ws，`Runtime.enable` 后发 `Runtime.evaluate` 等响应（id 匹配，超时 10s）。

### 6.7 邮件（neosonia-mail）—— owner: mail

不用 Outlook、不用本机客户端。**IMAP 收 + SMTP 发**，连接参数全部来自 `Settings::mail`（UI 可改）。

```rust
pub struct MailService { /* 无状态构造：MailService::new(settings) */ }
impl MailService {
    pub fn from_settings(s: &Settings) -> Self;
    pub async fn test_imap(&self) -> anyhow::Result<String>;      // 返回 "OK uid=..." 
    pub async fn test_smtp(&self) -> anyhow::Result<String>;
    pub async fn folders(&self) -> anyhow::Result<Vec<String>>;
    pub async fn list(&self, folder: &str, limit: usize, unread_only: bool) -> anyhow::Result<Vec<MailSummary>>;
    pub async fn fetch(&self, uid: u32, folder: &str) -> anyhow::Result<MailFull>;
    pub async fn mark_read(&self, uid: u32, read: bool, folder: &str) -> anyhow::Result<()>;
    pub async fn send(&self, msg: OutgoingMail) -> anyhow::Result<()>;
    pub async fn reply(&self, uid: u32, body: &str, folder: &str) -> anyhow::Result<()>; // In-Reply-To/References + Re:
}
pub struct MailSummary { uid: u32, from: String, from_name: String, subject: String, date: String, snippet: String, unread: bool, has_attachments: bool }
pub struct MailFull { uid, from, to: Vec<String>, cc: Vec<String>, subject, date, body_text: String, has_attachments: bool, message_id: String, references: Vec<String> }
```
工具（注册进 registry）：`mail_send` `mail_list` `mail_read` `mail_reply` `mail_mark_read` `mail_folders`
`mail_test_connection` `mail_wait_for`（`sender?`/`subject_contains?`/`timeout_secs?=120`，轮询新邮件，等到就返回；**这就是"允许等待命令"的邮件版**）。

白名单（`Settings::mail.whitelist`，支持 `@domain` 后缀匹配，已在 core 实现 `is_whitelisted`）：
- `mail_send` 目标不在白名单 且 `!allow_outbound_to_anyone` → 拒绝并说明如何加白名单；
- `mail_reply` 只对白名单来信自动回复。

轮询器（后台任务，由 app 启动）：
```rust
pub struct MailPoller;
pub enum MailEvent { Started, NewMail(MailSummary), Replied{uid, to}, Error(String), Idle }
impl MailPoller { pub fn spawn(settings: Arc<RwLock<Settings>>, tx: tokio::sync::mpsc::UnboundedSender<MailEvent>) -> MailPoller; pub fn stop(&self); }
```
去重：已处理的 uid 存 `data/mail_seen.json`。间隔用 `poll_interval_secs`，支持热改配置。
**不要**在 `mail_reply` 里自己调模型——app 层拿到 `NewMail` 事件后组织一次 agent turn。

## 7. UI 契约（Lead + ui-panels）

- 主题令牌来自 `docs/ui-design-palette.json`（Firefox Nova），用 `include_str!` 载入。
- 全部文案中文，**零 emoji**。
- 浮窗：右侧、宽 424、贴边距 12、高 88% 屏高、圆角 16、置顶、无边框、可拖拽、`Alt+Space` 显隐。
- 布局：标题栏 48（拖拽区 + 状态 + 关闭）→ 导航 40（对话/设置/记忆/邮件/人设）→ 内容区 → 输入区。
- 聊天流：用户消息右对齐浅色块；助手消息 markdown 渲染；工具调用折叠卡片（运行中/成功/失败三态）；
  思考内容可折叠；流式打字；自动滚到底部。
- 设置页：服务器 base_url、模型名、API key（密码框 + 测试连接按钮）、快捷键录制、主题、
  工具开关、邮件 IMAP/SMTP 表单 + 白名单编辑 + 测试连接、子代理开关。
- 记忆页：事实列表、搜索、删除、固定块编辑。
- 邮件页：收件箱列表、详情、回复框。

## 8. 完成定义

`cargo build --workspace` 通过 → `cargo test --workspace` 通过 → `cargo run -p neosonia-app` 出窗口
→ 截图确认 UI 正确 → 用真实模型跑一轮"读文件 + 跑命令 + 截图"的工具调用链 → 邮件连接测试（用户提供凭据后）。
