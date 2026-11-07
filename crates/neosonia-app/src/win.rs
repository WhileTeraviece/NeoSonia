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
/// 当前前台窗口是否属于 NeoSonia 进程。
///
/// 用于"只在前台且聚焦时接受粘贴"：之前用全局键盘轮询 Ctrl+V，
/// 在别的程序里粘贴图片/文件时 NeoSonia 也会收到一份。
#[cfg(windows)]
pub fn is_own_window_foreground() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId,
    };
    // SAFETY: 标准 Win32 只读查询，不持有/释放任何句柄。
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return false;
        }
        let mut pid = 0u32;
        let _ = GetWindowThreadProcessId(hwnd, Some(&mut pid as *mut u32));
        pid == std::process::id()
    }
}

#[cfg(not(windows))]
pub fn is_own_window_foreground() -> bool {
    true
}