//! fenestra 应用主体：把 UI 消息、后台引擎事件和窗口控制接起来。

use std::sync::Arc;
use std::time::Duration;

use fenestra::prelude::*;
use neosonia_core::config::{Paths, Settings, ThemeMode};
use neosonia_core::model::{AgentEvent, Persona};

use crate::bridge::Bridge;
use crate::hotkey::{HotkeyAction, Hotkeys};
use crate::state::{
    AppState, ChatItem, Msg, NavPage, NoticeLevel, SettingValue, SubagentCard, TestResult,
    TestTarget, ToolCard, ToolState,
};
use crate::theme::{self, UiColors};
use crate::ui;

pub struct NeoSoniaApp {
    tray: Option<crate::tray::Tray>,
    tick_count: u64,
    pub st: AppState,
    /// 屏幕活动提示（调用截图/键鼠/UIA 工具时在屏幕上提示）。
    overlay: crate::overlay::ActivityOverlay,
    bridge: Option<Bridge>,
    /// 全局快捷键（键盘钩子 + 组合键）。
    hotkeys: Hotkeys,
    docked: bool,
    /// 正常（可见）时的窗口位置；隐藏 = 移到屏幕外，避免 SW_HIDE 让全局热键失效。
    dock_pos: Option<(f64, f64)>,
    last_drag_ms: i64,
    system_dark: bool,
    /// 主题缓存（每帧重建 Theme 会重复算色板，缓存后只在主题/缩放变化时重算）。
    theme_cache: std::cell::RefCell<Option<(ThemeMode, bool, u64, Theme, UiColors)>>,
    last_history_refresh_ms: i64,
    /// 自启模式下只隐藏一次窗口。
    hidden_on_start: bool,
    /// 最近一次点击导航的时间（用来量页面切换延迟）。
    nav_started_at: std::cell::Cell<Option<std::time::Instant>>,
    /// 帧计数 / 统计窗口起点（NEOSONIA_PROFILE 用）。
    frames: std::cell::Cell<u64>,
    fps_since: std::cell::Cell<std::time::Instant>,
    /// 本次进程是否自启（窗口默认隐藏）。
    boot_hidden: bool,
    /// 上次同步过的"开机自启"值；只有变化时才去 spawn reg（用户报展开/切页/聚焦就弹 reg.exe）。
    last_autostart: Option<bool>,
    /// 热键 toggle 等修饰键抬起后再执行（避免隐藏窗口吞掉按键抬起）。
    pending_toggle: bool,
}

impl NeoSoniaApp {
    pub fn new(paths: Paths, settings: Settings) -> Self {
        Self::new_with_boot_hidden(paths, settings, false)
    }

    /// `boot_hidden`：本次启动是否因为开机自启而隐藏窗口（不写回配置）。
    pub fn new_with_boot_hidden(paths: Paths, settings: Settings, boot_hidden: bool) -> Self {
        let mut st = AppState::new(paths, settings);
        st.agent_mode = st.settings.agent.mode.id().to_string();
        Self {
            st,
            tray: None,
            tick_count: 0,
            bridge: None,
            hotkeys: Hotkeys::new(),
            docked: false,
            dock_pos: None,
            last_drag_ms: 0,
            system_dark: true,
            theme_cache: std::cell::RefCell::new(None),
            boot_hidden,
            nav_started_at: std::cell::Cell::new(None),
        pending_toggle: false,
        last_autostart: None,
            frames: std::cell::Cell::new(0),
            fps_since: std::cell::Cell::new(std::time::Instant::now()),
            overlay: crate::overlay::ActivityOverlay::new(),
            last_history_refresh_ms: 0,
            hidden_on_start: false,
        }
    }

    fn colors(&self) -> UiColors {
        self.app_theme();
        self.theme_cache
            .borrow()
            .as_ref()
            .map(|(_, _, _, _, c)| *c)
            .unwrap_or_else(|| theme::colors(&self.app_theme()))
    }

