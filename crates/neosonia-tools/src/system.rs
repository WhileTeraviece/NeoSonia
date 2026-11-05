//! 系统集成工具：系统信息、前台窗口、打开路径/URL、通知、进程列表与终止。
//!
//! Windows 的前台窗口用 raw FFI（user32）实现，避免与 UIA 的 windows crate
//! 版本耦合；Linux 退化为 xdotool（存在时）。

use std::sync::Arc;
use std::time::{Duration, Instant};

use neosonia_core::model::{ToolCtx, ToolOutcome};
use neosonia_core::registry::ToolRegistry;
use neosonia_core::util::{human_bytes, truncate_chars};
use serde_json::{json, Value};
use sysinfo::{Disks, Pid, ProcessesToUpdate, System};

use crate::{arg_bool, arg_i64, arg_str, arg_str_req, arg_u64};

pub fn register(reg: &ToolRegistry) {
    reg.register(Arc::new(SystemInfo));
    reg.register(Arc::new(ForegroundWindow));
    reg.register(Arc::new(WaitForWindow));
    reg.register(Arc::new(OpenPath));
    reg.register(Arc::new(Notify));
    reg.register(Arc::new(ListProcesses));
    reg.register(Arc::new(KillProcess));
    reg.register(Arc::new(WindowHideApp));
    reg.register(Arc::new(WindowIsHidden));
}

/// 标题包含匹配（不区分大小写；空 needle 视为匹配任何窗口）。
fn title_matches(title: &str, needle: &str) -> bool {
    let needle = needle.trim();
    if needle.is_empty() {
        return true;
    }
    title.to_ascii_lowercase().contains(&needle.to_ascii_lowercase())
}

async fn wait_for_window(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    let title_contains = match arg_str_req(&args, "title_contains") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let timeout_secs = arg_u64(&args, "timeout_secs", 30).clamp(1, 600);
    let interval_ms = arg_u64(&args, "interval_ms", 300).clamp(50, 5000);
    let started = Instant::now();
    let deadline = started + Duration::from_secs(timeout_secs);
    ctx.phase(format!("等待窗口：标题包含 {title_contains:?}"));
    loop {
        if ctx.is_cancelled() {
            return ToolOutcome::err(format!(
                "等待窗口已取消（已等待 {:.1} 秒）。",
                started.elapsed().as_secs_f64()
            ));
        }
        match foreground_info() {
            Ok(info) => {
                if title_matches(&info.title, &title_contains) {
                    let mut info = info;
                    if let Some(pid) = info.pid {
                        info.process_name = process_name_of(pid);
                    }
                    return ToolOutcome::ok(format!(
                        "[ok] 已等到目标窗口（等待 {:.1} 秒）\n{}",
                        started.elapsed().as_secs_f64(),
                        render_foreground(&info)
                    ))
                    .with_display(truncate_chars(&info.title, 40));
                }
            }
            Err(_) => {
                // 没有前台窗口（锁屏/切桌面）时继续等
            }
        }
        if Instant::now() >= deadline {
            return ToolOutcome::err(format!(
                "等待窗口超时（{timeout_secs} 秒）：前台窗口标题始终不包含 {title_contains:?}。\n\
                 可先 foreground_window 看当前窗口，或用 ui_window_list 列出所有窗口。"
            ));
        }
        let remaining = (deadline - Instant::now()).min(Duration::from_millis(interval_ms));
        tokio::time::sleep(remaining).await;
    }
}

// ============================================================
// 前台窗口
// ============================================================

#[derive(Clone, Debug, Default)]
struct ForegroundInfo {
    title: String,
    pid: Option<u32>,
    hwnd: String,
    /// (left, top, right, bottom) 物理像素，屏幕坐标
    rect: Option<(i32, i32, i32, i32)>,
    process_name: Option<String>,
}

#[cfg(windows)]
mod fg_win {
    use super::ForegroundInfo;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetForegroundWindow() -> isize;
        fn GetWindowTextW(hwnd: isize, lpstring: *mut u16, nmaxcount: i32) -> i32;
        fn GetWindowTextLengthW(hwnd: isize) -> i32;
        fn GetWindowRect(hwnd: isize, lprect: *mut Rect) -> i32;
        fn GetWindowThreadProcessId(hwnd: isize, lpdwprocessid: *mut u32) -> u32;
        fn IsWindow(hwnd: isize) -> i32;
    }

    pub fn foreground_window() -> Result<ForegroundInfo, String> {
        // SAFETY: 都是 user32 的只读查询；hwnd 来自 GetForegroundWindow，
        // 传给其它 API 前用 IsWindow 校验，缓冲区长度按 GetWindowTextLengthW 分配。
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd == 0 || IsWindow(hwnd) == 0 {
                return Err("当前没有前台窗口（可能锁屏或桌面切换中）。".to_string());
            }
            let len = GetWindowTextLengthW(hwnd).max(0) as usize;
            let mut buf = vec![0u16; len + 1];
            let copied = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32).max(0) as usize;
            buf.truncate(copied);
            let title = String::from_utf16_lossy(&buf);
            let mut pid: u32 = 0;
            GetWindowThreadProcessId(hwnd, &mut pid);
            let mut rect = Rect::default();
            let rect = if GetWindowRect(hwnd, &mut rect) != 0 {
                Some((rect.left, rect.top, rect.right, rect.bottom))
            } else {
                None
            };
            Ok(ForegroundInfo {
                title: if title.trim().is_empty() {
                    "（无标题）".to_string()
                } else {
                    title
                },
                pid: if pid == 0 { None } else { Some(pid) },
                hwnd: format!("0x{hwnd:X}"),
                rect,
                process_name: None,
            })
        }
    }
}

#[cfg(unix)]
mod fg_unix {
    use super::ForegroundInfo;
    use std::path::PathBuf;
    use std::process::Command;

