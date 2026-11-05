//! NEOSONIA patch: Win32 `WM_DROPFILES` 文件拖放兜底。
//!
//! winit 0.30 的 OLE 拖放在这台机器的透明/置顶无边框窗口上实测收不到
//! `WindowEvent::DroppedFile`，所以这里直接用 Win32 的 `DragAcceptFiles` +
//! 子类化 wndproc 兜底（与 OLE 不冲突，谁先到都行）。
//!
//! 线程模型：wndproc 在 UI 线程被调用，不能在这里直接拿 `AppRunner` 的
//! `&mut`，所以只把路径推进全局队列，并 `InvalidateRect` 制造一次
//! `WM_PAINT` 唤醒 winit 事件循环；runner 在窗口事件入口调用 [`drain`]
//! 取出路径，再走与 `WindowEvent::DroppedFile` 相同的 `input_main` 路径。
//!
//! `NEOSONIA_DROP_LOG=1` 时用 `tracing::info!` 打印收到的事件/路径。

use std::path::PathBuf;
use std::sync::Mutex;

const WM_DROPFILES: u32 = 0x0233;
const WM_NCDESTROY: u32 = 0x0082;
const GWLP_WNDPROC: i32 = -4;

static QUEUE: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
/// 每个被安装窗口的原 wndproc（`SetWindowLongPtrW` 返回值）。
static ORIGINAL_PROCS: Mutex<Vec<(isize, isize)>> = Mutex::new(Vec::new());
/// 事件循环唤醒器（由 runner 注入：`EventLoopProxy::send_event(DropsPending)`）。
static WAKER: Mutex<Option<Box<dyn Fn() + Send>>> = Mutex::new(None);

/// runner 启动时注入唤醒函数（只在 wndproc 线程调用）。
pub(crate) fn set_waker(waker: Box<dyn Fn() + Send>) {
    if let Ok(mut slot) = WAKER.lock() {
        *slot = Some(waker);
    }
}

fn wake() {
    if let Ok(slot) = WAKER.lock()
        && let Some(waker) = slot.as_ref()
    {
        waker();
    }
}

#[link(name = "shell32")]
unsafe extern "system" {
    fn DragAcceptFiles(hwnd: isize, accept: i32);
    fn DragQueryFileW(hdrop: isize, file_index: u32, file_name: *mut u16, cch: u32) -> u32;
    fn DragFinish(hdrop: isize);
}

#[link(name = "ole32")]
unsafe extern "system" {
    fn RevokeDragDrop(hwnd: isize) -> i32;
}

#[link(name = "user32")]
unsafe extern "system" {
    fn SetWindowLongPtrW(hwnd: isize, index: i32, value: isize) -> isize;
    fn ChangeWindowMessageFilterEx(hwnd: isize, msg: u32, action: u32, p: *mut core::ffi::c_void) -> i32;
    fn CallWindowProcW(prev: isize, hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> isize;
    fn DefWindowProcW(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> isize;
    fn InvalidateRect(hwnd: isize, rect: *const core::ffi::c_void, erase: i32) -> i32;
}

pub(crate) fn drop_log_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("NEOSONIA_DROP_LOG")
            .map(|v| v == "1")
            .unwrap_or(false)
    })
}

/// 从 `HDROP` 里取出所有文件路径（wndproc 与单测共用）。
///
/// # Safety
/// `hdrop` 必须是系统/测试构造的合法 `HDROP`。
unsafe fn query_hdrop_paths(hdrop: isize) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    // 0xFFFF_FFFF = 查询文件数量
    let count = unsafe { DragQueryFileW(hdrop, 0xFFFF_FFFF, std::ptr::null_mut(), 0) };
    if drop_log_enabled() {
        tracing::warn!(count, "WM_DROPFILES: 收到拖放文件数量");
    }
    for index in 0..count {
        let len = unsafe { DragQueryFileW(hdrop, index, std::ptr::null_mut(), 0) };
        if len == 0 {
            continue;
        }
        let mut buf = vec![0u16; len as usize + 1];
        let copied = unsafe { DragQueryFileW(hdrop, index, buf.as_mut_ptr(), len + 1) };
        if copied == 0 {
            continue;
        }
        buf.truncate(copied as usize);
        if let Ok(path) = String::from_utf16(&buf) {
            if drop_log_enabled() {
                tracing::warn!(path = %path, "WM_DROPFILES: 拖放路径");
            }
            paths.push(PathBuf::from(path));
        }
    }
    paths
}