    /// 把当前界面用 headless 渲染器画成 PNG（`logs/headless-ui.png`）。
    /// 注意 headless 只用内嵌 Inter 字体，中文会显示成方框——它只用来确认
    /// 布局/颜色/控件是否正常，不用于中文排版验证。
    fn dump_headless_ui(&self) {
        let th = self.app_theme();
        let c = theme::colors(&th);
        let el = ui::root(&self.st, &c);
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            fenestra::shell::render_element(el, &th, (424, 760))
        })) {
            Ok(img) => {
                let path = self.st.paths.logs.join("headless-ui.png");
                match img.save(&path) {
                    Ok(()) => tracing::info!(path = %path.display(), "headless UI dumped"),
                    Err(e) => tracing::warn!("headless UI save failed: {e}"),
                }
            }
            Err(_) => tracing::warn!("headless UI render panicked"),
        }
    }

    fn app_theme(&self) -> Theme {
        let mode = self.st.theme_mode;
        let scale = self.st.settings.ui.font_scale;
        let key = (mode, self.system_dark, scale.to_bits());
        if let Some((m, sd, s, t, _)) = self.theme_cache.borrow().as_ref() {
            if (*m, *sd, *s) == key {
                return t.clone();
            }
        }
        let theme = theme::nova_theme(mode, self.system_dark, scale);
        let colors = theme::colors(&theme);
        *self.theme_cache.borrow_mut() =
            Some((mode, self.system_dark, scale.to_bits(), theme.clone(), colors));
        theme
    }

    // ---------------- 启动 ----------------

    fn boot(&mut self, proxy: Proxy<Msg>) {
        // 预热：后台线程把界面里会用到的中文/常用字形先渲染一遍，
        // 避免第一次切到设置/人设页时 parley 现找字体回退导致 1-2 秒卡顿。
        crate::warmup::spawn(self.app_theme());
        self.st.proxy = Some(proxy.clone());

        // 自检：把界面离线渲染成 PNG，用来验证 view 树本身没问题。
        // NEOSONIA_PAGE=settings|memory|mail|persona 可指定要 dump 的页面。
        if let Ok(page) = std::env::var("NEOSONIA_PAGE") {
            self.st.page = match page.as_str() {
                "settings" => NavPage::Settings,
                "memory" => NavPage::Memory,
                "mail" => NavPage::Mail,
                "persona" => NavPage::Persona,
                "history" => NavPage::History,
                "remote" => NavPage::Remote,
                _ => NavPage::Chat,
            };
            self.refresh_memory();
            if self.st.personas.is_empty() {
                self.st.personas = vec![Persona::builtin_default()];
            }
        }
        if std::env::var("NEOSONIA_DUMP_UI").ok().as_deref() == Some("1") {
            self.dump_headless_ui();
        }

        match Bridge::start(&self.st.paths, self.st.settings.clone(), proxy.clone()) {
            Ok(b) => {
                self.st.personas = b.personas.list();
                self.st.memory_blocks = b.memory.blocks().unwrap_or_default();
                self.st.memory_items = b.memory.recent(30).unwrap_or_default();
                self.st.skills = b.skill_list();
                self.st.history.sessions = b.list_conversations("");
                self.st.history.current_id = b.conversation_id();
                self.bridge = Some(b);
                // 局域网服务状态要在 bridge 装好之后再拉（sync_remote_state 依赖 self.bridge）
                if self.st.settings.remote.enabled {
                    self.sync_remote_state();
                }
                // MCP 服务器运行态
                if let Some(b) = &self.bridge {
                    if b.mcp_enabled() {
                        self.st.mcp.servers = b.mcp_servers();
                    }
                }
            }
            Err(e) => {
                self.st.push_notice(
                    format!("后台引擎启动失败: {e}（仍可在设置页修改配置后重启）"),
                    NoticeLevel::Error,
                );
                self.st.personas = vec![Persona::builtin_default()];
            }
        }

        // 全局快捷键：主键切换显隐；主键后按 ui.screenshot_chord（默认 S）= 立即截图发模型。
        if let Some(proxy) = self.st.proxy.clone() {
            let p = proxy.clone();
            match self.hotkeys.install(
                &self.st.settings.ui.hotkey,
                &self.st.settings.ui.screenshot_hotkey,
                "",
                self.st.settings.ui.chord_timeout_ms,
                move |action: HotkeyAction| {
                    let msg = match action {
                        HotkeyAction::ToggleWindow => Msg::ToggleWindow,
                        HotkeyAction::Screenshot => Msg::ScreenshotHotkey,
                    };
                    p.send(msg);
                },
            ) {
                Ok(()) => {
                    let main = crate::hotkey::normalize(&self.st.settings.ui.hotkey);
                    let chord = String::new(); // 用户要求 Alt+Space 不截图，只保留 Ctrl+Alt+A
                    self.st.push_notice(
                        format!("就绪。按 {main} 显示 / 隐藏窗口，{main} 然后 {chord} 立即截图。"),
                        NoticeLevel::Info,
                    );
                }
                Err(e) => self.st.push_notice(format!("{e}"), NoticeLevel::Warning),
            }
        }

        if self.st.settings.mail.enabled {
            self.refresh_mail();
        }

        // 托盘（可选；失败不影响启动）。事件由托盘自己的线程直接投递，点一下就响应。
        if self.st.settings.ui.show_tray
            && let Some(proxy) = self.st.proxy.clone()
        {
            let tip = format!(
                "NeoSonia — {} 显示/隐藏，{} 截图",
                crate::hotkey::normalize(&self.st.settings.ui.hotkey),
                crate::hotkey::normalize(&self.st.settings.ui.screenshot_hotkey)
            );
            match crate::tray::Tray::new(proxy, &tip) {
                Ok(t) => self.tray = Some(t),
                Err(e) => tracing::warn!("托盘不可用: {e}"),
            }
        }

        // 开机自启：把系统状态同步成配置（自启模式下不打开窗口）
        if self.last_autostart != Some(self.st.settings.ui.autostart) {
            self.last_autostart = Some(self.st.settings.ui.autostart);
            if let Err(e) = crate::autostart::sync(self.st.settings.ui.autostart) {
            tracing::warn!("同步开机自启失败: {e}");
            }
        }
        if self.st.settings.ui.start_hidden {
            self.st.push_notice(
                format!(
                    "已在后台运行。按 {} 打开窗口。",
                    self.st.settings.ui.hotkey
                ),
                NoticeLevel::Info,
            );
        }

        // 验证/演示钩子：启动后自动发一条消息（NEOSONIA_SEND="..."），
        // 用于无输入环境下的 UI 验收（正常使用不会触发）。
        if let Ok(text) = std::env::var("NEOSONIA_SEND") {
            let text = text.trim().to_string();
            if !text.is_empty() {
                if let Some(b) = &self.bridge {
                    self.st.push_user(&text);
                    self.st.composer.clear();
                    self.st.status.busy = true;
                    self.st.status.phase = "思考中".into();
                    self.st.status.turn_started_ms = neosonia_core::util::now_ms();
                    b.submit(text);
                }
            }
        }

        // 验证/演示钩子：启动时预置附件（NEOSONIA_ATTACH=路径，可多次出现）
        if let Ok(path) = std::env::var("NEOSONIA_ATTACH") {
            self.handle(Msg::AttachmentsPicked(vec![std::path::PathBuf::from(path)]));
        }

        // 首次运行引导：没有密钥就直接打开设置页。
        if self.st.settings.effective_api_key().is_empty() {
            self.st.page = NavPage::Settings;
            self.st.push_notice(
                "还没有配置模型服务：请在设置页填写服务器地址、模型名和 API key。",
                NoticeLevel::Warning,
            );
        }
    }

    /// 从 Bridge 拉取局域网服务状态（地址/配对码/二维码/设备）。
    fn sync_remote_state(&mut self) {
        let Some(b) = &self.bridge else { return };
        self.st.remote.enabled = b.web_enabled();
        if let Some(info) = b.web_info() {
            self.st.remote.url = info.url;
            self.st.remote.code = info.code;
            self.st.remote.expires_at_ms = info.expires_at_ms;
            self.st.remote.qr_png = info.qr_png.clone();
            self.st.remote.devices = info.devices.clone();
            self.st.remote.busy = false;
            self.st.remote.status = if info.qr_png.is_some() {
                "用手机扫描二维码即可配对".into()
            } else {
                "二维码生成失败，可复制下面的地址手动打开".into()
            };
            if let Some(p) = &info.qr_png {
                self.st.remote.qr_pixels = load_png_pixels(p, 240);
            }
        } else {
            self.st.remote.status = "服务未启动".into();
        }
    }

    /// 「截图发模型」：抓主屏 → 作为附件放进输入框 → 显示窗口并聚焦。
    /// 邮箱 OAuth2 登录：拿授权 URL → 打开浏览器 → 等回调（bridge 线程完成后发消息）。
    fn start_mail_oauth(&mut self, provider: String) {
        let provider = provider.trim().to_ascii_lowercase();
        let name = match provider.as_str() {
            "google" | "gmail" => "Google",
            _ => "Microsoft",
        };
        self.st.mail_oauth_status = format!("正在启动 {name} 授权…");
        let Some(b) = self.bridge.as_ref() else {
            self.st.mail_oauth_status = "后台引擎未启动，无法发起授权".into();
            self.st.dirty = true;
            return;
        };
        let proxy = self.st.proxy.clone();
        let started = match proxy {
            Some(p) => b.mail_oauth_start(&provider, p),
            None => Err(anyhow::anyhow!("UI 代理不可用")),
        };
        match started {
            Ok((url, code)) => {
                // device code 流程：用户需要在浏览器里输入一段代码，必须显示出来，
                // 同时自动复制到剪贴板（用户反馈"授权时没有代码无法授权"）。
                let status = match &code {
                    Some(code) => {
                        if arboard::Clipboard::new()
                            .and_then(|mut cb| cb.set_text(code.clone()))
                            .is_ok()
                        {
                            format!(
                                "请在浏览器打开：{url}\n输入代码：{code}（已复制到剪贴板）\n完成授权后会自动回到这里"
                            )
                        } else {
                            format!("请在浏览器打开：{url}\n输入代码：{code}\n完成授权后会自动回到这里")
                        }
                    }
                    None => format!("已打开浏览器，完成 {name} 授权后会自动回到这里（地址：{url}）"),
                };
                if let Err(e) = open::that(&url) {
                    self.st.mail_oauth_status = format!("浏览器没能打开（{e}）。{status}");
                } else {
                    self.st.mail_oauth_status = status;
                }
                if let Some(code) = &code {
                    self.st.push_notice(
                        format!("授权代码: {code}（已复制）"),
                        NoticeLevel::Info,
                    );
                }
            }
            Err(e) => {
                self.st.mail_oauth_status = format!("{e}");
            }
        }
        self.st.dirty = true;
    }

    fn screenshot_to_model(&mut self) {
        let Some(b) = &self.bridge else {
            self.st.push_notice("后台引擎未启动，无法截图", NoticeLevel::Error);
            return;
        };
        // 用户要求：**先立即截图，再打开窗口并粘贴**。抓屏在后台线程，
        // 完成（ScreenshotDone）后才 show_window，避免"窗口挡住要截的内容"。
        self.st.push_notice("正在截图…", NoticeLevel::Info);
        let proxy = self.st.proxy.clone();
        let paths = self.st.paths.clone();
        let Some(proxy) = proxy else {
            self.st.push_notice("UI 代理不可用，无法截图", NoticeLevel::Error);
            return;
        };
        std::thread::Builder::new()
            .name("neosonia-shot".into())
            .spawn(move || {
                let result = crate::bridge::capture_screen_to(&paths);
                proxy.send(Msg::ScreenshotDone(
                    result
                        .map(|p| p.display().to_string())
                        .map_err(|e| format!("{e}")),
                ));
            })
            .ok();
    }

    // ---------------- 窗口 ----------------

    /// 显示 / 隐藏窗口。
    ///
    /// 隐藏用"移到屏幕外"而不是 `set_visible(false)`：Windows 上把窗口真正 Hide
    /// 之后，`RegisterHotKey` 注册的全局热键就再也收不到消息了（实测 v0.4 前的
    /// "按一次 Alt+Space 之后就不灵了"就是它）。移到屏幕外可以保持热键可用，
    /// 而且唤回时不需要重建 swapchain，速度也更快。
    fn toggle_window(&mut self) {
        if self.st.window_visible {
            self.hide_window();
        } else {
            self.show_window();
        }
    }

    fn hide_window(&mut self) {
        self.st.window_visible = false;
        // 用户要求：只在窗口弹出时显示任务栏图标
        fenestra::shell::set_skip_taskbar(true);
        // 真正隐藏（winit 层面）。shell 里已加"隐藏时跳过渲染"，不会阻塞主线程。
        fenestra::shell::set_visible(false);
    }

    fn show_window(&mut self) {
        self.st.window_visible = true;
        // 用户报"最小化后再 Alt+Space 打开会变成半个窗口/错乱"：每次显示都做一次
        // 状态自检——恢复最小化 + 重新停靠（位置/尺寸都重新应用），保证几何正确。
        fenestra::shell::restore_from_minimized();
        self.dock_once_force();
        // 打开窗口时刷新一次邮件列表（否则停用期间到达的邮件要手动点刷新）。
        if self.st.settings.mail.enabled && self.st.page == NavPage::Mail {
            self.refresh_mail();
        }
        if let Some((x, y)) = self.dock_pos {
            fenestra::shell::set_outer_position_physical(x, y);
        }
        fenestra::shell::set_skip_taskbar(false);
        fenestra::shell::set_visible(true);
        fenestra::shell::focus();
        self.apply_window_icon();
    }

    /// 任务栏/Alt-Tab 图标用项目 logo（避免有时候显示默认 exe 图标）。
    fn apply_window_icon(&self) {
        static ICON: std::sync::OnceLock<Option<(u32, u32, Vec<u8>)>> = std::sync::OnceLock::new();
        let icon = ICON.get_or_init(|| {
            static PNG: &[u8] = include_bytes!("../../../assets/icon-256.png");
            image::load_from_memory(PNG).ok().map(|img| {
                let rgba = img.to_rgba8();
                let (w, h) = rgba.dimensions();
                (w, h, rgba.into_raw())
            })
        });
        if let Some((w, h, px)) = icon {
            fenestra::shell::set_window_icon(*w, *h, px.clone());
        }
    }

    /// 重新停靠（即使 `docked` 已经是 true 也执行一次）。
    fn dock_once_force(&mut self) {
        self.docked = false;
        self.dock_once();
    }

    fn dock_once(&mut self) {
        if self.docked {
            return;
        }
        if std::env::var("NEOSONIA_NO_DOCK").ok().as_deref() == Some("1") {
            self.docked = true;
            return;
        }
        let ui = &self.st.settings.ui;
        let screen = fenestra::shell::screen_info();
        let ok = fenestra::shell::dock(
            matches!(ui.side, neosonia_core::config::DockSide::Right),
            ui.margin,
            ui.width,
            ui.height_ratio,
        );
        tracing::info!(
            ok,
            ?screen,
            width = ui.width,
            height_ratio = ui.height_ratio,
            margin = ui.margin,
            "dock attempt"
        );
        if ok {
            self.docked = true;
            self.apply_window_icon();
            fenestra::shell::set_skip_taskbar(false);
            // 记录正常位置，供"显示/隐藏"回来时用
            if let Some(info) = fenestra::shell::screen_info() {
                let scale = info.scale.max(1.0);
                let phys_w = (ui.width * scale).round();
                let phys_h = (info.height_physical * ui.height_ratio.clamp(0.2, 1.0)).round();
                let gap = (ui.margin * scale).round();
                let x = if matches!(ui.side, neosonia_core::config::DockSide::Right) {
                    info.width_physical - phys_w - gap
                } else {
                    gap
                };
                let y = ((info.height_physical - phys_h) / 2.0).floor();
                self.dock_pos = Some((x, y));
            }
            // 开机自启：窗口只创建不打开（后端照常跑，热键/托盘可用）
            if (self.boot_hidden || self.st.settings.ui.start_hidden) && !self.hidden_on_start {
                self.hidden_on_start = true;
                self.hide_window();
            }
        }
    }

    // ---------------- 设置 ----------------

    fn save_settings(&mut self) {
        let settings = match self.st.draft_to_settings() {
            Ok(s) => s,
            Err(e) => {
                self.st.push_notice(format!("设置格式有误: {e}"), NoticeLevel::Error);
                return;
            }
        };
        settings
            .save(&self.st.paths.settings_file)
            .unwrap_or_else(|e| {
                self.st.push_notice(format!("保存设置失败: {e}"), NoticeLevel::Error);
            });
        self.st.accept_saved(settings.clone());

        if let Some(b) = &self.bridge {
            b.reload_config(settings.clone());
        }
        self.apply_runtime_settings(&settings);
        self.st.push_notice("设置已保存", NoticeLevel::Success);
    }

    fn apply_runtime_settings(&mut self, settings: &Settings) {
        if self.hotkeys.toggle_spec() != crate::hotkey::normalize(&settings.ui.hotkey) {
            let mut proxy = self.st.proxy.clone();
            if let Some(p) = proxy.take() {
                let p2 = p.clone();
                if let Err(e) = self.hotkeys.install(
                    &settings.ui.hotkey,
                    &settings.ui.screenshot_hotkey,
                    &settings.ui.screenshot_chord,
                    settings.ui.chord_timeout_ms,
                    move |action: HotkeyAction| {
                        let msg = match action {
                            HotkeyAction::ToggleWindow => Msg::ToggleWindow,
                            HotkeyAction::Screenshot => Msg::ScreenshotHotkey,
                        };
                        p2.send(msg);
                    },
                ) {
                    self.st.push_notice(format!("{e}"), NoticeLevel::Warning);
                }
                self.st.proxy = Some(p);
            }
        }
        fenestra::shell::set_always_on_top(settings.ui.always_on_top);
        self.overlay.set_enabled(settings.ui.computer_activity_indicator);
        // 托盘提示里的快捷键必须和实际设置一致（用户反馈不一致）
        if let Some(tray) = &self.tray {
            let tip = format!(
                "NeoSonia — {} 显示/隐藏，{} 截图",
                crate::hotkey::normalize(&settings.ui.hotkey),
                crate::hotkey::normalize(&settings.ui.screenshot_hotkey)
            );
            tray.set_tooltip(&tip);
        }
        self.docked = false;
        self.st.theme_mode = settings.ui.theme;


        // 开机自启
        if self.last_autostart != Some(settings.ui.autostart) {
            self.last_autostart = Some(settings.ui.autostart);
            if let Err(e) = crate::autostart::sync(settings.ui.autostart) {
            self.st.push_notice(format!("开机自启设置失败: {e}"), NoticeLevel::Warning);
            }
        }
        // 主题/缩放可能变了，清缓存
        *self.theme_cache.borrow_mut() = None;
        // 局域网服务状态刷新
        self.sync_remote_state();
    }

    fn reload_settings(&mut self) {
        match Settings::load(&self.st.paths.settings_file) {
            Ok(s) => {
                self.st.accept_saved(s.clone());
                if let Some(b) = &self.bridge {
                    b.reload_config(s.clone());
                }
                self.apply_runtime_settings(&s);
                self.st.push_notice("已从磁盘重新载入设置", NoticeLevel::Info);
            }
            Err(e) => self.st.push_notice(format!("读取设置失败: {e}"), NoticeLevel::Error),
        }
    }

    /// 把 mail 的 OAuthStatus 同步到 UI 状态（受众/权限/是否需要重新授权）。
    fn refresh_mail_oauth(&mut self) {
        let Some(b) = &self.bridge else { return };
        let Some(status) = b.mail_oauth_status() else {
            return;
        };
        self.st.mail_oauth_needs_reauth = status.needs_reauth;
        self.st.mail_oauth_status_message = status.status_message.clone();
        self.st.mail_oauth_audience = status.audience.clone();
        self.st.mail_oauth_scopes = status.scopes.clone();
        self.st.mail_oauth_has_imap_scope = status.has_imap_scope;
        self.st.mail_oauth_has_smtp_scope = status.has_smtp_scope;
        if status.needs_reauth && !status.status_message.trim().is_empty() {
            self.st.mail_oauth_status = format!(
                "需要重新授权：{}（点「用 Microsoft 登录」重新授权）",
                status.status_message.trim()
            );
        }
    }

    fn test_connection(&mut self, target: TestTarget) {
        let settings = self.st.settings.clone();
        self.st.test = TestResult {
            target: Some(target),
            running: true,
            ok: false,
            message: format!("正在测试{}…", target.label()),
            summary: String::new(),
            imap_ok: false,
            imap_message: String::new(),
            smtp_ok: false,
            smtp_message: String::new(),
        };
        let (Some(bridge), Some(proxy)) = (&self.bridge, self.st.proxy.clone()) else {
            self.st.test.running = false;
            self.st.test.message = "后台引擎未启动".into();
            return;
        };

        bridge.spawn(async move {
            let result = match target {
                TestTarget::Llm => {
                    let client = neosonia_core::llm::LlmClient::new(
                        settings.server.clone(),
                        settings.effective_api_key(),
                    );
                    match client.list_models().await {
                        Ok(models) => TestResult {
                            target: Some(target),
                            running: false,
                            ok: true,
                            message: format!("连接成功，可用模型 {} 个: {}", models.len(), models.join(", ")),
            summary: String::new(),
            imap_ok: false,
            imap_message: String::new(),
            smtp_ok: false,
            smtp_message: String::new(),
                        },
                        Err(e) => TestResult {
                            target: Some(target),
                            running: false,
                            ok: false,
                            message: format!("连接失败: {e}"),
            summary: String::new(),
            imap_ok: false,
            imap_message: String::new(),
            smtp_ok: false,
            smtp_message: String::new(),
                        },
                    }
                }
                TestTarget::Imap | TestTarget::Smtp => {
                    // 统一走 mail 的 test_connection：一次拿到 IMAP + SMTP + OAuth 诊断
                    let report = neosonia_mail::test_connection(&settings).await;
                    TestResult {
                        target: Some(target),
                        running: false,
                        ok: report.imap.ok && report.smtp.ok,
                        message: report.summary.clone(),
                        summary: report.summary.clone(),
                        imap_ok: report.imap.ok,
                        imap_message: report.imap.message.clone(),
                        smtp_ok: report.smtp.ok,
                        smtp_message: report.smtp.message.clone(),
                    }
                }
            };
            proxy.send(Msg::TestFinished(result));
        });
    }

    // ---------------- 记忆 ----------------

    fn search_memory(&mut self) {
        let q = self.st.memory_query.clone();
        let Some(b) = &self.bridge else { return };
        let items = if q.trim().is_empty() {
            b.memory.recent(50).unwrap_or_default()
        } else {
            b.memory.recall(&q, 50).unwrap_or_default()
        };
        self.st.memory_items = items;
    }

    fn refresh_memory(&mut self) {
        let Some(b) = &self.bridge else { return };
        self.st.memory_items = b.memory.recent(50).unwrap_or_default();
        self.st.memory_blocks = b.memory.blocks().unwrap_or_default();
    }

    // ---------------- 邮件 ----------------

    fn refresh_mail(&mut self) {
        if self.st.mail.loading {
            return;
        }
        // 先拿 bridge/proxy 再置 loading：否则提前 return 会把 loading 永久卡 true，
        // 之后点「刷新」全被静默忽略（用户报"刷新也不更新"）。
        let (Some(bridge), Some(proxy)) = (&self.bridge, self.st.proxy.clone()) else {
            return;
        };
        // 用 bridge 的 live 配置，避免 UI 快照过旧（换账号/保存设置后 list 旧邮箱）。
        let settings = bridge.settings.read().clone();
        self.st.mail.loading = true;
        bridge.spawn(async move {
            let svc = neosonia_mail::MailService::from_settings(&settings);
            match svc.list("INBOX", 40, false).await {
                Ok(list) => proxy.send(Msg::MailLoaded(list)),
                Err(e) => proxy.send(Msg::MailStatus(format!("收取失败: {e}"))),
            }
        });
    }

    fn open_mail(&mut self, uid: u32) {
        let settings = self.st.settings.clone();
        self.st.mail.open_uid = Some(uid);
        self.st.mail.reply_text.clear();
        let (Some(bridge), Some(proxy)) = (&self.bridge, self.st.proxy.clone()) else { return };
        bridge.spawn(async move {
            let svc = neosonia_mail::MailService::from_settings(&settings);
            match svc.fetch(uid, "INBOX").await {
                Ok(full) => proxy.send(Msg::MailBody(Box::new(full))),
                Err(e) => proxy.send(Msg::MailStatus(format!("读取邮件失败: {e}"))),
            }
        });
    }

    fn send_reply(&mut self) {
        let Some(uid) = self.st.mail.open_uid else { return };
        let body = self.st.mail.reply_text.trim().to_string();
        if body.is_empty() {
            self.st.mail.status = Some("回复内容为空".into());
            return;
        }
        let settings = self.st.settings.clone();
        let (Some(bridge), Some(proxy)) = (&self.bridge, self.st.proxy.clone()) else { return };
        self.st.mail.status = Some("正在发送…".into());
        bridge.spawn(async move {
            let svc = neosonia_mail::MailService::from_settings(&settings);
            let msg = match svc.reply(uid, &body, "INBOX").await {
                Ok(()) => Msg::MailStatus("回复已发送".into()),
                Err(e) => Msg::MailStatus(format!("回复失败: {e}")),
            };
            proxy.send(msg);
        });
    }

    // ---------------- 白名单 ----------------

    fn whitelist_add(&mut self, addr: String) {
        let mut list = self.st.draft_str_list("/mail/whitelist");
        let a = addr.trim().to_ascii_lowercase();
        if a.is_empty() {
            return;
        }
        if list.iter().any(|x| x.eq_ignore_ascii_case(&a)) {
            self.st.push_notice("该地址已在白名单中", NoticeLevel::Info);
            return;
        }
        list.push(a.clone());
        self.st.apply_setting("/mail/whitelist", SettingValue::StrList(list));
        self.st.whitelist_input.clear();
        self.save_settings();
        self.st.push_notice(format!("已加入白名单: {a}"), NoticeLevel::Success);
    }

    fn whitelist_remove(&mut self, addr: String) {
        let list: Vec<String> = self
            .st
            .draft_str_list("/mail/whitelist")
            .into_iter()
            .filter(|x| !x.eq_ignore_ascii_case(addr.trim()))
            .collect();
        self.st.apply_setting("/mail/whitelist", SettingValue::StrList(list));
        // 同时清掉这条地址的备注，避免残留（UI 文案承诺"删除会一并移除备注"）
        let key = format!("/mail/whitelist_notes/{}", addr.trim());
        if self.st.draft.pointer(&key).is_some() {
            self.st.apply_setting(&key, SettingValue::Str(String::new()));
        }
        self.save_settings();
        self.st.push_notice(format!("已移出白名单: {addr}"), NoticeLevel::Info);
    }

    // ---------------- 人设 ----------------

    fn select_persona(&mut self, id: String) {
        self.st.apply_setting("/agent/persona", SettingValue::Str(id.clone()));
        self.st.active_persona = id.clone();
        if let Some(b) = &self.bridge {
            b.set_persona(&id);
        }
        self.save_settings();
    }

    // ---------------- agent 事件 ----------------

    fn on_agent_event(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::TurnStarted { turn_id: _, user_text: _ } => {
                self.st.status.busy = true;
                self.st.status.phase = "思考中".into();
                self.st.status.turn_started_ms = neosonia_core::util::now_ms();
                self.st.dirty = true;
            }
            AgentEvent::Phase(p) => {
                self.st.status.phase = p;
                self.st.dirty = true;
            }
            AgentEvent::ReasoningDelta(d) => self.st.append_reasoning(&d),
            AgentEvent::TextDelta(d) => self.st.append_assistant_text(&d),
            AgentEvent::AssistantMessage { text, reasoning } => {
                // 流式气泡已经累积；这里补一次完整性检查（非流式或重连时有用）。
                if let Some(ChatItem::Assistant { text: t, reasoning: r, streaming, .. }) =
                    self.st.items.iter_mut().rev().find(|i| matches!(i, ChatItem::Assistant { .. }))
                {
                    if t.is_empty() && !text.is_empty() {
                        *t = text;
                    }
                    if r.is_empty() && !reasoning.clone().unwrap_or_default().is_empty() {
                        *r = reasoning.unwrap_or_default();
                    }
                    *streaming = false;
                }
            }
            AgentEvent::ToolStarted { call_id, name, args, preview } => {
                // 看屏幕 / 操作键鼠类工具：在桌面上显示活动提示
                if let Some((kind, label)) = crate::overlay::hud_for_tool(&name) {
                    self.overlay.show(kind, label);
                }
                self.st.finish_assistant(None);
                self.st.items_version += 1;
                self.st.items.push(ChatItem::Tool(ToolCard {
                    call_id: call_id.clone(),
                    name,
                    args,
                    preview,
                    state: ToolState::Running,
                    output: String::new(),
                    elapsed_ms: 0,
                    expanded: false,
                    thumb: None,
                }));
                self.st.status.phase = "调用工具".into();
                self.st.dirty = true;
            }
            AgentEvent::ToolOutput { call_id, chunk } => {
                if let Some(card) = self.st.tool_card_mut(&call_id) {
                    if card.output.len() < 60_000 {
                        card.output.push_str(&chunk);
                    }
                }
                self.st.dirty = true;
            }
            AgentEvent::ToolFinished { call_id, name, ok, output, summary, elapsed_ms } => {
                // 截图类工具：把缩略图挂到卡片上（"AI 看到了什么"）
                let mut thumb: Option<(u32, u32, Vec<u8>)> = None;
                if let Some(path) = screenshot_path_in(&output) {
                    if let Some(b) = &self.bridge {
                        if let Some(px) = b.load_attachment(path).pixels {
                            thumb = Some(px);
                        }
                    }
                }
                if crate::overlay::hud_for_tool(&name).is_some() {
                    let kind = crate::overlay::hud_for_tool(&name).map(|(k, _)| k).unwrap();
                    self.overlay.show_for(kind, "NeoSonia 已完成", 900);
                }
                if let Some(card) = self.st.tool_card_mut(&call_id) {
                    card.state = if ok { ToolState::Ok } else { ToolState::Failed };
                    card.elapsed_ms = elapsed_ms;
                    if !output.is_empty() {
                        card.output = output;
                    }
                    if summary.is_some() {
                        card.preview = summary;
                    }
                    if thumb.is_some() {
                        card.thumb = thumb;
                    }
                } else {
                    self.st.items_version += 1;
                    self.st.items.push(ChatItem::Tool(ToolCard {
                        call_id,
                        name,
                        args: String::new(),
                        preview: summary,
                        state: if ok { ToolState::Ok } else { ToolState::Failed },
                        output,
                        elapsed_ms,
                        expanded: false,
                        thumb,
                    }));
                }
                self.st.dirty = true;
            }
            AgentEvent::SubagentStarted { id, label, task } => {
                self.st.status.subagents += 1;
                self.st.items_version += 1;
                self.st.items.push(ChatItem::Subagent(SubagentCard {
                    id,
                    label,
                    task,
                    state: ToolState::Running,
                    activity: Vec::new(),
                    summary: String::new(),
                    elapsed_ms: 0,
                }));
                self.st.dirty = true;
            }
            AgentEvent::SubagentActivity { id, line } => {
                if let Some(card) = self.st.subagent_mut(&id) {
                    card.activity.push(line);
                    if card.activity.len() > 20 {
                        card.activity.remove(0);
                    }
                }
                self.st.dirty = true;
            }
            AgentEvent::SubagentFinished { id, label, ok, summary, elapsed_ms } => {
                self.st.status.subagents = self.st.status.subagents.saturating_sub(1);
                if let Some(card) = self.st.subagent_mut(&id) {
                    card.state = if ok { ToolState::Ok } else { ToolState::Failed };
                    card.summary = summary;
                    card.elapsed_ms = elapsed_ms;
                } else {
                    self.st.items_version += 1;
                    self.st.items.push(ChatItem::Subagent(SubagentCard {
                        id,
                        label,
                        task: String::new(),
                        state: if ok { ToolState::Ok } else { ToolState::Failed },
                        activity: Vec::new(),
                        summary,
                        elapsed_ms,
                    }));
                }
                self.st.dirty = true;
            }
            AgentEvent::MemoryChanged { action, content } => {
                let level = NoticeLevel::Success;
                self.st.push_notice(
                    format!(
                        "记忆{action}: {}",
                        neosonia_core::util::truncate_chars(&content, 60)
                    ),
                    level,
                );
                self.refresh_memory();
            }
            AgentEvent::Usage { prompt_tokens, completion_tokens, cached_tokens: _ } => {
                self.st.status.tokens_in = prompt_tokens;
                self.st.status.tokens_out = completion_tokens;
                self.st.dirty = true;
            }
            AgentEvent::ApprovalRequested {
                call_id,
                name,
                args,
                reason,
                risk_level,
                risk_reason,
                class,
                emphasized,
            } => {
                // 审查功能关闭时 core 不会发这个事件；发来就弹窗等用户决定。
                let level = risk_level.clamp(1, 4);
                let text = if risk_reason.trim().is_empty() {
                    reason.clone()
                } else {
                    risk_reason.clone()
                };
                self.st.review_log.push(format!(
                    "风险 {} · {name} · {}",
                    level,
                    neosonia_core::util::truncate_chars(&text, 60)
                ));
                if self.st.review_log.len() > 20 {
                    self.st.review_log.remove(0);
                }
                self.st.pending_risk = Some(crate::state::RiskPrompt {
                    call_id: call_id.clone(),
                    tool: name.clone(),
                    args: args.clone(),
                    level,
                    reason: text.clone(),
                    class: class.clone(),
                    emphasized: emphasized || level >= 4,
                });
                // 桌面端弹窗；同时镜像到局域网待审列表（手机端只读提示）
                crate::bridge::publish_review_pending(&call_id, &name, &args, level, &text, &class, emphasized);
                // 弹窗出现时把窗口带到前面，避免用户没看到
                if !self.st.window_visible {
                    self.show_window();
                }
                self.st.dirty = true;
            }
            AgentEvent::Error(e) => {
                self.st.last_error = Some(e.clone());
                self.st.finish_assistant(Some(e.clone()));
                self.st.push_notice(e, NoticeLevel::Error);
                self.st.status.busy = false;
                self.st.status.phase = "出错了".into();
            }
            AgentEvent::Notice(n) => self.st.push_notice(n, NoticeLevel::Info),
            AgentEvent::ModeChanged { mode, label } => {
                self.st.agent_mode = mode.clone();
                self.st.push_notice(format!("已切换到{label}模式"), NoticeLevel::Info);
                self.st.dirty = true;
            }
            AgentEvent::RiskReviewed { tool, level, class, reason } => {
                self.st.review_log.push(format!(
                    "风险 {level} · {tool} · {}",
                    neosonia_core::util::truncate_chars(&reason, 60)
                ));
                if self.st.review_log.len() > 20 {
                    self.st.review_log.remove(0);
                }
                tracing::debug!(level, %class, "risk reviewed");
                self.st.dirty = true;
            }
            AgentEvent::WindowVisibilityRequested { hidden, reason } => {
                // LLM 用计算机工具时可以自己隐藏窗口（紫色提示框是独立窗口，不受影响）；
                // 回合结束/需要审批时 core 会发 hidden=false，这里自动把窗口恢复。
                if hidden {
                    self.st.window_hidden_by_agent = true;
                    self.hide_window();
                    tracing::info!(reason = %reason, "agent 请求隐藏窗口");
                } else {
                    self.st.window_hidden_by_agent = false;
                    self.show_window();
                    tracing::info!(reason = %reason, "agent 请求恢复窗口");
                }
                self.st.dirty = true;
            }
            AgentEvent::PlanUpdated { steps } => {
                self.st.plan = steps
                    .into_iter()
                    .map(|s| crate::state::PlanStepView {
                        title: s.title,
                        status: s.status,
                    })
                    .collect();
                self.st.dirty = true;
            }
            AgentEvent::Delivered {
                files,
                links,
                caption,
            } => {
                let items: Vec<crate::state::DeliverItem> = files
                    .into_iter()
                    .map(|f| crate::state::DeliverItem {
                        path: f.path,
                        name: f.name,
                        kind: f.kind,
                        size: f.size,
                    })
                    .collect();
                if !items.is_empty() || !links.is_empty() {
                    self.st.items.push(crate::state::ChatItem::Delivered {
                        files: items,
                        links,
                        caption,
                    });
                    self.st.items_version += 1;
                }
                self.st.dirty = true;
            }
            AgentEvent::SubagentCount { active, total } => {
                self.st.subagent_active = active;
                self.st.subagent_total = total;
                self.st.dirty = true;
            }
            AgentEvent::ProviderStatusChanged {
                provider,
                indicator,
                label,
                url,
                checked_at_ms: _,
            } => {
                self.st.provider_status =
                    Some((provider.clone(), indicator.clone(), label.clone(), url.clone()));
                if indicator != "green" && indicator != "unknown" {
                    self.st.push_notice(
                        format!("{provider} API 状态：{label}（{url}）"),
                        if indicator == "red" {
                            NoticeLevel::Error
                        } else {
                            NoticeLevel::Warning
                        },
                    );
                }
                self.st.dirty = true;
            }
            AgentEvent::ContextCompressed {
                before_tokens,
                after_tokens,
                kept,
            } => {
                self.st.push_notice(
                    format!(
                        "上下文已压缩：约 {before_tokens} → {after_tokens} tokens（保留最近 {kept} 条原文）"
                    ),
                    NoticeLevel::Info,
                );
                self.st.dirty = true;
            }
            AgentEvent::Cancelled { turn_id: _ } => {
                self.st.status.busy = false;
                self.st.status.phase = "已停止".into();
                self.st.finish_assistant(None);
                self.st.push_notice("已停止当前任务", NoticeLevel::Warning);
                self.st.dirty = true;
            }
            AgentEvent::ConversationStarted { conversation_id, title } => {
                self.st.history.current_id = Some(conversation_id);
                tracing::debug!(title = %title, "conversation started");
                if let Some(b) = &self.bridge {
                    self.st.history.sessions = b.list_conversations("");
                }
                self.st.dirty = true;
            }
            AgentEvent::ImagesAttached { count, source } => {
                self.st.push_notice(
                    format!("已把 {count} 张图片发给模型（{source}）"),
                    NoticeLevel::Info,
                );
            }
            AgentEvent::TurnFinished { turn_id: _, cancelled } => {
                self.st.finish_assistant(None);
                self.st.status.busy = false;
                self.st.status.phase = if cancelled { "已停止".into() } else { "就绪".into() };
                self.st.dirty = true;
            }
        }
    }

    // ---------------- 主消息分发 ----------------

    fn handle(&mut self, msg: Msg) {
        // NEOSONIA_DEBUG_INPUT=1：打印收到的事件类型，排查输入问题。
        if std::env::var("NEOSONIA_DEBUG_INPUT").ok().as_deref() == Some("1") {
            let name = match &msg {
                Msg::Tick => "tick",
                Msg::ComposerChanged(_) => "composer_changed",
                Msg::Submit => "submit",
                Msg::Nav(_) => "nav",
                Msg::HideWindow => "hide_window",
                Msg::ShowWindow => "show_window",
                Msg::ToggleWindow => "toggle_window",
                Msg::DragWindow => "drag_window",
                Msg::ClearChat => "clear_chat",
                Msg::ToggleTool(_) => "toggle_tool",
                Msg::ToggleReasoning(_) => "toggle_reasoning",
                Msg::CancelTurn => "cancel_turn",
                Msg::AgentEvent(_) => "agent_event",
                _ => "other",
            };
            if name == "tick" {
                self.tick_count += 1;
                if self.tick_count % 50 == 1 {
                    tracing::info!(ticks = self.tick_count, "ui tick alive");
                }
            } else {
                tracing::info!(msg = name, "ui msg");
            }
        }
        match msg {
            Msg::Tick => {

                // 屏幕活动提示的自动隐藏
                self.overlay.tick();
                if std::env::var("NEOSONIA_HOTKEY_DEBUG").map(|v| v == "1").unwrap_or(false)
                    && self.tick_count % 8 == 0
                {
                    tracing::info!(
                        ticks = self.tick_count,
                        visible = fenestra::shell::is_visible(),
                        "tick alive"
                    );
                }

                // 全局快捷键事件
                for action in self.hotkeys.poll() {
                    tracing::info!(?action, "hotkey dispatch");
                    match action {
                        HotkeyAction::ToggleWindow => {
                            // 修饰键还按着时先挂起：隐藏窗口会吞掉按键抬起，
                            // 随后 Alt+Space 就再也收不到（用户实测"第一次能隐藏，之后唤不出"）。
                            if crate::hotkey::modifier_down() {
                                self.pending_toggle = true;
                            } else {
                                self.toggle_window();
                            }
                        }
                        HotkeyAction::Screenshot => self.screenshot_to_model(),
                    }
                }
                if self.pending_toggle && !crate::hotkey::modifier_down() {
                    self.pending_toggle = false;
                    self.toggle_window();
                }
                // 模型 API 状态：每 60 秒刷新一次（输入框右侧小球）
                if self.tick_count % 240 == 0 {
                    if let Some(b) = &self.bridge {
                        b.refresh_provider_status();
                        if let Some(s) = b.provider_status() {
                            self.st.provider_status =
                                Some((s.provider, s.indicator, s.label, s.url));
                            self.st.dirty = true;
                        }
                    }
                }
                // 模型 API 状态：把异步拉到的最新结果同步到 UI（10 秒一次，不改网络）
                if self.tick_count % 40 == 0 {
                    if let Some(b) = &self.bridge {
                        match b.provider_status() {
                            Some(s) => {
                                let cur = (s.provider, s.indicator, s.label, s.url);
                                if self.st.provider_status.as_ref() != Some(&cur) {
                                    tracing::info!(provider = %cur.0, indicator = %cur.1, label = %cur.2, "provider status synced");
                                    self.st.provider_status = Some(cur);
                                    self.st.dirty = true;
                                }
                            }
                            None => {
                                tracing::info!("provider status: None（尚未刷新或未知厂商）");
                            }
                        }
                    }
                }
                // Ctrl+V 粘贴图片（文本粘贴由输入框原生处理，避免重复插入）
                // 只在前台 + 可见时接受 Ctrl+V：热键/键盘钩子是全局的，
                // 不加判断会导致在别的窗口里粘贴图片时 NeoSonia 也吃一份。
                let paste_allowed = self.st.window_visible
                    && !self.st.window_hidden_by_agent
                    && crate::win::is_own_window_foreground();
                let ctrl_v = paste_allowed
                    && crate::hotkey::ctrl_down()
                    && crate::hotkey::key_down(0x56);
                if ctrl_v && !self.st.paste_armed {
                    self.st.paste_armed = true;
                    self.paste_clipboard_media();
                } else if !ctrl_v {
                    self.st.paste_armed = false;
                }
                // 邮件 OAuth 诊断：每 10 秒同步一次（poller 可能清掉失效的 access_token）
                if self.tick_count % 40 == 0 {
                    self.refresh_mail_oauth();
                }
                // 本地索引运行态：每 5 秒刷新一次（设置页摘要显示"已索引 N 条"）
                if self.tick_count % 20 == 0
                    && self.st.settings.index.enabled
                    && let Some(b) = &self.bridge
                {
                    let (n, busy) = b.index_stats();
                    self.st.index_count = n;
                    self.st.index_busy = busy;
                }
                self.dock_once();
                // 不允许"点任务栏图标缩小进任务栏"：检测到最小化就立即恢复并 re-dock
                if self.st.window_visible && fenestra::shell::is_minimized() {
                    fenestra::shell::restore_from_minimized();
                    self.dock_once_force();
                }
                if let Some((_, _, at)) = &self.st.toast {
                    if neosonia_core::util::now_ms() - at > 3200 {
                        self.st.toast = None;
                        self.st.dirty = true;
                    }
                }
            }
            Msg::ShowWindow => self.show_window(),
            Msg::Noop => {}
            Msg::IndexStats(n, busy) => {
                self.st.index_count = n;
                self.st.index_busy = busy;
                self.st.dirty = true;
            }
            Msg::DeviceRenameStart(id) => {
                if let Ok(raw) = std::fs::read_to_string(&self.st.paths.devices_file) {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&raw) {
                        let list = json
                            .get("devices")
                            .and_then(|v| v.as_array())
                            .cloned()
                            .or_else(|| json.as_array().cloned())
                            .unwrap_or_default();
                        if let Some(name) = list
                            .iter()
                            .find(|d| d.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
                            .and_then(|d| d.get("name"))
                            .and_then(|v| v.as_str())
                        {
                            self.st.device_rename_input = name.to_string();
                        }
                    }
                }
                self.st.device_rename_id = id;
                self.st.dirty = true;
            }
            Msg::DeviceRenameChanged(s) => {
                self.st.device_rename_input = s;
                self.st.dirty = true;
            }
            Msg::DeviceRenameCancel => {
                self.st.device_rename_id.clear();
                self.st.device_rename_input.clear();
                self.st.dirty = true;
            }
            Msg::DeviceRename { id, name } => {
                let name = name.trim().to_string();
                if name.is_empty() || name.chars().count() > 32 {
                    self.st.push_notice("设备名需 1-32 个字符", NoticeLevel::Warning);
                    return;
                }
                let ok = self
                    .bridge
                    .as_ref()
                    .and_then(|b| b.rename_device(&id, &name))
                    .is_some();
                if ok {
                    self.st.push_notice(
                        format!("设备已重命名为：{name}"),
                        NoticeLevel::Success,
                    );
                    self.sync_remote_state();
                } else {
                    self.st.push_notice("重命名失败：设备不存在或配置文件不可写", NoticeLevel::Error);
                }
                self.st.device_rename_id.clear();
                self.st.device_rename_input.clear();
                self.st.dirty = true;
            }            Msg::MailLogout => {
                if let Some(b) = &self.bridge {
                    let note = b.mail_logout();
                    self.st.push_notice(note, NoticeLevel::Info);
                    self.refresh_mail_oauth();
                    self.st.dirty = true;
                }
            }
            Msg::PasteClipboard => self.paste_clipboard(),
            Msg::AnimTick => {
                // 只要求重绘；进度由 view 按时间戳计算，播完自然静止。
                self.st.dirty = true;
            }
            Msg::AgentWindowVisibility(hidden) => {
                if hidden {
                    self.st.window_hidden_by_agent = true;
                    self.hide_window();
                } else {
                    self.st.window_hidden_by_agent = false;
                    self.show_window();
                }
                self.st.dirty = true;
            }
            Msg::ApplyPendingPage => {
                if let Some(page) = self.st.page_pending.take() {
                    self.st.page = page;
                    self.st.page_anim_start_ms = neosonia_core::util::now_ms();
                    // 180ms 过渡动画：短期高频补帧（约 12 帧），播完自动停止。
                    if let Some(proxy) = self.st.proxy.clone() {
                        std::thread::spawn(move || {
                            for _ in 0..12 {
                                std::thread::sleep(std::time::Duration::from_millis(16));
                                proxy.send(Msg::AnimTick);
                            }
                        });
                    }
                    self.nav_started_at.set(Some(std::time::Instant::now()));
                    self.st.dirty = true;
                }
            }            Msg::CopyText(text) => {
                match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text.clone())) {
                    Ok(()) => self.st.push_notice("已复制到剪贴板", NoticeLevel::Success),
                    Err(e) => self.st.push_notice(format!("复制失败: {e}"), NoticeLevel::Error),
                }
                self.st.dirty = true;
            }
            Msg::PersonaEditStart(id) => {
                let edit = match id {
                    Some(id) => self
                        .st
                        .personas
                        .iter()
                        .find(|p| p.id == id)
                        .map(|p| crate::state::PersonaEditState {
                            is_new: false,
                            id: p.id.clone(),
                            name: p.name.clone(),
                            description: p.description.clone(),
                            body: p.system_prompt.clone(),
                            error: String::new(),
                        })
                        .unwrap_or_default(),
                    None => crate::state::PersonaEditState {
                        is_new: true,
                        name: "新人设".into(),
                        description: String::new(),
                        body: "你是一个新的助手人设。请在这里描述它的性格、语气与工作方式。".into(),
                        ..Default::default()
                    },
                };
                self.st.persona_edit = Some(edit);
                self.st.dirty = true;
            }
            Msg::PersonaEditField { field, value } => {
                if let Some(edit) = self.st.persona_edit.as_mut() {
                    match field.as_str() {
                        "id" => edit.id = value,
                        "name" => edit.name = value,
                        "description" => edit.description = value,
                        "body" => edit.body = value,
                        _ => {}
                    }
                    edit.error.clear();
                }
                self.st.dirty = true;
            }
            Msg::PersonaCancelEdit => {
                self.st.persona_edit = None;
                self.st.dirty = true;
            }
            Msg::PersonaSaveEdit => {
                let Some(edit) = self.st.persona_edit.clone() else {
                    return;
                };
                let Some(b) = &self.bridge else {
                    if let Some(e) = self.st.persona_edit.as_mut() {
                        e.error = "后台未启动，无法保存人设".into();
                    }
                    return;
                };
                let result = if edit.is_new {
                    b.persona_create(&edit.id, &edit.name, &edit.description, &edit.body)
                } else {
                    b.persona_save(&edit.id, &edit.name, &edit.description, &edit.body)
                };
                match result {
                    Ok(_) => {
                        self.st.personas = b.personas.list();
                        self.st.persona_edit = None;
                        self.st.push_notice("人设已保存", NoticeLevel::Success);
                    }
                    Err(e) => {
                        if let Some(ed) = self.st.persona_edit.as_mut() {
                            ed.error = format!("{e}");
                        }
                    }
                }
                self.st.dirty = true;
            }
            Msg::PersonaDeleteRequest(id) => {
                if let Some(b) = &self.bridge {
                    match b.persona_delete(&id) {
                        Ok(_) => {
                            self.st.personas = b.personas.list();
                            self.st.persona_edit = None;
                            self.st.push_notice("人设已删除", NoticeLevel::Info);
                        }
                        Err(e) => self.st.push_notice(format!("{e}"), NoticeLevel::Error),
                    }
                }
                self.st.dirty = true;
            }
            Msg::QuitApp => {
                if let Some(b) = &self.bridge {
                    b.engine.shutdown();
                }
                self.overlay.hide();
                tracing::info!("托盘退出");
                std::process::exit(0);
            }
            Msg::HideWindow => self.hide_window(),
            Msg::ToggleWindow => self.toggle_window(),
            Msg::DragWindow => {
                let now = neosonia_core::util::now_ms();
                if now - self.last_drag_ms > 600 {
                    fenestra::shell::start_dragging();
                }
                self.last_drag_ms = now;
            }
            Msg::Quit => {
                if let Some(b) = &self.bridge {
                    b.engine.shutdown();
                }
                // 关闭窗口即退出事件循环
                self.hide_window();
            }
            Msg::SetTheme(mode) => {
                self.st.apply_setting("/ui/theme", SettingValue::Str(theme_str(mode).into()));
                self.st.theme_mode = mode;
                self.save_settings();
            }
            Msg::Nav(page) => {
                if page != self.st.page || self.st.page_pending.is_some() {
                    // 两段式：先渲染当前页 + 加载提示（首帧很便宜），
                    // 下一帧再构建目标页；首次构建较重的页面不会看起来卡死。
                    self.st.page_pending = Some(page);
                    self.st.dirty = true;
                    if let Some(proxy) = &self.st.proxy {
                        let proxy = proxy.clone();
                        std::thread::spawn(move || {
                            std::thread::sleep(std::time::Duration::from_millis(30));
                            proxy.send(Msg::ApplyPendingPage);
                        });
                    }
                }
            }
            Msg::ComposerChanged(s) => {
                // TextArea 没有"回车提交"选项：回车会插入 \n。
                // 这里把不带 Shift 的回车转成发送（Shift+Enter 保留换行）。
                if s.contains('\n') && !crate::hotkey::shift_down() {
                    self.st.composer = s.trim_end_matches(['\n', '\r']).to_string();
                    self.st.dirty = true;
                    self.handle(Msg::Submit);
                    return;
                }
                self.st.composer = s;
                self.st.dirty = true;
            }
            Msg::Submit => {
                let text = self.st.composer.trim().to_string();
                if text.is_empty() && self.st.attachments.is_empty() {
                    return;
                }
                let Some(b) = &self.bridge else {
                    self.st.push_notice("后台引擎未启动，先检查设置", NoticeLevel::Error);
                    return;
                };
                // 组装：图片 -> ImagePart（视觉输入）；文本 -> 正文；目录/其它 -> 路径清单
                let (prompt, images, thumbs) = build_prompt(&self.st, b);
                self.st.push_user_with_thumbs(&text, thumbs);
                self.st.composer.clear();
                self.st.attachments.clear();
                self.st.status.busy = true;
                self.st.status.phase = "思考中".into();
                self.st.status.turn_started_ms = neosonia_core::util::now_ms();
                if images.is_empty() {
                    b.submit(prompt);
                } else {
                    b.submit_with_images(prompt, images);
                }
                self.st.dirty = true;
            }
            Msg::CancelTurn => {
                if let Some(b) = &self.bridge {
                    b.cancel();
                }
                self.st.status.phase = "正在停止".into();
                self.st.dirty = true;
            }
            Msg::ClearChat => {
                if let Some(b) = &self.bridge {
                    b.reset();
                }
                // 新会话不能残留上一段的聊天记录/工具卡/附件（用户报的 bug），
                // 并且默认切回日常模式。
                self.st.items.clear();
                self.st.attachments.clear();
                self.st.history.open = false;
                self.st.page = NavPage::Chat;
                self.st.agent_mode = "daily".to_string();
                self.st.page_pending = None;
                self.st.items_version += 1;
                if let Some(b) = &self.bridge {
                    b.set_mode(neosonia_core::config::AgentMode::Daily);
                }
                self.st
                    .push_notice("新对话已开始（已切回日常模式）", NoticeLevel::Info);
                self.refresh_memory();
                self.st.dirty = true;
            }
            Msg::ToggleReasoning(idx) => {
                if let Some(ChatItem::Assistant { reasoning_open, .. }) = self.st.items.get_mut(idx) {
                    *reasoning_open = !*reasoning_open;
                    // 聊天列表是"按 items_version 缓存的快照"（chat.rs 用 Rc 复用），
                    // 只改状态不递增版本会导致快照不重建 → 桌面前端点"思考过程/详情"
                    // 完全没反应（远程 web 走另一套渲染所以正常）。
                    self.st.items_version += 1;
                }
                self.st.dirty = true;
            }
            Msg::ToggleTool(call_id) => {
                if let Some(card) = self.st.tool_card_mut(&call_id) {
                    card.expanded = !card.expanded;
                    self.st.items_version += 1;
                }
                self.st.dirty = true;
            }
            Msg::AgentEvent(ev) => {
                // 事件几乎总会改 items（流式文本/工具卡片/提示），让聊天快照失效一次。
                self.st.items_version += 1;
                self.on_agent_event((*ev).clone());
            }
            Msg::Approve { call_id, ok } => {
                if let Some(b) = &self.bridge {
                    b.approve(&call_id, ok);
                }
            }
            Msg::SettingEdit(sp) => {
                let theme_path = sp.path == "/ui/theme";
                let value = sp.value.clone();
                self.st.apply_setting(&sp.path, sp.value);
                if theme_path {
                    if let SettingValue::Str(s) = &value {
                        self.st.theme_mode = parse_theme(s);
                    }
                }
                self.st.dirty = true;
            }
            Msg::SettingsSave => self.save_settings(),
            Msg::SettingsReload => self.reload_settings(),
            Msg::TestConnection(t) => self.test_connection(t),
            Msg::TestFinished(r) => {
                if r.ok {
                    self.st.push_notice(format!("{}: {}", r.target.map(|t| t.label()).unwrap_or("测试"), r.message), NoticeLevel::Success);
                } else {
                    self.st.push_notice(format!("{}: {}", r.target.map(|t| t.label()).unwrap_or("测试"), r.message), NoticeLevel::Error);
                }
                self.st.test = r;
            }
            Msg::WhitelistAdd(a) => self.whitelist_add(a),
            Msg::WhitelistRemove(a) => self.whitelist_remove(a),
            Msg::WhitelistInputChanged(s) => {
                self.st.whitelist_input = s;
                self.st.dirty = true;
            }
            Msg::HotkeyApply => {
                let spec = crate::hotkey::normalize(&self.st.draft_str("/ui/hotkey"));
                match crate::hotkey::parse_spec(&spec) {
                    Ok(_) => {
                        self.st.apply_setting("/ui/hotkey", SettingValue::Str(spec.clone()));
                        self.st.push_notice(format!("快捷键已更新: {spec}"), NoticeLevel::Success);
                        self.save_settings();
                    }
                    Err(e) => self.st.push_notice(format!("{e}"), NoticeLevel::Error),
                }
            }
            Msg::PersonaSelect(id) => {
                self.select_persona(id);
                let name = self
                    .st
                    .personas
                    .iter()
                    .find(|p| p.id == self.st.active_persona)
                    .map(|p| p.name.clone())
                    .unwrap_or_default();
                self.st.push_notice(format!("已切换人设: {name}"), NoticeLevel::Info);
            }
            Msg::PersonaReload => {
                if let Some(b) = &self.bridge {
                    b.personas.reload();
                    self.st.personas = b.personas.list();
                }
                self.st.push_notice("人设已重新扫描", NoticeLevel::Info);
            }
            Msg::PersonaSaved(s) => self.st.push_notice(s, NoticeLevel::Success),
            Msg::MemoryQueryChanged(s) => {
                self.st.memory_query = s;
                self.st.dirty = true;
            }
            Msg::MemorySearch => self.search_memory(),
            Msg::MemoryLoaded(items) => {
                self.st.memory_items = items;
                self.st.dirty = true;
            }
            Msg::MemoryDelete(id) => {
                if let Some(b) = &self.bridge {
                    if b.memory.forget(id).unwrap_or(false) {
                        self.st.push_notice("已删除该条记忆", NoticeLevel::Info);
                    }
                }
                self.search_memory();
            }
            Msg::MemoryBlockChanged { key, value } => {
                self.st.memory_blocks.insert(key, value);
                self.st.dirty = true;
            }
            Msg::MemoryBlockSave { key, value } => {
                if let Some(b) = &self.bridge {
                    match b.memory.set_block(&key, &value) {
                        Ok(()) => self.st.push_notice(format!("记忆块 {key} 已保存"), NoticeLevel::Success),
                        Err(e) => self.st.push_notice(format!("保存失败: {e}"), NoticeLevel::Error),
                    }
                }
            }
            Msg::MailRefresh => self.refresh_mail(),
            Msg::MailLoaded(list) => {
                self.st.mail.unread = list.iter().filter(|m| m.unread).count() as u32;
                self.st.mail.summaries = list;
                self.st.mail.loaded = true;
                self.st.mail.loading = false;
                self.st.dirty = true;
            }
            Msg::MailOpen(uid) => self.open_mail(uid),
            Msg::MailBody(full) => {
                self.st.mail.body_expanded = false;
                self.st.mail.open = Some(*full);
                self.st.dirty = true;
            }
            Msg::MailClose => {
                self.st.mail.open_uid = None;
                self.st.mail.open = None;
                self.st.mail.reply_text.clear();
                self.st.mail.body_expanded = false;
                self.st.dirty = true;
            }
            Msg::MailToggleBody => {
                self.st.mail.body_expanded = !self.st.mail.body_expanded;
                self.st.dirty = true;
            }
            Msg::MailReplyChanged(s) => {
                self.st.mail.reply_text = s;
                self.st.dirty = true;
            }
            Msg::MailSendReply => self.send_reply(),
            Msg::MailStatus(s) => {
                self.st.mail.status = Some(s.clone());
                self.st.mail.loading = false;
                self.st.push_notice(s, NoticeLevel::Info);
                self.st.dirty = true;
            }
            Msg::Copy(s) => match arboard::Clipboard::new() {
                Ok(mut cb) => match cb.set_text(s) {
                    Ok(()) => self.st.push_notice("已复制", NoticeLevel::Success),
                    Err(e) => self.st.push_notice(format!("复制失败: {e}"), NoticeLevel::Error),
                },
                Err(e) => self.st.push_notice(format!("剪贴板不可用: {e}"), NoticeLevel::Error),
            },
            Msg::OpenPath(p) => {
                match open::that(&p) {
                    Ok(()) => {}
                    Err(e) => self
                        .st
                        .push_notice(format!("打开失败: {e}"), NoticeLevel::Error),
                }
                self.st.dirty = true;
            }
            Msg::RevealPath(p) => {
                #[cfg(windows)]
                {
                    let _ = std::process::Command::new("explorer")
                        .arg(format!("/select,{p}"))
                        .spawn();
                }
                #[cfg(not(windows))]
                {
                    if let Some(dir) = std::path::Path::new(&p).parent() {
                        let _ = open::that(dir);
                    }
                }
                self.st.dirty = true;
            }            Msg::OpenLink(url) => {
                if let Err(e) = open::that(&url) {
                    self.st.push_notice(format!("打开链接失败: {e}"), NoticeLevel::Error);
                }
            }
            Msg::Toast { text, level } => {
                self.st.toast = Some((text, level, neosonia_core::util::now_ms()));
                self.st.dirty = true;
            }
            Msg::PersistError(e) => self.st.push_notice(e, NoticeLevel::Error),

            // ---------------- 附件 ----------------
            Msg::AttachClick => {
                if let Some(files) = rfd::FileDialog::new()
                    .set_title("选择要发给模型的文件")
                    .add_filter("图片", &["png", "jpg", "jpeg", "gif", "webp", "bmp"])
                    .add_filter("文本/代码", &["txt", "md", "rs", "py", "js", "ts", "json", "toml", "csv", "log", "html", "css"])
                    .add_filter("所有文件", &["*"])
                    .pick_files()
                {
                    self.handle(Msg::AttachmentsPicked(files));
                }
            }
            Msg::AttachFolderClick => {
                if let Some(dir) = rfd::FileDialog::new().set_title("选择要发给模型的文件夹").pick_folder() {
                    self.handle(Msg::AttachmentsPicked(vec![dir]));
                }
            }
            Msg::AttachmentsPicked(paths) => {
                let Some(b) = &self.bridge else { return };
                for p in paths {
                    if self.st.attachments.len() >= 8 {
                        self.st.push_notice("最多同时附加 8 个文件", NoticeLevel::Warning);
                        break;
                    }
                    if self.st.attachments.iter().any(|a| a.path == p) {
                        continue;
                    }
                    let att = b.load_attachment(p);
                    self.st.attachments.push(att);
                }
                self.st.dirty = true;
            }
            Msg::AttachmentRemove(i) => {
                if i < self.st.attachments.len() {
                    self.st.attachments.remove(i);
                }
                self.st.dirty = true;
            }
            Msg::AttachmentClear => {
                self.st.attachments.clear();
                self.st.dirty = true;
            }

            // ---------------- 截图 ----------------
            Msg::ScreenshotHotkey => self.screenshot_to_model(),
            Msg::ScreenshotDone(Ok(p)) => {
                self.handle(Msg::AttachmentsPicked(vec![std::path::PathBuf::from(p)]));
                // 截图快捷键的语义：打开窗口、切到对话页、把图附在输入框里
                self.st.page = NavPage::Chat;
                self.st.history.open = false;
                self.show_window();
                self.st.push_notice(
                    "截图已附到输入框，补充一句话后回车发送（会作为图片发给模型）。",
                    NoticeLevel::Success,
                );
                self.st.dirty = true;
            }
            Msg::ScreenshotDone(Err(e)) => {
                self.st.push_notice(format!("截图失败: {e}"), NoticeLevel::Error);
            }

            // ---------------- 对话历史 ----------------
            Msg::HistoryToggleExpanded => {
                self.st.history.expanded = !self.st.history.expanded;
                self.st.dirty = true;
            }
            Msg::HistoryToggle => {
                // 用户要求：不在对话页点历史按钮也要生效——统一先切回对话页，再切换历史面板。
                if self.st.page != NavPage::Chat {
                    self.st.page = NavPage::Chat;
                    self.st.page_pending = None;
                    self.st.history.open = true;
                    self.load_history("");
                } else if self.st.history.open {
                    self.st.history.open = false;
                } else {
                    self.st.history.open = true;
                    self.load_history("");
                }
                self.st.dirty = true;
            }
            Msg::HistoryQueryChanged(q) => {
                self.st.history.query = q;
                self.st.dirty = true;
            }
            Msg::HistorySearch => {
                let q = self.st.history.query.clone();
                self.load_history(&q);
            }
            Msg::HistoryLoaded(sessions) => {
                self.st.history.sessions = sessions;
                self.st.history.loading = false;
                self.st.dirty = true;
            }
            Msg::HistoryHits(hits) => {
                self.st.history.hits = hits;
                self.st.history.loading = false;
                self.st.dirty = true;
            }
            Msg::HistorySelect(id) => {
                // 用户要求：点历史会话按钮时若不在对话页，要切回对话页
                self.st.page = NavPage::Chat;
                if let Some(b) = &self.bridge {
                    b.load_conversation(id.clone());
                    let msgs = b.conversations.messages(&id, 500).unwrap_or_default();
                    // 用户报"重启后历史会话点进去没有内容"：之前只取了消息数却没塞进 items。
                    self.st.items = messages_to_items(&msgs);
                    self.st.items_version += 1;
                    self.st.attachments.clear();
                    self.st.history.open = false;
                    self.st.history.active_id = Some(id.clone());
                    self.st.history.current_id = Some(id);
                    self.st.page = NavPage::Chat;
                    let n = self.st.items.len();
                    self.st.push_notice(format!("已载入历史会话（{n} 条消息）"), NoticeLevel::Info);
                    self.st.dirty = true;
                }
            }
            Msg::HistoryDelete(id) => {
                if let Some(b) = &self.bridge {
                    if b.delete_conversation(&id) {
                        self.st.push_notice("会话已删除", NoticeLevel::Info);
                    }
                }
                let q = self.st.history.query.clone();
                self.load_history(&q);
            }
            Msg::HistoryNew => {
                if let Some(b) = &self.bridge {
                    b.new_conversation();
                }
                self.st.items_version += 1;
                self.st.attachments.clear();
                self.st.history.open = false;
                self.st.history.active_id = None;
                self.st.push_notice("新对话已开始", NoticeLevel::Info);
                self.st.dirty = true;
            }

            // ---------------- 局域网设备 ----------------
            Msg::RemoteToggle => {
                let enabled = !self.st.settings.remote.enabled;
                self.st.apply_setting("/remote/enabled", SettingValue::Bool(enabled));
                self.save_settings();
                self.sync_remote_state();
            }
            Msg::RemoteRefresh => {
                self.save_settings();
                if let Some(b) = &self.bridge {
                    if b.web_enabled() {
                        let _ = b.refresh_pairing();
                    }
                    self.st.remote.devices = b.web_devices();
                }
                self.sync_remote_state();
                self.st.dirty = true;
            }
            Msg::RemotePairingReady { url, code, expires_at_ms, qr_png } => {
                self.st.remote.url = url;
                self.st.remote.code = code;
                self.st.remote.expires_at_ms = expires_at_ms;
                if let Some(p) = qr_png {
                    let path = std::path::PathBuf::from(p);
                    self.st.remote.qr_pixels = load_png_pixels(&path, 240);
                    self.st.remote.qr_png = Some(path);
                }
                self.st.dirty = true;
            }
            Msg::RemoteDevices(devices) => {
                self.st.remote.devices = devices;
                self.st.dirty = true;
            }
            Msg::RemoteRevoke(id) => {
                if let Some(b) = &self.bridge {
                    if b.revoke_device(&id) {
                        self.st.push_notice("已移除该设备的信任", NoticeLevel::Info);
                    }
                    self.st.remote.devices = b.web_devices();
                }
                self.st.dirty = true;
            }
            Msg::RemoteStatus(s) => {
                self.st.remote.status = s;
                self.st.remote.busy = false;
                self.st.dirty = true;
            }

            // ---------------- 技能 ----------------
            Msg::SkillsLoaded(list) => {
                self.st.skills = list;
                self.st.dirty = true;
            }
            Msg::SkillInstallRequest => {
                if let Some(file) = rfd::FileDialog::new()
                    .set_title("选择技能文件（SKILL.md 或 .md）")
                    .add_filter("Markdown", &["md", "markdown"])
                    .pick_file()
                {
                    self.handle(Msg::SkillInstallPath(file.display().to_string()));
                }
            }
            Msg::SkillInstallPath(p) => {
                if let Some(b) = &self.bridge {
                    b.install_skill_path(std::path::PathBuf::from(p));
                }
            }
            Msg::SkillInstallUrl(u) => {
                let url = u.trim().to_string();
                if url.is_empty() {
                    self.st.push_notice("请先填写技能的下载地址", NoticeLevel::Warning);
                    return;
                }
                if let Some(b) = &self.bridge {
                    b.install_skill_url(url);
                }
                self.st.skill_install_input.clear();
            }
            Msg::SkillUninstall(id) => {
                if let Some(b) = &self.bridge {
                    if b.uninstall_skill(&id) {
                        self.st.push_notice(format!("已卸载技能 {id}"), NoticeLevel::Info);
                    }
                    self.st.skills = b.skill_list();
                }
            }
            Msg::SkillOpenDir => {
                let dir = self.st.paths.skills.clone();
                let _ = std::fs::create_dir_all(&dir);
                if let Some(b) = &self.bridge {
                    b.open_path(&dir);
                }
            }
            Msg::SkillInstallInputChanged(s) => {
                self.st.skill_install_input = s;
                self.st.dirty = true;
            }

            // ---------------- 本地索引 ----------------
            Msg::IndexRootsChanged(s) => {
                self.st.index_roots_input = s;
                self.st.dirty = true;
            }
            Msg::IndexRebuild => {
                // 先把多行输入同步进设置，再保存并重建
                let roots: Vec<String> = self
                    .st
                    .index_roots_input
                    .lines()
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty())
                    .collect();
                if !roots.is_empty() {
                    self.st.apply_setting("/index/roots", SettingValue::StrList(roots));
                }
                self.save_settings();
                if let Some(b) = &self.bridge {
                    b.rebuild_index();
                }
                self.st.push_notice("开始重建本地文件索引（后台进行）", NoticeLevel::Info);
            }
            Msg::ToggleSection(id) => {
                if let Some(pos) = self.st.settings_open.iter().position(|x| x == &id) {
                    self.st.settings_open.remove(pos);
                } else {
                    self.st.settings_open.push(id);
                }
                self.st.dirty = true;
            }
            Msg::SetAgentMode(mode) => {
                let mode = mode.trim().to_ascii_lowercase();
                let (id, label) = match mode.as_str() {
                    "coding" => ("coding", "编程"),
                    "computer_use" | "computeruse" => ("computer_use", "计算机使用"),
                    _ => ("daily", "日常"),
                };
                self.st.apply_setting(
                    "/agent/mode",
                    SettingValue::Str(id.to_string()),
                );
                self.st.agent_mode = id.to_string();
                if let Some(b) = &self.bridge {
                    let m = match id {
                        "coding" => neosonia_core::config::AgentMode::Coding,
                        "computer_use" => neosonia_core::config::AgentMode::ComputerUse,
                        _ => neosonia_core::config::AgentMode::Daily,
                    };
                    b.set_mode(m);
                }
                self.save_settings();
                self.st.push_notice(format!("已切换到{label}模式"), NoticeLevel::Info);
                self.st.dirty = true;
            }
            Msg::MailOAuthStart(provider) => {
                self.start_mail_oauth(provider);
            }
            Msg::MailOAuthDone(result) => {
                match result {
                    Ok(email) => {
                        self.st.mail_oauth_status = format!("已登录: {email}");
                        self.st.push_notice(
                            format!("邮箱 OAuth2 登录成功: {email}"),
                            NoticeLevel::Success,
                        );
                        if let Some(b) = &self.bridge {
                            self.st.settings = b.settings.read().clone();
                            self.st.draft =
                                serde_json::to_value(&self.st.settings).unwrap_or_default();
                            self.st.saved_draft = self.st.draft.clone();
                        }
                    }
                    Err(e) => {
                        self.st.mail_oauth_status = format!("登录失败: {e}");
                        self.st.push_notice(
                            format!("邮箱 OAuth2 失败: {e}"),
                            NoticeLevel::Error,
                        );
                    }
                }
                self.st.dirty = true;
            }
            Msg::RiskDecision { call_id, decision } => {
                if let Some(b) = &self.bridge {
                    b.approve_decision(&call_id, decision);
                }
                if let Some(p) = self.st.pending_risk.take() {
                    let text = match decision {
                        crate::state::RiskChoice::Approve => format!("已允许: {}（{}）", p.tool, p.reason),
                        crate::state::RiskChoice::Deny => format!("已拒绝: {}", p.tool),
                        crate::state::RiskChoice::DenyClass => {
                            format!("已拒绝 {}，1 天内自动拒绝同类操作", p.tool)
                        }
                    };
                    self.st.review_log.push(text);
                    if self.st.review_log.len() > 20 {
                        self.st.review_log.remove(0);
                    }
                }
                self.st.dirty = true;
            }
            Msg::McpReload => {
                self.save_settings();
                if let Some(b) = &self.bridge {
                    self.st.mcp.servers = b.mcp_servers();
                }
                self.st.push_notice(
                    "MCP 配置已保存；新增/修改服务器需要重启应用生效",
                    NoticeLevel::Info,
                );
                self.st.dirty = true;
            }
            Msg::McpRestart(name) => {
                let mut ok = false;
                if let Some(b) = &self.bridge {
                    match b.mcp_restart(&name) {
                        Ok(()) => {
                            ok = true;
                            self.st.mcp.servers = b.mcp_servers();
                        }
                        Err(e) => self.st.push_notice(format!("重启 MCP 失败: {e}"), NoticeLevel::Error),
                    }
                }
                if ok {
                    self.st.push_notice(format!("MCP 服务器 {name} 已重启"), NoticeLevel::Info);
                }
                self.st.dirty = true;
            }
            Msg::OpenPage(page) => {
                self.nav_started_at.set(Some(std::time::Instant::now()));
                self.st.page = page;
                if page == NavPage::Chat {
                    self.st.history.open = false;
                }
                if page == NavPage::Remote {
                    self.sync_remote_state();
                }
                self.st.dirty = true;
            }
        }
    }

    fn load_history(&mut self, query: &str) {
        let Some(b) = &self.bridge else { return };
        self.st.history.loading = true;
        let sessions = b.list_conversations(query);
        let hits = b.search_conversations(query);
        self.st.history.sessions = sessions;
        self.st.history.hits = hits;
        self.st.history.loading = false;
        self.st.dirty = true;
    }
}

