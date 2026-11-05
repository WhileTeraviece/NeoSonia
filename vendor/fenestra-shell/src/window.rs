//! The windowed runners: winit event loop + wgpu surface + vello renderer.
//!
//! [`run_scene`] paints via a raw scene callback (no input). [`run_static`]
//! runs an element view with scrolling and animation frames; the full `App`
//! runner with messages arrives in M4 and builds on the same plumbing.

use std::sync::Arc;
#[cfg(not(target_arch = "wasm32"))]
use std::time::{Duration, Instant};
#[cfg(target_arch = "wasm32")]
use web_time::Instant;

#[cfg(not(target_arch = "wasm32"))]
use fenestra_core::Theme;
use fenestra_core::{
    App, Element, Fonts, FrameState, InputEvent, Key, KeyInput, build_frame, dispatch,
    refresh_hover,
};
use kurbo::Point;
use vello::peniko::Color;
use vello::util::{RenderContext, RenderSurface};
use vello::wgpu::{self, CurrentSurfaceTexture};
use vello::{AaConfig, AaSupport, RenderParams, Renderer, RendererOptions, Scene};
// ---------------------------------------------------------------- NEOSONIA perf
/// NEOSONIA patch：帧耗时日志（`NEOSONIA_FRAME_LOG=1` 时打印到 stderr），
/// 用来定位"切页面/点击按钮卡一下"到底卡在 build / layout / paint / present 哪一段。
fn frame_log_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("NEOSONIA_FRAME_LOG").map(|v| v == "1").unwrap_or(false))
}

/// NEOSONIA patch：抗锯齿模式可配（`NEOSONIA_AA=area|msaa8|msaa16`），
/// 集显上 msaa8 通常比 area 更快。
fn aa_config() -> AaConfig {
    static MODE: std::sync::OnceLock<AaConfig> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| {
        match std::env::var("NEOSONIA_AA").unwrap_or_default().to_ascii_lowercase().as_str() {
            "msaa8" => AaConfig::Msaa8,
            "msaa16" => AaConfig::Msaa16,
            _ => AaConfig::Area,
        }
    })
}
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, Position};
use winit::event::{MouseScrollDelta, StartCause, WindowEvent};
#[cfg(not(target_arch = "wasm32"))]
use winit::event_loop::EventLoopProxy;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

use crate::ShellError;

/// One wheel "line" in logical pixels.
pub(crate) const LINE_SCROLL_PX: f64 = 40.0;

/// Extracts `(dx, dy)` logical-pixel deltas from a winit wheel event, honoring
/// the window scale for pixel deltas.
pub(crate) fn wheel_deltas(delta: MouseScrollDelta, scale: f64) -> (f64, f64) {
    match delta {
        MouseScrollDelta::LineDelta(x, y) => {
            (f64::from(x) * LINE_SCROLL_PX, f64::from(y) * LINE_SCROLL_PX)
        }
        MouseScrollDelta::PixelDelta(pos) => (pos.x / scale, pos.y / scale),
    }
}

/// A raw paint callback: `(scene, logical_w, logical_h, background)`.
#[cfg(not(target_arch = "wasm32"))]
type PaintFn = Box<dyn FnMut(&mut Scene, f64, f64, Color)>;
/// A message-free element view function.
#[cfg(not(target_arch = "wasm32"))]
type ViewFn = Box<dyn Fn(&Theme) -> Element<()>>;

/// Options for the application window.
#[derive(Debug, Clone)]
pub struct WindowOptions {
    /// Window title.
    pub title: String,
    /// Initial inner size in logical pixels.
    pub inner_size: (f64, f64),
    /// Minimum inner size in logical pixels.
    pub min_size: Option<(f64, f64)>,
    /// Whether the window can be resized (true by default).
    pub resizable: bool,
    /// Open maximized.
    pub maximized: bool,
    /// Open borderless-fullscreen on the current monitor.
    pub fullscreen: bool,
    /// Window icon as straight-alpha RGBA8 `(width, height, pixels)`.
    pub icon: Option<(u32, u32, Vec<u8>)>,
    /// Custom faces registered on the runner's fonts before the first
    /// frame: design languages work in windows, not just headlessly.
    pub fonts: Vec<(fenestra_core::FamilyRole, Vec<u8>)>,
    /// Keep the window above all normal windows (NEOSONIA patch).
    pub always_on_top: bool,
    /// Draw the OS title bar and border (true by default; NEOSONIA patch).
    pub decorations: bool,
    /// Request a transparent window surface (NEOSONIA patch). The shell
    /// selects a premultiplied surface alpha mode when the driver supports
    /// it and clears the window background to alpha 0; app content still
    /// paints its own (possibly translucent) fills.
    pub transparent: bool,
    /// Initial outer top-left position in *physical* pixels (NEOSONIA
    /// patch). `None` lets the OS place the window. On Windows this is the
    /// outer (title-bar) corner; multiply logical coordinates by the monitor
    /// scale factor before passing them here.
    pub position: Option<(f64, f64)>,
    /// Hide the taskbar button (Windows only; ignored elsewhere — NEOSONIA
    /// patch).
    pub skip_taskbar: bool,
}

impl WindowOptions {
    /// A window with the given title and the default 1024x768 logical size.
    pub fn titled(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            inner_size: (1024.0, 768.0),
            min_size: None,
            resizable: true,
            maximized: false,
            fullscreen: false,
            icon: None,
            fonts: Vec::new(),
            always_on_top: false,
            decorations: true,
            transparent: false,
            position: None,
            skip_taskbar: false,
        }
    }

    /// Sets the initial inner size in logical pixels.
    pub fn with_size(mut self, width: f64, height: f64) -> Self {
        self.inner_size = (width, height);
        self
    }

    /// Sets the minimum inner size in logical pixels.
    pub fn with_min_size(mut self, width: f64, height: f64) -> Self {
        self.min_size = Some((width, height));
        self
    }

    /// Allows or forbids resizing (allowed by default).
    pub fn with_resizable(mut self, resizable: bool) -> Self {
        self.resizable = resizable;
        self
    }

    /// Opens the window maximized.
    pub fn maximized(mut self) -> Self {
        self.maximized = true;
        self
    }

    /// Opens borderless-fullscreen on the current monitor.
    pub fn fullscreen(mut self) -> Self {
        self.fullscreen = true;
        self
    }

    /// Sets the window icon from straight-alpha RGBA8 pixels (ignored on
    /// platforms without window icons, including the web).
    pub fn with_icon(mut self, width: u32, height: u32, rgba: Vec<u8>) -> Self {
        self.icon = Some((width, height, rgba));
        self
    }

    /// Registers a custom face under a family role for this window's
    /// fonts (TTF/OTF bytes; see `Fonts::register`).
    pub fn with_font(mut self, role: fenestra_core::FamilyRole, data: Vec<u8>) -> Self {
        self.fonts.push((role, data));
        self
    }

    /// Keeps the window above normal windows (NEOSONIA patch).
    pub fn with_always_on_top(mut self, always_on_top: bool) -> Self {
        self.always_on_top = always_on_top;
        self
    }

    /// Turns the OS title bar and border on or off (NEOSONIA patch).
    pub fn with_decorations(mut self, decorations: bool) -> Self {
        self.decorations = decorations;
        self
    }

    /// Requests a transparent window surface (NEOSONIA patch). The clear
    /// color becomes transparent; paint an opaque or translucent background
    /// in the view yourself if you want one.
    pub fn with_transparent(mut self, transparent: bool) -> Self {
        self.transparent = transparent;
        self
    }

    /// Sets the initial outer top-left position in physical pixels
    /// (NEOSONIA patch). Use `logical_position * monitor_scale_factor` when
    /// you have logical coordinates.
    pub fn with_position(mut self, x: f64, y: f64) -> Self {
        self.position = Some((x, y));
        self
    }

    /// Hides the taskbar button (Windows only; ignored elsewhere — NEOSONIA
    /// patch).
    pub fn with_skip_taskbar(mut self, skip_taskbar: bool) -> Self {
        self.skip_taskbar = skip_taskbar;
        self
    }
}

enum RenderState {
    Active {
        surface: Box<RenderSurface<'static>>,
        valid_surface: bool,
        window: Arc<Window>,
    },
    Suspended(Option<Arc<Window>>),
    /// Window created; the async surface setup is in flight (web only).
    #[cfg(target_arch = "wasm32")]
    Pending(Arc<Window>),
}

/// Shared surface plumbing for every windowed runner.
struct WindowShell {
    context: RenderContext,
    renderers: Vec<Option<Renderer>>,
    state: RenderState,
    scene: Scene,
    options: WindowOptions,
    background: Color,
    /// The first unrecoverable failure (GPU/surface/renderer). Recorded by
    /// [`Self::fail`], which also stops the event loop; the runner entry
    /// function returns it once `run_app` unwinds. Runners with secondary
    /// windows funnel those failures into the main shell's slot too.
    fatal: Option<ShellError>,
    /// Completed async surface setup, parked until the next [`Self::pump`]
    /// (web only; the web is single-threaded so `Rc<RefCell>` suffices).
    #[cfg(target_arch = "wasm32")]
    ready: WasmReady,
}

