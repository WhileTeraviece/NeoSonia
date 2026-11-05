//! 平台相关的极小工具（启动失败提示等）。

#[cfg(windows)]
pub fn message_box_error(title: &str, text: &str) {
    use windows::core::HSTRING;
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    unsafe {
        MessageBoxW(None, &HSTRING::from(text), &HSTRING::from(title), MB_OK | MB_ICONERROR);
    }
}

#[cfg(not(windows))]
pub fn message_box_error(title: &str, text: &str) {
    eprintln!("{title}: {text}");
}
