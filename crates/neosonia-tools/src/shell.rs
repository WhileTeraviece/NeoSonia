//! 命令执行工具：长任务、流式输出、后台任务、取消、超时。
//!
//! 核心行为（按 docs/contracts.md 6.2）：
//! - Windows 默认 `pwsh -NoProfile -NonInteractive -Command`，无 pwsh 回退
//!   `powershell -NoProfile -NonInteractive -Command`，再回退 `cmd /C`；
//! - Linux 用 `bash -lc`，无 bash 用 `sh -lc`；
//! - 默认超时取 `ToolsSettings::command_timeout_secs`（1800 秒，0 = 不超时，只受用户取消约束）；
//! - stdout/stderr 用两个读任务流式回传，并写入 200KB 环形缓冲；
//! - 后台任务进全局任务表，可 list / read / kill / send_input；
//! - 进程组：Windows `CREATE_NEW_PROCESS_GROUP` + `taskkill /T /F`，
//!   Linux `process_group(0)` + `kill -TERM/-KILL -- -pid`。

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use neosonia_core::config::ShellKind;
use neosonia_core::model::{ToolCtx, ToolOutcome};
use neosonia_core::registry::ToolRegistry;
use neosonia_core::util::{human_duration_ms, middle_truncate, truncate_chars};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::{arg_bool, arg_f64, arg_str, arg_str_req, arg_u64, io_error_hint, resolve_read_path};

/// 每个任务的输出环形缓冲上限（字符数，约 200KB 英文）。
const RING_CHAR_LIMIT: u64 = 200_000;
/// 任务表最多保留多少个历史任务。
const MAX_JOBS: usize = 64;
/// 命令最长允许的超时（秒），防止参数误传成天文数字。
const MAX_TIMEOUT_SECS: u64 = 7 * 24 * 3600;

// ============================================================
// 输出缓冲
// ============================================================

struct OutChunk {
    start: u64,
    stderr: bool,
    text: String,
}

/// stdout / stderr 合并的顺序缓冲；超出上限时丢弃最老的块。
#[derive(Default)]
struct OutputBuf {
    chunks: VecDeque<OutChunk>,
    total: u64,
}

impl OutputBuf {
    fn push(&mut self, stderr: bool, text: &str) {
        if text.is_empty() {
            return;
        }
        let len = text.chars().count() as u64;
        self.chunks.push_back(OutChunk {
            start: self.total,
            stderr,
            text: text.to_string(),
        });
        self.total += len;
        while self.chunks.len() > 1 {
            let front_start = self.chunks.front().map(|c| c.start).unwrap_or(self.total);
            if self.total - front_start > RING_CHAR_LIMIT {
                self.chunks.pop_front();
            } else {
                break;
            }
        }
    }

    fn base(&self) -> u64 {
        self.chunks.front().map(|c| c.start).unwrap_or(self.total)
    }

    /// 读取 `cursor` 之后的新内容，返回 (文本, 新 cursor)。
    fn read_from(&self, cursor: u64) -> (String, u64) {
        let base = self.base();
        let mut out = String::new();
        let mut effective = cursor;
        if effective < base {
            out.push_str("[... 更早的输出已超出 200KB 缓冲，已丢弃 ...]\n");
            effective = base;
        }
        for chunk in &self.chunks {
            let end = chunk.start + chunk.text.chars().count() as u64;
            if end <= effective {
                continue;
            }
            if effective > chunk.start {
                let skip = (effective - chunk.start) as usize;
                out.extend(chunk.text.chars().skip(skip));
            } else {
                out.push_str(&chunk.text);
            }
        }
        (out, self.total)
    }

    /// 按流拆开（用于返回完整结果）。
    fn split_streams(&self) -> (String, String) {
        let mut out = String::new();
        let mut err = String::new();
        for chunk in &self.chunks {
            if chunk.stderr {
                err.push_str(&chunk.text);
            } else {
                out.push_str(&chunk.text);
            }
        }
        (out, err)
    }
}

/// 子进程输出解码：优先 UTF-8（允许跨块的不完整字节），失败时按 GBK 兜底
/// （Windows PowerShell 5.1 重定向输出常是系统代码页）。
#[derive(Default)]
struct StreamDecoder {
    pending: Vec<u8>,
}

impl StreamDecoder {
    fn push(&mut self, bytes: &[u8]) -> String {
        if bytes.is_empty() {
            return String::new();
        }
        self.pending.extend_from_slice(bytes);
        match std::str::from_utf8(&self.pending) {
            Ok(s) => {
                let out = s.to_string();
                self.pending.clear();
                out
            }
            // 末尾是不完整的多字节序列：等下一块，不要吐替换字符。
            Err(e) if e.error_len().is_none() && self.pending.len() <= 8 => String::new(),
            Err(e) => {
                let valid = e.valid_up_to();
                let (prefix, rest) = self.pending.split_at(valid);
                let mut out = String::from_utf8_lossy(prefix).into_owned();
                let (decoded, _, had_errors) = encoding_rs::GBK.decode(rest);
                if had_errors {
                    out.push_str(&String::from_utf8_lossy(rest));
                } else {
                    out.push_str(&decoded);
                }
                self.pending.clear();
                out
            }
        }
    }

    fn flush(&mut self) -> String {
        if self.pending.is_empty() {
            return String::new();
        }
        let bytes = std::mem::take(&mut self.pending);
        let (decoded, _, had_errors) = encoding_rs::GBK.decode(&bytes);
        if had_errors {
            String::from_utf8_lossy(&bytes).into_owned()
        } else {
            decoded.into_owned()
        }
    }
}

// ============================================================
// 任务表
// ============================================================

#[derive(Clone, Copy, PartialEq, Eq)]
enum JobStatus {
    Running,
    Finished,
}

/// 任务对外的状态文案：running / cancelling / finished / cancelled / failed。
fn job_state_label(status: JobStatus, killed: bool, exit_code: Option<i32>) -> &'static str {
    match status {
        JobStatus::Running => {
            if killed {
                "cancelling"
            } else {
                "running"
            }
        }
        JobStatus::Finished => {
            if killed {
                "cancelled"
            } else if exit_code == Some(0) {
                "finished"
            } else {
                "failed"
            }
        }
    }
}

/// 取文本最后 n 行（job_status 输出尾部用）。
fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

struct JobState {
    id: String,
    command: String,
    shell_desc: String,
    cwd: String,
    background: bool,
    /// background_task 指定的任务名（list/status 展示用）。
    label: Option<String>,
    started: Instant,
    pid: u32,
    status: JobStatus,
    exit_code: Option<i32>,
    killed: bool,
    output: Arc<Mutex<OutputBuf>>,
    read_cursor: u64,
    stdin: Option<tokio::process::ChildStdin>,
    /// 还没结束的输出读任务数（stdout/stderr 各一个）。进程退出后要等它们把
    /// 管道里的剩余字节读完，才能报告完整输出。
    pumps_left: Arc<AtomicUsize>,
}

#[derive(Default)]
struct JobStore {
    jobs: BTreeMap<String, Arc<Mutex<JobState>>>,
}

static JOBS: Lazy<Mutex<JobStore>> = Lazy::new(|| Mutex::new(JobStore::default()));

fn register_job(job: Arc<Mutex<JobState>>) {
    let id = job.lock().id.clone();
    let mut store = JOBS.lock();
    store.jobs.insert(id, job);
    // 超出上限时清理最老的已结束任务。
    while store.jobs.len() > MAX_JOBS {
        let oldest_finished = store
            .jobs
            .iter()
            .find(|(_, j)| j.lock().status == JobStatus::Finished)
            .map(|(id, _)| id.clone());
        match oldest_finished {
            Some(id) => {
                store.jobs.remove(&id);
            }
            None => break,
        }
    }
}

fn get_job(id: &str) -> Option<Arc<Mutex<JobState>>> {
    JOBS.lock().jobs.get(id).cloned()
}

fn is_job_finished(id: &str) -> bool {
    get_job(id)
        .map(|j| j.lock().status == JobStatus::Finished)
        .unwrap_or(true)
}

fn mark_killed(id: &str) {
    if let Some(job) = get_job(id) {
        job.lock().killed = true;
    }
}

