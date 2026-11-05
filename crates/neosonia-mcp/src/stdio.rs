//! stdio 传输：用 tokio 起子进程，newline-delimited JSON-RPC 2.0。
//!
//! 结构：
//! - 一个后台任务按行读子进程 stdout，把响应按 id 分发给等待者；EOF 时让所有
//!   等待中的请求立刻失败；
//! - 一个后台任务持续读 stderr 并保留末尾若干字节，握手失败时拼进错误信息；
//! - 写 stdin 走一把异步 Mutex，避免两条消息交错；
//! - 进程用 `kill_on_drop(true)`，并且 [`StdioSession::close`] / Drop 都会主动 kill。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use neosonia_core::{McpServerConfig, Paths};
use parking_lot::Mutex as BlockingMutex;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, oneshot};

use crate::jsonrpc;
use crate::transport::Session;

/// 请求 id -> 等待回包的发送端。
type PendingMap = BlockingMutex<BTreeMap<u64, oneshot::Sender<Result<Value, String>>>>;

/// 一条 stdio 传输的 MCP 连接。
pub(crate) struct StdioSession {
    server_name: String,
    command_line: String,
    child: BlockingMutex<Option<Child>>,
    stdin: Mutex<Option<ChildStdin>>,
    pending: Arc<PendingMap>,
    next_id: AtomicU64,
    pid: Option<u32>,
    closed: Arc<AtomicBool>,
    stderr_tail: Arc<BlockingMutex<String>>,
}

impl StdioSession {
    /// 按配置起子进程，并开始后台读 stdout / stderr。
    pub(crate) async fn spawn(config: &McpServerConfig, paths: &Paths) -> anyhow::Result<Self> {
        let command_line = format!("{} {}", config.command.trim(), config.args.join(" "))
            .trim()
            .to_string();
        let program = neosonia_core::util::expand_tilde(config.command.trim());
        let mut command = Command::new(&program);
        command.args(&config.args);
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        command.kill_on_drop(true);
        for (key, value) in &config.env {
            command.env(key, expand_env_value(value));
        }
        if let Some(cwd) = resolve_cwd(&config.cwd, paths) {
            command.current_dir(cwd);
        }
        let mut child = command.spawn().with_context(|| {
            format!(
                "启动 MCP 服务器「{}」失败（命令：{command_line}）",
                config.name
            )
        })?;
        let pid = child.id();
        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .with_context(|| format!("MCP 服务器「{}」没有 stdout 管道", config.name))?;
        let stderr = child
            .stderr
            .take()
            .with_context(|| format!("MCP 服务器「{}」没有 stderr 管道", config.name))?;

        let pending: Arc<PendingMap> = Arc::new(BlockingMutex::new(BTreeMap::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let stderr_tail = Arc::new(BlockingMutex::new(String::new()));

        // 后台读 stdout：按 id 分发响应；EOF 时让所有等待者失败。
        let pending_reader = pending.clone();
        let closed_reader = closed.clone();
        let name_reader = config.name.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        let line = line.trim();
                        if line.is_empty() {
                            continue;
                        }
                        match jsonrpc::parse_line(line) {
                            Ok(jsonrpc::ServerMessage::Response { id, result }) => {
                                let sender = pending_reader.lock().remove(&id);
                                match sender {
                                    Some(sender) => {
                                        let _ = sender.send(result.map_err(|e| e.to_string()));
                                    }
                                    None => tracing::debug!(
                                        server = %name_reader,
                                        id,
                                        "收到无人等待的 JSON-RPC 回包（可能请求已超时）"
                                    ),
                                }
                            }
                            Ok(jsonrpc::ServerMessage::Notification) => {
                                tracing::trace!(server = %name_reader, "收到 MCP 服务器通知");
                            }
                            Ok(jsonrpc::ServerMessage::Request { method }) => {
                                tracing::debug!(
                                    server = %name_reader,
                                    method = %method,
                                    "服务器反向请求客户端能力，本客户端不支持，已忽略"
                                );
                            }
                            Err(e) => {
                                tracing::warn!(
                                    server = %name_reader,
                                    error = %e,
                                    "MCP 服务器 stdout 上收到无法解析的行，已跳过"
                                );
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!(
                            server = %name_reader,
                            error = %e,
                            "读取 MCP 服务器 stdout 失败"
                        );
                        break;
                    }
                }
            }
            // stdout 断掉：所有等待中的请求立即失败，避免挂到超时。
            closed_reader.store(true, Ordering::SeqCst);
            let waiting = std::mem::take(&mut *pending_reader.lock());
            for (_, sender) in waiting {
                let _ = sender.send(Err("MCP 服务器进程已退出（stdout 关闭）。".to_string()));
            }
        });

        // 后台读 stderr：只保留末尾 4000 字符，用于诊断握手失败。
        let tail_writer = stderr_tail.clone();
        let name_stderr = config.name.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        tracing::debug!(server = %name_stderr, line = %line, "MCP 服务器 stderr");
                        let mut tail = tail_writer.lock();
                        tail.push_str(&line);
                        tail.push('\n');
                        trim_tail(&mut tail, 4000);
                    }
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            server_name: config.name.clone(),
            command_line,
            child: BlockingMutex::new(Some(child)),
            stdin: Mutex::new(stdin),
            pending,
            next_id: AtomicU64::new(0),
            pid,
            closed,
            stderr_tail,
        })
    }

    /// 写一条 JSON-RPC 消息（末尾加换行，MCP stdio 的分帧方式）。
    async fn write_line(&self, value: &Value) -> anyhow::Result<()> {
        let mut line = serde_json::to_string(value).context("序列化 JSON-RPC 消息失败")?;
        line.push('\n');
        let mut stdin = self.stdin.lock().await;
        let Some(io) = stdin.as_mut() else {
            bail!("MCP 服务器「{}」的 stdin 已关闭。", self.server_name);
        };
        io.write_all(line.as_bytes()).await.with_context(|| {
            format!(
                "写入 MCP 服务器「{}」的 stdin 失败（进程可能已退出，命令：{}）",
                self.server_name, self.command_line
            )
        })?;
        io.flush()
            .await
            .with_context(|| format!("刷新 MCP 服务器「{}」的 stdin 失败", self.server_name))?;
        Ok(())
    }
}

