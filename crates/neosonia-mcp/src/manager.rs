//! MCP 管理器：启动所有 enabled 的服务器、把工具注册进 registry、重连、关闭。
//!
//! 生命周期：
//! - [`McpManager::start`] 在 tokio runtime 内调用，一台服务器失败不影响其它服务器；
//! - 工具 handler 持有 `Arc<ServerState>`，重连后自动用上新连接（无需重新注册）；
//! - [`McpManager::shutdown`] 关掉所有子进程；管理器被 drop 时也会兜底杀进程。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{anyhow, bail};
use neosonia_core::{Paths, Settings, ToolRegistry};
use parking_lot::{Mutex, RwLock};

use crate::naming::{self, ConfigPlan};
use crate::server::{McpServerState, McpToolInfo, ServerState};
use crate::tool::{LIST_SERVERS_TOOL, ListServersTool, register_tool};

/// MCP 客户端总管理器。
///
/// 由 app 在启动时创建一次并长期持有；`shutdown()` 之后不可再重连。
pub struct McpManager {
    settings: Arc<RwLock<Settings>>,
    paths: Paths,
    registry: ToolRegistry,
    servers: Arc<BTreeMap<String, Arc<ServerState>>>,
    /// 本管理器注册过的工具名（用于区分"自己的同名工具"与"外部工具冲突"）。
    registered: Arc<Mutex<BTreeSet<String>>>,
    total_limit: usize,
    shutdown: Arc<AtomicBool>,
}

impl McpManager {
    /// 启动所有 enabled 的服务器：握手 initialize → tools/list，并把工具注册进 registry。
    ///
    /// 必须在 tokio runtime 内调用。单台服务器失败只记录在它的
    /// [`McpServerState::error`] 里，不会让 `start` 返回错误，也不影响 app 启动。
    pub async fn start(
        settings: Arc<RwLock<Settings>>,
        registry: &ToolRegistry,
        paths: &Paths,
    ) -> anyhow::Result<Arc<McpManager>> {
        let snapshot = settings.read().clone();
        let mcp = snapshot.mcp.clone();
        let mut map: BTreeMap<String, Arc<ServerState>> = BTreeMap::new();
        let mut startables: Vec<Arc<ServerState>> = Vec::new();

        if mcp.enabled {
            let mut unnamed = 0usize;
            for plan in naming::plan_configs(&mcp) {
                match plan {
                    ConfigPlan::Start(config) => {
                        let state = Arc::new(ServerState::new(config));
                        map.insert(state.name().to_string(), state.clone());
                        startables.push(state);
                    }
                    ConfigPlan::Broken { config, error } => {
                        let mut config = config;
                        let mut key = config.name.trim().to_string();
                        if key.is_empty() {
                            unnamed += 1;
                            key = format!("（未命名 {unnamed}）");
                            config.name = key.clone();
                        }
                        if map.contains_key(&key) {
                            tracing::warn!(server = %key, "MCP 服务器配置重复，已忽略");
                            continue;
                        }
                        let state = Arc::new(ServerState::new(config));
                        state.mark_error(error);
                        map.insert(key, state);
                    }
                    ConfigPlan::Duplicate { name } => {
                        tracing::warn!(server = %name, "MCP 服务器名称重复，后一个配置已忽略");
                    }
                }
            }
            // 顺序连接：每台都有自己的超时，失败已隔离。
            for state in &startables {
                if let Err(error) = state.connect(paths).await {
                    let reason = format!("{error:#}");
                    tracing::warn!(
                        server = %state.name(),
                        error = %reason,
                        "MCP 服务器启动失败（已隔离，不影响其它服务器）"
                    );
                }
            }
        } else {
            tracing::debug!("MCP 总开关（mcp.enabled）关闭，不启动任何服务器");
        }

        let servers = Arc::new(map);
        let registered = Arc::new(Mutex::new(BTreeSet::new()));

        if mcp.enabled {
            for state in servers.values() {
                for info in state.tool_infos() {
                    register_tool(registry, &registered, state, &info, mcp.max_total_tools);
                }
            }
            // 诊断工具用固定名字，不占 max_total_tools 名额。
            if registry.get(LIST_SERVERS_TOOL).is_some() {
                tracing::warn!(
                    tool = LIST_SERVERS_TOOL,
                    "工具名 mcp_list_servers 已被其它工具占用，MCP 诊断工具未注册"
                );
            } else {
                registry.register(Arc::new(ListServersTool::new(
                    servers.clone(),
                    mcp.max_total_tools,
                )));
            }
        }

        let server_count = servers.len();
        let connected = servers.values().filter(|state| state.snapshot().ok).count();
        let tool_count: usize = servers
            .values()
            .map(|state| state.tool_infos().len())
            .sum();
        tracing::info!(
            servers = server_count,
            connected,
            tools = tool_count,
            "MCP 客户端已启动"
        );

        Ok(Arc::new(McpManager {
            settings,
            paths: paths.clone(),
            registry: registry.clone(),
            servers,
            registered,
            total_limit: mcp.max_total_tools,
            shutdown: Arc::new(AtomicBool::new(false)),
        }))
    }

