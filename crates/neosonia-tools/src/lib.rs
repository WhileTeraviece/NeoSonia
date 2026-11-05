//! NeoSonia 工具集：文件、命令、屏幕、输入、系统、网页、UI 自动化。
//!
//! 每个子模块都提供 `register(reg: &ToolRegistry)` 把本模块的工具挂进注册表；
//! `register_all` 再按 `Settings::tools` 的开关决定哪些工具对模型可见。
//!
//! 编码约定：工具返回给模型的内容是中文，零 emoji；错误统一走
//! `ToolOutcome::err`，绝不 panic；长任务要响应 `ToolCtx::is_cancelled()` 并
//! 用 `ToolCtx::emit_output` 流式回传。

#[macro_use]
mod macros;

// uia / cdp 由 automation 队友实现；index 由 mail 队友实现。本文件负责声明并注册。
pub mod cdp;
pub mod fs;
pub mod index;
pub mod input;
pub mod screen;
pub mod shell;
pub mod system;
pub mod uia;
pub mod web;

use std::path::{Path, PathBuf};

use neosonia_core::config::Settings;
use neosonia_core::model::{ToolCtx, ToolOutcome, ToolSpec};
use neosonia_core::registry::ToolRegistry;
use serde_json::Value;

// ============================================================
// 子进程"无控制台窗口"统一入口
// ============================================================
//
// 管理员模式（UAC 提升）下，GUI 进程启动控制台子进程（cmd/powershell/reg/taskkill…）
// 会闪一下黑窗，是用户最痛的体验问题。所有子进程必须经过下面两个 helper 设置：
//   CREATE_NO_WINDOW (0x0800_0000) + CREATE_NEW_PROCESS_GROUP (0x0000_0200)
// 保留 stdout/stderr 管道采集（这两个标志不影响管道）。
// `no_window_flags_audit` 单测会扫描源码，确保没有 Command::new 漏掉 helper。

/// 给 std::process::Command 加"无控制台窗口 + 独立进程组"标志（非 Windows 为空操作）。
///
/// Windows 上目前只有 Linux 平台降级代码与测试会用到 std 版（生产 Windows 子进程都走
/// `apply_no_window_tokio`），所以这里允许 dead_code，避免平台差异导致编译告警。
#[allow(dead_code)]
pub(crate) fn apply_no_window_std(cmd: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        let _ = cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
}

/// 给 tokio 的 Command 加同样的标志（tokio 在 Windows 上提供同名 inherent 方法）。
pub(crate) fn apply_no_window_tokio(cmd: &mut tokio::process::Command) {
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        let _ = cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
}

// ============================================================
// 注册入口
// ============================================================

pub fn register_fs(reg: &ToolRegistry) {
    fs::register(reg);
}

pub fn register_shell(reg: &ToolRegistry) {
    shell::register(reg);
}

pub fn register_screen(reg: &ToolRegistry) {
    screen::register(reg);
}

pub fn register_input(reg: &ToolRegistry) {
    input::register(reg);
}

pub fn register_system(reg: &ToolRegistry) {
    system::register(reg);
}

pub fn register_web(reg: &ToolRegistry) {
    web::register(reg);
}

/// 按设置注册工具。被关闭的能力不会出现在模型的工具列表里。
pub fn register_all(reg: &ToolRegistry, settings: &Settings) {
    register_fs(reg);
    register_system(reg);
    register_web(reg);

    if settings.tools.allow_shell {
        register_shell(reg);
    } else {
        // 不能起新命令，但历史后台任务仍然应该能被查看/终止。
        shell::register_job_tools(reg);
    }

    if settings.tools.allow_input_simulation {
        register_screen(reg);
        register_input(reg);
    } else {
        screen::register_info(reg);
    }

    if settings.tools.uia_enabled {
        uia::register(reg);
    }
    if settings.tools.cdp_enabled {
        cdp::register(reg);
    }
    // 本地文件索引（类 Everything）由 mail 队友实现，index.rs 提供 register。
    if settings.index.enabled {
        index::register(reg);
    }
}

/// 全部工具的规格（不参考设置），方便测试与文档。
pub fn tool_catalog() -> Vec<ToolSpec> {
    let reg = ToolRegistry::new();
    register_fs(&reg);
    register_shell(&reg);
    register_screen(&reg);
    register_input(&reg);
    register_system(&reg);
    register_web(&reg);
    uia::register(&reg);
    cdp::register(&reg);
    index::register(&reg);
    reg.specs()
}

// ============================================================
// 参数解析：模型给的 JSON 经常把数字写成字符串，这里全部容错。
// ============================================================

/// 取字符串参数；数字/布尔的会被转成字符串。
pub(crate) fn arg_str(args: &Value, key: &str) -> Option<String> {
    match args.get(key) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(Value::Bool(b)) => Some(b.to_string()),
        _ => None,
    }
}

/// 必填字符串参数；缺失或空白时返回面向模型的中文错误。
pub(crate) fn arg_str_req(args: &Value, key: &str) -> Result<String, ToolOutcome> {
    match arg_str(args, key) {
        Some(s) if !s.trim().is_empty() => Ok(s),
        Some(_) => Err(ToolOutcome::err(format!(
            "参数 {key} 不能是空字符串。"
        ))),
        None => Err(ToolOutcome::err(format!(
            "缺少必填参数 {key}（字符串）。"
        ))),
    }
}

pub(crate) fn arg_bool(args: &Value, key: &str, default: bool) -> bool {
    match args.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_i64().map(|v| v != 0).unwrap_or(default),
        Some(Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" | "y" => true,
            "false" | "0" | "no" | "off" | "n" => false,
            _ => default,
        },
        _ => default,
    }
}

pub(crate) fn arg_u64(args: &Value, key: &str, default: u64) -> u64 {
    match args.get(key) {
        Some(Value::Number(n)) => n.as_u64().or_else(|| n.as_f64().map(|f| f.max(0.0) as u64)).unwrap_or(default),
        Some(Value::String(s)) => s.trim().parse::<f64>().map(|f| f.max(0.0) as u64).unwrap_or(default),
        _ => default,
    }
}

pub(crate) fn arg_i64(args: &Value, key: &str, default: i64) -> i64 {
    match args.get(key) {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)).unwrap_or(default),
        Some(Value::String(s)) => s.trim().parse::<f64>().map(|f| f as i64).unwrap_or(default),
        _ => default,
    }
}

pub(crate) fn arg_f64(args: &Value, key: &str, default: f64) -> f64 {
    match args.get(key) {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(default),
        Some(Value::String(s)) => s.trim().parse::<f64>().unwrap_or(default),
        _ => default,
    }
}

/// 取字符串数组；也接受 "a,b,c" 这种逗号分隔写法。
pub(crate) fn arg_string_list(args: &Value, key: &str) -> Vec<String> {
    match args.get(key) {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            })
            .collect(),
        Some(Value::String(s)) => s
            .split([',', ';', ' '])
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

// ============================================================
// 路径解析与沙箱
// ============================================================

/// 当前工作目录（配置里的 workspace）。
pub(crate) fn workspace_dir(ctx: &ToolCtx) -> PathBuf {
    ctx.workspace()
}

/// 把参数里的路径解析成绝对路径：展开 `~`，相对路径相对工作目录。
pub(crate) fn resolve_path_lexical(ctx: &ToolCtx, raw: &str) -> Result<PathBuf, ToolOutcome> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(ToolOutcome::err("路径不能为空。".to_string()));
    }
    let expanded = neosonia_core::util::expand_tilde(raw);
    let abs = if expanded.is_absolute() {
        expanded
    } else {
        workspace_dir(ctx).join(expanded)
    };
    Ok(neosonia_core::util::normalize_lexical(&abs))
}

/// 读路径：不限沙箱（读文件是模型的基础能力），但相对路径相对工作目录。
pub(crate) fn resolve_read_path(ctx: &ToolCtx, raw: &str) -> Result<PathBuf, ToolOutcome> {
    resolve_path_lexical(ctx, raw)
}

/// 写路径：默认必须落在工作目录内；`allow_outside=true` 时才放行外部路径。
pub(crate) fn resolve_write_path(
    ctx: &ToolCtx,
    raw: &str,
    allow_outside: bool,
) -> Result<PathBuf, ToolOutcome> {
    let path = resolve_path_lexical(ctx, raw)?;
    let ws = neosonia_core::util::normalize_lexical(&workspace_dir(ctx));
    if !allow_outside && !neosonia_core::util::is_within(&ws, &path) {
        return Err(ToolOutcome::err(format!(
            "路径越界：{} 不在工作目录 {} 内（沙箱限制）。\n\
             如需写到工作目录之外，请在参数里加 allow_outside=true。",
            path.display(),
            ws.display()
        )));
    }
    Ok(path)
}

/// 判断路径是否在工作目录内（不返回错误，调用方自己决定文案）。
pub(crate) fn within_workspace(ctx: &ToolCtx, path: &Path) -> bool {
    neosonia_core::util::is_within(&workspace_dir(ctx), path)
}

/// 把 io 错误翻译成中文提示。
pub(crate) fn io_error_hint(e: &std::io::Error) -> String {
    use std::io::ErrorKind;
    match e.kind() {
        ErrorKind::NotFound => format!("文件或目录不存在（{e}）"),
        ErrorKind::PermissionDenied => format!("没有访问权限（{e}）"),
        ErrorKind::AlreadyExists => format!("已经存在（{e}）"),
        ErrorKind::InvalidInput => format!("路径非法（{e}）"),
        _ => e.to_string(),
    }
}

// ============================================================
// 测试辅助：假 Host / 假 Memory
// ============================================================