    fn run(arg: &[&str]) -> Option<String> {
        let mut cmd = Command::new("xdotool");
        crate::apply_no_window_std(&mut cmd);
        let output = cmd.args(arg).output().ok()?;
        if !output.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    pub fn foreground_window() -> Result<ForegroundInfo, String> {
        if which("xdotool").is_none() {
            return Err(
                "当前 Linux 环境没有 xdotool，无法查询前台窗口。可安装 xdotool，或改用 screenshot + mouse_click。"
                    .to_string(),
            );
        }
        let wid = run(&["getactivewindow"])
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "xdotool 没有返回活动窗口（可能没有 X11 会话）。".to_string())?;
        let title = run(&["getwindowname", &wid]).unwrap_or_else(|| "（无标题）".to_string());
        let pid = run(&["getwindowpid", &wid]).and_then(|s| s.parse::<u32>().ok());
        let geo = run(&["getwindowgeometry", "--shell", &wid]);
        let mut rect = None;
        if let Some(geo) = geo {
            let mut x = None;
            let mut y = None;
            let mut w = None;
            let mut h = None;
            for line in geo.lines() {
                if let Some(v) = line.strip_prefix("X=") {
                    x = v.trim().parse::<i32>().ok();
                } else if let Some(v) = line.strip_prefix("Y=") {
                    y = v.trim().parse::<i32>().ok();
                } else if let Some(v) = line.strip_prefix("WIDTH=") {
                    w = v.trim().parse::<i32>().ok();
                } else if let Some(v) = line.strip_prefix("HEIGHT=") {
                    h = v.trim().parse::<i32>().ok();
                }
            }
            if let (Some(x), Some(y), Some(w), Some(h)) = (x, y, w, h) {
                rect = Some((x, y, x + w, y + h));
            }
        }
        Ok(ForegroundInfo {
            title: if title.trim().is_empty() {
                "（无标题）".to_string()
            } else {
                title
            },
            pid,
            hwnd: wid,
            rect,
            process_name: None,
        })
    }

    fn which(name: &str) -> Option<PathBuf> {
        let paths = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        None
    }
}

fn foreground_info() -> Result<ForegroundInfo, String> {
    #[cfg(windows)]
    {
        fg_win::foreground_window()
    }
    #[cfg(unix)]
    {
        fg_unix::foreground_window()
    }
    #[cfg(not(any(windows, unix)))]
    {
        Err("当前平台未实现前台窗口查询。".to_string())
    }
}

/// 用 sysinfo 查进程名（拿不到就返回 None，不让工具失败）。
fn process_name_of(pid: u32) -> Option<String> {
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::Some(&[Pid::from_u32(pid)]), true);
    sys.process(Pid::from_u32(pid))
        .map(|p| p.name().to_string_lossy().to_string())
}

fn render_foreground(info: &ForegroundInfo) -> String {
    let mut out = format!("标题：{}\n", info.title);
    out.push_str(&format!("窗口句柄：{}\n", info.hwnd));
    match info.pid {
        Some(pid) => out.push_str(&format!(
            "进程：{}（pid {}）\n",
            info.process_name.as_deref().unwrap_or("（未知）"),
            pid
        )),
        None => out.push_str("进程：（未知）\n"),
    }
    if let Some((l, t, r, b)) = info.rect {
        out.push_str(&format!(
            "窗口矩形：x={l} y={t} 宽={} 高={}（物理像素）\n",
            r - l,
            b - t
        ));
    }
    out
}

async fn foreground_window(_ctx: &ToolCtx, _args: Value) -> ToolOutcome {
    let joined = tokio::task::spawn_blocking(|| {
        let info = foreground_info()?;
        let mut info = info;
        if let Some(pid) = info.pid {
            info.process_name = process_name_of(pid);
        }
        Ok::<ForegroundInfo, String>(info)
    })
    .await;
    match joined {
        Ok(Ok(info)) => {
            ToolOutcome::ok(format!("[ok] 前台窗口\n{}", render_foreground(&info)))
                .with_display(truncate_chars(&info.title, 40))
        }
        Ok(Err(e)) => ToolOutcome::err(e),
        Err(e) => ToolOutcome::err(format!("查询前台窗口任务异常：{e}")),
    }
}

// ============================================================
// system_info
// ============================================================

fn collect_system_info() -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "系统：{}\n",
        System::long_os_version().unwrap_or_else(|| std::env::consts::OS.to_string())
    ));
    out.push_str(&format!(
        "内核：{}\n",
        System::kernel_version().unwrap_or_else(|| "（未知）".to_string())
    ));
    out.push_str(&format!(
        "主机名：{}\n",
        System::host_name().unwrap_or_else(|| "（未知）".to_string())
    ));
    out.push_str(&format!(
        "架构：{}\n",
        std::env::consts::ARCH
    ));

    let mut sys = System::new_all();
    sys.refresh_all();
    let cpus = sys.cpus();
    let cores = cpus.len();
    let physical = sys.physical_core_count().unwrap_or(cores);
    let brand = cpus
        .first()
        .map(|c| c.brand().to_string())
        .unwrap_or_default();
    let freq = cpus.first().map(|c| c.frequency()).unwrap_or(0);
    out.push_str(&format!(
        "CPU：{} 逻辑核（{} 物理核）{}  当前负载 {:.1}%\n",
        cores,
        physical,
        if brand.trim().is_empty() {
            String::new()
        } else {
            format!("，{}", brand.trim())
        },
        sys.global_cpu_usage()
    ));
    if freq > 0 {
        out.push_str(&format!("CPU 频率：{} MHz\n", freq));
    }
    let total = sys.total_memory();
    let used = sys.used_memory();
    let pct = if total > 0 {
        used as f64 / total as f64 * 100.0
    } else {
        0.0
    };
    out.push_str(&format!(
        "内存：已用 {} / 共 {}（{:.1}%）\n",
        human_bytes(used),
        human_bytes(total),
        pct
    ));
    let swap_total = sys.total_swap();
    if swap_total > 0 {
        out.push_str(&format!(
            "交换：已用 {} / 共 {}\n",
            human_bytes(sys.used_swap()),
            human_bytes(swap_total)
        ));
    }

    let disks = Disks::new_with_refreshed_list();
    out.push_str(&format!("磁盘：{} 个挂载点\n", disks.list().len()));
    let mut mounts: Vec<_> = disks.list().iter().collect();
    mounts.sort_by(|a, b| a.mount_point().cmp(b.mount_point()));
    for disk in mounts.iter().take(10) {
        let total = disk.total_space();
        let free = disk.available_space();
        out.push_str(&format!(
            "  {} 可用 {} / 共 {}{}\n",
            disk.mount_point().display(),
            human_bytes(free),
            human_bytes(total),
            if disk.is_removable() { "（可移动）" } else { "" }
        ));
    }

    match foreground_info() {
        Ok(mut info) => {
            if let Some(pid) = info.pid {
                info.process_name = process_name_of(pid);
            }
            out.push_str("\n前台窗口：\n");
            out.push_str(&render_foreground(&info));
        }
        Err(e) => out.push_str(&format!("\n前台窗口：不可用（{e}）\n")),
    }

    out.push_str(&format!(
        "\n工作目录：{}\n当前时间：{}\n",
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "（未知）".to_string()),
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S %:z")
    ));
    out
}

