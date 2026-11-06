//! NeoSonia 桌面 agent 入口。
//!
//! 右侧常驻浮窗 + 全局快捷键 + 本地 agent 循环。
//!
//! 发布版是纯 GUI 程序：Windows 下不分配控制台窗口（否则会挡住浮窗）；
//! 日志写到 `<root>/logs/neosonia.log`。
#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod attachments;
mod autostart;
mod bridge;
mod chat;
mod hotkey;
mod math;
mod overlay;
mod panels;
mod state;
mod theme;
mod tray;
mod warmup;
mod ui;
mod win;

use neosonia_core::config::{Paths, Settings};
use tracing_subscriber::EnvFilter;

/// 崩溃/panic 记录：写到 `<root>/logs/crash-<时间>.txt`，方便事后定位"执行操作时崩溃"。
fn install_crash_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let dir = std::env::var("NEOSONIA_CRASH_DIR").unwrap_or_else(|_| ".".to_string());
        let _ = std::fs::create_dir_all(&dir);
        let path = std::path::Path::new(&dir).join(format!("crash-{now}.txt"));
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("<unnamed>");
        let loc = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".into());
        let msg = format!(
            "panic at {loc}\nthread: {name}\npayload: {:?}\n\nbacktrace:\n{}\n",
            info,
            std::backtrace::Backtrace::force_capture()
        );
        let _ = std::fs::write(&path, msg.as_bytes());
        eprintln!("[crash] {path:?}");
        default_hook(info);
    }));
}

