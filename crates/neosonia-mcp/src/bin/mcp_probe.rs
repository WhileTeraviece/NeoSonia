//! MCP 真机探查工具：连上 `scripts/mcp-demo-server.mjs`，列出工具并真正调用。
//!
//! 用法（PowerShell，仓库根目录）：
//! ```text
//! cargo run --offline -p neosonia-mcp --bin mcp_probe
//! ```
//! 可选环境变量 `NEOSONIA_NODE` 指定 node 可执行文件（默认 `C:\Program Files\nodejs\node.exe`，
//! 找不到则用 PATH 里的 `node`）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use neosonia_core::config::{McpServerConfig, McpSettings, Paths, Settings};
use neosonia_core::model::{
    AgentEvent, AgentHost, MemoryApi, MemoryItem, MemoryStats, SubagentRequest, ToolCtx,
};
use neosonia_core::registry::ToolRegistry;
use parking_lot::RwLock;
use serde_json::json;

use neosonia_mcp::McpManager;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let root = repo_root()?;
    let script = root.join("scripts").join("mcp-demo-server.mjs");
    anyhow::ensure!(script.is_file(), "找不到演示服务器脚本：{}", script.display());
    let node = node_exe();
    println!("Node：{}", node.display());
    println!("演示服务器：{}", script.display());

    let paths = Paths::from_root(std::env::temp_dir().join("neosonia-mcp-probe"));
    paths.ensure().context("创建探查用目录失败")?;
    let settings = demo_settings(&node, &script, &root);
    let registry = ToolRegistry::new();
    let manager = McpManager::start(settings.clone(), &registry, &paths)
        .await
        .context("启动 MCP 管理器失败")?;

    section("1. 服务器状态");
    for state in manager.servers() {
        println!(
            "{}：ok={} 工具={} pid={:?} 错误={:?}",
            state.name, state.ok, state.tools, state.pid, state.error
        );
    }
    anyhow::ensure!(
        manager.servers().iter().any(|state| state.name == "demo" && state.ok),
        "demo 服务器应该连接成功"
    );

    section("2. 发现的 MCP 工具");
    for info in manager.tools() {
        println!(
            "{}（服务器 {}，远端名 {}）\n  描述：{}\n  schema：{}",
            info.full_name, info.server, info.name, info.description, info.schema
        );
    }

    section("3. 注册表里的工具名（证明真的注册成 neosonia_core::Tool）");
    let names = registry.names();
    println!("共 {} 个：{names:?}", names.len());
    anyhow::ensure!(names.iter().any(|name| name == "mcp_demo_add"), "注册表缺少 mcp_demo_add");

    let host = Arc::new(ProbeHost {
        settings: settings.clone(),
        paths: paths.clone(),
        events: parking_lot::Mutex::new(Vec::new()),
    });
    let ctx = ToolCtx::new(host, "probe-turn", "probe-call");

    section("4. 调用 mcp_demo_echo {text: \"你好，MCP\"}");
    let out = registry
        .call(&ctx, "mcp_demo_echo", &json!({"text": "你好，MCP"}))
        .await;
    println!("ok={}\n{}", out.ok, out.content);
    anyhow::ensure!(
        out.ok && out.content.contains("你好，MCP"),
        "echo 调用结果不对"
    );

    section("5. 调用 mcp_demo_add {a: 2, b: 3}（期望得到 5）");
    let out = registry
        .call(&ctx, "mcp_demo_add", &json!({"a": 2, "b": 3}))
        .await;
    println!("ok={}\n{}", out.ok, out.content);
    anyhow::ensure!(
        out.ok && out.content.contains('5'),
        "add 调用结果不对：{}",
        out.content
    );

    section("6. mcp_list_servers（模型自查工具）");
    let out = registry.call(&ctx, "mcp_list_servers", &json!({})).await;
    println!("ok={}\n{}", out.ok, out.content);
    anyhow::ensure!(
        out.ok && out.content.contains("mcp_demo_add"),
        "诊断工具输出不对"
    );

    section("7. 超时验证：slow 服务器 call_timeout_secs = 1，调用 sleep(3000)");
    let out = registry
        .call(&ctx, "mcp_slow_sleep", &json!({"ms": 3000}))
        .await;
    println!("ok={}（期望 false）\n{}", out.ok, out.content);
    anyhow::ensure!(
        !out.ok && out.content.contains("超时"),
        "超时验证失败：{}",
        out.content
    );

    section("8. 重连：restart_async(\"demo\") 后再次调用 add");
    manager
        .restart_async("demo")
        .await
        .context("重连 demo 失败")?;
    let out = registry
        .call(&ctx, "mcp_demo_add", &json!({"a": 20, "b": 22}))
        .await;
    println!("ok={}\n{}", out.ok, out.content);
    anyhow::ensure!(
        out.ok && out.content.contains("42"),
        "重连后调用失败：{}",
        out.content
    );

    section("9. 关闭：shutdown() 后服务器应全部离线，工具调用返回中文错误");
    manager.shutdown();
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    for state in manager.servers() {
        println!(
            "{}：ok={} 工具={} 错误={:?}",
            state.name, state.ok, state.tools, state.error
        );
    }
    anyhow::ensure!(
        manager.servers().iter().all(|state| !state.ok),
        "shutdown 后应全部离线"
    );
    let out = registry
        .call(&ctx, "mcp_demo_echo", &json!({"text": "x"}))
        .await;
    println!("ok={}（期望 false）\n{}", out.ok, out.content);
    anyhow::ensure!(!out.ok, "关闭后调用应该失败");

    section("10. 结论");
    println!(
        "MCP 客户端真机验证通过：stdio 握手、tools/list、tools/call、mcp_list_servers、\
         调用超时、重连、关闭清理均正常。"
    );
    Ok(())
}

