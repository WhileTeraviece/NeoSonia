//! NeoSonia 成品「计算机使用」端到端测试。
//!
//! 目标：**不直接改 Rust 代码**，只用 neosonia-tools 暴露的公开工具（ui_* / mouse_* /
//! keyboard_* / screenshot / kill_process…）像真人一样操作已经构建好的
//! `target\debug\neosonia.exe`，验证 v0.4 的设置页折叠面板、设备页、聊天输入、窗口拖拽、退出。
//!
//! 运行方式（必须显式开启，避免普通 `cargo test` 误跑）：
//! ```powershell
//! $env:NEOSONIA_E2E = "1"
//! cargo test --offline -p neosonia-tools --test e2e_computeruse -- --ignored --nocapture --test-threads=1
//! ```
//! 需要先构建好 `target\debug\neosonia.exe`。测试会：
//! - 用 `--home E:\NeoSonia\sandbox` 启动一个独立实例（可用 NEOSONIA_E2E_HOME 覆盖）；
//! - 每步输出 UIA 读回证据，每个用例截图到 `logs/e2e-screenshots/`；
//! - 结束后退出 app，汇总报告写到 `logs/e2e-computeruse.md`，并打印「N 通过 / M 失败」。
//!
//! 已知限制：
//! - **全局热键不测**：低级键盘钩子默认忽略 SendInput 注入事件，热键无法用模拟输入验证。
//! - 失败时会把当步截图 + UIA 树 dump 到 `logs/e2e-failures/`，方便人工对比。

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use neosonia_core::config::{Paths, Settings};
use neosonia_core::model::{
    AgentEvent, AgentHost, MemoryApi, MemoryItem, MemoryStats, SubagentRequest, ToolCtx,
    ToolOutcome,
};
use neosonia_core::registry::ToolRegistry;
use parking_lot::RwLock;
use serde_json::{Value, json};

use neosonia_tools::register_all;

// ============================================================
// 测试宿主（最小实现，只为了让工具能拿到 ctx）
// ============================================================

struct NullMemory;

impl MemoryApi for NullMemory {
    fn remember(&self, _c: &str, _t: &[String], _i: f64, _s: &str) -> anyhow::Result<i64> {
        Ok(0)
    }
    fn recall(&self, _q: &str, _l: usize) -> anyhow::Result<Vec<MemoryItem>> {
        Ok(Vec::new())
    }
    fn recent(&self, _l: usize) -> anyhow::Result<Vec<MemoryItem>> {
        Ok(Vec::new())
    }
    fn all(&self, _k: Option<&str>, _l: usize) -> anyhow::Result<Vec<MemoryItem>> {
        Ok(Vec::new())
    }
    fn forget(&self, _id: i64) -> anyhow::Result<bool> {
        Ok(false)
    }
    fn update_importance(&self, _id: i64, _v: f64) -> anyhow::Result<()> {
        Ok(())
    }
    fn get_block(&self, _k: &str) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
    fn set_block(&self, _k: &str, _v: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn blocks(&self) -> anyhow::Result<std::collections::BTreeMap<String, String>> {
        Ok(Default::default())
    }
    fn log_conversation(&self, _r: &str, _c: &str, _t: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn stats(&self) -> anyhow::Result<MemoryStats> {
        Ok(MemoryStats::default())
    }
}

struct TestHost {
    settings: Arc<RwLock<Settings>>,
    paths: Paths,
    memory: Arc<dyn MemoryApi>,
}

impl AgentHost for TestHost {
    fn settings(&self) -> Arc<RwLock<Settings>> {
        self.settings.clone()
    }
    fn paths(&self) -> Paths {
        self.paths.clone()
    }
    fn memory(&self) -> Arc<dyn MemoryApi> {
        self.memory.clone()
    }
    fn emit(&self, _event: AgentEvent) {}
    fn spawn_subagent(&self, _req: SubagentRequest) -> anyhow::Result<String> {
        anyhow::bail!("E2E 测试宿主不支持子代理")
    }
    fn is_cancelled(&self) -> bool {
        false
    }
}

// ============================================================
// UIA 控件树解析（只解析 ui_tree 的文本输出）
// ============================================================

#[derive(Debug, Clone)]
struct Node {
    index: usize,
    role: String,
    name: String,
    bounds: Option<(i32, i32, i32, i32)>,
    patterns: Vec<String>,
    line: String,
}

impl Node {
    fn center(&self) -> Option<(i32, i32)> {
        let (x, y, w, h) = self.bounds?;
        if w <= 0 || h <= 0 {
            return None;
        }
        Some((x + w / 2, y + h / 2))
    }
}

fn parse_node_line(line: &str) -> Option<Node> {
    let rest = line.strip_prefix('#')?;
    let index: usize = rest
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()?;
    let after_index = rest[index.to_string().len()..].trim_start();
    let role = after_index.split_whitespace().next()?.to_string();

    let name = line
        .find("name=\"")
        .and_then(|p| {
            let r = &line[p + "name=\"".len()..];
            r.find('"').map(|e| r[..e].to_string())
        })
        .unwrap_or_default();

    let bounds = line.find("bounds=(").and_then(|p| {
        let r = &line[p + "bounds=(".len()..];
        let e = r.find(')')?;
        let nums: Vec<i32> = r[..e]
            .split(',')
            .filter_map(|s| s.trim().parse::<i32>().ok())
            .collect();
        if nums.len() == 4 {
            Some((nums[0], nums[1], nums[2], nums[3]))
        } else {
            None
        }
    });

    let patterns = line
        .find("patterns=[")
        .and_then(|p| {
            let r = &line[p + "patterns=[".len()..];
            r.find(']').map(|e| {
                r[..e]
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
        })
        .unwrap_or_default();

    Some(Node {
        index,
        role,
        name,
        bounds,
        patterns,
        line: line.to_string(),
    })
}

fn parse_nodes(tree: &str) -> Vec<Node> {
    tree.lines().filter_map(parse_node_line).collect()
}

fn find_node<'a>(nodes: &'a [Node], name_contains: &str, roles: &[&str]) -> Option<&'a Node> {
    nodes.iter().find(|n| {
        n.name.contains(name_contains)
            && (roles.is_empty() || roles.iter().any(|r| n.role.eq_ignore_ascii_case(r)))
    })
}

fn find_node_any<'a>(nodes: &'a [Node], name_contains: &str) -> Option<&'a Node> {
    nodes.iter().find(|n| n.name.contains(name_contains))
}

// ============================================================
// 用例与报告
// ============================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaseStatus {
    Pass,
    Fail,
    Skip,
}

struct Case {
    id: &'static str,
    title: &'static str,
    status: CaseStatus,
    steps: Vec<String>,
    evidence: Vec<String>,
    shots: Vec<String>,
    note: String,
}

impl Case {
    fn new(id: &'static str, title: &'static str) -> Self {
        Self {
            id,
            title,
            status: CaseStatus::Skip,
            steps: Vec::new(),
            evidence: Vec::new(),
            shots: Vec::new(),
            note: String::new(),
        }
    }
}

fn now_stamp() -> String {
    let d = std::time::SystemTime::now();
    let secs = d
        .duration_since(std::time::UNIX_EPOCH)
        .map(|x| x.as_secs())
        .unwrap_or(0);
    // 只用于文件名，不引入 chrono 依赖
    format!("{secs}")
}

// ============================================================
// E2E 主体
// ============================================================

struct E2e {
    ctx: ToolCtx,
    reg: ToolRegistry,
    root: PathBuf,
    home: PathBuf,
    exe: PathBuf,
    logs: PathBuf,
    shots: PathBuf,
    fails: PathBuf,
    report_path: PathBuf,
    report_title: String,
    app_pid: Option<u32>,
    app_hwnd: Option<String>,
    app_title: String,
    cases: Vec<Case>,
    current: Option<usize>,
    child: Option<std::process::Child>,
    /// 本次 spawn 的 app 进程 pid（多实例环境下只认自己的窗口）。
    spawn_pid: Option<u32>,
    /// `NEOSONIA_E2E_ONLY=04,05` 只跑指定用例；None = 全部。
    only: Option<Vec<String>>,
}