fn main() {
    install_crash_hook();
    // 崩溃日志默认写到 data/logs 旁的 logs/ 目录
    if std::env::var("NEOSONIA_CRASH_DIR").is_err() {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let _ = std::fs::create_dir_all(dir.join("logs"));
                // SAFETY: 单线程启动阶段设置环境变量
                unsafe {
                    std::env::set_var("NEOSONIA_CRASH_DIR", dir.join("logs"));
                }
            }
        }
    }
    // 混合显卡机器上，vello/wgpu 默认可能选到 Vulkan 设备（dGPU），
    // 但它渲染出的 swapchain 在集显驱动的桌面上会呈现为空白窗口。
    // 本机实测（Intel HD 530 + GTX 960M）：Vulkan -> 白屏，DX12 -> 正常。
    // 所以 Windows 默认锁 DX12；需要时仍可用环境变量 WGPU_BACKEND 覆盖。
    #[cfg(windows)]
    if std::env::var_os("WGPU_BACKEND").is_none() {
        // SAFETY: 单线程启动阶段、在任何 wgpu 初始化之前设置环境变量。
        unsafe {
            std::env::set_var("WGPU_BACKEND", "dx12");
        }
    }

    let mut home_override = None;
    let mut probe = false;
    let mut ask: Option<String> = None;
    let mut http_probe: Option<String> = None;
    let mut autostart_mode = false;
    let mut uninstall_mode = false;
    let mut uninstall_silent = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--home" => home_override = args.next(),
            "--autostart" => autostart_mode = true,
            "--uninstall" => uninstall_mode = true,
            "--silent" => uninstall_silent = true,
            "--help" | "-h" => {
                println!("NeoSonia — 常驻桌面的 AI agent");
                println!("用法: neosonia [--home <目录>] [--probe] [--autostart]");
                println!("  --home <目录>   指定配置/数据根目录（默认: 仓库根或系统数据目录）");
                println!("  --probe         只开一个最小测试窗口（排查 GPU/窗口问题）");
                println!("  --ask <指令>    无界面跑一轮 agent（联调/脚本用），把过程打到 stdout");
                println!("  --autostart     开机自启用：只跑后端，不打开窗口（热键/托盘可唤出）");
                println!("  --uninstall     卸载：删安装目录（含配置/数据）、注册表项与快捷方式");
                println!("  --silent        与 --uninstall 配合：不弹确认框");
                println!("快捷键: 默认 Alt+Space 显示/隐藏窗口，可在设置页修改");
                return;
            }
            "--probe" => probe = true,
            "--ask" => ask = args.next(),
            "--http-probe" => http_probe = args.next(),
            other => {
                eprintln!("未知参数: {other}（用 --help 查看用法）");
            }
        }
    }

    // 卸载入口：必须早于任何目录/日志/窗口初始化（安装目录随后由 cmd 延迟删除）。
    if uninstall_mode {
        std::process::exit(run_uninstall(uninstall_silent));
    }

    let paths = match home_override {
        Some(h) => Paths::from_root(h),
        None => Paths::resolve(),
    };
    if let Err(e) = paths.ensure() {
        eprintln!("无法创建数据目录 {}: {e}", paths.root.display());
        return;
    }

    init_logging(&paths);

    if let Some(url) = http_probe {
        run_http_probe(&url);
        return;
    }

    let mut settings = match Settings::load_or_create(&paths) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("读取配置失败: {e}（使用默认配置继续）");
            Settings::default()
        }
    };

    // 开机自启模式：只跑后端，不主动打开窗口（热键/托盘唤出）。
    if autostart_mode {
        tracing::info!("autostart mode: window starts hidden");
    }

    if let Some(prompt) = ask {
        run_headless_ask(&paths, &settings, prompt);
        return;
    }

    if probe {
        fenestra::run(
            ProbeApp { n: 0 },
            fenestra::WindowOptions::titled("Probe").with_size(420.0, 320.0),
        );
        return;
    }

    tracing::info!(
        root = %paths.root.display(),
        model = %settings.server.model,
        hotkey = %settings.ui.hotkey,
        start_hidden = settings.ui.start_hidden,
        "NeoSonia starting"
    );

    let app = app::NeoSoniaApp::new_with_boot_hidden(paths, settings.clone(), autostart_mode);

    // 环境变量逃生口（排查透明窗口/合成器问题时用）：
    //   NEOSONIA_WIN_TRANSPARENT=0  关闭透明窗口
    //   NEOSONIA_WIN_DECORATIONS=1  打开系统标题栏
    //   NEOSONIA_WIN_TOPMOST=0      关闭置顶
    let transparent = std::env::var("NEOSONIA_WIN_TRANSPARENT")
        .map(|v| v != "0")
        .unwrap_or(!cfg!(windows));
    let decorations = std::env::var("NEOSONIA_WIN_DECORATIONS")
        .map(|v| v == "1")
        .unwrap_or(settings.ui.decorations);
    let topmost = std::env::var("NEOSONIA_WIN_TOPMOST")
        .map(|v| v != "0")
        .unwrap_or(settings.ui.always_on_top);

    // 无边框 + 置顶 + 透明（圆角浮窗需要）、不进任务栏。
    let mut options = fenestra::WindowOptions::titled("NeoSonia")
        .with_size(settings.ui.width, 780.0)
        .with_min_size(360.0, 480.0)
        .with_resizable(true)
        .with_always_on_top(topmost)
        .with_decorations(decorations)
        .with_transparent(transparent)
        .with_skip_taskbar(false);

    // 用项目 logo 做窗口图标（编译期嵌入的 PNG 解码成 RGBA）。
    if let Some((w, h, rgba)) = window_icon() {
        options = options.with_icon(w, h, rgba);
    }

    tracing::info!(
        transparent,
        decorations,
        topmost,
        width = settings.ui.width,
        "window options"
    );

    if let Err(e) = fenestra::shell::run_app(app, options) {
        tracing::error!("窗口启动失败: {e}");
        #[cfg(windows)]
        win::message_box_error(
            "NeoSonia 启动失败",
            &format!("{e}\n\n提示: 无 GPU 环境可设置 FENESTRA_CPU=1 使用 CPU 渲染"),
        );
    }
}

