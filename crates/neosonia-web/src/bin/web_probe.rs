//! web-remote 真机验证探针（不是产品代码）。
//!
//! 用途：在开发机上真实拉起 `WebServer`，给 curl / 浏览器截图 / node WebSocket
//! 测试提供证据。数据全部落在 `logs/web-probe-home/` 下，不碰真实 `data/`。
//!
//! 用法：
//! ```powershell
//! cargo run -p neosonia-web --bin web_probe -- --port 0 --seconds 120
//! ```
//! 参数：
//! - `--port <u16>`：监听端口，0 = 系统分配（默认 0）
//! - `--seconds <u64>`：运行多少秒后自动退出，0 = 等 Ctrl+C（默认 0）
//!
//! 启动后会把配对信息写到 `<repo>/logs/web-probe.json` 并打印 `READY ...` 行。

use std::sync::Arc;
use std::time::Duration;

use neosonia_core::{
    AgentEngine, AgentEvent, ChatMessage, ConversationStore, EngineConfig, MemoryStore, Paths,
    PersonaStore, Settings, SkillStore, ToolRegistry,
};
use neosonia_web::{ReviewItem, WebConfig, WebServer};
use parking_lot::RwLock;
use serde_json::json;

fn arg_value(args: &[String], name: &str) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == name {
            return it.next().cloned();
        }
        if let Some(rest) = a.strip_prefix(&format!("{name}=")) {
            return Some(rest.to_string());
        }
    }
    None
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let port: u16 = arg_value(&args, "--port")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let seconds: u64 = arg_value(&args, "--seconds")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let repo_root = Paths::resolve().root;
    let home = repo_root.join("logs").join("web-probe-home");
    // 每次探针都从干净环境开始，避免旧 devices.json 影响配对结果。
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home)?;
    let paths = Paths::from_root(&home);
    paths.ensure()?;

    let mut settings = Settings::default();
    settings.remote.enabled = true;
    settings.remote.bind = "127.0.0.1".to_string();
    settings.remote.port = port;
    settings.remote.require_pairing = true;
    settings.remote.pair_ttl_secs = 1800;
    settings.remote.max_devices = 8;
    settings.remote.lan_only = true;
    settings.ui.start_hidden = true;
    let settings = Arc::new(RwLock::new(settings));

    let memory = Arc::new(MemoryStore::open(&paths.memory_db)?);
    let personas = PersonaStore::load(&paths.personas);
    let skills = SkillStore::load(&paths.skills);
    let conversations = Arc::new(ConversationStore::open(&paths.conversations_db)?);

    // 造一条历史会话，方便 curl 验证 /api/sessions 与 /api/sessions/{id}。
    let conv_id = conversations.ensure(Some("probe-conv-1"), "探针测试会话", "default")?;
    conversations.append(
        &conv_id,
        &ChatMessage::user("探针：列出工作目录并检查文件"),
    )?;
    conversations.append(
        &conv_id,
        &ChatMessage::assistant("探针：这是历史里的助手回复，用于验证会话读取。"),
    )?;

    // 造一条 240 条消息的长会话，验证 ?before=<seq>&limit= 分页（> 200 条触发首屏虚拟化）。
    let long_id = conversations.ensure(Some("probe-long"), "长会话分页测试", "default")?;
    for i in 1..=240u32 {
        let msg = if i % 2 == 1 {
            ChatMessage::user(format!("分页测试消息 {i}：用于验证远端加载更早消息。"))
        } else {
            ChatMessage::assistant(format!("分页测试回复 {i}：seq 窗口应连续且不重叠。"))
        };
        conversations.append(&long_id, &msg)?;
    }

    // 造一张假截图，验证 /api/screenshot/{name} 的鉴权与防穿越。
    let shot_path = paths.screenshots.join("probe-shot.png");
    let img = image::ImageBuffer::from_pixel(240, 160, image::Rgba([28u8, 24, 36, 255]));
    img.save(&shot_path)?;

    let (events_tx, _) = tokio::sync::broadcast::channel::<AgentEvent>(1024);
    let sink_tx = events_tx.clone();
    let sink: neosonia_core::EventSink = Arc::new(move |ev| {
        let _ = sink_tx.send(ev);
    });

    let engine = Arc::new(AgentEngine::start(
        EngineConfig {
            settings: settings.clone(),
            paths: paths.clone(),
            registry: ToolRegistry::new(),
            memory,
            personas,
            conversations: conversations.clone(),
            skills,
        },
        sink,
    )?);

    let server = WebServer::start(WebConfig {
        paths: paths.clone(),
        settings: settings.clone(),
        engine: engine.clone(),
        conversations: conversations.clone(),
        events: events_tx.clone(),
        bind: Some("0.0.0.0".to_string()),
        advertise_url: None,
    })?;

    let info = server.info();
    let qr = info
        .qr_png
        .as_ref()
        .map(|p| p.to_string_lossy().to_string());

    // 审查队列演示数据：验证 /api/review/pending 与 WS risk_review（只读）。
    let hub = neosonia_web::review_hub();
    hub.clear();
    hub.push(ReviewItem {
        call_id: "probe-review-1".to_string(),
        tool: "run_command".to_string(),
        args_summary: "pwsh -NoProfile -Command \"Get-ChildItem C:\\Windows\\System32\"".to_string(),
        risk_level: "high".to_string(),
        risk_reason: "执行系统命令并读取系统目录".to_string(),
        class: "shell".to_string(),
        emphasized: true,
    });
    hub.push(ReviewItem {
        call_id: "probe-review-2".to_string(),
        tool: "write_file".to_string(),
        args_summary: "config/settings.toml（覆盖写入）".to_string(),
        risk_level: "medium".to_string(),
        risk_reason: "会修改桌面端配置文件".to_string(),
        class: "fs".to_string(),
        emphasized: false,
    });

    let report = json!({
        "port": server.port(),
        "bound": server.bound_addr().to_string(),
        "lan_url": server.lan_url(),
        "pair_url": info.url,
        "code": info.code,
        "expires_at_ms": info.expires_at_ms,
        "qr_png": qr,
        "conv_id": conv_id,
        "long_conv_id": long_id,
        "review_pending": hub.len(),
        "screenshot": "probe-shot.png",
        "home": home.to_string_lossy(),
        "pid": std::process::id(),
    });
    let report_path = repo_root.join("logs").join("web-probe.json");
    std::fs::write(&report_path, serde_json::to_string_pretty(&report)?)?;

    println!("READY {}", serde_json::to_string(&report)?);
    println!("REPORT {}", report_path.display());
    println!("PROBE 数据目录 {}", home.display());
    println!("PROBE 配对码 {} 配对链接 {}", info.code, info.url);

    // 每 3 秒往广播里推一条心跳，验证 WS 转发链路（不依赖模型）。
    let heartbeat = events_tx.clone();
    tokio::spawn(async move {
        let mut n: u32 = 0;
        loop {
            tokio::time::sleep(Duration::from_secs(3)).await;
            n += 1;
            let _ = heartbeat.send(AgentEvent::Notice(format!("探针心跳 #{n}")));
        }
    });

    // 每 6 秒更新一条审查项，验证 WS risk_review 广播（只读展示）。
    tokio::spawn(async move {
        let hub = neosonia_web::review_hub();
        let mut n: u32 = 0;
        loop {
            tokio::time::sleep(Duration::from_secs(6)).await;
            n += 1;
            hub.push(ReviewItem {
                call_id: "probe-review-1".to_string(),
                tool: "run_command".to_string(),
                args_summary: "pwsh -NoProfile -Command \"Get-ChildItem C:\\Windows\\System32\"".to_string(),
                risk_level: "high".to_string(),
                risk_reason: format!("执行系统命令并读取系统目录（风险复查 #{n}）"),
                class: "shell".to_string(),
                emphasized: true,
            });
        }
    });

    if seconds > 0 {
        println!("PROBE 将在 {seconds} 秒后自动退出");
        tokio::time::sleep(Duration::from_secs(seconds)).await;
    } else {
        println!("PROBE 运行中，Ctrl+C 退出");
        let _ = tokio::signal::ctrl_c().await;
    }

    server.stop();
    engine.shutdown();
    println!("PROBE 已停止");
    Ok(())
}