fn parse_theme(s: &str) -> ThemeMode {
    match s.trim().to_ascii_lowercase().as_str() {
        "light" => ThemeMode::Light,
        "system" => ThemeMode::System,
        _ => ThemeMode::Dark,
    }
}

fn theme_str(mode: ThemeMode) -> &'static str {
    match mode {
        ThemeMode::Dark => "dark",
        ThemeMode::Light => "light",
        ThemeMode::System => "system",
    }
}

impl App for NeoSoniaApp {
    type Msg = Msg;

    fn init(&mut self, proxy: Proxy<Msg>) {
        self.boot(proxy);
    }

    /// 点任务栏关闭/标题栏 X 不退出进程：隐藏到后台（托盘 + Alt+Space 可唤回）。
    /// 真正退出走托盘菜单「退出」或 Msg::Quit。
    fn on_close_requested(&mut self) -> bool {
        self.hide_window();
        false
    }

    fn update(&mut self, msg: Msg) {
        // 用户反馈"执行操作时有时崩溃"：把单条消息处理包起来，
        // 某个面板/工具事件里的 panic 不应该让整个应用退出。
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.handle(msg)));
        if result.is_err() {
            tracing::error!("处理 UI 消息时 panic（已捕获，应用继续运行）");
            self.st.push_notice(
                "界面处理时发生内部错误（已捕获），如果界面异常请重启应用",
                NoticeLevel::Error,
            );
            self.st.dirty = true;
        }
    }

    fn theme(&self) -> Theme {
        self.app_theme()
    }

    fn subscriptions(&self) -> Vec<Sub<Msg>> {
        vec![Sub::every(
            "neosonia-tick",
            Duration::from_millis(250),
            || Msg::Tick,
        )]
    }

    fn view(&self) -> Element<Msg> {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.build_view())) {
            Ok(el) => el,
            Err(_) => {
                tracing::error!("构建视图时 panic（已捕获），显示降级界面");
                let c = self.colors();
                let mut items: Vec<Element<Msg>> = Vec::new();
                items.push(
                    text("界面渲染出现内部错误")
                        .size(TextSize::Lg)
                        .color(c.danger)
                        .into(),
                );
                items.push(
                    text("可以按 Alt+Space 隐藏再打开，或重启应用。崩溃日志在 logs/crash-*.txt。")
                        .size(TextSize::Sm)
                        .color(c.text_muted)
                        .into(),
                );
                col()
                    .w_full()
                    .h_full()
                    .p(16.0)
                    .gap(8.0)
                    .bg(c.bg)
                    .children(items)
                    .into()
            }
        }
    }
}