/// 日志同时写文件（`<root>/logs/neosonia.log`）和 stderr。
fn init_logging(paths: &Paths) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("neosonia=info,neosonia_core=info,neosonia_tools=info,neosonia_mail=info,warn")
    });

    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.logs.join("neosonia.log"))
        .ok();

    use tracing_subscriber::prelude::*;
    let fmt_layer = tracing_subscriber::fmt::layer().with_target(false).with_ansi(false);

    match file {
        Some(f) => {
            let file_layer = tracing_subscriber::fmt::layer()
                .with_writer(std::sync::Mutex::new(f))
                .with_ansi(false)
                .with_target(false);
            let _ = tracing_subscriber::registry()
                .with(filter)
                .with(fmt_layer)
                .with(file_layer)
                .try_init();
        }
        None => {
            let _ = tracing_subscriber::registry().with(filter).with(fmt_layer).try_init();
        }
    }
}

// ============================================================
// 最小测试窗口：排查 GPU / 窗口合成问题用
// ============================================================

struct ProbeApp {
    n: i64,
}

#[derive(Clone)]
enum ProbeMsg {
    Inc,
}

impl fenestra::prelude::App for ProbeApp {
    type Msg = ProbeMsg;

    fn update(&mut self, msg: ProbeMsg) {
        match msg {
            ProbeMsg::Inc => self.n += 1,
        }
    }

    fn view(&self) -> fenestra::prelude::Element<ProbeMsg> {
        use fenestra::prelude::*;
        col()
            .w_full()
            .h_full()
            .p(SP4)
            .gap(SP3)
            .bg(Theme::dark().bg)
            .items_center()
            .justify_center()
            .children((
                text(format!("fenestra probe {}", self.n))
                    .size(TextSize::Xl)
                    .color(Theme::dark().text),
                button("+1").on_click(ProbeMsg::Inc),
            ))
            .into()
    }
}

// ============================================================
// 无界面跑一轮 agent（联调 / 脚本）
// ============================================================

