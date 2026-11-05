# neosonia-mcp

NeoSonia 的 MCP（Model Context Protocol）客户端。把外部 MCP 服务器暴露的工具注册成
`neosonia_core::Tool`，模型可以像调用本机工具一样调用它们。

## 功能

- stdio 传输：`tokio::process::Command` 起子进程，newline-delimited JSON-RPC 2.0，
  握手顺序 `initialize` -> `notifications/initialized` -> `tools/list`（支持 `nextCursor` 分页）。
- http 传输：`POST {url}` JSON-RPC，带 `Mcp-Session-Id`（服务器给过就带）与
  `Accept: application/json, text/event-stream`；响应是普通 JSON 或单事件 SSE 都能解析。
- 工具命名：`<prefix><tool>`，prefix 默认 `mcp_<服务器名>_`，可用 `tool_prefix` 自定义；
  名字清洗成合法函数名并限制 64 字符。
- 注册 `mcp_list_servers` 诊断工具（中文输出服务器状态、错误原因、工具名）。
- 失败隔离：单台服务器启动/握手/调用失败只影响它自己，不影响 app 启动。
- 清理：`shutdown()` 或管理器被 drop 时杀掉所有 stdio 子进程。

## 公开 API

```rust
pub struct McpManager;
impl McpManager {
    pub async fn start(
        settings: Arc<parking_lot::RwLock<Settings>>,
        registry: &ToolRegistry,
        paths: &Paths,
    ) -> anyhow::Result<Arc<McpManager>>;
    pub fn servers(&self) -> Vec<McpServerState>;
    pub fn tools(&self) -> Vec<McpToolInfo>;
    pub fn restart(&self, name: &str) -> anyhow::Result<()>;       // 异步派发，立即返回
    pub async fn restart_async(&self, name: &str) -> anyhow::Result<()>;
    pub fn shutdown(&self);
}
pub struct McpServerState { pub name: String, pub ok: bool, pub tools: usize, pub error: Option<String>, pub pid: Option<u32> }
pub struct McpToolInfo { pub server: String, pub name: String, pub full_name: String, pub description: String, pub schema: serde_json::Value }
pub const PROTOCOL_VERSION: &str = "2024-11-05";
```

`start` 必须在 tokio runtime 内调用。单台服务器失败不会让 `start` 返回 `Err`，
原因记录在对应 `McpServerState::error` 里。

## 配置示例

```toml
[mcp]
enabled = true
max_total_tools = 100

[[mcp.servers]]
name = "demo"
enabled = true
transport = "stdio"
command = "node"
args = ["E:/NeoSonia/scripts/mcp-demo-server.mjs"]
startup_timeout_secs = 30
call_timeout_secs = 120
```

`startup_timeout_secs` 是**整个握手**的预算（initialize + initialized + tools/list），
`call_timeout_secs` 是单次 `tools/call` 的超时。

## 真机验证

```powershell
# 1) 直接连 demo server（stdio），打印服务器状态、工具清单、add 结果、超时/重连/关闭
cargo run --offline -p neosonia-mcp --bin mcp_probe

# 2) ignore 掉的真机测试（stdio + http 各一条）
$env:NEOSONIA_VERIFY = "1"
cargo test --offline -p neosonia-mcp --test live_demo -- --ignored --nocapture
```

`scripts/mcp-demo-server.mjs` 三种模式：无参数 = stdio；`--http [port]` = HTTP
（tools/list 故意用 SSE 回，验证 SSE 解析；port=0 自动分配）；`--stall` = 只读不回包，
用于验证启动握手超时。

## 已知限制

1. 服务器反向请求（sampling / roots / elicitation）不支持：只记 debug 日志，不回错误包。
2. MCP 结果里的 image / audio 不转成视觉输入给模型，只给中文占位文本；core 的 vision
   仍走 `[screenshot:]` 标记那条路。
3. `restart()` 是异步派发，想等重连完成用 `restart_async()`。重连后服务器已删除的旧工具
   名不会从 registry 摘除，调用时返回中文错误；新出现的工具会注册。
4. 工具名冲突或超过 `max_total_tools` 时跳过并 `warn`，`mcp_list_servers` 会显示被跳过数量。
5. `notifications/tools/list_changed` 不自动刷新（需要 restart）；progress 通知忽略。
6. http 是最小 Streamable HTTP：只处理单次 POST 的响应与单事件 SSE，不做 GET 长流，
   也不发 DELETE 终止会话。
7. 同一台服务器的请求串行（一把异步 Mutex）：长 `tools/call` 会阻塞同服务器的其它调用。
   MCP stdio 本身是单通道，这是有意为之。
8. 工具调用进行中不响应 agent 取消，等超时返回。
9. 配置热更新不生效：改 `[mcp]` 后需要重启 app（或重建 manager）。
10. `mcp.enabled = false` 时不注册任何 MCP 工具（含 `mcp_list_servers`）；
    `enabled = true` 但 0 台服务器时仍会注册 `mcp_list_servers` 方便自查。

### Windows 上 `command = "npx"` 起不来（重要）

现象：`server.error` 里是「启动 MCP 服务器「xxx」失败（命令：npx ...）」，通常是
os error 2（找不到文件）或 193（不是有效的 Win32 应用）。

原因：Windows 上 Node 只提供 `npx.cmd` / `npx.ps1`（外加一个给 Git Bash 用的无扩展名
shell 脚本），没有 `npx.exe`；而 Rust 的 `Command` 底层是 `CreateProcessW`，它不会按
`PATHEXT` 自动补 `.cmd`，也不会执行批处理文件。`npm` / `pnpm` / `yarn` 等同类 shim 一样。

推荐写法（不新增代码、行为可控）：

```toml
# 显式用 cmd 包一层
[[mcp.servers]]
name = "filesystem"
command = "cmd"
args = ["/c", "npx", "-y", "@modelcontextprotocol/server-filesystem", "E:/workspace"]
```

注意：`cmd /c` 会重新解析引号与 `&`、`^`、`%` 等特殊字符，路径尽量放在无特殊字符的目录；
含空格的参数整体作为一条 arg 传入即可（Rust 侧会加引号，但 cmd 的规则仍要小心）。

更稳的替代方案：

- 用 `node` 直接跑已安装包的入口：
  `command = "node"`, `args = ["C:/Users/<你>/node_modules/@modelcontextprotocol/server-filesystem/dist/index.js", "E:/workspace"]`；
- 用 `uvx`（uv 提供真正的 `uvx.exe`，不受此问题影响）；
- 把 `npx.cmd` 的完整路径交给 `cmd /c`。

不建议客户端自动帮用户套 `cmd /c`，原因有三：

1. 杀进程只会杀掉 `cmd.exe`，`npx` 拉起的 `node` 子进程可能变成孤儿进程，需要
   Windows Job Object（`KILL_ON_JOB_CLOSE`）或 `taskkill /T /F` 才能整棵树清理；
2. `cmd` 的引号与特殊字符转义规则容易出错，自动包装反而增加不可预期行为；
3. 用户显式写 `cmd /c ...` 时语义清楚，出问题也容易排查。

以后如果要支持自动包装，建议两条路线：A. 用 Job Object 包住整个进程树后再自动 `cmd /c` 包装
（需要加 `windows` crate）；B. 按 `PATH` + `PATHEXT` 找到 `.cmd`/`.bat` 后 `cmd /C` 包装，
并用 `taskkill /T /F` 清理进程树（不新增依赖，但依赖系统命令）。

另外：`npx` 首次运行会联网下载包，`startup_timeout_secs` 建议给 60~180 秒。