/// The handoff slot for the web's async surface creation: the setup result,
/// carrying the error when WebGPU is unavailable.
#[cfg(target_arch = "wasm32")]
type WasmReady = std::rc::Rc<
    std::cell::RefCell<Option<Result<(RenderContext, Box<RenderSurface<'static>>), ShellError>>>,
>;

impl WindowShell {
    fn new(options: WindowOptions, background: Color) -> Self {
        Self {
            context: RenderContext::new(),
            renderers: Vec::new(),
            state: RenderState::Suspended(None),
            scene: Scene::new(),
            options,
            background,
            fatal: None,
            #[cfg(target_arch = "wasm32")]
            ready: WasmReady::default(),
        }
    }

    /// Records the first unrecoverable failure and stops the event loop; the
    /// runner entry function surfaces it as its `Err` return. Adversarial
    /// review 2026-07 (finding B): these paths used to be `expect`s — a VM
    /// without GPU drivers or a mid-run device loss took the process down
    /// instead of returning an actionable error.
    fn fail(&mut self, event_loop: &ActiveEventLoop, err: ShellError) {
        // The web loop cannot return an error (`run_app` already returned):
        // surface the failure in the browser console before stopping.
        #[cfg(target_arch = "wasm32")]
        web_sys::console::error_1(&format!("fenestra: {err}").into());
        if self.fatal.is_none() {
            self.fatal = Some(err);
        }
        event_loop.exit();
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) -> Result<(), ShellError> {
        self.resumed_with(event_loop, |_, _| {})
    }

    /// Like [`Self::resumed`], but runs `before_visible` between window
    /// creation and the first `set_visible(true)` — the AccessKit adapter
    /// must attach while the window is still hidden.
    fn resumed_with(
        &mut self,
        event_loop: &ActiveEventLoop,
        before_visible: impl FnOnce(&ActiveEventLoop, &Arc<Window>),
    ) -> Result<(), ShellError> {
        let cached = match &mut self.state {
            RenderState::Suspended(cached_window) => cached_window.take(),
            _ => return Ok(()),
        };
        let window = match cached {
            Some(window) => window,
            None => {
                let attrs = Window::default_attributes()
                    .with_title(self.options.title.clone())
                    .with_inner_size(LogicalSize::new(
                        self.options.inner_size.0,
                        self.options.inner_size.1,
                    ))
                    .with_resizable(self.options.resizable)
                    .with_maximized(self.options.maximized)
                    .with_decorations(self.options.decorations)
                    .with_transparent(self.options.transparent)
                    .with_visible(false);
                let attrs = if self.options.always_on_top {
                    attrs.with_window_level(winit::window::WindowLevel::AlwaysOnTop)
                } else {
                    attrs
                };
                let attrs = match self.options.position {
                    Some((x, y)) => {
                        // winit 0.30's `Position::Physical` carries integer
                        // physical pixels; round the public f64 API.
                        let position = PhysicalPosition::new(x.round() as i32, y.round() as i32);
                        attrs.with_position(Position::Physical(position))
                    }
                    None => attrs,
                };
                let attrs = match self.options.min_size {
                    Some((w, h)) => attrs.with_min_inner_size(LogicalSize::new(w, h)),
                    None => attrs,
                };
                let attrs = if self.options.fullscreen {
                    attrs.with_fullscreen(Some(winit::window::Fullscreen::Borderless(None)))
                } else {
                    attrs
                };
                #[cfg(not(target_arch = "wasm32"))]
                let attrs = match self.options.icon.clone() {
                    Some((w, h, rgba)) => match winit::window::Icon::from_rgba(rgba, w, h) {
                        Ok(icon) => attrs.with_window_icon(Some(icon)),
                        // Malformed icon data: open without one, never panic.
                        Err(_) => attrs,
                    },
                    None => attrs,
                };
                #[cfg(windows)]
                let attrs = {
                    use winit::platform::windows::WindowAttributesExtWindows;
                    attrs.with_skip_taskbar(self.options.skip_taskbar)
                };
                #[cfg(target_arch = "wasm32")]
                let attrs = {
                    use winit::platform::web::WindowAttributesExtWebSys;
                    // winit creates the canvas; have it inserted into the page.
                    attrs.with_append(true)
                };
                Arc::new(
                    event_loop
                        .create_window(attrs)
                        .map_err(ShellError::WindowCreate)?,
                )
            }
        };
        before_visible(event_loop, &window);
        // NEOSONIA patch: 让应用层能通过 `fenestra_shell::{set_visible, dock, ...}`
        // 控制原生窗口（右侧常驻浮窗 + 全局快捷键显隐需要）。
        crate::control::attach(window.clone());
        // NEOSONIA patch: winit 的 OLE 拖放实测在透明/置顶无边框窗口上收不到，
        // 这里安装 Win32 WM_DROPFILES 兜底（与 OLE 不冲突）。
        #[cfg(windows)]
        if let Ok(handle) = {
            use winit::raw_window_handle::HasWindowHandle as _;
            window.window_handle()
        } && let winit::raw_window_handle::RawWindowHandle::Win32(w32) = handle.as_raw()
        {
            crate::dragdrop::install(w32.hwnd.get());
        }
        let was_hidden = window.is_visible() == Some(false);
        self.activate(window.clone())?;
        if was_hidden {
            window.set_visible(true);
        }
        Ok(())
    }

    /// Builds (or rebuilds, after a lost surface) the swapchain for `window`
    /// and enters the active state.
    #[cfg(not(target_arch = "wasm32"))]
    fn activate(&mut self, window: Arc<Window>) -> Result<(), ShellError> {
        let size = window.inner_size();
        let mut surface = pollster::block_on(self.context.create_surface(
            window.clone(),
            size.width.max(1),
            size.height.max(1),
            wgpu::PresentMode::AutoVsync,
        ))
        .map_err(ShellError::Surface)?;

        // NEOSONIA diagnostic: GPU/adapter/surface facts (enable with
        // NEOSONIA_GPU_LOG=1). Helps on hybrid-GPU laptops where presenting a
        // Vulkan swapchain owned by the dGPU produces a blank window.
        if std::env::var("NEOSONIA_GPU_LOG").ok().as_deref() == Some("1") {
            let adapter = self.context.devices[surface.dev_id].adapter();
            let info = adapter.get_info();
            eprintln!(
                "[neosonia] adapter name={:?} backend={:?} device_type={:?} driver={:?} {}",
                info.name, info.backend, info.device_type, info.driver, info.driver_info
            );
            let caps = surface.surface.get_capabilities(adapter);
            eprintln!(
                "[neosonia] caps formats={:?} alpha_modes={:?} present_modes={:?}",
                caps.formats, caps.alpha_modes, caps.present_modes
            );
            eprintln!(
                "[neosonia] surface config format={:?} present_mode={:?} alpha_mode={:?} size={}x{}",
                surface.config.format,
                surface.config.present_mode,
                surface.config.alpha_mode,
                size.width,
                size.height
            );
        }


        // swapchain. vello configures `CompositeAlphaMode::Auto`; on Windows
        // that can resolve to `Opaque`, which makes every clear opaque and
        // silently defeats `with_transparent(true)`. Prefer premultiplied
        // (matching vello's output), then postmultiplied, then whatever the
        // driver advertises.
        if self.options.transparent {
            let caps = surface
                .surface
                .get_capabilities(self.context.devices[surface.dev_id].adapter());
            let alpha_mode = if caps
                .alpha_modes
                .contains(&wgpu::CompositeAlphaMode::PreMultiplied)
            {
                wgpu::CompositeAlphaMode::PreMultiplied
            } else if caps
                .alpha_modes
                .contains(&wgpu::CompositeAlphaMode::PostMultiplied)
            {
                wgpu::CompositeAlphaMode::PostMultiplied
            } else {
                caps.alpha_modes
                    .first()
                    .copied()
                    .unwrap_or(wgpu::CompositeAlphaMode::Auto)
            };
            surface.config.alpha_mode = alpha_mode;
            self.context.configure_surface(&surface);
        }

        self.renderers
            .resize_with(self.context.devices.len(), || None);
        if self.renderers[surface.dev_id].is_none() {
            self.renderers[surface.dev_id] = Some(
                Renderer::new(
                    &self.context.devices[surface.dev_id].device,
                    RendererOptions {
                        // `FENESTRA_CPU` runs vello's compute stages on the
                        // CPU — the live-window switch for software adapters
                        // (see `CPU_ENV`).
                        use_cpu: crate::headless::cpu_compute_requested(),
                        antialiasing_support: AaSupport::area_only(),
                        ..Default::default()
                    },
                )
                .map_err(ShellError::Vello)?,
            );
        }

        self.state = RenderState::Active {
            surface: Box::new(surface),
            valid_surface: size.width != 0 && size.height != 0,
            window,
        };
        Ok(())
    }

    /// Web: surface/device setup is async — kick it off and park in
    /// `Pending`; [`Self::pump`] finishes the activation when it lands.
    /// Setup failure (no WebGPU) travels through the `ready` slot and
    /// surfaces on the next pump. Always `Ok` here.
    #[cfg(target_arch = "wasm32")]
    fn activate(&mut self, window: Arc<Window>) -> Result<(), ShellError> {
        let size = window.inner_size();
        let ready = std::rc::Rc::clone(&self.ready);
        let win = window.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let mut context = RenderContext::new();
            let surface = context
                .create_surface(
                    win.clone(),
                    size.width.max(1),
                    size.height.max(1),
                    wgpu::PresentMode::AutoVsync,
                )
                .await
                .map(|surface| (context, Box::new(surface)))
                .map_err(ShellError::Surface);
            *ready.borrow_mut() = Some(surface);
            win.request_redraw();
        });
        self.state = RenderState::Pending(window);
        Ok(())
    }

    /// Completes a pending web activation once the async setup finished.
    /// No-op on native and while nothing is pending.
    fn pump(&mut self) -> Result<(), ShellError> {
        #[cfg(target_arch = "wasm32")]
        if let RenderState::Pending(window) = &self.state
            && let Some(setup) = self.ready.borrow_mut().take()
        {
            let (context, surface) = setup?;
            let window = window.clone();
            self.context = context;
            self.renderers.clear();
            self.renderers
                .resize_with(self.context.devices.len(), || None);
            if self.renderers[surface.dev_id].is_none() {
                self.renderers[surface.dev_id] = Some(
                    Renderer::new(
                        &self.context.devices[surface.dev_id].device,
                        RendererOptions {
                            // No env vars on the web; WebGPU implies a real
                            // adapter, so the CPU pipeline has no use here.
                            use_cpu: false,
                            antialiasing_support: AaSupport::area_only(),
                            ..Default::default()
                        },
                    )
                    .map_err(ShellError::Vello)?,
                );
            }
            let size = window.inner_size();
            self.state = RenderState::Active {
                surface,
                valid_surface: size.width != 0 && size.height != 0,
                window,
            };
        }
        Ok(())
    }

    fn suspended(&mut self) {
        if let RenderState::Active { window, .. } = &self.state {
            self.state = RenderState::Suspended(Some(window.clone()));
        }
    }

    fn window(&self) -> Option<&Arc<Window>> {
        match &self.state {
            RenderState::Active { window, .. } => Some(window),
            _ => None,
        }
    }

    fn resized(&mut self, width: u32, height: u32) {
        let RenderState::Active {
            surface,
            valid_surface,
            window,
        } = &mut self.state
        else {
            return;
        };
        if width != 0 && height != 0 {
            self.context.resize_surface(surface, width, height);
            *valid_surface = true;
        } else {
            *valid_surface = false;
        }
        window.request_redraw();
    }

    /// Logical size and scale factor of the active window.
    fn logical_size(&self) -> Option<(f64, f64, f64)> {
        match &self.state {
            RenderState::Active {
                surface, window, ..
            } => {
                let scale = window.scale_factor();
                Some((
                    f64::from(surface.config.width) / scale,
                    f64::from(surface.config.height) / scale,
                    scale,
                ))
            }
            _ => None,
        }
    }

    /// Scales the logical fragment to physical pixels and presents it.
    /// Recoverable surface states (lost/outdated/occluded/timeout) are
    /// handled in place; anything else is an error for [`Self::fail`].
    fn present(&mut self, fragment: &Scene) -> Result<(), ShellError> {
        let RenderState::Active {
            surface,
            valid_surface,
            window,
        } = &mut self.state
        else {
            return Ok(());
        };
        if !*valid_surface {
            return Ok(());
        }
        let width = surface.config.width;
        let height = surface.config.height;
        let scale = window.scale_factor();

        self.scene.reset();
        self.scene
            .append(fragment, Some(vello::kurbo::Affine::scale(scale)));

        // NEOSONIA patch: on a transparent window the clear alpha must be 0,
        // otherwise the opaque theme background fills every unpainted pixel
        // and the window looks solid. App content can still paint a
        // translucent (or opaque) fill on top.
        let base_color = if self.options.transparent {
            self.background.with_alpha(0.0)
        } else {
            self.background
        };

        let handle = &self.context.devices[surface.dev_id];
        self.renderers[surface.dev_id]
            .as_mut()
            .expect("renderer exists for surface device")
            .render_to_texture(
                &handle.device,
                &handle.queue,
                &self.scene,
                &surface.target_view,
                &RenderParams {
                    base_color,
                    width,
                    height,
                    antialiasing_method: aa_config(),
                },
            )
            .map_err(ShellError::Vello)?;

        let surface_texture = match surface.surface.get_current_texture() {
            CurrentSurfaceTexture::Success(texture) => texture,
            CurrentSurfaceTexture::Outdated | CurrentSurfaceTexture::Suboptimal(_) => {
                self.context.configure_surface(surface);
                window.request_redraw();
                return Ok(());
            }
            CurrentSurfaceTexture::Occluded => {
                // Hidden window: skip the frame; WindowEvent::Occluded(false)
                // requests the next redraw when it becomes visible again.
                return Ok(());
            }
            CurrentSurfaceTexture::Timeout => {
                window.request_redraw();
                return Ok(());
            }
            CurrentSurfaceTexture::Lost => {
                // Recoverable (GPU reset, driver update, display change):
                // rebuild the swapchain on the same window and repaint.
                let window = window.clone();
                window.request_redraw();
                return self.activate(window);
            }
            CurrentSurfaceTexture::Validation => return Err(ShellError::SurfaceValidation),
        };

        let mut encoder = handle
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("fenestra surface blit"),
            });
        surface.blitter.copy(
            &handle.device,
            &mut encoder,
            &surface.target_view,
            &surface_texture
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default()),
        );
        handle.queue.submit([encoder.finish()]);
        surface_texture.present();
        handle
            .device
            .poll(wgpu::PollType::Poll)
            .map_err(ShellError::Poll)?;
        Ok(())
    }
}