fn run_headless_ask(paths: &Paths, settings: &Settings, prompt: String) {
    use neosonia_core::agent::{AgentEngine, EngineConfig, EventSink};
    use neosonia_core::memory::MemoryStore;
    use neosonia_core::model::AgentEvent;
    use neosonia_core::persona::PersonaStore;
    use neosonia_core::registry::ToolRegistry;
    use parking_lot::RwLock;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("runtime 创建失败: {e}");
            return;
        }
    };

    let registry = ToolRegistry::new();
    neosonia_tools::register_all(&registry, settings);
    neosonia_mail::register(&registry);
    eprintln!("[tools] {} 个", registry.len());

    // ask 模式也支持 MCP（和 GUI 用同一套管理器）
    let settings_arc = Arc::new(RwLock::new(settings.clone()));
    if settings.mcp.enabled {
        match rt.block_on(neosonia_mcp::McpManager::start(
            settings_arc.clone(),
            &registry,
            paths,
        )) {
            Ok(mgr) => {
                let servers = mgr.servers();
                let total: usize = servers.iter().map(|s| s.tools).sum();
                eprintln!("[mcp] {} 台服务器 / {} 个工具", servers.len(), total);
                for s in servers {
                    eprintln!(
                        "[mcp] {} -> ok={} tools={} {}",
                        s.name,
                        s.ok,
                        s.tools,
                        s.error.unwrap_or_default()
                    );
                }
                std::mem::forget(mgr);
            }
            Err(e) => eprintln!("[mcp] 启动失败: {e}"),
        }
    }

    let memory = match MemoryStore::open(&paths.memory_db) {
        Ok(m) => Arc::new(m),
        Err(e) => {
            eprintln!("记忆库打开失败: {e}");
            return;
        }
    };
    let personas = PersonaStore::load(&paths.personas);
    let conversations = match neosonia_core::ConversationStore::open(&paths.conversations_db) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!("会话库打开失败: {e}");
            return;
        }
    };
    let skills = neosonia_core::SkillStore::load(&paths.skills);
    let settings_arc = Arc::new(RwLock::new(settings.clone()));

    let done = Arc::new(AtomicBool::new(false));
    let done2 = done.clone();
    let sink: EventSink = Arc::new(move |ev: AgentEvent| match ev {
        AgentEvent::TextDelta(t) => {
            use std::io::Write;
            print!("{t}");
            let _ = std::io::stdout().flush();
        }
        AgentEvent::ReasoningDelta(_) => {}
        AgentEvent::Phase(p) => eprintln!("\n[phase] {p}"),
        AgentEvent::ToolStarted { name, preview, .. } => {
            eprintln!("\n[tool] {name} {}", preview.unwrap_or_default());
        }
        AgentEvent::ToolFinished { name, ok, elapsed_ms, output, .. } => {
            let first = output.lines().next().unwrap_or("").chars().take(160).collect::<String>();
            eprintln!(
                "[tool] {name} {} {}ms: {first}",
                if ok { "ok" } else { "FAIL" },
                elapsed_ms
            );
        }
        AgentEvent::SubagentStarted { label, .. } => eprintln!("\n[subagent] {label} 启动"),
        AgentEvent::SubagentFinished { label, ok, summary, .. } => {
            eprintln!("[subagent] {label} {}: {}", if ok { "ok" } else { "FAIL" }, summary.lines().next().unwrap_or(""))
        }
        AgentEvent::Usage { prompt_tokens, completion_tokens, .. } => {
            eprintln!("\n[usage] in={prompt_tokens} out={completion_tokens}");
        }
        AgentEvent::Error(e) => eprintln!("\n[error] {e}"),
        AgentEvent::ImagesAttached { count, source } => {
            eprintln!("\n[vision] 已把 {count} 张图片发给模型（{source}）")
        }
        AgentEvent::ConversationStarted { conversation_id, title } => {
            eprintln!("\n[conversation] {conversation_id} {title}")
        }
        AgentEvent::TurnFinished { cancelled, .. } => {
            eprintln!("\n--- turn finished (cancelled={cancelled}) ---");
            done2.store(true, Ordering::SeqCst);
        }
        _ => {}
    });

    let cfg = EngineConfig {
        settings: settings_arc,
        paths: paths.clone(),
        registry,
        memory,
        personas,
        conversations,
        skills,
    };

    let engine = {
        let _enter = rt.enter();
        match AgentEngine::start(cfg, sink) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("引擎启动失败: {e}");
                return;
            }
        }
    };

    println!("[user] {prompt}");
    engine.submit(prompt);

    let start = std::time::Instant::now();
    while !done.load(Ordering::SeqCst) {
        if start.elapsed().as_secs() > 600 {
            eprintln!("\n[timeout] 600s");
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
    engine.shutdown();
    std::thread::sleep(std::time::Duration::from_millis(300));
}

// ============================================================
// HTTP 连通性探针（排查 TLS/代理/证书问题）
// ============================================================

fn run_http_probe(url: &str) {
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("runtime 失败: {e}");
            return;
        }
    };
    rt.block_on(async {
        for (label, no_proxy) in [("default", false), ("no_proxy", true)] {
            let mut b = reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(15))
                .timeout(std::time::Duration::from_secs(30));
            if no_proxy {
                b = b.no_proxy();
            }
            let client = match b.build() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("[{label}] client build failed: {e}");
                    continue;
                }
            };
            match client.get(url).send().await {
                Ok(resp) => println!("[{label}] HTTP {}", resp.status()),
                Err(e) => {
                    println!("[{label}] ERROR: {e}");
                    let mut src: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(&e);
                    let mut depth = 0;
                    while let Some(s) = src {
                        println!("    cause[{depth}]: {s}");
                        src = std::error::Error::source(s);
                        depth += 1;
                        if depth > 6 {
                            break;
                        }
                    }
                }
            }
        }
    });
}