impl NeoSoniaApp {
    /// 真正的视图构建（包一层 catch_unwind，渲染 panic 不再让应用退出）。
    fn build_view(&self) -> Element<Msg> {
        let t0 = std::time::Instant::now();
        let c = self.colors();
        let root = ui::root(&self.st, &c);
        let ms = t0.elapsed().as_secs_f32() * 1000.0;
        let verbose = std::env::var("NEOSONIA_PROFILE")
            .map(|v| v == "1")
            .unwrap_or(false);
        if verbose || ms > 25.0 {
            tracing::info!(
                page = ?self.st.page,
                ms = format!("{ms:.1}"),
                items = self.st.items.len(),
                attachments = self.st.attachments.len(),
                "view built"
            );
        }
        if let Some(at) = self.nav_started_at.take() {
            let dt = at.elapsed().as_secs_f32() * 1000.0;
            tracing::info!(page = ?self.st.page, ms = format!("{dt:.1}"), "page switch");
        }
        self.frames.set(self.frames.get() + 1);
        let since = self.fps_since.get();
        if since.elapsed() >= std::time::Duration::from_secs(5) {
            let n = self.frames.replace(0);
            let fps = n as f32 / since.elapsed().as_secs_f32();
            if verbose {
                tracing::info!(fps = format!("{fps:.1}"), "frame rate");
            }
            self.fps_since.set(std::time::Instant::now());
        }
        root
    }
}
// ============================================================
// 小工具
// ============================================================