// ------------------------------------------------------------- run_scene

/// Opens a window and repaints via `paint(scene, logical_w, logical_h, bg)`
/// on every redraw. Blocks until the window closes. Low-level escape hatch;
/// element views should prefer [`run_static`] (or the M4 `App` runner).
#[cfg(not(target_arch = "wasm32"))]
pub fn run_scene(
    options: WindowOptions,
    background: Color,
    paint: impl FnMut(&mut Scene, f64, f64, Color) + 'static,
) -> Result<(), ShellError> {
    let event_loop = EventLoop::new().map_err(ShellError::EventLoop)?;
    let mut app = SceneApp {
        shell: WindowShell::new(options, background),
        fragment: Scene::new(),
        paint: Box::new(paint),
    };
    let run = event_loop.run_app(&mut app).map_err(ShellError::EventLoop);
    match app.shell.fatal.take() {
        Some(err) => Err(err),
        None => run,
    }
}

#[cfg(not(target_arch = "wasm32"))]
struct SceneApp {
    shell: WindowShell,
    fragment: Scene,
    paint: PaintFn,
}

#[cfg(not(target_arch = "wasm32"))]
impl ApplicationHandler for SceneApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if let Err(e) = self.shell.resumed(event_loop) {
            self.shell.fail(event_loop, e);
        }
    }

    fn suspended(&mut self, _event_loop: &ActiveEventLoop) {
        self.shell.suspended();
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        if self.shell.window().is_none_or(|w| w.id() != window_id) {
            return;
        }
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => self.shell.resized(size.width, size.height),
            WindowEvent::ScaleFactorChanged { .. } => {
                if let Some(w) = self.shell.window() {
                    w.request_redraw();
                }
            }
            WindowEvent::Occluded(occluded) => {
                if !occluded && let Some(w) = self.shell.window() {
                    w.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                let Some((lw, lh, _scale)) = self.shell.logical_size() else {
                    return;
                };
                self.fragment.reset();
                let bg = self.shell.background;
                (self.paint)(&mut self.fragment, lw, lh, bg);
                let fragment = std::mem::replace(&mut self.fragment, Scene::new());
                if let Err(e) = self.shell.present(&fragment) {
                    self.shell.fail(event_loop, e);
                }
                self.fragment = fragment;
            }
            _ => {}
        }
    }
}

/// A [`FrameState`] for a live window, seeded with the OS "reduce motion"
/// accessibility setting so animations snap for users who asked for it.
#[cfg(not(target_arch = "wasm32"))]
fn live_state() -> FrameState {
    let mut state = FrameState::new();
    state.reduced_motion = crate::reduce_motion::os_reduce_motion();
    state
}

/// The wasm build reads `prefers-reduced-motion` through the browser, so the
/// live state is the plain default here.
#[cfg(target_arch = "wasm32")]
fn live_state() -> FrameState {
    FrameState::new()
}

// ------------------------------------------------------------- run_static

/// Opens a window showing a message-free element view. The view is rebuilt
/// on every redraw; scroll state persists in a [`FrameState`]. Blocks until
/// the window closes.
#[cfg(not(target_arch = "wasm32"))]
pub fn run_static(
    options: WindowOptions,
    theme: Theme,
    view: impl Fn(&Theme) -> Element<()> + 'static,
) -> Result<(), ShellError> {
    let event_loop = EventLoop::new().map_err(ShellError::EventLoop)?;
    let background = theme.bg;
    let mut fonts = Fonts::with_system();
    for (role, data) in &options.fonts {
        fonts.register(*role, data.clone());
    }
    let mut app = StaticApp {
        shell: WindowShell::new(options, background),
        theme,
        fonts,
        state: live_state(),
        view: Box::new(view),
        cursor: Point::ORIGIN,
        started: Instant::now(),
        last_frame: None,
    };
    let run = event_loop.run_app(&mut app).map_err(ShellError::EventLoop);
    match app.shell.fatal.take() {
        Some(err) => Err(err),
        None => run,
    }
}

#[cfg(not(target_arch = "wasm32"))]
struct StaticApp {
    shell: WindowShell,
    theme: Theme,
    fonts: Fonts,
    state: FrameState,
    view: ViewFn,
    /// Cursor position in logical coordinates.
    cursor: Point,
    started: Instant,
    /// The frame from the last redraw, used to route input between frames.
    last_frame: Option<fenestra_core::Frame>,
}

#[cfg(not(target_arch = "wasm32"))]
impl StaticApp {
    fn redraw(&mut self, event_loop: &ActiveEventLoop) {
        let Some((lw, lh, scale)) = self.shell.logical_size() else {
            return;
        };
        self.state.tick(self.started.elapsed().as_secs_f64());
        let el = (self.view)(&self.theme);
        let __t_frame = std::time::Instant::now();
        #[expect(clippy::cast_possible_truncation, reason = "window sizes fit in f32")]
        let frame = build_frame(
            &el,
            &self.theme,
            &mut self.fonts,
            &mut self.state,
            (lw as f32, lh as f32),
            scale,
        );
        // Live-window limitation: the swapchain path renders a single pass, so
        // frosted glass shows its translucent tint without the CPU backdrop
        // blur (which needs a read-back). Headless rendering — the golden source
        // of truth — uses the two-pass `render_plan`. See ARCHITECTURE.md
        // ("Real frosted-glass backdrop blur").
        let __frame_ms = __t_frame.elapsed().as_secs_f32() * 1000.0;
        let __t_paint = std::time::Instant::now();
        let scene = frame.paint(&mut self.fonts, &mut self.state);
        let __paint_ms = __t_paint.elapsed().as_secs_f32() * 1000.0;
        if let Err(e) = self.shell.present(&scene) {
            self.shell.fail(event_loop, e);
            return;
        }
        if frame.animating {
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(16),
            ));
        } else {
            event_loop.set_control_flow(ControlFlow::Wait);
        }
        self.last_frame = Some(frame);
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl ApplicationHandler for StaticApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if let Err(e) = self.shell.resumed(event_loop) {
            self.shell.fail(event_loop, e);
        }
    }

    fn suspended(&mut self, _event_loop: &ActiveEventLoop) {
        self.shell.suspended();
    }

    fn new_events(&mut self, _event_loop: &ActiveEventLoop, cause: StartCause) {
        if matches!(cause, StartCause::ResumeTimeReached { .. })
            && let Some(w) = self.shell.window()
        {
            w.request_redraw();
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        if self.shell.window().is_none_or(|w| w.id() != window_id) {
            return;
        }
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => self.shell.resized(size.width, size.height),
            WindowEvent::ScaleFactorChanged { .. } => {
                if let Some(w) = self.shell.window() {
                    w.request_redraw();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let scale = self.shell.window().map_or(1.0, |w| w.scale_factor());
                self.cursor = Point::new(position.x / scale, position.y / scale);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let scale = self.shell.window().map_or(1.0, |w| w.scale_factor());
                let (dx, dy) = wheel_deltas(delta, scale);
                if let Some(frame) = &self.last_frame {
                    let y_target = (dy.abs() > 1e-3)
                        .then(|| frame.scrollable_y_at(self.cursor))
                        .flatten();
                    let x_target = (dx.abs() > 1e-3)
                        .then(|| frame.scrollable_x_at(self.cursor))
                        .flatten();
                    #[expect(clippy::cast_possible_truncation, reason = "scroll deltas fit in f32")]
                    {
                        if let Some(id) = y_target {
                            self.state.scroll_by(id, -dy as f32);
                        }
                        if let Some(id) = x_target {
                            self.state.scroll_by_x(id, -dx as f32);
                        }
                    }
                    if (y_target.is_some() || x_target.is_some())
                        && let Some(w) = self.shell.window()
                    {
                        w.request_redraw();
                    }
                }
            }
            WindowEvent::RedrawRequested => self.redraw(event_loop),
            _ => {}
        }
    }
}