/// 任务状态快照（拿到后不持锁，方便在 await 之间使用）。
#[derive(Clone)]
struct JobView {
    id: String,
    command: String,
    shell_desc: String,
    cwd: String,
    background: bool,
    label: Option<String>,
    pid: u32,
    status: JobStatus,
    exit_code: Option<i32>,
    killed: bool,
    elapsed: Duration,
    output: Arc<Mutex<OutputBuf>>,
}

fn view_job(id: &str) -> Option<JobView> {
    let job = get_job(id)?;
    let guard = job.lock();
    Some(JobView {
        id: guard.id.clone(),
        command: guard.command.clone(),
        shell_desc: guard.shell_desc.clone(),
        cwd: guard.cwd.clone(),
        background: guard.background,
        label: guard.label.clone(),
        pid: guard.pid,
        status: guard.status,
        exit_code: guard.exit_code,
        killed: guard.killed,
        elapsed: guard.started.elapsed(),
        output: guard.output.clone(),
    })
}

/// 等 stdout/stderr 读任务把管道剩余内容读完（最多 `timeout`）。
async fn wait_pumps_left(pumps: &Arc<AtomicUsize>, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while pumps.load(Ordering::SeqCst) > 0 {
        if Instant::now() >= deadline {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ============================================================
// shell 选择
// ============================================================

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ShellChoice {
    Auto,
    Pwsh,
    WindowsPowerShell,
    Cmd,
    Bash,
    Sh,
}

fn parse_shell_choice(param: Option<&str>, configured: ShellKind) -> Result<ShellChoice, String> {
    let from_param = match param.map(|s| s.trim().to_ascii_lowercase()) {
        None => None,
        Some(s) if s.is_empty() || s == "auto" => Some(ShellChoice::Auto),
        Some(s) => match s.as_str() {
            "pwsh" | "pwsh7" | "powershell7" | "ps7" => Some(ShellChoice::Pwsh),
            "powershell" | "ps" | "ps5" | "windowspowershell" => Some(ShellChoice::WindowsPowerShell),
            "cmd" | "cmd.exe" | "batch" => Some(ShellChoice::Cmd),
            "bash" | "sh" => {
                if s == "bash" {
                    Some(ShellChoice::Bash)
                } else {
                    Some(ShellChoice::Sh)
                }
            }
            other => {
                return Err(format!(
                    "不认识的 shell：{other:?}。可用值：auto / pwsh / powershell / cmd / bash / sh。"
                ))
            }
        },
    };
    if let Some(choice) = from_param {
        return Ok(choice);
    }
    Ok(match configured {
        ShellKind::Auto => ShellChoice::Auto,
        ShellKind::Powershell => ShellChoice::WindowsPowerShell,
        ShellKind::Cmd => ShellChoice::Cmd,
        ShellKind::Bash => ShellChoice::Bash,
        ShellKind::Sh => ShellChoice::Sh,
    })
}

struct ShellCmd {
    program: PathBuf,
    args: Vec<String>,
    desc: String,
}

#[cfg(windows)]
fn find_in_path(names: &[&str]) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        for name in names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(windows)]
fn find_pwsh() -> Option<PathBuf> {
    if let Some(p) = find_in_path(&["pwsh.exe"]) {
        return Some(p);
    }
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(pf) = std::env::var_os("ProgramFiles") {
        let base = PathBuf::from(&pf);
        dirs.push(base.join("PowerShell").join("7"));
        dirs.push(base.join("PowerShell").join("7-preview"));
    }
    if let Some(la) = std::env::var_os("LOCALAPPDATA") {
        dirs.push(PathBuf::from(la).join("Microsoft").join("WindowsApps"));
    }
    dirs.push(PathBuf::from(r"C:\Program Files\PowerShell\7"));
    for dir in dirs {
        let candidate = dir.join("pwsh.exe");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(windows)]
fn find_windows_powershell() -> Option<PathBuf> {
    if let Some(p) = find_in_path(&["powershell.exe"]) {
        return Some(p);
    }
    if let Some(root) = std::env::var_os("SystemRoot") {
        let candidate = PathBuf::from(root)
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(windows)]
fn find_cmd() -> Option<PathBuf> {
    if let Some(comspec) = std::env::var_os("COMSPEC") {
        let p = PathBuf::from(comspec);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Some(root) = std::env::var_os("SystemRoot") {
        let candidate = PathBuf::from(root).join("System32").join("cmd.exe");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    find_in_path(&["cmd.exe"])
}

#[cfg(windows)]
fn build_shell(choice: ShellChoice, command: &str) -> Result<ShellCmd, String> {
    let candidates: Vec<ShellChoice> = match choice {
        ShellChoice::Auto => vec![
            ShellChoice::Pwsh,
            ShellChoice::WindowsPowerShell,
            ShellChoice::Cmd,
        ],
        ShellChoice::Bash => {
            return Err(
                "Windows 上没有 bash（除非装了 Git Bash/WSL 且手动指定路径）；请用 shell=auto / powershell / pwsh / cmd。"
                    .to_string(),
            )
        }
        ShellChoice::Sh => {
            return Err("Windows 上没有 sh；请用 shell=auto / powershell / pwsh / cmd。".to_string())
        }
        other => vec![other],
    };
    let mut tried: Vec<String> = Vec::new();
    for candidate in candidates {
        match candidate {
            ShellChoice::Pwsh => match find_pwsh() {
                Some(program) => {
                    return Ok(ShellCmd {
                        desc: format!("pwsh ({})", program.display()),
                        program,
                        args: vec![
                            "-NoProfile".into(),
                            "-NonInteractive".into(),
                            "-ExecutionPolicy".into(),
                            "Bypass".into(),
                            "-Command".into(),
                            command.to_string(),
                        ],
                    })
                }
                None => tried.push("pwsh.exe".to_string()),
            },
            ShellChoice::WindowsPowerShell => match find_windows_powershell() {
                Some(program) => {
                    return Ok(ShellCmd {
                        desc: format!("powershell ({})", program.display()),
                        program,
                        args: vec![
                            "-NoProfile".into(),
                            "-NonInteractive".into(),
                            "-ExecutionPolicy".into(),
                            "Bypass".into(),
                            "-Command".into(),
                            command.to_string(),
                        ],
                    })
                }
                None => tried.push("powershell.exe".to_string()),
            },
            ShellChoice::Cmd => match find_cmd() {
                Some(program) => {
                    return Ok(ShellCmd {
                        desc: format!("cmd ({})", program.display()),
                        program,
                        args: vec!["/C".into(), command.to_string()],
                    })
                }
                None => tried.push("cmd.exe".to_string()),
            },
            ShellChoice::Auto | ShellChoice::Bash | ShellChoice::Sh => {}
        }
    }
    Err(format!(
        "没有找到可用的 shell（尝试过：{}）。请检查 PATH 或用参数 shell 指定。",
        if tried.is_empty() {
            "无".to_string()
        } else {
            tried.join(", ")
        }
    ))
}

#[cfg(unix)]
fn find_program(names: &[&str]) -> Option<PathBuf> {
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            for name in names {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    for dir in ["/bin", "/usr/bin", "/usr/local/bin"] {
        for name in names {
            let candidate = PathBuf::from(dir).join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(unix)]
fn build_shell(choice: ShellChoice, command: &str) -> Result<ShellCmd, String> {
    match choice {
        ShellChoice::Cmd => Err("Linux 上没有 cmd；请用 shell=auto / bash / sh。".to_string()),
        ShellChoice::Pwsh => Err("Linux 上默认不使用 pwsh；请用 shell=auto / bash / sh。".to_string()),
        ShellChoice::WindowsPowerShell => {
            Err("Linux 上没有 powershell；请用 shell=auto / bash / sh。".to_string())
        }
        ShellChoice::Auto | ShellChoice::Bash => {
            if let Some(program) = find_program(&["bash"]) {
                return Ok(ShellCmd {
                    desc: format!("bash ({})", program.display()),
                    program,
                    args: vec!["-lc".into(), command.to_string()],
                });
            }
            if choice == ShellChoice::Bash {
                return Err("没有找到 bash。请安装 bash，或用 shell=sh。".to_string());
            }
            match find_program(&["sh"]) {
                Some(program) => Ok(ShellCmd {
                    desc: format!("sh ({})", program.display()),
                    program,
                    args: vec!["-lc".into(), command.to_string()],
                }),
                None => Err("没有找到 bash 也没有 sh，无法执行命令。".to_string()),
            }
        }
        ShellChoice::Sh => match find_program(&["sh"]) {
            Some(program) => Ok(ShellCmd {
                desc: format!("sh ({})", program.display()),
                program,
                args: vec!["-lc".into(), command.to_string()],
            }),
            None => Err("没有找到 sh，无法执行命令。".to_string()),
        },
    }
}

// ============================================================
// 启动 / 等待 / 终止
// ============================================================

struct StartSpec {
    command: String,
    working_dir: Option<String>,
    shell: Option<String>,
    envs: BTreeMap<String, String>,
    background: bool,
    /// 可选的人类可读任务名（background_task 用；run_command 为 None）。
    label: Option<String>,
}

fn make_command(shell: &ShellCmd, cwd: &PathBuf, envs: &BTreeMap<String, String>) -> Command {
    let mut cmd = Command::new(&shell.program);
    // Windows：CREATE_NO_WINDOW + CREATE_NEW_PROCESS_GROUP（管理员模式不闪黑窗/可整树终止）。
    crate::apply_no_window_tokio(&mut cmd);
    cmd.args(&shell.args);
    cmd.current_dir(cwd);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(false);
    // 让结构化日志/构建输出更适合被模型读取：关掉颜色与分页。
    cmd.env("TERM", "dumb");
    cmd.env("NO_COLOR", "1");
    cmd.env("CLICOLOR", "0");
    cmd.env("PYTHONUNBUFFERED", "1");
    cmd.env("CARGO_TERM_COLOR", "never");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    #[cfg(windows)]
    {
        // 标志已在 Command::new 之后统一通过 crate::apply_no_window_tokio 设置
        //（CREATE_NO_WINDOW + CREATE_NEW_PROCESS_GROUP），这里无需重复处理。
    }
    #[cfg(unix)]
    {
        // 独立进程组，kill -- -pid 能终止整棵树。
        let _ = cmd.process_group(0);
    }
    cmd
}

fn start_job(ctx: &ToolCtx, spec: StartSpec) -> Result<(String, u32), ToolOutcome> {
    // 备份区只读：命令字符串里引用备份区目录，或 working_dir 指向备份区，一律拒绝执行。
    let paths = ctx.paths();
    if crate::snapshot::backups_dir_mentioned(&spec.command, &paths.root) {
        return Err(crate::snapshot::command_backups_denied(
            &paths,
            "命令里引用了备份区目录",
        ));
    }
    let settings = ctx.settings();
    let workspace = ctx.workspace();
    if let Err(e) = std::fs::create_dir_all(&workspace) {
        return Err(ToolOutcome::err(format!(
            "工作目录不可用（{}）：{}",
            workspace.display(),
            io_error_hint(&e)
        )));
    }
    let cwd = match spec.working_dir.as_deref() {
        Some(raw) if !raw.trim().is_empty() => match resolve_read_path(ctx, raw) {
            Ok(p) => p,
            Err(e) => return Err(e),
        },
        _ => workspace,
    };
    if crate::snapshot::path_in_backups(&paths, &cwd) {
        return Err(crate::snapshot::command_backups_denied(
            &paths,
            &format!("working_dir 指向备份区目录 {}", cwd.display()),
        ));
    }
    if !cwd.is_dir() {
        return Err(ToolOutcome::err(format!(
            "working_dir 不是存在的目录：{}",
            cwd.display()
        )));
    }
    let choice = match parse_shell_choice(spec.shell.as_deref(), settings.tools.shell) {
        Ok(c) => c,
        Err(e) => return Err(ToolOutcome::err(e)),
    };
    let shell = match build_shell(choice, &spec.command) {
        Ok(s) => s,
        Err(e) => return Err(ToolOutcome::err(e)),
    };
    let mut cmd = make_command(&shell, &cwd, &spec.envs);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Err(ToolOutcome::err(format!(
                "启动命令失败：{}（shell={}）。请检查命令是否存在、working_dir 是否可访问。",
                e,
                shell.desc
            )))
        }
    };
    let pid = child.id().unwrap_or(0);
    let job_id = format!("j{}", neosonia_core::util::short_id());
    let output = Arc::new(Mutex::new(OutputBuf::default()));
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdin = child.stdin.take();
    let pump_count = stdout.is_some() as usize + stderr.is_some() as usize;
    let pumps_left = Arc::new(AtomicUsize::new(pump_count));

    let job = Arc::new(Mutex::new(JobState {
        id: job_id.clone(),
        command: spec.command.clone(),
        shell_desc: shell.desc.clone(),
        cwd: cwd.display().to_string(),
        background: spec.background,
        label: spec.label.clone(),
        started: Instant::now(),
        pid,
        status: JobStatus::Running,
        exit_code: None,
        killed: false,
        output: output.clone(),
        read_cursor: 0,
        stdin,
        pumps_left: pumps_left.clone(),
    }));
    register_job(job.clone());

    if let Some(stdout) = stdout {
        let out = output.clone();
        let ctx = ctx.clone();
        let pumps = pumps_left.clone();
        tokio::spawn(pump_stream(stdout, out, Some(ctx), false, pumps));
    }
    if let Some(stderr) = stderr {
        let out = output.clone();
        let ctx = ctx.clone();
        let pumps = pumps_left.clone();
        tokio::spawn(pump_stream(stderr, out, Some(ctx), true, pumps));
    }
    {
        let job = job.clone();
        tokio::spawn(async move {
            let status = child.wait().await;
            let mut guard = job.lock();
            guard.status = JobStatus::Finished;
            guard.exit_code = status.ok().and_then(|s| s.code());
        });
    }
    Ok((job_id, pid))
}

async fn pump_stream<R>(
    mut reader: R,
    out: Arc<Mutex<OutputBuf>>,
    ctx: Option<ToolCtx>,
    is_stderr: bool,
    pumps_left: Arc<AtomicUsize>,
) where
    R: AsyncRead + Unpin + Send + 'static,
{
    let mut decoder = StreamDecoder::default();
    let mut buf = vec![0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                let text = decoder.push(&buf[..n]);
                if !text.is_empty() {
                    out.lock().push(is_stderr, &text);
                    if let Some(ctx) = &ctx {
                        ctx.emit_output(text);
                    }
                }
            }
            Err(_) => break,
        }
    }
    let tail = decoder.flush();
    if !tail.is_empty() {
        out.lock().push(is_stderr, &tail);
        if let Some(ctx) = &ctx {
            ctx.emit_output(tail);
        }
    }
    pumps_left.fetch_sub(1, Ordering::SeqCst);
}

/// 终止整个进程树。返回给模型看的一句说明。
async fn terminate_tree(pid: u32, force: bool) -> String {
    if pid == 0 {
        return "没有拿到进程 id，无法终止。".to_string();
    }
    #[cfg(windows)]
    {
        let mut args: Vec<String> = vec!["/PID".into(), pid.to_string(), "/T".into()];
        if force {
            args.push("/F".into());
        }
        let mut cmd = Command::new("taskkill");
        crate::apply_no_window_tokio(&mut cmd);
        cmd.args(&args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        match cmd.spawn() {
            Ok(mut child) => {
                let _ = tokio::time::timeout(Duration::from_secs(8), child.wait()).await;
                format!("已执行 taskkill {}。", if force { "/T /F" } else { "/T" })
            }
            Err(e) => format!("执行 taskkill 失败：{e}"),
        }
    }
    #[cfg(unix)]
    {
        let signal = if force { "-KILL" } else { "-TERM" };
        let group = format!("-{pid}");
        let mut cmd = Command::new("kill");
        crate::apply_no_window_tokio(&mut cmd);
        cmd.args([signal, "--", group.as_str()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        match cmd.spawn() {
            Ok(mut child) => {
                let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
                format!("已向进程组 -{pid} 发送 {signal}。")
            }
            Err(e) => format!("执行 kill 失败：{e}"),
        }
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = (pid, force);
        "当前平台不支持终止进程树。".to_string()
    }
}

fn wait_job_finished(job_id: String, max_wait: Duration) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let deadline = Instant::now() + max_wait;
        loop {
            if is_job_finished(&job_id) {
                return;
            }
            if Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
}

// ============================================================
// run_command
// ============================================================

/// 解析 `env` 参数为环境变量表（只接受字符串/数字/布尔）。
fn parse_env_map(args: &Value) -> BTreeMap<String, String> {
    let mut envs = BTreeMap::new();
    if let Some(Value::Object(obj)) = args.get("env") {
        for (k, v) in obj {
            let val = match v {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                _ => continue,
            };
            if !k.trim().is_empty() {
                envs.insert(k.clone(), val);
            }
        }
    }
    envs
}

async fn run_command(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    let command = match arg_str_req(&args, "command") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let settings = ctx.settings();
    // 参数没给 timeout_secs 就用配置；显式传 0 = 不超时。
    let timeout_secs = if args.get("timeout_secs").is_some() {
        arg_u64(&args, "timeout_secs", settings.tools.command_timeout_secs)
    } else {
        settings.tools.command_timeout_secs
    };
    let timeout_secs = timeout_secs.min(MAX_TIMEOUT_SECS);
    let background = arg_bool(&args, "background", false);
    let envs = parse_env_map(&args);
    let spec = StartSpec {
        command: command.clone(),
        working_dir: arg_str(&args, "working_dir"),
        shell: arg_str(&args, "shell"),
        envs,
        background,
        label: None,
    };
    let (job_id, pid) = match start_job(ctx, spec) {
        Ok(v) => v,
        Err(e) => return e,
    };

    if background {
        return ToolOutcome::ok(format!(
            "[ok] 命令已在后台启动\n任务 id：{job_id}\nPID：{pid}\n命令：{}\n\n\
             用 read_job_output(job_id=\"{job_id}\", wait_ms=60000) 等待并读取输出；\
             list_jobs 查看所有任务；kill_job 终止任务。",
            truncate_chars(&command, 300)
        ))
        .with_display(format!("后台任务 {job_id}"));
    }

    ctx.phase(format!(
        "正在执行命令：{}",
        truncate_chars(command.lines().next().unwrap_or(""), 60)
    ));
    let deadline = if timeout_secs == 0 {
        None
    } else {
        Some(Instant::now() + Duration::from_secs(timeout_secs))
    };
    let mut abort_reason: Option<&'static str> = None;
    loop {
        if ctx.is_cancelled() {
            abort_reason = Some("cancelled");
            break;
        }
        if let Some(dl) = deadline {
            if Instant::now() >= dl {
                abort_reason = Some("timeout");
                break;
            }
        }
        if is_job_finished(&job_id) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    if abort_reason.is_some() {
        mark_killed(&job_id);
        let _ = terminate_tree(pid, true).await;
        // 给子进程一点退出时间
        let _ = wait_job_finished(job_id.clone(), Duration::from_secs(5)).await;
    }

    // 等 stdout/stderr 读任务把管道剩余字节读完，避免输出被截断。
    if let Some(pumps) = get_job(&job_id).map(|j| j.lock().pumps_left.clone()) {
        wait_pumps_left(&pumps, Duration::from_secs(3)).await;
    }

    let Some(view) = view_job(&job_id) else {
        return ToolOutcome::err("任务状态丢失，无法读取输出。".to_string());
    };
    let still_running = view.status == JobStatus::Running;
    let (stdout, stderr, over_buffered) = {
        let out = view.output.lock();
        let (o, e) = out.split_streams();
        (o, e, out.base() > 0)
    };
    let max_chars = settings.tools.max_output_chars.max(2000);
    let stdout = middle_truncate(stdout.trim_end_matches('\n'), max_chars);
    let stderr = middle_truncate(stderr.trim_end_matches('\n'), max_chars);

    let mut content = String::new();
    match abort_reason {
        Some("timeout") => content.push_str(&format!(
            "[err] 命令超时：达到 timeout_secs={} 秒上限，已终止进程树（已运行 {}）。\n\
             提示：长任务可用更大的 timeout_secs、传 0 表示不超时，或者 background=true 后台运行后再 read_job_output 等待。\n",
            timeout_secs,
            human_duration_ms(view.elapsed.as_millis() as u64)
        )),
        Some("cancelled") => content.push_str(&format!(
            "[err] 已取消：用户中断，命令及其子进程已终止（已运行 {}）。\n",
            human_duration_ms(view.elapsed.as_millis() as u64)
        )),
        _ => {
            if view.exit_code == Some(0) && !still_running {
                content.push_str("[ok] 命令执行完成。\n");
            } else {
                content.push_str(&format!(
                    "[err] 命令失败：退出码 {}。\n",
                    view.exit_code
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "（无，可能被信号终止）".to_string())
                ));
            }
        }
    }
    if abort_reason.is_some() && still_running {
        content.push_str("警告：已发送终止信号，但进程 5 秒内仍未退出，可能仍在运行；可用 list_jobs 复查。\n");
    }
    content.push_str(&format!(
        "命令：{}\nshell：{}\n工作目录：{}\n退出码：{}\n耗时：{}\n任务 id：{}\n",
        view.command,
        view.shell_desc,
        view.cwd,
        view.exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "（无）".to_string()),
        human_duration_ms(view.elapsed.as_millis() as u64),
        job_id
    ));
    if !stdout.trim().is_empty() {
        content.push_str("\n--- stdout ---\n");
        content.push_str(&stdout);
        content.push('\n');
    }
    if !stderr.trim().is_empty() {
        content.push_str("\n--- stderr ---\n");
        content.push_str(&stderr);
        content.push('\n');
    }
    if stdout.trim().is_empty() && stderr.trim().is_empty() {
        content.push_str("\n（命令没有输出）\n");
    }
    if over_buffered {
        content.push_str(
            "\n[注意] 输出超过 200KB 环形缓冲上限，较老的部分已被丢弃（以上是最近保留的内容）。\n",
        );
    }

    let ok = abort_reason.is_none() && !still_running && view.exit_code == Some(0);
    let display = format!(
        "退出码 {}，耗时 {}",
        view.exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "-".to_string()),
        human_duration_ms(view.elapsed.as_millis() as u64)
    );
    ToolOutcome { ok, content, display: Some(display) }
}

// ============================================================
// 后台任务管理工具
// ============================================================

/// 启动一个带名字的后台任务（语义等同 `run_command {background:true}`，
/// 额外支持 `label` 便于模型/用户区分多个并发任务）。
async fn background_task(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    let command = match arg_str_req(&args, "command") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let label = arg_str(&args, "label")
        .map(|label| truncate_chars(label.trim(), 80))
        .filter(|label| !label.is_empty());
    let spec = StartSpec {
        command: command.clone(),
        working_dir: arg_str(&args, "working_dir"),
        shell: arg_str(&args, "shell"),
        envs: parse_env_map(&args),
        background: true,
        label: label.clone(),
    };
    let (task_id, pid) = match start_job(ctx, spec) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let label_line = label
        .as_ref()
        .map(|label| format!("名称：{label}\n"))
        .unwrap_or_default();
    ToolOutcome::ok(format!(
        "[ok] 后台任务已启动\n任务 id：{task_id}\n{label_line}PID：{pid}\n命令：{}\n\n\
         用 background_task_status(task_id=\"{task_id}\") 一次看状态和最后 20 行输出；\
         read_job_output 增量读输出；kill_job 终止。",
        truncate_chars(&command, 300)
    ))
    .with_display(match &label {
        Some(label) => format!("后台任务 {task_id}（{label}）"),
        None => format!("后台任务 {task_id}"),
    })
}

/// 查看后台任务状态（语义等同 job_status，命名对齐 background_task）。
async fn background_task_status(_ctx: &ToolCtx, args: Value) -> ToolOutcome {
    let target = arg_str(&args, "task_id").filter(|id| !id.trim().is_empty());
    let mut views: Vec<JobView> = all_job_views()
        .into_iter()
        .filter(|view| view.background)
        .collect();
    views.sort_by(|a, b| a.id.cmp(&b.id));
    if let Some(task_id) = &target {
        views.retain(|view| &view.id == task_id);
        if views.is_empty() {
            return ToolOutcome::err(format!(
                "没有找到后台任务 {task_id}。用 background_task_status（不传 task_id）查看当前所有任务。"
            ));
        }
    }
    if views.is_empty() {
        return ToolOutcome::ok(
            "当前没有后台任务。用 background_task 启动一个（例如长跑脚本/服务器）。",
        )
        .with_display("无后台任务".to_string());
    }
    let running = views
        .iter()
        .filter(|view| view.status == JobStatus::Running)
        .count();
    let mut out = format!("后台任务：共 {} 个（运行中 {running}）\n\n", views.len());
    for view in &views {
        let state = job_state_label(view.status, view.killed, view.exit_code);
        let tail = job_output_tail(&view.output, 20);
        out.push_str(&format!(
            "任务 task_id：{}\n名称：{}\n状态：{}\n命令：{}\nPID：{}\n已运行：{}\n退出码：{}\n最后 20 行输出：\n{}\n\n",
            view.id,
            view.label.as_deref().unwrap_or("（未命名）"),
            state,
            truncate_chars(&view.command.replace('\n', " "), 200),
            view.pid,
            human_duration_ms(view.elapsed.as_millis() as u64),
            view.exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| if view.status == JobStatus::Running {
                    "（运行中）".to_string()
                } else {
                    "（无）".to_string()
                }),
            if tail.trim().is_empty() {
                "（暂无输出）".to_string()
            } else {
                tail
            }
        ));
    }
    out.push_str("用 read_job_output 增量读输出、kill_job 终止；所有任务互不影响。");
    ToolOutcome::ok(out).with_display(format!("{} 个后台任务（{running} 运行中）", views.len()))
}

/// 拿当前所有任务的状态快照（不持锁，方便在 await 之间使用）。
fn all_job_views() -> Vec<JobView> {
    let jobs: Vec<Arc<Mutex<JobState>>> = {
        let store = JOBS.lock();
        store.jobs.values().cloned().collect()
    };
    jobs.into_iter()
        .map(|job| {
            let guard = job.lock();
            JobView {
                id: guard.id.clone(),
                command: guard.command.clone(),
                shell_desc: guard.shell_desc.clone(),
                cwd: guard.cwd.clone(),
                background: guard.background,
                label: guard.label.clone(),
                pid: guard.pid,
                status: guard.status,
                exit_code: guard.exit_code,
                killed: guard.killed,
                elapsed: guard.started.elapsed(),
                output: guard.output.clone(),
            }
        })
        .collect()
}

/// 任务输出尾部（合并 stdout/stderr，最多 n 行）。
fn job_output_tail(output: &Arc<Mutex<OutputBuf>>, n: usize) -> String {
    let (stdout, stderr) = output.lock().split_streams();
    let mut merged = String::new();
    if !stdout.trim().is_empty() {
        merged.push_str(stdout.trim_end());
    }
    if !stderr.trim().is_empty() {
        if !merged.is_empty() {
            merged.push('\n');
        }
        merged.push_str(stderr.trim_end());
    }
    last_lines(&merged, n)
}

async fn list_jobs(_ctx: &ToolCtx, _args: Value) -> ToolOutcome {
    let views = all_job_views();
    if views.is_empty() {
        return ToolOutcome::ok("当前没有任务。").with_display("无任务".to_string());
    }
    let mut out = format!("任务数：{}\n\n", views.len());
    for v in &views {
        let status = match (v.status, v.exit_code) {
            (JobStatus::Running, _) => "运行中".to_string(),
            (JobStatus::Finished, Some(0)) => "已完成".to_string(),
            (JobStatus::Finished, Some(c)) => format!("已结束（退出码 {c}）"),
            (JobStatus::Finished, None) => "已结束（退出码未知）".to_string(),
        };
        let output_chars = v.output.lock().total;
        out.push_str(&format!(
            "{}  [{}]  {}\n    命令：{}\n    PID：{}  耗时：{}  输出：{} 字符  模式：{}\n",
            v.id,
            status,
            if v.killed { "（曾被终止）" } else { "" },
            truncate_chars(&v.command.replace('\n', " "), 160),
            v.pid,
            human_duration_ms(v.elapsed.as_millis() as u64),
            output_chars,
            if v.background { "后台" } else { "前台" }
        ));
        if let Some(label) = &v.label {
            out.push_str(&format!("    名称：{label}\n"));
        }
    }
    ToolOutcome::ok(out).with_display(format!("{} 个任务", views.len()))
}

/// 一次拿到所有后台任务（或指定任务）的状态 + 最后 20 行输出。
async fn job_status(_ctx: &ToolCtx, args: Value) -> ToolOutcome {
    let target = arg_str(&args, "job_id").filter(|s| !s.trim().is_empty());
    let mut views = all_job_views();
    if let Some(job_id) = &target {
        views.retain(|view| &view.id == job_id);
        if views.is_empty() {
            return ToolOutcome::err(format!(
                "没有找到任务 {job_id}。可用 job_status（不传 job_id）查看所有任务 id。"
            ));
        }
    }
    if views.is_empty() {
        return ToolOutcome::ok("当前没有任务。用 run_command 时传 background=true 可以启动后台任务。")
            .with_display("无任务".to_string());
    }
    let mut out = format!("任务状态：共 {} 个\n\n", views.len());
    for view in &views {
        let label = job_state_label(view.status, view.killed, view.exit_code);
        let tail = job_output_tail(&view.output, 20);
        out.push_str(&format!(
            "任务：{}\n名称：{}\n状态：{}\n命令：{}\n已运行：{}\n退出码：{}\n最后 20 行输出：\n{}\n\n",
            view.id,
            view.label.as_deref().unwrap_or("（未命名）"),
            label,
            truncate_chars(&view.command.replace('\n', " "), 200),
            human_duration_ms(view.elapsed.as_millis() as u64),
            view.exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| {
                    if view.status == JobStatus::Running {
                        "（运行中）".to_string()
                    } else {
                        "（无）".to_string()
                    }
                }),
            if tail.trim().is_empty() {
                "（暂无输出）".to_string()
            } else {
                tail
            }
        ));
    }
    let running = views
        .iter()
        .filter(|view| view.status == JobStatus::Running)
        .count();
    out.push_str(&format!(
        "汇总：运行中 {running}，已结束 {}。\n用 read_job_output 增量读、kill_job 终止、list_jobs 看简表。",
        views.len() - running
    ));
    let display = if running > 0 {
        format!("{} 个任务（{running} 运行中）", views.len())
    } else {
        format!("{} 个任务", views.len())
    };
    ToolOutcome::ok(out).with_display(display)
}

async fn read_job_output(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    let job_id = match arg_str_req(&args, "job_id") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let wait_ms = arg_u64(&args, "wait_ms", 0).min(600_000);
    let max_chars = arg_u64(
        &args,
        "max_chars",
        ctx.settings().tools.max_output_chars as u64,
    )
    .max(200) as usize;
    let Some(job) = get_job(&job_id) else {
        let ids: Vec<String> = {
            let store = JOBS.lock();
            store.jobs.keys().cloned().collect()
        };
        return ToolOutcome::err(format!(
            "没有找到任务 {job_id}。当前任务 id：{}。",
            if ids.is_empty() {
                "（无）".to_string()
            } else {
                ids.join(", ")
            }
        ));
    };

    // 可选等待：等新输出或任务结束。
    if wait_ms > 0 {
        let deadline = Instant::now() + Duration::from_millis(wait_ms);
        loop {
            if ctx.is_cancelled() {
                break;
            }
            let (finished, cursor) = {
                let guard = job.lock();
                (guard.status == JobStatus::Finished, guard.read_cursor)
            };
            let has_new = {
                let guard = job.lock();
                let out = guard.output.lock();
                out.total > cursor
            };
            if finished || has_new || Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    let (status, exit_code, elapsed, cursor, output) = {
        let guard = job.lock();
        (
            guard.status,
            guard.exit_code,
            guard.started.elapsed(),
            guard.read_cursor,
            guard.output.clone(),
        )
    };
    let (chunk, new_cursor) = output.lock().read_from(cursor);
    {
        let mut guard = job.lock();
        guard.read_cursor = new_cursor;
    }
    let chunk = middle_truncate(&chunk, max_chars);
    let status_line = match (status, exit_code) {
        (JobStatus::Running, _) => "运行中".to_string(),
        (JobStatus::Finished, Some(0)) => "已完成（退出码 0）".to_string(),
        (JobStatus::Finished, Some(c)) => format!("已结束（退出码 {c}）"),
        (JobStatus::Finished, None) => "已结束（退出码未知）".to_string(),
    };
    let mut out = format!(
        "任务：{job_id}\n状态：{status_line}\n已运行：{}\n累计输出：{} 字符\n\n",
        human_duration_ms(elapsed.as_millis() as u64),
        output.lock().total
    );
    if chunk.trim().is_empty() {
        if wait_ms > 0 {
            out.push_str(&format!("[等待 {wait_ms}ms 后仍没有新输出]\n"));
        } else {
            out.push_str("[没有新输出]\n");
        }
    } else {
        out.push_str("--- 新增输出 ---\n");
        out.push_str(chunk.trim_end_matches('\n'));
        out.push('\n');
    }
    let ok = !(status == JobStatus::Finished && exit_code != Some(0));
    if !ok {
        out.push_str("\n[注意] 任务已结束且退出码非 0，可结合上面的输出判断失败原因。\n");
    }
    ToolOutcome::ok(out).with_display(format!("{job_id}：{status_line}"))
}

async fn kill_job(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    let job_id = match arg_str_req(&args, "job_id") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let force = arg_bool(&args, "force", true);
    let Some(view) = view_job(&job_id) else {
        return ToolOutcome::err(format!("没有找到任务 {job_id}。"));
    };
    if view.status == JobStatus::Finished {
        return ToolOutcome::ok(format!(
            "[ok] 任务 {job_id} 已经结束（退出码 {}），无需终止。",
            view.exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "未知".to_string())
        ))
        .with_display("任务已结束".to_string());
    }
    mark_killed(&job_id);
    let note = terminate_tree(view.pid, force).await;
    let _ = wait_job_finished(job_id.clone(), Duration::from_secs(5)).await;
    if let Some(pumps) = get_job(&job_id).map(|j| j.lock().pumps_left.clone()) {
        wait_pumps_left(&pumps, Duration::from_secs(2)).await;
    }
    let after = view_job(&job_id);
    let (finished, exit_code, out) = match after {
        Some(v) => (
            v.status == JobStatus::Finished,
            v.exit_code,
            v.output,
        ),
        None => (
            true,
            None,
            Arc::new(Mutex::new(OutputBuf::default())),
        ),
    };
    let tail = {
        let (stdout, stderr) = out.lock().split_streams();
        let mut merged = String::new();
        if !stdout.trim().is_empty() {
            merged.push_str(stdout.trim_end());
        }
        if !stderr.trim().is_empty() {
            if !merged.is_empty() {
                merged.push('\n');
            }
            merged.push_str(stderr.trim_end());
        }
        middle_truncate(&merged, 4000)
    };
    let mut content = format!(
        "任务：{job_id}\n终止操作：{note}\n状态：{}\n",
        if finished {
            format!(
                "已终止（退出码 {}）",
                exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "未知".to_string())
            )
        } else {
            "5 秒内仍未退出（进程可能无响应）".to_string()
        }
    );
    if !tail.trim().is_empty() {
        content.push_str("\n--- 终止前输出（尾部）---\n");
        content.push_str(&tail);
        content.push('\n');
    }
    let _ = ctx;
    if finished {
        ToolOutcome::ok(content).with_display("已终止任务".to_string())
    } else {
        ToolOutcome::err(content).with_display("终止失败".to_string())
    }
}

async fn send_job_input(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    let job_id = match arg_str_req(&args, "job_id") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let input = match arg_str(&args, "input") {
        Some(s) => s,
        None => return ToolOutcome::err("缺少必填参数 input（要写入 stdin 的一行）。".to_string()),
    };
    let _ = ctx;
    let Some(job) = get_job(&job_id) else {
        return ToolOutcome::err(format!("没有找到任务 {job_id}。"));
    };
    let line = if input.ends_with('\n') {
        input.clone()
    } else {
        format!("{input}\n")
    };
    let stdin = {
        let mut guard = job.lock();
        guard.stdin.take()
    };
    match stdin {
        Some(mut stdin) => {
            let write_result = match stdin.write_all(line.as_bytes()).await {
                Ok(()) => stdin.flush().await,
                Err(e) => Err(e),
            };
            {
                let mut guard = job.lock();
                guard.stdin = Some(stdin);
            }
            match write_result {
                Ok(()) => ToolOutcome::ok(format!(
                    "[ok] 已向任务 {job_id} 的 stdin 写入 {} 字节（含换行）。",
                    line.len()
                ))
                .with_display("已写入 stdin".to_string()),
                Err(e) => ToolOutcome::err(format!("写入 stdin 失败：{e}")),
            }
        }
        None => ToolOutcome::err(format!(
            "任务 {job_id} 没有可写的 stdin：可能已结束，或启动时 stdin 已关闭。"
        )),
    }
}

async fn wait_tool(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    let seconds = arg_f64(&args, "seconds", -1.0);
    if seconds < 0.0 {
        return ToolOutcome::err(
            "缺少必填参数 seconds（等待秒数，支持小数，上限 600 秒）。".to_string(),
        );
    }
    let secs = seconds.min(600.0);
    let reason = arg_str(&args, "reason").unwrap_or_default();
    let started = Instant::now();
    let until = started + Duration::from_secs_f64(secs);
    loop {
        if ctx.is_cancelled() {
            return ToolOutcome::err(format!(
                "等待已取消（已等待 {:.1} 秒）。",
                started.elapsed().as_secs_f64()
            ));
        }
        let now = Instant::now();
        if now >= until {
            break;
        }
        let remain = (until - now).min(Duration::from_millis(100));
        tokio::time::sleep(remain).await;
    }
    ToolOutcome::ok(format!(
        "[ok] 已等待 {:.1} 秒。{}",
        started.elapsed().as_secs_f64(),
        if reason.trim().is_empty() {
            String::new()
        } else {
            format!("原因：{reason}")
        }
    ))
    .with_display(format!("等待 {:.1}s", started.elapsed().as_secs_f64()))
}

// ============================================================
// 注册
// ============================================================

pub fn register(reg: &ToolRegistry) {
    reg.register(Arc::new(RunCommand));
    reg.register(Arc::new(BackgroundTask));
    register_job_tools(reg);
}

/// 只注册任务管理工具（不允许执行命令时也用得上：查看/终止历史任务、显式等待）。
pub fn register_job_tools(reg: &ToolRegistry) {
    reg.register(Arc::new(JobStatusTool));
    reg.register(Arc::new(BackgroundTaskStatus));
    reg.register(Arc::new(ListJobs));
    reg.register(Arc::new(ReadJobOutput));
    reg.register(Arc::new(KillJob));
    reg.register(Arc::new(WaitTool));
    reg.register(Arc::new(SendJobInput));
}

// ============================================================
// 工具声明
// ============================================================

tool_impl!(
    RunCommand,
    "run_command",
    "在桌面本机执行 shell 命令并返回退出码、耗时和输出。这是长任务工具：默认超时 1800 秒，\
     执行期间输出会流式推送到界面；用户取消会立刻终止整个进程树。\
     Windows 依次尝试 pwsh / powershell / cmd；Linux 用 bash -lc（无 bash 用 sh -lc）。\
     需在后台跑（例如启动服务器）时用 background=true；之后用 job_status 一次看所有后台任务的\
     状态+最后 20 行输出（running/finished/cancelled），或 read_job_output 增量读输出、kill_job 终止。\
     命令内容原样传给 shell，不会做额外转义；写多行脚本请直接用换行。",
    json!({
        "type": "object",
        "properties": {
            "command": {"type": "string", "description": "要执行的命令或脚本（原样传给 shell）"},
            "working_dir": {"type": "string", "description": "工作目录，默认工作区；相对路径相对工作区"},
            "shell": {"type": "string", "enum": ["auto", "pwsh", "powershell", "cmd", "bash", "sh"], "description": "指定 shell，默认 auto"},
            "timeout_secs": {"type": "integer", "description": "超时秒数；不传用配置默认值（1800），0 表示不超时"},
            "background": {"type": "boolean", "description": "后台运行并立即返回任务 id，默认 false"},
            "env": {"type": "object", "description": "额外环境变量，如 {\"RUST_LOG\":\"debug\"}"}
        },
        "required": ["command"]
    }),
    run_command,
    approval
);

tool_impl!(
    BackgroundTask,
    "background_task",
    "启动一个带名字的后台任务（语义等同 run_command 的 background=true，额外支持 label）。\n\
     返回 `任务 id`（= task_id），之后用 background_task_status 查看状态 / read_job_output 增量读 / kill_job 终止。\n\
     支持多个后台任务并发运行、互不影响；适合长跑脚本、服务器、轮询任务。",
    json!({
        "type": "object",
        "properties": {
            "command": {"type": "string", "description": "要运行的命令（原样传给 shell）"},
            "label": {"type": "string", "description": "任务名（便于区分多个后台任务），可选"},
            "working_dir": {"type": "string", "description": "工作目录，默认工作区"},
            "shell": {"type": "string", "enum": ["auto", "pwsh", "powershell", "cmd", "bash", "sh"], "description": "指定 shell，默认 auto"},
            "env": {"type": "object", "description": "额外环境变量"}
        },
        "required": ["command"]
    }),
    background_task,
    approval
);

tool_impl!(
    BackgroundTaskStatus,
    "background_task_status",
    "查看后台任务状态：`task_id` / 名称 / 状态（running / finished / cancelled / failed）/ 已运行时长 / 退出码 / 最后 20 行输出。\n\
     不传 task_id 时一次返回所有后台任务。和 background_task 配对使用（job_status 是同一套任务表的通用版本，两者都可用）。",
    json!({
        "type": "object",
        "properties": {
            "task_id": {"type": "string", "description": "可选；background_task 返回的任务 id，不传则列出所有后台任务"}
        }
    }),
    background_task_status
);

tool_impl!(
    JobStatusTool,
    "job_status",
    "一次调用查看所有后台任务（或指定 job_id 的任务）的状态：id、命令摘要、状态\
     （running / cancelling / finished / cancelled / failed）、已运行时长、退出码、最后 20 行输出。\n\
     典型用法：run_command 传 background=true 启动长任务 → 稍后 job_status 看是否还在跑/卡住/报错 → \
     kill_job 终止（之后状态变 cancelled）。info 比 list_jobs 更详细，list_jobs 仍在。",
    json!({
        "type": "object",
        "properties": {
            "job_id": {"type": "string", "description": "可选；不传则返回所有任务的状态"}
        }
    }),
    job_status
);

tool_impl!(
    ListJobs,
    "list_jobs",
    "列出所有命令任务（前台/后台）的简表：id、状态、退出码、耗时、命令、输出字符数。\
     需要每个任务的最后 20 行输出时用 job_status。",
    json!({"type": "object", "properties": {}}),
    list_jobs
);

tool_impl!(
    ReadJobOutput,
    "read_job_output",
    "读取任务自上次读取之后的新输出；wait_ms>0 时会阻塞等待新输出或任务结束（最多 600000ms）。\
     适合「启动长任务后等它输出/完成」：先 read_job_output(wait_ms=60000) 等一分钟，再看状态决定是否继续等。",
    json!({
        "type": "object",
        "properties": {
            "job_id": {"type": "string", "description": "run_command 返回的任务 id（形如 j3f2a1b0）"},
            "wait_ms": {"type": "integer", "description": "最多阻塞等待多少毫秒，默认 0 立即返回"},
            "max_chars": {"type": "integer", "description": "本次返回的最大字符数，默认取配置的 max_output_chars"}
        },
        "required": ["job_id"]
    }),
    read_job_output
);

tool_impl!(
    KillJob,
    "kill_job",
    "终止任务及其整个子进程树（Windows taskkill /T /F；Linux kill 进程组）。",
    json!({
        "type": "object",
        "properties": {
            "job_id": {"type": "string", "description": "任务 id"},
            "force": {"type": "boolean", "description": "是否强制终止，默认 true"}
        },
        "required": ["job_id"]
    }),
    kill_job,
    approval
);

tool_impl!(
    SendJobInput,
    "send_job_input",
    "给后台任务的 stdin 写入一行（用于需要交互输入的命令，例如确认提示）。会自动补换行。",
    json!({
        "type": "object",
        "properties": {
            "job_id": {"type": "string", "description": "任务 id"},
            "input": {"type": "string", "description": "要写入 stdin 的一行文本"}
        },
        "required": ["job_id", "input"]
    }),
    send_job_input
);

tool_impl!(
    WaitTool,
    "wait",
    "显式等待一段时间（秒，支持小数，上限 600 秒）。等待期间响应取消。\
     用于轮询外部状态之间的间隔，或等某个后台进程启动。",
    json!({
        "type": "object",
        "properties": {
            "seconds": {"type": "number", "description": "等待秒数，最多 600"},
            "reason": {"type": "string", "description": "可选，等待原因（会显示在结果里）"}
        },
        "required": ["seconds"]
    }),
    wait_tool
);

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{fake_ctx, temp_root};
    use serde_json::json;

    fn echo_cmd(text: &str) -> String {
        if cfg!(windows) {
            format!("Write-Output '{}'", text.replace('\'', "''"))
        } else {
            format!("echo '{}'", text.replace('\'', "'\\''"))
        }
    }

    fn sleep_cmd(secs: u64) -> String {
        if cfg!(windows) {
            format!("Start-Sleep -Seconds {secs}")
        } else {
            format!("sleep {secs}")
        }
    }

    fn exit_cmd(code: i32) -> String {
        if cfg!(windows) {
            format!("exit {code}")
        } else {
            format!("exit {code}")
        }
    }

    #[test]
    fn shell_choice_parsing() {
        assert_eq!(
            parse_shell_choice(None, ShellKind::Auto).unwrap(),
            ShellChoice::Auto
        );
        assert_eq!(
            parse_shell_choice(Some("pwsh"), ShellKind::Auto).unwrap(),
            ShellChoice::Pwsh
        );
        assert_eq!(
            parse_shell_choice(Some("cmd"), ShellKind::Auto).unwrap(),
            ShellChoice::Cmd
        );
        assert!(parse_shell_choice(Some("fish"), ShellKind::Auto).is_err());
        assert_eq!(
            parse_shell_choice(None, ShellKind::Bash).unwrap(),
            ShellChoice::Bash
        );
    }

    #[test]
    fn auto_shell_is_available() {
        let shell = build_shell(ShellChoice::Auto, "echo hi").expect("应该有可用 shell");
        assert!(shell.program.is_file());
        #[cfg(windows)]
        {
            let name = shell
                .program
                .file_name()
                .map(|n| n.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            assert!(
                name.contains("pwsh") || name.contains("powershell") || name.contains("cmd"),
                "意外 shell: {}",
                shell.program.display()
            );
        }
        assert!(shell.args.iter().any(|a| a == "echo hi"));
    }

    #[tokio::test]
    async fn run_command_echo_streams_and_succeeds() {
        let root = temp_root("shell-echo");
        let (ctx, host) = fake_ctx(&root);
        let out = run_command(
            &ctx,
            json!({"command": echo_cmd("hello-neosonia"), "timeout_secs": 60}),
        )
        .await;
        assert!(out.ok, "{}", out.content);
        assert!(out.content.contains("hello-neosonia"), "{}", out.content);
        assert!(out.content.contains("退出码：0"), "{}", out.content);
        // 流式输出应该已经通过 emit_output 推送过
        assert!(host.emitted_output().contains("hello-neosonia"));
    }

    #[tokio::test]
    async fn run_command_nonzero_exit_is_error() {
        let root = temp_root("shell-exit");
        let (ctx, _host) = fake_ctx(&root);
        let out = run_command(
            &ctx,
            json!({"command": exit_cmd(3), "timeout_secs": 60}),
        )
        .await;
        assert!(!out.ok, "{}", out.content);
        assert!(out.content.contains("退出码：3"), "{}", out.content);
    }

    #[tokio::test]
    async fn run_command_timeout_kills_process() {
        let root = temp_root("shell-timeout");
        let (ctx, _host) = fake_ctx(&root);
        let started = Instant::now();
        let out = run_command(
            &ctx,
            json!({"command": sleep_cmd(30), "timeout_secs": 1}),
        )
        .await;
        let elapsed = started.elapsed();
        assert!(!out.ok, "{}", out.content);
        assert!(out.content.contains("超时"), "{}", out.content);
        assert!(
            elapsed < Duration::from_secs(15),
            "超时后应该很快返回，实际 {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn run_command_cancel_kills_process() {
        let root = temp_root("shell-cancel");
        let (ctx, host) = fake_ctx(&root);
        host.cancel();
        let started = Instant::now();
        let out = run_command(&ctx, json!({"command": sleep_cmd(30)})).await;
        assert!(!out.ok, "{}", out.content);
        assert!(out.content.contains("取消"), "{}", out.content);
        assert!(started.elapsed() < Duration::from_secs(15));
    }

    #[tokio::test]
    async fn background_job_lifecycle() {
        let root = temp_root("shell-bg");
        let (ctx, _host) = fake_ctx(&root);
        let out = run_command(
            &ctx,
            json!({"command": sleep_cmd(20), "background": true}),
        )
        .await;
        assert!(out.ok, "{}", out.content);
        let job_line = out
            .content
            .lines()
            .find(|l| l.starts_with("任务 id："))
            .expect("应该有任务 id");
        let job_id = job_line.trim_start_matches("任务 id：").trim().to_string();
        assert!(!job_id.is_empty());

        let listing = list_jobs(&ctx, json!({})).await;
        assert!(listing.ok);
        assert!(listing.content.contains(&job_id), "{}", listing.content);

        // 等待 300ms：没有输出，应返回运行中
        let read = read_job_output(&ctx, json!({"job_id": job_id, "wait_ms": 300})).await;
        assert!(read.ok, "{}", read.content);
        assert!(read.content.contains("运行中"), "{}", read.content);

        let killed = kill_job(&ctx, json!({"job_id": job_id, "force": true})).await;
        assert!(killed.ok, "{}", killed.content);
        assert!(is_job_finished(&job_id), "任务应该已结束");
    }

    #[tokio::test]
    async fn wait_honors_cancel() {
        let root = temp_root("shell-wait");
        let (ctx, host) = fake_ctx(&root);
        let host2 = host.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            host2.cancel();
        });
        let started = Instant::now();
        let out = wait_tool(&ctx, json!({"seconds": 30})).await;
        assert!(!out.ok);
        assert!(out.content.contains("取消"), "{}", out.content);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn read_job_output_handles_rolling_buffer() {
        let mut buf = OutputBuf::default();
        for i in 0..10 {
            buf.push(false, &format!("chunk-{i}"));
        }
        let (text, cursor) = buf.read_from(0);
        assert_eq!(text, "chunk-0chunk-1chunk-2chunk-3chunk-4chunk-5chunk-6chunk-7chunk-8chunk-9");
        assert_eq!(cursor, buf.total);
        let (empty, cursor2) = buf.read_from(cursor);
        assert!(empty.is_empty());
        assert_eq!(cursor2, cursor);
        // 塞爆缓冲后最老的块应被丢弃
        for _ in 0..300 {
            buf.push(false, &"x".repeat(1000));
        }
        assert!(buf.total - buf.base() <= RING_CHAR_LIMIT + 1000);
        let (tail, cursor) = buf.read_from(0);
        assert!(tail.contains("已丢弃"), "{}", truncate_chars(&tail, 120));
        assert_eq!(cursor, buf.total);
    }

    #[test]
    fn decoder_handles_split_utf8() {
        let mut dec = StreamDecoder::default();
        let bytes = "中文".as_bytes();
        let first = dec.push(&bytes[..3]);
        let second = dec.push(&bytes[3..]);
        assert_eq!(format!("{first}{second}"), "中文");
    }

    #[test]
    fn decoder_falls_back_to_gbk() {
        let (bytes, _, _) = encoding_rs::GBK.encode("中文输出");
        let mut dec = StreamDecoder::default();
        let text = dec.push(&bytes);
        assert_eq!(text, "中文输出");
    }

    #[test]
    fn job_state_labels() {
        assert_eq!(job_state_label(JobStatus::Running, false, None), "running");
        assert_eq!(job_state_label(JobStatus::Running, true, None), "cancelling");
        assert_eq!(
            job_state_label(JobStatus::Finished, false, Some(0)),
            "finished"
        );
        assert_eq!(
            job_state_label(JobStatus::Finished, false, Some(1)),
            "failed"
        );
        assert_eq!(
            job_state_label(JobStatus::Finished, true, Some(0)),
            "cancelled"
        );
        assert_eq!(
            job_state_label(JobStatus::Finished, true, Some(1)),
            "cancelled"
        );
    }

    #[test]
    fn last_lines_window() {
        let text = (1..=30).map(|i| format!("line-{i}")).collect::<Vec<_>>().join("\n");
        let tail = last_lines(&text, 20);
        assert_eq!(tail.lines().count(), 20);
        assert!(tail.starts_with("line-11"));
        assert!(tail.ends_with("line-30"));
        // 行数不足时全部返回
        assert_eq!(last_lines("a\nb", 20), "a\nb");
        assert_eq!(last_lines("", 20), "");
    }

    #[tokio::test]
    async fn job_status_reports_all_states_and_tail() {
        let root = temp_root("job-status");
        let (ctx, _host) = fake_ctx(&root);
        let started = run_command(
            &ctx,
            json!({"command": "for ($i=1; $i -le 30; $i++) { Write-Output \"tick $i\" }", "timeout_secs": 60}),
        )
        .await;
        assert!(started.ok, "{}", started.content);
        let job_id = started
            .content
            .lines()
            .find(|l| l.starts_with("任务 id："))
            .map(|l| l.trim_start_matches("任务 id：").trim().to_string())
            .expect("应有任务 id");

        let status = job_status(&ctx, json!({})).await;
        assert!(status.ok, "{}", status.content);
        assert!(status.content.contains(&job_id), "{}", status.content);
        assert!(status.content.contains("状态："), "{}", status.content);
        assert!(status.content.contains("最后 20 行输出："), "{}", status.content);
        // 命令已完成，状态应是 finished（测试里同步等待了命令结束）
        assert!(
            status.content.contains("状态：finished"),
            "同步命令完成后应为 finished: {}",
            status.content
        );
        // 最后 20 行：tick 11..30
        assert!(status.content.contains("tick 30"), "{}", status.content);
        assert!(!status.content.contains("tick 5\n"), "{}", status.content);
        println!("job_status 输出：\n{}", status.content);

        // 指定 job_id 只返回一条
        let single = job_status(&ctx, json!({"job_id": job_id})).await;
        assert!(single.ok, "{}", single.content);
        assert!(single.content.contains("共 1 个"), "{}", single.content);
        // 不存在的 id 返回错误
        let missing = job_status(&ctx, json!({"job_id": "j_not_exist"})).await;
        assert!(!missing.ok);
    }

    #[tokio::test]
    async fn job_status_shows_running_then_cancelled() {
        let root = temp_root("job-status-cancel");
        let (ctx, _host) = fake_ctx(&root);
        let started = run_command(
            &ctx,
            json!({"command": "Start-Sleep -Seconds 30", "background": true}),
        )
        .await;
        assert!(started.ok, "{}", started.content);
        let job_id = started
            .content
            .lines()
            .find(|l| l.starts_with("任务 id："))
            .map(|l| l.trim_start_matches("任务 id：").trim().to_string())
            .expect("应有任务 id");

        let running = job_status(&ctx, json!({"job_id": job_id})).await;
        assert!(running.ok, "{}", running.content);
        assert!(
            running.content.contains("状态：running"),
            "{}",
            running.content
        );

        let killed = kill_job(&ctx, json!({"job_id": job_id})).await;
        assert!(killed.ok, "{}", killed.content);
        let after = job_status(&ctx, json!({"job_id": job_id})).await;
        assert!(after.ok, "{}", after.content);
        assert!(
            after.content.contains("状态：cancelled"),
            "kill 后应为 cancelled: {}",
            after.content
        );
        assert!(after.content.contains("汇总：运行中 0"), "{}", after.content);
        println!("cancelled 后 job_status：\n{}", after.content);
    }
}