/// 把 PNG 文件解码成 (宽, 高, RGBA)，并按 `max_edge` 缩放（二维码/缩略图用）。
/// 供 chat 交付物预览用的公开包装。
pub fn load_png_pixels_public(
    path: &std::path::Path,
    max_edge: u32,
) -> Option<(u32, u32, Vec<u8>)> {
    load_png_pixels(path, max_edge)
}

fn load_png_pixels(path: &std::path::Path, max_edge: u32) -> Option<(u32, u32, Vec<u8>)> {
    let img = image::open(path).ok()?;
    let img = if img.width() > max_edge || img.height() > max_edge {
        img.thumbnail(max_edge, max_edge)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Some((w, h, rgba.into_raw()))
}

// ============================================================
// 附件 -> 模型输入
// ============================================================

/// 把输入框里的附件组装成 (prompt, 图片, UI 缩略图)：
/// - 图片：缩放后 base64 → ImagePart（模型直接"看到"）
/// - 文本：内容贴进 prompt（截断）
/// - 目录：列出顶层条目
/// - 其它文件：给绝对路径（模型可以用工具读）
fn build_prompt(
    st: &AppState,
    b: &Bridge,
) -> (
    String,
    Vec<neosonia_core::model::ImagePart>,
    Vec<(u32, u32, Vec<u8>)>,
) {
    use base64::Engine as _;
    use neosonia_core::model::ImagePart;

    let mut images: Vec<ImagePart> = Vec::new();
    let mut thumbs: Vec<(u32, u32, Vec<u8>)> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let max_edge = st.settings.agent.vision_max_edge.max(256);

    for att in &st.attachments {
        match att.kind {
            neosonia_core::AttachmentKind::Image => {
                if let Some((w, h, px)) = &att.pixels {
                    thumbs.push((*w, *h, px.clone()));
                }
                match image::open(&att.path) {
                    Ok(img) => {
                        let img = if img.width() > max_edge || img.height() > max_edge {
                            img.thumbnail(max_edge, max_edge)
                        } else {
                            img
                        };
                        let rgba = img.to_rgba8();
                        let (w, h) = rgba.dimensions();
                        let mut cursor = std::io::Cursor::new(Vec::new());
                        if img
                            .write_to(&mut cursor, image::ImageFormat::Png)
                            .is_ok()
                        {
                            let b64 = base64::engine::general_purpose::STANDARD
                                .encode(cursor.into_inner());
                            images.push(ImagePart::png(
                                b64,
                                w,
                                h,
                                Some(att.path.display().to_string()),
                            ));
                            notes.push(format!("[图片附件] {}", att.name));
                        }
                    }
                    Err(e) => notes.push(format!("[图片附件读取失败] {}: {e}", att.name)),
                }
            }
            neosonia_core::AttachmentKind::Text => {
                let body = att.text_preview.clone().unwrap_or_default();
                notes.push(format!(
                    "[文本附件 {} | {}]\n{}",
                    att.name,
                    att.human_size(),
                    body
                ));
            }
            neosonia_core::AttachmentKind::Folder => {
                let mut entries: Vec<String> = Vec::new();
                if let Ok(rd) = std::fs::read_dir(&att.path) {
                    for e in rd.flatten().take(80) {
                        let name = e.file_name().to_string_lossy().to_string();
                        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                        entries.push(if is_dir { format!("{name}/") } else { name });
                    }
                }
                notes.push(format!(
                    "[文件夹附件] {}\n{}",
                    att.path.display(),
                    entries.join("\n")
                ));
            }
            _ => {
                notes.push(format!(
                    "[文件附件] {} | {}",
                    att.path.display(),
                    att.human_size()
                ));
            }
        }
    }

    let mut prompt = st.composer.trim().to_string();
    if !notes.is_empty() {
        if !prompt.is_empty() {
            prompt.push_str("\n\n");
        }
        prompt.push_str(&notes.join("\n\n"));
    }
    if prompt.trim().is_empty() {
        prompt = "看看这些附件，告诉我有什么。".to_string();
    }
    let _ = b;
    (prompt, images, thumbs)
}

/// 从工具输出里找 `[screenshot:<path>]` 标记。
fn screenshot_path_in(text: &str) -> Option<std::path::PathBuf> {
    let start = text.find("[screenshot:")?;
    let rest = &text[start + "[screenshot:".len()..];
    let end = rest.find(']')?;
    let raw = rest[..end].trim().trim_matches('"');
    if raw.is_empty() {
        None
    } else {
        Some(std::path::PathBuf::from(raw))
    }
}

// ============================================================
// 会话消息 -> UI 条目
// ============================================================

/// 把会话库里的 ChatMessage 转成聊天页的条目（含图片缩略图、工具卡片）。
fn messages_to_items(msgs: &[neosonia_core::ChatMessage]) -> Vec<crate::state::ChatItem> {
    use base64::Engine as _;
    use neosonia_core::model::Role;

    let mut out: Vec<crate::state::ChatItem> = Vec::new();
    for m in msgs {
        match m.role {
            Role::User => {
                let mut thumbs: Vec<(u32, u32, Vec<u8>)> = Vec::new();
                for img in m.images.iter().take(4) {
                    if let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(&img.data_base64)
                    {
                        if let Ok(im) = image::load_from_memory(&raw) {
                            let th = im.thumbnail(256, 256).to_rgba8();
                            let (w, h) = th.dimensions();
                            thumbs.push((w, h, th.into_raw()));
                        }
                    }
                }
                // 系统注入的"这是截图的画面"消息不展示成用户气泡
                let text = m.text();
                if text.starts_with("（这是刚才截图的画面") {
                    continue;
                }
                out.push(crate::state::ChatItem::User { text, thumbs });
            }
            Role::Assistant => {
                let text = m.text();
                if text.trim().is_empty() && m.tool_calls.is_empty() {
                    continue;
                }
                if !text.trim().is_empty() {
                    out.push(crate::state::ChatItem::Assistant {
                        text,
                        reasoning: m.reasoning_content.clone().unwrap_or_default(),
                        reasoning_open: false,
                        streaming: false,
                        error: None,
                    });
                }
                for tc in &m.tool_calls {
                    let card = crate::state::ToolCard {
                        call_id: tc.id.clone(),
                        name: tc.function.name.clone(),
                        args: tc.function.arguments.clone(),
                        preview: None,
                        state: crate::state::ToolState::Ok,
                        output: String::new(),
                        elapsed_ms: 0,
                        expanded: false,
                        thumb: None,
                    };
                    let idx = out.len();
                    out.push(crate::state::ChatItem::Tool(card));
                    // 补上结果（tool 消息在后续元素里）
                    let _ = idx;
                }
            }
            Role::Tool => {
                let text = m.text();
                let ok = m.tool_ok.unwrap_or(true);
                // 找到对应的工具卡片，把输出补进去
                let mut filled = false;
                for item in out.iter_mut().rev() {
                    if let crate::state::ChatItem::Tool(card) = item {
                        if Some(&card.call_id) == m.tool_call_id.as_ref() || card.output.is_empty() {
                            card.output = neosonia_core::util::truncate_chars(&text, 4000);
                            card.state = if ok {
                                crate::state::ToolState::Ok
                            } else {
                                crate::state::ToolState::Failed
                            };
                            filled = true;
                            break;
                        }
                    }
                }
                if !filled && !text.trim().is_empty() {
                    out.push(crate::state::ChatItem::Notice {
                        text: neosonia_core::util::truncate_chars(&text, 400),
                        level: crate::state::NoticeLevel::Info,
                    });
                }
            }
            Role::System => {}
        }
    }
    out
}

impl NeoSoniaApp {
    /// Ctrl+V：只把剪贴板里的**图片**变成附件（文本交给输入框原生粘贴，避免重复）。
    fn paste_clipboard_media(&mut self) {
        let mut cb = match arboard::Clipboard::new() {
            Ok(c) => c,
            Err(_) => return,
        };
        let img = match cb.get_image() {
            Ok(img) => img,
            Err(_) => {
                // 管理员模式下从资源管理器拖放会被 Windows UIPI 拒绝，但
                // "Ctrl+C 复制文件 → 本窗口 Ctrl+V" 走剪贴板 CF_HDROP，不受限制。
                let files = clipboard_files();
                if !files.is_empty() {
                    let n = files.len();
                    self.handle(Msg::AttachmentsPicked(files));
                    self.st
                        .push_notice(format!("已从剪贴板粘贴 {n} 个文件"), NoticeLevel::Success);
                }
                return;
            }
        };
        let Some(img) = image::RgbaImage::from_raw(img.width as u32, img.height as u32, img.bytes.into_owned()) else { return };
        let dir = self.st.paths.uploads.clone();
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("paste-{}.png", neosonia_core::util::now_ms()));
        match img.save(&path) {
            Ok(()) => {
                self.handle(Msg::AttachmentsPicked(vec![path]));
                self.st.push_notice("已从剪贴板粘贴图片", NoticeLevel::Success);
            }
            Err(e) => self.st.push_notice(format!("保存粘贴图片失败: {e}"), NoticeLevel::Error),
        }
        self.st.dirty = true;
    }

    /// 粘贴剪贴板：图片存为附件；文本填进输入框。
    fn paste_clipboard(&mut self) {
        let mut cb = match arboard::Clipboard::new() {
            Ok(c) => c,
            Err(e) => {
                self.st.push_notice(format!("读取剪贴板失败: {e}"), NoticeLevel::Error);
                return;
            }
        };
        if let Ok(img) = cb.get_image() {
            let img = image::RgbaImage::from_raw(img.width as u32, img.height as u32, img.bytes.into_owned());
            if let Some(img) = img {
                let dir = self.st.paths.uploads.clone();
                let _ = std::fs::create_dir_all(&dir);
                let path = dir.join(format!("paste-{}.png", neosonia_core::util::now_ms()));
                match img.save(&path) {
                    Ok(()) => {
                        self.handle(Msg::AttachmentsPicked(vec![path]));
                        self.st.push_notice("已从剪贴板粘贴图片", NoticeLevel::Success);
                    }
                    Err(e) => self.st.push_notice(format!("保存粘贴图片失败: {e}"), NoticeLevel::Error),
                }
                self.st.dirty = true;
                return;
            }
        }
        match cb.get_text() {
            Ok(t) => {
                self.st.composer.push_str(&t);
                self.st.dirty = true;
            }
            Err(_) => self.st.push_notice("剪贴板里没有可粘贴的图片或文本", NoticeLevel::Info),
        }
    }
}

