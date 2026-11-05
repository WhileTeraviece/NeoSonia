# fenestra 0.41.0 API 手册（从源码核对）

> 适用版本：`fenestra` / `fenestra-core` / `fenestra-kit` / `fenestra-shell` / `fenestra-markdown` / `fenestra-looks` 均为 **0.41.0**。
> 本文所有签名、字段、枚举变体都从 0.41.0 源码逐条读出，不依赖记忆。凡源码中不存在或无法确认的 API，会明确写“不存在”或“未确认”。
>
> 源码位置：
> - facade 与 shell：`C:\Users\example\.cargo\registry\src\mirrors.tuna.tsinghua.edu.cn-4dc01642fd091eda\fenestra-0.41.0`、`...\fenestra-shell-0.41.0`
> - core / kit / markdown / looks：本机 registry 当时尚未解出这几个目录，已从 crates.io 下载同版本 `.crate` 并解压到 `<_probe>\fenestra-src\fenestra-{core,kit,markdown,looks}-0.41.0` 后核对。
>
> 路径说明：`E:\NeoSonia\vendor\fenestra-shell` 是本项目打过补丁的 shell（见文末“本项目 patch”一节）；下文凡标注“本项目 patch”的字段在上游 0.41.0 中不存在。

## 目录

1. [App trait 与生命周期](#1-app-trait-与生命周期)
2. [run / run_app / WindowOptions](#2-run--run_app--windowoptions)
3. [Element 构建器](#3-element-构建器)
4. [设计令牌与 Theme](#4-设计令牌与-theme)
5. [组件清单](#5-组件清单)
6. [事件与输入](#6-事件与输入)
7. [异步、Proxy、Cmd、Sub](#7-异步proxycmdsub)
8. [fenestra-markdown](#8-fenestra-markdown)
9. [image_rgba8](#9-image_rgba8)
10. [headless 渲染与截图回归](#10-headless-渲染与截图回归)
11. [坑列表](#11-坑列表)
12. [本项目 vendored shell patch](#12-本项目-vendored-shell-patch)

---

## 1. App trait 与生命周期

源码：`fenestra-core-0.41.0/src/app.rs`。

### 1.1 完整定义（逐字核对）

```rust
pub trait App {
    type Msg: Clone + 'static;

    fn init(&mut self, proxy: Proxy<Self::Msg>) {}

    fn update(&mut self, msg: Self::Msg);

    fn update_with(&mut self, msg: Self::Msg) -> Cmd<Self::Msg> {
        self.update(msg);
        Cmd::none()
    }

    fn init_cmd(&mut self) -> Cmd<Self::Msg> {
        Cmd::none()
    }

    fn subscriptions(&self) -> Vec<Sub<Self::Msg>> {
        Vec::new()
    }

    fn menu(&self) -> Option<MenuSpec<Self::Msg>> {
        None
    }

    fn view(&self) -> Element<Self::Msg>;

    fn theme(&self) -> Theme {
        Theme::light()
    }

    fn windows(&self) -> Vec<WindowDesc<Self::Msg>> {
        Vec::new()
    }

    fn view_for(&self, key: &str) -> Element<Self::Msg> {
        let _ = key;
        self.view()
    }

    fn view_at(&self, key: &str, size: (f32, f32)) -> Element<Self::Msg> {
        let _ = size;
        self.view_for(key)
    }

    fn theme_for(&self, key: &str) -> Theme {
        let _ = key;
        self.theme()
    }
}
```

要点：

- `Msg` 约束是 **`Clone + 'static`**，不是 `Send`。`run` / `run_app` 额外要求 `A::Msg: Send`，因为 `Proxy` 会跨线程投递。
- 没有 `on_key` / `on_global_key` / `on_window_event` 钩子。全局按键只能通过 `Element::on_key`（焦点元素）或原生菜单加速键实现，详见第 6 节。
- `init` 默认空实现。runner 在**第一帧之前**调用一次，传入可克隆的 `Proxy<Msg>`。
- runner 实际调用 `update_with`，不是直接调用 `update`。只实现 `update` 的普通 app 仍能工作；要返回效果时覆盖 `update_with`，并让 `update` 留空（或仍保留原逻辑，默认 `update_with` 会转调）。
- `init_cmd` 在 `init` 之后、事件循环启动前取一次，作为启动效果。
- `subscriptions` 按 key 对账：新 key 启动，消失的 key 停止，同一 key 改 period 会重启。**native only**；web runner 忽略。
- `windows` / `view_for` / `view_at` / `theme_for` 支持多窗口；**secondary windows 是 native only**。
- `menu` 只在 macOS 接原生菜单栏；其他平台用 kit 的 `menubar`。
- `view` 每帧重建整棵树，无 diff、无宏。

### 1.2 最小可编译 Counter

```rust
use fenestra::prelude::*;

struct Counter { n: i64 }

#[derive(Clone)]
enum Msg { Inc, Dec }

impl App for Counter {
    type Msg = Msg;

    fn update(&mut self, msg: Msg) {
        match msg {
            Msg::Inc => self.n += 1,
            Msg::Dec => self.n -= 1,
        }
    }

    fn view(&self) -> Element<Msg> {
        col().p(SP6).gap(SP4).items_center().children((
            text(self.n.to_string()).size(TextSize::Xl2).weight(Weight::Semibold),
            row().gap(SP3).children((
                button("Decrement").variant(ButtonVariant::Secondary).on_click(Msg::Dec),
                button("Increment").on_click(Msg::Inc),
            )),
        ))
    }
}

fn main() {
    fenestra::run(Counter { n: 0 }, WindowOptions::titled("Counter"));
}
```

### 1.3 `init` 里怎么用 `Proxy`

`Proxy<Msg>` 的完整 API（`fenestra-core-0.41.0/src/proxy.rs`）：

```rust
pub struct Proxy<Msg> { /* Arc<dyn Fn(Msg) + Send + Sync> */ }

impl<Msg> Clone for Proxy<Msg> { /* 可克隆、便宜 */ }

impl<Msg> Proxy<Msg> {
    pub fn new(send: impl Fn(Msg) + Send + Sync + 'static) -> Self;
    pub fn send(&self, msg: Msg);
}
```

app 侧只需要 `send`。典型写法是存进 state，再克隆进线程：

```rust
use std::time::Duration;
use fenestra::prelude::*;

struct Clock { seconds: u64, proxy: Option<Proxy<Msg>> }

#[derive(Clone)]
enum Msg { Tick }

impl App for Clock {
    type Msg = Msg;

    fn init(&mut self, proxy: Proxy<Msg>) {
        self.proxy = Some(proxy.clone());
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(1));
            proxy.send(Msg::Tick);
        });
    }

    fn update(&mut self, msg: Msg) {
        match msg {
            Msg::Tick => self.seconds += 1,
        }
    }

    fn view(&self) -> Element<Msg> {
        text(self.seconds.to_string()).size(TextSize::Xl2).into()
    }
}

fn main() {
    fenestra::run(
        Clock { seconds: 0, proxy: None },
        WindowOptions::titled("clock").with_size(320.0, 160.0),
    );
}
```

更推荐用 `Sub::every` 做周期任务，不必手写线程（见 7.3）。

---

## 2. run / run_app / WindowOptions

源码：`fenestra-0.41.0/src/lib.rs`、`fenestra-shell-0.41.0/src/window.rs`。

### 2.1 facade 签名

```rust
pub fn run<A: App + 'static>(app: A, options: WindowOptions)
where
    A::Msg: Send;
```

`run` 内部就是 `fenestra_shell::run_app(app, options).expect("fenestra event loop failed")`，所以事件循环/GPU 初始化失败会 **panic**。需要错误处理时直接调 `fenestra_shell::run_app`：

```rust
pub fn run_app<A: App + 'static>(
    mut app: A,
    options: WindowOptions,
) -> Result<(), fenestra_shell::ShellError>
where
    A::Msg: Send;
```

`run()` 与 `run_app` 都是阻塞直到窗口关闭。web 上 `run_app` 非阻塞。

prelude 里没有 `run`，要写 `fenestra::run(...)`；`WindowOptions` 在 prelude 中。

### 2.2 `WindowOptions` 全部字段

上游 0.41.0 + 本项目 patch（patch 字段已标注）。`fenestra::WindowOptions` 就是 `fenestra_shell::WindowOptions` 的再导出，所以根 `Cargo.toml` 的 `[patch.crates-io]` 生效后，下面这些新 builder 可以直接通过 `fenestra::prelude::*` 使用。

```rust
#[derive(Debug, Clone)]
pub struct WindowOptions {
    pub title: String,
    pub inner_size: (f64, f64),                  // logical px，默认 1024x768
    pub min_size: Option<(f64, f64)>,            // logical px
    pub resizable: bool,                         // 默认 true
    pub maximized: bool,                         // 默认 false
    pub fullscreen: bool,                        // borderless fullscreen
    pub icon: Option<(u32, u32, Vec<u8>)>,       // straight-alpha RGBA8
    pub fonts: Vec<(FamilyRole, Vec<u8>)>,       // 启动前注册的字体

    // ---- 本项目 vendored patch 新增 ----
    pub always_on_top: bool,                     // 默认 false
    pub decorations: bool,                       // 默认 true
    pub transparent: bool,                       // 默认 false
    pub position: Option<(f64, f64)>,            // 物理像素，outer 左上角（内部四舍五入到整数像素）
    pub skip_taskbar: bool,                      // 默认 false，Windows only
}
```

构造与上游 builder：

```rust
WindowOptions::titled("Title")        // 1024x768，resizable
    .with_size(1280.0, 800.0)         // 逻辑像素
    .with_min_size(640.0, 480.0)
    .with_resizable(true)
    .maximized()
    .fullscreen()
    .with_icon(w, h, rgba_vec)
    .with_font(FamilyRole::Display, ttf_bytes);
```

本项目 patch builder：

```rust
WindowOptions::titled("NeoSonia")
    .with_size(1100.0, 720.0)
    .with_always_on_top(true)
    .with_decorations(false)
    .with_transparent(true)
    .with_position(120.0, 80.0)   // 物理像素
    .with_skip_taskbar(true)
```

最小可编译启动：

```rust
use fenestra::prelude::*;

struct App0;

impl App for App0 {
    type Msg = ();
    fn update(&mut self, _: ()) {}
    fn view(&self) -> Element<()> {
        col().w_full().h_full().bg(Theme::light().bg).children((
            text("Hello").size(TextSize::Xl2),
        ))
    }
}

fn main() {
    fenestra::run(
        App0,
        WindowOptions::titled("Hello").with_size(640.0, 400.0),
    );
}
```

### 2.3 `run_scene` / `run_static`

`run_scene` 是低层逃生口，直接给 vello `Scene`：

```rust
pub fn run_scene(
    options: WindowOptions,
    background: vello::peniko::Color,
    paint: impl FnMut(&mut vello::Scene, f64, f64, vello::peniko::Color) + 'static,
) -> Result<(), ShellError>;
```

`run_static` 是无消息的元素视图（自带滚动/动画帧）：

```rust
pub fn run_static(
    options: WindowOptions,
    theme: Theme,
    view: impl Fn(&Theme) -> Element<()> + 'static,
) -> Result<(), ShellError>;
```

示例：

```rust
use fenestra::prelude::*;
use fenestra::shell::{WindowOptions, run_static};

fn main() {
    let theme = Theme::dark();
    run_static(
        WindowOptions::titled("static").with_size(480.0, 320.0),
        theme,
        |t| col().p(SP6).gap(SP2).children((
            text("Static view").size(TextSize::Lg).color(t.text),
            divider(),
        )),
    ).expect("event loop failed");
}
```

---

## 3. Element 构建器

源码：`fenestra-core-0.41.0/src/element.rs`、`style.rs`、`children.rs`。

### 3.1 所有核心构造函数

```rust
pub fn div<Msg>() -> Element<Msg>;                 // 普通盒，默认 flex row
pub fn row<Msg>() -> Element<Msg>;                 // flex row
pub fn col<Msg>() -> Element<Msg>;                 // flex column
pub fn stack<Msg>() -> Element<Msg>;               // z-stack，children 同格
pub fn spacer<Msg>() -> Element<Msg>;              // grow(1) 的空白
pub fn divider<Msg>() -> Element<Msg>;             // 1px 横线，border_subtle
pub fn text<Msg>(content: impl Into<String>) -> Element<Msg>;
pub fn rich_text<Msg>(spans: impl IntoIterator<Item = Span>) -> Element<Msg>;
pub fn span(text: impl Into<String>) -> Span;
pub fn responsive<Msg>(f: impl Fn((f32, f32)) -> Element<Msg> + 'static) -> Element<Msg>;
pub fn responsive_hinted<Msg>(
    hint: (f32, f32),
    f: impl Fn((f32, f32)) -> Element<Msg> + 'static,
) -> Element<Msg>;
pub fn raw_input<Msg>(value: impl Into<String>, placeholder: impl Into<String>) -> Element<Msg>;
pub fn raw_text_area<Msg>(value: impl Into<String>, placeholder: impl Into<String>) -> Element<Msg>;
pub fn image_rgba8<Msg>(width: u32, height: u32, pixels: Vec<u8>) -> Element<Msg>;
pub fn image_from_data<Msg>(data: ImageData) -> Element<Msg>;
pub fn image_payload(width: u32, height: u32, pixels: Vec<u8>) -> ImageData;
pub fn path<Msg>(bez: kurbo::BezPath, viewbox: (f64, f64), stroke: Option<f64>) -> Element<Msg>;
```

`rich_text` 用于一段内联混排，`span(...)` 可链 `.weight/.color/.size_px/.family/.italic`：

```rust
use fenestra::prelude::*;

fn rich() -> Element<()> {
    rich_text([
        span("Hello ").weight(Weight::Semibold),
        span("world").color(Theme::light().accent),
        span("!").italic(),
    ])
    .size(TextSize::Lg)
}
```

### 3.2 链式方法真实签名

以下均来自 `impl<Msg> Element<Msg>`；`Style` 方法同名，Element 上都有转发。

#### 尺寸、间距、布局

```rust
pub fn p(self, v: f32) -> Self;
pub fn px(self, v: f32) -> Self;
pub fn py(self, v: f32) -> Self;
pub fn pt/pr/pb/pl(self, v: f32) -> Self;
pub fn m/mx/my/mt/mr/mb/ml(self, v: f32) -> Self;
pub fn gap(self, v: f32) -> Self;

pub fn w(self, v: impl Into<Length>) -> Self;
pub fn h(self, v: impl Into<Length>) -> Self;
pub fn min_w/max_w/min_h/max_h(self, v: impl Into<Length>) -> Self;
pub fn w_full(self) -> Self;
pub fn h_full(self) -> Self;
pub fn w_ch(self, chars: f32) -> Self;
pub fn min_w_ch/max_w_ch(self, chars: f32) -> Self;
pub fn measure(self, chars: f32) -> Self;

pub fn grow(self) -> Self;
pub fn grow_by(self, factor: f32) -> Self;
pub fn shrink0(self) -> Self;
pub fn shrink(self, factor: f32) -> Self;

pub fn items_start(self) -> Self;
pub fn items_center(self) -> Self;
pub fn items_end(self) -> Self;
pub fn items_baseline(self) -> Self;
pub fn self_start/self_center/self_end/self_stretch(self) -> Self;
pub fn justify_start/justify_center/justify_end/justify_between(self) -> Self;
pub fn wrap(self) -> Self;
pub fn absolute(self) -> Self;
pub fn top/right/bottom/left(self, v: f32) -> Self;
pub fn overflow_hidden(self) -> Self;
pub fn scroll_y(self) -> Self;
pub fn scroll_x(self) -> Self;
pub fn scroll_xy(self) -> Self;
pub fn sticky_top(self, offset: f32) -> Self;
pub fn sticky_bottom/left/right(self, offset: f32) -> Self;
pub fn stick_to_bottom(self) -> Self;
```

`Length`：

```rust
pub enum Length {
    Px(f32),
    Pct(f32),   // 0.0..=100.0
    Ch(f32),
    Auto,       // default
}
// impl From<f32> for Length => Px
```

所以 `.w(320.0)`、`.w(Length::Pct(100.0))`、`.w_full()` 都合法。

#### 背景、边框、圆角、阴影

```rust
pub fn bg(self, paint: impl Into<Paint>) -> Self;
pub fn border(self, width: f32, color: Color) -> Self;
pub fn border_top/right/bottom/left(self, width: f32, color: Color) -> Self;
pub fn ring(self, width: f32, color: Color) -> Self;
pub fn rounded(self, r: f32) -> Self;
pub fn rounded_full(self) -> Self;
pub fn rounded_t/rounded_b/rounded_l/rounded_r(self, r: f32) -> Self;
pub fn corners(self, tl: f32, tr: f32, br: f32, bl: f32) -> Self;
pub fn corner_smoothing(self, s: f32) -> Self;
pub fn shadow(self, token: ShadowToken) -> Self;

pub fn opacity(self, v: f32) -> Self;
pub fn trim(self, v: f32) -> Self;
pub fn backdrop_blur(self, radius: f32) -> Self;
pub fn element_filter(self, filter: ElementFilter) -> Self;
pub fn specular_edge(self, edge: SpecularEdge) -> Self;
pub fn sheen(self, sheen: Sheen) -> Self;
pub fn adaptive_tint(self, adaptive: AdaptiveTint) -> Self;
pub fn surface(self, role: Surface) -> Self;
```

**没有 `.fg()`**。文字颜色是 `.color(Color)`；背景是 `.bg(...)`。

#### 文字

```rust
pub fn size(self, size: TextSize) -> Self;
pub fn size_px(self, px: f32) -> Self;
pub fn tracking(self, em: f32) -> Self;
pub fn leading(self, multiple: f32) -> Self;
pub fn tabular(self) -> Self;
pub fn proportional_nums(self) -> Self;
pub fn oldstyle_nums(self) -> Self;
pub fn lining_nums(self) -> Self;
pub fn small_caps(self) -> Self;
pub fn ligatures(self, on: bool) -> Self;
pub fn fractions(self) -> Self;
pub fn family(self, family: FamilyRole) -> Self;
pub fn weight(self, weight: Weight) -> Self;
pub fn color(self, color: Color) -> Self;
pub fn mono(self) -> Self;
pub fn truncate(self) -> Self;
pub fn text_align(self, align: TextAlign) -> Self;    // Start | Center | End
pub fn balance(self) -> Self;
pub fn pretty(self) -> Self;
pub fn text_wrap(self, wrap: TextWrap) -> Self;       // Normal | Balance | Pretty
pub fn optical(self, optical: OpticalSizing) -> Self;
pub fn optical_auto(self) -> Self;
```

#### 事件与交互

```rust
pub fn id(self, key: &str) -> Self;
pub fn on_click(self, msg: Msg) -> Self;
pub fn on_double_click(self, msg: Msg) -> Self;
pub fn on_right_click(self, msg: Msg) -> Self;
pub fn on_hover(self, msg: Msg) -> Self;
pub fn on_key(self, f: impl Fn(&KeyInput) -> Option<Msg> + 'static) -> Self;
pub fn on_drag(self, f: impl Fn(f32, f32) -> Option<Msg> + 'static) -> Self;
pub fn on_drag_end(self, msg: Msg) -> Self;
pub fn on_swipe(self, f: impl Fn(SwipeDir) -> Msg + 'static) -> Self;
pub fn on_input(self, f: impl Fn(&str) -> Msg + 'static) -> Self;
pub fn on_file_drop(self, f: impl Fn(&std::path::Path) -> Msg + 'static) -> Self;
pub fn drag_source(self, payload: impl Into<String>) -> Self;
pub fn on_drop(self, f: impl Fn(&str) -> Option<Msg> + 'static) -> Self;
pub fn on_close(self, msg: Msg) -> Self;
pub fn on_type_ahead(self, f: impl Fn(&str) -> Option<Msg> + 'static) -> Self;
pub fn overlay(self, overlay: Overlay) -> Self;
pub fn focusable(self, focusable: bool) -> Self;
pub fn autofocus(self) -> Self;
pub fn cursor(self, cursor: Cursor) -> Self;
pub fn disabled(self, disabled: bool) -> Self;
pub fn transition(self, t: Transition) -> Self;
pub fn enter(self, transition: Transition) -> Self;
pub fn exit(self, transition: Transition) -> Self;
pub fn exit_to(self, opacity: f32, scale: f32, dx: f32, dy: f32) -> Self;
pub fn animate_layout(self) -> Self;
pub fn selectable(self) -> Self;
pub fn live(self) -> Self;
pub fn semantics(self, semantics: Semantics) -> Self;
pub fn label(self, label: impl Into<String>) -> Self;
pub fn value(self, value: impl Into<String>) -> Self;
pub fn press_scale(self) -> Self;
pub fn invalid(self, invalid: bool) -> Self;
pub fn state_layer(self, content: impl Fn(&Theme) -> Color + 'static) -> Self;
pub fn hover(self, f: impl Fn(Style) -> Style + 'static) -> Self;
pub fn hover_themed(self, f: impl Fn(&Theme, Style) -> Style + 'static) -> Self;
pub fn active/active_themed/focus/focus_themed(...) -> Self;
pub fn themed(self, f: impl Fn(&Theme, Style) -> Style + 'static) -> Self;
```

**没有 `Element::on_change`**。`on_change` 是 `Slider` / `RangeSlider` / `Select` 自己的 builder 方法，不是 Element 通用方法。

#### 子节点

```rust
pub fn children<M>(self, children: impl IntoChildren<Msg, M>) -> Self;
pub fn child(self, child: impl Into<Element<Msg>>) -> Self;
```

`.children(...)` 支持两类：

1. 同一种 `Into<Element<Msg>>` 的迭代器/数组：`.children([button("A"), button("B")])`；
2. 最多 12 个不同类型的 tuple：`.children((text("Hi"), button("Go"), row()))`。

#### 组合

```rust
pub fn map<B: 'static>(self, f: impl Fn(Msg) -> B + Clone + 'static) -> Element<B>;
```

子组件用自己的 `Msg` 写，父组件用它 map 进来：

```rust
use fenestra::prelude::*;

#[derive(Clone)]
enum ChildMsg { Clicked }

fn child() -> Element<ChildMsg> {
    button("Child").on_click(ChildMsg::Clicked).into()
}

#[derive(Clone)]
enum ParentMsg { Child(ChildMsg) }

fn parent() -> Element<ParentMsg> {
    child().map(ParentMsg::Child)
}
```

### 3.3 完整可编译片段

```rust
use fenestra::prelude::*;

#[derive(Clone)]
enum Msg { Save, Name(String) }

fn view(name: &str) -> Element<Msg> {
    col()
        .w_full()
        .h_full()
        .p(SP6)
        .gap(SP4)
        .items_center()
        .justify_center()
        .bg(Theme::light().bg)
        .children((
            text("Settings").size(TextSize::Xl).weight(Weight::Semibold),
            text("Muted line")
                .size(TextSize::Sm)
                .color(Theme::light().text_muted),
            text_input(name)
                .placeholder("Name")
                .width(280.0)
                .on_input(Msg::Name)
                .id("name"),
            row().gap(SP3).children((
                button("Save").on_click(Msg::Save),
                button("Cancel").variant(ButtonVariant::Ghost),
            )),
        ))
}
```

注意最后 `text_input(...)` 返回 `TextInput<Msg>`，`button(...)` 返回 `Button<Msg>`；tuple 的 `IntoChildren` 会自动 `.into()`，所以能直接混排。

---

## 4. 设计令牌与 Theme

源码：`fenestra-core-0.41.0/src/tokens.rs`、`theme.rs`、`color` crate。

### 4.1 间距 `SP*`

```rust
pub const SP0: f32 = 0.0;
pub const SP0_5: f32 = 2.0;
pub const SP1: f32 = 4.0;
pub const SP2: f32 = 8.0;
pub const SP3: f32 = 12.0;
pub const SP4: f32 = 16.0;
pub const SP5: f32 = 20.0;
pub const SP6: f32 = 24.0;
pub const SP8: f32 = 32.0;
pub const SP10: f32 = 40.0;
pub const SP12: f32 = 48.0;
pub const SP16: f32 = 64.0;
```

### 4.2 圆角 `R_*`

```rust
pub const R_SM: f32 = 6.0;
pub const R_MD: f32 = 10.0;
pub const R_LG: f32 = 14.0;
pub const R_XL: f32 = 20.0;
pub const R_FULL: f32 = f32::INFINITY;  // 绘制时裁到短边一半
```

`RadiusScale` 用于整站换圆角：

```rust
pub struct RadiusScale { pub sm: f32, pub md: f32, pub lg: f32, pub xl: f32 }
impl RadiusScale {
    pub fn from_base(base: f32) -> Self;  // 0.6/1.0/1.4/2.0 × base
    pub fn sharp() -> Self;               // 1/2/3/4
    pub fn soft() -> Self;                // base 16
}
Theme::light().with_radius(RadiusScale::sharp())
```

### 4.3 `TextSize` 全部变体

```rust
pub enum TextSize {
    Xs,    // 12px
    Sm,    // 14px
    Base,  // 16px（默认）
    Lg,    // 20px
    Xl,    // 25px
    Xl2,   // 31px
    Xl3,   // 39px
}
impl TextSize {
    pub const fn px(self) -> f32;
    pub const fn line_height(self) -> f32;  // 1.5 / 1.4 / 1.25 / 1.15
    pub fn letter_spacing(self) -> f32;
}
```

### 4.4 `Weight`

```rust
pub enum Weight {
    Regular,   // 400，默认
    Medium,    // 500
    Semibold,  // 600
}
impl Weight { pub const fn value(self) -> f32; }
```

只有三档。要 700 只能注册自己的字体并靠 `FamilyRole`，没有 `Weight::Bold`。

### 4.5 颜色类型怎么构造

`fenestra_core::Color` 是 `peniko::Color` 的再导出，实际是 `color::AlphaColor<color::Srgb>`：

```rust
pub type Color = color::AlphaColor<color::Srgb>;
```

可用构造：

```rust
use fenestra::prelude::*;

let a = Color::from_rgba8(255, 80, 40, 255);      // sRGB 8-bit，最常用
let b = Color::new([0.2, 0.4, 0.9, 1.0]);         // sRGB 浮点 0..=1
let c = a.with_alpha(0.5);                        // 半透明
let d = Color::TRANSPARENT;                       // 全透明
let e = oklch(0.62, 0.17, 262.0);                 // L 0..=1, C, H 度
let [l, ch, h] = oklch_of(e);                     // 反查 OKLCH
let f = Color::WHITE;
let g = Color::BLACK;
```

注意 `Color::new` 的四个分量是 **sRGB 编码值**（不是线性光），与 `from_rgba8` 同域；OKLCH 请走 `oklch(...)`。

其他相关枚举：

```rust
pub enum ShadowToken { Xs, Sm, Md, Lg, Xl }
pub enum Elevation { Shadowed, Flat }          // 默认 Shadowed
pub enum MotionDuration { Micro, Fast, Base, Slow } // 100/120/200/300ms
pub enum WritingDir { Ltr, Rtl }
pub enum Contrast { Low, Standard, High }
```

`ShadowToken` 通过 `.shadow(token)` 使用，由 `Theme::shadow(token)` 解析成多层阴影；`Elevation::Flat` 让静置卡片改用边框而不是阴影。

### 4.6 `Theme` API

`Theme` 字段很多，全部 `pub`，关键字段：

```rust
pub struct Theme {
    pub mode: Mode,                    // Light | Dark
    pub accent_hue: f32,
    pub neutral_hue: f32,
    pub neutral_chroma_mult: f32,
    pub neutrals: Ramp,                // 12 阶
    pub accents: Ramp,
    pub neutral_alpha: Ramp,
    pub accent_alpha: Ramp,
    pub danger: StatusColors,
    pub warning: StatusColors,
    pub success: StatusColors,

    pub bg: Color,
    pub surface: Color,
    pub surface_raised: Color,
    pub element: Color,
    pub element_hover: Color,
    pub element_active: Color,
    pub border_subtle: Color,
    pub border: Color,
    pub border_strong: Color,

    pub text: Color,
    pub text_muted: Color,
    pub text_subtle: Color,
    pub text_disabled: Color,

    pub accent: Color,
    pub accent_hover: Color,
    pub accent_active: Color,
    pub accent_bg: Color,
    pub accent_border: Color,
    pub accent_text: Color,
    pub on_accent: Color,

    pub radius: RadiusScale,
    pub corner_smoothing: f32,
    pub optical_sizing: OpticalSizing,
    pub elevation: Elevation,
    pub text_scale: f32,
    pub direction: WritingDir,
}
```

构造与派生：

```rust
pub fn light() -> Self;                                   // accent hue 262
pub fn dark() -> Self;
pub fn from_accent(hue_deg: f32, mode: Mode) -> Self;      // 换品牌色
pub fn duotone(neutral_hue: f32, neutral_chroma: f32, accent_hue: f32, mode: Mode) -> Self;
pub fn derive(base: BaseField, accent_hue: f32, contrast: Contrast, mode: Mode) -> Self;
```

`with_*` 链式：

```rust
pub fn with_radius(self, radius: RadiusScale) -> Self;
pub fn with_corner_smoothing(self, smoothing: f32) -> Self;   // clamp 0..=1
pub fn with_optical_sizing(self, optical: OpticalSizing) -> Self;
pub fn with_elevation(self, elevation: Elevation) -> Self;    // Shadowed | Flat
pub fn with_text_scale(self, scale: f32) -> Self;             // Dynamic Type
pub fn with_direction(self, direction: WritingDir) -> Self;
pub fn rtl(self) -> Self;
pub fn is_rtl(&self) -> bool;
```

查询/派生：

```rust
pub fn accent_gradient(&self, angle_deg: f32) -> Paint;
pub fn shadow(&self, token: ShadowToken) -> Vec<Shadow>;
pub fn shadow_tint(&self) -> Color;
pub fn elevated_surface(&self, level: u8) -> Color;
pub fn contrast_report(&self) -> Vec<ContrastViolation>;
pub fn validate_contrast(&self) -> Result<(), Vec<ContrastViolation>>;
pub fn text_on(&self, bg: Color) -> Color;
pub fn dump(&self) -> String;
```

**`Theme::with_accent(...)` 不存在**。要换品牌色用：

```rust
use fenestra::prelude::*;

fn brand() -> Theme {
    Theme::from_accent(30.0, Mode::Dark)       // 橙色品牌
        .with_radius(RadiusScale::sharp())
        .with_elevation(Elevation::Flat)
}
```

主题切换就是 `App::theme()` 根据 state 返回不同 theme：

```rust
struct S { dark: bool }
impl App for S {
    type Msg = ();
    fn update(&mut self, _: ()) {}
    fn theme(&self) -> Theme {
        if self.dark { Theme::dark() } else { Theme::light() }
    }
    fn view(&self) -> Element<()> { text("x").into() }
}
```

### 4.7 `fenestra-looks` 六种设计语言

源码：`fenestra-looks-0.41.0/src/lib.rs`。

```rust
pub struct Look {
    pub name: &'static str,
    pub theme: Theme,
    pub faces: Vec<(FamilyRole, &'static [u8])>,
}
impl Look {
    pub fn fonts(&self) -> Fonts;   // embedded + 该 look 的字体
}

pub fn product(mode: Mode) -> Look;          // name "product"
pub fn editorial(mode: Mode) -> Look;        // name "editorial"
pub fn terminal(mode: Mode) -> Look;         // name "terminal"
pub fn console(mode: Mode) -> Look;          // name "console"
pub fn warm_editorial(mode: Mode) -> Look;   // name "warm-editorial"
pub fn playful(mode: Mode) -> Look;          // name "playful"
pub fn all(mode: Mode) -> Vec<Look>;         // 按上面顺序
```

调用：

```rust
use fenestra::Mode;
use fenestra::prelude::*;

fn use_look() {
    let look = fenestra_looks::editorial(Mode::Dark);
    let theme = look.theme.clone();

    // headless：
    let fonts = look.fonts();

    // 窗口：
    let mut opts = WindowOptions::titled("Editorial");
    for (role, bytes) in &look.faces {
        opts = opts.with_font(*role, bytes.to_vec());
    }

    let _ = (theme, fonts, opts);
}
```

注意 `Look.name` 里是 `"warm-editorial"`（连字符），函数名是 `warm_editorial`（下划线）。

`fenestra-looks` 在根 `Cargo.toml` 里目前只是 facade 的 dev-dependency，不在 `[workspace.dependencies]`。若 app 要直接 `use fenestra_looks`，需要在对应 crate 的 Cargo.toml 加 `fenestra-looks = "0.41.0"`。本次未改任何 crate 的 Cargo.toml。

---

## 5. 组件清单

源码：`fenestra-kit-0.41.0/src/widgets/*.rs`。所有组件 builder 都实现 `Into<Element<Msg>>`，既可直接进 `.children(...)`，也可显式 `.into()`。

共享尺寸/密度类型（`widgets/mod.rs`）：

```rust
pub enum ControlSize { Xs, Sm, Md, Lg }   // 高度 24 / 32 / 36 / 40
pub enum Density { Compact, Comfortable, Spacious } // 默认 Comfortable
```

`ControlSize::metrics()` 返回 `ControlMetrics { height, pad_x, gap, font, icon }`；`metrics_at(Density)` 在保持字号不变的前提下缩放间距/高度。

### 5.1 Button / IconButton

```rust
pub enum ButtonVariant { Primary, Secondary, Ghost, Danger }  // Primary 默认
pub fn button<Msg>(label: impl Into<String>) -> Button<Msg>;
impl<Msg> Button<Msg> {
    pub fn variant(self, variant: ButtonVariant) -> Self;
    pub fn size(self, size: ControlSize) -> Self;   // Xs/Sm/Md/Lg
    pub fn density(self, density: Density) -> Self; // Compact/Comfortable/Spacious
    pub fn disabled(self, disabled: bool) -> Self;
    pub fn on_click(self, msg: Msg) -> Self;
    pub fn id(self, key: &str) -> Self;
}

pub fn icon_button<Msg>(icon: impl Into<Element<Msg>>) -> IconButton<Msg>;
impl<Msg> IconButton<Msg> {
    pub fn variant(self, ButtonVariant) -> Self;
    pub fn size(self, ControlSize) -> Self;
    pub fn density(self, Density) -> Self;
    pub fn disabled(self, bool) -> Self;
    pub fn label(self, impl Into<String>) -> Self;  // 无障碍名
    pub fn on_click(self, Msg) -> Self;
    pub fn id(self, &str) -> Self;
}
```

```rust
use fenestra::prelude::*;

#[derive(Clone)]
enum Msg { Save, Delete }

fn actions() -> Element<Msg> {
    row().gap(SP3).children((
        button("Save").on_click(Msg::Save),
        button("Delete").variant(ButtonVariant::Danger).on_click(Msg::Delete),
        icon_button(fenestra::kit::icons::x())
            .variant(ButtonVariant::Ghost)
            .label("Close"),
    ))
}
```

`fenestra::kit::icons` 提供 Lucide 子集，例如 `icons::x()`、`icons::chevron_down()`、`icons::check()`、`icons::circle_dot()`。

### 5.2 TextInput（重点）

```rust
pub fn text_input<Msg>(value: impl Into<String>) -> TextInput<Msg>;

impl<Msg> TextInput<Msg> {
    pub fn placeholder(self, impl Into<String>) -> Self;
    pub fn size(self, ControlSize) -> Self;
    pub fn density(self, Density) -> Self;
    pub fn width(self, f32) -> Self;                 // 默认 220
    pub fn invalid(self, bool) -> Self;
    pub fn disabled(self, bool) -> Self;
    pub fn on_input(self, f: impl Fn(String) -> Msg + 'static) -> Self;
    pub fn id(self, key: &str) -> Self;
    pub fn prefix(self, adornment: impl Into<Element<Msg>>) -> Self;
    pub fn suffix(self, adornment: impl Into<Element<Msg>>) -> Self;
}
```

#### 值绑定、读取、设置

- **读取内容**：没有 getter。app 自己是 source of truth；`on_input(|s: String| ...)` 把每次编辑后的完整字符串发回来，存进 state。
- **设置内容**：下次 `view()` 用新的 `text_input(&self.value)`。`build_frame` 里有 `editor.sync(&data.value, ...)`：外部值变化会重置 retained editor（并记一个 undo 单元），光标/选区可能被重置。
- **监听回车**：builder 本身 **没有** `on_submit` / `on_enter`。先把 `TextInput` 转成 `Element`，再挂 `Element::on_key`。单行输入里 Enter 不被编辑器消费，能到 `on_key`。
- **聚焦**：`Element::autofocus()`（元素新出现时聚焦一次）；或用户点击；普通 app 不能直接拿到 `WidgetId` 去 `FrameState::set_focus`。需要“按条件聚焦”时，用条件挂 `.autofocus()` 并配合稳定 `.id` 让它重新出现。
- **隐藏输入**：未确认有 `password`/mask 选项；源码中 `InputData` 只有 `value/placeholder/multiline`，没有 password 标志。不要假设有。

完整示例：

```rust
use fenestra::prelude::*;

struct Search { q: String, submitted: String }

#[derive(Clone)]
enum Msg { Query(String), Submit }

impl App for Search {
    type Msg = Msg;

    fn update(&mut self, msg: Msg) {
        match msg {
            Msg::Query(s) => self.q = s,
            Msg::Submit => self.submitted = self.q.clone(),
        }
    }

    fn view(&self) -> Element<Msg> {
        let mut input: Element<Msg> = text_input(&self.q)
            .placeholder("Search…")
            .width(320.0)
            .on_input(Msg::Query)
            .id("search")
            .into();
        input = input
            .on_key(|k| (k.key == Key::Enter).then_some(Msg::Submit))
            .autofocus();

        col().p(SP6).gap(SP3).children((
            input,
            text(format!("submitted: {}", self.submitted)),
        ))
    }
}

fn main() {
    fenestra::run(
        Search { q: String::new(), submitted: String::new() },
        WindowOptions::titled("search").with_size(480.0, 240.0),
    );
}
```

`autofocus` 在每次 `view()` 都调用不会每帧抢焦点：core 只在元素“新出现”时聚焦（`autofocus_last` 记录 frame）。

### 5.3 TextArea

```rust
pub fn text_area<Msg>(value: impl Into<String>) -> TextArea<Msg>;

impl<Msg> TextArea<Msg> {
    pub fn placeholder(self, impl Into<String>) -> Self;
    pub fn width(self, f32) -> Self;          // 默认 320
    pub fn min_height(self, f32) -> Self;     // 默认 80
    pub fn invalid(self, bool) -> Self;
    pub fn disabled(self, bool) -> Self;
    pub fn on_input(self, f: impl Fn(String) -> Msg + 'static) -> Self;
    pub fn id(self, key: &str) -> Self;
}
```

```rust
use fenestra::prelude::*;

#[derive(Clone)]
enum Msg { Notes(String) }

fn notes(v: &str) -> Element<Msg> {
    text_area(v)
        .placeholder("Write something…")
        .width(420.0)
        .min_height(120.0)
        .on_input(Msg::Notes)
        .id("notes")
        .into()
}
```

**坑**：多行编辑器把 Enter 当换行消费掉（`handle_key` 中 `Key::Enter if multiline` 直接插入 `\n`），所以无法用 Enter 提交。请用显式按钮，或让用户 Tab 到按钮后按 Enter。Ctrl+Enter 也会被消费（该分支不检查修饰键）。

### 5.4 Select

```rust
pub fn select<Msg>(
    selected: usize,
    options: impl IntoIterator<Item = impl Into<String>>,
) -> Select<Msg>;

impl<Msg> Select<Msg> {
    pub fn size(self, ControlSize) -> Self;
    pub fn density(self, Density) -> Self;
    pub fn width(self, f32) -> Self;      // 默认 200
    pub fn disabled(self, bool) -> Self;
    pub fn on_change(self, f: impl Fn(usize) -> Msg + 'static) -> Self;
    pub fn id(self, &str) -> Self;
}
```

```rust
use fenestra::prelude::*;

#[derive(Clone)]
enum Msg { Plan(usize) }

fn plan(v: usize) -> Element<Msg> {
    select(v, ["Daily", "Weekly", "Monthly"])
        .width(200.0)
        .on_change(Msg::Plan)
        .id("plan")
        .into()
}
```

键盘：Up/Down 直接改值（每步发一条 `on_change`），Enter/Space 开合列表，字母做 type-ahead。

### 5.5 Switch / Checkbox

```rust
pub fn switch<Msg>(on: bool) -> Switch<Msg>;
impl<Msg> Switch<Msg> {
    pub fn label(self, impl Into<String>) -> Self;
    pub fn disabled(self, bool) -> Self;
    pub fn on_toggle(self, msg: Msg) -> Self;
    pub fn id(self, &str) -> Self;
}

pub fn checkbox<Msg>(checked: bool) -> Checkbox<Msg>;
impl<Msg> Checkbox<Msg> {
    pub fn label(self, impl Into<String>) -> Self;
    pub fn indeterminate(self, on: bool) -> Self;
    pub fn size(self, ControlSize) -> Self;
    pub fn disabled(self, bool) -> Self;
    pub fn on_toggle(self, msg: Msg) -> Self;
    pub fn id(self, &str) -> Self;
}
```

两者 `on_toggle` 都接受**固定 Msg**，不是闭包；需要新值时在 `update` 里翻转：

```rust
use fenestra::prelude::*;

#[derive(Clone)]
enum Msg { Dark(bool), Remember }

fn controls(dark: bool, remember: bool) -> Element<Msg> {
    col().gap(SP3).children((
        switch(dark).label("Dark").on_toggle(Msg::Dark(!dark)).id("dark"),
        checkbox(remember).label("Remember me").on_toggle(Msg::Remember).id("remember"),
    ))
}
```

### 5.6 Slider / RangeSlider

```rust
pub fn slider<Msg>(value: f32) -> Slider<Msg>;
impl<Msg> Slider<Msg> {
    pub fn range(self, min: f32, max: f32) -> Self;  // 默认 0.0..=1.0
    pub fn width(self, f32) -> Self;                 // 默认 200
    pub fn step(self, f32) -> Self;                  // 默认 range/20
    pub fn marks(self, on: bool) -> Self;
    pub fn disabled(self, bool) -> Self;
    pub fn on_change(self, f: impl Fn(f32) -> Msg + 'static) -> Self;
    pub fn id(self, &str) -> Self;
}

pub fn range_slider<Msg>(low: f32, high: f32) -> RangeSlider<Msg>;
impl<Msg> RangeSlider<Msg> {
    pub fn range(self, min: f32, max: f32) -> Self;
    pub fn width(self, f32) -> Self;
    pub fn step(self, f32) -> Self;
    pub fn marks(self, on: bool) -> Self;
    pub fn disabled(self, bool) -> Self;
    pub fn on_change(self, f: impl Fn(f32, f32) -> Msg + 'static) -> Self;
    pub fn id(self, &str) -> Self;
}
```

```rust
use fenestra::prelude::*;

#[derive(Clone)]
enum Msg { Volume(f32) }

fn volume(v: f32) -> Element<Msg> {
    slider(v)
        .range(0.0, 100.0)
        .step(1.0)
        .marks(true)
        .width(240.0)
        .on_change(Msg::Volume)
        .id("volume")
        .into()
}
```

值始终是 domain 单位，不是 0..1 比例。

### 5.7 Tabs

```rust
pub fn tabs<Msg: Clone + 'static>(
    active: usize,
    labels: impl IntoIterator<Item = impl Into<String>>,
    on_select: impl Fn(usize) -> Msg + 'static,
) -> Element<Msg>;
```

```rust
use fenestra::prelude::*;

#[derive(Clone)]
enum Msg { Tab(usize) }

fn tabbar(active: usize) -> Element<Msg> {
    tabs(active, ["Overview", "Activity", "Settings"], Msg::Tab)
}
```

### 5.8 Modal

```rust
pub fn modal<Msg>(title: impl Into<String>) -> Modal<Msg>;
impl<Msg> Modal<Msg> {
    pub fn child(self, child: impl Into<Element<Msg>>) -> Self;
    pub fn max_width(self, f32) -> Self;   // 默认 480
    pub fn on_close(self, msg: Msg) -> Self;
    pub fn id(self, &str) -> Self;
}
```

Modal 是 app-driven：只在 open 时把它挂进树。它自身是 `Overlay::modal()` 子节点，需要有个 anchor（通常根 `col()`）。

```rust
use fenestra::prelude::*;

struct S { open: bool }

#[derive(Clone)]
enum Msg { Open, Close }

impl App for S {
    type Msg = Msg;
    fn update(&mut self, msg: Msg) {
        match msg { Msg::Open => self.open = true, Msg::Close => self.open = false }
    }
    fn view(&self) -> Element<Msg> {
        let mut root = col().w_full().h_full().p(SP6)
            .children((button("Open").on_click(Msg::Open),));
        if self.open {
            root = root.child(
                modal("Confirm")
                    .child(text("Are you sure?"))
                    .on_close(Msg::Close),
            );
        }
        root
    }
}
```

`on_close` 在 Esc、点 backdrop、右上角关闭按钮时触发。

### 5.9 Toast

```rust
pub fn toast_stack<Msg>(
    toasts: impl IntoIterator<Item = (impl Into<String>, Status)>,
) -> ToastStack<Msg>;

impl<Msg> ToastStack<Msg> {
    pub fn width(self, f32) -> Self;                        // 默认 340
    pub fn on_dismiss(self, f: impl Fn(usize) -> Msg + 'static) -> Self;
    pub fn id(self, &str) -> Self;
}
```

`Status`：`Accent`（默认）/`Danger`/`Warning`/`Success`。

```rust
use std::time::Duration;
use fenestra::prelude::*;

struct S { items: Vec<(String, Status)>, proxy: Option<Proxy<Msg>> }

#[derive(Clone)]
enum Msg { Add, Expire(usize), Dismiss(usize) }

impl App for S {
    type Msg = Msg;

    fn init(&mut self, proxy: Proxy<Msg>) { self.proxy = Some(proxy); }

    fn update(&mut self, msg: Msg) {
        match msg {
            Msg::Add => {
                self.items.push(("Saved".into(), Status::Success));
                let i = self.items.len() - 1;
                if let Some(p) = self.proxy.clone() {
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_secs(4));
                        p.send(Msg::Expire(i));
                    });
                }
            }
            Msg::Expire(i) | Msg::Dismiss(i) => { if i < self.items.len() { self.items.remove(i); } }
        }
    }

    fn view(&self) -> Element<Msg> {
        col().w_full().h_full().p(SP6).children((
            button("Add").on_click(Msg::Add),
            toast_stack(self.items.iter().cloned())
                .on_dismiss(Msg::Dismiss)
                .id("toasts"),
        ))
    }
}
```

自动消失不是组件行为，要 app 自己发延迟消息（如上），或用 `Cmd`/`Sub`。

### 5.10 Card / StatCard / Badge / Avatar / Callout / Status

```rust
pub fn card<Msg>() -> Element<Msg>;   // p SP6, gap SP3, Surface::Card
pub fn reading_column<Msg>() -> Element<Msg>; // 52ch 阅读栏

pub fn stat_card<Msg>(label: impl Into<String>, value: impl Into<String>) -> StatCard<Msg>;
impl<Msg> StatCard<Msg> {
    pub fn delta(self, label: impl Into<String>, status: Status) -> Self;
}

pub fn badge<Msg>(label: impl Into<String>, status: Status) -> Element<Msg>;
pub fn badge_dot<Msg>(status: Status) -> Element<Msg>;
pub fn avatar<Msg>(initials: impl Into<String>) -> Element<Msg>; // 32px 圆形
pub fn callout<Msg>(status: Status, message: impl Into<String>) -> Element<Msg>;

pub fn status<Msg>(label: impl Into<String>, status: Status) -> StatusIndicator<Msg>;
impl<Msg> StatusIndicator<Msg> { pub fn live(self, live: bool) -> Self; }
```

```rust
use fenestra::prelude::*;

fn card_demo() -> Element<()> {
    card().children((
        text("Revenue").size(TextSize::Sm).color(Theme::light().text_muted),
        text("$48,210").size(TextSize::Xl2).weight(Weight::Semibold).tabular(),
        badge("+12.5%", Status::Success),
        status("Operational", Status::Success).live(true),
        avatar("RH"),
    ))
}
```

### 5.11 Kbd / Divider / Spinner / Progress / Meter

```rust
pub fn kbd<Msg>(keys: impl IntoIterator<Item = impl Into<String>>) -> Element<Msg>;
pub fn kbd_raised<Msg>(keys: impl IntoIterator<Item = impl Into<String>>) -> Element<Msg>;

pub fn divider<Msg>() -> Element<Msg>;  // core

pub fn spinner<Msg>() -> Element<Msg>;  // 16px，800ms/圈
pub fn progress<Msg>(fraction: f32) -> Element<Msg>;            // 0..=1
pub fn progress_indeterminate<Msg>() -> Element<Msg>;
pub fn wavy_progress<Msg>(fraction: f32, width: f32) -> WavyProgress<Msg>;
impl<Msg> WavyProgress<Msg> {
    pub fn amplitude(self, f32) -> Self;
    pub fn wavelength(self, f32) -> Self;
}

pub fn meter(value: f32, min: f32, max: f32) -> Meter;
impl Meter {
    pub fn low(self, f32) -> Self;
    pub fn high(self, f32) -> Self;
    pub fn optimum(self, f32) -> Self;
    pub fn label(self, impl Into<String>) -> Self;
}
```

```rust
use fenestra::prelude::*;

fn progress_demo(p: f32) -> Element<()> {
    col().gap(SP3).children((
        progress(p),
        progress_indeterminate(),
        spinner(),
        kbd(["cmd", "K"]),
        divider(),
        meter(0.72, 0.0, 1.0).label("Disk").low(0.2).high(0.8),
    ))
}
```

### 5.12 Table / DataTable

简单表：

```rust
pub fn table<Msg>(
    columns: impl IntoIterator<Item = impl Into<String>>,
    rows: Vec<Vec<String>>,
) -> Element<Msg>;
```

```rust
use fenestra::prelude::*;

fn simple_table() -> Element<()> {
    table(
        ["Name", "Status"],
        vec![
            vec!["api-server".into(), "Healthy".into()],
            vec!["worker".into(), "Degraded".into()],
        ],
    )
}
```

DataTable（排序/多选/虚拟化/列宽/列序/固定列/过滤行）：

```rust
pub fn data_table<Msg>(
    columns: impl IntoIterator<Item = impl Into<String>>,
    rows: Vec<Vec<String>>,
) -> DataTable<Msg>;

impl<Msg> DataTable<Msg> {
    pub fn id(self, key: &str) -> Self;
    pub fn sort(self, column: usize, ascending: bool) -> Self;
    pub fn selected(self, row: Option<usize>) -> Self;
    pub fn selection(self, flags: impl IntoIterator<Item = bool>) -> Self;
    pub fn sticky_header(self, on: bool) -> Self;
    pub fn column_widths(self, widths: impl IntoIterator<Item = f32>) -> Self;
    pub fn resize_active(self, column: Option<usize>) -> Self;
    pub fn column_order(self, order: impl IntoIterator<Item = usize>) -> Self;
    pub fn filter(self, text: impl IntoIterator<Item = String>) -> Self;
    pub fn pinned_left(self, count: usize) -> Self;
    pub fn pinned_right(self, count: usize) -> Self;
    pub fn on_sort(self, f: impl Fn(usize) -> Msg + 'static) -> Self;
    pub fn on_select(self, f: impl Fn(usize) -> Msg + 'static) -> Self;
    pub fn on_select_row(self, f: impl Fn(usize) -> Msg + 'static) -> Self;
    pub fn on_select_all(self, msg: Msg) -> Self;
    pub fn on_resize(self, f: impl Fn(usize, f32) -> Msg + 'static) -> Self;
    pub fn on_resize_end(self, msg: Msg) -> Self;
    pub fn on_reorder(self, f: impl Fn(usize, usize) -> Msg + 'static) -> Self;
    pub fn on_filter(self, f: impl Fn(usize, String) -> Msg + 'static) -> Self;
}
```

无需“具体名见源码”的保留说明：上表逐条来自 `data_table.rs`。超过 50 行（`AUTO_SCROLL_ROWS`）或请求 sticky header/pin 时，body 自动变成专用滚动容器并虚拟化；必须给稳定 `.id`，否则滚动偏移和过滤器 editor 状态无法持久。

### 5.13 滚动与虚拟列表

滚动是容器样式：

```rust
pub fn overflow_hidden(self) -> Self;
pub fn scroll_y(self) -> Self;
pub fn scroll_x(self) -> Self;
pub fn scroll_xy(self) -> Self;
pub fn sticky_top/sticky_bottom/sticky_left/sticky_right(self, offset: f32) -> Self;
pub fn stick_to_bottom(self) -> Self;
```

滚轮由 runner 转发 `InputEvent::Wheel`，自动找指针下最近的、对应轴的滚动容器。滚动偏移存在 `FrameState`，按容器 `WidgetId` 持久化；给容器稳定 `.id(...)`。

固定高度虚拟列表：

```rust
pub fn virtual_list<Msg: 'static>(
    count: usize,
    row_height: f32,
    builder: impl Fn(usize) -> Element<Msg> + 'static,
) -> Element<Msg>;

pub fn virtual_list_variable<Msg: 'static>(
    count: usize,
    estimated_height: f32,
    builder: impl Fn(usize) -> Element<Msg> + 'static,
) -> Element<Msg>;
```

```rust
use fenestra::prelude::*;

struct S { n: usize }

#[derive(Clone)]
enum Msg { Open(usize) }

fn log_list(n: usize) -> Element<Msg> {
    virtual_list(n, 36.0, move |i| {
        row()
            .items_center()
            .px(SP3)
            .h(36.0)
            .on_click(Msg::Open(i))
            .children((text(format!("Row {i}")).size(TextSize::Sm),))
    })
    .id("log")
}
```

要点：必须 `.scroll_y()`（`virtual_list` 已包好）和稳定 `.id`；行高固定时只生成可视窗口；变高列表用估算高度并逐帧自校正；虚拟行内 **不支持 overlay**；`count` 上限 10,000,000。

### 5.14 Image

```rust
pub fn image_rgba8<Msg>(width: u32, height: u32, pixels: Vec<u8>) -> Element<Msg>;
pub fn image_payload(width: u32, height: u32, pixels: Vec<u8>) -> ImageData;
pub fn image_from_data<Msg>(data: ImageData) -> Element<Msg>;
```

```rust
use fenestra::prelude::*;

fn thumb(width: u32, height: u32, rgba: Vec<u8>) -> Element<()> {
    image_rgba8(width, height, rgba)
        .w(96.0)
        .h(64.0)
        .rounded(R_MD)
}
```

像素是 **straight-alpha** RGBA8，row-major；`rounded_full()` 可裁成圆头像。像素不足 `width*height` 时自动缩到完整行数，不 panic。

### 5.15 Tooltip / Popover / CommandPalette

```rust
pub fn tooltip<Msg>(target: impl Into<Element<Msg>>, label: impl Into<String>) -> Element<Msg>;

pub fn popover<Msg: 'static>(content: impl Into<Element<Msg>>) -> Element<Msg>;
// 默认 Overlay::menu()（点击父级开合）；可 .overlay(Overlay::...) 覆盖

pub fn command_palette<Msg>(
    query: impl Into<String>,
    open: bool,
    commands: impl IntoIterator<Item = (impl Into<String>, Msg)>,
) -> CommandPalette<Msg>;
impl<Msg> CommandPalette<Msg> {
    pub fn on_input(self, f: impl Fn(String) -> Msg + 'static) -> Self;
    pub fn highlighted(self, index: Option<usize>) -> Self;
    pub fn on_navigate(self, f: impl Fn(usize) -> Msg + 'static) -> Self;
    pub fn on_close(self, msg: Msg) -> Self;
    pub fn id(self, &str) -> Self;
}
```

```rust
use fenestra::prelude::*;

#[derive(Clone)]
enum Msg { Save, Open, Query(String), Nav(usize), Close }

fn palette(query: &str, open: bool) -> Element<Msg> {
    if !open { return col(); }
    command_palette(query, open, [("Save", Msg::Save), ("Open", Msg::Open)])
        .on_input(Msg::Query)
        .on_navigate(Msg::Nav)
        .on_close(Msg::Close)
        .id("palette")
        .into()
}

fn save_tip() -> Element<Msg> {
    tooltip(button("Save").on_click(Msg::Save), "Save the document")
}
```

CommandPalette 内部已把 Enter/Up/Down 接到输入框；`open=false` 时 builder 也返回空 `col()`，但通常还是只在 open 时挂载。

---

## 6. 事件与输入

源码：`fenestra-core-0.41.0/src/events.rs`、`fenestra-shell-0.41.0/src/window.rs`。

### 6.1 键盘类型

```rust
pub enum Key {
    Enter, Space, Escape,
    ArrowLeft, ArrowRight, ArrowUp, ArrowDown,
    Home, End, Backspace, Delete, PageUp, PageDown,
    Char(char),
}

pub struct KeyInput {
    pub key: Key,
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub meta: bool,
}

impl KeyInput {
    pub fn plain(key: Key) -> Self;
}
```

### 6.2 `InputEvent`

```rust
pub enum InputEvent {
    PointerMove { x: f32, y: f32 },
    PointerDown,
    PointerUp,
    RightDown,
    RightUp,
    FileDrop(std::path::PathBuf),
    Wheel { dx: f32, dy: f32 },
    Modifiers { shift: bool, ctrl: bool, alt: bool, meta: bool },
    PointerLeave,
    Tab,
    ShiftTab,
    Key(KeyInput),
    Text(String),
    ImePreedit { text: String, cursor: Option<(usize, usize)> },
}
```

### 6.3 Element 上能挂的事件

| 方法 | 触发 |
| --- | --- |
| `on_click(Msg)` | 按下并在同一元素上释放；也会把元素设为 focusable 并给 Pointer 光标 |
| `on_double_click(Msg)` | 400ms 内第二次完成点击（两次 single 也会各发一次） |
| `on_right_click(Msg)` | 右键按下 |
| `on_hover(Msg)` | 指针进入 |
| `on_key(Fn(&KeyInput)->Option<Msg>)` | **焦点在该元素上**时按键 |
| `on_input(Fn(&str)->Msg)` | 输入框每次编辑 |
| `on_drag(Fn(f32,f32)->Option<Msg>)` | 按下并拖动，参数是元素内 0..=1 的比例 |
| `on_drag_end(Msg)` | 被捕获的拖动释放 |
| `on_swipe(Fn(SwipeDir)->Msg)` | 快速滑动 |
| `on_file_drop(Fn(&Path)->Msg)` | 文件拖入 |
| `on_drop(Fn(&str)->Option<Msg>)` | 接收内部 drag_source 载荷 |
| `on_type_ahead(Fn(&str)->Option<Msg>)` | 焦点元素上的累积键入（1s 窗口） |
| `on_close(Msg)` | 可关闭 overlay 被 Esc/外部点击关闭时 |

没有 window-level 的 `on_key`。

### 6.4 全局快捷键 / 全局按键：真实情况

**0.41.0 没有 App 级全局键盘钩子。** 源码里 `dispatch` 只对 `state.focus` 指向的元素调用 `on_key`；没有冒泡，也没有 window fallback。

因此：

1. 给某个控件挂 `.on_key(...)`，只在它聚焦时生效。
2. 给根容器挂 `.on_key(...)` + `.focusable(true)` + `.autofocus()`，只在根聚焦时生效；一旦用户点进 TextInput，根失去焦点，快捷键不再触发。
3. 想“任何时候都响应”，需要在每个可能聚焦的控件上都挂同一处理器（例如把 TextInput 转成 Element 后再 `.on_key`）；或者用 OS 级 `global-hotkey` crate（本 workspace 已依赖 `global-hotkey = "0.6"`），不经过 fenestra。
4. macOS 原生菜单可用 `MenuSpec` / `MenuItemDesc::accelerator` 注册加速键；其他平台没有原生菜单加速键。

可编译的“根级快捷键 + 输入框回车”模式：

```rust
use fenestra::prelude::*;

#[derive(Clone)]
enum Msg { Save, Submit, Query(String), FocusInput }

struct S { q: String }

impl S {
    fn shortcut(k: &KeyInput) -> Option<Msg> {
        match k.key {
            Key::Char('s') if k.ctrl || k.meta => Some(Msg::Save),
            Key::Escape => Some(Msg::FocusInput),
            _ => None,
        }
    }
}

impl App for S {
    type Msg = Msg;
    fn update(&mut self, msg: Msg) {
        match msg {
            Msg::Query(s) => self.q = s,
            _ => {}
        }
    }
    fn view(&self) -> Element<Msg> {
        let mut input: Element<Msg> = text_input(&self.q)
            .on_input(Msg::Query)
            .id("q")
            .into();
        input = input.on_key(|k| {
            if k.key == Key::Enter { Some(Msg::Submit) } else { S::shortcut(k) }
        });

        col()
            .w_full()
            .h_full()
            .focusable(true)
            .autofocus()
            .on_key(S::shortcut)
            .children((input, button("Save").on_click(Msg::Save)))
    }
}
```

注意：普通字母键在 runner 里会走 `InputEvent::Text`，不会进 `on_key`；`on_key` 只看得到命名键（Enter/Esc/方向键等）和带 Ctrl/Cmd/Alt 的字符键。想监听普通字符请用 `on_input`/`on_type_ahead`，或者自己解析 `Text`（但 `Element` 没有 `on_text` 钩子）。

### 6.5 Tab 焦点

- `InputEvent::Tab` / `ShiftTab` 在 `Frame::focusables()` 顺序里循环。
- 点击 `on_click` 元素会自动设 focus（`on_click` 内部把 `focusable=true`）；`tabs` 等组合控件会显式 `.focusable(false)` 以保持整组一个 tab stop。
- `Esc` 关闭最顶层 overlay；`Enter/Space` 激活聚焦的 clickable 或开合菜单。
- 普通 App 无法直接 `set_focus(WidgetId)`；`Harness::focus(&by::...)` 可以，runner 内部也通过 AccessKit Focus action 做。

### 6.6 文件拖放

```rust
use fenestra::prelude::*;

fn dropper() -> Element<std::path::PathBuf> {
    col()
        .w(400.0)
        .h(200.0)
        .on_file_drop(|p| p.to_path_buf())
        .children((text("Drop a file here"),))
}
```

---

## 7. 异步、Proxy、Cmd、Sub

源码：`fenestra-core-0.41.0/src/cmd.rs`、`proxy.rs`、`app.rs`。

### 7.1 `Proxy<Msg>`

见 1.3。任意线程可 `send`；runner 用 winit user event 唤醒事件循环并重绘。窗口关闭后发送会被静默丢弃。

### 7.2 `Cmd<Msg>`

```rust
pub struct Cmd<Msg>;

impl<Msg> Cmd<Msg> {
    pub fn none() -> Self;
    pub fn msg(msg: Msg) -> Self;
    pub fn task(f: impl FnOnce() -> Msg + Send + 'static) -> Self;
    pub fn future(fut: impl Future<Output = Msg> + Send + 'static) -> Self;
    pub fn batch(cmds: impl IntoIterator<Item = Cmd<Msg>>) -> Self;
    pub fn map<B: 'static>(self, f: impl Fn(Msg) -> B + Send + Sync + Clone + 'static) -> Cmd<B>;
    pub fn is_none(&self) -> bool;
    pub fn run(self, now: &mut impl FnMut(Msg), spawn: &mut impl FnMut(CmdUnit<Msg>));
}

pub type CmdFuture<Msg> = Pin<Box<dyn Future<Output = Msg> + Send>>;

pub enum CmdUnit<Msg> {
    Task(Box<dyn FnOnce() -> Msg + Send>),
    Future(CmdFuture<Msg>),
}
impl<Msg> CmdUnit<Msg> {
    pub fn block(self) -> Msg;
}
```

语义：

- `None`：无效果。
- `msg`：下一轮循环立即投递（不递归进当前 `update`）。
- `task`：worker 线程执行，阻塞 IO/重计算可用；完成后消息回到 `update_with`。
- `future`：在 worker 线程用最小 park-based executor 驱动；**必须 runtime-agnostic**，不能直接 await tokio 的 IO。tokio 任务请自己持有 runtime，用 `Proxy` 回传。
- `batch`：并发，完成顺序不定。
- `map`：组合子组件效果。

`apply_cmd` 是 runner/harness 共用的执行语义：immediate 消息 FIFO 排空，deferred 单元交给 spawn。自喂消息链上限 `MAX_EFFECT_CHAIN = 10_000`，超过会 eprintln 并停止（防止 UI 冻结）。

### 7.3 `Sub<Msg>`

```rust
pub struct Sub<Msg>;

impl<Msg> Sub<Msg> {
    pub fn every(
        key: impl Into<String>,
        period: Duration,
        make: impl Fn() -> Msg + Send + Sync + 'static,
    ) -> Self;
    pub fn key(&self) -> &str;
    pub fn period(&self) -> Duration;
    pub fn tick(&self) -> Msg;
}
```

只有 `Sub::every`（不是 `Sub::interval`）。周期最少 1ms。声明在 `App::subscriptions()`，按 key 对账。

### 7.4 最小可编译：线程 + Cmd + Sub

```rust
use std::time::Duration;
use fenestra::prelude::*;

struct Worker {
    status: String,
    ticks: u64,
    proxy: Option<Proxy<Msg>>,
}

#[derive(Clone)]
enum Msg {
    Load,
    Loaded(String),
    Tick,
}

impl App for Worker {
    type Msg = Msg;

    fn init(&mut self, proxy: Proxy<Msg>) {
        self.proxy = Some(proxy);
    }

    fn update(&mut self, msg: Msg) {
        match msg {
            Msg::Load => {
                self.status = "loading…".into();
                // 手动线程 + Proxy 的写法
                if let Some(p) = self.proxy.clone() {
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_millis(300));
                        p.send(Msg::Loaded("done".into()));
                    });
                }
            }
            Msg::Loaded(s) => self.status = s,
            Msg::Tick => self.ticks += 1,
        }
    }

    fn subscriptions(&self) -> Vec<Sub<Msg>> {
        vec![Sub::every("tick", Duration::from_secs(1), || Msg::Tick)]
    }

    fn view(&self) -> Element<Msg> {
        col().p(SP6).gap(SP3).children((
            text(&self.status),
            text(format!("ticks: {}", self.ticks)).size(TextSize::Sm),
            button("Load").on_click(Msg::Load),
        ))
    }
}

fn main() {
    fenestra::run(
        Worker { status: "idle".into(), ticks: 0, proxy: None },
        WindowOptions::titled("worker").with_size(420.0, 240.0),
    );
}
```

同一逻辑用 `Cmd` 更“纯”（用下面这个 `impl App for Worker` 替换上面的同名 impl，字段定义不变）：

```rust
impl App for Worker {
    type Msg = Msg;
    fn update(&mut self, _: Msg) {}
    fn update_with(&mut self, msg: Msg) -> Cmd<Msg> {
        match msg {
            Msg::Load => {
                self.status = "loading…".into();
                Cmd::task(|| {
                    std::thread::sleep(Duration::from_millis(300));
                    Msg::Loaded("done".into())
                })
            }
            Msg::Loaded(s) => { self.status = s; Cmd::none() }
            Msg::Tick => { self.ticks += 1; Cmd::none() }
        }
    }
    fn view(&self) -> Element<Msg> { text(&self.status).into() }
}
```

`Cmd::future` 例子（不依赖 tokio）：

```rust
Cmd::future(async {
    // 这里不能直接用 tokio::time::sleep；用 std 线程包一层或自己的 runtime。
    Msg::Loaded("done".into())
})
```

---

## 8. fenestra-markdown

源码：`fenestra-markdown-0.41.0/src/lib.rs`。

```rust
pub struct Markdown<Msg>;
pub fn markdown<Msg>(source: impl Into<String>) -> Markdown<Msg>;
impl<Msg> Markdown<Msg> {
    pub fn on_link(self, f: impl Fn(&str) -> Msg + 'static) -> Self;
}
impl<Msg: Clone + 'static> From<Markdown<Msg>> for Element<Msg>;
```

用法：

```rust
use fenestra::prelude::*;

#[derive(Clone)]
enum Msg { OpenLink(String) }

fn doc(src: &str) -> Element<Msg> {
    fenestra_markdown::markdown(src)
        .on_link(|url| Msg::OpenLink(url.to_owned()))
        .into()
}

fn simple(src: &str) -> Element<()> {
    fenestra_markdown::markdown(src).into()
}
```

支持 CommonMark + GFM：标题、段落、粗斜体、行内 code、代码块（带语言 chip 和轻量语法着色）、列表、表格、任务列表、脚注、删除线、引用、分隔线、图片（alt 文本回退）、尖括号 autolink。

限制（源码注释）：

- 裸 URL autolink 在 pulldown-cmark 0.13.4 的 `ENABLE_GFM` 下尚不支持；`<https://…>` 可用。
- 代码高亮是 highlight-grade lexer，不是完整语法；跨行块注释/字符串后续行会按普通代码着色。
- 图片只渲染 alt 文本回退，不实际解码网络图片。
- 渲染结果的链接点击按“整段 row”处理（v1 取舍）。

---

## 9. image_rgba8

```rust
pub fn image_rgba8<Msg>(width: u32, height: u32, pixels: Vec<u8>) -> Element<Msg>;
```

```rust
use fenestra::prelude::*;

fn avatar_thumb(w: u32, h: u32, rgba: Vec<u8>) -> Element<()> {
    image_rgba8(w, h, rgba)
        .w(32.0)
        .h(32.0)
        .rounded_full()
}
```

- `pixels`：straight-alpha RGBA8，row-major，长度至少 `width * height * 4`；不足时按完整行截断，不 panic。
- 默认尺寸是图片尺寸；用 `.w/.h` 改变会拉伸。
- 绘制会裁到圆角，所以 `.rounded_full()` 是圆头像的正确做法。
- 大图/多帧建议用 `image_payload` + `image_from_data` 复用同一 `Arc` 像素块，避免每帧复制。

---

## 10. headless 渲染与截图回归

源码：`fenestra-shell-0.41.0/src/element_render.rs`、`synthetic.rs`、`headless.rs`、`harness.rs`。

### 10.1 元素渲染

```rust
pub fn render_element<Msg>(
    el: Element<Msg>,
    theme: &Theme,
    size: (u32, u32),
) -> image::RgbaImage;

pub fn try_render_element<Msg>(
    el: Element<Msg>,
    theme: &Theme,
    size: (u32, u32),
) -> Result<image::RgbaImage, ShellError>;

pub fn render_element_with<Msg>(
    el: Element<Msg>,
    theme: &Theme,
    size: (u32, u32),
    fonts: &mut Fonts,
) -> image::RgbaImage;

pub fn render_element_scaled<Msg>(
    el: Element<Msg>,
    theme: &Theme,
    size: (u32, u32),
    scale: f64,
) -> image::RgbaImage;

pub fn render_element_over<Msg>(
    el: Element<Msg>,
    theme: &Theme,
    size: (u32, u32),
    scale: f64,
    bg: Color,
    fonts: &mut Fonts,
    state: &mut FrameState,
) -> Result<image::RgbaImage, ShellError>;

pub fn render_element_with_state<Msg>(
    el: Element<Msg>,
    theme: &Theme,
    size: (u32, u32),
    state: &mut FrameState,
) -> image::RgbaImage;
```

```rust
use fenestra::prelude::*;
use fenestra::shell::render_element;

fn main() {
    let img = render_element(
        col().p(SP6).children((text("Preview").size(TextSize::Xl2),)),
        &Theme::dark(),
        (800, 600),
    );
    img.save("preview.png").expect("save png");
}
```

### 10.2 驱动 app 渲染

```rust
use fenestra::shell::{SyntheticEvent, render_app};

pub fn render_app<A: App>(
    app: &mut A,
    events: &[SyntheticEvent],
    size: (u32, u32),
    theme: &Theme,
) -> image::RgbaImage
where
    A::Msg: Send;
```

`SyntheticEvent` 全部变体：

```rust
pub enum SyntheticEvent {
    MouseMove { x: f32, y: f32 },
    MouseDown,
    MouseUp,
    RightDown,
    RightUp,
    FileDrop(std::path::PathBuf),
    Key(KeyInput),
    Text(String),
    Wheel { dx: f32, dy: f32 },
    Tab,
    ShiftTab,
    Modifiers(bool, bool, bool, bool),  // shift, ctrl, alt, meta
}
```

示例：

```rust
use fenestra::prelude::*;
use fenestra::shell::{SyntheticEvent, render_app};

#[derive(Clone)]
enum Msg { Name(String) }

struct S { name: String }

impl App for S {
    type Msg = Msg;
    fn update(&mut self, msg: Msg) { match msg { Msg::Name(s) => self.name = s } }
    fn view(&self) -> Element<Msg> {
        text_input(&self.name)
            .on_input(Msg::Name)
            .id("name")
            .into()
    }
}

fn main() {
    let mut app = S { name: String::new() };
    let img = render_app(
        &mut app,
        &[
            SyntheticEvent::MouseMove { x: 40.0, y: 24.0 },
            SyntheticEvent::MouseDown,
            SyntheticEvent::MouseUp,
            SyntheticEvent::Text("hello".into()),
        ],
        (480, 240),
        &Theme::light(),
    );
    assert_eq!(app.name, "hello");
    img.save("shot.png").expect("save png");
}
```

### 10.3 `Harness`（语义查询 + 稳定截图）

```rust
pub struct Harness<A: App>;

impl<A: App> Harness<A> where A::Msg: Send {
    pub fn new(app: A, theme: Theme, size: (u32, u32)) -> Self;
    pub fn try_new(app: A, theme: Theme, size: (u32, u32)) -> Result<Self, ShellError>;

    pub fn click(&mut self, q: &Query);
    pub fn right_click(&mut self, q: &Query);
    pub fn double_click(&mut self, q: &Query);
    pub fn hover(&mut self, q: &Query);
    pub fn type_text(&mut self, text: impl Into<String>);
    pub fn key(&mut self, key: KeyInput);
    pub fn tab(&mut self);
    pub fn shift_tab(&mut self);
    pub fn focus(&mut self, q: &Query);
    pub fn wheel(&mut self, q: &Query, dy: f32);
    pub fn pump(&mut self, ms: f64);
    pub fn update(&mut self, msg: A::Msg);
    pub fn query(&self, q: &Query) -> Option<AccessNode>;
    pub fn get(&self, q: &Query) -> AccessNode;
    pub fn take_messages(&mut self) -> Vec<A::Msg>;
    pub fn app(&self) -> &A;
    pub fn app_mut(&mut self) -> &mut A;
    pub fn render(&mut self) -> RgbaImage;
    pub fn film(&mut self, frames: usize, interval_ms: u64) -> Vec<RgbaImage>;
}
```

`Query` / `by`：

```rust
pub mod by {
    pub fn role(role: Semantics) -> Query;
    pub fn label(label: impl Into<String>) -> Query;
    pub fn label_contains(label: impl Into<String>) -> Query;
    pub fn value(value: impl Into<String>) -> Query;
    pub fn value_contains(value: impl Into<String>) -> Query;
    pub fn id(key: impl Into<String>) -> Query;
}
impl Query {
    pub fn name(self, label: impl Into<String>) -> Self;
    pub fn name_contains(self, label: impl Into<String>) -> Self;
}
```

```rust
use fenestra::prelude::*;
use fenestra::shell::Harness;

#[derive(Clone)]
enum Msg { Name(String) }

struct S { name: String }

impl App for S {
    type Msg = Msg;
    fn update(&mut self, msg: Msg) { match msg { Msg::Name(s) => self.name = s } }
    fn view(&self) -> Element<Msg> {
        text_input(&self.name).on_input(Msg::Name).id("name").into()
    }
}

fn main() {
    let mut h = Harness::new(S { name: String::new() }, Theme::light(), (480, 240));
    h.focus(&by::id("name"));
    h.type_text("hello");
    assert_eq!(h.query(&by::value("hello")).is_some(), true);
    let png = h.render();
    png.save("harness.png").expect("save png");
}
```

### 10.4 确定性与 alpha

- headless 固定 scale=1.0、reduced motion、embedded fonts only；布局/颜色/大部分像素确定。
- `render_element_scaled` / `render_element_over` 可做 hi-DPI 截图回归。
- vello 输出是 **premultiplied alpha**；写 straight-alpha 格式（如 PNG）前要 un-premultiply，否则透明边缘会发黑。`render_element` 走 `theme.bg` 不透明背景时通常看不出，但透明 UI 截图要注意。
- 渲染尺寸会被 clamp 到 `1..=max_dimension`（通常 8192）；检查返回图尺寸。
- headless 仍需要 compute-capable wgpu adapter；无 GPU/CI 时设 `FENESTRA_CPU=1` 用 vello CPU compute（`CPU_ENV`）。

---

## 11. 坑列表

以下均来自 0.41.0 源码 doc comment / 实现，按影响排序。

1. **CJK / emoji / 系统等宽字体只在窗口里有**：headless 用 `Fonts::embedded()`（只含 Inter），README 明确说真实 CJK/emoji/RTL 面来自 OS，只在真窗口出现。headless 截中文可能 tofu。要确定性中文截图，用 `render_element_with` + `Fonts` 手动 `register` 一个中文字体。
2. **没有全局键盘钩子**：`App` 无 `on_key`；`dispatch` 只把 Key 给 `state.focus` 的元素，且不冒泡。普通字母还走 `InputEvent::Text`，不进 `on_key`。全局快捷键需逐控件挂 handler、OS 级 `global-hotkey`，或 macOS 菜单 accelerator。
3. **TextArea 的 Enter 被吃掉**：多行编辑器无条件把 Enter 变 `\n`，Ctrl/Cmd+Enter 也一样；没有 `on_submit`。单行 TextInput 的 Enter 可以经转换后的 `Element::on_key` 捕获，但 builder 没直接暴露。
4. **TextInput/TextArea 没有 getter，也没有 password/mask**：值由 app 持有；`on_input` 回传完整新字符串；外部改值会 `editor.sync` 重置 retained editor（并压入一个 undo 单元），光标/选区可能跳。
5. **`on_click` 会自动 focusable**：组合控件（tabs、菜单行）常需 `.focusable(false)` 把 tab stop 收回到容器；否则 Tab 会停到每个内部元素。
6. **没有 `.fg()`、没有 `Element::on_change`、没有 `Theme::with_accent`**：文字色用 `.color(...)`，换品牌色用 `Theme::from_accent(...)`，`on_change` 只存在于 Slider/Select 等 builder。
7. **透明窗口依赖 surface alpha mode**：上游 vello 固定 `CompositeAlphaMode::Auto`，Windows 可能解析成 `Opaque`。本项目 vendor patch 已改为优先 `PreMultiplied`、再 `PostMultiplied`，并把 clear alpha 置 0；但驱动不支持 alpha 时仍会不透明。
8. **窗口透明 ≠ 内容透明**：shell 只把 clear color 的 alpha 置 0。若 view 根节点 `.bg(theme.bg)`（不透明），整个窗口仍是不透明。要半透明就画 `.bg(theme.bg.with_alpha(0.85))`。
9. **`position` 是物理像素，Windows 上是 outer 位置**：logical 坐标要先乘 monitor scale factor；Windows 的 (x,y) 指标题栏外框左上角，不是内容区。
10. **`skip_taskbar` 只在 Windows 生效**：其他平台字段存在但被忽略；`always_on_top` 在 Wayland/Web 上 winit 明确标注 unsupported，在 X11/Windows/macOS 可用，也可能被 fullscreen 覆盖。
11. **多窗口与 subscriptions 是 native only**：web runner 忽略 `App::windows` 和 `App::subscriptions`；`menu` 只有 macOS 接原生菜单。
12. **虚拟列表限制**：行内不支持 overlay；必须给稳定 `.id`；变高列表首帧用估算高度，滚到未测量区域会自校正（可能轻微跳动）；`count` 上限 10,000,000。
13. **overlay 不能放进 generated content**：`virtual_rows` / `responsive` 生成的子树不在声明树里，里面的 `overlay(...)` 会被静默跳过。overlay 要挂在生成内容之外。
14. **live window 没有 backdrop blur**：单 pass swapchain 路径只画玻璃的半透明 tint；真正的 backdrop blur 只在 headless 双 pass `render_plan` 里有。不要把窗口里的 glass 当截图回归基准。
15. **headless alpha 是 premultiplied**：保存 PNG 前要 un-premultiply；否则透明边缘发黑。
16. **无 GPU 就没有 headless**：`Headless::new` 找不到 compute adapter 会 `ShellError::NoDevice`；Windows WARP 在 vello GPU compute 下可能崩溃，需 `FENESTRA_CPU=1`。
17. **`render_element` 会 panic**：无 adapter/渲染失败直接 panic；CI 里用 `try_render_element` 系列把错误当值处理。
18. **每帧全量重建**：无 diff；`view()` 要便宜。大列表用虚拟化，昂贵计算放 `update`/`Cmd`，不要放 `view`。
19. **`Sub::every` 周期最小 1ms**；`Cmd` 自喂链上限 10,000 条，超过会停并打 stderr。`Cmd::future` 不能直接用 tokio reactor。
20. **AccessKit 只在 native**；web 还在等上游 adapter。headless 可用 `Frame::access_tree()` / `Harness::query` 做无障碍断言。
21. **markdown 的限制**：裸 URL autolink 不支持；代码高亮是简化 lexer；图片只出 alt 文本。
22. **`#[non_exhaustive]`**：`Surface`、`SurfaceRadius`、`SurfaceFill` 标了 `#[non_exhaustive]`，下游 match 必须带 `_` 分支；`SurfaceBundle` 是普通 struct，`Material` 没有标。新增变体不会算 breaking，但也意味着不能穷举。
23. **`Theme::validate_contrast` 可能失败**：自定义 accent/duotone 可能低于 APCA floor；发布前跑一下。
24. **`Weight` 只有 400/500/600**；没有 Bold。需要更粗只能注册自带字体到 `FamilyRole`。
25. **`Theme` 的 `with_text_scale` clamp 到 0.5..=3.0**；`with_corner_smoothing` clamp 0..=1。

---

## 12. 本项目 vendored shell patch

目录：`E:\NeoSonia\vendor\fenestra-shell`（上游 0.41.0 全量复制 + 修改）。

根 `Cargo.toml` 只加了一处：

```toml
[patch.crates-io]
fenestra-shell = { path = "vendor/fenestra-shell" }
```

新增字段与 builder：

```rust
WindowOptions::titled("NeoSonia")
    .with_always_on_top(true)
    .with_decorations(false)
    .with_transparent(true)
    .with_position(100.0, 80.0)  // 物理像素
    .with_skip_taskbar(true)
```

实现要点：

- `always_on_top`：`WindowAttributes::with_window_level(WindowLevel::AlwaysOnTop)`（winit 跨平台；Windows 上由 winit 转成 topmost）。
- `decorations`：`.with_decorations(false)`。
- `transparent`：`.with_transparent(true)`；`activate()` 里重配 `surface.config.alpha_mode` 为 `PreMultiplied`（不支持则 `PostMultiplied`，再退到驱动支持的第一个）；`present()` 里把 `RenderParams.base_color` 的 alpha 置 0。
- `position`：`.with_position(Position::Physical(PhysicalPosition::new(x, y)))`。
- `skip_taskbar`：`#[cfg(windows)]` 下 `WindowAttributesExtWindows::with_skip_taskbar`。
- 其余 API（`run_app` / `run_scene` / `run_static` / `WindowOptions::titled` / 所有旧字段与方法）保持不变，`version = "0.41.0"` 未改。

详细逐行说明见 `E:\NeoSonia\vendor\fenestra-shell\CHANGES-NEOSONIA.md`。

---

## 附：按需求点的快速索引

| 需求 | 位置 |
| --- | --- |
| `App` trait 完整定义、`type Msg`、`init` Proxy | 1.1、1.3 |
| `subscriptions`/`Sub`/`Cmd` 效果层 | 7.2、7.3 |
| `theme()` / `on_key` 钩子 | 1.1、4.6、6.3、6.4 |
| `run` / `run_app` / `WindowOptions` 全字段 | 2.1、2.2 |
| `Element` 构造函数与链式方法真实签名 | 3.1、3.2 |
| `SP1..SP6`、`R_*`、`TextSize`、`Weight`、`Color`、`Theme`、`fenestra-looks` | 4 |
| TextInput 读/写/回车/聚焦 | 5.2 |
| 长列表滚动/虚拟化 | 5.13 |
| 键盘事件/自定义快捷键/全局按键 | 6 |
| Proxy 线程 → 消息 → 重绘 | 1.3、7.4 |
| `fenestra-markdown` | 8 |
| `image_rgba8` | 9 |
| headless `render_element` / `render_app` | 10 |
| 坑列表 | 11 |
| vendored patch | 12、`CHANGES-NEOSONIA.md` |