#[async_trait]
impl Session for StdioSession {
    async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        if self.closed.load(Ordering::SeqCst) {
            bail!("MCP 服务器「{}」的连接已关闭。", self.server_name);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (sender, receiver) = oneshot::channel::<Result<Value, String>>();
        self.pending.lock().insert(id, sender);
        // 超时 / 取消时把无人认领的 pending 清掉，避免 map 泄漏。
        let _guard = PendingGuard {
            pending: self.pending.clone(),
            id,
        };
        self.write_line(&jsonrpc::request(id, method, params))
            .await?;
        let result = receiver.await.map_err(|_| {
            anyhow!(
                "MCP 服务器「{}」在等待 {method} 回包时断开。",
                self.server_name
            )
        })?;
        result.map_err(|message| {
            anyhow!(
                "MCP 服务器「{}」返回错误（{method}）：{message}",
                self.server_name
            )
        })
    }

    async fn notify(&self, method: &str, params: Value) -> anyhow::Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            bail!("MCP 服务器「{}」的连接已关闭。", self.server_name);
        }
        self.write_line(&jsonrpc::notification(method, params))
            .await
    }

    fn pid(&self) -> Option<u32> {
        self.pid
    }

    fn stderr_tail(&self) -> String {
        self.stderr_tail.lock().clone()
    }

    async fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        // 先关 stdin：多数 MCP 服务器读到 EOF 会自己退出。
        let stdin = self.stdin.lock().await.take();
        if let Some(mut stdin) = stdin {
            let _ = stdin.shutdown().await;
        }
        let child = self.child.lock().take();
        if let Some(mut child) = child {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
        }
        // 让还在等待的请求立刻失败。
        let waiting = std::mem::take(&mut *self.pending.lock());
        for (_, sender) in waiting {
            let _ = sender.send(Err("MCP 服务器已关闭。".to_string()));
        }
    }
}

impl Drop for StdioSession {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
        if let Some(mut child) = self.child.lock().take() {
            let _ = child.start_kill();
        }
    }
}

/// 请求结束时如果还没被认领，就把 pending 里对应条目清掉（超时 / 取消路径）。
struct PendingGuard {
    pending: Arc<PendingMap>,
    id: u64,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        let _ = self.pending.lock().remove(&self.id);
    }
}

/// 展开 `env` 里的 `${VAR}` 写法；没设这个变量就保持原样。
fn expand_env_value(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.len() > 3 && trimmed.starts_with("${") && trimmed.ends_with('}') {
        let name = &trimmed[2..trimmed.len() - 1];
        if let Ok(value) = std::env::var(name) {
            return value;
        }
    }
    raw.to_string()
}

/// 解析 cwd：空 = 项目根；相对路径相对项目根；不存在则退回当前进程工作目录。
fn resolve_cwd(raw: &str, paths: &Paths) -> Option<PathBuf> {
    let trimmed = raw.trim();
    let candidate = if trimmed.is_empty() {
        paths.root.clone()
    } else {
        let expanded = neosonia_core::util::expand_tilde(trimmed);
        if expanded.is_absolute() {
            expanded
        } else {
            paths.root.join(expanded)
        }
    };
    if candidate.is_dir() {
        Some(candidate)
    } else {
        tracing::warn!(
            cwd = %candidate.display(),
            "MCP 服务器的 cwd 不存在，改用当前进程工作目录"
        );
        None
    }
}

/// 只保留 string 末尾 `max_bytes` 字节（按字符边界裁剪）。
fn trim_tail(text: &mut String, max_bytes: usize) {
    if text.len() <= max_bytes {
        return;
    }
    let mut start = text.len() - max_bytes;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    *text = text[start..].to_string();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_value_expansion() {
        assert_eq!(expand_env_value("plain"), "plain");
        // PATH 一定存在，用来验证 `${VAR}` 展开
        let path = std::env::var("PATH").unwrap_or_default();
        assert_eq!(expand_env_value(" ${PATH} "), path);
        // 不存在的变量保持原样
        assert_eq!(
            expand_env_value("${NEOSONIA_MCP_绝不存在的变量}"),
            "${NEOSONIA_MCP_绝不存在的变量}"
        );
        assert_eq!(expand_env_value("${}"), "${}");
    }

    #[test]
    fn cwd_resolution() {
        let root = std::env::temp_dir().join(format!(
            "neosonia-mcp-test-{}-{}",
            std::process::id(),
            neosonia_core::util::short_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let paths = Paths::from_root(root.clone());

        assert_eq!(resolve_cwd("", &paths), Some(root.clone()));
        assert_eq!(resolve_cwd("   ", &paths), Some(root.clone()));
        assert_eq!(resolve_cwd(".", &paths), Some(root.join(".")));
        // 不存在的目录返回 None（交给调用方继承当前工作目录）
        assert_eq!(resolve_cwd("不存在的子目录", &paths), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn trim_tail_keeps_valid_utf8() {
        let mut text = "中文中文中文abc".to_string();
        trim_tail(&mut text, 4);
        assert!(text.len() <= 4, "实际：{text}");
        assert!(text.ends_with("abc"), "实际：{text}");

        let mut short = "短".to_string();
        trim_tail(&mut short, 100);
        assert_eq!(short, "短");
    }
}