/// 窗口图标：项目 logo（`assets/icon-256.png`，编译期嵌入）解码成 RGBA。
fn window_icon() -> Option<(u32, u32, Vec<u8>)> {
    static PNG: &[u8] = include_bytes!("../../../assets/icon-256.png");
    let img = image::load_from_memory(PNG).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Some((w, h, rgba.into_raw()))
}
/// 卸载：不依赖独立的 Uninstall.exe（第三方安全软件会拦截/隔离自删除的卸载器），
/// 由安装目录里的 neosonia.exe --uninstall 完成：
/// 删注册表项/快捷方式，然后交给分离的 cmd 在进程退出后删安装目录。
#[cfg(windows)]
fn run_uninstall(silent: bool) -> i32 {
    use std::os::windows::process::CommandExt;
    use windows::Win32::UI::WindowsAndMessaging::{
        IDYES, MB_ICONQUESTION, MB_YESNO, MessageBoxW,
    };

    let dir = match std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())) {
        Some(dir) => dir,
        None => return 1,
    };
    if !silent {
        let text = format!(
            "确定卸载 NeoSonia？\n\n将删除安装目录（含配置/数据/workspace/skills）以及快捷方式和「应用和功能」条目：\n{}",
            dir.display()
        );
        // SAFETY: 纯 Win32 消息框调用，字符串在调用期间有效。
        let result = unsafe {
            MessageBoxW(
                None,
                &windows::core::HSTRING::from(text),
                &windows::core::HSTRING::from("卸载 NeoSonia"),
                MB_YESNO | MB_ICONQUESTION,
            )
        };
        if result != IDYES {
            return 1;
        }
    }

    // 1) 注册表项（「应用和功能」）
    let _ = std::process::Command::new("reg.exe")
        .args([
            "delete",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\NeoSonia",
            "/f",
        ])
        .creation_flags(0x0800_0000)
        .status();

    // 2) 快捷方式（开始菜单目录 + 桌面）
    if let Ok(appdata) = std::env::var("APPDATA") {
        let menu = std::path::Path::new(&appdata)
            .join(r"Microsoft\Windows\Start Menu\Programs\NeoSonia");
        let _ = std::fs::remove_dir_all(menu);
    }
    if let Ok(profile) = std::env::var("USERPROFILE") {
        let desktop = std::path::Path::new(&profile).join(r"Desktop\NeoSonia.lnk");
        let _ = std::fs::remove_file(desktop);
    }

    // 3) 进程退出后删目录：写一个批处理（比把 & 命令塞进 /c 参数可靠），
    //    先 cd 到系统盘避免"目录是当前工作目录"导致 rmdir 失败，延迟后结束其它
    //    实例（它们会锁住 exe/配置），再递归删除；结果写到 %TEMP% 便于排查。
    let log = std::env::temp_dir().join("neosonia-uninstall.log");
    let batch = std::env::temp_dir().join("neosonia-uninstall.cmd");
    let body = format!(
        "@echo off\r\ncd /d \"%SystemRoot%\"\r\nping 127.0.0.1 -n 4 >nul\r\ntaskkill /IM neosonia.exe /F >nul 2>&1\r\nrmdir /s /q \"{dir}\"\r\nif exist \"{dir}\" (echo FAILED %date% %time% >> \"{log}\") else (echo OK %date% %time% >> \"{log}\")\r\ndel \"%~f0\"\r\n",
        dir = dir.display(),
        log = log.display()
    );
    let _ = std::fs::write(&batch, body);
    let _ = std::process::Command::new("cmd.exe")
        .args(["/c", &batch.to_string_lossy().to_string()])
        .creation_flags(0x0800_0000 | 0x0000_0008) // CREATE_NO_WINDOW | DETACHED_PROCESS
        .spawn();
    0
}

#[cfg(not(windows))]
fn run_uninstall(silent: bool) -> i32 {
    let _ = silent;
    let Ok(exe) = std::env::current_exe() else {
        return 1;
    };
    let Some(dir) = exe.parent() else {
        return 1;
    };
    match std::fs::remove_dir_all(dir) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("卸载失败: {e}");
            1
        }
    }
}