async fn system_info(_ctx: &ToolCtx, _args: Value) -> ToolOutcome {
    let joined = tokio::task::spawn_blocking(collect_system_info).await;
    match joined {
        Ok(text) => ToolOutcome::ok(text).with_display("系统信息".to_string()),
        Err(e) => ToolOutcome::err(format!("采集系统信息任务异常：{e}")),
    }
}

// ============================================================
// open_path / notify
// ============================================================

async fn open_path(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    let target = match arg_str_req(&args, "target") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let trimmed = target.trim();
    let is_url = ["http://", "https://", "mailto:", "file://", "ftp://"]
        .iter()
        .any(|scheme| trimmed.to_ascii_lowercase().starts_with(scheme));
    let resolved: String = if is_url {
        trimmed.to_string()
    } else {
        let path = match crate::resolve_read_path(ctx, trimmed) {
            Ok(p) => p,
            Err(e) => return e,
        };
        if !path.exists() {
            return ToolOutcome::err(format!(
                "路径不存在：{}。相对路径相对工作目录 {}。",
                path.display(),
                ctx.workspace().display()
            ));
        }
        path.display().to_string()
    };
    let target2 = resolved.clone();
    let joined = tokio::task::spawn_blocking(move || open::that(&target2)).await;
    match joined {
        Ok(Ok(())) => ToolOutcome::ok(format!(
            "[ok] 已用系统默认程序打开：{}\n{}",
            resolved,
            if is_url { "（URL 会交给默认浏览器）" } else { "" }
        ))
        .with_display(truncate_chars(&resolved, 50)),
        Ok(Err(e)) => ToolOutcome::err(format!(
            "打开 {} 失败：{e}。可检查文件关联，或改用 run_command 启动。",
            resolved
        )),
        Err(e) => ToolOutcome::err(format!("打开任务异常：{e}")),
    }
}

/// 单引号转义，避免把用户文本注入 PowerShell 脚本。
fn ps_quote(s: &str) -> String {
    s.replace('\'', "''")
}

#[cfg(windows)]
fn notify_script(title: &str, body: &str) -> String {
    // 用 NotifyIcon 气泡提示（Win10/11 会显示为通知）；不依赖 BurntToast。
    format!(
        "Add-Type -AssemblyName System.Windows.Forms\r\n\
         $n = New-Object System.Windows.Forms.NotifyIcon\r\n\
         $n.Icon = [System.Drawing.SystemIcons]::Information\r\n\
         $n.Visible = $true\r\n\
         $n.ShowBalloonTip(8000, '{}', '{}', [System.Windows.Forms.ToolTipIcon]::Info)\r\n\
         Start-Sleep -Seconds 9\r\n\
         $n.Dispose()\r\n",
        ps_quote(title),
        ps_quote(body)
    )
}

async fn notify(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    let title = match arg_str_req(&args, "title") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let body = match arg_str(&args, "body") {
        Some(s) => s,
        None => return ToolOutcome::err("缺少必填参数 body（通知正文）。".to_string()),
    };
    let title = truncate_chars(&title, 120);
    let body = truncate_chars(&body, 500);

    #[cfg(windows)]
    {
        let script = notify_script(&title, &body);
        let mut cmd = tokio::process::Command::new("powershell.exe");
        // 管理员运行时 GUI 进程起 powershell 会闪黑窗；CREATE_NO_WINDOW 彻底避免。
        crate::apply_no_window_tokio(&mut cmd);
        cmd.args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script.as_str(),
        ]);
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        match cmd.spawn() {
            Ok(_child) => {
                // 不等待：通知进程自己显示 8 秒后退出。
                ToolOutcome::ok(format!(
                    "[ok] 已发送系统通知\n标题：{title}\n正文：{body}"
                ))
                .with_display("已发送通知".to_string())
            }
            Err(e) => {
                // 最后兜底：写日志，避免调用彻底失败。
                let log_path = ctx.paths().logs.join("notifications.log");
                let line = format!(
                    "[{}] {} | {}\n",
                    chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                    title,
                    body
                );
                let note = match std::fs::create_dir_all(&ctx.paths().logs)
                    .and_then(|_| std::fs::OpenOptions::new().create(true).append(true).open(&log_path).and_then(|mut f| {
                        use std::io::Write;
                        f.write_all(line.as_bytes())
                    }))
                {
                    Ok(()) => format!("已退化为写日志：{}", log_path.display()),
                    Err(log_err) => format!("写日志也失败：{log_err}"),
                };
                ToolOutcome::ok(format!(
                    "[ok] 系统通知不可用（{e}），{note}\n标题：{title}\n正文：{body}"
                ))
                .with_display("通知退化为日志".to_string())
            }
        }
    }
    #[cfg(unix)]
    {
        let mut cmd = tokio::process::Command::new("notify-send");
        crate::apply_no_window_tokio(&mut cmd);
        cmd.args(["--app-name=NeoSonia", title.as_str(), body.as_str()]);
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        match cmd.spawn() {
            Ok(_child) => ToolOutcome::ok(format!(
                "[ok] 已发送桌面通知\n标题：{title}\n正文：{body}"
            ))
            .with_display("已发送通知".to_string()),
            Err(e) => {
                let log_path = ctx.paths().logs.join("notifications.log");
                let _ = std::fs::create_dir_all(&ctx.paths().logs);
                let line = format!(
                    "[{}] {} | {}\n",
                    chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                    title,
                    body
                );
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&log_path)
                {
                    use std::io::Write;
                    let _ = f.write_all(line.as_bytes());
                }
                ToolOutcome::ok(format!(
                    "[ok] notify-send 不可用（{e}），已写入日志 {}",
                    log_path.display()
                ))
                .with_display("通知退化为日志".to_string())
            }
        }
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = (title, body);
        ToolOutcome::err("当前平台未实现通知。".to_string())
    }
}