// ------------------------------------------------------------- run_app

/// User events crossing into the app runner's loop: type-erased app
/// messages from a [`fenestra_core::Proxy`] (any thread), and AccessKit's
/// activation/action events.
enum RunnerEvent {
    App(Box<dyn std::any::Any + Send>),
    /// NEOSONIA patch: Win32 WM_DROPFILES 收到文件，通知 runner 去 drain 队列。
    #[cfg(windows)]
    DropsPending,
    #[cfg(not(target_arch = "wasm32"))]
    Access(accesskit_winit::Event),
    /// A native menu item was chosen (the muda item id).
    #[cfg(target_os = "macos")]
    Menu(String),
}

#[cfg(not(target_arch = "wasm32"))]
impl From<accesskit_winit::Event> for RunnerEvent {
    fn from(event: accesskit_winit::Event) -> Self {
        Self::Access(event)
    }
}

/// Runs an [`App`]: the full Elm-shaped loop with hit testing, hover/active/
/// focus, keyboard navigation, message dispatch, and event-driven repaint
/// (animation frames only while something animates). Calls [`App::init`]
/// with a [`fenestra_core::Proxy`] before the first frame; proxied messages
/// wake the loop and repaint. Blocks until the window closes.
pub fn run_app<A: App + 'static>(mut app: A, options: WindowOptions) -> Result<(), ShellError>
where
    A::Msg: Send,
{
    let event_loop = EventLoop::<RunnerEvent>::with_user_event()
        .build()
        .map_err(ShellError::EventLoop)?;
    #[cfg(not(target_arch = "wasm32"))]
    let access_proxy = event_loop.create_proxy();
    let proxy = event_loop.create_proxy();
    // Dropped silently once the loop is gone (window closed).
    let msg_proxy = fenestra_core::Proxy::new(move |msg: A::Msg| {
        let _ = proxy.send_event(RunnerEvent::App(Box::new(msg)));
    });
    app.init(msg_proxy.clone());
    let background = app.theme().bg;
    let mut fonts = Fonts::with_system();
    for (role, data) in &options.fonts {
        fonts.register(*role, data.clone());
    }
    let mut state = live_state();
    #[cfg(not(target_arch = "wasm32"))]
    state.set_clipboard(Box::new(crate::OsClipboard::default()));
    // On the web, copy-out reaches the system clipboard; paste-in from
    // other apps stays in-app (see `WebClipboard`).
    #[cfg(target_arch = "wasm32")]
    state.set_clipboard(Box::new(crate::WebClipboard::default()));
    let runner = AppRunner {
        shell: WindowShell::new(options, background),
        msg_proxy,
        #[cfg(not(target_arch = "wasm32"))]
        subs: std::collections::HashMap::new(),
        #[cfg(target_os = "macos")]
        menu: None,
        app,
        fonts,
        state,
        cursor: Point::ORIGIN,
        started: Instant::now(),
        last: None,
        dirty: true,
        cached_scene: None,
        modifiers: winit::keyboard::ModifiersState::empty(),
        #[cfg(not(target_arch = "wasm32"))]
        adapter: None,
        #[cfg(not(target_arch = "wasm32"))]
        proxy: access_proxy,
        #[cfg(not(target_arch = "wasm32"))]
        secondary: std::collections::HashMap::new(),
    };
    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut runner = runner;
        // Startup effect + initial subscriptions: results arrive as user
        // events once the loop is running.
        let cmd = runner.app.init_cmd();
        runner.run_cmd(cmd);
        runner.reconcile_subs();
        #[cfg(target_os = "macos")]
        {
            let menu_proxy = event_loop.create_proxy();
            muda::MenuEvent::set_event_handler(Some(move |event: muda::MenuEvent| {
                let _ = menu_proxy.send_event(RunnerEvent::Menu(event.id.0.clone()));
            }));
            runner.reconcile_menu();
        }
        let run = event_loop
            .run_app(&mut runner)
            .map_err(ShellError::EventLoop);
        match runner.shell.fatal.take() {
            Some(err) => Err(err),
            None => run,
        }
    }
    #[cfg(target_arch = "wasm32")]
    {
        use winit::platform::web::EventLoopExtWebSys;
        let mut runner = runner;
        let cmd = runner.app.init_cmd();
        runner.run_cmd(cmd);
        // Non-blocking on the web: the loop keeps running after main returns.
        event_loop.spawn_app(runner);
        Ok(())
    }
}

struct AppRunner<A: App> {
    shell: WindowShell,
    app: A,
    fonts: Fonts,
    state: FrameState,
    /// Message proxy shared with [`App::init`]: effect results and
    /// subscription ticks are delivered through it, waking the loop.
    msg_proxy: fenestra_core::Proxy<A::Msg>,
    /// Running subscription timers, reconciled against
    /// [`App::subscriptions`] after every update (native only).
    #[cfg(not(target_arch = "wasm32"))]
    subs: std::collections::HashMap<String, SubHandle>,
    /// The attached native menu, reconciled against [`App::menu`] by
    /// structural fingerprint (macOS only).
    #[cfg(target_os = "macos")]
    menu: Option<MenuHandle<A::Msg>>,
    cursor: Point,
    started: Instant,
    /// View and frame from the last redraw, for input routing.
    last: Option<(Element<A::Msg>, fenestra_core::Frame)>,
    /// Anything changed since the last full frame? OS-driven redraws
    /// (expose, un-occlude) re-present the cached scene when clean —
    /// the whole build/layout/paint pipeline is skipped.
    dirty: bool,
    /// The last painted scene with its (logical w, h, scale) key.
    cached_scene: Option<(Scene, (f64, f64, f64))>,
    modifiers: winit::keyboard::ModifiersState,
    /// The AccessKit adapter, created before the window first shows.
    #[cfg(not(target_arch = "wasm32"))]
    adapter: Option<accesskit_winit::Adapter>,
    /// Loop proxy handed to the adapter for activation/action events.
    #[cfg(not(target_arch = "wasm32"))]
    proxy: EventLoopProxy<RunnerEvent>,
    /// Secondary windows declared by [`App::windows`], keyed by their
    /// stable key and reconciled after every update (native only).
    #[cfg(not(target_arch = "wasm32"))]
    secondary: std::collections::HashMap<String, SecondaryWindow<A>>,
}

/// One reconciled secondary window: its own surface, retained state, and
/// accessibility adapter; app state and fonts are shared.
#[cfg(not(target_arch = "wasm32"))]
struct SecondaryWindow<A: App> {
    shell: WindowShell,
    state: FrameState,
    cursor: Point,
    last: Option<(Element<A::Msg>, fenestra_core::Frame)>,
    on_close: A::Msg,
    title: String,
    adapter: Option<accesskit_winit::Adapter>,
}

/// A running subscription timer: its period (part of its identity — a
/// changed period restarts the sub) and the cancel flag its thread checks
/// after each sleep. Dropping the handle cancels within one period.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct SubHandle {
    period: std::time::Duration,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(not(target_arch = "wasm32"))]