/// demo：正常超时；slow：call_timeout_secs = 1，用来验证调用超时。
fn demo_settings(node: &Path, script: &Path, root: &Path) -> Arc<RwLock<Settings>> {
    let mut demo = McpServerConfig::default();
    demo.name = "demo".to_string();
    demo.command = node.display().to_string();
    demo.args = vec![script.display().to_string()];
    demo.cwd = root.display().to_string();
    demo.startup_timeout_secs = 20;
    demo.call_timeout_secs = 20;

    let mut slow = demo.clone();
    slow.name = "slow".to_string();
    slow.call_timeout_secs = 1;

    let mut settings = Settings::default();
    settings.mcp = McpSettings {
        enabled: true,
        servers: vec![demo, slow],
        max_total_tools: 100,
    };
    Arc::new(RwLock::new(settings))
}

fn section(title: &str) {
    println!("\n=== {title} ===");
}

fn node_exe() -> PathBuf {
    if let Some(value) = std::env::var_os("NEOSONIA_NODE") {
        return PathBuf::from(value);
    }
    let bundled = PathBuf::from(r"C:\Program Files\nodejs\node.exe");
    if bundled.is_file() {
        return bundled;
    }
    PathBuf::from("node")
}

fn repo_root() -> anyhow::Result<PathBuf> {
    if let Some(value) = std::env::var_os("NEOSONIA_HOME") {
        let root = strip_verbatim(PathBuf::from(value));
        if root.join("scripts").join("mcp-demo-server.mjs").is_file() {
            return Ok(root);
        }
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = manifest.join("..").join("..");
    let root = strip_verbatim(root.canonicalize().unwrap_or(root));
    if root.join("scripts").join("mcp-demo-server.mjs").is_file() {
        return Ok(root);
    }
    let cwd = std::env::current_dir()?;
    if cwd.join("scripts").join("mcp-demo-server.mjs").is_file() {
        return Ok(cwd);
    }
    bail!("找不到 scripts/mcp-demo-server.mjs，请从仓库根目录运行或设置 NEOSONIA_HOME")
}

/// Windows 的 `canonicalize()` 会加 `\\?\` 扩展前缀，Node 等外部程序不认这种脚本路径，
/// 这里把前缀去掉（UNC 路径还原成 `\\server\share` 形式）。
fn strip_verbatim(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy().to_string();
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    path
}

/// 什么都不存的记忆实现，只为满足 `AgentHost`。
struct NullMemory;

impl MemoryApi for NullMemory {
    fn remember(
        &self,
        _content: &str,
        _tags: &[String],
        _importance: f64,
        _source: &str,
    ) -> anyhow::Result<i64> {
        Ok(0)
    }
    fn recall(&self, _query: &str, _limit: usize) -> anyhow::Result<Vec<MemoryItem>> {
        Ok(Vec::new())
    }
    fn recent(&self, _limit: usize) -> anyhow::Result<Vec<MemoryItem>> {
        Ok(Vec::new())
    }
    fn all(&self, _kind: Option<&str>, _limit: usize) -> anyhow::Result<Vec<MemoryItem>> {
        Ok(Vec::new())
    }
    fn forget(&self, _id: i64) -> anyhow::Result<bool> {
        Ok(false)
    }
    fn update_importance(&self, _id: i64, _importance: f64) -> anyhow::Result<()> {
        Ok(())
    }
    fn get_block(&self, _key: &str) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
    fn set_block(&self, _key: &str, _value: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn blocks(&self) -> anyhow::Result<BTreeMap<String, String>> {
        Ok(BTreeMap::new())
    }
    fn log_conversation(&self, _role: &str, _content: &str, _turn_id: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn stats(&self) -> anyhow::Result<MemoryStats> {
        Ok(MemoryStats::default())
    }
}

/// 最小宿主：只实现 `ToolCtx` 需要的能力（事件收集、配置、路径）。
struct ProbeHost {
    settings: Arc<RwLock<Settings>>,
    paths: Paths,
    events: parking_lot::Mutex<Vec<AgentEvent>>,
}

impl AgentHost for ProbeHost {
    fn settings(&self) -> Arc<RwLock<Settings>> {
        self.settings.clone()
    }

    fn paths(&self) -> Paths {
        self.paths.clone()
    }

    fn memory(&self) -> Arc<dyn MemoryApi> {
        Arc::new(NullMemory)
    }

    fn emit(&self, event: AgentEvent) {
        self.events.lock().push(event);
    }

    fn spawn_subagent(&self, _request: SubagentRequest) -> anyhow::Result<String> {
        bail!("探查工具不支持子代理")
    }

    fn is_cancelled(&self) -> bool {
        false
    }
}