// ============================================================
// 进程
// ============================================================

async fn list_processes(_ctx: &ToolCtx, args: Value) -> ToolOutcome {
    let filter = arg_str(&args, "filter")
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    let limit = arg_u64(&args, "limit", 30).clamp(1, 500) as usize;
    let joined = tokio::task::spawn_blocking(move || -> String {
        let mut sys = System::new_all();
        sys.refresh_processes(ProcessesToUpdate::All, true);
        let mut rows: Vec<(String, u32, u64, f32)> = sys
            .processes()
            .iter()
            .map(|(pid, p)| {
                (
                    p.name().to_string_lossy().to_string(),
                    pid.as_u32(),
                    p.memory(),
                    p.cpu_usage(),
                )
            })
            .filter(|(name, pid, _, _)| {
                filter.is_empty()
                    || name.to_lowercase().contains(&filter)
                    || pid.to_string() == filter
            })
            .collect();
        rows.sort_by(|a, b| b.2.cmp(&a.2).then(a.1.cmp(&b.1)));
        let total = rows.len();
        rows.truncate(limit);
        let mut out = format!(
            "进程：{} 个匹配{}，显示按内存降序的前 {} 个\n\n",
            total,
            if filter.is_empty() {
                String::new()
            } else {
                format!("（filter={filter:?}）")
            },
            rows.len()
        );
        out.push_str("PID      内存         CPU     名称\n");
        for (name, pid, mem, cpu) in rows {
            out.push_str(&format!(
                "{:<8} {:<11} {:>5.1}%  {}\n",
                pid,
                human_bytes(mem),
                cpu,
                truncate_chars(&name, 60)
            ));
        }
        out
    })
    .await;
    match joined {
        Ok(text) => ToolOutcome::ok(text).with_display("进程列表".to_string()),
        Err(e) => ToolOutcome::err(format!("枚举进程任务异常：{e}")),
    }
}

async fn kill_process(ctx: &ToolCtx, args: Value) -> ToolOutcome {
    let raw_pid = arg_i64(&args, "pid", -1);
    if raw_pid <= 0 {
        return ToolOutcome::err("缺少必填参数 pid（正整数进程 id）。".to_string());
    }
    let pid = raw_pid as u32;
    if pid <= 4 {
        return ToolOutcome::err(format!(
            "拒绝终止 pid {pid}：这是系统关键进程（pid <= 4）。"
        ));
    }
    if pid == std::process::id() {
        return ToolOutcome::err("拒绝终止 NeoSonia 自己。".to_string());
    }
    let force = arg_bool(&args, "force", true);
    let _ = ctx;
    let pid_text = pid.to_string();
    #[cfg(windows)]
    {
        let mut cmd = tokio::process::Command::new("taskkill");
        // 同样避免 taskkill 的控制台窗口闪现（管理员模式尤其明显）。
        crate::apply_no_window_tokio(&mut cmd);
        if force {
            cmd.args(["/PID", pid_text.as_str(), "/F"]);
        } else {
            cmd.args(["/PID", pid_text.as_str()]);
        }
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let output = cmd.output().await;
        match output {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                let msg = stdout
                    .lines()
                    .chain(stderr.lines())
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if out.status.success() {
                    ToolOutcome::ok(format!("[ok] 已终止进程 {pid}。{msg}"))
                        .with_display(format!("终止 pid {pid}"))
                } else {
                    ToolOutcome::err(format!(
                        "终止进程 {pid} 失败（{}）：{}",
                        out.status, msg
                    ))
                }
            }
            Err(e) => ToolOutcome::err(format!("执行 taskkill 失败：{e}")),
        }
    }
    #[cfg(unix)]
    {
        let signal = if force { "-KILL" } else { "-TERM" };
        let mut cmd = tokio::process::Command::new("kill");
        crate::apply_no_window_tokio(&mut cmd);
        let output = cmd.args([signal, pid_text.as_str()]).output().await;
        match output {
            Ok(out) if out.status.success() => {
                ToolOutcome::ok(format!("[ok] 已向进程 {pid} 发送 {signal}。"))
                    .with_display(format!("终止 pid {pid}"))
            }
            Ok(out) => ToolOutcome::err(format!(
                "终止进程 {pid} 失败：{}",
                String::from_utf8_lossy(&out.stderr).trim()
            )),
            Err(e) => ToolOutcome::err(format!("执行 kill 失败：{e}")),
        }
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = force;
        ToolOutcome::err("当前平台未实现终止进程。".to_string())
    }
}

// ============================================================
// 工具声明
// ============================================================

tool_impl!(
    SystemInfo,
    "system_info",
    "获取系统概况：OS 版本、主机名、CPU 核数与负载、内存、磁盘剩余、当前前台窗口标题与进程。\
     排查环境问题（缺依赖、磁盘满、机器卡）时先看这个。",
    json!({"type": "object", "properties": {}}),
    system_info
);