impl Drop for SubHandle {
    fn drop(&mut self) {
        self.cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Runs one deferred effect unit off the UI thread, delivering its message
/// through `proxy`.
///
/// If the thread cannot be spawned — a process at its thread limit, a
/// sandbox that forbids threads — the unit runs *here* instead. Blocking
/// the UI thread for the length of one effect is bad; dropping the effect
/// is worse, because the app then waits forever for a reply that is never
/// coming and the user is left with a spinner that never stops. Losing a
/// message is the one outcome this must not have.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn spawn_effect_unit<Msg: 'static>(
    unit: fenestra_core::CmdUnit<Msg>,
    proxy: &fenestra_core::Proxy<Msg>,
) where
    // The message itself never crosses the boundary as a value — the unit
    // produces it on the worker and hands it straight to the proxy — so
    // these are the real requirements, not `Msg: Send`.
    fenestra_core::CmdUnit<Msg>: Send,
    fenestra_core::Proxy<Msg>: Send,
{
    dispatch_effect_unit(unit, proxy, |work| {
        std::thread::Builder::new()
            .name("fenestra-cmd".into())
            .spawn(work)
            .map(drop)
    });
}

/// The spawn-or-fall-back half of [`spawn_effect_unit`], with the thread
/// spawner passed in so the failure path is reachable from a test — an
/// out-of-threads process is not something a test can arrange for real,
/// and this path only matters when it is the one that runs.
#[cfg(not(target_arch = "wasm32"))]
fn dispatch_effect_unit<Msg: 'static, S>(
    unit: fenestra_core::CmdUnit<Msg>,
    proxy: &fenestra_core::Proxy<Msg>,
    spawn: S,
) where
    fenestra_core::CmdUnit<Msg>: Send,
    fenestra_core::Proxy<Msg>: Send,
    S: FnOnce(Box<dyn FnOnce() + Send + 'static>) -> std::io::Result<()>,
{
    // The spawner consumes the closure, and with it the unit, so the unit
    // is parked somewhere both sides can reach: the worker takes it if it
    // starts, and we take it back if it never does.
    let slot = std::sync::Arc::new(std::sync::Mutex::new(Some(unit)));
    let worker_slot = std::sync::Arc::clone(&slot);
    let worker_proxy = proxy.clone();
    let spawned = spawn(Box::new(move || {
        if let Some(unit) = worker_slot.lock().ok().and_then(|mut g| g.take()) {
            worker_proxy.send(unit.block());
        }
    }));
    let Err(e) = spawned else { return };
    // Report the spawn failure on its own, before trying to recover from
    // it. Sharing one condition with the recovery meant that failing to
    // reclaim the unit also swallowed the message about failing to spawn —
    // a silent loss inside the function written to prevent silent losses.
    let recovered = slot.lock().ok().and_then(|mut g| g.take());
    match recovered {
        Some(unit) => {
            eprintln!(
                "fenestra: could not spawn an effect worker thread ({e}); running the effect on \
                 the UI thread instead — the window will not repaint until it finishes"
            );
            proxy.send(unit.block());
        }
        None => eprintln!(
            "fenestra: could not spawn an effect worker thread ({e}), and the effect could not be \
             reclaimed to run here either — its message will never arrive"
        ),
    }
}

/// Reconciles running subscription timers against a desired set: new keys
/// start a timer thread delivering ticks through `proxy`, missing keys
/// cancel theirs (within one period), a changed period restarts. Shared by
/// the app runner and [`crate::Embedded`].
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn reconcile_subs_into<Msg: 'static>(
    subs: &mut std::collections::HashMap<String, SubHandle>,
    desired: Vec<fenestra_core::Sub<Msg>>,
    proxy: &fenestra_core::Proxy<Msg>,
) {
    subs.retain(|key, handle| {
        desired
            .iter()
            .any(|s| s.key() == key && s.period() == handle.period)
    });
    for sub in desired {
        if subs.contains_key(sub.key()) {
            continue;
        }
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handle = SubHandle {
            period: sub.period(),
            cancel: std::sync::Arc::clone(&cancel),
        };
        let key = sub.key().to_owned();
        let proxy = proxy.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("fenestra-sub-{key}"))
            .spawn(move || {
                loop {
                    std::thread::sleep(sub.period());
                    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    proxy.send(sub.tick());
                }
            });
        if let Err(e) = spawned {
            // Deliberately not inserted: leaving the key absent is what
            // makes the next reconcile try again. Say so, because a timer
            // is often the only thing that would *cause* another
            // reconcile, so "later" can mean "never" — unlike a one-shot
            // effect, there is nothing to run inline as a fallback.
            eprintln!(
                "fenestra: could not spawn the timer for subscription {key:?} ({e}); it is not \
                 running, and will only be retried if something else updates the app"
            );
            continue;
        }
        subs.insert(key, handle);
    }
}

/// The attached native menu: its structural fingerprint, the live muda
/// menu (kept alive for the app's lifetime), and item-id → message map.
#[cfg(target_os = "macos")]
struct MenuHandle<Msg> {
    fingerprint: String,
    _menu: muda::Menu,
    actions: std::collections::HashMap<String, Msg>,
}

/// Builds a muda menu from the declarative spec: the platform app menu
/// (with Quit) first, then the declared menus. Returns the id → message
/// map alongside.
#[cfg(target_os = "macos")]
fn build_menu<Msg: Clone>(
    spec: &fenestra_core::MenuSpec<Msg>,
) -> Result<(muda::Menu, std::collections::HashMap<String, Msg>), muda::Error> {
    fn append_items<Msg: Clone>(
        submenu: &muda::Submenu,
        items: &[fenestra_core::MenuItemDesc<Msg>],
        actions: &mut std::collections::HashMap<String, Msg>,
    ) -> Result<(), muda::Error> {
        for item in items {
            match item {
                fenestra_core::MenuItemDesc::Item {
                    title,
                    msg,
                    accelerator,
                    enabled,
                } => {
                    let accel = accelerator.as_ref().and_then(|a| {
                        a.parse::<muda::accelerator::Accelerator>()
                            .map_err(|e| {
                                eprintln!("fenestra: menu accelerator {a:?} did not parse: {e}");
                            })
                            .ok()
                    });
                    let mi = muda::MenuItem::new(title, *enabled, accel);
                    actions.insert(mi.id().0.clone(), msg.clone());
                    submenu.append(&mi)?;
                }
                fenestra_core::MenuItemDesc::Separator => {
                    submenu.append(&muda::PredefinedMenuItem::separator())?;
                }
                fenestra_core::MenuItemDesc::Submenu { title, items } => {
                    let nested = muda::Submenu::new(title, true);
                    append_items(&nested, items, actions)?;
                    submenu.append(&nested)?;
                }
            }
        }
        Ok(())
    }
    let menu = muda::Menu::new();
    let mut actions = std::collections::HashMap::new();
    // macOS: the first submenu becomes the application menu.
    let app_menu = muda::Submenu::new("App", true);
    app_menu.append(&muda::PredefinedMenuItem::quit(None))?;
    menu.append(&app_menu)?;
    for desc in &spec.menus {
        let submenu = muda::Submenu::new(&desc.title, true);
        append_items(&submenu, &desc.items, &mut actions)?;
        menu.append(&submenu)?;
    }
    Ok((menu, actions))
}

impl<A: App> AppRunner<A> {
    /// Reconciles the native menu bar against [`App::menu`]: rebuilt only
    /// when the structural fingerprint changes; `None` after a menu was
    /// attached leaves the last menu in place (macOS keeps an app menu
    /// regardless).
    #[cfg(target_os = "macos")]
    fn reconcile_menu(&mut self) {
        let Some(spec) = self.app.menu() else {
            return;
        };
        let fingerprint = spec.fingerprint();
        if self
            .menu
            .as_ref()
            .is_some_and(|m| m.fingerprint == fingerprint)
        {
            return;
        }
        match build_menu(&spec) {
            Ok((menu, actions)) => {
                menu.init_for_nsapp();
                self.menu = Some(MenuHandle {
                    fingerprint,
                    _menu: menu,
                    actions,
                });
            }
            Err(e) => eprintln!("fenestra: building the native menu failed: {e}"),
        }
    }

    /// Applies one message through [`App::update_with`] and executes the
    /// returned effect.
    fn apply(&mut self, msg: A::Msg) {
        let cmd = self.app.update_with(msg);
        self.run_cmd(cmd);
    }

    /// Executes an effect through the shared [`fenestra_core::apply_cmd`]
    /// semantics (immediate messages re-enter `update_with` FIFO, no
    /// recursion); deferred units run on worker threads (native) or the
    /// microtask queue (web), delivering results through the proxy.
    fn run_cmd(&mut self, cmd: fenestra_core::Cmd<A::Msg>) {
        let proxy = self.msg_proxy.clone();
        fenestra_core::apply_cmd(&mut self.app, cmd, &mut |unit| {
            #[cfg(not(target_arch = "wasm32"))]
            {
                spawn_effect_unit(unit, &proxy);
            }
            #[cfg(target_arch = "wasm32")]
            {
                match unit {
                    // No threads on the web: blocking tasks run inline
                    // (keep them light there) …
                    fenestra_core::CmdUnit::Task(f) => proxy.send(f()),
                    // … while futures ride the browser's microtask queue.
                    fenestra_core::CmdUnit::Future(fut) => {
                        let proxy = proxy.clone();
                        wasm_bindgen_futures::spawn_local(async move {
                            proxy.send(fut.await);
                        });
                    }
                }
            }
        });
    }

    /// Reconciles running subscription timers against
    /// [`App::subscriptions`] — see [`reconcile_subs_into`].
    #[cfg(not(target_arch = "wasm32"))]
    fn reconcile_subs(&mut self) {
        reconcile_subs_into(&mut self.subs, self.app.subscriptions(), &self.msg_proxy);
    }

