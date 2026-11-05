# NEOSONIA patches to fenestra-shell 0.41.0

Upstream crate: `fenestra-shell` 0.41.0 (MIT OR Apache-2.0).
This directory is a verbatim copy of the crates.io source plus the changes
below. `version = "0.41.0"` is deliberately unchanged so the root
`[patch.crates-io]` entry keeps resolving the same semver requirement, and
the public API of `run_app`, `run_scene`, `run_static`,
`WindowOptions::titled`, and every existing field/method is untouched.

Only `src/window.rs` was modified. No upstream test files were edited.

## 1. New `WindowOptions` fields (`src/window.rs`)

Added after the existing `fonts` field, in the struct definition:

```rust
pub always_on_top: bool,
pub decorations: bool,
pub transparent: bool,
pub position: Option<(f64, f64)>,
pub skip_taskbar: bool,
```

Why: fenestra 0.41.0 exposes only title/size/min-size/resizable/maximized/
fullscreen/icon/fonts. NeoSonia needs a floating always-on-top assistant
window, a frameless/transparent shell, deterministic initial placement, and
the option to keep the window out of the taskbar.

Defaults set in `WindowOptions::titled`:

- `always_on_top: false` - preserve upstream behavior.
- `decorations: true` - winit's default; existing apps look identical.
- `transparent: false` - opaque swapchain and opaque clear.
- `position: None` - let the OS place the window as before.
- `skip_taskbar: false` - taskbar button shown as before.

## 2. New builder methods (`src/window.rs`, `impl WindowOptions`)

Added after `with_font`:

```rust
pub fn with_always_on_top(self, always_on_top: bool) -> Self
pub fn with_decorations(self, decorations: bool) -> Self
pub fn with_transparent(self, transparent: bool) -> Self
pub fn with_position(self, x: f64, y: f64) -> Self
pub fn with_skip_taskbar(self, skip_taskbar: bool) -> Self
```

All are consuming builders, matching the existing `with_*` style.

`with_position` takes **physical** pixels. On Windows winit interprets this
as the outer (title bar) top-left corner. Callers holding logical
coordinates must multiply by the monitor scale factor first.

## 3. Window attributes (`WindowShell::resumed_with`, `src/window.rs`)

In the `Window::default_attributes()` chain:

```rust
.with_decorations(self.options.decorations)
.with_transparent(self.options.transparent)
```

and, when `always_on_top` is set:

```rust
.with_window_level(winit::window::WindowLevel::AlwaysOnTop)
```

and, when `position` is set:

```rust
let position = PhysicalPosition::new(x.round() as i32, y.round() as i32);
attrs.with_position(Position::Physical(position))
```

`Position` and `PhysicalPosition` were added to the existing `winit::dpi`
import. winit 0.30's `Position::Physical` holds integer physical pixels, so
the f64 builder arguments are rounded to the nearest integer.

Windows-only taskbar handling, placed next to the existing platform block:

```rust
#[cfg(windows)]
let attrs = {
    use winit::platform::windows::WindowAttributesExtWindows;
    attrs.with_skip_taskbar(self.options.skip_taskbar)
};
```

Why: `WindowLevel::AlwaysOnTop` is the cross-platform winit way to request
topmost z-order; on Windows winit translates it to the topmost window state
(no direct `SetWindowPos` call is needed). `skip_taskbar` has no
cross-platform winit API, so it is gated to Windows with
`WindowAttributesExtWindows::with_skip_taskbar`; the field/builder still
exist everywhere and are simply ignored on other platforms.

## 4. Transparent swapchain (`WindowShell::activate`, `src/window.rs`)

`create_surface` is now bound as `mut surface`. After the surface exists,
when `transparent` is requested:

1. Read the surface capabilities from the adapter.
2. Choose `CompositeAlphaMode::PreMultiplied` if supported.
3. Otherwise choose `CompositeAlphaMode::PostMultiplied`.
4. Otherwise fall back to the driver's first advertised alpha mode (or
   `Auto` if the list is empty).
5. Write the choice into `surface.config.alpha_mode` and call
   `RenderContext::configure_surface` again.

Why: vello 0.9's `RenderContext::create_render_surface` hard-codes
`alpha_mode: CompositeAlphaMode::Auto`. On Windows/DX12 `Auto` may resolve
to `Opaque`, so the compositor treats every presented pixel as opaque even
when the app clears to alpha 0. Pre-multiplied is preferred because vello's
render target already stores premultiplied alpha; post-multiplied is the
widely available fallback. If the driver supports neither, the window still
opens, but Windows may composite it opaquely - this is a platform
capability limit, not a configuration error.

## 5. Transparent clear color (`WindowShell::present`, `src/window.rs`)

The `RenderParams.base_color` passed to `render_to_texture` is now:

```rust
let base_color = if self.options.transparent {
    self.background.with_alpha(0.0)
} else {
    self.background
};
```

Why: the runner uses the theme background as vello's clear color. With an
alpha-capable swapchain, clearing to the opaque theme background would still
produce a fully opaque window. Forcing alpha 0 makes unpainted areas truly
transparent. Content that wants an opaque or translucent background can
still paint it itself (for example `.bg(theme.bg)` or
`.bg(theme.bg.with_alpha(0.85))`); the shell only controls the clear.

This applies to all three runners (`run_app`, `run_scene`, `run_static`)
because they share `WindowShell::present`. `run_scene` callers that pass an
explicit background color will see its alpha forced to 0 when `transparent`
is set; paint the desired background inside the callback in that case.

## Notes / limitations

- `transparent` is honored by the native runner. The wasm path's async
  surface creation is unchanged; browser transparency still depends on the
  canvas/WebGPU surface configuration.
- Transparency requires OS compositor support. On Windows 11 it works with
  DWM composition; combined with `with_decorations(false)` it produces the
  usual frameless translucent window. If the driver only advertises
  `PostMultiplied`, translucency still composites, but edge colors can differ
  slightly because vello's internal target stores premultiplied alpha.
- `skip_taskbar` is a Windows-only attribute. On macOS/Linux it is a no-op.
- `WindowLevel::AlwaysOnTop` is supported on Windows, macOS and X11; winit
  marks it unsupported on Wayland and the web, where the hint is ignored.
- No upstream behavior changes when the new fields keep their defaults.