tool_impl!(
    ForegroundWindow,
    "foreground_window",
    "获取当前前台窗口的标题、进程名、pid、窗口矩形（物理像素）。\
     用于确认键盘输入会打到哪个窗口；Windows 用 Win32 API，Linux 需要 xdotool。",
    json!({"type": "object", "properties": {}}),
    foreground_window
);

tool_impl!(
    WaitForWindow,
    "wait_for_window",
    "轮询前台窗口，等到标题包含指定文本（不区分大小写）时返回；适合等某个程序启动/弹窗出现后再操作。\
     超时会返回中文错误；等待期间响应取消。可以用 foreground_window 先确认当前窗口。",
    json!({
        "type": "object",
        "properties": {
            "title_contains": {"type": "string", "description": "标题里要包含的文本（不区分大小写）"},
            "timeout_secs": {"type": "integer", "description": "最长等待秒数，默认 30，最大 600"},
            "interval_ms": {"type": "integer", "description": "轮询间隔毫秒，默认 300"}
        },
        "required": ["title_contains"]
    }),
    wait_for_window
);

tool_impl!(
    OpenPath,
    "open_path",
    "用系统默认程序打开文件、目录或 URL（例如打开浏览器、资源管理器、记事本、PDF）。\
     target 可以是 http(s) URL 或本地路径（相对路径相对工作目录）。",
    json!({
        "type": "object",
        "properties": {
            "target": {"type": "string", "description": "要打开的路径或 URL"}
        },
        "required": ["target"]
    }),
    open_path
);

tool_impl!(
    Notify,
    "notify",
    "弹一条桌面通知（Windows 用系统通知气泡，Linux 用 notify-send）。\
     适合长任务完成后提醒用户，不要用于刷屏。",
    json!({
        "type": "object",
        "properties": {
            "title": {"type": "string", "description": "通知标题"},
            "body": {"type": "string", "description": "通知正文"}
        },
        "required": ["title", "body"]
    }),
    notify
);

tool_impl!(
    ListProcesses,
    "list_processes",
    "列出正在运行的进程（名字/pid/内存/CPU），可用 filter 按名字或 pid 过滤，默认按内存降序显示前 30 个。",
    json!({
        "type": "object",
        "properties": {
            "filter": {"type": "string", "description": "按进程名子串或 pid 过滤"},
            "limit": {"type": "integer", "description": "最多返回多少条，默认 30"}
        }
    }),
    list_processes
);

tool_impl!(
    KillProcess,
    "kill_process",
    "终止指定 pid 的进程（危险操作）。默认强制终止；拒绝 pid<=4 的系统关键进程和 NeoSonia 自身。",
    json!({
        "type": "object",
        "properties": {
            "pid": {"type": "integer", "description": "要终止的进程 id"},
            "force": {"type": "boolean", "description": "是否强制终止，默认 true"}
        },
        "required": ["pid"]
    }),
    kill_process,
    approval
);

// ============================================================
// 隐藏/恢复 NeoSonia 自己的主窗口
// ============================================================
//
// 只操作标题为 "NeoSonia" 的主窗口；紫色活动提示 HUD 是独立窗口
// （窗口类 NeoSoniaHudPill / NeoSoniaHudBar，无标题），按窗口类显式排除，绝不隐藏。

/// 判断一个窗口是否是 NeoSonia 主窗口（标题匹配 + 排除 HUD 窗口类）。
fn is_app_main_window(title: &str, class: &str) -> bool {
    let class_lower = class.trim().to_ascii_lowercase();
    if class_lower.starts_with("neosoniahud") {
        return false;
    }
    title.trim().eq_ignore_ascii_case("neosonia")
}

/// 窗口矩形是否完全在可视区域外（app 自己的"隐藏"实现是把窗口移到屏幕外）。
fn rect_is_offscreen(rect: (i32, i32, i32, i32), virtual_rect: (i32, i32, i32, i32)) -> bool {
    let (left, top, right, bottom) = rect;
    let (vx, vy, vw, vh) = virtual_rect;
    right <= vx || bottom <= vy || left >= vx + vw || top >= vy + vh
}