    fn redraw(&mut self, event_loop: &ActiveEventLoop) {
        let __t_total = std::time::Instant::now();
        if let Err(e) = self.shell.pump() {
            self.shell.fail(event_loop, e);
            return;
        }
        let Some((lw, lh, scale)) = self.shell.logical_size() else {
            return;
        };
        // NEOSONIA patch：窗口隐藏时不做 build/layout/paint（隐藏窗口上 present 可能阻塞，
        // 而且浪费 CPU；重新显示时由收到的事件触发一次完整重绘）。
        if let Some(w) = self.shell.window()
            && w.is_visible() == Some(false)
        {
            self.dirty = false;
            return;
        }
        // Clean frame at the same size: re-present the cached scene and
        // skip build/layout/paint entirely (expose/un-occlude redraws).
        if !self.dirty
            && let Some((scene, key)) = &self.cached_scene
            && *key == (lw, lh, scale)
        {
            if let Err(e) = self.shell.present(scene) {
                self.shell.fail(event_loop, e);
            }
            return;
        }
        let theme = self.app.theme();
        self.shell.background = theme.bg;
        self.state.tick(self.started.elapsed().as_secs_f64());
        #[expect(clippy::cast_possible_truncation, reason = "window sizes fit in f32")]
        let logical = (lw as f32, lh as f32);
        let __t_view = std::time::Instant::now();
        let view = self.app.view_at(fenestra_core::MAIN_WINDOW, logical);
        let __view_ms = __t_view.elapsed().as_secs_f32() * 1000.0;
        let __t_frame = std::time::Instant::now();
        let frame = build_frame(
            &view,
            &theme,
            &mut self.fonts,
            &mut self.state,
            logical,
            scale,
        );
        // Single-pass live window: glass is tint-only here (see the `run_app`
        // redraw note); two-pass blur is the headless golden path.
        let __frame_ms = __t_frame.elapsed().as_secs_f32() * 1000.0;
        let __t_paint = std::time::Instant::now();
        let scene = frame.paint(&mut self.fonts, &mut self.state);
        let __paint_ms = __t_paint.elapsed().as_secs_f32() * 1000.0;
        let __t_present = std::time::Instant::now();
        if let Err(e) = self.shell.present(&scene) {
            self.shell.fail(event_loop, e);
            return;
        }
        let __present_ms = __t_present.elapsed().as_secs_f32() * 1000.0;
        // The frame is clean until something changes it; animation and
        // hover refresh keep it dirty so the pipeline runs again.
        if frame_log_enabled() {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            eprintln!(
                "[frame t={now_ms}] total={:.1}ms view={:.1}ms layout={:.1}ms paint={:.1}ms present={:.1}ms",
                __t_total.elapsed().as_secs_f32() * 1000.0,
                __view_ms,
                __frame_ms,
                __paint_ms,
                __present_ms
            );
        }
        self.cached_scene = Some((scene, (lw, lh, scale)));
        self.dirty = frame.animating;
        // Content may have moved under a stationary pointer (scroll,
        // layout change): refresh hover and repaint once more if it did.
        if refresh_hover(&view, &frame, &mut self.state)
            && let Some(w) = self.shell.window()
        {
            self.dirty = true;
            w.request_redraw();
        }
        if frame.animating {
            #[cfg(not(target_arch = "wasm32"))]
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(16),
            ));
            // The browser paces frames; just ask for the next one.
            #[cfg(target_arch = "wasm32")]
            if let Some(w) = self.shell.window() {
                w.request_redraw();
            }
        } else {
            #[cfg(not(target_arch = "wasm32"))]
            let secondary_animating = self
                .secondary
                .values()
                .any(|b| b.last.as_ref().is_some_and(|(_, f)| f.animating));
            #[cfg(target_arch = "wasm32")]
            let secondary_animating = false;
            if !secondary_animating {
                event_loop.set_control_flow(ControlFlow::Wait);
            }
        }
        self.last = Some((view, frame));
        // Anchor the IME candidate window to the focused caret.
        if let Some(caret) = self.state.ime_caret()
            && let Some(w) = self.shell.window()
        {
            w.set_ime_cursor_area(
                winit::dpi::LogicalPosition::new(caret.x0, caret.y0),
                winit::dpi::LogicalSize::new(1.0, caret.height()),
            );
        }
        #[cfg(not(target_arch = "wasm32"))]
        self.push_access_tree();
    }

    /// Pushes the current frame's accessibility projection to the platform
    /// (no-op until assistive technology activates the tree).
    #[cfg(not(target_arch = "wasm32"))]
    fn push_access_tree(&mut self) {
        let scale = self.shell.window().map_or(1.0, |w| w.scale_factor());
        let focus = self.state.focused();
        let Some(mut adapter) = self.adapter.take() else {
            return;
        };
        let Some((_, frame)) = &self.last else {
            self.adapter = Some(adapter);
            return;
        };
        // NEOSONIA patch：无障碍树更新里任何 panic（accesskit 内部 bug）都不应该
        // 带崩主线程/整个应用；失败就关掉无障碍，后续只记一行日志。
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            adapter.update_if_active(|| crate::access::tree_update(frame, focus, scale));
        }));
        match result {
            Ok(()) => self.adapter = Some(adapter),
            Err(_) => {
                eprintln!("[neosonia] 无障碍树更新 panic，已禁用无障碍（避免崩溃）");
            }
        }
    }

    fn input(&mut self, event: InputEvent) -> bool {
        let Some((view, frame)) = &self.last else {
            return false;
        };
        let result = dispatch(view, frame, &mut self.state, &mut self.fonts, event);
        if let Some(cursor) = result.cursor
            && let Some(w) = self.shell.window()
        {
            w.set_cursor(winit::window::Cursor::Icon(map_cursor(cursor)));
        }
        let had_msgs = !result.msgs.is_empty();
        for msg in result.msgs {
            self.apply(msg);
        }
        if result.redraw || had_msgs {
            self.dirty = true;
            if let Some(w) = self.shell.window() {
                w.request_redraw();
            }
        }
        had_msgs
    }

    /// Routes one input event into the main window, reconciling windows
    /// afterwards if it produced messages.
    fn input_main(&mut self, event_loop: &ActiveEventLoop, event: InputEvent) {
        if self.input(event) {
            self.after_update(event_loop);
        }
    }

    /// Routes one input event into a secondary window, reconciling
    /// windows afterwards if it produced messages.
    #[cfg(not(target_arch = "wasm32"))]
    fn secondary_input_main(&mut self, key: &str, event_loop: &ActiveEventLoop, event: InputEvent) {
        if self.secondary_input(key, event) {
            self.after_update(event_loop);
        }
    }

    /// NEOSONIA patch: 把 Win32 WM_DROPFILES 兜底收到的路径转成
    /// `InputEvent::FileDrop`，与 `WindowEvent::DroppedFile` 走同一条路径。
    #[cfg(windows)]
    fn pump_os_file_drops(&mut self, event_loop: &ActiveEventLoop) {
        let paths = crate::dragdrop::drain();
        if paths.is_empty() {
            return;
        }
        if crate::dragdrop::drop_log_enabled() {
            tracing::warn!(count = paths.len(), "OS 文件拖放：转交 InputEvent::FileDrop");
        }
        for path in paths {
            self.input_main(event_loop, InputEvent::FileDrop(path));
        }
    }

    /// Reconciles secondary windows against [`App::windows`] and asks
    /// every window for a repaint — called whenever messages were applied.
    fn after_update(&mut self, event_loop: &ActiveEventLoop) {
        self.dirty = true;
        #[cfg(not(target_arch = "wasm32"))]
        self.reconcile_subs();
        #[cfg(target_os = "macos")]
        self.reconcile_menu();
        #[cfg(not(target_arch = "wasm32"))]
        self.reconcile_windows(event_loop);
        #[cfg(target_arch = "wasm32")]
        let _ = event_loop;
        if let Some(w) = self.shell.window() {
            w.request_redraw();
        }
        #[cfg(not(target_arch = "wasm32"))]
        for bundle in self.secondary.values() {
            if let Some(w) = bundle.shell.window() {
                w.request_redraw();
            }
        }
    }

    /// Opens, closes, and retitles secondary windows to match the app's
    /// declared list.
    #[cfg(not(target_arch = "wasm32"))]
    fn reconcile_windows(&mut self, event_loop: &ActiveEventLoop) {
        let desired = self.app.windows();
        self.secondary
            .retain(|key, _| desired.iter().any(|d| &d.key == key));
        for desc in desired {
            match self.secondary.get_mut(&desc.key) {
                Some(bundle) => {
                    bundle.on_close = desc.on_close;
                    if bundle.title != desc.title {
                        bundle.title.clone_from(&desc.title);
                        if let Some(w) = bundle.shell.window() {
                            w.set_title(&desc.title);
                        }
                    }
                }
                None => {
                    let mut shell = WindowShell::new(
                        WindowOptions::titled(desc.title.clone())
                            .with_size(desc.size.0, desc.size.1),
                        self.shell.background,
                    );
                    let proxy = self.proxy.clone();
                    let mut adapter = None;
                    if let Err(e) = shell.resumed_with(event_loop, |el, window| {
                        adapter = Some(accesskit_winit::Adapter::with_event_loop_proxy(
                            el, window, proxy,
                        ));
                    }) {
                        // Failing to open a declared window is app-fatal: the
                        // GPU/window stack is broken, not just this window.
                        self.shell.fail(event_loop, e);
                        return;
                    }
                    if let Some(w) = shell.window() {
                        w.set_ime_allowed(true);
                        w.request_redraw();
                    }
                    let mut state = live_state();
                    state.set_clipboard(Box::new(crate::OsClipboard::default()));
                    self.secondary.insert(
                        desc.key.clone(),
                        SecondaryWindow {
                            shell,
                            state,
                            cursor: Point::ORIGIN,
                            last: None,
                            on_close: desc.on_close,
                            title: desc.title,
                            adapter,
                        },
                    );
                }
            }
        }
    }

    /// Redraws one secondary window: the same pipeline as the main one,
    /// against its own retained state and `view_for(key)`.
    #[cfg(not(target_arch = "wasm32"))]
    fn secondary_redraw(&mut self, key: &str, event_loop: &ActiveEventLoop) {
        let theme = self.app.theme_for(key);
        let now = self.started.elapsed().as_secs_f64();
        let Some(bundle) = self.secondary.get_mut(key) else {
            return;
        };
        if let Err(e) = bundle.shell.pump() {
            self.shell.fail(event_loop, e);
            return;
        }
        let Some((lw, lh, scale)) = bundle.shell.logical_size() else {
            return;
        };
        bundle.shell.background = theme.bg;
        bundle.state.tick(now);
        #[expect(clippy::cast_possible_truncation, reason = "window sizes fit in f32")]
        let logical = (lw as f32, lh as f32);
        let view = self.app.view_at(key, logical);
        let frame = build_frame(
            &view,
            &theme,
            &mut self.fonts,
            &mut bundle.state,
            logical,
            scale,
        );
        // Single-pass live window: glass is tint-only here (see the `run_app`
        // redraw note); two-pass blur is the headless golden path.
        let scene = frame.paint(&mut self.fonts, &mut bundle.state);
        if let Err(e) = bundle.shell.present(&scene) {
            // Secondary failures stop the app through the main shell's
            // channel: device-level errors are app-fatal, not per-window.
            self.shell.fail(event_loop, e);
            return;
        }
        if refresh_hover(&view, &frame, &mut bundle.state)
            && let Some(w) = bundle.shell.window()
        {
            w.request_redraw();
        }
        if frame.animating {
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(16),
            ));
        }
        if let Some(caret) = bundle.state.ime_caret()
            && let Some(w) = bundle.shell.window()
        {
            w.set_ime_cursor_area(
                winit::dpi::LogicalPosition::new(caret.x0, caret.y0),
                winit::dpi::LogicalSize::new(1.0, caret.height()),
            );
        }
        bundle.last = Some((view, frame));
        let focus = bundle.state.focused();
        if let Some(adapter) = &mut bundle.adapter
            && let Some((_, frame)) = &bundle.last
        {
            adapter.update_if_active(|| crate::access::tree_update(frame, focus, scale));
        }
    }

    /// The full event handler for one secondary window — the same arms as
    /// the main window, against the bundle's own surface and state.
    #[cfg(not(target_arch = "wasm32"))]
    fn secondary_window_event(
        &mut self,
        key: &str,
        event_loop: &ActiveEventLoop,
        event: WindowEvent,
    ) {
        #[cfg(windows)]
        self.pump_os_file_drops(event_loop);
        if let Some(bundle) = self.secondary.get_mut(key)
            && let Some(window) = bundle.shell.window()
            && let Some(adapter) = &mut bundle.adapter
        {
            adapter.process_event(window, &event);
        }
        match event {
            WindowEvent::CloseRequested => {
                if let Some(msg) = self.secondary.get(key).map(|b| b.on_close.clone()) {
                    self.apply(msg);
                    self.after_update(event_loop);
                }
            }
            WindowEvent::Resized(size) => {
                if let Some(bundle) = self.secondary.get_mut(key) {
                    bundle.shell.resized(size.width, size.height);
                }
            }
            WindowEvent::ScaleFactorChanged { .. } | WindowEvent::Occluded(false) => {
                if let Some(w) = self.secondary.get(key).and_then(|b| b.shell.window()) {
                    w.request_redraw();
                }
            }
            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
                let m = self.modifiers;
                self.secondary_input_main(
                    key,
                    event_loop,
                    InputEvent::Modifiers {
                        shift: m.shift_key(),
                        ctrl: m.control_key(),
                        alt: m.alt_key(),
                        meta: m.super_key(),
                    },
                );
            }
            WindowEvent::DroppedFile(path) => {
                self.secondary_input_main(key, event_loop, InputEvent::FileDrop(path));
            }
            WindowEvent::CursorLeft { .. } => {
                self.secondary_input_main(key, event_loop, InputEvent::PointerLeave);
            }
            WindowEvent::CursorMoved { position, .. } => {
                let Some(bundle) = self.secondary.get_mut(key) else {
                    return;
                };
                let scale = bundle.shell.window().map_or(1.0, |w| w.scale_factor());
                bundle.cursor = Point::new(position.x / scale, position.y / scale);
                #[expect(clippy::cast_possible_truncation, reason = "positions fit in f32")]
                let (x, y) = (bundle.cursor.x as f32, bundle.cursor.y as f32);
                self.secondary_input_main(key, event_loop, InputEvent::PointerMove { x, y });
            }
            WindowEvent::MouseInput {
                state,
                button: winit::event::MouseButton::Left,
                ..
            } => {
                self.secondary_input_main(
                    key,
                    event_loop,
                    match state {
                        winit::event::ElementState::Pressed => InputEvent::PointerDown,
                        winit::event::ElementState::Released => InputEvent::PointerUp,
                    },
                );
            }
            WindowEvent::MouseInput {
                state,
                button: winit::event::MouseButton::Right,
                ..
            } => {
                self.secondary_input_main(
                    key,
                    event_loop,
                    match state {
                        winit::event::ElementState::Pressed => InputEvent::RightDown,
                        winit::event::ElementState::Released => InputEvent::RightUp,
                    },
                );
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let scale = self
                    .secondary
                    .get(key)
                    .and_then(|b| b.shell.window())
                    .map_or(1.0, |w| w.scale_factor());
                let (dx, dy) = wheel_deltas(delta, scale);
                #[expect(clippy::cast_possible_truncation, reason = "deltas fit in f32")]
                self.secondary_input_main(
                    key,
                    event_loop,
                    InputEvent::Wheel {
                        dx: dx as f32,
                        dy: dy as f32,
                    },
                );
            }
            WindowEvent::KeyboardInput { event, .. }
                if event.state == winit::event::ElementState::Pressed =>
            {
                let mods = self.modifiers;
                let printable = !mods.control_key()
                    && !mods.super_key()
                    && event
                        .text
                        .as_ref()
                        .is_some_and(|t| !t.is_empty() && t.chars().all(|c| !c.is_control()));
                if printable {
                    if let Some(t) = &event.text {
                        self.secondary_input_main(key, event_loop, InputEvent::Text(t.to_string()));
                    }
                } else if let Some(input) = map_key(&event, mods) {
                    self.secondary_input_main(key, event_loop, input);
                }
            }
            WindowEvent::Ime(ime) => match ime {
                winit::event::Ime::Preedit(text, cursor) => {
                    self.secondary_input_main(
                        key,
                        event_loop,
                        InputEvent::ImePreedit { text, cursor },
                    );
                }
                winit::event::Ime::Commit(text) => {
                    self.secondary_input_main(key, event_loop, InputEvent::ImeCommit(text));
                }
                winit::event::Ime::Enabled | winit::event::Ime::Disabled => {}
            },
            WindowEvent::RedrawRequested => self.secondary_redraw(key, event_loop),
            _ => {}
        }
    }

    /// Dispatches one input event against a secondary window. Returns
    /// whether messages were applied (the caller then reconciles).
    #[cfg(not(target_arch = "wasm32"))]
    fn secondary_input(&mut self, key: &str, event: InputEvent) -> bool {
        let Some(bundle) = self.secondary.get_mut(key) else {
            return false;
        };
        let Some((view, frame)) = &bundle.last else {
            return false;
        };
        let result = dispatch(view, frame, &mut bundle.state, &mut self.fonts, event);
        if let Some(cursor) = result.cursor
            && let Some(w) = bundle.shell.window()
        {
            w.set_cursor(winit::window::Cursor::Icon(map_cursor(cursor)));
        }
        let had_msgs = !result.msgs.is_empty();
        if (result.redraw || had_msgs)
            && let Some(w) = bundle.shell.window()
        {
            w.request_redraw();
        }
        let msgs = result.msgs;
        for msg in msgs {
            self.apply(msg);
        }
        had_msgs
    }
}