    /// 所有服务器运行态（按名称排序；含禁用 / 配置错误的条目）。
    pub fn servers(&self) -> Vec<McpServerState> {
        self.servers.values().map(|state| state.snapshot()).collect()
    }

    /// 所有已发现的 MCP 工具（不区分是否真的注册成功）。
    pub fn tools(&self) -> Vec<McpToolInfo> {
        let mut infos = Vec::new();
        for state in self.servers.values() {
            infos.extend(state.tool_infos());
        }
        infos
    }

    /// 重连单台服务器（异步执行，立即返回）。
    ///
    /// 想等重连真正完成用 [`McpManager::restart_async`]。
    pub fn restart(&self, name: &str) -> anyhow::Result<()> {
        let state = self.check_restart(name)?;
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| anyhow!("restart 需要在 tokio 运行时内调用。"))?;
        state.mark_reconnecting();
        let spawned = state.clone();
        let registry = self.registry.clone();
        let registered = self.registered.clone();
        let total_limit = self.total_limit;
        let paths = self.paths.clone();
        let shutdown = self.shutdown.clone();
        let server_name = spawned.name().to_string();
        handle.spawn(async move {
            if shutdown.load(Ordering::SeqCst) {
                return;
            }
            match spawned.connect(&paths).await {
                Ok(infos) => {
                    for info in &infos {
                        register_tool(&registry, &registered, &spawned, info, total_limit);
                    }
                    tracing::info!(
                        server = %server_name,
                        tools = infos.len(),
                        "MCP 服务器已重连"
                    );
                }
                Err(error) => {
                    let reason = format!("{error:#}");
                    tracing::warn!(
                        server = %server_name,
                        error = %reason,
                        "MCP 服务器重连失败"
                    );
                }
            }
        });
        Ok(())
    }

    /// 重连单台服务器并等它完成（测试 / 需要同步语义时用）。
    pub async fn restart_async(&self, name: &str) -> anyhow::Result<()> {
        let state = self.check_restart(name)?;
        state.mark_reconnecting();
        let infos = state.connect(&self.paths).await?;
        for info in &infos {
            register_tool(&self.registry, &self.registered, &state, info, self.total_limit);
        }
        tracing::info!(server = %state.name(), tools = infos.len(), "MCP 服务器已重连");
        Ok(())
    }

    fn check_restart(&self, name: &str) -> anyhow::Result<Arc<ServerState>> {
        if self.shutdown.load(Ordering::SeqCst) {
            bail!("MCP 管理器已关闭，不能再重连服务器。");
        }
        let state = self.servers.get(name).cloned().ok_or_else(|| {
            let known: Vec<String> = self.servers.keys().cloned().collect();
            anyhow!(
                "没有名为「{name}」的 MCP 服务器（已配置：{}）。",
                known.join("、")
            )
        })?;
        if !self.settings.read().mcp.enabled {
            bail!("MCP 总开关（mcp.enabled）已关闭，不能重连服务器「{name}」。");
        }
        if !state.is_enabled() {
            bail!("MCP 服务器「{name}」在配置中已禁用（enabled = false），不能重连。");
        }
        Ok(state)
    }

    /// 关闭所有 MCP 服务器（幂等）。在 tokio runtime 内会异步优雅关闭，
    /// 不在 runtime 内则同步杀进程。
    pub fn shutdown(&self) {
        if self.shutdown.swap(true, Ordering::SeqCst) {
            return;
        }
        let states: Vec<Arc<ServerState>> = self.servers.values().cloned().collect();
        if states.is_empty() {
            return;
        }
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    for state in states {
                        state.close().await;
                    }
                    tracing::debug!("MCP 管理器已关闭所有服务器");
                });
            }
            Err(_) => {
                for state in states {
                    state.force_abort();
                }
            }
        }
    }
}