#[cfg(windows)]
mod app_win {
    use super::{is_app_main_window, rect_is_offscreen};
    use std::sync::{Mutex, OnceLock};

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn EnumWindows(
            lp_enum_func: unsafe extern "system" fn(isize, isize) -> i32,
            l_param: isize,
        ) -> i32;
        fn GetWindowTextW(hwnd: isize, lpstring: *mut u16, nmaxcount: i32) -> i32;
        fn GetWindowTextLengthW(hwnd: isize) -> i32;
        fn GetClassNameW(hwnd: isize, lpclassname: *mut u16, nmaxcount: i32) -> i32;
        fn IsWindowVisible(hwnd: isize) -> i32;
        fn ShowWindow(hwnd: isize, ncmdshow: i32) -> i32;
        fn GetWindowRect(hwnd: isize, lprect: *mut Rect) -> i32;
        fn SetWindowPos(
            hwnd: isize,
            hwndinsertafter: isize,
            x: i32,
            y: i32,
            cx: i32,
            cy: i32,
            uflags: u32,
        ) -> i32;
        fn GetSystemMetrics(nindex: i32) -> i32;
    }

    const SW_HIDE: i32 = 0;
    const SW_SHOW: i32 = 5;
    const SWP_NOZORDER: u32 = 0x0004;
    const SWP_NOSIZE: u32 = 0x0001;
    const SWP_NOACTIVATE: u32 = 0x0010;
    const SWP_SHOWWINDOW: u32 = 0x0040;
    const SM_XVIRTUALSCREEN: i32 = 76;
    const SM_YVIRTUALSCREEN: i32 = 77;
    const SM_CXVIRTUALSCREEN: i32 = 78;
    const SM_CYVIRTUALSCREEN: i32 = 79;

    #[derive(Clone, Debug)]
    struct WinEntry {
        hwnd: isize,
        title: String,
        class: String,
        visible: bool,
        rect: Option<(i32, i32, i32, i32)>,
    }

    fn read_window_text(hwnd: isize) -> String {
        // SAFETY: 先取长度再按长度分配缓冲区，buffer 一定足够。
        unsafe {
            let len = GetWindowTextLengthW(hwnd).max(0) as usize;
            let mut buf = vec![0u16; len + 1];
            let copied = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32).max(0) as usize;
            buf.truncate(copied);
            String::from_utf16_lossy(&buf)
        }
    }

    fn read_class_name(hwnd: isize) -> String {
        // SAFETY: 固定 256 的缓冲区，GetClassNameW 会按需截断。
        unsafe {
            let mut buf = vec![0u16; 256];
            let copied = GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32).max(0) as usize;
            buf.truncate(copied);
            String::from_utf16_lossy(&buf)
        }
    }

    unsafe extern "system" fn enum_windows_cb(hwnd: isize, lparam: isize) -> i32 {
        // SAFETY: lparam 是调用方传进来的 &mut Vec<WinEntry>，生命周期覆盖整个枚举过程。
        let list = unsafe { &mut *(lparam as *mut Vec<WinEntry>) };
        let title = read_window_text(hwnd);
        let class = read_class_name(hwnd);
        let mut rect = Rect::default();
        let rect = unsafe {
            if GetWindowRect(hwnd, &mut rect) != 0 {
                Some((rect.left, rect.top, rect.right, rect.bottom))
            } else {
                None
            }
        };
        list.push(WinEntry {
            hwnd,
            title,
            class,
            visible: unsafe { IsWindowVisible(hwnd) != 0 },
            rect,
        });
        1
    }

    fn virtual_screen_rect() -> (i32, i32, i32, i32) {
        // SAFETY: 只读系统度量。
        unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1),
                GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1),
            )
        }
    }

    fn main_windows() -> Vec<WinEntry> {
        all_windows()
            .into_iter()
            .filter(|entry| is_app_main_window(&entry.title, &entry.class))
            .collect()
    }

    fn all_windows() -> Vec<WinEntry> {
        let mut list: Vec<WinEntry> = Vec::new();
        // SAFETY: 回调只往 list 里 push；list 在调用期间有效。
        unsafe {
            EnumWindows(enum_windows_cb, (&mut list as *mut Vec<WinEntry>) as isize);
        }
        list
    }

    /// 测试用：列出所有 HUD 窗口（类名以 NeoSoniaHud 开头）及其可见性。
    #[cfg(test)]
    pub fn hud_windows_for_test() -> Vec<(String, bool)> {
        all_windows()
            .into_iter()
            .filter(|entry| entry.class.to_ascii_lowercase().starts_with("neosoniahud"))
            .map(|entry| (entry.class, entry.visible))
            .collect()
    }

    /// 测试用：列出可能的控制台窗口（类名/标题特征），用于断言子进程没有弹黑窗。
    #[cfg(test)]
    pub fn console_windows_for_test() -> Vec<(String, String)> {
        all_windows()
            .into_iter()
            .filter(|entry| {
                let class = entry.class.to_ascii_lowercase();
                let title = entry.title.to_ascii_lowercase();
                entry.visible
                    && (class.contains("console")
                        || class.contains("cascadia")
                        || title.contains("cmd.exe")
                        || title.contains("powershell")
                        || title.contains("pwsh"))
            })
            .map(|entry| (entry.class, entry.title))
            .collect()
    }

    /// 隐藏前记录主窗口位置，恢复时用（仅本进程内存，app 重启后失效）。
    fn saved_position() -> &'static Mutex<Option<(i32, i32)>> {
        static SAVED: OnceLock<Mutex<Option<(i32, i32)>>> = OnceLock::new();
        SAVED.get_or_init(|| Mutex::new(None))
    }

    fn save_position(position: Option<(i32, i32)>) {
        let mut guard = saved_position()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = position;
    }

    fn load_position() -> Option<(i32, i32)> {
        *saved_position()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn hide_app(hide: bool) -> Result<String, String> {
        let windows = main_windows();
        let Some(target) = windows.first().cloned() else {
            return Err(
                "没有找到 NeoSonia 主窗口（标题 \"NeoSonia\"）：app 可能没在运行，或窗口标题被修改。\
                 紫色 HUD 与其它窗口不会被操作。"
                    .to_string(),
            );
        };
        if hide {
            if let Some(rect) = target.rect {
                save_position(Some((rect.0, rect.1)));
            }
            // SAFETY: hwnd 来自 EnumWindows，且已按标题/类名过滤。
            unsafe {
                ShowWindow(target.hwnd, SW_HIDE);
            }
            let visible = unsafe { IsWindowVisible(target.hwnd) != 0 };
            if visible {
                return Err("隐藏 NeoSonia 主窗口失败（ShowWindow 后仍可见）。".to_string());
            }
            Ok(format!(
                "[ok] 已隐藏 NeoSonia 主窗口\n标题：{}\n窗口类：{}\nhwnd：0x{:X}\n\
                 紫色 HUD 是独立窗口，不受影响（保持显示）。\n用 window_hide_app(hide=false) 恢复。",
                target.title, target.class, target.hwnd
            ))
        } else {
            // SAFETY: 同上；SW_SHOW 恢复可见性，必要时把窗口移回屏幕内。
            unsafe {
                ShowWindow(target.hwnd, SW_SHOW);
            }
            let mut rect = Rect::default();
            let has_rect = unsafe { GetWindowRect(target.hwnd, &mut rect) != 0 };
            if has_rect {
                let current = (rect.left, rect.top, rect.right, rect.bottom);
                if rect_is_offscreen(current, virtual_screen_rect()) {
                    let (x, y) = load_position().unwrap_or((200, 120));
                    // SAFETY: 仅移动位置，不改尺寸/不激活。
                    unsafe {
                        SetWindowPos(
                            target.hwnd,
                            0,
                            x,
                            y,
                            0,
                            0,
                            SWP_NOZORDER | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                        );
                    }
                }
            }
            let visible = unsafe { IsWindowVisible(target.hwnd) != 0 };
            if !visible {
                return Err("恢复 NeoSonia 主窗口失败（仍不可见）。".to_string());
            }
            Ok(format!(
                "[ok] 已恢复 NeoSonia 主窗口\n标题：{}\nhwnd：0x{:X}",
                target.title, target.hwnd
            ))
        }
    }

    pub fn is_hidden() -> Result<(bool, String), String> {
        let windows = main_windows();
        let Some(target) = windows.first() else {
            return Err(
                "没有找到 NeoSonia 主窗口（标题 \"NeoSonia\"）：app 可能没在运行。".to_string(),
            );
        };
        let offscreen = target
            .rect
            .map(|rect| rect_is_offscreen(rect, virtual_screen_rect()))
            .unwrap_or(false);
        let hidden = !target.visible || offscreen;
        let reason = if !target.visible {
            "窗口不可见（IsWindowVisible=false，通常由 SW_HIDE / app 自身隐藏触发）"
        } else if offscreen {
            "窗口在屏幕外（app 自带的隐藏实现会把窗口移出可视区域）"
        } else {
            "窗口可见且在屏幕内"
        };
        let rect_text = target
            .rect
            .map(|(l, t, r, b)| format!("({l},{t})-({r},{b})"))
            .unwrap_or_else(|| "（未知）".to_string());
        Ok((
            hidden,
            format!(
                "[ok] NeoSonia 主窗口：{}\n判定：{reason}\nhwnd：0x{:X}\n标题：{}\n窗口类：{}\n窗口矩形：{rect_text}\n\
                 紫色 HUD 是独立窗口，不在本工具操作范围内。",
                if hidden { "隐藏" } else { "可见" },
                target.hwnd,
                target.title,
                target.class
            ),
        ))
    }
}