unsafe extern "system" fn wndproc(
    hwnd: isize,
    msg: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    if msg == WM_DROPFILES {
        let hdrop = wparam as isize;
        let paths = unsafe { query_hdrop_paths(hdrop) };
        unsafe { DragFinish(hdrop) };
        if !paths.is_empty() {
            if let Ok(mut queue) = QUEUE.lock() {
                queue.extend(paths);
            }
        }
        // 唤醒 winit 事件循环（proxy 事件优先；InvalidateRect 作为兜底）。
        wake();
        unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
        return 0;
    }
    if msg == WM_NCDESTROY {
        if let Ok(mut procs) = ORIGINAL_PROCS.lock() {
            procs.retain(|(h, _)| *h != hwnd);
        }
    }
    let prev = ORIGINAL_PROCS
        .lock()
        .ok()
        .and_then(|procs| procs.iter().find(|(h, _)| *h == hwnd).map(|(_, prev)| *prev));
    match prev {
        Some(prev) if prev != 0 => unsafe { CallWindowProcW(prev, hwnd, msg, wparam, lparam) },
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// 给一个原生窗口安装 `WM_DROPFILES` 接收与子类化 wndproc。
pub(crate) fn install(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    unsafe {
        // winit 的 OLE drop target 在这台机器的透明/置顶无边框窗口上会拦截真实拖拽
        // （鼠标显示"禁止"且收不到 DroppedFile）；撤销它，改用 WM_DROPFILES 路径。
        let revoke = RevokeDragDrop(hwnd);
        if drop_log_enabled() {
            tracing::warn!(hwnd, hr = revoke, "已撤销 winit OLE 拖放目标（RevokeDragDrop）");
        }
        // 管理员模式（提高完整性级别）下 UIPI 会静默丢弃来自普通权限资源管理器的消息，
        // 包括 WM_DROPFILES（真实拖放"禁止"光标/无事件的常见原因）。放行这三条消息。
        const MSGFLT_ALLOW: u32 = 1;
        for msg in [0x0233u32 /*WM_DROPFILES*/, 0x004Au32 /*WM_COPYDATA*/, 0x0049u32] {
            let hr = unsafe { ChangeWindowMessageFilterEx(hwnd, msg, MSGFLT_ALLOW, std::ptr::null_mut()) };
            if drop_log_enabled() {
                tracing::warn!(hwnd, msg, hr, "ChangeWindowMessageFilterEx(ALLOW)");
            }
        }
        DragAcceptFiles(hwnd, 1);
        let proc: unsafe extern "system" fn(isize, u32, usize, isize) -> isize = wndproc;
        let prev = SetWindowLongPtrW(hwnd, GWLP_WNDPROC, proc as usize as isize);
        if prev == 0 {
            tracing::warn!(hwnd, "SetWindowLongPtrW(GWLP_WNDPROC) 失败，WM_DROPFILES 兜底未安装");
            return;
        }
        if let Ok(mut procs) = ORIGINAL_PROCS.lock() {
            procs.push((hwnd, prev));
        }
        if drop_log_enabled() {
            tracing::warn!(hwnd, "已安装 WM_DROPFILES 兜底（DragAcceptFiles + 子类化 wndproc）");
        }
    }
}

/// 取出所有已收到的拖放路径（取走即清空）。
pub(crate) fn drain() -> Vec<PathBuf> {
    QUEUE
        .lock()
        .map(|mut queue| std::mem::take(&mut *queue))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    // 测试用：构造一个合法 HDROP（DROPFILES 结构 + 双空结尾的 UTF-16 路径）。
    const GMEM_MOVEABLE: u32 = 0x0002;
    const GMEM_ZEROINIT: u32 = 0x0040;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GlobalAlloc(flags: u32, bytes: usize) -> isize;
        fn GlobalLock(handle: isize) -> *mut core::ffi::c_void;
        fn GlobalUnlock(handle: isize) -> i32;
        fn GlobalFree(handle: isize) -> isize;
    }

    fn make_hdrop(paths: &[&str]) -> isize {
        // DROPFILES { pFiles: u32, pt: POINT(x,y), fNC: i32, fWide: i32 } = 20 bytes
        const HEADER: usize = 20;
        let mut wide: Vec<u16> = Vec::new();
        for path in paths {
            wide.extend(path.encode_utf16());
            wide.push(0);
        }
        wide.push(0);
        let bytes = HEADER + wide.len() * 2;
        unsafe {
            let handle = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes);
            assert!(handle != 0, "GlobalAlloc 失败");
            let base = GlobalLock(handle) as *mut u8;
            assert!(!base.is_null(), "GlobalLock 失败");
            // pFiles = 20
            std::ptr::write_unaligned(base as *mut u32, HEADER as u32);
            // fWide = TRUE（偏移 16）
            std::ptr::write_unaligned(base.add(16) as *mut i32, 1);
            let text = base.add(HEADER) as *mut u16;
            std::ptr::copy_nonoverlapping(wide.as_ptr(), text, wide.len());
            GlobalUnlock(handle);
            handle
        }
    }

    #[test]
    fn wm_dropfiles_parses_paths_and_drains_queue() {
        // 两个文件：解析顺序、内容、drain 后清空
        let hdrop = make_hdrop(&[r"C:\temp\a.png", r"C:\temp\b.lnk"]);
        unsafe {
            wndproc(0, WM_DROPFILES, hdrop as usize, 0);
        }
        let paths = drain();
        assert_eq!(paths.len(), 2, "应解析出两个路径：{paths:?}");
        assert_eq!(paths[0], PathBuf::from(r"C:\temp\a.png"));
        assert_eq!(paths[1], PathBuf::from(r"C:\temp\b.lnk"));
        assert!(drain().is_empty(), "drain 后队列应清空");
        unsafe {
            GlobalFree(hdrop);
        }

        // 空列表：不应产生路径
        let empty = make_hdrop(&[]);
        unsafe {
            wndproc(0, WM_DROPFILES, empty as usize, 0);
        }
        assert!(drain().is_empty(), "空 HDROP 不应产生路径");
        unsafe {
            GlobalFree(empty);
        }
    }
}