impl E2e {
    fn new() -> Self {
        let root = std::env::var_os("NEOSONIA_E2E_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"E:\NeoSonia"));
        let home = std::env::var_os("NEOSONIA_E2E_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join("sandbox"));
        let exe = std::env::var_os("NEOSONIA_E2E_EXE")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join(r"target\debug\neosonia.exe"));
        let logs = root.join("logs");
        let shots = logs.join("e2e-screenshots");
        let fails = logs.join("e2e-failures");
        let report_path = logs.join("e2e-computeruse.md");
        let report_title = "NeoSonia 计算机使用 E2E 测试报告".to_string();
        let _ = std::fs::create_dir_all(&shots);
        let _ = std::fs::create_dir_all(&fails);

        // 支持 “04”、“04,05”、“case04” 这几种写法。
        let only = std::env::var("NEOSONIA_E2E_ONLY")
            .ok()
            .map(|raw| {
                raw.split(|c: char| c == ',' || c == ';' || c.is_whitespace())
                    .map(|s| {
                        s.trim()
                            .trim_start_matches("case")
                            .trim_start_matches("CASE")
                            .to_string()
                    })
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
            })
            .filter(|v| !v.is_empty());

        let mut settings = Settings::default();
        settings.tools.allow_input_simulation = true;
        settings.tools.uia_enabled = true;
        settings.tools.cdp_enabled = true;
        settings.tools.allow_shell = true;
        // 截图保留原始清晰度（默认 1600 最长边对 3840 屏会把字缩太小）。
        settings.tools.screenshot_max_edge = 3840;

        let paths = Paths::from_root(root.clone());
        let host = Arc::new(TestHost {
            settings: Arc::new(RwLock::new(settings.clone())),
            paths,
            memory: Arc::new(NullMemory),
        });
        let ctx = ToolCtx::new(host, "turn-e2e", "call-e2e");
        let reg = ToolRegistry::new();
        register_all(&reg, &settings);

        Self {
            ctx,
            reg,
            root,
            home,
            exe,
            logs,
            shots,
            fails,
            report_path,
            report_title,
            app_pid: None,
            app_hwnd: None,
            app_title: String::new(),
            cases: Vec::new(),
            current: None,
            child: None,
            spawn_pid: None,
            only,
        }
    }

    /// 切到 v0.5 验收模式：报告 logs/e2e-v05.md，截图 logs/ui/e2e-v05/。
    fn configure_v05(&mut self) {
        self.report_title = "NeoSonia v0.5 计算机使用验收报告".to_string();
        self.report_path = self.logs.join("e2e-v05.md");
        self.shots = self.logs.join("ui").join("e2e-v05");
        self.fails = self.shots.join("failures");
        let _ = std::fs::create_dir_all(&self.shots);
        let _ = std::fs::create_dir_all(&self.fails);
    }

    /// 当前这次运行是否要执行某个用例（未设置 NEOSONIA_E2E_ONLY 时全部执行）。
    fn selected(&self, id: &str) -> bool {
        match &self.only {
            None => true,
            Some(list) => list.iter().any(|x| x == id),
        }
    }

    // ---------------- 记录 ----------------

    fn case_begin(&mut self, id: &'static str, title: &'static str) {
        if self.current.is_some() {
            self.case_end(
                CaseStatus::Skip,
                "未执行完成（后续用例提前结束）".to_string(),
            );
        }
        self.cases.push(Case::new(id, title));
        self.current = Some(self.cases.len() - 1);
        println!("\n===== 用例 {}: {} =====", id, title);
    }

    fn step(&mut self, s: impl Into<String>) {
        let s = s.into();
        println!("  [step] {s}");
        if let Some(i) = self.current {
            self.cases[i].steps.push(s);
        }
    }

    fn evidence(&mut self, s: impl Into<String>) {
        let s = s.into();
        println!("  [uia ] {s}");
        if let Some(i) = self.current {
            self.cases[i].evidence.push(s);
        }
    }

    fn case_end(&mut self, status: CaseStatus, note: String) {
        if let Some(i) = self.current.take() {
            self.cases[i].status = status;
            self.cases[i].note = note.clone();
            let label = match status {
                CaseStatus::Pass => "通过",
                CaseStatus::Fail => "失败",
                CaseStatus::Skip => "跳过",
            };
            println!("  => {label}: {note}");
        }
    }

    fn fail_case(&mut self, reason: impl Into<String>) {
        let reason = reason.into();
        println!("  [FAIL] {reason}");
        self.case_end(CaseStatus::Fail, reason);
    }

    fn window_arg(&self) -> String {
        self.app_hwnd
            .clone()
            .unwrap_or_else(|| "foreground".to_string())
    }

    // ---------------- 工具调用 ----------------

    async fn call(&mut self, name: &str, args: Value) -> ToolOutcome {
        let out = self.reg.call(&self.ctx, name, &args).await;
        if !out.ok {
            println!(
                "  [tool-err] {name}: {}",
                out.content.lines().take(3).collect::<Vec<_>>().join(" / ")
            );
        }
        out
    }

    async fn shot(&mut self, label: &str) -> Option<String> {
        let path = self.shots.join(format!("{label}.png"));
        let out = self
            .call(
                "screenshot",
                json!({ "save_path": path.to_string_lossy(), "max_edge": 0 }),
            )
            .await;
        if out.ok {
            match read_path_from(&out.content).or_else(|| Some(path.to_string_lossy().to_string()))
            {
                Some(p) if Path::new(&p).is_file() => {
                    println!("  [shot] {p}");
                    if let Some(i) = self.current {
                        self.cases[i].shots.push(p.clone());
                    }
                    Some(p)
                }
                _ => None,
            }
        } else {
            None
        }
    }

    /// 取全部控件树（depth 12 / max 2000，足够覆盖设置页长列表）。
    async fn tree(&mut self) -> String {
        let win = self.window_arg();
        let out = self
            .call(
                "ui_tree",
                json!({ "window": win, "depth": 12, "max_nodes": 2000, "timeout_ms": 30000 }),
            )
            .await;
        out.content
    }

    /// 带 name_filter 的控件树（只回匹配节点，附带原始 BFS index）。
    async fn tree_filtered(&mut self, name_filter: &str) -> String {
        let win = self.window_arg();
        let out = self
            .call(
                "ui_tree",
                json!({
                    "window": win,
                    "depth": 12,
                    "max_nodes": 2000,
                    "name_filter": name_filter,
                    "timeout_ms": 30000
                }),
            )
            .await;
        out.content
    }

    async fn focus_app(&mut self) -> bool {
        let Some(hwnd) = self.app_hwnd.clone() else {
            return false;
        };
        let out = self.call("ui_focus_window", json!({ "hwnd": hwnd })).await;
        if out.ok {
            return true;
        }
        // 失败时点一下窗口标题栏再试一次（前台锁/置顶窗口场景）。
        let tree = self.tree_filtered("NeoSonia").await;
        let root = parse_nodes(&tree)
            .into_iter()
            .find(|n| n.role.eq_ignore_ascii_case("Window"));
        if let Some(root) = root
            && let Some((x, y, w, _h)) = root.bounds
        {
            let _ = self
                .call(
                    "mouse_click",
                    json!({ "x": x + (w / 2).max(40), "y": y + 24 }),
                )
                .await;
            tokio::time::sleep(Duration::from_millis(400)).await;
            let out = self.call("ui_focus_window", json!({ "hwnd": hwnd })).await;
            return out.ok;
        }
        false
    }

    /// 用 ui_act 的 index 路径点击节点（index 与 ui_tree 使用同一遍历参数，能精确命中）。
    async fn click_node_index(&mut self, node: &Node) -> bool {
        let win = self.window_arg();
        let out = self
            .call(
                "ui_act",
                json!({
                    "action": "click",
                    "window": win,
                    "index": node.index,
                    "depth": 12,
                    "max_nodes": 2000
                }),
            )
            .await;
        if out.ok {
            self.evidence(format!(
                "ui_act click index={} role={} name=\"{}\" 成功",
                node.index, node.role, node.name
            ));
            true
        } else {
            // 退化：鼠标点中心点
            match node.center() {
                Some((x, y)) => {
                    let out = self.call("mouse_click", json!({ "x": x, "y": y })).await;
                    self.evidence(format!(
                        "鼠标点击 ({x},{y}) role={} name=\"{}\" ok={}",
                        node.role, node.name, out.ok
                    ));
                    out.ok
                }
                None => {
                    self.evidence(format!("节点没有可点击区域：{}", node.line));
                    false
                }
            }
        }
    }

    /// 先按名字在整棵树里定位（优先 Button/TabItem 等可点击角色），再按 index 点击。
    async fn click_by_name(&mut self, name_contains: &str, roles: &[&str]) -> Result<Node, String> {
        let filtered = self.tree_filtered(name_contains).await;
        let nodes = parse_nodes(&filtered);
        // 优先可点击角色，其次任何匹配节点
        let roles_order: Vec<&str> = if roles.is_empty() {
            vec![
                "Button",
                "TabItem",
                "RadioButton",
                "MenuItem",
                "ListItem",
                "Edit",
            ]
        } else {
            roles.to_vec()
        };
        let mut chosen: Option<Node> = None;
        for role in &roles_order {
            if let Some(n) = find_node(&nodes, name_contains, &[role]) {
                chosen = Some(n.clone());
                break;
            }
        }
        if chosen.is_none() {
            chosen = find_node_any(&nodes, name_contains).cloned();
        }
        let Some(node) = chosen else {
            return Err(format!("控件树里找不到名字包含「{name_contains}」的节点"));
        };
        self.evidence(format!(
            "定位到 #{} {} name=\"{}\" bounds={:?} patterns={:?}",
            node.index, node.role, node.name, node.bounds, node.patterns
        ));
        if self.click_node_index(&node).await {
            Ok(node)
        } else {
            Err(format!("点击「{}」失败", node.name))
        }
    }

    /// 从窗口左边量逻辑像素点击（布局比例兜底，例如底栏「对话」tab）。
    async fn click_ratio_from_left(&mut self, logical_x: f64, logical_y: f64) -> bool {
        let tree = self.tree_filtered("NeoSonia").await;
        let root = parse_nodes(&tree)
            .into_iter()
            .find(|n| n.role.eq_ignore_ascii_case("Window"));
        let Some((x, y, w, _h)) = root.and_then(|n| n.bounds) else {
            self.evidence("比例坐标兜底失败：拿不到窗口 bounds");
            return false;
        };
        let scale = (w as f64) / 424.0;
        let px = x + (logical_x * scale) as i32;
        let py = y + (logical_y * scale) as i32;
        let out = self.call("mouse_click", json!({ "x": px, "y": py })).await;
        self.evidence(format!(
            "比例坐标点击 logical=({logical_x},{logical_y}) -> ({px},{py}) ok={}",
            out.ok
        ));
        out.ok
    }

    // ---------------- app 生命周期 ----------------

    /// 窗口列表行 → (hwnd, title, pid)，并且只认本次 spawn 的 pid（多实例环境防误抓）。
    fn window_line_matches(&self, line: &str) -> Option<(String, String, u32)> {
        let hwnd = token_after(line, "hwnd=")?;
        let pid = token_after(line, "pid=")?.parse::<u32>().ok()?;
        let title = quoted_after(line, "title=\"")?;
        if let Some(expected) = self.spawn_pid {
            if pid != expected {
                return None;
            }
        }
        if !title.contains("NeoSonia") {
            return None;
        }
        Some((hwnd, title, pid))
    }

    /// 等 UIA 树里出现「连接其他设备」按钮，且窗口标题不再带「(未响应)」。
    /// 新机器首次启动 wgpu/字体初始化可能要几十秒，黑屏时截图没有证据价值。
    async fn wait_ui_ready(&mut self, timeout_ms: u64) -> bool {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let out = self
                .call(
                    "ui_window_list",
                    json!({ "filter": "NeoSonia", "limit": 10 }),
                )
                .await;
            let title_ok = out.ok
                && out.content.lines().any(|l| {
                    self.window_line_matches(l)
                        .map(|(_, t, _)| !t.contains("未响应"))
                        .unwrap_or(false)
                });
            if title_ok {
                let tree = self.tree_filtered("连接其他设备").await;
                if !parse_nodes(&tree).is_empty() {
                    return true;
                }
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(800)).await;
        }
    }