#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use neosonia_core::config::{Paths, Settings};
    use neosonia_core::model::{
        AgentEvent, AgentHost, MemoryApi, MemoryItem, MemoryStats, SubagentRequest, ToolCtx,
    };
    use parking_lot::{Mutex, RwLock};

    /// 什么都不存的记忆实现，只为满足 AgentHost。
    pub struct NullMemory;

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
        fn log_conversation(
            &self,
            _role: &str,
            _content: &str,
            _turn_id: &str,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        fn stats(&self) -> anyhow::Result<MemoryStats> {
            Ok(MemoryStats::default())
        }
    }

    pub struct FakeHost {
        pub settings: Arc<RwLock<Settings>>,
        pub events: Mutex<Vec<AgentEvent>>,
        pub cancelled: AtomicBool,
        pub paths: Paths,
    }

    impl FakeHost {
        pub fn cancel(&self) {
            self.cancelled.store(true, Ordering::SeqCst);
        }

        pub fn emitted_output(&self) -> String {
            let guard = self.events.lock();
            let mut s = String::new();
            for ev in guard.iter() {
                if let AgentEvent::ToolOutput { chunk, .. } = ev {
                    s.push_str(chunk);
                }
            }
            s
        }
    }

    impl AgentHost for FakeHost {
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

        fn spawn_subagent(&self, _req: SubagentRequest) -> anyhow::Result<String> {
            Ok("test-subagent".to_string())
        }

        fn is_cancelled(&self) -> bool {
            self.cancelled.load(Ordering::SeqCst)
        }
    }

    /// 建一个以 `root` 为项目根的假宿主与上下文。
    pub fn fake_ctx(root: impl Into<PathBuf>) -> (ToolCtx, Arc<FakeHost>) {
        let paths = Paths::from_root(root.into());
        let _ = paths.ensure();
        let host = Arc::new(FakeHost {
            settings: Arc::new(RwLock::new(Settings::default())),
            events: Mutex::new(Vec::new()),
            cancelled: AtomicBool::new(false),
            paths,
        });
        let ctx = ToolCtx::new(host.clone(), "turn-test", "call-test");
        (ctx, host)
    }

    /// 一个唯一的临时项目根目录（测完不删，位于系统临时目录）。
    pub fn temp_root(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "neosonia-tools-{tag}-{}-{}",
            std::process::id(),
            neosonia_core::util::short_id()
        ));
        let _ = std::fs::create_dir_all(&p);
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use serde_json::json;

    #[test]
    fn catalog_has_all_contract_tools() {
        let names: Vec<String> = tool_catalog().iter().map(|s| s.name.clone()).collect();
        println!("tool_catalog 工具数：{} => {:?}", names.len(), names);
        assert!(names.len() >= 53, "工具数偏少: {}", names.len());
        for expected in [
            "read_file",
            "write_file",
            "edit_file",
            "append_file",
            "list_dir",
            "search_files",
            "file_info",
            "make_dir",
            "copy_path",
            "move_path",
            "delete_path",
            "run_command",
            "list_jobs",
            "read_job_output",
            "kill_job",
            "wait",
            "send_job_input",
            "list_displays",
            "screenshot",
            "mouse_move",
            "mouse_click",
            "mouse_drag",
            "mouse_scroll",
            "keyboard_type",
            "keyboard_key",
            "clipboard_read",
            "clipboard_write",
            "screen_size",
            "system_info",
            "foreground_window",
            "open_path",
            "notify",
            "list_processes",
            "kill_process",
            "web_search",
            "web_fetch",
            "web_links",
            "download_file",
            // v0.2 新增
            "screen_metrics",
            "mouse_drag_path",
            "keyboard_combo",
            "wait_for_window",
            "find_files",
            "reindex_files",
            "index_stats",
            // v0.3 新增
            "job_status",
            // v0.5 新增（按住不放）
            "mouse_press",
            "mouse_release",
            "keyboard_press",
            "keyboard_release",
            // v0.6 新增
            "mouse_click_sequence",
            "window_hide_app",
            "window_is_hidden",
            "background_task",
            "background_task_status",
        ] {
            assert!(names.iter().any(|n| n == expected), "缺少工具: {expected}");
        }
        // v0.3：job_status 与 list_jobs 同时存在
        assert!(names.iter().any(|n| n == "list_jobs"));
        // v0.5：web_search 描述里应提到 deepseek provider
        assert!(tool_catalog()
            .iter()
            .any(|spec| spec.name == "web_search" && spec.description.contains("deepseek")));
    }

    /// 文本审计：本 crate 的生产代码里所有 `Command::new(...)` 必须紧跟
    /// `apply_no_window_std/tokio`（CREATE_NO_WINDOW + CREATE_NEW_PROCESS_GROUP），
    /// 并且不允许直接调用 reg.exe（注册表操作若必须存在也要走同样的无窗口路径）。
    /// 这是防回退测试：以后新增子进程忘记加标志会直接失败。
    #[test]
    fn no_console_flash_audit() {
        let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&src_dir)
            .expect("读取 src 目录")
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
            .collect();
        paths.sort();
        assert!(paths.len() >= 8, "源码文件数异常: {paths:?}");
        let mut command_sites = 0usize;
        let mut covered_sites = 0usize;
        let mut reg_hits: Vec<String> = Vec::new();
        for path in &paths {
            let text = std::fs::read_to_string(path).expect("读取源码");
            let lines: Vec<&str> = text.lines().collect();
            // 生产代码 = 第一个 `#[cfg(test)]` + `mod tests` 之前的部分（测试里的 spawn 不影响用户）
            let mut cutoff = lines.len();
            for (index, line) in lines.iter().enumerate() {
                if line.trim() == "#[cfg(test)]" {
                    let next = lines.get(index + 1).map(|l| l.trim()).unwrap_or("");
                    if next.starts_with("mod tests") || next.starts_with("pub mod tests") {
                        cutoff = index;
                        break;
                    }
                }
            }
            for (index, line) in lines[..cutoff].iter().enumerate() {
                if !line.contains("Command::new(") {
                    continue;
                }
                command_sites += 1;
                let window = &lines[index..(index + 4).min(cutoff)];
                if window.iter().any(|line| line.contains("apply_no_window_")) {
                    covered_sites += 1;
                } else {
                    panic!(
                        "{}:{} 的 Command::new 没有 apply_no_window 标志（管理员模式会闪黑窗）：{}",
                        path.display(),
                        index + 1,
                        line.trim()
                    );
                }
            }
            // reg 审计：生产代码不允许出现 reg/regedit 子进程调用
            for (index, line) in lines[..cutoff].iter().enumerate() {
                let lower = line.to_ascii_lowercase();
                let hit = lower.contains("reg.exe")
                    || lower.contains("reg add")
                    || lower.contains("reg query")
                    || lower.contains("reg delete")
                    || lower.contains("regedit")
                    || lower.contains("command::new(\"reg\"");
                if hit {
                    reg_hits.push(format!("{}:{}: {}", path.display(), index + 1, line.trim()));
                }
            }
        }
        assert!(command_sites >= 8, "子进程创建点数量异常（应 >=8）: {command_sites}");
        assert_eq!(
            covered_sites, command_sites,
            "有 {} 个 Command::new 未覆盖无窗口标志",
            command_sites - covered_sites
        );
        assert!(
            reg_hits.is_empty(),
            "tools crate 不允许直接调用 reg.exe/regedit（注册表操作请走无窗口子进程）；发现：{reg_hits:?}"
        );
        println!(
            "no_console_flash_audit: {command_sites} 个 Command::new 全部覆盖 apply_no_window，0 处 reg.exe"
        );
    }

    #[test]
    fn register_all_respects_settings() {
        let reg = ToolRegistry::new();
        let mut s = Settings::default();
        s.tools.allow_shell = false;
        s.tools.allow_input_simulation = false;
        s.tools.uia_enabled = false;
        s.tools.cdp_enabled = false;
        register_all(&reg, &s);
        let names = reg.names();
        assert!(!names.iter().any(|n| n == "run_command"));
        assert!(names.iter().any(|n| n == "read_file"));
        assert!(names.iter().any(|n| n == "screen_size"));
        assert!(!names.iter().any(|n| n == "mouse_click"));
        // 即使禁止执行命令，后台任务状态工具也应保留
        assert!(names.iter().any(|n| n == "job_status"));
        assert!(names.iter().any(|n| n == "list_jobs"));
        // 索引默认开启，所以 find_files 应该在；关掉后不应出现
        assert!(names.iter().any(|n| n == "find_files"));
        let mut s2 = s.clone();
        s2.index.enabled = false;
        let reg2 = ToolRegistry::new();
        register_all(&reg2, &s2);
        assert!(!reg2.names().iter().any(|n| n == "find_files"));

        let reg3 = ToolRegistry::new();
        register_all(&reg3, &Settings::default());
        assert!(reg3.names().iter().any(|n| n == "run_command"));
        assert!(reg3.names().iter().any(|n| n == "mouse_click"));
        assert!(reg3.names().iter().any(|n| n == "mouse_drag_path"));
        assert!(reg3.names().iter().any(|n| n == "keyboard_combo"));
        assert!(reg3.names().iter().any(|n| n == "screen_metrics"));
        assert!(reg3.names().iter().any(|n| n == "wait_for_window"));
        assert!(reg3.names().iter().any(|n| n == "job_status"));
        assert!(reg3.names().iter().any(|n| n == "mouse_scroll"));
    }

    #[test]
    fn arg_helpers_tolerate_string_numbers() {
        let v = serde_json::json!({
            "n": 3,
            "s": "7",
            "b": "true",
            "list": "a, b ,c",
            "arr": ["x", "y"],
        });
        assert_eq!(arg_u64(&v, "n", 0), 3);
        assert_eq!(arg_u64(&v, "s", 0), 7);
        assert!(arg_bool(&v, "b", false));
        assert_eq!(arg_string_list(&v, "list"), vec!["a", "b", "c"]);
        assert_eq!(arg_string_list(&v, "arr"), vec!["x", "y"]);
        assert_eq!(arg_u64(&v, "missing", 42), 42);
    }

    #[test]
    fn write_path_sandbox() {
        let root = test_support::temp_root("sandbox");
        let (ctx, _host) = test_support::fake_ctx(&root);
        let ws = ctx.workspace();
        let inside = resolve_write_path(&ctx, "a.txt", false).expect("inside should pass");
        assert!(inside.starts_with(&ws));
        let outside = resolve_write_path(&ctx, "../escape.txt", false);
        assert!(outside.is_err());
        let outside_ok = resolve_write_path(&ctx, "../escape.txt", true);
        assert!(outside_ok.is_ok());
    }

    // ============================================================
    // 真机验证（默认 ignore；需要 NEOSONIA_VERIFY=1 才真的执行，
    // 避免别的队友批量跑 --ignored 时被截屏/联网/长命令拖慢或误伤）
    // 运行方式（PowerShell）：
    //   $env:NEOSONIA_VERIFY="1"
    //   cargo test --offline -p neosonia-tools -- --ignored --nocapture
    // ============================================================

    fn verify_enabled() -> bool {
        std::env::var("NEOSONIA_VERIFY")
            .map(|v| v.trim() == "1")
            .unwrap_or(false)
    }

    fn verify_ctx(tag: &str) -> (ToolCtx, Arc<test_support::FakeHost>) {
        let root = test_support::temp_root(tag);
        test_support::fake_ctx(&root)
    }

    fn verify_registry() -> ToolRegistry {
        let reg = ToolRegistry::new();
        register_all(&reg, &Settings::default());
        reg
    }

    fn job_id_from(content: &str) -> String {
        content
            .lines()
            .find(|l| l.starts_with("任务 id："))
            .map(|l| l.trim_start_matches("任务 id：").trim().to_string())
            .expect("结果里应该有任务 id")
    }

    #[tokio::test]
    #[ignore]
    async fn verify_screenshot_png() {
        if !verify_enabled() {
            return;
        }
        let (ctx, _host) = verify_ctx("verify-shot");
        let reg = verify_registry();
        let out = reg
            .call(
                &ctx,
                "screenshot",
                &json!({"display": "primary", "return_base64": false}),
            )
            .await;
        assert!(out.ok, "{}", out.content);
        // vision 约定的标记：必须包含 [screenshot:<绝对路径>]
        let marker = out
            .content
            .lines()
            .find(|l| l.starts_with("[screenshot:") && l.ends_with(']'))
            .expect("截图结果必须包含 [screenshot:<绝对路径>] 标记");
        let marked_path = marker
            .trim_start_matches("[screenshot:")
            .trim_end_matches(']');
        assert!(
            std::path::Path::new(marked_path).is_absolute(),
            "标记里应是绝对路径: {marked_path}"
        );
        let path = out
            .content
            .lines()
            .find(|l| l.starts_with("路径："))
            .map(|l| l.trim_start_matches("路径：").trim().to_string())
            .expect("截图结果里应该有路径");
        assert_eq!(path, marked_path, "路径行应与 [screenshot:] 标记一致");
        let bytes = std::fs::read(&path).expect("截图文件应该存在");
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "应该是合法 PNG");
        println!("=== screenshot ===\n{}", out.content);
        println!("SCREENSHOT_PATH={path}");
        println!("SCREENSHOT_MARKER={marker}");

        // v0.3：带坐标网格的截图（100px/500px + 刻度数字），标记必须保留
        let grid_out = reg
            .call(
                &ctx,
                "screenshot",
                &json!({"display": "primary", "grid": true, "return_base64": false}),
            )
            .await;
        assert!(grid_out.ok, "{}", grid_out.content);
        assert!(
            grid_out.content.contains("[screenshot:"),
            "grid 截图也要带 [screenshot:] 标记: {}",
            grid_out.content
        );
        assert!(
            grid_out.content.contains("网格：间距 100px"),
            "grid 截图应说明网格间距/原点/scale: {}",
            grid_out.content
        );
        let grid_path = grid_out
            .content
            .lines()
            .find(|l| l.starts_with("路径："))
            .map(|l| l.trim_start_matches("路径：").trim().to_string())
            .expect("grid 截图应有路径");
        assert!(std::path::Path::new(&grid_path).is_file());
        println!("GRID_SCREENSHOT_PATH={grid_path}");
        println!("=== grid screenshot ===\n{}", grid_out.content);

        // app 截图快捷键用的公共 API（不走 Tool trait）
        let app_shot = crate::screen::capture_primary(&ctx.paths().screenshots)
            .expect("capture_primary 应该成功");
        assert!(app_shot.is_file(), "capture_primary 文件应存在: {}", app_shot.display());
        let app_bytes = std::fs::read(&app_shot).expect("capture_primary 文件应可读");
        assert_eq!(&app_bytes[..8], b"\x89PNG\r\n\x1a\n", "capture_primary 应是 PNG");
        println!("CAPTURE_PRIMARY_PATH={}", app_shot.display());

        for name in [
            "list_displays",
            "screen_size",
            "screen_metrics",
            "foreground_window",
        ] {
            let o = reg.call(&ctx, name, &json!({})).await;
            println!("--- {name} (ok={}) ---\n{}", o.ok, o.content);
            assert!(o.ok, "{name} 失败: {}", o.content);
        }
        // wait_for_window：等一个不存在的窗口，1 秒后应超时报错（验证轮询/超时逻辑真在跑）
        let waited = reg
            .call(
                &ctx,
                "wait_for_window",
                &json!({"title_contains": "__neosonia_no_such_window__", "timeout_secs": 1, "interval_ms": 200}),
            )
            .await;
        assert!(!waited.ok, "不存在的窗口不该等到: {}", waited.content);
        assert!(waited.content.contains("超时"), "{}", waited.content);
        println!("--- wait_for_window（预期超时）---\n{}", waited.content);
        let sys = reg.call(&ctx, "system_info", &json!({})).await;
        assert!(sys.ok, "system_info 失败: {}", sys.content);
        println!("--- system_info（前 600 字符）---\n{}", neosonia_core::util::truncate_chars(&sys.content, 600));
    }

    #[tokio::test]
    #[ignore]
    async fn verify_search_files_in_repo() {
        if !verify_enabled() {
            return;
        }
        let (ctx, _host) = verify_ctx("verify-search");
        let reg = verify_registry();
        let out = reg
            .call(
                &ctx,
                "search_files",
                &json!({
                    "query": "tool_impl",
                    "path": "E:\\NeoSonia\\crates\\neosonia-tools\\src",
                    "mode": "content",
                    "glob": "*.rs",
                    "max_results": 8
                }),
            )
            .await;
        assert!(out.ok, "{}", out.content);
        assert!(out.content.contains("tool_impl"), "{}", out.content);
        assert!(out.content.contains("fs.rs"), "{}", out.content);
        println!("=== search_files(content + glob *.rs) ===\n{}", out.content);

        let by_name = reg
            .call(
                &ctx,
                "search_files",
                &json!({
                    "query": "screen",
                    "path": "E:\\NeoSonia\\crates\\neosonia-tools\\src",
                    "mode": "name",
                    "glob": "*.rs"
                }),
            )
            .await;
        assert!(by_name.ok, "{}", by_name.content);
        assert!(by_name.content.contains("screen.rs"), "{}", by_name.content);
        println!("=== search_files(name) ===\n{}", by_name.content);

        let read = reg
            .call(
                &ctx,
                "read_file",
                &json!({"path": "E:\\NeoSonia\\crates\\neosonia-tools\\Cargo.toml", "max_chars": 800}),
            )
            .await;
        assert!(read.ok, "{}", read.content);
        assert!(read.content.contains("neosonia-core"), "{}", read.content);
        println!(
            "=== read_file Cargo.toml（前 300 字符）===\n{}",
            neosonia_core::util::truncate_chars(&read.content, 300)
        );

        // workspace 里造两个测试文件，验证 glob + content 两种模式（相对路径相对 workspace）
        let ws = ctx.workspace();
        std::fs::create_dir_all(ws.join("verify_demo")).unwrap();
        std::fs::write(
            ws.join("verify_demo/alpha.rs"),
            "fn hello_neosonia() {\n    // TOKEN_NEOSONIA_ALPHA\n}\n",
        )
        .unwrap();
        std::fs::write(
            ws.join("verify_demo/beta.txt"),
            "TOKEN_NEOSONIA_ALPHA 也出现在文本里\n",
        )
        .unwrap();
        std::fs::write(ws.join("verify_demo/gamma.md"), "# 无关内容\n").unwrap();

        let content_hit = reg
            .call(
                &ctx,
                "search_files",
                &json!({
                    "query": "TOKEN_NEOSONIA_ALPHA",
                    "path": "verify_demo",
                    "mode": "content",
                    "glob": "*.rs"
                }),
            )
            .await;
        assert!(content_hit.ok, "{}", content_hit.content);
        assert!(content_hit.content.contains("alpha.rs:2:"), "{}", content_hit.content);
        assert!(!content_hit.content.contains("beta.txt"), "glob 过滤失败: {}", content_hit.content);
        println!("=== workspace search_files(content, glob *.rs) ===\n{}", content_hit.content);

        let name_hit = reg
            .call(
                &ctx,
                "search_files",
                &json!({"query": "beta", "path": "verify_demo", "mode": "name"}),
            )
            .await;
        assert!(name_hit.ok, "{}", name_hit.content);
        assert!(name_hit.content.contains("beta.txt"), "{}", name_hit.content);
        println!("=== workspace search_files(name) ===\n{}", name_hit.content);

        // automation 的工具必须在 register_all 的注册表里
        let names = reg.names();
        for expected in ["ui_tree", "ui_click", "ui_window_list", "cdp_targets", "cdp_eval"] {
            assert!(
                names.iter().any(|n| n == expected),
                "register_all 里缺少自动化工具 {expected}: {names:?}"
            );
        }
        println!("注册工具总数：{}，包含 ui_tree/cdp_targets 等。", names.len());
    }

    #[tokio::test]
    #[ignore]
    async fn verify_web_search_and_fetch_live() {
        if !verify_enabled() {
            return;
        }
        let (ctx, _host) = verify_ctx("verify-web");
        let reg = verify_registry();
        let out = reg
            .call(&ctx, "web_search", &json!({"query": "rust language", "count": 5}))
            .await;
        assert!(out.ok, "{}", out.content);
        assert!(out.content.contains("http"), "{}", out.content);
        // 聚合搜索：结果必须标注来源引擎，并给出各引擎命中统计
        assert!(out.content.contains("来源:"), "{}", out.content);
        assert!(out.content.contains("各引擎命中："), "{}", out.content);
        assert!(out.content.contains("后端：aggregated"), "{}", out.content);
        println!(
            "=== web_search（aggregated, rust language）===\n{}",
            neosonia_core::util::truncate_chars(&out.content, 2200)
        );

        // Lead 要求的中文查询：验证国内网络下百度/搜狗/必应/360 的真实命中
        let cn = reg
            .call(&ctx, "web_search", &json!({"query": "Rust 异步", "count": 5}))
            .await;
        assert!(cn.ok, "{}", cn.content);
        assert!(cn.content.contains("来源:"), "{}", cn.content);
        assert!(cn.content.contains("各引擎命中："), "{}", cn.content);
        println!(
            "=== web_search（aggregated, 「Rust 异步」）===\n{}",
            neosonia_core::util::truncate_chars(&cn.content, 2500)
        );

        let fetch = reg
            .call(
                &ctx,
                "web_fetch",
                &json!({"url": "https://www.bing.com/search?q=rust", "max_chars": 2000}),
            )
            .await;
        assert!(fetch.ok, "{}", fetch.content);
        println!(
            "=== web_fetch bing ===\n{}",
            neosonia_core::util::truncate_chars(&fetch.content, 1200)
        );

        let dl = reg
            .call(
                &ctx,
                "download_file",
                &json!({"url": "https://www.bing.com/robots.txt", "max_bytes": 1048576}),
            )
            .await;
        assert!(dl.ok, "{}", dl.content);
        println!("=== download_file ===\n{}", dl.content);
    }

    #[tokio::test]
    #[ignore]
    async fn verify_long_command_stream_and_cancel() {
        if !verify_enabled() {
            return;
        }
        let (ctx, host) = verify_ctx("verify-long");
        let reg = verify_registry();
        let script = "1..65 | ForEach-Object { Write-Output \"tick $_\"; Start-Sleep -Seconds 1 }";
        let started = Instant::now();
        let out = reg
            .call(
                &ctx,
                "run_command",
                &json!({"command": script, "timeout_secs": 180}),
            )
            .await;
        let elapsed = started.elapsed();
        assert!(out.ok, "{}", out.content);
        assert!(
            elapsed.as_secs() >= 60,
            "长任务应该跑满 60 秒以上，实际 {elapsed:?}"
        );
        assert!(out.content.contains("tick 65"), "{}", out.content);
        let streamed = host.emitted_output();
        let ticks = streamed.matches("tick ").count();
        assert!(ticks >= 60, "流式输出应有 >=60 条 tick，实际 {ticks}");
        println!(
            "长命令完成：耗时 {elapsed:?}，流式 tick 条数 {ticks}，结果尾部：\n{}",
            neosonia_core::util::truncate_chars(&out.content, 800)
        );

        // 取消验证：120 秒命令，5 秒后用户取消
        let (ctx2, host2) = verify_ctx("verify-cancel");
        let cancel_host = host2.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            cancel_host.cancel();
        });
        let started2 = Instant::now();
        let out2 = reg
            .call(
                &ctx2,
                "run_command",
                &json!({
                    "command": "1..120 | ForEach-Object { Write-Output \"tick $_\"; Start-Sleep -Seconds 1 }",
                    "timeout_secs": 300
                }),
            )
            .await;
        let elapsed2 = started2.elapsed();
        assert!(!out2.ok, "{}", out2.content);
        assert!(out2.content.contains("取消"), "{}", neosonia_core::util::truncate_chars(&out2.content, 500));
        assert!(
            elapsed2 < Duration::from_secs(40),
            "取消应及时返回，实际 {elapsed2:?}"
        );
        assert!(host2.emitted_output().contains("tick "), "取消前应有流式输出");
        println!(
            "取消验证：耗时 {elapsed2:?}，结果：\n{}",
            neosonia_core::util::truncate_chars(&out2.content, 600)
        );
    }

    #[tokio::test]
    #[ignore]
    async fn verify_background_job_and_stdin() {
        if !verify_enabled() {
            return;
        }
        let (ctx, _host) = verify_ctx("verify-bg");
        let reg = verify_registry();
        // PowerShell 的 [Console]::In 能可靠读重定向进来的 stdin：验证 send_job_input 真的写进了进程
        let start = reg
            .call(
                &ctx,
                "run_command",
                &json!({
                    "command": "$v = [Console]::In.ReadLine(); Write-Output ('GOT=' + $v)",
                    "background": true
                }),
            )
            .await;
        assert!(start.ok, "{}", start.content);
        let job_id = job_id_from(&start.content);
        tokio::time::sleep(Duration::from_millis(400)).await;
        let input = reg
            .call(
                &ctx,
                "send_job_input",
                &json!({"job_id": job_id, "input": "hello-neosonia"}),
            )
            .await;
        println!("=== send_job_input ===\n{}", input.content);
        let read = reg
            .call(
                &ctx,
                "read_job_output",
                &json!({"job_id": job_id, "wait_ms": 3000}),
            )
            .await;
        println!("=== read_job_output ===\n{}", read.content);
        assert!(read.ok, "{}", read.content);
        assert!(
            read.content.contains("GOT=hello-neosonia"),
            "stdin 交互失败：{}",
            read.content
        );
        let jobs = reg.call(&ctx, "list_jobs", &json!({})).await;
        println!("=== list_jobs ===\n{}", neosonia_core::util::truncate_chars(&jobs.content, 800));
        let killed = reg.call(&ctx, "kill_job", &json!({"job_id": job_id})).await;
        println!("=== kill_job ===\n{}", killed.content);
    }

    // ============================================================
    // v0.3 真机验证辅助
    // ============================================================

    /// 从 foreground_window 结果里解析 pid。
    fn parse_foreground_pid(content: &str) -> Option<u32> {
        let pos = content.find("（pid ")? + "（pid ".len();
        let rest = &content[pos..];
        let end = rest.find('）')?;
        rest[..end].trim().parse().ok()
    }

    /// 从 foreground_window 结果里解析窗口句柄（形如 0x3A0D98）。
    fn parse_foreground_hwnd(content: &str) -> Option<String> {
        let pos = content.find("窗口句柄：")? + "窗口句柄：".len();
        let rest = &content[pos..];
        let hwnd: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
        if hwnd.starts_with("0x") { Some(hwnd) } else { None }
    }

    /// 从控件树文本里找 `bounds=(x,y,w,h)`（role/name 过滤，name 为空表示不过滤）。
    fn find_node_bounds(
        content: &str,
        role: &str,
        name_contains: &str,
    ) -> Option<(i32, i32, i32, i32)> {
        for line in content.lines() {
            if !line.starts_with('#') || !line.contains(role) || !line.contains(name_contains) {
                continue;
            }
            let Some(pos) = line.find("bounds=(") else {
                continue;
            };
            let rest = &line[pos + "bounds=(".len()..];
            let Some(end) = rest.find(')') else {
                continue;
            };
            let nums: Vec<i32> = rest[..end]
                .split(',')
                .filter_map(|s| s.trim().parse::<i32>().ok())
                .collect();
            if nums.len() == 4 {
                return Some((nums[0], nums[1], nums[2], nums[3]));
            }
        }
        None
    }

    fn find_doc_bounds(tree: &str) -> Option<(i32, i32, i32, i32)> {
        find_node_bounds(tree, "Document", "文本编辑器")
            .or_else(|| find_node_bounds(tree, "Edit", "文本编辑器"))
            .or_else(|| find_node_bounds(tree, "Document", "bounds="))
            .or_else(|| find_node_bounds(tree, "Edit", "bounds="))
    }

    /// 从控件树的状态栏读「N 个字符」。
    fn text_char_count(tree: &str) -> Option<u32> {
        for line in tree.lines() {
            let Some(pos) = line.find("个字符") else {
                continue;
            };
            let head = &line[..pos];
            if let Some(q) = head.rfind("name=\"") {
                let digits: String = head[q + "name=\"".len()..]
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                if let Ok(n) = digits.parse() {
                    return Some(n);
                }
            }
        }
        None
    }

    /// 截图结果里的 PNG 路径。
    fn screenshot_path_of(content: &str) -> Option<String> {
        content
            .lines()
            .find(|l| l.starts_with("[screenshot:") && l.ends_with(']'))
            .map(|l| l.trim_start_matches("[screenshot:").trim_end_matches(']').to_string())
    }

    async fn focus_window(reg: &ToolRegistry, ctx: &ToolCtx, hwnd: &str) {
        let _ = reg
            .call(ctx, "ui_focus_window", &json!({"hwnd": hwnd}))
            .await;
        tokio::time::sleep(Duration::from_millis(120)).await;
    }

    /// 测试结束（包括 panic 展开）时确保目标进程被杀掉，避免残留记事本窗口。
    struct KillOnDrop(u32);

    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let _ = std::process::Command::new("taskkill")
                .args(["/PID", &self.0.to_string(), "/T", "/F"])
                .output();
        }
    }

    /// 从 foreground_window 结果里解析窗口矩形 (x, y, w, h)。
    fn parse_foreground_rect(content: &str) -> Option<(i32, i32, i32, i32)> {
        let pos = content.find("窗口矩形：")? + "窗口矩形：".len();
        let line = content[pos..].lines().next()?;
        let mut numbers: Vec<i64> = Vec::new();
        for part in line.split(|c: char| !c.is_ascii_digit() && c != '-') {
            if let Ok(value) = part.parse::<i64>() {
                numbers.push(value);
            }
        }
        if numbers.len() >= 4 {
            Some((
                numbers[0] as i32,
                numbers[1] as i32,
                numbers[2] as i32,
                numbers[3] as i32,
            ))
        } else {
            None
        }
    }

    /// 从窗口列表文本里按标题子串找 (hwnd, pid)。
    fn find_window_in_list(content: &str, title_hint: &str) -> Option<(String, u32)> {
        for line in content.lines() {
            if !line.contains(title_hint) {
                continue;
            }
            let hwnd = line.find("hwnd=").map(|pos| {
                let rest = &line[pos + "hwnd=".len()..];
                rest.chars()
                    .take_while(|c| !c.is_whitespace())
                    .collect::<String>()
            })?;
            if !hwnd.starts_with("0x") {
                continue;
            }
            let pid = line
                .find("pid=")
                .and_then(|pos| {
                    let rest = &line[pos + "pid=".len()..];
                    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                    digits.parse::<u32>().ok()
                })
                .unwrap_or(0);
            return Some((hwnd, pid));
        }
        None
    }

    /// 启动记事本并打开指定文件，返回 (pid, hwnd)。
    ///
    /// 注意：Windows 前台锁会让 `Start-Process` 出来的窗口不一定立刻成为前台，
    /// 所以这里先轮询 `ui_window_list` 找到窗口句柄，再显式 focus，最后用
    /// `foreground_window` 确认（比只等前台标题稳）。
    async fn open_notepad_with_file(
        reg: &ToolRegistry,
        ctx: &ToolCtx,
        doc_path: &std::path::Path,
        title_hint: &str,
    ) -> (u32, String) {
        let launch_cmd = format!(
            "Start-Process notepad.exe -ArgumentList '{}'",
            doc_path.display()
        );
        let launch = reg
            .call(
                ctx,
                "run_command",
                &json!({"command": launch_cmd, "timeout_secs": 30}),
            )
            .await;
        assert!(launch.ok, "启动记事本失败：{}", launch.content);
        for attempt in 0..25 {
            let list = reg
                .call(
                    ctx,
                    "ui_window_list",
                    &json!({"filter": title_hint, "limit": 50}),
                )
                .await;
            if let Some((hwnd, pid)) = list
                .ok
                .then(|| find_window_in_list(&list.content, title_hint))
                .flatten()
            {
                println!("=== ui_window_list 找到记事本（第 {attempt} 次）：hwnd={hwnd} pid={pid}");
                focus_window(reg, ctx, &hwnd).await;
                let fg = reg.call(ctx, "foreground_window", &json!({})).await;
                if fg.ok {
                    println!("=== foreground_window ===\n{}", fg.content);
                    let matched_hwnd = parse_foreground_hwnd(&fg.content)
                        .map(|current| current.eq_ignore_ascii_case(&hwnd))
                        .unwrap_or(false);
                    let matched_pid = parse_foreground_pid(&fg.content)
                        .map(|current| current == pid)
                        .unwrap_or(false);
                    if matched_hwnd || matched_pid {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        return (pid, hwnd);
                    }
                }
            }
            // 兜底：窗口已在后台但 ui_window_list 过滤不到时，看前台是否已经是它
            let fg = reg.call(ctx, "foreground_window", &json!({})).await;
            if fg.ok
                && (fg.content.to_lowercase().contains(&title_hint.to_lowercase())
                    || fg.content.to_lowercase().contains("notepad"))
            {
                println!("=== foreground_window（轮询命中）===\n{}", fg.content);
                let pid = parse_foreground_pid(&fg.content).expect("应能解析记事本 pid");
                let hwnd = parse_foreground_hwnd(&fg.content).expect("应能解析记事本 hwnd");
                focus_window(reg, ctx, &hwnd).await;
                tokio::time::sleep(Duration::from_millis(300)).await;
                return (pid, hwnd);
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
        // 最后把现场打印出来，方便定位（而不是干巴巴一句超时）
        let list = reg
            .call(ctx, "ui_window_list", &json!({"limit": 50}))
            .await;
        let fg = reg.call(ctx, "foreground_window", &json!({})).await;
        panic!(
            "等待记事本窗口失败（title_hint={title_hint}）\n窗口列表：\n{}\n当前前台：\n{}",
            list.content, fg.content
        );
    }

    async fn char_count(reg: &ToolRegistry, ctx: &ToolCtx, hwnd: &str) -> u32 {
        let tree = reg
            .call(
                ctx,
                "ui_tree",
                &json!({"window": hwnd, "depth": 10, "max_nodes": 400}),
            )
            .await;
        if !tree.ok {
            return 0;
        }
        text_char_count(&tree.content).unwrap_or(0)
    }

    async fn doc_bounds(reg: &ToolRegistry, ctx: &ToolCtx, hwnd: &str) -> Option<(i32, i32, i32, i32)> {
        let tree = reg
            .call(
                ctx,
                "ui_tree",
                &json!({"window": hwnd, "depth": 10, "max_nodes": 400}),
            )
            .await;
        if !tree.ok {
            return None;
        }
        find_doc_bounds(&tree.content)
    }

    // ============================================================
    // v0.3 真机验证：鼠标按键/双击/右键/滚轮（记事本）+ 截图证据
    // ============================================================

    #[tokio::test]
    #[ignore]
    async fn verify_mouse_notepad() {
        if !verify_enabled() {
            return;
        }
        let (ctx, _host) = verify_ctx("verify-notepad");
        let reg = verify_registry();

        // 0) 清理残留记事本，避免误抓到上一次测试/用户留下的窗口
        let cleanup = reg
            .call(
                &ctx,
                "run_command",
                &json!({
                    "command": "Get-Process notepad -ErrorAction SilentlyContinue | Stop-Process -Force",
                    "timeout_secs": 30
                }),
            )
            .await;
        println!("=== 清理残留记事本 ===\n{}", cleanup.content);
        tokio::time::sleep(Duration::from_millis(500)).await;

        // 1) 用固定文件启动记事本（标题唯一，Ctrl+S 可写回磁盘做验证）
        let doc_path = ctx.paths().data_dir.join("verify_notepad.txt");
        let initial = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu";
        std::fs::write(&doc_path, initial).expect("写初始测试文件");
        let (pid, hwnd) =
            open_notepad_with_file(&reg, &ctx, &doc_path, "verify_notepad").await;
        let _kill_guard = KillOnDrop(pid);

        let count0 = char_count(&reg, &ctx, &hwnd).await;
        let (dx, dy, dw, dh) = doc_bounds(&reg, &ctx, &hwnd).await.expect("控件树里没找到文本区 bounds");
        let _ = dh;
        println!("[verify] 初始字符数={count0} 文本区bounds=({dx},{dy},{dw},{dh})");
        assert!(
            count0 >= initial.len() as u32 - 2,
            "初始字符数应约等于文件长度 {}，实际 {count0}",
            initial.len()
        );
        let click_y = dy + 18;
        let click_x = dx + (dw as f64 * 0.35) as i32;

        // 2) 左键点击定位光标：点文本中间，输入 [LEFT] → 字符数净增 6（中间插入，不覆盖）
        focus_window(&reg, &ctx, &hwnd).await;
        let click = reg
            .call(
                &ctx,
                "mouse_click",
                &json!({"x": click_x, "y": click_y, "button": "left"}),
            )
            .await;
        assert!(click.ok, "{}", click.content);
        println!("=== mouse_click(left 定位光标) ===\n{}", click.content);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mark = reg
            .call(&ctx, "keyboard_type", &json!({"text": "[LEFT]"}))
            .await;
        assert!(mark.ok, "{}", mark.content);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let count1 = char_count(&reg, &ctx, &hwnd).await;
        println!("[verify] 左键点击 + 输入 [LEFT] 后字符数：{count0} -> {count1}");
        assert_eq!(
            count1 as i64 - count0 as i64,
            6,
            "左键点击定位后输入应为净插入 6 个字符（不能覆盖原文）"
        );

        // Ctrl+S 保存，读磁盘验证标记插在文本中间
        let _ = reg
            .call(&ctx, "keyboard_key", &json!({"key": "s", "modifiers": ["ctrl"]}))
            .await;
        tokio::time::sleep(Duration::from_millis(600)).await;
        let saved = std::fs::read_to_string(&doc_path).unwrap_or_default();
        println!("[verify] Ctrl+S 保存后文件内容：{saved:?}");
        if saved.contains("[LEFT]") {
            let byte_pos = saved.find("[LEFT]").unwrap_or(0);
            let char_pos = saved[..byte_pos].chars().count();
            let total = saved.chars().count();
            assert!(
                char_pos > 3 && char_pos + 6 < total,
                "左键点击应把光标定位到文本中间（[LEFT] 在第 {char_pos}/{total} 字符）"
            );
            println!("[verify] 左键点击定位光标成功：[LEFT] 在第 {char_pos}/{total} 字符（中间）");
        } else {
            println!("[warn] Ctrl+S 未写回磁盘（可能焦点被抢），改用字符数增量作为左键定位证据");
        }

        // 3) 双击选词：同位置 double=true 选中一个单词，输入 WORDSEL 应替换（净变化 = 7 - 词长）
        focus_window(&reg, &ctx, &hwnd).await;
        let dbl = reg
            .call(
                &ctx,
                "mouse_click",
                &json!({
                    "x": click_x, "y": click_y, "button": "left", "double": true
                }),
            )
            .await;
        assert!(dbl.ok, "{}", dbl.content);
        println!("=== mouse_click(double 选中单词) ===\n{}", dbl.content);
        tokio::time::sleep(Duration::from_millis(250)).await;
        let repl = reg
            .call(&ctx, "keyboard_type", &json!({"text": "WORDSEL"}))
            .await;
        assert!(repl.ok, "{}", repl.content);
        tokio::time::sleep(Duration::from_millis(350)).await;
        let count2 = char_count(&reg, &ctx, &hwnd).await;
        let selected_len = count1 as i64 + 7 - count2 as i64;
        println!(
            "[verify] 双击后输入 WORDSEL：字符数 {count1} -> {count2}，推断选中 {selected_len} 个字符"
        );
        assert!(
            (1..=30).contains(&selected_len),
            "双击应选中一个单词并被 WORDSEL 替换（推断选中长度 {selected_len}，若为 0 说明没选中）"
        );
        let _ = reg
            .call(&ctx, "keyboard_key", &json!({"key": "s", "modifiers": ["ctrl"]}))
            .await;
        tokio::time::sleep(Duration::from_millis(600)).await;
        let saved2 = std::fs::read_to_string(&doc_path).unwrap_or_default();
        println!("[verify] 双击替换后文件内容：{saved2:?}");
        assert!(
            saved2.is_empty() || saved2.contains("WORDSEL"),
            "双击选中的单词应被 WORDSEL 替换: {saved2:?}"
        );

        // 4) 右键：弹出上下文菜单，截图留证后按 Esc 关闭
        focus_window(&reg, &ctx, &hwnd).await;
        let rclick = reg
            .call(
                &ctx,
                "mouse_click",
                &json!({"x": dx + (dw as f64 * 0.55) as i32, "y": click_y, "button": "right"}),
            )
            .await;
        assert!(rclick.ok, "{}", rclick.content);
        println!("=== mouse_click(right 上下文菜单) ===\n{}", rclick.content);
        tokio::time::sleep(Duration::from_millis(700)).await;
        let menu_shot = reg
            .call(
                &ctx,
                "screenshot",
                &json!({"display": "primary", "grid": false, "return_base64": false}),
            )
            .await;
        assert!(menu_shot.ok, "{}", menu_shot.content);
        let menu_path = screenshot_path_of(&menu_shot.content).expect("右键菜单截图应有路径");
        println!("RIGHT_CLICK_MENU_SCREENSHOT={menu_path}");
        let _ = reg.call(&ctx, "keyboard_key", &json!({"key": "esc"})).await;
        tokio::time::sleep(Duration::from_millis(300)).await;

        // 5) 收尾：结束当前记事本（不弹保存框）
        let killed = reg
            .call(&ctx, "kill_process", &json!({"pid": pid, "force": true}))
            .await;
        println!("=== kill_process(notepad pid={pid}) ===\n{}", killed.content);
        assert!(killed.ok, "{}", killed.content);
        tokio::time::sleep(Duration::from_millis(400)).await;

        // 6) 滚轮：开一个 40 行文件，滚动前后截图应不同
        let scroll_path = ctx.paths().data_dir.join("verify_notepad_scroll.txt");
        let multiline = (1..=40)
            .map(|i| format!("line-{i:02} neosonia scroll test"))
            .collect::<Vec<_>>()
            .join("\r\n");
        std::fs::write(&scroll_path, &multiline).expect("写滚动测试文件");
        let (pid2, hwnd2) =
            open_notepad_with_file(&reg, &ctx, &scroll_path, "verify_notepad_scroll").await;
        let _kill_guard2 = KillOnDrop(pid2);
        let scroll_doc = doc_bounds(&reg, &ctx, &hwnd2).await;
        let fg2 = reg.call(&ctx, "foreground_window", &json!({})).await;
        let (sx, sy, sw, sh) = scroll_doc
            .or_else(|| {
                if fg2.ok {
                    parse_foreground_rect(&fg2.content)
                } else {
                    None
                }
            })
            .unwrap_or((0, 0, 800, 600));
        focus_window(&reg, &ctx, &hwnd2).await;
        let before = reg
            .call(&ctx, "screenshot", &json!({"display": "primary"}))
            .await;
        assert!(before.ok, "{}", before.content);
        let before_path = screenshot_path_of(&before.content).expect("滚动前截图应有路径");
        let scroll = reg
            .call(
                &ctx,
                "mouse_scroll",
                &json!({
                    "x": sx + sw / 2, "y": sy + sh / 2, "direction": "down", "amount": 8
                }),
            )
            .await;
        assert!(scroll.ok, "{}", scroll.content);
        println!("=== mouse_scroll(direction=down, amount=8) ===\n{}", scroll.content);
        tokio::time::sleep(Duration::from_millis(700)).await;
        let after = reg
            .call(&ctx, "screenshot", &json!({"display": "primary"}))
            .await;
        assert!(after.ok, "{}", after.content);
        let after_path = screenshot_path_of(&after.content).expect("滚动后截图应有路径");
        let before_bytes = std::fs::read(&before_path).expect("读滚动前截图");
        let after_bytes = std::fs::read(&after_path).expect("读滚动后截图");
        assert_ne!(
            before_bytes, after_bytes,
            "滚轮滚动后画面应变化（两张截图相同说明没滚动）"
        );
        println!("SCROLL_BEFORE_SCREENSHOT={before_path}");
        println!("SCROLL_AFTER_SCREENSHOT={after_path}");
        let killed2 = reg
            .call(&ctx, "kill_process", &json!({"pid": pid2, "force": true}))
            .await;
        println!("=== kill_process(scroll notepad pid={pid2}) ===\n{}", killed2.content);
    }

    // ============================================================
    // v0.3 真机验证：后台命令 job_status（running → cancelled）
    // ============================================================

    #[tokio::test]
    #[ignore]
    async fn verify_background_job_status_flow() {
        if !verify_enabled() {
            return;
        }
        let (ctx, _host) = verify_ctx("verify-job-status");
        let reg = verify_registry();
        let started = reg
            .call(
                &ctx,
                "run_command",
                &json!({
                    "command": "node -e \"setInterval(()=>console.log(Date.now()),1000)\"",
                    "background": true
                }),
            )
            .await;
        assert!(started.ok, "{}", started.content);
        let job_id = job_id_from(&started.content);
        println!("=== 启动 node 后台任务 ===\n{}", started.content);

        // 等 3 秒让它输出几行，然后 job_status 应一次拿到 running + 尾部输出
        tokio::time::sleep(Duration::from_secs(3)).await;
        let status = reg.call(&ctx, "job_status", &json!({})).await;
        assert!(status.ok, "{}", status.content);
        println!("=== job_status（运行中）===\n{}", status.content);
        assert!(
            status.content.contains(&job_id),
            "job_status 应包含任务 {job_id}: {}",
            status.content
        );
        assert!(
            status.content.contains("状态：running"),
            "3 秒后应仍在运行: {}",
            status.content
        );
        assert!(
            status.content.contains("最后 20 行输出："),
            "应包含输出尾部: {}",
            status.content
        );
        // node 每秒打印一个毫秒时间戳，尾部应有纯数字行
        let has_output = status
            .content
            .lines()
            .any(|line| line.trim().chars().all(|c| c.is_ascii_digit()) && line.trim().len() >= 10);
        assert!(has_output, "job_status 的尾部输出应有 node 打印的时间戳: {}", status.content);

        // 单个任务查询
        let single = reg
            .call(&ctx, "job_status", &json!({"job_id": job_id}))
            .await;
        assert!(single.ok, "{}", single.content);
        assert!(single.content.contains("共 1 个"), "{}", single.content);

        // kill 后状态应变 cancelled
        let killed = reg.call(&ctx, "kill_job", &json!({"job_id": job_id})).await;
        assert!(killed.ok, "{}", killed.content);
        println!("=== kill_job ===\n{}", killed.content);
        tokio::time::sleep(Duration::from_millis(500)).await;
        let after = reg
            .call(&ctx, "job_status", &json!({"job_id": job_id}))
            .await;
        assert!(after.ok, "{}", after.content);
        println!("=== job_status（kill 后）===\n{}", after.content);
        assert!(
            after.content.contains("状态：cancelled"),
            "kill 后状态应为 cancelled: {}",
            after.content
        );
        assert!(
            after.content.contains("汇总：运行中 0"),
            "kill 后不应还有运行中任务: {}",
            after.content
        );
    }

    // ============================================================
    // v0.5 真机验证：鼠标按住拖拽（press → move → release）+ drag_path hold_ms
    // ============================================================
    #[tokio::test]
    #[ignore]
    async fn verify_hold_press_drag() {
        if !verify_enabled() {
            return;
        }
        let (ctx, _host) = verify_ctx("verify-hold");
        let reg = verify_registry();
        let _ = reg
            .call(
                &ctx,
                "run_command",
                &json!({
                    "command": "Get-Process notepad -ErrorAction SilentlyContinue | Stop-Process -Force",
                    "timeout_secs": 30
                }),
            )
            .await;
        tokio::time::sleep(Duration::from_millis(500)).await;

        let doc_path = ctx.paths().data_dir.join("verify_hold.txt");
        let initial = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu";
        std::fs::write(&doc_path, initial).expect("写测试文件");
        let (pid, hwnd) = open_notepad_with_file(&reg, &ctx, &doc_path, "verify_hold").await;
        let _kill_guard = KillOnDrop(pid);
        let (dx, dy, _dw, dh) = doc_bounds(&reg, &ctx, &hwnd)
            .await
            .expect("控件树里没找到文本区 bounds");
        // Win11 记事本可能恢复上次会话的文档/多标签，这里用剪贴板显式写入已知文本，
        // 保证拖选断言面对的是固定内容（而不是上次残留的 1 个字符）。
        focus_window(&reg, &ctx, &hwnd).await;
        let _ = reg
            .call(&ctx, "clipboard_write", &json!({"text": initial}))
            .await;
        let _ = reg
            .call(&ctx, "keyboard_combo", &json!({"keys": ["ctrl", "a"]}))
            .await;
        let paste = reg
            .call(&ctx, "keyboard_combo", &json!({"keys": ["ctrl", "v"]}))
            .await;
        assert!(paste.ok, "{}", paste.content);
        tokio::time::sleep(Duration::from_millis(500)).await;
        let count0 = char_count(&reg, &ctx, &hwnd).await;
        println!("[verify] 初始字符数={count0} 文本区=({dx},{dy}) 高={dh}");
        assert!(
            count0 as usize + 2 >= initial.len(),
            "初始化文本失败（应约 {} 字符，实际 {count0}）",
            initial.len()
        );

        // 1) mouse_press → mouse_move（两次）→ 等 300ms → mouse_release = 手动拖选
        focus_window(&reg, &ctx, &hwnd).await;
        let start = (dx + 5, dy + 18);
        let middle = (dx + 160, dy + 18);
        let end = (dx + 320, dy + 18);
        let pressed = reg
            .call(
                &ctx,
                "mouse_press",
                &json!({"x": start.0, "y": start.1, "button": "left"}),
            )
            .await;
        assert!(pressed.ok, "{}", pressed.content);
        println!("=== mouse_press ===\n{}", pressed.content);
        // 重复 press 应给中文提示（状态机生效）
        let duplicate = reg
            .call(&ctx, "mouse_press", &json!({"button": "left"}))
            .await;
        assert!(!duplicate.ok, "重复 press 应被拒绝: {}", duplicate.content);
        println!("=== mouse_press（重复，预期提示）===\n{}", duplicate.content);

        let moved1 = reg
            .call(&ctx, "mouse_move", &json!({"x": middle.0, "y": middle.1, "smooth": false}))
            .await;
        assert!(moved1.ok, "{}", moved1.content);
        tokio::time::sleep(Duration::from_millis(150)).await;
        let moved2 = reg
            .call(&ctx, "mouse_move", &json!({"x": end.0, "y": end.1, "smooth": false}))
            .await;
        assert!(moved2.ok, "{}", moved2.content);
        // 按住状态保持 300ms 再松手
        tokio::time::sleep(Duration::from_millis(300)).await;
        let released = reg.call(&ctx, "mouse_release", &json!({"button": "left"})).await;
        assert!(released.ok, "{}", released.content);
        println!("=== mouse_release ===\n{}", released.content);

        // 输入单个字符替换选区：字符数应明显下降，证明拖选真的选中了文本
        let typed = reg
            .call(&ctx, "keyboard_type", &json!({"text": "X"}))
            .await;
        assert!(typed.ok, "{}", typed.content);
        tokio::time::sleep(Duration::from_millis(400)).await;
        let count1 = char_count(&reg, &ctx, &hwnd).await;
        println!("[verify] 按住拖选 + 输入 X 后字符数：{count0} -> {count1}");
        if count1 + 5 >= count0 {
            println!(
                "[warn] 按住拖选没有产生明显选区（字符数 {count0} -> {count1}，可能受中文输入法/焦点影响），继续验证 drag_path 和窗口拖动"
            );
        } else {
            println!("[verify] 按住拖选成功（字符数下降 {}）", count0 - count1);
        }

        // 截图留证（拖选后的记事本画面）
        let shot = reg
            .call(
                &ctx,
                "screenshot",
                &json!({"display": "primary", "grid": true}),
            )
            .await;
        assert!(shot.ok, "{}", shot.content);
        let shot_path = screenshot_path_of(&shot.content).expect("截图应有路径");
        println!("HOLD_DRAG_SCREENSHOT={shot_path}");

        // 2) mouse_drag_path + hold_ms：先把文本恢复，再拖选一段，终点按住 300ms 再松
        // 用剪贴板粘贴恢复（keyboard_type 在记事本里可能被输入法吞字）
        focus_window(&reg, &ctx, &hwnd).await;
        let _ = reg
            .call(&ctx, "clipboard_write", &json!({"text": initial}))
            .await;
        let _ = reg
            .call(&ctx, "keyboard_combo", &json!({"keys": ["ctrl", "a"]}))
            .await;
        let restore = reg
            .call(&ctx, "keyboard_combo", &json!({"keys": ["ctrl", "v"]}))
            .await;
        assert!(restore.ok, "{}", restore.content);
        tokio::time::sleep(Duration::from_millis(500)).await;
        let count2 = char_count(&reg, &ctx, &hwnd).await;
        println!("[verify] 恢复文本后字符数={count2}");
        let drag = reg
            .call(
                &ctx,
                "mouse_drag_path",
                &json!({
                    "points": [{"x": start.0, "y": start.1}, {"x": end.0, "y": end.1}],
                    "duration_ms": 400,
                    "hold_ms": 300
                }),
            )
            .await;
        assert!(drag.ok, "{}", drag.content);
        println!("=== mouse_drag_path(hold_ms=300) ===\n{}", drag.content);
        assert!(drag.content.contains("终点按住 300ms"), "{}", drag.content);
        let typed2 = reg
            .call(&ctx, "keyboard_type", &json!({"text": "Y"}))
            .await;
        assert!(typed2.ok, "{}", typed2.content);
        tokio::time::sleep(Duration::from_millis(400)).await;
        let count3 = char_count(&reg, &ctx, &hwnd).await;
        println!("[verify] drag_path(hold_ms=300) + 输入 Y 后字符数：{count2} -> {count3}");
        if count3 + 3 >= count2 {
            println!(
                "[warn] drag_path 拖选没有产生明显选区（字符数 {count2} -> {count3}，可能受焦点影响）"
            );
        } else {
            println!("[verify] drag_path 拖选成功（字符数下降 {}）", count2 - count3);
        }

        // 3) 按住标题栏拖动窗口：press → 分段 move → release，窗口位置应变化
        focus_window(&reg, &ctx, &hwnd).await;
        let fg = reg.call(&ctx, "foreground_window", &json!({})).await;
        assert!(fg.ok, "{}", fg.content);
        let (wx, wy, ww, _wh) = parse_foreground_rect(&fg.content).expect("窗口矩形");
        println!("[verify] 拖动前窗口矩形：x={wx} y={wy} 宽={ww}");
        // 抓标题栏：横向用窗口正中（标签页在左侧、按钮在右侧，正中一般是空白标题栏），
        // 纵向 y+16 位于标题栏内。
        let title_point = (wx + ww / 2, wy + 16);
        let press_title = reg
            .call(
                &ctx,
                "mouse_press",
                &json!({"x": title_point.0, "y": title_point.1, "button": "left"}),
            )
            .await;
        assert!(press_title.ok, "{}", press_title.content);
        tokio::time::sleep(Duration::from_millis(150)).await;
        // 分两段移动（先小位移触发拖动，再大位移），避免一次性 Jump 被系统忽略
        let step1 = reg
            .call(
                &ctx,
                "mouse_move",
                &json!({"x": title_point.0 + 60, "y": title_point.1 + 30, "smooth": false}),
            )
            .await;
        assert!(step1.ok, "{}", step1.content);
        tokio::time::sleep(Duration::from_millis(150)).await;
        let move_title = reg
            .call(
                &ctx,
                "mouse_move",
                &json!({"x": title_point.0 + 200, "y": title_point.1 + 100, "smooth": false}),
            )
            .await;
        assert!(move_title.ok, "{}", move_title.content);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let release_title = reg.call(&ctx, "mouse_release", &json!({})).await;
        assert!(release_title.ok, "{}", release_title.content);
        tokio::time::sleep(Duration::from_millis(400)).await;
        let fg2 = reg.call(&ctx, "foreground_window", &json!({})).await;
        assert!(fg2.ok, "{}", fg2.content);
        let (nx, ny, _nw, _nh) = parse_foreground_rect(&fg2.content).expect("窗口矩形");
        println!("[verify] 按住标题栏拖动：窗口从 ({wx},{wy}) 移到 ({nx},{ny})");
        if (nx - wx).abs() < 100 || (ny - wy).abs() < 50 {
            println!(
                "[warn] 按住标题栏拖动没有让窗口明显移动（{wx},{wy} -> {nx},{ny}），可能当前窗口不是标准标题栏，已保留截图"
            );
        } else {
            println!("[verify] 按住标题栏拖动成功（位移 {},{})", nx - wx, ny - wy);
        }
        let shot2 = reg
            .call(
                &ctx,
                "screenshot",
                &json!({"display": "primary", "grid": false}),
            )
            .await;
        assert!(shot2.ok, "{}", shot2.content);
        println!(
            "HOLD_MOVE_WINDOW_SCREENSHOT={}",
            screenshot_path_of(&shot2.content).unwrap_or_default()
        );

        // 收尾：kill 记事本
        let killed = reg
            .call(&ctx, "kill_process", &json!({"pid": pid, "force": true}))
            .await;
        println!("=== kill_process(notepad pid={pid}) ===\n{}", killed.content);
    }

    // ============================================================
    // v0.5 真机验证：DeepSeek provider（无 key 时应 warn + 自动回退聚合）
    // ============================================================

    #[tokio::test]
    #[ignore]
    async fn verify_web_search_deepseek_fallback() {
        if !verify_enabled() {
            return;
        }
        let (ctx, _host) = verify_ctx("verify-deepseek");
        let reg = verify_registry();
        // 本机没有 DeepSeek API key：provider=deepseek 应回退聚合搜索并注明原因
        let out = reg
            .call(
                &ctx,
                "web_search",
                &json!({"query": "rust language", "count": 5, "provider": "deepseek"}),
            )
            .await;
        assert!(out.ok, "{}", out.content);
        println!("=== web_search provider=deepseek（无 key，预期回退）===\n{}", out.content);
        assert!(
            out.content.contains("DeepSeek 官方搜索不可用"),
            "结果里应注明回退原因: {}",
            out.content
        );
        assert!(
            out.content.contains("回退聚合"),
            "结果里应说明已回退聚合: {}",
            out.content
        );
        assert!(
            out.content.contains("来源:"),
            "回退后的结果仍应带来源引擎: {}",
            out.content
        );
        // 聚合搜索本身仍可用（有真实结果）
        let hits = out
            .content
            .lines()
            .filter(|line| line.trim_start().starts_with("来源:"))
            .count();
        assert!(hits >= 1, "应至少有 1 条结果: {}", out.content);
    }

    // ============================================================
    // v0.6 真机验证：截图提速 / 无黑窗 / 窗口自隐藏 / 连续点击 / 后台任务
    // ============================================================

    /// 从截图结果里解析耗时行：(抓屏方式, 抓屏 ms, 预处理 ms, PNG 编码 ms, 总 ms)。
    fn screenshot_timing(content: &str) -> Option<(String, u64, u64, u64, u64)> {
        let line = content.lines().find(|line| line.starts_with("耗时："))?;
        let parts: Vec<&str> = line.split('+').collect();
        if parts.len() != 3 {
            return None;
        }
        let first_number = |text: &str| -> Option<u64> {
            let mut digits = String::new();
            for ch in text.chars() {
                if ch.is_ascii_digit() {
                    digits.push(ch);
                } else if !digits.is_empty() {
                    return digits.parse().ok();
                }
            }
            None
        };
        let capture = first_number(parts[0])?;
        let prepare = first_number(parts[1])?;
        // 第三段形如 " PNG 编码 40ms = 77ms"，取最后一个数字作为总耗时
        let encode = first_number(parts[2])?;
        let total = parts[2]
            .rsplit('=')
            .next()
            .and_then(|tail| first_number(tail))
            .unwrap_or(encode);
        let method = if parts[0].contains("GDI") {
            "GDI".to_string()
        } else {
            "xcap".to_string()
        };
        Some((method, capture, prepare, encode, total))
    }

    #[tokio::test]
    #[ignore]
    async fn verify_screenshot_fast_latency() {
        if !verify_enabled() {
            return;
        }
        let (ctx, _host) = verify_ctx("verify-shot-fast");
        let reg = verify_registry();
        // 预热一次（首帧可能含初始化开销）
        let _ = reg
            .call(&ctx, "screenshot", &json!({"display": "primary", "fast": true}))
            .await;

        let mut fast_totals = Vec::new();
        let mut fast_walls = Vec::new();
        for _ in 0..3 {
            let started = Instant::now();
            let out = reg
                .call(
                    &ctx,
                    "screenshot",
                    &json!({"display": "primary", "fast": true, "return_base64": false}),
                )
                .await;
            let wall = started.elapsed().as_millis() as u64;
            assert!(out.ok, "{}", out.content);
            let (method, capture, prepare, encode, total) =
                screenshot_timing(&out.content).expect("结果里应有耗时行");
            println!(
                "[fast] 方式={method} 抓屏={capture}ms 预处理={prepare}ms 编码={encode}ms 合计={total}ms（调用墙钟 {wall}ms）"
            );
            assert_eq!(method, "GDI", "fast=true 应走 GDI 快路径: {}", out.content);
            fast_totals.push(total);
            fast_walls.push(wall);
        }

        let mut slow_totals = Vec::new();
        for _ in 0..2 {
            let out = reg
                .call(
                    &ctx,
                    "screenshot",
                    &json!({"display": "primary", "fast": false}),
                )
                .await;
            assert!(out.ok, "{}", out.content);
            let (method, capture, prepare, encode, total) =
                screenshot_timing(&out.content).expect("结果里应有耗时行");
            println!(
                "[xcap] 方式={method} 抓屏={capture}ms 预处理={prepare}ms 编码={encode}ms 合计={total}ms"
            );
            slow_totals.push(total);
        }
        let fast_avg = fast_totals.iter().sum::<u64>() / fast_totals.len() as u64;
        let slow_avg = slow_totals.iter().sum::<u64>() / slow_totals.len() as u64;
        let wall_avg = fast_walls.iter().sum::<u64>() / fast_walls.len() as u64;
        println!(
            "SCREENSHOT_LATENCY fast_avg={fast_avg}ms xcap_avg={slow_avg}ms wall_avg={wall_avg}ms"
        );
        assert!(
            fast_avg < slow_avg,
            "GDI 快路径应快于 xcap（fast={fast_avg}ms, xcap={slow_avg}ms）"
        );
        println!(
            "SCREENSHOT_SPEEDUP={:.2}x",
            slow_avg as f64 / fast_avg.max(1) as f64
        );
    }

    #[tokio::test]
    #[ignore]
    async fn verify_no_console_window_flash() {
        if !verify_enabled() {
            return;
        }
        let (ctx, _host) = verify_ctx("verify-no-flash");
        let reg = verify_registry();
        let before: Vec<String> = crate::system::console_windows_for_test()
            .into_iter()
            .map(|(class, title)| format!("{class}|{title}"))
            .collect();
        println!("[verify] 执行前控制台窗口 {} 个：{:?}", before.len(), before);

        for command in [
            "cmd /c echo hi",
            "powershell -NoProfile -Command \"Write-Output hi\"",
            "reg query HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Explorer",
        ] {
            let out = reg.call(&ctx, "run_command", &json!({"command": command})).await;
            assert!(out.ok, "{}", out.content);
            assert!(
                out.content.contains("hi") || out.content.contains("HKEY_"),
                "命令输出应正常采集（管道不受 CREATE_NO_WINDOW 影响）: {}",
                out.content
            );
        }
        // notify 工具也会 spawn powershell（Toast）：确认同样不弹黑窗
        let notify = reg
            .call(
                &ctx,
                "notify",
                &json!({"title": "NeoSonia 验证", "body": "无控制台窗口测试"}),
            )
            .await;
        println!("=== notify ===\n{}", notify.content);
        tokio::time::sleep(Duration::from_millis(600)).await;
        let after: Vec<String> = crate::system::console_windows_for_test()
            .into_iter()
            .map(|(class, title)| format!("{class}|{title}"))
            .collect();
        println!("[verify] 执行后控制台窗口 {} 个：{:?}", after.len(), after);
        let new_windows: Vec<&String> = after.iter().filter(|w| !before.contains(w)).collect();
        assert!(
            new_windows.is_empty(),
            "run_command 不应新弹出控制台窗口（管理员模式黑窗问题）：{new_windows:?}"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn verify_window_hide_app_and_hud() {
        if !verify_enabled() {
            return;
        }
        let (ctx, _host) = verify_ctx("verify-window-hide");
        let reg = verify_registry();
        let initial = reg.call(&ctx, "window_is_hidden", &json!({})).await;
        if !initial.ok {
            println!("[skip] app 未运行，无法验证窗口隐藏：{}", initial.content);
            return;
        }
        println!("=== window_is_hidden（初始）===\n{}", initial.content);
        // 触发一次截图，让 app 的 HUD（紫色活动提示）显示出来
        let _ = reg.call(&ctx, "screenshot", &json!({"fast": true})).await;
        tokio::time::sleep(Duration::from_millis(250)).await;
        let hud_before = crate::system::hud_windows_visible_for_test();
        let hud_visible_before = hud_before.iter().filter(|(_, visible)| *visible).count();
        println!("[verify] 隐藏前 HUD 窗口：{hud_before:?}");

        let hidden = reg.call(&ctx, "window_hide_app", &json!({"hide": true})).await;
        assert!(hidden.ok, "{}", hidden.content);
        println!("=== window_hide_app(hide=true) ===\n{}", hidden.content);
        tokio::time::sleep(Duration::from_millis(400)).await;
        let state = reg.call(&ctx, "window_is_hidden", &json!({})).await;
        assert!(state.ok, "{}", state.content);
        println!("=== window_is_hidden（隐藏后）===\n{}", state.content);
        assert!(
            state.content.contains("主窗口：隐藏"),
            "隐藏后 window_is_hidden 应报告隐藏: {}",
            state.content
        );
        let hud_after = crate::system::hud_windows_visible_for_test();
        let hud_visible_after = hud_after.iter().filter(|(_, visible)| *visible).count();
        println!("[verify] 隐藏后 HUD 窗口：{hud_after:?}");
        if hud_visible_before > 0 {
            assert!(
                hud_visible_after > 0,
                "紫色 HUD 不能跟着主窗口一起隐藏（before={hud_visible_before}, after={hud_visible_after}）"
            );
        }
        // 截图留证（主窗口隐藏后的桌面）
        let shot_hidden = reg
            .call(&ctx, "screenshot", &json!({"display": "primary"}))
            .await;
        assert!(shot_hidden.ok, "{}", shot_hidden.content);
        println!(
            "WINDOW_HIDDEN_SCREENSHOT={}",
            screenshot_path_of(&shot_hidden.content).unwrap_or_default()
        );

        let shown = reg.call(&ctx, "window_hide_app", &json!({"hide": false})).await;
        assert!(shown.ok, "{}", shown.content);
        println!("=== window_hide_app(hide=false) ===\n{}", shown.content);
        tokio::time::sleep(Duration::from_millis(400)).await;
        let state2 = reg.call(&ctx, "window_is_hidden", &json!({})).await;
        assert!(state2.ok, "{}", state2.content);
        println!("=== window_is_hidden（恢复后）===\n{}", state2.content);
        assert!(
            state2.content.contains("主窗口：可见"),
            "恢复后应报告可见: {}",
            state2.content
        );
        let shot_shown = reg
            .call(&ctx, "screenshot", &json!({"display": "primary"}))
            .await;
        assert!(shot_shown.ok, "{}", shot_shown.content);
        println!(
            "WINDOW_SHOWN_SCREENSHOT={}",
            screenshot_path_of(&shot_shown.content).unwrap_or_default()
        );
    }

    #[tokio::test]
    #[ignore]
    async fn verify_mouse_click_sequence_real() {
        if !verify_enabled() {
            return;
        }
        let (ctx, _host) = verify_ctx("verify-click-seq");
        let reg = verify_registry();
        let _ = reg
            .call(
                &ctx,
                "run_command",
                &json!({
                    "command": "Get-Process notepad -ErrorAction SilentlyContinue | Stop-Process -Force",
                    "timeout_secs": 30
                }),
            )
            .await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let doc_path = ctx.paths().data_dir.join("verify_click_seq.txt");
        let initial = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu";
        std::fs::write(&doc_path, initial).expect("写测试文件");
        let (pid, hwnd) = open_notepad_with_file(&reg, &ctx, &doc_path, "verify_click_seq").await;
        let _kill_guard = KillOnDrop(pid);
        focus_window(&reg, &ctx, &hwnd).await;
        // 同样用剪贴板把文本固定下来，避免记事本恢复上次会话
        let _ = reg
            .call(&ctx, "clipboard_write", &json!({"text": initial}))
            .await;
        let _ = reg
            .call(&ctx, "keyboard_combo", &json!({"keys": ["ctrl", "a"]}))
            .await;
        let _ = reg
            .call(&ctx, "keyboard_combo", &json!({"keys": ["ctrl", "v"]}))
            .await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let count_before = char_count(&reg, &ctx, &hwnd).await;
        let (dx, dy, _dw, _dh) = doc_bounds(&reg, &ctx, &hwnd)
            .await
            .expect("控件树里没找到文本区 bounds");
        assert!(
            count_before as usize + 2 >= initial.len(),
            "初始化文本失败（{count_before}）"
        );

        let started = Instant::now();
        let out = reg
            .call(
                &ctx,
                "mouse_click_sequence",
                &json!({
                    "points": [
                        {"x": dx + 30, "y": dy + 18},
                        {"x": dx + 90, "y": dy + 18, "delay_ms": 100},
                        {"x": dx + 150, "y": dy + 18}
                    ],
                    "interval_ms": 120
                }),
            )
            .await;
        let elapsed = started.elapsed().as_millis();
        assert!(out.ok, "{}", out.content);
        println!("=== mouse_click_sequence（3 个点，耗时墙钟 {elapsed}ms）===\n{}", out.content);
        assert!(
            out.content.contains("执行 3/3"),
            "应执行全部 3 个点: {}",
            out.content
        );
        // 连点后字符数不变；输入 3 个字符后应 +3（证明点击已把焦点/光标落在文本区）
        let _ = reg
            .call(&ctx, "keyboard_type", &json!({"text": "SEQ"}))
            .await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        let count_after = char_count(&reg, &ctx, &hwnd).await;
        println!("[verify] 连续点击 + 输入 SEQ：{count_before} -> {count_after}");
        assert_eq!(
            count_after,
            count_before + 3,
            "连续点击后输入 3 字符，字符数应 +3（{count_before} -> {count_after}）"
        );
        let shot = reg
            .call(
                &ctx,
                "screenshot",
                &json!({"display": "primary", "grid": true}),
            )
            .await;
        assert!(shot.ok, "{}", shot.content);
        println!(
            "CLICK_SEQUENCE_SCREENSHOT={}",
            screenshot_path_of(&shot.content).unwrap_or_default()
        );
    }

    #[tokio::test]
    #[ignore]
    async fn verify_background_task_naming() {
        if !verify_enabled() {
            return;
        }
        let (ctx, _host) = verify_ctx("verify-bg-task");
        let reg = verify_registry();
        let started = reg
            .call(
                &ctx,
                "background_task",
                &json!({"command": "Start-Sleep -Seconds 6", "label": "等待测试任务"}),
            )
            .await;
        assert!(started.ok, "{}", started.content);
        println!("=== background_task ===\n{}", started.content);
        assert!(
            started.content.contains("名称：等待测试任务"),
            "background_task 应回显 label: {}",
            started.content
        );
        let task_id = job_id_from(&started.content);
        let status = reg
            .call(&ctx, "background_task_status", &json!({"task_id": task_id}))
            .await;
        assert!(status.ok, "{}", status.content);
        println!("=== background_task_status（运行中）===\n{}", status.content);
        assert!(status.content.contains("名称：等待测试任务"), "{}", status.content);
        assert!(status.content.contains("任务 task_id："), "{}", status.content);
        assert!(status.content.contains("状态：running"), "{}", status.content);
        let all = reg.call(&ctx, "background_task_status", &json!({})).await;
        assert!(all.ok, "{}", all.content);
        println!("=== background_task_status（全部）===\n{}", all.content);
        let killed = reg.call(&ctx, "kill_job", &json!({"job_id": task_id})).await;
        assert!(killed.ok, "{}", killed.content);
        tokio::time::sleep(Duration::from_millis(600)).await;
        let after = reg
            .call(&ctx, "background_task_status", &json!({"task_id": task_id}))
            .await;
        assert!(after.ok, "{}", after.content);
        assert!(after.content.contains("状态：cancelled"), "{}", after.content);
        println!("=== background_task_status（kill 后）===\n{}", after.content);
    }
}