impl Drop for McpManager {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        for state in self.servers.values() {
            state.force_abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use neosonia_core::{McpServerConfig, McpTransport};

    /// 只做配置过滤 / 未连接行为测试，不真的起子进程。
    fn settings_with(mcp_enabled: bool, servers: Vec<McpServerConfig>) -> Arc<RwLock<Settings>> {
        let mut settings = Settings::default();
        settings.mcp.enabled = mcp_enabled;
        settings.mcp.max_total_tools = 100;
        settings.mcp.servers = servers;
        Arc::new(RwLock::new(settings))
    }

    fn http_server(name: &str) -> McpServerConfig {
        let mut config = McpServerConfig::default();
        config.name = name.to_string();
        config.transport = McpTransport::Http;
        config.url = "http://127.0.0.1:9/mcp".to_string();
        config
    }

    fn paths() -> Paths {
        Paths::from_root(std::env::temp_dir().join(format!(
            "neosonia-mcp-manager-{}-{}",
            std::process::id(),
            neosonia_core::util::short_id()
        )))
    }

    #[tokio::test]
    async fn disabled_mcp_registers_nothing() {
        let settings = settings_with(false, vec![http_server("remote")]);
        let registry = ToolRegistry::new();
        let paths = paths();
        let _ = paths.ensure();
        let manager = McpManager::start(settings, &registry, &paths)
            .await
            .expect("总开关关闭时 start 也应成功");
        assert!(manager.servers().is_empty());
        assert!(manager.tools().is_empty());
        assert!(registry.is_empty(), "关闭时不应注册任何 MCP 工具");
        manager.shutdown();
    }

    #[tokio::test]
    async fn unreachable_server_is_isolated() {
        let mut server = http_server("remote");
        // 缩短超时，避免测试变慢（连接被拒绝会立即失败，不会真等这么久）
        server.startup_timeout_secs = 2;
        let settings = settings_with(true, vec![server]);
        let registry = ToolRegistry::new();
        let paths = paths();
        let _ = paths.ensure();
        let manager = McpManager::start(settings, &registry, &paths)
            .await
            .expect("单台失败不应让 start 报错");
        let states = manager.servers();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].name, "remote");
        assert!(!states[0].ok);
        assert!(states[0].error.is_some(), "应记录中文错误原因");
        assert_eq!(states[0].tools, 0);
        assert!(manager.tools().is_empty());
        // mcp_list_servers 仍然注册了，模型能自查
        assert!(registry.get(LIST_SERVERS_TOOL).is_some());
        manager.shutdown();
        assert!(manager.servers().iter().all(|state| !state.ok));
    }

    #[tokio::test]
    async fn broken_configs_show_up_but_do_not_connect() {
        let mut disabled = McpServerConfig::default();
        disabled.name = "off".to_string();
        disabled.enabled = false;
        disabled.command = "node".to_string();
        let settings = settings_with(true, vec![disabled]);
        let registry = ToolRegistry::new();
        let paths = paths();
        let _ = paths.ensure();
        let manager = McpManager::start(settings, &registry, &paths)
            .await
            .expect("配置问题不应让 start 报错");
        let states = manager.servers();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].name, "off");
        assert!(!states[0].ok);
        assert!(states[0].error.as_deref().unwrap_or("").contains("禁用"));
        manager.shutdown();
    }

    #[tokio::test]
    async fn restart_unknown_server_is_chinese_error() {
        let settings = settings_with(true, Vec::new());
        let registry = ToolRegistry::new();
        let paths = paths();
        let _ = paths.ensure();
        let manager = McpManager::start(settings, &registry, &paths)
            .await
            .expect("空配置 start 成功");
        let error = manager.restart("nope").expect_err("未知服务器应报错");
        assert!(error.to_string().contains("没有名为「nope」"), "{error}");
        let error = manager
            .restart_async("nope")
            .await
            .expect_err("未知服务器应报错");
        assert!(error.to_string().contains("没有名为「nope」"), "{error}");
        manager.shutdown();
        let error = manager.restart("nope").expect_err("已关闭应报错");
        assert!(error.to_string().contains("已关闭"), "{error}");
    }
}
