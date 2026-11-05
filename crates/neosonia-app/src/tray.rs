//! 系统托盘：显示/隐藏窗口、打开设备页、退出。
//!
//! 用 `tray-icon`（Windows 走 Shell_NotifyIcon，Linux 走 StatusNotifier）。
//! **托盘菜单事件由一个后台线程轮询**（每 40ms），拿到后直接通过 `Proxy<Msg>`
//! 投递给 UI —— 这样点托盘是"立即响应"，不用等主循环的 Tick（v0.3 之前
//! 用户反馈"点托盘后前端卡好长时间"就是事件要等 Tick 才处理）。
//!
//! 图标用项目 logo（`assets/icon-256.png`，编译期嵌入），不是运行时文件。

use std::sync::Arc;
use std::thread::JoinHandle;

use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

use crate::state::{Msg, NavPage};

/// 托盘句柄。必须一直持有，Drop 时图标消失（并停止轮询线程）。
pub struct Tray {
    icon: TrayIcon,
    poller: Option<JoinHandle<()>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl Tray {
    /// 创建托盘图标并启动事件轮询线程。失败返回 Err，调用方降级为"没有托盘"。
    pub fn new(proxy: fenestra::prelude::Proxy<Msg>, tooltip: &str) -> anyhow::Result<Self> {
        let menu = Menu::new();
        let show_item = MenuItem::new("显示 / 隐藏窗口", true, None);
        let devices_item = MenuItem::new("连接其他设备…", true, None);
        let quit_item = MenuItem::new("退出 NeoSonia", true, None);
        let sep = PredefinedMenuItem::separator();
        let sep2 = PredefinedMenuItem::separator();
        menu.append_items(&[&show_item, &devices_item, &sep, &sep2, &quit_item])
            .map_err(|e| anyhow::anyhow!("托盘菜单创建失败: {e}"))?;

        let (rgba, w, h) = tray_icon_pixels();
        let icon = tray_icon::Icon::from_rgba(rgba, w, h)
            .map_err(|e| anyhow::anyhow!("托盘图标解码失败: {e}"))?;

        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(tooltip)
            .with_icon(icon)
            .build()
            .map_err(|e| anyhow::anyhow!("托盘图标创建失败: {e}"))?;

        let ids = Ids {
            show: show_item.id().clone(),
            devices: devices_item.id().clone(),
            quit: quit_item.id().clone(),
        };
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = stop.clone();
        let poller = std::thread::Builder::new()
            .name("neosonia-tray".into())
            .spawn(move || {
                while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                    poll_once(&ids, &proxy);
                    std::thread::sleep(std::time::Duration::from_millis(40));
                }
            })
            .ok();

        Ok(Self {
            icon,
            poller,
            stop,
        })
    }
}

impl Tray {
    /// 更新托盘提示（快捷键改了要跟着改，用户反馈"描述和实际设置不一致"）。
    pub fn set_tooltip(&self, tip: &str) {
        let _ = self.icon.set_tooltip(Some(tip));
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.stop
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = self.poller.take() {
            let _ = h.join();
        }
    }
}

struct Ids {
    show: MenuId,
    devices: MenuId,
    quit: MenuId,
}

/// 轮询一次托盘事件并投递给 UI（后台线程调用）。
fn poll_once(ids: &Ids, proxy: &fenestra::prelude::Proxy<Msg>) {
    let rx = MenuEvent::receiver();
    while let Ok(ev) = rx.try_recv() {
        let msg = if ev.id == ids.show {
            Msg::ToggleWindow
        } else if ev.id == ids.devices {
            Msg::OpenPage(NavPage::Remote)
        } else if ev.id == ids.quit {
            Msg::QuitApp
        } else {
            continue;
        };
        proxy.send(msg);
    }
}

/// logo -> RGBA（64x64；DPI 不同由系统缩放）。
fn tray_icon_pixels() -> (Vec<u8>, u32, u32) {
    static PNG: &[u8] = include_bytes!("../../../assets/tray-32.png");
    match image::load_from_memory(PNG) {
        Ok(img) => {
            let rgba = img.to_rgba8();
            let (w, h) = rgba.dimensions();
            (rgba.into_raw(), w, h)
        }
        Err(_) => {
            // 兜底：紫色方块，保证托盘至少有个图标
            let w = 32u32;
            let mut buf = Vec::with_capacity((w * w * 4) as usize);
            for _ in 0..(w * w) {
                buf.extend_from_slice(&[0xb8, 0x9c, 0xff, 0xff]);
            }
            (buf, w, w)
        }
    }
}