pub(crate) fn map_cursor(cursor: fenestra_core::Cursor) -> winit::window::CursorIcon {
    match cursor {
        fenestra_core::Cursor::Default => winit::window::CursorIcon::Default,
        fenestra_core::Cursor::Pointer => winit::window::CursorIcon::Pointer,
        fenestra_core::Cursor::Text => winit::window::CursorIcon::Text,
        fenestra_core::Cursor::NotAllowed => winit::window::CursorIcon::NotAllowed,
    }
}

/// Translates a winit key event into a fenestra [`InputEvent`].
pub(crate) fn map_key(
    event: &winit::event::KeyEvent,
    mods: winit::keyboard::ModifiersState,
) -> Option<InputEvent> {
    use winit::keyboard::{Key as WKey, NamedKey};
    let key = match &event.logical_key {
        WKey::Named(NamedKey::Tab) => {
            return Some(if mods.shift_key() {
                InputEvent::ShiftTab
            } else {
                InputEvent::Tab
            });
        }
        WKey::Named(named) => match named {
            NamedKey::Enter => Key::Enter,
            NamedKey::Space => Key::Space,
            NamedKey::Escape => Key::Escape,
            NamedKey::ArrowLeft => Key::ArrowLeft,
            NamedKey::ArrowRight => Key::ArrowRight,
            NamedKey::ArrowUp => Key::ArrowUp,
            NamedKey::ArrowDown => Key::ArrowDown,
            NamedKey::Home => Key::Home,
            NamedKey::End => Key::End,
            NamedKey::Backspace => Key::Backspace,
            NamedKey::Delete => Key::Delete,
            NamedKey::PageUp => Key::PageUp,
            NamedKey::PageDown => Key::PageDown,
            _ => return None,
        },
        WKey::Character(s) => Key::Char(s.chars().next()?),
        _ => return None,
    };
    Some(InputEvent::Key(KeyInput {
        key,
        shift: mods.shift_key(),
        ctrl: mods.control_key(),
        alt: mods.alt_key(),
        meta: mods.super_key(),
    }))
}