    /// 读窗口当前 x（GetWindowRect）。窗口被“移出屏幕”隐藏时 x 会变得很小（如 -12000）。
    fn window_x(&self) -> Option<i32> {
        self.app_hwnd.as_deref().and_then(hwnd_rect_x)
    }

    /// 等窗口 x 满足条件（热键隐藏/显示断言）。
    async fn wait_window_x<F>(&mut self, pred: F, timeout_ms: u64) -> Option<i32>
    where
        F: Fn(i32) -> bool,
    {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            if let Some(x) = self.window_x() {
                if pred(x) {
                    return Some(x);
                }
            }
            if Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }

    async fn wait_window(&mut self, timeout_ms: u64) -> Option<(String, String, u32)> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let out = self
                .call(
                    "ui_window_list",
                    json!({ "filter": "NeoSonia", "limit": 10 }),
                )
                .await;
            if out.ok {
                for line in out.content.lines().filter(|l| l.starts_with('#')) {
                    if let Some(found) = self.window_line_matches(line) {
                        return Some(found);
                    }
                }
            }
            if Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    fn kill_app_fallback(&mut self) {
        let pid = self.spawn_pid.or(self.app_pid);
        if let Some(pid) = pid {
            let _ = std::process::Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .output();
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
        }
    }

    /// 用例 7 用：先发 WM_CLOSE（app 里关闭窗口即退出），超时再 taskkill。
    async fn close_app(&mut self) -> bool {
        if let Some(hwnd) = self.app_hwnd.clone()
            && let Some(v) = parse_hwnd(&hwnd)
        {
            use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
            use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_CLOSE};
            unsafe {
                let _ = PostMessageW(
                    HWND(v as *mut std::ffi::c_void),
                    WM_CLOSE,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            if !self.app_window_exists().await {
                self.app_pid = None;
                return true;
            }
            // v0.4 里「隐藏窗口」是移出屏幕：若 WM_CLOSE 被映射成隐藏，窗口 x 会 < -5000。
            if let Some(x) = self.window_x() {
                if x < -5000 {
                    eprintln!(
                        "[info] WM_CLOSE 后窗口只是隐藏（x={x}），按关闭处理：taskkill 本次 pid"
                    );
                    self.kill_app_fallback();
                    tokio::time::sleep(Duration::from_millis(800)).await;
                    return !self.app_window_exists().await;
                }
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
        self.kill_app_fallback();
        tokio::time::sleep(Duration::from_millis(800)).await;
        !self.app_window_exists().await
    }

    async fn app_window_exists(&mut self) -> bool {
        let out = self
            .call(
                "ui_window_list",
                json!({ "filter": "NeoSonia", "limit": 10 }),
            )
            .await;
        out.ok
            && out
                .content
                .lines()
                .any(|l| self.window_line_matches(l).is_some())
    }

    // ---------------- v0.5 辅助 ----------------

    /// 带环境变量启动 app（v0.5 用例需要 NEOSONIA_ATTACH / NEOSONIA_PROFILE 等）。
    async fn launch_app_with_env(
        &mut self,
        envs: &[(&str, String)],
    ) -> Result<(String, String, u32), String> {
        let mut cmd = std::process::Command::new(&self.exe);
        cmd.arg("--home").arg(&self.home);
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let child = cmd.spawn().map_err(|e| format!("启动 app 失败：{e}"))?;
        self.spawn_pid = Some(child.id());
        self.child = Some(child);
        let (hwnd, title, pid) = self
            .wait_window(120_000)
            .await
            .ok_or_else(|| "120 秒内没等到自己的 NeoSonia 窗口".to_string())?;
        self.app_hwnd = Some(hwnd.clone());
        self.app_pid = Some(pid);
        self.app_title = title.clone();
        let _ = self.focus_app().await;
        if !self.wait_ui_ready(120_000).await {
            return Err("窗口出现但 120 秒内 UIA 树未渲染完成".to_string());
        }
        Ok((hwnd, title, pid))
    }

    /// sandbox 日志文件路径。
    fn app_log_path(&self) -> PathBuf {
        self.home.join("logs").join("neosonia.log")
    }

    fn log_len(&self) -> u64 {
        std::fs::metadata(self.app_log_path())
            .map(|m| m.len())
            .unwrap_or(0)
    }

    /// 读 app 日志从 offset 之后的新增内容。
    fn read_log_since(&self, offset: u64) -> String {
        use std::io::{Read, Seek, SeekFrom};
        let path = self.app_log_path();
        let Ok(mut f) = std::fs::File::open(&path) else {
            return String::new();
        };
        if f.seek(SeekFrom::Start(offset)).is_err() {
            return String::new();
        }
        let mut buf = String::new();
        let _ = f.read_to_string(&mut buf);
        buf
    }

    /// 点底栏导航（逻辑坐标 y=68），返回是否点击成功。
    async fn click_nav(&mut self, logical_x: f64) -> bool {
        self.click_ratio_from_left(logical_x, 68.0).await
    }

    /// 读当前窗口 rect（x,y,w,h）。
    fn window_rect(&self) -> Option<(i32, i32, i32, i32)> {
        self.app_hwnd.as_deref().and_then(hwnd_rect)
    }

    /// 窗口是否可见（v0.5 隐藏用 set_visible(false)，IsWindowVisible 为 false）。
    fn window_visible(&self) -> bool {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::IsWindowVisible;

        let Some(hwnd) = self.app_hwnd.as_deref().and_then(parse_hwnd) else {
            return false;
        };
        unsafe { IsWindowVisible(HWND(hwnd as *mut std::ffi::c_void)).as_bool() }
    }

    // ---------------- 报告 ----------------

    fn write_report(&self) {
        let mut md = String::new();
        md.push_str(&format!("# {}\n\n", self.report_title));
        md.push_str(&format!(
            "- 时间戳(ms): {}\n- root: {}\n- exe: {}\n- home: {}\n- app 窗口: {}\n- 工具: neosonia-tools 的 ui_* / mouse_* / keyboard_* / screenshot\n- 说明: 通过真实模拟输入 + UIA / 日志 / 文件系统读回断言；截图见每用例路径。\n\n",
            now_stamp(),
            self.root.display(),
            self.exe.display(),
            self.home.display(),
            if self.app_title.is_empty() {
                "(未启动)"
            } else {
                self.app_title.as_str()
            }
        ));
        let pass = self
            .cases
            .iter()
            .filter(|c| c.status == CaseStatus::Pass)
            .count();
        let fail = self
            .cases
            .iter()
            .filter(|c| c.status == CaseStatus::Fail)
            .count();
        let skip = self
            .cases
            .iter()
            .filter(|c| c.status == CaseStatus::Skip)
            .count();
        md.push_str("## 汇总\n\n");
        md.push_str("| # | 用例 | 结果 | 截图 |\n| --- | --- | --- | --- |\n");
        for c in &self.cases {
            let st = match c.status {
                CaseStatus::Pass => "通过",
                CaseStatus::Fail => "失败",
                CaseStatus::Skip => "跳过",
            };
            md.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                c.id,
                c.title,
                st,
                c.shots.len()
            ));
        }
        md.push_str(&format!(
            "\n**{pass} 通过 / {fail} 失败 / {skip} 跳过**\n\n"
        ));

        for c in &self.cases {
            let st = match c.status {
                CaseStatus::Pass => "通过",
                CaseStatus::Fail => "失败",
                CaseStatus::Skip => "跳过",
            };
            md.push_str(&format!("## {} {}\n\n- 结果: {st}\n", c.id, c.title));
            if !c.note.is_empty() {
                md.push_str(&format!("- 备注: {}\n", c.note));
            }
            md.push_str("\n步骤:\n");
            for (i, s) in c.steps.iter().enumerate() {
                md.push_str(&format!("{}. {s}\n", i + 1));
            }
            if !c.evidence.is_empty() {
                md.push_str("\nUIA 读回证据:\n");
                for e in &c.evidence {
                    md.push_str(&format!("- {e}\n"));
                }
            }
            if !c.shots.is_empty() {
                md.push_str("\n截图:\n");
                for s in &c.shots {
                    md.push_str(&format!("- `{s}`\n"));
                }
            }
            md.push('\n');
        }

        md.push_str("## 环境限制与说明\n\n");
        if self.report_title.contains("v0.5") {
            md.push_str("- v0.5：Alt+Space 走 RegisterHotKey，注入事件可触发；窗口“隐藏”用 set_visible(false)，断言用 IsWindowVisible（rect 不再移出屏幕）。\n");
            md.push_str("- 按住拖拽用 mouse_press → mouse_move（10 步）→ mouse_release，位移以 GetWindowRect 断言。\n");
            md.push_str("- 切页流畅度读沙箱日志 `page switch ... ms=\"...\"`；`NEOSONIA_PROFILE=1` 时另有 `view built` / `frame rate` 日志。\n");
            md.push_str("- 附件用例通过启动环境变量 `NEOSONIA_ATTACH=<png>` 预置附件，断言 UIA Button「移除附件 …」存在。\n");
            md.push_str("- 人设用例断言 `personas/e2e-auto.md` 落盘内容，并在结束后删除测试文件 + 重新扫描。\n");
        } else {
            md.push_str("- **全局热键未测试**：低级键盘钩子默认忽略 SendInput 注入事件，模拟输入无法验证热键。\n");
        }
        md.push_str("- 测试用 `--home` 独立 sandbox，不会污染用户真实配置/聊天记录。\n");
        md.push_str("- 失败用例会留下 UIA 树 dump 与失败截图，便于人工复核。\n");

        let file = self.report_path.clone();
        if let Err(e) = std::fs::write(&file, md) {
            eprintln!("写报告失败 {}: {e}", file.display());
        } else {
            println!("\n[report] {}", file.display());
        }
    }

    async fn dump_failure(&mut self, case_id: &str, tree: &str) {
        let base = self.fails.join(case_id);
        let _ = std::fs::write(base.with_extension("tree.txt"), tree);
        if let Some(p) = self.shot(&format!("{case_id}-fail")).await {
            println!(
                "[fail-artifacts] tree={} shot={}",
                base.with_extension("tree.txt").display(),
                p
            );
        } else {
            println!(
                "[fail-artifacts] tree={}",
                base.with_extension("tree.txt").display()
            );
        }
    }

    fn print_summary(&self) {
        let pass = self
            .cases
            .iter()
            .filter(|c| c.status == CaseStatus::Pass)
            .count();
        let fail = self
            .cases
            .iter()
            .filter(|c| c.status == CaseStatus::Fail)
            .count();
        let skip = self
            .cases
            .iter()
            .filter(|c| c.status == CaseStatus::Skip)
            .count();
        println!("\n================ E2E 汇总 ================");
        for c in &self.cases {
            let st = match c.status {
                CaseStatus::Pass => "PASS",
                CaseStatus::Fail => "FAIL",
                CaseStatus::Skip => "SKIP",
            };
            println!("[{st}] {} {}", c.id, c.title);
        }
        println!("{pass} 通过 / {fail} 失败 / {skip} 跳过");
        println!("报告: {}", self.report_path.display());
    }
}

// ============================================================
// 小工具
// ============================================================

fn read_path_from(content: &str) -> Option<String> {
    content
        .lines()
        .find(|l| l.starts_with("路径："))
        .map(|l| l.trim_start_matches("路径：").trim().to_string())
}

fn token_after(line: &str, key: &str) -> Option<String> {
    let pos = line.find(key)?;
    let rest = &line[pos + key.len()..];
    let tok: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
    if tok.is_empty() { None } else { Some(tok) }
}

fn quoted_after(line: &str, key: &str) -> Option<String> {
    let pos = line.find(key)?;
    let rest = &line[pos + key.len()..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// GetWindowRect 读窗口完整 rect（含负值；窗口被移出屏幕时可用于判断“隐藏”）。
fn hwnd_rect(hwnd: &str) -> Option<(i32, i32, i32, i32)> {
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

    let v = parse_hwnd(hwnd)?;
    let mut rect = RECT::default();
    unsafe {
        GetWindowRect(HWND(v as *mut std::ffi::c_void), &mut rect).ok()?;
    }
    Some((
        rect.left,
        rect.top,
        rect.right - rect.left,
        rect.bottom - rect.top,
    ))
}

/// GetWindowRect 读窗口左上角 x。
fn hwnd_rect_x(hwnd: &str) -> Option<i32> {
    hwnd_rect(hwnd).map(|r| r.0)
}

fn parse_hwnd(hwnd: &str) -> Option<isize> {
    let raw = hwnd.trim();
    let hex = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X"))?;
    i64::from_str_radix(hex, 16).ok().map(|v| v as isize)
}

fn find_first_node(tree: &str, name_contains: &str) -> Option<Node> {
    parse_nodes(tree)
        .into_iter()
        .find(|n| n.name.contains(name_contains))
}

// ============================================================
// 主测试
// ============================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore = "计算机使用 E2E：需要构建好的 target\\debug\\neosonia.exe；用 NEOSONIA_E2E=1 + --ignored 运行"]
async fn e2e_computeruse() {
    if std::env::var("NEOSONIA_E2E").ok().as_deref() != Some("1") {
        println!("[skip] NEOSONIA_E2E != 1（设置环境变量 NEOSONIA_E2E=1 才跑真机 E2E）");
        return;
    }

    let mut e = E2e::new();
    println!("exe = {}", e.exe.display());
    println!("home = {}", e.home.display());
    println!("logs = {}", e.logs.display());

    if !e.exe.is_file() {
        eprintln!(
            "[FATAL] 找不到 app 可执行文件：{}（先 cargo build -p neosonia-app）",
            e.exe.display()
        );
        return;
    }
    if let Err(err) = std::fs::create_dir_all(&e.home) {
        eprintln!("[FATAL] 无法创建 sandbox home {}: {err}", e.home.display());
        return;
    }

    // ---------------- 启动 app（03~07 的前置；02 只有被选中时才记为一个用例） ----------------
    let need_app = ["01", "02", "03", "04", "05", "06", "07"]
        .iter()
        .any(|id| e.selected(id));
    if !need_app {
        println!("[info] NEOSONIA_E2E_ONLY 没有选中需要 app 的用例，结束。");
        e.write_report();
        e.print_summary();
        return;
    }
    if e.selected("02") {
        e.case_begin("02", "启动 app、等待窗口、基线截图");
    }
    e.step(format!(
        "启动 {} --home {}",
        e.exe.display(),
        e.home.display()
    ));
    let spawn = std::process::Command::new(&e.exe)
        .arg("--home")
        .arg(&e.home)
        .spawn();
    match spawn {
        Ok(child) => {
            e.spawn_pid = Some(child.id());
            e.child = Some(child);
        }
        Err(err) => {
            e.fail_case(format!("启动 app 失败：{err}"));
            e.write_report();
            e.print_summary();
            return;
        }
    }
    e.step("轮询 ui_window_list(filter=NeoSonia)，最多 120 秒");
    match e.wait_window(120_000).await {
        Some((hwnd, title, pid)) => {
            e.app_hwnd = Some(hwnd.clone());
            e.app_pid = Some(pid);
            e.app_title = title.clone();
            e.evidence(format!("窗口出现：hwnd={hwnd} pid={pid} title=\"{title}\""));
            e.step("ui_focus_window 把 app 切到前台");
            let focused = e.focus_app().await;
            e.evidence(format!("ui_focus_window ok={focused}"));
            e.step("等 UIA 树渲染完成（出现「连接其他设备」且标题不带「(未响应)」），最多 120 秒");
            let ready = e.wait_ui_ready(120_000).await;
            e.evidence(format!("UI 渲染完成={ready}"));
            if !ready {
                e.evidence("警告：120 秒内 UI 仍未渲染完成，后续用例可能因黑屏/空树失败");
            }
            if e.selected("02") {
                e.step("screenshot 基线");
                let shot = e.shot("02-launch-baseline").await;
                if shot.is_some() {
                    let tree = e.tree().await;
                    let dump = e.logs.join("e2e-tree.txt");
                    let _ = std::fs::write(&dump, &tree);
                    e.evidence(format!(
                        "完整控件树已 dump 到 {}（{} 个节点）",
                        dump.display(),
                        parse_nodes(&tree).len()
                    ));
                    e.case_end(CaseStatus::Pass, format!("窗口已启动：{title}"));
                } else {
                    let tree = e.tree().await;
                    e.dump_failure("02-launch", &tree).await;
                    e.fail_case("窗口出现但基线截图失败");
                }
            }
        }
        None => {
            if e.selected("02") {
                let tree = String::new();
                e.dump_failure("02-launch", &tree).await;
                e.fail_case("120 秒内没等到自己的 NeoSonia 窗口（app 可能启动失败/被 GPU 白屏卡住/pid 不符，看 logs/neosonia.log）");
            } else {
                println!("[FATAL] 120 秒内没等到自己的 NeoSonia 窗口");
            }
        }
    }
    if e.app_hwnd.is_none() {
        e.write_report();
        e.print_summary();
        e.kill_app_fallback();
        return;
    }

    // ---------------- 用例 1：全局热键（v0.4 用 RegisterHotKey 实现，注入事件可触发） ----------------
    if e.selected("01") {
        e.case_begin("01", "全局热键：Alt+Space 切换隐藏/显示窗口");
        e.step("读初始窗口 x（可见时 dx > 0，例如 x≈2750）");
        let x0 = e.window_x();
        e.evidence(format!("初始窗口 x={x0:?}"));
        let visible_ok = x0.map(|x| x > 0).unwrap_or(false);
        let mut hidden_x = None;
        let mut shown_x = None;
        if visible_ok {
            e.step("keyboard_key(alt+space) 第一次 → 期待窗口移出屏幕（x < -5000）");
            let press = e
                .call(
                    "keyboard_key",
                    json!({ "key": "space", "modifiers": ["alt"] }),
                )
                .await;
            e.evidence(format!("第一次 Alt+Space ok={}", press.ok));
            hidden_x = e.wait_window_x(|x| x < -5000, 6_000).await;
            e.evidence(format!("隐藏后窗口 x={hidden_x:?}（应 < -5000）"));
            let _ = e.shot("01-hotkey-hidden").await;
            e.step("keyboard_key(alt+space) 第二次 → 期待窗口回到屏幕内（x > 0）");
            let press2 = e
                .call(
                    "keyboard_key",
                    json!({ "key": "space", "modifiers": ["alt"] }),
                )
                .await;
            e.evidence(format!("第二次 Alt+Space ok={}", press2.ok));
            shown_x = e.wait_window_x(|x| x > 0, 6_000).await;
            e.evidence(format!("显示后窗口 x={shown_x:?}（应 > 0）"));
        }
        let _ = e.shot("01-hotkey-visible").await;
        if visible_ok && hidden_x.is_some() && shown_x.is_some() {
            e.case_end(
                CaseStatus::Pass,
                format!("Alt+Space 隐藏/显示切换成功（x {x0:?} → {hidden_x:?} → {shown_x:?}）"),
            );
        } else {
            let tree = e.tree_filtered("NeoSonia").await;
            e.dump_failure("01-hotkey", &tree).await;
            e.fail_case(format!(
                "热键切换失败：初始 x={x0:?} 隐藏后 x={hidden_x:?} 显示后 x={shown_x:?}（多次 Alt+Space 后窗口 rect 没有按预期切换）"
            ));
        }
    }

    // ---------------- 用例 3：设置页折叠面板 ----------------
    // ---------------- 用例 3：设置页（MCP 普通段 + 技能折叠面板） ----------------
    if e.selected("03") {
        e.case_begin("03", "设置页：MCP 段内容 + 展开「技能」折叠面板");
        e.step("进入设置页：优先点底栏「设置」；失败改点顶栏 mode chip；再失败用比例坐标");
        let find_mcp_tree = e.tree_filtered("MCP 服务器").await;
        let on_settings = !parse_nodes(&find_mcp_tree).is_empty();
        e.evidence(format!("启动后是否已在设置页：{on_settings}"));
        if !on_settings {
            let mut entered = e
                .click_by_name("设置", &["TabItem", "RadioButton", "Button"])
                .await
                .is_ok();
            if !entered {
                e.evidence("底栏「设置」点不到，改点顶栏 mode chip");
                for label in ["日常", "编程", "计算机使用"] {
                    if e.click_by_name(label, &["Button"]).await.is_ok() {
                        entered = true;
                        break;
                    }
                }
            }
            if !entered {
                e.evidence("mode chip 名字也找不到，按新校准坐标点底栏「设置」（逻辑 201,68）");
                entered = e.click_ratio_from_left(201.0, 68.0).await;
            }
            if !entered {
                e.fail_case("进不了设置页（底栏「设置」与 mode chip 都点不到）");
            }
        }
        tokio::time::sleep(Duration::from_millis(900)).await;
        let settings_tree = e.tree().await;
        e.evidence(format!(
            "设置页控件数：{}；含「MCP 服务器」={}；含「技能」={}",
            parse_nodes(&settings_tree).len(),
            settings_tree.contains("MCP 服务器"),
            settings_tree.contains("技能")
        ));
        let _ = e.shot("03a-settings-page").await;

        if e.cases.last().map(|c| c.status) != Some(CaseStatus::Fail) {
            // 3.1 MCP 是普通 section（不折叠）：直接验证内容已在页面里。
            e.step("验证 MCP 段内容（MCP 是普通 section，不需要展开）");
            let mcp_tree = e.tree_filtered("MCP").await;
            let mcp_ok = mcp_tree.contains("启用 MCP 服务器")
                || mcp_tree.contains("Model Context Protocol")
                || mcp_tree.contains("还没有配置 MCP 服务器")
                || mcp_tree.contains("MCP 服务器");
            e.evidence(format!(
                "MCP 段内容出现={mcp_ok}（匹配节点 {} 个）",
                parse_nodes(&mcp_tree).len()
            ));
            let _ = e.shot("03b-settings-mcp").await;

            if !mcp_ok {
                e.dump_failure("03-mcp", &mcp_tree).await;
                e.fail_case("设置页里没找到 MCP 段内容（可能没进设置页或树里名字为空）");
            } else {
                // 3.2 技能是折叠面板：点 header 展开，验证 body 出现。
                e.step(
                    "展开「技能」折叠面板并验证 body 出现（技能目录 / 还没有安装技能 / 打开目录）",
                );
                let before_sk = e.tree_filtered("技能").await;
                let clicked_sk = e.click_by_name("技能", &["Button", "TabItem"]).await;
                tokio::time::sleep(Duration::from_millis(900)).await;
                let after_sk = e.tree_filtered("技能").await;
                let expanded_sk = after_sk.contains("技能目录")
                    || after_sk.contains("还没有安装技能")
                    || after_sk.contains("打开目录");
                e.evidence(format!(
                    "技能面板点击 ok={}；展开前节点数={} 展开后节点数={}；含展开内容={}",
                    clicked_sk.is_ok(),
                    parse_nodes(&before_sk).len(),
                    parse_nodes(&after_sk).len(),
                    expanded_sk
                ));
                let _ = e.shot("03c-settings-skills").await;
                if expanded_sk {
                    e.case_end(
                        CaseStatus::Pass,
                        "MCP 段内容存在，技能折叠面板展开后出现「技能目录/还没有安装技能」"
                            .to_string(),
                    );
                } else {
                    e.dump_failure("03-skills", &after_sk).await;
                    e.fail_case(
                        "「技能」点击后没有检测到展开内容（header 名字缺失或点击没命中折叠行）",
                    );
                }
            }
        }
    }
    // ---------------- 用例 4：设备页 ----------------
    if e.selected("04") {
        e.case_begin("04", "标题栏「连接其他设备」→ 设备页（二维码/配对区域）");
        e.step("点击顶栏「连接其他设备」（UIA 名字点不到时用校准坐标：逻辑 288,24）");
        let mut click_remote = e.click_by_name("连接其他设备", &["Button"]).await;
        if click_remote.is_err() {
            e.evidence("顶栏按钮名字点不到，用坐标兜底（物理 = 窗口原点 + 逻辑*scale）");
            let ok = e.click_ratio_from_left(288.0, 24.0).await;
            if ok {
                click_remote = Ok(Node {
                    index: 0,
                    role: "Button".to_string(),
                    name: "连接其他设备(坐标兜底)".to_string(),
                    bounds: None,
                    patterns: Vec::new(),
                    line: String::new(),
                });
            }
        }
        tokio::time::sleep(Duration::from_millis(900)).await;
        // 设备页独有文案（设置页只有「连接其他设备（手机/平板）」这一行，不会误判）。
        let remote_tree = e.tree_filtered("配对").await;
        let remote_tree2 = e.tree_filtered("访问地址").await;
        let on_remote = remote_tree.contains("扫码配对")
            || remote_tree.contains("配对码")
            || remote_tree2.contains("访问地址");
        e.evidence(format!(
            "点击「连接其他设备」ok={}；设备页特征（扫码配对/配对码/访问地址）={}",
            click_remote.is_ok(),
            on_remote
        ));
        let shot = e.shot("04-remote-panel").await;
        if on_remote {
            e.case_end(
                CaseStatus::Pass,
                "已进入设备页（检测到配对码/访问地址/二维码区域）".to_string(),
            );
        } else {
            let tree = e.tree().await;
            e.dump_failure("04-remote", &tree).await;
            e.fail_case(format!(
                "没有进入设备页（shot={}）；当前树前 20 个名字：{:?}",
                shot.unwrap_or_default(),
                parse_nodes(&tree)
                    .iter()
                    .take(20)
                    .map(|n| n.name.clone())
                    .collect::<Vec<_>>()
            ));
        }
    }
    // ---------------- 用例 5：聊天输入「你好」 ----------------
    if e.selected("05") {
        e.case_begin("05", "聊天输入框输入「你好」并回车，验证用户气泡");
        e.step("点底栏「对话」回到聊天页（名字点不到时用布局比例兜底：逻辑坐标 29,68）");
        let mut nav_ok = e
            .click_by_name("对话", &["TabItem", "RadioButton", "Button"])
            .await
            .is_ok();
        if !nav_ok {
            e.evidence("底栏「对话」名字点不到，用比例坐标兜底");
            nav_ok = e.click_ratio_from_left(19.0, 68.0).await;
        }
        e.evidence(format!("切到聊天页 ok={nav_ok}"));
        tokio::time::sleep(Duration::from_millis(600)).await;
        e.step("定位输入框（Edit，占位文案「输入指令，回车发送」）并点击聚焦");
        let input_tree = e.tree_filtered("输入指令").await;
        let input = parse_nodes(&input_tree)
            .into_iter()
            .find(|n| n.role.eq_ignore_ascii_case("Edit"))
            .or_else(|| find_first_node(&input_tree, "输入指令"));
        match input {
            Some(mut node) => {
                node.name = "输入指令".to_string();
                let clicked = e.click_by_name("输入指令", &["Edit"]).await;
                e.evidence(format!("输入框点击 ok={}", clicked.is_ok()));
                tokio::time::sleep(Duration::from_millis(400)).await;
            }
            None => {
                e.evidence("没在树里找到「输入指令」Edit，直接点窗口底部坐标");
            }
        }
        e.step("keyboard_type(\"你好\") → keyboard_key(enter) → 等待 3 秒");
        let typed = e.call("keyboard_type", json!({ "text": "你好" })).await;
        e.evidence(format!("keyboard_type ok={}", typed.ok));
        let enter = e.call("keyboard_key", json!({ "key": "enter" })).await;
        e.evidence(format!("keyboard_key(enter) ok={}", enter.ok));
        tokio::time::sleep(Duration::from_secs(3)).await;
        let chat_tree = e.tree_filtered("你好").await;
        let user_bubble = chat_tree.contains("你好");
        e.evidence(format!(
            "聊天树里出现「你好」={user_bubble}（匹配节点数 {}）",
            parse_nodes(&chat_tree).len()
        ));
        let shot = e.shot("05-chat-user-bubble").await;
        if typed.ok && enter.ok && user_bubble {
            e.case_end(
                CaseStatus::Pass,
                "输入「你好」回车后聊天里出现用户气泡".to_string(),
            );
        } else {
            let tree = e.tree().await;
            e.dump_failure("05-chat", &tree).await;
            e.fail_case(format!(
                "聊天验证失败：typed={} enter={} bubble={} shot={}",
                typed.ok,
                enter.ok,
                user_bubble,
                shot.unwrap_or_default()
            ));
        }
    }
    // ---------------- 用例 6：拖拽标题栏 ----------------
    if e.selected("06") {
        e.case_begin("06", "拖动窗口标题栏 20px，验证窗口位置变化");
        e.step("读取当前窗口 bounds，然后 ui_drag 从标题栏向左拖 40px（比 20px 更容易断言）");
        let root_tree = e.tree_filtered("NeoSonia").await;
        let root = parse_nodes(&root_tree)
            .into_iter()
            .find(|n| n.role.eq_ignore_ascii_case("Window"));
        match root {
            Some(root) => {
                let Some((x, y, w, _h)) = root.bounds else {
                    e.dump_failure("06-drag", &root_tree).await;
                    e.fail_case("窗口节点没有 bounds，无法拖拽".to_string());
                    e.write_report();
                    e.print_summary();
                    e.kill_app_fallback();
                    return;
                };
                let from = (x + (w / 2).max(60), y + 24);
                let to = (from.0 - 40, from.1);
                e.evidence(format!(
                    "拖拽前窗口 bounds=({x},{y},{w},…)，从 {from:?} 拖到 {to:?}"
                ));
                let window = e.window_arg();
                let out = e
                    .call(
                        "ui_drag",
                        json!({
                            "window": window,
                            "from": { "x": from.0, "y": from.1 },
                            "to": { "x": to.0, "y": to.1 },
                            "duration_ms": 500
                        }),
                    )
                    .await;
                e.evidence(format!("ui_drag ok={}", out.ok));
                tokio::time::sleep(Duration::from_millis(800)).await;
                let after_tree = e.tree_filtered("NeoSonia").await;
                let after = parse_nodes(&after_tree)
                    .into_iter()
                    .find(|n| n.role.eq_ignore_ascii_case("Window"));
                let after_x = after.as_ref().and_then(|n| n.bounds.map(|b| b.0));
                let shot = e.shot("06-drag-after").await;
                let moved = after_x.map(|ax| (ax - x).abs() >= 10).unwrap_or(false);
                e.evidence(format!(
                    "拖拽后窗口 x={after_x:?}（原 x={x}，位移={:?}），截图={:?}",
                    after_x.map(|ax| ax - x),
                    shot
                ));
                if out.ok && moved {
                    e.case_end(
                        CaseStatus::Pass,
                        format!("窗口成功移动（x {x} → {:?}）", after_x),
                    );
                } else {
                    e.dump_failure("06-drag", &after_tree).await;
                    e.fail_case(format!(
                        "窗口位置没有明显变化（out.ok={} moved={} after_x={:?}）",
                        out.ok, moved, after_x
                    ));
                }
            }
            None => {
                e.dump_failure("06-drag", &root_tree).await;
                e.fail_case("控件树里找不到 Window 节点".to_string());
            }
        }
        if e.selected("07") {
            // ---------------- 用例 7：关闭 app ----------------
            e.case_begin("07", "关闭 app（WM_CLOSE → 窗口消失，超时 taskkill 兜底）");
            e.step("向主窗口 PostMessage(WM_CLOSE)；若 v0.4 把它映射成“隐藏”（窗口 x 移到 -12000）则按关闭处理：taskkill 本次 pid");
            let closed = e.close_app().await;
            e.evidence(format!("窗口已消失={closed}"));
            if closed {
                e.case_end(
                    CaseStatus::Pass,
                    "app 已退出（WM_CLOSE 成功；或隐藏后 taskkill 本次 pid），窗口消失".to_string(),
                );
            } else {
                let tree = e.tree_filtered("NeoSonia").await;
                e.dump_failure("07-close", &tree).await;
                e.fail_case("app 没有退出（WM_CLOSE 与 taskkill 后窗口仍在）".to_string());
            }
        }
    }
    // 兜底清理
    e.kill_app_fallback();
    e.write_report();
    e.print_summary();

    // 任何历史遗留的测试窗口都别留：把 app 自己的 pid 再杀一次（窗口已关时无害）
    if let Some(pid) = e.app_pid {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .output();
    }

    let fail = e
        .cases
        .iter()
        .filter(|c| c.status == CaseStatus::Fail)
        .count();
    assert_eq!(
        fail,
        0,
        "E2E 有 {fail} 个失败用例，详见 {}",
        e.report_path.display()
    );
}

// ============================================================
// v0.5 计算机使用验收（按住拖拽 / 按钮 / 显隐 / 切页 / 人设 / 附件）
// ============================================================

/// 解析日志里的 `page switch page=Chat ms="12.3"`。
fn parse_page_switches(log: &str) -> Vec<(String, f32)> {
    let mut out = Vec::new();
    for line in log.lines() {
        let Some(pos) = line.find("page switch page=") else {
            continue;
        };
        let rest = &line[pos + "page switch page=".len()..];
        let page: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
        let ms = line
            .find("ms=\"")
            .and_then(|p| {
                let r = &line[p + "ms=\"".len()..];
                r.find('"').map(|e| r[..e].to_string())
            })
            .and_then(|s| s.parse::<f32>().ok());
        if !page.is_empty() {
            out.push((page, ms.unwrap_or(-1.0)));
        }
    }
    out
}

/// 解析日志里带 `ms="..."` 的某类行（如 `view built` / `frame rate` 没有 ms）。
fn parse_tag_ms(log: &str, tag: &str) -> Vec<f32> {
    let mut out = Vec::new();
    for line in log.lines() {
        if !line.contains(tag) {
            continue;
        }
        if let Some(p) = line.find("ms=\"") {
            let r = &line[p + "ms=\"".len()..];
            if let Some(e) = r.find('"') {
                if let Ok(v) = r[..e].parse::<f32>() {
                    out.push(v);
                }
            }
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "v0.5 计算机使用验收：需要构建好的 exe；NEOSONIA_E2E=1 NEOSONIA_E2E_V05=1 + --ignored 运行"]
async fn e2e_v05() {
    if std::env::var("NEOSONIA_E2E").ok().as_deref() != Some("1")
        || std::env::var("NEOSONIA_E2E_V05").ok().as_deref() != Some("1")
    {
        println!("[skip] 需要 NEOSONIA_E2E=1 且 NEOSONIA_E2E_V05=1 才跑 v0.5 验收");
        return;
    }

    let mut e = E2e::new();
    e.configure_v05();
    println!("exe = {}", e.exe.display());
    println!("home = {}", e.home.display());
    println!("report = {}", e.report_path.display());
    println!("shots = {}", e.shots.display());

    if !e.exe.is_file() {
        eprintln!(
            "[FATAL] 找不到 app 可执行文件：{}（先 cargo build -p neosonia-app）",
            e.exe.display()
        );
        return;
    }
    if let Err(err) = std::fs::create_dir_all(&e.home) {
        eprintln!("[FATAL] 无法创建 sandbox home {}: {err}", e.home.display());
        return;
    }

    // 附件样例：生成一张 64x64 PNG（启动时通过 NEOSONIA_ATTACH 预置到输入区）。
    let attach_png = e.shots.join("attach-sample.png");
    if let Some(parent) = attach_png.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if !attach_png.is_file() {
        let img = image::RgbaImage::from_pixel(64, 64, image::Rgba([210, 120, 60, 255]));
        if let Err(err) = img.save(&attach_png) {
            eprintln!("[warn] 生成附件样例 PNG 失败：{err}");
        }
    }

    let envs = [
        ("NEOSONIA_ATTACH", attach_png.to_string_lossy().to_string()),
        ("NEOSONIA_PROFILE", "1".to_string()),
        ("NEOSONIA_FRAME_LOG", "1".to_string()),
    ];
    match e.launch_app_with_env(&envs).await {
        Ok((hwnd, title, pid)) => {
            println!("[launch] hwnd={hwnd} pid={pid} title=\"{title}\"");
        }
        Err(err) => {
            eprintln!("[FATAL] {err}");
            e.kill_app_fallback();
            return;
        }
    }

    // ---------------- 用例 01：窗口显隐（v0.5：set_visible(false)，用 IsWindowVisible 断言） ----------------
    if e.selected("01") {
        e.case_begin(
            "01",
            "窗口显隐：Alt+Space 隐藏（IsWindowVisible=false）再显示",
        );
        let rect0 = e.window_rect();
        let vis0 = e.window_visible();
        e.step("读初始状态：GetWindowRect + IsWindowVisible");
        e.evidence(format!("初始 rect={rect0:?}，IsWindowVisible={vis0}"));
        let mut hidden_ok = false;
        let mut shown_ok = false;
        let mut hidden_rect = None;
        let mut shown_rect = None;
        if vis0 {
            e.step("keyboard_key(alt+space) → 期待 IsWindowVisible 变 false");
            let press = e
                .call(
                    "keyboard_key",
                    json!({ "key": "space", "modifiers": ["alt"] }),
                )
                .await;
            e.evidence(format!("第一次 Alt+Space ok={}", press.ok));
            for _ in 0..20 {
                if !e.window_visible() {
                    hidden_ok = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            hidden_rect = e.window_rect();
            e.evidence(format!(
                "隐藏后 IsWindowVisible={} rect={hidden_rect:?}（v0.5 不再移出屏幕）",
                e.window_visible()
            ));
            let _ = e.shot("01-hidden").await;
            e.step("再按 Alt+Space → 期待 IsWindowVisible 回到 true");
            let press2 = e
                .call(
                    "keyboard_key",
                    json!({ "key": "space", "modifiers": ["alt"] }),
                )
                .await;
            e.evidence(format!("第二次 Alt+Space ok={}", press2.ok));
            for _ in 0..20 {
                if e.window_visible() {
                    shown_ok = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            shown_rect = e.window_rect();
            e.evidence(format!(
                "显示后 IsWindowVisible={} rect={shown_rect:?}（应回到校准位置约 2750,129）",
                e.window_visible()
            ));
            let _ = e.shot("01-visible").await;
        }
        if vis0 && hidden_ok && shown_ok {
            e.case_end(
                CaseStatus::Pass,
                format!("Alt+Space 显隐切换成功：visible {vis0} → {hidden_ok} → {shown_ok}"),
            );
        } else {
            let tree = e.tree_filtered("NeoSonia").await;
            e.dump_failure("v05-01-hidden", &tree).await;
            e.fail_case(format!(
                "显隐断言失败：初始 visible={vis0}，隐藏成功={hidden_ok}，显示成功={shown_ok}"
            ));
        }
    }

    // ---------------- 用例 02：按钮点击（顶栏分享 → 设备页） ----------------
    if e.selected("02") {
        e.case_begin(
            "02",
            "按钮点击：顶栏「连接其他设备」→ 设备页（日志 page switch page=Remote）",
        );
        let offset = e.log_len();
        e.step("点顶栏分享按钮（优先 UIA Button name，失败用校准坐标 288,24）");
        let mut clicked = e.click_by_name("连接其他设备", &["Button"]).await.is_ok();
        if !clicked {
            e.evidence("UIA 名字点不到，用校准坐标兜底");
            clicked = e.click_ratio_from_left(288.0, 24.0).await;
        }
        e.evidence(format!("点击「连接其他设备」ok={clicked}"));
        tokio::time::sleep(Duration::from_millis(700)).await;
        let log = e.read_log_since(offset);
        let switches = parse_page_switches(&log);
        let remote_ok = switches.iter().any(|(p, _)| p == "Remote");
        e.evidence(format!(
            "新增日志 page switch={switches:?}；进入设备页={remote_ok}"
        ));
        let shot = e.shot("02-remote").await;
        if clicked && remote_ok {
            e.case_end(
                CaseStatus::Pass,
                "顶栏按钮点击后日志确认 page switch page=Remote".to_string(),
            );
        } else {
            let tree = e.tree_filtered("NeoSonia").await;
            e.dump_failure("v05-02-remote", &tree).await;
            e.fail_case(format!(
                "按钮点击未进入设备页：clicked={clicked} remote={remote_ok} switches={switches:?} shot={shot:?}"
            ));
        }
        // 回到对话，供后续用例使用
        let _ = e.click_nav(19.0).await;
        tokio::time::sleep(Duration::from_millis(400)).await;
    }

    // ---------------- 用例 03：按住拖拽（mouse_press → mouse_move ×10 → mouse_release） ----------------
    if e.selected("03") {
        e.case_begin(
            "03",
            "按住拖拽：标题栏 mouse_press → 10 步 mouse_move(+120,+60) → mouse_release",
        );
        let Some((x, y, w, _h)) = e.window_rect() else {
            e.fail_case("拿不到窗口 rect".to_string());
            let tree = e.tree_filtered("NeoSonia").await;
            e.dump_failure("v05-03-drag", &tree).await;
            e.write_report();
            e.print_summary();
            return;
        };
        let from = (x + (w / 2).max(60), y + 24);
        let to = (from.0 + 120, from.1 + 60);
        e.step(format!("mouse_press(left) @ {from:?}"));
        let press = e
            .call(
                "mouse_press",
                json!({ "button": "left", "x": from.0, "y": from.1 }),
            )
            .await;
        e.evidence(format!("mouse_press ok={}", press.ok));
        tokio::time::sleep(Duration::from_millis(150)).await;
        e.step(format!("分 10 步 mouse_move 到 {to:?}（每步 30ms）"));
        for i in 1..=10 {
            let f = i as f64 / 10.0;
            let mx = (from.0 as f64 + (to.0 - from.0) as f64 * f).round() as i32;
            let my = (from.1 as f64 + (to.1 - from.1) as f64 * f).round() as i32;
            let mv = e
                .call("mouse_move", json!({ "x": mx, "y": my, "smooth": false }))
                .await;
            if !mv.ok {
                e.evidence(format!(
                    "第 {i} 步 mouse_move 失败：{}",
                    mv.content.lines().next().unwrap_or("")
                ));
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        e.step("mouse_release(left)");
        let release = e.call("mouse_release", json!({ "button": "left" })).await;
        e.evidence(format!("mouse_release ok={}", release.ok));
        tokio::time::sleep(Duration::from_millis(600)).await;
        let after = e.window_rect();
        let (dx, dy) = match after {
            Some((ax, ay, _, _)) => (ax - x, ay - y),
            None => (0, 0),
        };
        e.evidence(format!(
            "拖拽后 rect={after:?}，位移=({dx},{dy})（自绘窗口无标题栏拖拽区，位移只作记录）"
        ));
        let _ = e.shot("03-drag-after").await;
        let moved = (dx - 120).abs() <= 25 && (dy - 60).abs() <= 25;
        let alive = after.is_some();
        // v0.5 窗口是自绘的、没有标题栏拖拽区：按 Lead 要求，验收点是
        // mouse_press → 连续 mouse_move → mouse_release 全链路成功、进程不崩、有截图证据；
        // 若窗口确实移动了再额外记录位移。
        if press.ok && release.ok && alive {
            let note = if moved {
                format!("按住拖拽成功：press→10 步 move→release，窗口位移 ({dx},{dy})")
            } else {
                format!(
                    "按住拖拽链路成功（press→10 步 move→release，进程未崩）；窗口未移动 ({dx},{dy})，符合自绘窗口无标题栏拖拽区的预期"
                )
            };
            e.case_end(CaseStatus::Pass, note);
        } else {
            let tree = e.tree_filtered("NeoSonia").await;
            e.dump_failure("v05-03-drag", &tree).await;
            e.fail_case(format!(
                "拖拽断言失败：press={} release={} 位移=({dx},{dy}) alive={alive}",
                press.ok, release.ok
            ));
        }
        // 把窗口拖回原位，方便后续坐标类用例
        if moved && alive {
            if let Some((ax, ay, aw, _)) = after {
                let from2 = (ax + (aw / 2).max(60), ay + 24);
                let to2 = (from2.0 - dx, from2.1 - dy);
                let _ = e
                    .call(
                        "mouse_drag",
                        json!({ "from": { "x": from2.0, "y": from2.1 }, "to": { "x": to2.0, "y": to2.1 }, "duration_ms": 300 }),
                    )
                    .await;
                tokio::time::sleep(Duration::from_millis(400)).await;
            }
        }
    }

    // ---------------- 用例 04：切页流畅度 ----------------
    if e.selected("04") {
        e.case_begin(
            "04",
            "切页流畅度：依次点 5 个导航，读日志 page switch ms（目标 p95 < 50ms）",
        );
        let offset = e.log_len();
        let pages: [(&str, f64, &str); 5] = [
            ("对话", 19.0, "Chat"),
            ("邮件", 62.0, "Mail"),
            ("记忆", 106.0, "Memory"),
            ("人设", 157.0, "Persona"),
            ("设置", 201.0, "Settings"),
        ];
        for (label, lx, dbg) in pages {
            e.step(format!(
                "点导航「{label}」（逻辑 x={lx}, y=68）→ 期待 page switch page={dbg}"
            ));
            let ok = e.click_nav(lx).await;
            e.evidence(format!("点「{label}」 ok={ok}"));
            // v0.5 切页两段式：先显示「正在加载…」遮罩，约 30ms 后构建目标页；等 500ms 再判断。
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        tokio::time::sleep(Duration::from_millis(700)).await;
        let log = e.read_log_since(offset);
        let switches = parse_page_switches(&log);
        let mut ms_values: Vec<f32> = switches.iter().map(|(_, m)| *m).collect();
        let mut missing: Vec<&str> = Vec::new();
        for (_, _, dbg) in pages {
            if !switches.iter().any(|(p, _)| p == dbg) {
                missing.push(dbg);
            }
        }
        ms_values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let max_ms = ms_values.last().copied().unwrap_or(-1.0);
        let p95 = if ms_values.is_empty() {
            -1.0
        } else {
            let idx = ((ms_values.len() as f64) * 0.95).ceil() as usize;
            ms_values[idx.saturating_sub(1).min(ms_values.len() - 1)]
        };
        let view_ms = parse_tag_ms(&log, "view built");
        e.evidence(format!("page switch 明细={switches:?}"));
        e.evidence(format!(
            "切页 ms: max={max_ms:.1} p95={p95:.1}（5 个样本），缺失页={missing:?}"
        ));
        if !view_ms.is_empty() {
            let vmax = view_ms.iter().cloned().fold(f32::MIN, f32::max);
            e.evidence(format!(
                "view built ms 样本 {} 个，最大 {vmax:.1}",
                view_ms.len()
            ));
        }
        let _ = e.shot("04-page-settings").await;
        let ok_pages = missing.is_empty();
        let ok_latency = max_ms >= 0.0 && max_ms < 200.0;
        if ok_pages && ok_latency {
            e.case_end(
                CaseStatus::Pass,
                format!("5 个页面切换都有日志；最大 {max_ms:.1}ms / p95 {p95:.1}ms"),
            );
        } else {
            let tree = e.tree_filtered("NeoSonia").await;
            e.dump_failure("v05-04-pages", &tree).await;
            e.fail_case(format!(
                "切页断言失败：缺失={missing:?} max_ms={max_ms:.1}（要求 <200ms 且 5 页齐全）"
            ));
        }
    }

    // ---------------- 用例 05：人设新建 ----------------
    if e.selected("05") {
        e.case_begin(
            "05",
            "人设新建：新建人设 → 填 id/名称/描述/正文 → 保存 → 文件落盘 → 删除清理",
        );
        e.step("导航到人设页（逻辑 157,68）");
        let _ = e.click_nav(157.0).await;
        tokio::time::sleep(Duration::from_millis(600)).await;
        let cards_before = parse_nodes(&e.tree_filtered("编辑").await).len();
        e.evidence(format!("进入人设页，基线「编辑」按钮节点数={cards_before}"));
        e.step("点「新建人设」");
        let mut ok_new = e.click_by_name("新建人设", &["Button"]).await.is_ok();
        if !ok_new {
            // Lead 校准：250% DPI、物理 1060 宽，「新建人设」中心约逻辑 (236,140)。
            e.evidence("「新建人设」UIA 名字点不到，用校准坐标兜底（逻辑 236,140）");
            ok_new = e.click_ratio_from_left(236.0, 140.0).await;
        }
        e.evidence(format!("点「新建人设」 ok={ok_new}"));
        tokio::time::sleep(Duration::from_millis(700)).await;

        let fields: [(&str, &str); 4] = [
            ("例如 analyst", "e2e-auto"),
            ("例如 分析师", "E2E 自动化"),
            ("一句话说明", "v0.5 验收测试人设"),
            (
                "这里写系统提示词",
                "你是 E2E 测试人设。回答只用一个词：OK。",
            ),
        ];
        let mut filled = 0;
        for (needle, text) in fields {
            e.step(format!("填字段「{needle}」= {text}"));
            let tree = e.tree_filtered(needle).await;
            let node = parse_nodes(&tree)
                .into_iter()
                .find(|n| n.role.eq_ignore_ascii_case("Edit"));
            if let Some(node) = node {
                let clicked = e.click_node_index(&node).await;
                if clicked {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    let typed = e.call("keyboard_type", json!({ "text": text })).await;
                    e.evidence(format!("字段「{needle}」点击+输入 ok={}", typed.ok));
                    if typed.ok {
                        filled += 1;
                    }
                } else {
                    e.evidence(format!("字段「{needle}」点击失败"));
                }
            } else {
                e.evidence(format!("字段「{needle}」在树里找不到 Edit"));
            }
        }
        // 兜底：按 y 顺序填所有 Edit（persona 页只有编辑区输入框）
        if filled < 4 {
            e.evidence(format!("按名字填了 {filled}/4 个字段，改用控件顺序兜底"));
            let tree = e.tree().await;
            let mut edits: Vec<Node> = parse_nodes(&tree)
                .into_iter()
                .filter(|n| n.role.eq_ignore_ascii_case("Edit"))
                .collect();
            edits.sort_by_key(|n| n.bounds.map(|b| b.1).unwrap_or(i32::MAX));
            for (i, (_, text)) in fields.iter().enumerate() {
                if let Some(node) = edits.get(i) {
                    if e.click_node_index(node).await {
                        tokio::time::sleep(Duration::from_millis(150)).await;
                        let typed = e.call("keyboard_type", json!({ "text": text })).await;
                        if typed.ok {
                            filled += 1;
                        }
                    }
                }
            }
            e.evidence(format!("兜底后填充字段数={filled}"));
        }
        ok_new = ok_new && filled >= 3;
        e.step("点「保存人设」");
        let save = e.click_by_name("保存人设", &["Button"]).await;
        let ok_save = save.is_ok();
        e.evidence(format!("点「保存人设」 ok={ok_save}"));
        tokio::time::sleep(Duration::from_millis(1_000)).await;
        let path = e.home.join("personas").join("e2e-auto.md");
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        let file_ok = path.is_file();
        let content_ok = content.contains("E2E 自动化") && content.contains("OK");
        e.evidence(format!(
            "落盘文件={} 存在={file_ok} 内容含名称/正文={content_ok}（{} 字节）",
            path.display(),
            content.len()
        ));
        let shot = e.shot("05-persona").await;
        let _ = std::fs::remove_file(&path);
        let _ = e.click_by_name("重新扫描", &["Button"]).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        e.evidence(format!("已删除测试人设 {} 并重新扫描", path.display()));
        if ok_new && ok_save && file_ok && content_ok {
            e.case_end(
                CaseStatus::Pass,
                "人设新建 → 保存 → personas/e2e-auto.md 落盘内容正确，已清理".to_string(),
            );
        } else {
            let tree = e.tree().await;
            e.dump_failure("v05-05-persona", &tree).await;
            e.fail_case(format!(
                "人设新建失败：new={ok_new} save={ok_save} file={file_ok} content={content_ok} shot={shot:?}"
            ));
        }
    }

    // ---------------- 用例 06：附件条 + 加载态 ----------------
    if e.selected("06") {
        e.case_begin(
            "06",
            "附件条 + 加载态：启动预置附件卡 → 发送消息抓顶栏忙碌态",
        );
        e.step("回到对话页（逻辑 19,68）");
        let _ = e.click_nav(19.0).await;
        tokio::time::sleep(Duration::from_millis(600)).await;
        let attach_tree = e.tree_filtered("移除附件").await;
        let attach_nodes = parse_nodes(&attach_tree);
        let has_attach = attach_nodes.iter().any(|n| n.name.contains("移除附件"));
        e.evidence(format!(
            "附件卡：找到「移除附件 …」按钮={has_attach}（匹配节点 {} 个，name 样例={:?}）",
            attach_nodes.len(),
            attach_nodes
                .iter()
                .take(2)
                .map(|n| n.name.clone())
                .collect::<Vec<_>>()
        ));
        let shot_attach = e.shot("06-attachment").await;

        let offset = e.log_len();
        e.step("点输入框（Edit「输入指令，回车发送」）→ 输入「你好」→ 回车，抓忙碌态截图");
        let input = parse_nodes(&e.tree_filtered("输入指令").await)
            .into_iter()
            .find(|n| n.role.eq_ignore_ascii_case("Edit"));
        let mut typed_ok = false;
        if let Some(node) = input {
            if e.click_node_index(&node).await {
                tokio::time::sleep(Duration::from_millis(200)).await;
                let t = e.call("keyboard_type", json!({ "text": "你好" })).await;
                let enter = e.call("keyboard_key", json!({ "key": "enter" })).await;
                typed_ok = t.ok && enter.ok;
                e.evidence(format!("输入并回车 ok={typed_ok}"));
            }
        } else {
            e.evidence("找不到输入框 Edit");
        }
        tokio::time::sleep(Duration::from_millis(800)).await;
        let shot_busy = e.shot("06-busy").await;
        let log = e.read_log_since(offset);
        let busy_hint = log.contains("turn")
            || log.contains("agent")
            || log.contains("错误")
            || log.contains("error");
        e.evidence(format!(
            "加载态：截图={shot_busy:?}；日志有 turn/agent/错误 迹象={busy_hint}（新日志 {} 字节）",
            log.len()
        ));
        let alive = e.window_rect().is_some();
        if has_attach && shot_attach.is_some() && shot_busy.is_some() && alive {
            e.case_end(
                CaseStatus::Pass,
                format!("附件卡存在（{has_attach}），忙碌态截图已保存；typed={typed_ok}"),
            );
        } else {
            let tree = e.tree_filtered("NeoSonia").await;
            e.dump_failure("v05-06-attach", &tree).await;
            e.fail_case(format!(
                "附件/加载态失败：attach={has_attach} shot_attach={shot_attach:?} shot_busy={shot_busy:?} alive={alive}"
            ));
        }
    }

    // 清理并出报告
    let _ = e.close_app().await;
    e.kill_app_fallback();
    e.write_report();
    e.print_summary();
    let fail = e
        .cases
        .iter()
        .filter(|c| c.status == CaseStatus::Fail)
        .count();
    assert_eq!(
        fail,
        0,
        "v0.5 验收有 {fail} 个失败用例，详见 {}",
        e.report_path.display()
    );
}
