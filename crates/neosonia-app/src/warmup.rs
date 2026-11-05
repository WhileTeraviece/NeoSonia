//! 字形/文本预热。
//!
//! parley 第一次遇到新的中文字形时会做字体回退查找与 shaping，首次打开
//! 文字很多的页面（设置/人设）会卡 1-2 秒。这里在启动几秒后用后台线程把
//! 界面用到的字符先排版一遍（**只做 shaping/layout，不上 GPU**），把
//! font/parley 缓存热起来，页面切换就不再首帧卡顿。

use fenestra::prelude::*;

/// 界面里用到的全部 CJK 字符（由 `scripts/gen-warmup-glyphs.ps1` 生成）。
static GLYPHS: &str = include_str!("warmup-glyphs.txt");

/// 启动后调用一次：延迟 4 秒（避开启动首帧），后台预热字形。
pub fn spawn(theme: Theme) {
    std::thread::Builder::new()
        .name("neosonia-warmup".into())
        .spawn(move || {
            // 等启动阶段过去（首帧、dock、索引扫描都在抢 CPU）
            std::thread::sleep(std::time::Duration::from_secs(4));
            let started = std::time::Instant::now();
            let mut items: Vec<Element<()>> = Vec::with_capacity(32);
            for line in GLYPHS.lines().filter(|l| !l.trim().is_empty()) {
                items.push(
                    text(line.to_string())
                        .size(TextSize::Sm)
                        .color(Color::BLACK)
                        .into(),
                );
            }
            let el: Element<()> = col().w_full().p(8.0).gap(2.0).children(items).into();
            // 只做 layout/shaping：与真实窗口共用同一个 Fonts 缓存（`with_fonts`
            // 内部是 Mutex，主线程此时通常空闲，最多等一帧）。
            let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                fenestra::shell::with_fonts(|fonts| {
                    let mut state = fenestra::FrameState::new();
                    state.reduced_motion = true;
                    let _ = fenestra::build_frame(
                        &el,
                        &theme,
                        fonts,
                        &mut state,
                        (900.0, 1400.0),
                        1.0,
                    );
                });
            }))
            .is_ok();
            tracing::info!(
                ok,
                ms = started.elapsed().as_millis() as u64,
                "字形预热完成"
            );
        })
        .ok();
}
