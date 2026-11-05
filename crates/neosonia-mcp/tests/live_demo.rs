//! 真机验证：连接 `scripts/mcp-demo-server.mjs`（需要本机 Node.js）。
//!
//! 默认 `#[ignore]`，避免普通 `cargo test` 起子进程 / 占端口。运行方式（PowerShell）：
//! ```text
//! $env:NEOSONIA_VERIFY = "1"
//! cargo test --offline -p neosonia-mcp --test live_demo -- --ignored --nocapture
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use neosonia_core::config::{McpServerConfig, McpSettings, McpTransport, Paths, Settings};
use neosonia_core::model::{
    AgentEvent, AgentHost, MemoryApi, MemoryItem, MemoryStats, SubagentRequest, ToolCtx,
};
use neosonia_core::registry::ToolRegistry;
use parking_lot::RwLock;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, BufReader};

use neosonia_mcp::McpManager;

fn verify_enabled() -> bool {
    std::env::var("NEOSONIA_VERIFY")
        .map(|value| value.trim() == "1")
        .unwrap_or(false)
}

fn repo_root() -> PathBuf {
    if let Some(value) = std::env::var_os("NEOSONIA_HOME") {
        let root = strip_verbatim(PathBuf::from(value));
        if root.join("scripts").join("mcp-demo-server.mjs").is_file() {
            return root;
        }
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = manifest.join("..").join("..");
    strip_verbatim(root.canonicalize().unwrap_or(root))
}

/// Windows 的 `canonicalize()` 会加 `\\?\` 扩展前缀，Node 不认这种脚本路径，这里去掉。
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

fn demo_script() -> PathBuf {
    let script = repo_root().join("scripts").join("mcp-demo-server.mjs");
    assert!(script.is_file(), "找不到演示服务器脚本：{}", script.display());
    script
}

fn temp_paths(tag: &str) -> Paths {
    Paths::from_root(std::env::temp_dir().join(format!(
        "neosonia-mcp-{tag}-{}-{}",
        std::process::id(),
        neosonia_core::util::short_id()
    )))
}

fn stdio_settings(node: &PathBuf, script: &PathBuf) -> Arc<RwLock<Settings>> {
    let mut demo = McpServerConfig::default();
    demo.name = "demo".to_string();
    demo.command = node.display().to_string();
    demo.args = vec![script.display().to_string()];
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

/// 起一个假宿主，给 `ToolCtx` 用。
fn test_host(settings: Arc<RwLock<Settings>>, paths: &Paths) -> Arc<TestHost> {
    Arc::new(TestHost {
        settings,
        paths: paths.clone(),
        events: parking_lot::Mutex::new(Vec::new()),
    })
}

#[tokio::test]
#[ignore]
async fn live_stdio_demo_roundtrip() {
    if !verify_enabled() {
        eprintln!("跳过真机测试：请设置 NEOSONIA_VERIFY=1");
        return;
    }
    let script = demo_script();
    let node = node_exe();
    let paths = temp_paths("live-stdio");
    paths.ensure().expect("创建临时目录失败");
    let settings = stdio_settings(&node, &script);
    let registry = ToolRegistry::new();
    let manager = McpManager::start(settings.clone(), &registry, &paths)
        .await
        .expect("McpManager::start 失败");

    // 1. 服务器状态
    let states = manager.servers();
    println!("服务器状态：{states:?}");
    let demo = states
        .iter()
        .find(|state| state.name == "demo")
        .expect("应有 demo 服务器");
    assert!(demo.ok, "demo 应连接成功：{:?}", demo.error);
    assert_eq!(demo.tools, 3, "demo 应暴露 3 个工具");
    assert!(demo.pid.is_some(), "stdio 服务器应有 pid");

    // 2. tools/list
    let infos = manager.tools();
    println!("工具清单：{infos:?}");
    for expected in ["mcp_demo_echo", "mcp_demo_add", "mcp_demo_sleep"] {
        assert!(
            infos.iter().any(|info| info.full_name == expected),
            "缺少工具 {expected}"
        );
        assert!(
            registry.get(expected).is_some(),
            "{expected} 应注册进 registry"
        );
    }
    let add = infos
        .iter()
        .find(|info| info.full_name == "mcp_demo_add")
        .expect("应有 add");
    assert_eq!(add.name, "add");
    assert_eq!(add.server, "demo");
    assert_eq!(add.schema["properties"]["a"]["type"], "number");

    // 3. tools/call（走注册后的 Tool，和 agent 循环一致）
    let host = test_host(settings.clone(), &paths);
    let ctx = ToolCtx::new(host, "live-turn", "live-call");

    let out = registry
        .call(&ctx, "mcp_demo_add", &json!({"a": 2, "b": 3}))
        .await;
    println!("add(2,3) ok={} 结果={}", out.ok, out.content);
    assert!(out.ok, "{}", out.content);
    assert!(out.content.contains('5'), "{}", out.content);

    let out = registry
        .call(&ctx, "mcp_demo_echo", &json!({"text": "真机验证"}))
        .await;
    println!("echo ok={} 结果={}", out.ok, out.content);
    assert!(out.ok, "{}", out.content);
    assert_eq!(out.content, "真机验证");

    // 4. mcp_list_servers
    let out = registry.call(&ctx, "mcp_list_servers", &json!({})).await;
    println!("mcp_list_servers：\n{}", out.content);
    assert!(out.ok);
    assert!(out.content.contains("demo"), "{}", out.content);
    assert!(out.content.contains("mcp_demo_add"), "{}", out.content);

    // 5. 调用超时（slow 服务器 call_timeout_secs = 1，sleep 3000ms）
    let out = registry
        .call(&ctx, "mcp_slow_sleep", &json!({"ms": 3000}))
        .await;
    println!("超时验证 ok={} 结果={}", out.ok, out.content);
    assert!(!out.ok, "应该超时失败：{}", out.content);
    assert!(out.content.contains("超时"), "{}", out.content);

    // 6. 重连后再调用
    manager.restart_async("demo").await.expect("重连失败");
    let out = registry
        .call(&ctx, "mcp_demo_add", &json!({"a": 20, "b": 22}))
        .await;
    println!("重连后 add(20,22) ok={} 结果={}", out.ok, out.content);
    assert!(out.ok, "{}", out.content);
    assert!(out.content.contains("42"), "{}", out.content);

    // 7. 关闭清理
    manager.shutdown();
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(
        manager.servers().iter().all(|state| !state.ok),
        "shutdown 后应全部离线"
    );
    let out = registry
        .call(&ctx, "mcp_demo_echo", &json!({"text": "x"}))
        .await;
    assert!(!out.ok, "关闭后调用应失败");
    println!("关闭后调用错误（预期）：{}", out.content);
}

#[tokio::test]
#[ignore]
async fn live_http_demo_roundtrip() {
    if !verify_enabled() {
        eprintln!("跳过真机测试：请设置 NEOSONIA_VERIFY=1");
        return;
    }
    let script = demo_script();
    let node = node_exe();
    let mut child = tokio::process::Command::new(&node)
        .arg(&script)
        .arg("--http")
        .arg("0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("启动 http 演示服务器失败");
    let stdout = child.stdout.take().expect("应有 stdout");
    let mut lines = BufReader::new(stdout).lines();
    let line = tokio::time::timeout(Duration::from_secs(15), lines.next_line())
        .await
        .expect("等待 http 演示服务器端口超时")
        .expect("读取 stdout 失败")
        .expect("stdout 提前关闭");
    let port: u16 = line
        .trim()
        .strip_prefix("NEOSONIA_MCP_HTTP_PORT=")
        .unwrap_or_else(|| panic!("端口行格式不对：{line:?}"))
        .trim()
        .parse()
        .expect("端口不是数字");
    println!("http 演示服务器端口：{port}");

    // 收集 stderr 便于失败时排查
    let stderr_log = Arc::new(parking_lot::Mutex::new(String::new()));
    if let Some(stderr) = child.stderr.take() {
        let sink = stderr_log.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let mut guard = sink.lock();
                guard.push_str(&line);
                guard.push('\n');
            }
        });
    }

    let mut server = McpServerConfig::default();
    server.name = "remote".to_string();
    server.transport = McpTransport::Http;
    server.url = format!("http://127.0.0.1:{port}/mcp");
    server.startup_timeout_secs = 10;
    server.call_timeout_secs = 10;
    let mut settings = Settings::default();
    settings.mcp = McpSettings {
        enabled: true,
        servers: vec![server],
        max_total_tools: 100,
    };
    let settings = Arc::new(RwLock::new(settings));
    let paths = temp_paths("live-http");
    paths.ensure().expect("创建临时目录失败");
    let registry = ToolRegistry::new();
    let manager = McpManager::start(settings.clone(), &registry, &paths)
        .await
        .expect("McpManager::start（http）失败");
    let states = manager.servers();
    let state = states.first().expect("应有 remote 服务器");
    assert!(
        state.ok,
        "http 服务器应连接成功：{:?}\n服务器 stderr：{}",
        state.error,
        stderr_log.lock().clone()
    );
    // tools/list 是 SSE 响应，能拿到工具说明 SSE 解析通过
    assert!(
        registry.get("mcp_remote_add").is_some(),
        "registry 里应有 mcp_remote_add；工具：{:?}",
        manager.tools()
    );

    let host = test_host(settings.clone(), &paths);
    let ctx = ToolCtx::new(host, "live-http-turn", "live-http-call");
    let out = registry
        .call(&ctx, "mcp_remote_add", &json!({"a": 2, "b": 3}))
        .await;
    println!("http add(2,3) ok={} 结果={}", out.ok, out.content);
    assert!(out.ok, "{}", out.content);
    assert!(out.content.contains('5'), "{}", out.content);

    let out = registry.call(&ctx, "mcp_list_servers", &json!({})).await;
    assert!(out.ok);
    assert!(out.content.contains("remote"), "{}", out.content);
    println!("http mcp_list_servers：\n{}", out.content);

    manager.shutdown();
    let _ = child.start_kill();
    let _ = child.wait().await;
}

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

struct TestHost {
    settings: Arc<RwLock<Settings>>,
    paths: Paths,
    events: parking_lot::Mutex<Vec<AgentEvent>>,
}

impl AgentHost for TestHost {
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
        anyhow::bail!("真机测试不支持子代理")
    }

    fn is_cancelled(&self) -> bool {
        false
    }
}
