//! 面板模块（设置 / 记忆 / 邮件 / 人设 / 历史 / 设备）。
//!
//! 每个面板都只提供 `pub fn view(st: &AppState, c: &UiColors) -> Element<Msg>`。

pub mod history;
pub mod mail;
pub mod memory;
pub mod persona;
pub mod remote;
pub mod settings;