/// 读取剪贴板里的文件列表（Windows `CF_HDROP`）。
///
/// 管理员模式下从资源管理器拖放会被 UIPI 拒绝，但"复制文件 → 粘贴"可用，
/// 所以 Ctrl+V 在剪贴板不是图片时回退到文件列表。
#[cfg(windows)]
fn clipboard_files() -> Vec<std::path::PathBuf> {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn IsClipboardFormatAvailable(format: u32) -> i32;
        fn OpenClipboard(owner: isize) -> i32;
        fn GetClipboardData(format: u32) -> isize;
        fn CloseClipboard() -> i32;
    }
    #[link(name = "shell32")]
    unsafe extern "system" {
        fn DragQueryFileW(hdrop: isize, index: u32, buf: *mut u16, cch: u32) -> u32;
    }
    const CF_HDROP: u32 = 15;
    let mut out = Vec::new();
    unsafe {
        if IsClipboardFormatAvailable(CF_HDROP) == 0 || OpenClipboard(0) == 0 {
            return out;
        }
        let hdrop = GetClipboardData(CF_HDROP);
        if hdrop != 0 {
            let count = DragQueryFileW(hdrop, 0xFFFF_FFFF, std::ptr::null_mut(), 0);
            for i in 0..count {
                let len = DragQueryFileW(hdrop, i, std::ptr::null_mut(), 0);
                if len == 0 {
                    continue;
                }
                let mut buf = vec![0u16; len as usize + 1];
                let copied = DragQueryFileW(hdrop, i, buf.as_mut_ptr(), len + 1);
                if copied == 0 {
                    continue;
                }
                buf.truncate(copied as usize);
                if let Ok(s) = String::from_utf16(&buf) {
                    out.push(std::path::PathBuf::from(s));
                }
            }
        }
        CloseClipboard();
    }
    out
}

#[cfg(not(windows))]
fn clipboard_files() -> Vec<std::path::PathBuf> {
    Vec::new()
}