impl<A: App> ApplicationHandler<RunnerEvent> for AppRunner<A> {
    #[cfg(not(target_arch = "wasm32"))]
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        // Fresh surface, possibly fresh scale: rebuild rather than trust
        // a scene cached across the suspend.
        self.dirty = true;
        let adapter = &mut self.adapter;
        let proxy = self.proxy.clone();
        if let Err(e) = self.shell.resumed_with(event_loop, |el, window| {
            // The adapter must attach while the window is still hidden.
            if adapter.is_none() {
                *adapter = Some(accesskit_winit::Adapter::with_event_loop_proxy(
                    el, window, proxy,
                ));
            }
        }) {
            self.shell.fail(event_loop, e);
            return;
        }
        if let Some(w) = self.shell.window() {
            w.set_ime_allowed(true);
        }
        for bundle in self.secondary.values_mut() {
            if let Err(e) = bundle.shell.resumed(event_loop) {
                self.shell.fail(event_loop, e);
                return;
            }
        }
        #[cfg(windows)]
        {
            let proxy = self.proxy.clone();
            crate::dragdrop::set_waker(Box::new(move || {
                let _ = proxy.send_event(RunnerEvent::DropsPending);
            }));
        }
        self.reconcile_windows(event_loop);
    }

    #[cfg(target_arch = "wasm32")]
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.dirty = true;
        if let Err(e) = self.shell.resumed(event_loop) {
            self.shell.fail(event_loop, e);
            return;
        }
        if let Some(w) = self.shell.window() {
            w.set_ime_allowed(true);
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: RunnerEvent) {
        match event {
            #[cfg(windows)]
            RunnerEvent::DropsPending => self.pump_os_file_drops(event_loop),
            RunnerEvent::App(msg) => {
                if let Ok(msg) = msg.downcast::<A::Msg>() {
                    self.apply(*msg);
                    self.after_update(event_loop);
                }
            }
            #[cfg(target_os = "macos")]
            RunnerEvent::Menu(id) => {
                if let Some(msg) = self.menu.as_ref().and_then(|m| m.actions.get(&id)).cloned() {
                    self.apply(msg);
                    self.after_update(event_loop);
                }
            }
            #[cfg(not(target_arch = "wasm32"))]
            RunnerEvent::Access(ev) => {
                // Route by window id: `None` is the main window, `Some(key)`
                // a secondary one; unknown ids are stale events.
                let is_main = self.shell.window().is_some_and(|w| w.id() == ev.window_id);
                let skey = (!is_main)
                    .then(|| {
                        self.secondary
                            .iter()
                            .find(|(_, b)| b.shell.window().is_some_and(|w| w.id() == ev.window_id))
                            .map(|(k, _)| k.clone())
                    })
                    .flatten();
                if !is_main && skey.is_none() {
                    return;
                }
                match ev.window_event {
                    accesskit_winit::WindowEvent::InitialTreeRequested => match &skey {
                        None => {
                            if self.last.is_some() {
                                self.push_access_tree();
                            } else if let Some(w) = self.shell.window() {
                                w.request_redraw();
                            }
                        }
                        Some(key) => {
                            if let Some(bundle) = self.secondary.get_mut(key) {
                                let scale = bundle.shell.window().map_or(1.0, |w| w.scale_factor());
                                let focus = bundle.state.focused();
                                if let Some((_, frame)) = &bundle.last {
                                    if let Some(adapter) = &mut bundle.adapter {
                                        adapter.update_if_active(|| {
                                            crate::access::tree_update(frame, focus, scale)
                                        });
                                    }
                                } else if let Some(w) = bundle.shell.window() {
                                    w.request_redraw();
                                }
                            }
                        }
                    },
                    accesskit_winit::WindowEvent::ActionRequested(req) => {
                        let id = fenestra_core::WidgetId(req.target_node.0);
                        match req.action {
                            accesskit::Action::Click => {
                                let msg = match &skey {
                                    None => self.last.as_ref().and_then(|(view, frame)| {
                                        fenestra_core::click_msg_of(view, frame, &self.state, id)
                                    }),
                                    Some(key) => self.secondary.get(key).and_then(|bundle| {
                                        bundle.last.as_ref().and_then(|(view, frame)| {
                                            fenestra_core::click_msg_of(
                                                view,
                                                frame,
                                                &bundle.state,
                                                id,
                                            )
                                        })
                                    }),
                                };
                                if let Some(msg) = msg {
                                    self.apply(msg);
                                    self.after_update(event_loop);
                                }
                            }
                            accesskit::Action::Focus => match &skey {
                                None => {
                                    self.state.set_focus(Some(id));
                                    self.dirty = true;
                                    if let Some(w) = self.shell.window() {
                                        w.request_redraw();
                                    }
                                }
                                Some(key) => {
                                    if let Some(bundle) = self.secondary.get_mut(key) {
                                        bundle.state.set_focus(Some(id));
                                        if let Some(w) = bundle.shell.window() {
                                            w.request_redraw();
                                        }
                                    }
                                }
                            },
                            _ => {}
                        }
                    }
                    accesskit_winit::WindowEvent::AccessibilityDeactivated => {}
                }
            }
        }
    }

    fn suspended(&mut self, _event_loop: &ActiveEventLoop) {
        self.shell.suspended();
        #[cfg(not(target_arch = "wasm32"))]
        for bundle in self.secondary.values_mut() {
            bundle.shell.suspended();
        }
    }

    fn new_events(&mut self, _event_loop: &ActiveEventLoop, cause: StartCause) {
        if !matches!(cause, StartCause::ResumeTimeReached { .. }) {
            return;
        }
        if let Some(w) = self.shell.window() {
            w.request_redraw();
        }
        #[cfg(not(target_arch = "wasm32"))]
        for bundle in self.secondary.values() {
            if let Some(w) = bundle.shell.window() {
                w.request_redraw();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        #[cfg(windows)]
        self.pump_os_file_drops(event_loop);
        if self.shell.window().is_none_or(|w| w.id() != window_id) {
            #[cfg(not(target_arch = "wasm32"))]
            if let Some(key) = self
                .secondary
                .iter()
                .find(|(_, b)| b.shell.window().is_some_and(|w| w.id() == window_id))
                .map(|(k, _)| k.clone())
            {
                self.secondary_window_event(&key, event_loop, event);
            }
            return;
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(adapter) = &mut self.adapter
            && let Some(window) = self.shell.window()
        {
            adapter.process_event(window, &event);
        }
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                // The cache key also guards size, but coalesced resizes can
                // land back on the cached geometry mid-drag.
                self.dirty = true;
                self.shell.resized(size.width, size.height);
            }
            WindowEvent::ScaleFactorChanged { .. } => {
                self.dirty = true;
                if let Some(w) = self.shell.window() {
                    w.request_redraw();
                }
            }
            WindowEvent::Focused(true) => {
                // Re-read the OS "reduce motion" setting on focus, so a change
                // made while the app was in the background takes effect.
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let reduce = crate::reduce_motion::os_reduce_motion();
                    if reduce != self.state.reduced_motion {
                        self.state.reduced_motion = reduce;
                        self.dirty = true;
                        if let Some(w) = self.shell.window() {
                            w.request_redraw();
                        }
                    }
                }
            }
            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
                let m = self.modifiers;
                self.input_main(
                    event_loop,
                    InputEvent::Modifiers {
                        shift: m.shift_key(),
                        ctrl: m.control_key(),
                        alt: m.alt_key(),
                        meta: m.super_key(),
                    },
                );
            }
            WindowEvent::Occluded(occluded) => {
                if !occluded && let Some(w) = self.shell.window() {
                    w.request_redraw();
                }
            }
            WindowEvent::DroppedFile(path) => {
                self.input_main(event_loop, InputEvent::FileDrop(path))
            }
            WindowEvent::CursorLeft { .. } => self.input_main(event_loop, InputEvent::PointerLeave),
            WindowEvent::CursorMoved { position, .. } => {
                let scale = self.shell.window().map_or(1.0, |w| w.scale_factor());
                self.cursor = Point::new(position.x / scale, position.y / scale);
                #[expect(clippy::cast_possible_truncation, reason = "positions fit in f32")]
                self.input_main(
                    event_loop,
                    InputEvent::PointerMove {
                        x: self.cursor.x as f32,
                        y: self.cursor.y as f32,
                    },
                );
            }
            WindowEvent::MouseInput {
                state,
                button: winit::event::MouseButton::Left,
                ..
            } => {
                self.input_main(
                    event_loop,
                    match state {
                        winit::event::ElementState::Pressed => InputEvent::PointerDown,
                        winit::event::ElementState::Released => InputEvent::PointerUp,
                    },
                );
            }
            WindowEvent::MouseInput {
                state,
                button: winit::event::MouseButton::Right,
                ..
            } => {
                self.input_main(
                    event_loop,
                    match state {
                        winit::event::ElementState::Pressed => InputEvent::RightDown,
                        winit::event::ElementState::Released => InputEvent::RightUp,
                    },
                );
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let scale = self.shell.window().map_or(1.0, |w| w.scale_factor());
                let (dx, dy) = wheel_deltas(delta, scale);
                #[expect(clippy::cast_possible_truncation, reason = "deltas fit in f32")]
                self.input_main(
                    event_loop,
                    InputEvent::Wheel {
                        dx: dx as f32,
                        dy: dy as f32,
                    },
                );
            }
            WindowEvent::KeyboardInput { event, .. }
                if event.state == winit::event::ElementState::Pressed =>
            {
                {
                    let mods = self.modifiers;
                    // Printable input arrives as Text (it may be multi-char);
                    // named keys and shortcuts go through Key.
                    let printable = !mods.control_key()
                        && !mods.super_key()
                        && event
                            .text
                            .as_ref()
                            .is_some_and(|t| !t.is_empty() && t.chars().all(|c| !c.is_control()));
                    if printable {
                        if let Some(t) = &event.text {
                            self.input_main(event_loop, InputEvent::Text(t.to_string()));
                        }
                    } else if let Some(input) = map_key(&event, mods) {
                        self.input_main(event_loop, input);
                    }
                }
            }
            WindowEvent::Ime(ime) => match ime {
                winit::event::Ime::Preedit(text, cursor) => {
                    self.input_main(event_loop, InputEvent::ImePreedit { text, cursor });
                }
                winit::event::Ime::Commit(text) => {
                    self.input_main(event_loop, InputEvent::ImeCommit(text));
                }
                winit::event::Ime::Enabled | winit::event::Ime::Disabled => {}
            },
            WindowEvent::RedrawRequested => self.redraw(event_loop),
            _ => {}
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::dispatch_effect_unit;
    use fenestra_core::{CmdUnit, Proxy};
    use std::sync::{Arc, Mutex};

    /// A worker thread that cannot be spawned used to mean a dropped
    /// message: the app waited forever for a reply that was never coming.
    /// The effect must still run, and its message must still arrive.
    #[test]
    fn a_failed_spawn_runs_the_effect_instead_of_losing_it() {
        let got: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&got);
        let proxy = Proxy::new(move |m: u32| sink.lock().expect("lock").push(m));
        dispatch_effect_unit(CmdUnit::Task(Box::new(|| 7)), &proxy, |_| {
            Err(std::io::Error::other("no threads today"))
        });
        assert_eq!(
            *got.lock().expect("lock"),
            vec![7],
            "the effect must be delivered even when no worker thread is available"
        );
    }

    /// The unit lives in the shared slot rather than inside the closure,
    /// so a spawner that swallows the closure entirely before failing
    /// still leaves the effect reclaimable. However the spawn fails, the
    /// message arrives.
    #[test]
    fn a_spawner_that_eats_the_closure_still_delivers() {
        let got: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&got);
        let proxy = Proxy::new(move |m: u32| sink.lock().expect("lock").push(m));
        dispatch_effect_unit(CmdUnit::Task(Box::new(|| 7)), &proxy, |work| {
            drop(work);
            Err(std::io::Error::other("no threads today"))
        });
        assert_eq!(
            *got.lock().expect("lock"),
            vec![7],
            "the effect is reclaimed from the slot, not from the dropped closure"
        );
    }

    /// The ordinary path still hands the unit to the spawner, and does not
    /// also run it on the caller's thread.
    #[test]
    fn a_successful_spawn_runs_the_effect_once() {
        let got: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&got);
        let proxy = Proxy::new(move |m: u32| sink.lock().expect("lock").push(m));
        dispatch_effect_unit(CmdUnit::Task(Box::new(|| 7)), &proxy, |work| {
            work();
            Ok(())
        });
        assert_eq!(
            *got.lock().expect("lock"),
            vec![7],
            "delivered exactly once"
        );
    }
}