#[cfg(not(windows))]
mod app_win {
    pub fn hide_app(_hide: bool) -> Result<String, String> {
        Err("window_hide_app 目前只支持 Windows（需要 Win32 ShowWindow）。".to_string())
    }

    pub fn is_hidden() -> Result<(bool, String), String> {
        Err("window_is_hidden 目前只支持 Windows。".to_string())
    }
}

/// 测试用：HUD 窗口（类名 NeoSoniaHud*）及可见性。
#[cfg(all(test, windows))]
pub(crate) fn hud_windows_visible_for_test() -> Vec<(String, bool)> {
    app_win::hud_windows_for_test()
}

#[cfg(all(test, not(windows)))]
pub(crate) fn hud_windows_visible_for_test() -> Vec<(String, bool)> {
    Vec::new()
}

/// 测试用：当前可见的控制台窗口（类名/标题特征），用于断言子进程没有弹黑窗。
#[cfg(all(test, windows))]
pub(crate) fn console_windows_for_test() -> Vec<(String, String)> {
    app_win::console_windows_for_test()
}

#[cfg(all(test, not(windows)))]
pub(crate) fn console_windows_for_test() -> Vec<(String, String)> {
    Vec::new()
}

async fn window_hide_app(_ctx: &ToolCtx, args: Value) -> ToolOutcome {
    let hide = arg_bool(&args, "hide", true);
    match app_win::hide_app(hide) {
        Ok(message) => ToolOutcome::ok(message)
            .with_display(if hide { "已隐藏主窗口" } else { "已恢复主窗口" }.to_string()),
        Err(e) => ToolOutcome::err(e),
    }
}

async fn window_is_hidden(_ctx: &ToolCtx, _args: Value) -> ToolOutcome {
    match app_win::is_hidden() {
        Ok((hidden, message)) => ToolOutcome::ok(message)
            .with_display(if hidden { "主窗口已隐藏" } else { "主窗口可见" }.to_string()),
        Err(e) => ToolOutcome::err(e),
    }
}

// ============================================================
// 隐藏/恢复 NeoSonia 自己的主窗口
// ============================================================
//
// 只操作标题为 "NeoSonia" 的主窗口；紫色活动提示 HUD 是独立窗口
// （窗口类 NeoSoniaHudPill / NeoSoniaHudBar，无标题），按窗口类显式排除，绝不隐藏。

tool_impl!(
    WindowHideApp,
    "window_hide_app",
    "隐藏或恢复 NeoSonia 自己的主窗口（只操作标题为 \"NeoSonia\" 的主窗口）。\n\
     - hide=true（默认）：隐藏主窗口（ShowWindow SW_HIDE），让用户桌面不被挡住；\n\
     - hide=false：恢复显示（必要时把窗口移回屏幕内）。\n\
     紫色活动提示 HUD 是独立窗口（类名 NeoSoniaHudPill/NeoSoniaHudBar），**不会被隐藏**。\n\
     典型用法：需要连续操作屏幕/让用户看桌面时先隐藏，结束时恢复。",
    json!({
        "type": "object",
        "properties": {
            "hide": {"type": "boolean", "description": "true=隐藏主窗口（默认），false=恢复显示"}
        }
    }),
    window_hide_app
);

tool_impl!(
    WindowIsHidden,
    "window_is_hidden",
    "查询 NeoSonia 主窗口当前是否隐藏（返回隐藏/可见、判定依据、hwnd、窗口矩形）。\
     给模型自检用：隐藏后确认状态，或恢复后确认已可见。紫色 HUD 不在判断范围内。",
    json!({"type": "object", "properties": {}}),
    window_is_hidden
);

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn powershell_single_quote_escaping() {
        assert_eq!(ps_quote("it's"), "it''s");
        assert_eq!(ps_quote("plain"), "plain");
        assert_eq!(ps_quote("'quoted'"), "''quoted''");
    }

    #[test]
    fn foreground_render_without_rect() {
        let info = ForegroundInfo {
            title: "窗口 A".into(),
            pid: Some(1234),
            hwnd: "0x1A2B".into(),
            rect: None,
            process_name: Some("notepad.exe".into()),
        };
        let text = render_foreground(&info);
        assert!(text.contains("窗口 A"));
        assert!(text.contains("notepad.exe"));
        assert!(text.contains("1234"));
        assert!(!text.contains("窗口矩形"));
    }

    #[test]
    fn foreground_render_with_rect() {
        let info = ForegroundInfo {
            title: "B".into(),
            pid: None,
            hwnd: "0x1".into(),
            rect: Some((10, 20, 110, 220)),
            process_name: None,
        };
        let text = render_foreground(&info);
        assert!(text.contains("宽=100"));
        assert!(text.contains("高=200"));
    }

    #[test]
    fn window_title_matching() {
        assert!(title_matches("记事本 - 无标题", "记事本"));
        assert!(title_matches("Untitled - Notepad", "notepad"));
        assert!(title_matches("Calculator", "CALC"));
        assert!(!title_matches("Visual Studio Code", "notepad"));
        // 空 needle 视为匹配任意窗口
        assert!(title_matches("任何窗口", "  "));
    }

    #[test]
    fn collection_does_not_panic() {
        // 真实采集一次，只要求不 panic 且内容非空。
        let text = collect_system_info();
        assert!(text.contains("系统："));
        assert!(text.contains("CPU："));
    }

    #[tokio::test]
    async fn kill_process_rejects_protected_pids() {
        let root = crate::test_support::temp_root("killproc");
        let (ctx, _host) = crate::test_support::fake_ctx(&root);
        let out = kill_process(&ctx, json!({"pid": 4})).await;
        assert!(!out.ok);
        let out = kill_process(&ctx, json!({"pid": std::process::id()})).await;
        assert!(!out.ok);
        let out = kill_process(&ctx, json!({})).await;
        assert!(!out.ok);
    }

    #[tokio::test]
    async fn open_path_missing_file_errors() {
        let root = crate::test_support::temp_root("openpath");
        let (ctx, _host) = crate::test_support::fake_ctx(&root);
        let out = open_path(&ctx, json!({"target": "definitely-not-exist-12345.txt"})).await;
        assert!(!out.ok);
        assert!(out.content.contains("不存在"), "{}", out.content);
    }

    #[tokio::test]
    async fn list_processes_works() {
        let root = crate::test_support::temp_root("procs");
        let (ctx, _host) = crate::test_support::fake_ctx(&root);
        let out = list_processes(&ctx, json!({"limit": 5})).await;
        assert!(out.ok, "{}", out.content);
        assert!(out.content.contains("PID"));
    }

    #[test]
    fn app_main_window_filtering() {
        // 主窗口：标题 NeoSonia（大小写不敏感）
        assert!(is_app_main_window("NeoSonia", "FenestraWindow"));
        assert!(is_app_main_window("neosonia", "SomeClass"));
        assert!(is_app_main_window("  NeoSonia  ", ""));
        // HUD 窗口：即使标题碰巧包含 NeoSonia 也不能被当作主窗口
        assert!(!is_app_main_window("NeoSonia", "NeoSoniaHudPill"));
        assert!(!is_app_main_window("NeoSonia", "NeoSoniaHudBar"));
        assert!(!is_app_main_window("NeoSonia HUD", "neosoniahudbar"));
        // 其它窗口/子串不匹配
        assert!(!is_app_main_window("NeoSonia - 聊天", "X"));
        assert!(!is_app_main_window("Notepad", "NeoSoniaHudPill"));
        assert!(!is_app_main_window("", "NeoSoniaHudPill"));
    }

    #[test]
    fn offscreen_detection() {
        let virtual_rect = (0, 0, 3840, 2160);
        assert!(!rect_is_offscreen((100, 100, 900, 700), virtual_rect));
        // 完全在左边/上边/右边/下边的外面
        assert!(rect_is_offscreen((-30000, 100, -29000, 700), virtual_rect));
        assert!(rect_is_offscreen((100, -30000, 900, -29000), virtual_rect));
        assert!(rect_is_offscreen((4000, 100, 4900, 700), virtual_rect));
        assert!(rect_is_offscreen((100, 2200, 900, 2600), virtual_rect));
        // 部分可见不算 offscreen
        assert!(!rect_is_offscreen((-100, -50, 500, 300), virtual_rect));
        // 负坐标虚拟屏（多显示器）也算屏内
        let negative_screen = (-1920, 0, 5760, 2160);
        assert!(!rect_is_offscreen((-1500, 100, -500, 700), negative_screen));
    }

    #[tokio::test]
    async fn window_hide_tools_report_state() {
        // 只查询状态，不改变窗口（避免测试隐藏用户界面）。
        let root = crate::test_support::temp_root("win-hide");
        let (ctx, _host) = crate::test_support::fake_ctx(&root);
        let out = window_is_hidden(&ctx, json!({})).await;
        // 有主窗口时 ok 并给出判定；没有主窗口时也应返回中文错误而不是 panic。
        if out.ok {
            assert!(out.content.contains("NeoSonia 主窗口"), "{}", out.content);
            assert!(
                out.content.contains("隐藏") || out.content.contains("可见"),
                "{}",
                out.content
            );
            assert!(out.content.contains("紫色 HUD"), "{}", out.content);
        } else {
            assert!(out.content.contains("没有找到 NeoSonia 主窗口"), "{}", out.content);
        }
        // 参数非法不应触发真实 ShowWindow：hide 传字符串 "true" 会被 arg_bool 解析成 true…
        // 所以这里只验证无参调用（默认 hide=true）不会 panic；真正隐藏留给真机验证。
        let computed = arg_bool(&json!({"hide": "false"}), "hide", true);
        assert!(!computed);
    }
}
