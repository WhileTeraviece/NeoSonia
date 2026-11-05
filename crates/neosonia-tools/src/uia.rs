//! UI Automation 工具：读取其它程序的控件树并模拟点击（Windows 真实现）。
//!
//! 本模块只依赖 `neosonia-core` 的契约类型和本 crate 的依赖，不引用其它工具模块，
//! 避免和 tools 队友的文件互相阻塞。对模型暴露 8 个工具：
//!
//! - `ui_window_list` 枚举顶层窗口
//! - `ui_focus_window` 把窗口切到前台
//! - `ui_tree`        读取控件树（BFS，带深度/节点数/超时限制）
//! - `ui_click`       点击控件（InvokePattern 优先，退化到 SetFocus + 鼠标点击中心）
//! - `ui_set_value`   给输入控件写值（ValuePattern 优先，退化到 Ctrl+A 覆盖输入）
//! - `ui_invoke_pattern` 调用控件的高级 UIA 模式（invoke/toggle/select/expand/collapse）
//! - `ui_act`         复合动作（focus → tree → 定位 → click/type/set_value/... 一条命令完成）
//! - `ui_drag`        按 UIA 坐标 / 元素名做鼠标拖拽（按下 → 分步移动 → 抬起）
//!
//! Windows 实现基于 `windows` 0.58 的 `Win32_UI_Accessibility`。每次调用都会在
//! 独立的工作线程里 `CoInitializeEx(MTA)`，RAII 在 Drop 里 `CoUninitialize`。
//! 其它平台（cfg(unix)）给出明确的降级实现与错误提示，不 panic。
//!
//! 编码约定：零 emoji；用户可见文案中文；错误一律 `ToolOutcome::err`。

use std::sync::Arc;

use async_trait::async_trait;
use neosonia_core::model::{ToolCtx, ToolOutcome, ToolSpec};
use neosonia_core::registry::{Tool, ToolRegistry};
use serde_json::{Value, json};

// ============================================================
// 参数解析（模型经常把数字写成字符串，全部容错）
// ============================================================

fn arg_str(args: &Value, key: &str) -> Option<String> {
    match args.get(key) {
        Some(Value::String(s)) => {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        }
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(Value::Bool(b)) => Some(b.to_string()),
        _ => None,
    }
}

fn arg_usize(args: &Value, key: &str) -> Option<usize> {
    match args.get(key) {
        Some(Value::Number(n)) => n
            .as_u64()
            .map(|v| v as usize)
            .or_else(|| n.as_f64().map(|v| v.max(0.0) as usize)),
        Some(Value::String(s)) => s.trim().parse::<u64>().ok().map(|v| v as usize),
        _ => None,
    }
}

/// 按字符截断，超出时在尾部标记。
fn limit_chars(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let total = s.chars().count();
    if total <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str(&format!("\n... [已截断，原文 {total} 字符] ..."));
    out
}

/// 大小写不敏感的子串包含（name_filter / 窗口标题匹配用）。
fn contains_ci(haystack: &str, needle: &str) -> bool {
    let needle = needle.trim();
    if needle.is_empty() {
        return true;
    }
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

// ============================================================
// 窗口标识解析（纯逻辑，可单测；Windows 实现使用它）
// ============================================================

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
enum WindowSpec {
    /// 前台窗口。
    Foreground,
    /// 显式窗口句柄。
    Hwnd(isize),
    /// 标题子串。
    Title(String),
}

/// 解析 window 参数：
/// - 缺省 / `foreground`（大小写不敏感）=> 前台窗口
/// - `0x1A2B` 或纯十进制 => 窗口句柄
/// - 其它 => 标题子串
#[allow(dead_code)]
fn classify_window_spec(spec: Option<&str>) -> WindowSpec {
    let Some(raw) = spec else {
        return WindowSpec::Foreground;
    };
    let t = raw.trim();
    if t.is_empty() || t.eq_ignore_ascii_case("foreground") || t.eq_ignore_ascii_case("active") {
        return WindowSpec::Foreground;
    }
    if let Some(h) = parse_hwnd_spec(t) {
        return WindowSpec::Hwnd(h);
    }
    WindowSpec::Title(t.to_string())
}

fn parse_hwnd_spec(t: &str) -> Option<isize> {
    let t = t.trim();
    let (neg, digits) = if let Some(rest) = t.strip_prefix('-') {
        (true, rest)
    } else {
        (false, t)
    };
    let value = if let Some(hex) = digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        i64::from_str_radix(hex, 16).ok()?
    } else if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) && digits.len() <= 18
    {
        digits.parse::<i64>().ok()?
    } else {
        return None;
    };
    let value = if neg { -value } else { value };
    Some(value as isize)
}

// ============================================================
// UIA 控件类型 -> 可读名字
// ============================================================

/// UIA control type id 映射成稳定的英文名字（与微软 UIAutomationClient.h 一致）。
/// 模型按这些名字过滤/点击，所以名字必须稳定，不能本地化。
#[allow(dead_code)]
fn control_type_name(id: u32) -> &'static str {
    match id {
        0 => "Unknown",
        50000 => "Button",
        50001 => "Calendar",
        50002 => "CheckBox",
        50003 => "ComboBox",
        50004 => "Edit",
        50005 => "Hyperlink",
        50006 => "Image",
        50007 => "ListItem",
        50008 => "List",
        50009 => "Menu",
        50010 => "MenuBar",
        50011 => "MenuItem",
        50012 => "ProgressBar",
        50013 => "RadioButton",
        50014 => "ScrollBar",
        50015 => "Slider",
        50016 => "Spinner",
        50017 => "StatusBar",
        50018 => "Tab",
        50019 => "TabItem",
        50020 => "Text",
        50021 => "ToolBar",
        50022 => "ToolTip",
        50023 => "Tree",
        50024 => "TreeItem",
        50025 => "Custom",
        50026 => "Group",
        50027 => "Thumb",
        50028 => "DataGrid",
        50029 => "DataItem",
        50030 => "Document",
        50031 => "SplitButton",
        50032 => "Window",
        50033 => "Pane",
        50034 => "Header",
        50035 => "HeaderItem",
        50036 => "Table",
        50037 => "TitleBar",
        50038 => "Separator",
        50039 => "SemanticZoom",
        _ => "Unknown",
    }
}

// ============================================================
// 复合动作 / 拖拽端点的纯逻辑解析（跨平台，可单测）
// ============================================================

/// `ui_act` 支持的动作。顺序即文档里的顺序。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UiAction {
    Click,
    Type,
    SetValue,
    Invoke,
    Toggle,
    Expand,
    Select,
    Scroll,
}

impl UiAction {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "click" => Some(UiAction::Click),
            "type" => Some(UiAction::Type),
            "set_value" | "setvalue" | "set" => Some(UiAction::SetValue),
            "invoke" => Some(UiAction::Invoke),
            "toggle" => Some(UiAction::Toggle),
            "expand" => Some(UiAction::Expand),
            "select" => Some(UiAction::Select),
            "scroll" => Some(UiAction::Scroll),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            UiAction::Click => "click",
            UiAction::Type => "type",
            UiAction::SetValue => "set_value",
            UiAction::Invoke => "invoke",
            UiAction::Toggle => "toggle",
            UiAction::Expand => "expand",
            UiAction::Select => "select",
            UiAction::Scroll => "scroll",
        }
    }

    /// 这些动作必须带 value（要写入/输入的文本）。
    fn needs_value(self) -> bool {
        matches!(self, UiAction::Type | UiAction::SetValue)
    }
}

/// `ui_drag` 的一个端点：显式坐标或某个控件名字（取元素中心）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum PointSpec {
    Xy(i32, i32),
    Name(String),
}

/// 解析 `{x,y}` / `{name}` 形式的端点；同时容忍 `[x,y]` 数组和 `"x,y"` 字符串。
fn parse_point_spec(value: Option<&Value>) -> Result<PointSpec, String> {
    let Some(v) = value else {
        return Err("缺少端点坐标：请给 {\"x\":..,\"y\":..} 或 {\"name\":\"控件名\"}。".into());
    };
    match v {
        Value::Object(map) => {
            if let Some(name) = map.get("name").and_then(Value::as_str) {
                let name = name.trim();
                if !name.is_empty() {
                    return Ok(PointSpec::Name(name.to_string()));
                }
            }
            let x = map.get("x").and_then(Value::as_i64);
            let y = map.get("y").and_then(Value::as_i64);
            match (x, y) {
                (Some(x), Some(y)) => Ok(PointSpec::Xy(x as i32, y as i32)),
                _ => Err(
                    "端点对象需要 {x,y}（整数）或 {name}（控件可访问名）；不要只给其中一个坐标。"
                        .into(),
                ),
            }
        }
        Value::Array(items) if items.len() == 2 => {
            let x = items[0].as_i64();
            let y = items[1].as_i64();
            match (x, y) {
                (Some(x), Some(y)) => Ok(PointSpec::Xy(x as i32, y as i32)),
                _ => Err("端点数组需要 [x, y] 两个整数。".into()),
            }
        }
        Value::String(s) => {
            let t = s.trim();
            if let Some((x, y)) = t.split_once(',') {
                if let (Ok(x), Ok(y)) = (x.trim().parse::<i32>(), y.trim().parse::<i32>()) {
                    return Ok(PointSpec::Xy(x, y));
                }
            }
            if t.is_empty() {
                Err("端点字符串不能为空。".into())
            } else {
                Ok(PointSpec::Name(t.to_string()))
            }
        }
        _ => Err("端点格式不支持：请给 {\"x\":..,\"y\":..} 或 {\"name\":\"控件名\"}。".into()),
    }
}

/// `ui_act` 的 scroll 方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrollDir {
    Up,
    Down,
}

/// scroll 方向来源：`value`（"up"/"down"/"上"/"下"）、`keys` 数组、缺省向下。
fn parse_scroll_dir(value: Option<&str>, keys: Option<&Value>) -> ScrollDir {
    let mut candidates: Vec<String> = Vec::new();
    if let Some(v) = value {
        candidates.push(v.trim().to_ascii_lowercase());
    }
    if let Some(Value::Array(items)) = keys {
        for item in items {
            if let Some(s) = item.as_str() {
                candidates.push(s.trim().to_ascii_lowercase());
            }
        }
    }
    for c in candidates {
        if matches!(
            c.as_str(),
            "up" | "upward" | "upwards" | "pageup" | "上" | "向上"
        ) {
            return ScrollDir::Up;
        }
        if matches!(
            c.as_str(),
            "down" | "downward" | "downwards" | "pagedown" | "下" | "向下"
        ) {
            return ScrollDir::Down;
        }
    }
    ScrollDir::Down
}

/// 从控件树文本里提取“N 个字符 / 行 N”这类状态（主要给真机测试用）。
#[allow(dead_code)]
fn find_count_before_unit(content: &str, unit: &str) -> Option<u32> {
    for line in content.lines() {
        let Some(pos) = line.find(unit) else {
            continue;
        };
        let head = &line[..pos];
        // UIA 的状态文本常写成 `5 个字符`（数字和单位之间有空格），先把紧挨单位的空白跳过。
        let mut digits: Vec<char> = Vec::new();
        for c in head.chars().rev() {
            if c.is_whitespace() && digits.is_empty() {
                continue;
            }
            if c.is_ascii_digit() {
                digits.push(c);
            } else {
                break;
            }
        }
        if digits.is_empty() {
            continue;
        }
        let text: String = digits.into_iter().rev().collect();
        if let Ok(n) = text.parse::<u32>() {
            return Some(n);
        }
    }
    None
}

// ============================================================
// 工具规格
// ============================================================

const UI_TREE_DESC: &str = "读取指定窗口的 UI Automation 控件树，用于发现其它程序（含 Electron / Chromium 应用）里的按钮、输入框等控件。\
默认读前台窗口、深度 8、最多 400 个节点（与 ui_click / ui_act 的默认遍历参数一致）；\
返回每个节点的 index（0 基 BFS 遍历序号）、role（控件类型）、name（可访问名）、value（当前值）、\
bounds（屏幕 x,y,w,h）、enabled、patterns（可用模式）。index 可直接给 ui_click / ui_act / ui_drag 用。\
注意：name_filter / role_filter 只过滤显示、不改变 index；遍历慢时会超时并返回已收集的部分节点；\
Electron/Chromium 应用在后台/最小化时只暴露很浅的树，先 ui_focus_window 激活再读效果更好。\
什么时候不该用：只想点某个网页元素且有调试端口时优先用 cdp_click（更快更稳）。";

const UI_ACT_DESC: &str = "复合动作：一条命令完成“激活窗口 → 遍历 UIA 树 → 按 name/role/index 定位 → 执行动作”，\
失败会自动重新遍历重试 1 次，并返回实际使用的策略。\
action 支持：click（InvokePattern 优先，否则鼠标点中心）、type（优先 ValuePattern.SetValue，否则聚焦后用键盘输入 value）、\
set_value（ValuePattern，否则 Ctrl+A 覆盖输入）、invoke / toggle / expand / select（对应 UIA 模式）、\
scroll（ScrollPattern 上下滚动，value 或 keys 传 up/down，默认向下；不支持时滚轮兜底）。\
定位：window + index（ui_tree 的 0 基 BFS 序号）或 window + name（大小写不敏感子串）+ 可选 role。\
什么时候用：需要“先找到控件再操作”的一步到位场景；只想点一下时 ui_click 更直接，网页元素用 cdp_click。";

const UI_DRAG_DESC: &str = "鼠标拖拽（按下 → 分步移动 → 抬起），用于拖选文本、拖文件、拖动滚动条/滑块。\
from / to 各支持 {x,y}（屏幕坐标）或 {name}（目标窗口里该控件中心，自动用 UIA 取坐标），\
两端可以混用，例如 from={name:\"文本编辑器\"}、to={x:900,y:400}。\
可选 duration_ms 控制拖拽时长（默认 400ms，分步移动更像人手）。\
需要 ToolsSettings::allow_input_simulation=true；只支持左键。什么时候不该用：网页内拖拽请用 cdp_eval 里的 JS。";

const UI_WINDOW_LIST_DESC: &str = "列出当前桌面上可见的顶层窗口（标题、类名、pid、hwnd），用于挑选要操作的目标窗口。\
filter 对标题/类名做大小写不敏感的子串过滤；limit 默认 30、上限 200。\
什么时候用：不知道窗口标题或句柄时先列窗口；拿到 hwnd 后可传给 ui_tree / ui_click 的 window 参数。";

const UI_FOCUS_WINDOW_DESC: &str = "把指定窗口切到前台并恢复显示（ShowWindow + SetForegroundWindow，失败时用 AttachThreadInput 重试）。\
用 title（标题子串）/ pid / hwnd 三选一定位，hwnd 优先、其次 pid、最后标题。\
什么时候用：在点击后台窗口的控件之前先激活它。注意 Windows 前台锁可能阻止切换，返回结果里会给出当前前台窗口是否已变成目标窗口。";

const UI_CLICK_DESC: &str = "点击窗口里的控件。优先用 UIA 的 InvokePattern（不需要鼠标、对 Electron/Chromium 按钮有效）；\
不支持 InvokePattern 时退化为 SetFocus + 把鼠标移到元素中心点左键点击（需要 ToolsSettings::allow_input_simulation=true）。\
定位方式：window + index（index 来自 ui_tree，0 基遍历序号；可再传 depth/max_nodes 与 ui_tree 保持一致），\
或 window + name（可访问名，大小写不敏感子串）+ 可选 role（控件类型，如 Button / MenuItem）。\
返回实际使用的点击方式和被点击控件的信息。什么时候不该用：网页元素优先用 cdp_click。";

const UI_SET_VALUE_DESC: &str = "给输入控件写文本。优先 ValuePattern.SetValue（不改焦点、最快）；\
不支持 ValuePattern 时聚焦元素、点击中心、Ctrl+A 全选后覆盖输入（需要 ToolsSettings::allow_input_simulation=true）。\
定位方式：window + index（来自 ui_tree）或 window + name + role。返回实际使用的方式。";

const UI_INVOKE_PATTERN_DESC: &str = "触发控件的高级 UI Automation 模式：invoke（按钮默认动作）、toggle（复选框/开关）、\
select（列表项/标签页）、expand（树节点/菜单/下拉展开）、collapse（折叠）。\
用 window + index 定位（index 来自 ui_tree，可传 depth/max_nodes 保持一致），pattern 必填。\
什么时候用：ui_click 只能说“点击”，而展开下拉、勾选复选框、切换标签页用对应模式更可靠。";

fn spec_ui_window_list() -> ToolSpec {
    ToolSpec::new(
        "ui_window_list",
        UI_WINDOW_LIST_DESC,
        json!({
            "type": "object",
            "properties": {
                "filter": {"type": "string", "description": "按标题/类名过滤的子串（大小写不敏感，可省略）"},
                "limit": {"type": "integer", "minimum": 1, "maximum": 200, "description": "最多返回多少个窗口，默认 30"}
            },
            "required": []
        }),
    )
}

fn spec_ui_focus_window() -> ToolSpec {
    ToolSpec::new(
        "ui_focus_window",
        UI_FOCUS_WINDOW_DESC,
        json!({
            "type": "object",
            "properties": {
                "title": {"type": "string", "description": "窗口标题子串（大小写不敏感）"},
                "pid": {"type": "integer", "description": "目标进程 pid"},
                "hwnd": {"type": ["integer", "string"], "description": "窗口句柄（十进制或 0x 十六进制字符串）"}
            },
            "required": []
        }),
    )
}

fn spec_ui_tree() -> ToolSpec {
    ToolSpec::new(
        "ui_tree",
        UI_TREE_DESC,
        json!({
            "type": "object",
            "properties": {
                "window": {"type": "string", "description": "目标窗口：foreground（默认前台）/ 标题子串 / 窗口句柄（十进制或 0x 十六进制）"},
                "depth": {"type": "integer", "minimum": 1, "maximum": 12, "description": "最大遍历深度，默认 8（与 ui_click 默认一致；Chromium 地址栏/工具栏在 7 层左右）"},
                "max_nodes": {"type": "integer", "minimum": 1, "maximum": 2000, "description": "最多返回节点数，默认 400（与 ui_click 默认一致）"},
                "name_filter": {"type": "string", "description": "只显示可访问名包含该子串的节点"},
                "role_filter": {"type": "string", "description": "只显示该控件类型的节点（如 Button / Edit / MenuItem）"},
                "timeout_ms": {"type": "integer", "minimum": 1000, "maximum": 60000, "description": "遍历超时毫秒数，默认 15000；超时返回已收集节点"}
            },
            "required": []
        }),
    )
}

fn spec_ui_click() -> ToolSpec {
    ToolSpec::new(
        "ui_click",
        UI_CLICK_DESC,
        json!({
            "type": "object",
            "properties": {
                "window": {"type": "string", "description": "目标窗口：foreground（默认）/ 标题子串 / 窗口句柄"},
                "index": {"type": "integer", "minimum": 0, "description": "ui_tree 返回的节点 index（0 基 BFS 序号）"},
                "name": {"type": "string", "description": "按可访问名查找（大小写不敏感子串）；与 index 二选一，index 优先"},
                "role": {"type": "string", "description": "配合 name 过滤控件类型，如 Button / MenuItem / ListItem"},
                "depth": {"type": "integer", "minimum": 1, "maximum": 12, "description": "遍历深度，默认 8（按 index 点击时与 ui_tree 保持一致的遍历顺序即可）"},
                "max_nodes": {"type": "integer", "minimum": 1, "maximum": 2000, "description": "遍历最多访问节点数，默认 400"}
            },
            "required": []
        }),
    )
}

fn spec_ui_set_value() -> ToolSpec {
    ToolSpec::new(
        "ui_set_value",
        UI_SET_VALUE_DESC,
        json!({
            "type": "object",
            "properties": {
                "window": {"type": "string", "description": "目标窗口：foreground（默认）/ 标题子串 / 窗口句柄"},
                "index": {"type": "integer", "minimum": 0, "description": "ui_tree 返回的节点 index"},
                "name": {"type": "string", "description": "按可访问名查找（与 index 二选一，index 优先）"},
                "role": {"type": "string", "description": "配合 name 过滤控件类型，如 Edit / ComboBox"},
                "value": {"type": "string", "description": "要写入的文本"},
                "depth": {"type": "integer", "minimum": 1, "maximum": 12, "description": "遍历深度，默认 8"},
                "max_nodes": {"type": "integer", "minimum": 1, "maximum": 2000, "description": "遍历最多访问节点数，默认 400"}
            },
            "required": ["value"]
        }),
    )
}

fn spec_ui_invoke_pattern() -> ToolSpec {
    ToolSpec::new(
        "ui_invoke_pattern",
        UI_INVOKE_PATTERN_DESC,
        json!({
            "type": "object",
            "properties": {
                "window": {"type": "string", "description": "目标窗口：foreground（默认）/ 标题子串 / 窗口句柄"},
                "index": {"type": "integer", "minimum": 0, "description": "ui_tree 返回的节点 index"},
                "name": {"type": "string", "description": "按可访问名查找（与 index 二选一，index 优先）"},
                "role": {"type": "string", "description": "配合 name 过滤控件类型"},
                "pattern": {"type": "string", "enum": ["invoke", "toggle", "select", "expand", "collapse"], "description": "要触发的 UIA 模式"},
                "depth": {"type": "integer", "minimum": 1, "maximum": 12, "description": "遍历深度，默认 8"},
                "max_nodes": {"type": "integer", "minimum": 1, "maximum": 2000, "description": "遍历最多访问节点数，默认 400"}
            },
            "required": ["pattern"]
        }),
    )
}

fn spec_ui_act() -> ToolSpec {
    ToolSpec::new(
        "ui_act",
        UI_ACT_DESC,
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["click", "type", "set_value", "invoke", "toggle", "expand", "select", "scroll"],
                    "description": "要执行的动作"
                },
                "window": {"type": "string", "description": "目标窗口：foreground（默认）/ 标题子串 / 窗口句柄"},
                "index": {"type": "integer", "minimum": 0, "description": "ui_tree 返回的节点 index（0 基 BFS 序号）"},
                "name": {"type": "string", "description": "按可访问名查找（大小写不敏感子串）；与 index 二选一，index 优先"},
                "role": {"type": "string", "description": "配合 name 过滤控件类型，如 Button / Edit / MenuItem"},
                "value": {"type": "string", "description": "type/set_value 要写入的文本；scroll 时可用 up/down 指定方向"},
                "keys": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "可选按键提示；scroll 时传 [\"up\"]/[\"down\"]，其它动作忽略"
                },
                "depth": {"type": "integer", "minimum": 1, "maximum": 12, "description": "遍历深度，默认 8"},
                "max_nodes": {"type": "integer", "minimum": 1, "maximum": 2000, "description": "遍历最多访问节点数，默认 400"}
            },
            "required": ["action"]
        }),
    )
}

fn spec_ui_drag() -> ToolSpec {
    ToolSpec::new(
        "ui_drag",
        UI_DRAG_DESC,
        json!({
            "type": "object",
            "properties": {
                "from": {
                    "type": "object",
                    "description": "起点：{\"x\":整数,\"y\":整数} 或 {\"name\":\"控件可访问名\"}",
                    "properties": {"x": {"type": "integer"}, "y": {"type": "integer"}, "name": {"type": "string"}}
                },
                "to": {
                    "type": "object",
                    "description": "终点：{\"x\":整数,\"y\":整数} 或 {\"name\":\"控件可访问名\"}",
                    "properties": {"x": {"type": "integer"}, "y": {"type": "integer"}, "name": {"type": "string"}}
                },
                "window": {"type": "string", "description": "用于解析 {name} 端点的目标窗口：foreground（默认）/ 标题子串 / 窗口句柄"},
                "duration_ms": {"type": "integer", "minimum": 100, "maximum": 5000, "description": "拖拽总时长，默认 400"},
                "button": {"type": "string", "enum": ["left"], "description": "鼠标按键，目前只支持 left"}
            },
            "required": ["from", "to"]
        }),
    )
}

// ============================================================
// 工具实现（平台差异全部委托给 platform 模块）
// ============================================================

struct UiWindowListTool;
struct UiFocusWindowTool;
struct UiTreeTool;
struct UiClickTool;
struct UiSetValueTool;
struct UiInvokePatternTool;
struct UiActTool;
struct UiDragTool;

#[async_trait]
impl Tool for UiWindowListTool {
    fn spec(&self) -> ToolSpec {
        spec_ui_window_list()
    }
    async fn call(&self, ctx: &ToolCtx, args: Value) -> ToolOutcome {
        platform::ui_window_list(ctx, args).await
    }
}

#[async_trait]
impl Tool for UiFocusWindowTool {
    fn spec(&self) -> ToolSpec {
        spec_ui_focus_window()
    }
    async fn call(&self, ctx: &ToolCtx, args: Value) -> ToolOutcome {
        platform::ui_focus_window(ctx, args).await
    }
}

#[async_trait]
impl Tool for UiTreeTool {
    fn spec(&self) -> ToolSpec {
        spec_ui_tree()
    }
    async fn call(&self, ctx: &ToolCtx, args: Value) -> ToolOutcome {
        platform::ui_tree(ctx, args).await
    }
}

#[async_trait]
impl Tool for UiClickTool {
    fn spec(&self) -> ToolSpec {
        spec_ui_click()
    }
    async fn call(&self, ctx: &ToolCtx, args: Value) -> ToolOutcome {
        platform::ui_click(ctx, args).await
    }
}

#[async_trait]
impl Tool for UiSetValueTool {
    fn spec(&self) -> ToolSpec {
        spec_ui_set_value()
    }
    async fn call(&self, ctx: &ToolCtx, args: Value) -> ToolOutcome {
        platform::ui_set_value(ctx, args).await
    }
}

#[async_trait]
impl Tool for UiInvokePatternTool {
    fn spec(&self) -> ToolSpec {
        spec_ui_invoke_pattern()
    }
    async fn call(&self, ctx: &ToolCtx, args: Value) -> ToolOutcome {
        platform::ui_invoke_pattern(ctx, args).await
    }
}

#[async_trait]
impl Tool for UiActTool {
    fn spec(&self) -> ToolSpec {
        spec_ui_act()
    }
    async fn call(&self, ctx: &ToolCtx, args: Value) -> ToolOutcome {
        platform::ui_act(ctx, args).await
    }
}

#[async_trait]
impl Tool for UiDragTool {
    fn spec(&self) -> ToolSpec {
        spec_ui_drag()
    }
    async fn call(&self, ctx: &ToolCtx, args: Value) -> ToolOutcome {
        platform::ui_drag(ctx, args).await
    }
}

/// 把这些工具注册进注册表（由 `neosonia_tools::register_all` / `tool_catalog` 调用）。
pub fn register(reg: &ToolRegistry) {
    reg.register(Arc::new(UiWindowListTool));
    reg.register(Arc::new(UiFocusWindowTool));
    reg.register(Arc::new(UiTreeTool));
    reg.register(Arc::new(UiClickTool));
    reg.register(Arc::new(UiSetValueTool));
    reg.register(Arc::new(UiInvokePatternTool));
    reg.register(Arc::new(UiActTool));
    reg.register(Arc::new(UiDragTool));
}

// ============================================================
// Windows 实现
// ============================================================

#[cfg(windows)]
mod platform {
    use std::collections::VecDeque;
    use std::time::{Duration, Instant};

    use neosonia_core::model::{ToolCtx, ToolOutcome};
    use serde_json::Value;

    use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT};
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
        CoUninitialize,
    };
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows::Win32::UI::Accessibility::{
        CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationExpandCollapsePattern,
        IUIAutomationInvokePattern, IUIAutomationLegacyIAccessiblePattern,
        IUIAutomationScrollItemPattern, IUIAutomationScrollPattern,
        IUIAutomationSelectionItemPattern, IUIAutomationTogglePattern, IUIAutomationValuePattern,
        ScrollAmount_LargeDecrement, ScrollAmount_LargeIncrement, ScrollAmount_NoAmount,
        UIA_ExpandCollapsePatternId, UIA_InvokePatternId, UIA_LegacyIAccessiblePatternId,
        UIA_ScrollItemPatternId, UIA_ScrollPatternId, UIA_SelectionItemPatternId,
        UIA_TogglePatternId, UIA_ValuePatternId,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, EnumWindows, GetClassNameW, GetForegroundWindow, GetWindowTextW,
        GetWindowThreadProcessId, IsWindow, IsWindowVisible, SW_RESTORE, SetForegroundWindow,
        ShowWindow, WNDENUMPROC,
    };
    use windows::core::BSTR;

    use super::{
        PointSpec, ScrollDir, UiAction, WindowSpec, arg_str, arg_usize, classify_window_spec,
        contains_ci, control_type_name, limit_chars, parse_point_spec, parse_scroll_dir,
    };

    /// COM 的 RPC_E_CHANGED_MODE：当前线程已经以别的套间模型初始化过（例如 STA）。
    const RPC_E_CHANGED_MODE: i32 = -2147417850i32;
    const DEFAULT_TREE_TIMEOUT_MS: u64 = 15_000;
    const SCAN_TIMEOUT_MS: u64 = 12_000;

    // --------------------------------------------------------
    // COM 会话（RAII）
    // --------------------------------------------------------

    /// 一次 UIA 调用的 COM 会话：负责 CoInitializeEx / CoUninitialize 配对。
    /// 必须在创建它的线程上 Drop（本模块所有调用都跑在同一个 spawn_blocking 线程里）。
    struct UiaSession {
        automation: IUIAutomation,
        should_uninit: bool,
    }

    impl UiaSession {
        fn new() -> Result<Self, String> {
            unsafe {
                let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
                let should_uninit = if hr.0 < 0 {
                    if hr.0 == RPC_E_CHANGED_MODE {
                        // 线程已经由别的代码初始化过（STA），复用，不对应 CoUninitialize。
                        false
                    } else {
                        return Err(format!(
                            "COM 初始化失败（CoInitializeEx）：{}（0x{:08X}）",
                            hr.message(),
                            hr.0 as u32
                        ));
                    }
                } else {
                    true
                };

                match CoCreateInstance::<_, IUIAutomation>(
                    &CUIAutomation,
                    None::<&windows::core::IUnknown>,
                    CLSCTX_INPROC_SERVER,
                ) {
                    Ok(automation) => Ok(Self {
                        automation,
                        should_uninit,
                    }),
                    Err(e) => {
                        if should_uninit {
                            CoUninitialize();
                        }
                        Err(format!(
                            "创建 IUIAutomation 失败：{}。请确认系统 UI Automation 服务正常（Windows 11 默认可用）。",
                            e.message()
                        ))
                    }
                }
            }
        }
    }

    impl Drop for UiaSession {
        fn drop(&mut self) {
            if self.should_uninit {
                unsafe { CoUninitialize() };
            }
        }
    }

    // --------------------------------------------------------
    // 顶层窗口枚举
    // --------------------------------------------------------

    #[derive(Debug, Clone)]
    struct WindowEntry {
        hwnd: isize,
        title: String,
        class: String,
        pid: u32,
        visible: bool,
    }

    fn wide_string(buf: &[u16], len: i32) -> String {
        let n = len.max(0) as usize;
        let n = n.min(buf.len());
        String::from_utf16_lossy(&buf[..n]).trim().to_string()
    }

    unsafe fn read_window(hwnd: HWND) -> WindowEntry {
        let mut title_buf = [0u16; 1024];
        let title_len = unsafe { GetWindowTextW(hwnd, &mut title_buf) };
        let title = wide_string(&title_buf, title_len);

        let mut class_buf = [0u16; 512];
        let class_len = unsafe { GetClassNameW(hwnd, &mut class_buf) };
        let class = wide_string(&class_buf, class_len);

        let mut pid = 0u32;
        let pid_ptr: *mut u32 = &mut pid;
        unsafe { GetWindowThreadProcessId(hwnd, Some(pid_ptr)) };
        let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();

        WindowEntry {
            hwnd: hwnd.0 as isize,
            title,
            class,
            pid,
            visible,
        }
    }

    unsafe extern "system" fn enum_windows_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let list = unsafe { &mut *(lparam.0 as *mut Vec<WindowEntry>) };
        let entry = unsafe { read_window(hwnd) };
        list.push(entry);
        BOOL(1)
    }

    /// 枚举所有顶层窗口（按 EnumWindows 的 Z 序）。
    fn enum_top_windows() -> Vec<WindowEntry> {
        let mut list: Vec<WindowEntry> = Vec::new();
        let ptr: *mut Vec<WindowEntry> = &mut list;
        let callback: WNDENUMPROC = Some(enum_windows_cb);
        let _ = unsafe { EnumWindows(callback, LPARAM(ptr as isize)) };
        list
    }

    // --------------------------------------------------------
    // 窗口解析
    // --------------------------------------------------------

    fn find_window_by_title(title: &str) -> Option<WindowEntry> {
        let windows = enum_top_windows();
        let mut fallback: Option<WindowEntry> = None;
        for w in windows {
            if !contains_ci(&w.title, title) || w.title.is_empty() {
                continue;
            }
            if w.visible {
                return Some(w);
            }
            if fallback.is_none() {
                fallback = Some(w);
            }
        }
        fallback
    }

    fn resolve_window(spec: Option<&str>) -> Result<WindowEntry, String> {
        match classify_window_spec(spec) {
            WindowSpec::Foreground => {
                let hwnd = unsafe { GetForegroundWindow() };
                if hwnd.is_invalid() {
                    return Err("当前没有前台窗口。可先用 ui_window_list 查看窗口，再用 window 参数指定标题或 hwnd。".into());
                }
                Ok(unsafe { read_window(hwnd) })
            }
            WindowSpec::Hwnd(h) => {
                let hwnd = HWND(h as *mut std::ffi::c_void);
                let valid = unsafe { IsWindow(hwnd) }.as_bool();
                if !valid {
                    return Err(format!(
                        "窗口句柄 {h}（0x{:X}）不是有效窗口。请先用 ui_window_list 获取正确的 hwnd。",
                        h as usize
                    ));
                }
                Ok(unsafe { read_window(hwnd) })
            }
            WindowSpec::Title(t) => find_window_by_title(&t).ok_or_else(|| {
                format!(
                    "没有找到标题包含“{t}”的可见窗口。可先用 ui_window_list 列出窗口，或改用 hwnd 参数。"
                )
            }),
        }
    }

    fn resolve_window_by_args(args: &Value) -> Result<WindowEntry, String> {
        resolve_window(arg_str(args, "window").as_deref())
    }

    fn window_hwnd(w: &WindowEntry) -> HWND {
        HWND(w.hwnd as *mut std::ffi::c_void)
    }

    // --------------------------------------------------------
    // 元素访问辅助
    // --------------------------------------------------------

    fn bstr_string(b: windows::core::Result<BSTR>) -> String {
        match b {
            Ok(s) => s.to_string(),
            Err(_) => String::new(),
        }
    }

    fn rect_tuple(r: RECT) -> (i32, i32, i32, i32) {
        let w = (r.right - r.left).max(0);
        let h = (r.bottom - r.top).max(0);
        (r.left, r.top, w, h)
    }

    #[derive(Debug, Clone)]
    struct NodeInfo {
        index: usize,
        depth: usize,
        role: String,
        name: String,
        value: String,
        bounds: Option<(i32, i32, i32, i32)>,
        enabled: bool,
        patterns: Vec<&'static str>,
    }

    fn quote_field(s: &str) -> String {
        let cleaned: String = s
            .chars()
            .map(|c| {
                if c == '\n' || c == '\r' || c == '\t' {
                    ' '
                } else {
                    c
                }
            })
            .collect();
        let cleaned = limit_chars(&cleaned, 160);
        format!("\"{}\"", cleaned.replace('"', "'"))
    }

    fn format_node(n: &NodeInfo) -> String {
        let mut line = format!(
            "#{} {} name={} bounds={} enabled={}",
            n.index,
            n.role,
            quote_field(&n.name),
            match n.bounds {
                Some((x, y, w, h)) => format!("({x},{y},{w},{h})"),
                None => "?".to_string(),
            },
            n.enabled
        );
        if !n.value.is_empty() {
            line.push_str(&format!(" value={}", quote_field(&n.value)));
        }
        if !n.patterns.is_empty() {
            line.push_str(&format!(" patterns=[{}]", n.patterns.join(",")));
        }
        if n.depth > 0 {
            line.push_str(&format!(" depth={}", n.depth));
        }
        line
    }

    /// 读取单个节点的全部展示信息。任何属性读取失败都退化为默认值，不让一个坏控件毁掉整棵树。
    fn collect_node_info(el: &IUIAutomationElement, index: usize, depth: usize) -> NodeInfo {
        unsafe {
            let control_id = el.CurrentControlType().map(|id| id.0 as u32).unwrap_or(0);
            let role = control_type_name(control_id).to_string();
            let name = bstr_string(el.CurrentName());
            let bounds = el.CurrentBoundingRectangle().ok().map(rect_tuple);
            let enabled = el.CurrentIsEnabled().map(|b| b.as_bool()).unwrap_or(false);

            let value =
                match el.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) {
                    Ok(vp) => bstr_string(vp.CurrentValue()),
                    Err(_) => String::new(),
                };

            let mut patterns: Vec<&'static str> = Vec::new();
            if el
                .GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
                .is_ok()
            {
                patterns.push("invoke");
            }
            if el
                .GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId)
                .is_ok()
            {
                patterns.push("toggle");
            }
            if el
                .GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(
                    UIA_SelectionItemPatternId,
                )
                .is_ok()
            {
                patterns.push("select");
            }
            if el
                .GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(
                    UIA_ExpandCollapsePatternId,
                )
                .is_ok()
            {
                patterns.push("expand");
            }
            if !value.is_empty() {
                patterns.push("value");
            }

            NodeInfo {
                index,
                depth,
                role,
                name,
                value,
                bounds,
                enabled,
                patterns,
            }
        }
    }

    // --------------------------------------------------------
    // BFS 遍历
    // --------------------------------------------------------

    enum WalkStop {
        None,
        MaxNodes,
        Timeout,
        Cancelled,
    }

    /// 广度优先遍历控件树。返回访问到的元素、是否超时、是否被取消。
    /// 遍历过程中周期性检查 `ctx.is_cancelled()`，并且有硬性 deadline，
    /// 保证 UIA 变慢/卡住时工具还能返回已收集的部分结果。
    fn walk_elements(
        session: &UiaSession,
        root: &IUIAutomationElement,
        max_depth: usize,
        max_nodes: usize,
        deadline: Instant,
        ctx: &ToolCtx,
    ) -> (Vec<IUIAutomationElement>, WalkStop) {
        let mut out: Vec<IUIAutomationElement> = Vec::new();
        let walker = match unsafe { session.automation.ControlViewWalker() } {
            Ok(w) => w,
            Err(_) => {
                out.push(root.clone());
                return (out, WalkStop::None);
            }
        };

        let mut queue: VecDeque<(IUIAutomationElement, usize)> = VecDeque::new();
        queue.push_back((root.clone(), 0usize));
        let mut stop = WalkStop::None;

        while let Some((el, depth)) = queue.pop_front() {
            if out.len() >= max_nodes {
                stop = WalkStop::MaxNodes;
                break;
            }
            if Instant::now() >= deadline {
                stop = WalkStop::Timeout;
                break;
            }
            if ctx.is_cancelled() {
                stop = WalkStop::Cancelled;
                break;
            }

            out.push(el.clone());

            if depth >= max_depth {
                continue;
            }
            let mut child = unsafe { walker.GetFirstChildElement(&el) }.ok();
            while let Some(c) = child {
                if out.len() + queue.len() >= max_nodes.saturating_mul(4).max(max_nodes) {
                    // 队列过长说明树异常大，提前截断，避免内存和耗时失控。
                    stop = WalkStop::MaxNodes;
                    break;
                }
                queue.push_back((c.clone(), depth + 1));
                child = unsafe { walker.GetNextSiblingElement(&c) }.ok();
            }
            if !matches!(stop, WalkStop::None) {
                break;
            }
        }
        (out, stop)
    }

    /// 在工作线程里跑 COM 调用（UIA 是同步阻塞接口，不能卡住 async 运行时）。
    async fn spawn_blocking_tool<F>(f: F) -> ToolOutcome
    where
        F: FnOnce() -> ToolOutcome + Send + 'static,
    {
        match tokio::task::spawn_blocking(f).await {
            Ok(outcome) => outcome,
            Err(e) => ToolOutcome::err(format!("UI Automation 工作线程异常：{e}")),
        }
    }

    // --------------------------------------------------------
    // 鼠标 / 键盘模拟（enigo）
    // --------------------------------------------------------

    fn mouse_click_at(x: i32, y: i32) -> Result<(), String> {
        use enigo::{Button, Coordinate, Direction, Enigo, Mouse, Settings};

        let mut enigo = Enigo::new(&Settings::default())
            .map_err(|e| format!("初始化鼠标模拟失败（enigo）：{e}。请确认桌面会话可交互。"))?;
        enigo
            .move_mouse(x, y, Coordinate::Abs)
            .map_err(|e| format!("移动鼠标到 ({x},{y}) 失败：{e}"))?;
        std::thread::sleep(Duration::from_millis(60));
        enigo
            .button(Button::Left, Direction::Click)
            .map_err(|e| format!("鼠标左键点击失败：{e}"))?;
        Ok(())
    }

    fn type_text_overwrite(text: &str) -> Result<(), String> {
        use enigo::{Direction, Enigo, Key, Keyboard, Settings};

        let mut enigo = Enigo::new(&Settings::default())
            .map_err(|e| format!("初始化键盘模拟失败（enigo）：{e}。请确认桌面会话可交互。"))?;
        enigo
            .key(Key::Control, Direction::Press)
            .map_err(|e| format!("按下 Ctrl 失败：{e}"))?;
        enigo
            .key(Key::Unicode('a'), Direction::Click)
            .map_err(|e| format!("按下 A 失败：{e}"))?;
        enigo
            .key(Key::Control, Direction::Release)
            .map_err(|e| format!("松开 Ctrl 失败：{e}"))?;
        enigo.text(text).map_err(|e| format!("输入文本失败：{e}"))?;
        Ok(())
    }

    /// 在当前焦点控件里直接输入文本（不做 Ctrl+A，用于 ui_act 的 type 回退）。
    fn type_plain_text(text: &str) -> Result<(), String> {
        use enigo::{Enigo, Keyboard, Settings};

        let mut enigo = Enigo::new(&Settings::default())
            .map_err(|e| format!("初始化键盘模拟失败（enigo）：{e}。请确认桌面会话可交互。"))?;
        enigo.text(text).map_err(|e| format!("输入文本失败：{e}"))
    }

    /// 鼠标从 from 拖到 to：按下 → 分步移动 → 抬起。分步移动更像人手，
    /// 也能让支持拖选的应用产生正确的中间事件。
    fn mouse_drag_path(from: (i32, i32), to: (i32, i32), duration: Duration) -> Result<(), String> {
        use enigo::{Button, Coordinate, Direction, Enigo, Mouse, Settings};

        let steps = ((duration.as_millis() as i64 / 16).clamp(10, 60)) as u32;
        let mut enigo = Enigo::new(&Settings::default())
            .map_err(|e| format!("初始化鼠标模拟失败（enigo）：{e}。请确认桌面会话可交互。"))?;
        enigo
            .move_mouse(from.0, from.1, Coordinate::Abs)
            .map_err(|e| format!("移动鼠标到起点 ({},{}) 失败：{e}", from.0, from.1))?;
        std::thread::sleep(Duration::from_millis(80));
        enigo
            .button(Button::Left, Direction::Press)
            .map_err(|e| format!("鼠标左键按下失败：{e}"))?;
        std::thread::sleep(Duration::from_millis(60));

        let step_sleep = duration / steps.max(1);
        for i in 1..=steps {
            let t = i as f64 / steps as f64;
            // 轻微 ease-in-out，让起点/终点附近更稳。
            let eased = t * t * (3.0 - 2.0 * t);
            let x = from.0 + ((to.0 - from.0) as f64 * eased).round() as i32;
            let y = from.1 + ((to.1 - from.1) as f64 * eased).round() as i32;
            enigo
                .move_mouse(x, y, Coordinate::Abs)
                .map_err(|e| format!("拖拽移动失败（第 {i} 步）：{e}"))?;
            std::thread::sleep(step_sleep.max(Duration::from_millis(4)));
        }

        std::thread::sleep(Duration::from_millis(60));
        enigo
            .button(Button::Left, Direction::Release)
            .map_err(|e| format!("鼠标左键抬起失败：{e}"))?;
        Ok(())
    }

    /// 在指定点滚动鼠标滚轮（正数向上、负数向下）。
    fn mouse_scroll_at(x: i32, y: i32, amount: i32) -> Result<(), String> {
        use enigo::{Axis, Coordinate, Enigo, Mouse, Settings};

        let mut enigo = Enigo::new(&Settings::default())
            .map_err(|e| format!("初始化鼠标模拟失败（enigo）：{e}。请确认桌面会话可交互。"))?;
        enigo
            .move_mouse(x, y, Coordinate::Abs)
            .map_err(|e| format!("移动鼠标到 ({x},{y}) 失败：{e}"))?;
        enigo
            .scroll(amount, Axis::Vertical)
            .map_err(|e| format!("鼠标滚轮滚动失败：{e}"))?;
        Ok(())
    }

    /// 尽量把窗口切到前台。Windows 的前台锁可能让 SetForegroundWindow 失败；
    /// 这里先给当前线程建消息队列（否则 AttachThreadInput 会失败），再 AttachThreadInput
    /// 到前台线程重试，最后用 SwitchToThisWindow 兜底，并核对 GetForegroundWindow 结果。
    fn force_foreground(hwnd: HWND) -> bool {
        use windows::Win32::UI::WindowsAndMessaging::{
            MSG, PM_NOREMOVE, PeekMessageW, SwitchToThisWindow,
        };

        unsafe {
            if !IsWindow(hwnd).as_bool() {
                return false;
            }
            // AttachThreadInput 要求调用线程有输入消息队列（spawn_blocking 线程默认没有）。
            let mut msg = MSG::default();
            let _ = PeekMessageW(&mut msg, HWND::default(), 0, 0, PM_NOREMOVE);

            let _ = ShowWindow(hwnd, SW_RESTORE);
            if SetForegroundWindow(hwnd).as_bool() && GetForegroundWindow() == hwnd {
                return true;
            }
            if GetForegroundWindow() == hwnd {
                return true;
            }

            // 前台锁：临时 attach 到当前前台线程，再抢一次。
            let fg = GetForegroundWindow();
            let fg_tid = GetWindowThreadProcessId(fg, None);
            let cur_tid = GetCurrentThreadId();
            let attached = fg_tid != 0
                && fg_tid != cur_tid
                && AttachThreadInput(fg_tid, cur_tid, true).as_bool();
            let _ = SetForegroundWindow(hwnd);
            let _ = BringWindowToTop(hwnd);
            if attached {
                let _ = AttachThreadInput(fg_tid, cur_tid, false);
            }
            if GetForegroundWindow() == hwnd {
                return true;
            }

            // 最后一招：先按一下 Alt 解除前台锁，再 SwitchToThisWindow 兜底。
            press_alt_unlock();
            let _ = SetForegroundWindow(hwnd);
            SwitchToThisWindow(hwnd, true);
            std::thread::sleep(Duration::from_millis(120));
            GetForegroundWindow() == hwnd
        }
    }

    /// 模拟按一下 Alt：Windows 的前台锁在收到一次用户输入后往往会被解除，
    /// 这是切换前台窗口的经典兜底（资源管理器/Alt+Tab 逻辑同理）。
    fn press_alt_unlock() {
        use enigo::{Direction, Enigo, Key, Keyboard, Settings};

        if let Ok(mut enigo) = Enigo::new(&Settings::default()) {
            let _ = enigo.key(Key::Alt, Direction::Press);
            let _ = enigo.key(Key::Alt, Direction::Release);
        }
    }

    fn input_simulation_allowed(ctx: &ToolCtx) -> bool {
        ctx.settings().tools.allow_input_simulation
    }

    // --------------------------------------------------------
    // 元素定位（index / name + role）
    // --------------------------------------------------------

    /// click / set_value / invoke_pattern 的定位参数。
    struct Locator {
        window: Option<String>,
        index: Option<usize>,
        name: Option<String>,
        role: Option<String>,
        depth: usize,
        max_nodes: usize,
    }

    impl Locator {
        fn parse(args: &Value) -> Result<Self, String> {
            let locator = Self {
                window: arg_str(args, "window"),
                index: arg_usize(args, "index"),
                name: arg_str(args, "name"),
                role: arg_str(args, "role"),
                depth: arg_usize(args, "depth").unwrap_or(8).clamp(1, 12),
                max_nodes: arg_usize(args, "max_nodes").unwrap_or(400).clamp(1, 2000),
            };
            if locator.index.is_none() && locator.name.is_none() {
                return Err(
                    "缺少定位参数：请给 index（来自 ui_tree 的节点序号）或 name（可访问名）。"
                        .into(),
                );
            }
            Ok(locator)
        }
    }

    fn role_matches(role_arg: &str, control_id: u32) -> bool {
        let role_name = control_type_name(control_id);
        if role_arg.eq_ignore_ascii_case(role_name) {
            return true;
        }
        if let Ok(id) = role_arg.trim().parse::<u32>() {
            return id == control_id;
        }
        // 允许 "button"/"Button"/"按钮" 这类宽松写法：英文名包含即算匹配。
        role_arg
            .to_ascii_lowercase()
            .contains(&role_name.to_ascii_lowercase())
            || role_name
                .to_ascii_lowercase()
                .contains(&role_arg.to_ascii_lowercase())
    }

    /// 扫描已访问的元素，找到用户指的那个。返回元素与匹配位置。
    fn locate_element(
        elements: &[IUIAutomationElement],
        locator: &Locator,
        ctx: &ToolCtx,
    ) -> Result<(IUIAutomationElement, String), String> {
        if let Some(idx) = locator.index {
            if idx >= elements.len() {
                return Err(format!(
                    "index={idx} 超出范围：当前只访问到 {} 个节点。请先调用 ui_tree 获取 index（或增大 depth / max_nodes）。",
                    elements.len()
                ));
            }
            return Ok((elements[idx].clone(), format!("index={idx}")));
        }

        let name_needle = locator.name.clone().unwrap_or_default();
        let deadline = Instant::now() + Duration::from_millis(SCAN_TIMEOUT_MS);
        for (idx, el) in elements.iter().enumerate() {
            if Instant::now() >= deadline {
                return Err(format!(
                    "按名字查找超时（已检查 {idx} 个节点）。建议改用 ui_tree 拿 index 再点击，或缩小 depth / 提高 timeout。"
                ));
            }
            if ctx.is_cancelled() {
                return Err("操作已取消".to_string());
            }
            let name = unsafe { bstr_string(el.CurrentName()) };
            if name.is_empty() || !contains_ci(&name, &name_needle) {
                continue;
            }
            if let Some(role_arg) = locator.role.as_deref() {
                let control_id = unsafe { el.CurrentControlType() }
                    .map(|id| id.0 as u32)
                    .unwrap_or(0);
                if !role_matches(role_arg, control_id) {
                    continue;
                }
            }
            return Ok((
                el.clone(),
                format!("name={} index={idx}", quote_field(&name)),
            ));
        }
        Err(format!(
            "没有找到可访问名包含“{name_needle}”{} 的控件。建议先调用 ui_tree（可用 name_filter 缩小范围）确认。",
            match locator.role.as_deref() {
                Some(r) => format!("且类型为 {r}"),
                None => String::new(),
            }
        ))
    }

    /// 打开窗口、遍历、定位，一条龙。
    fn locate_in_window(
        locator: &Locator,
        ctx: &ToolCtx,
    ) -> Result<(WindowEntry, IUIAutomationElement, String), String> {
        let window = resolve_window(locator.window.as_deref())?;
        let session = UiaSession::new()?;
        let hwnd = window_hwnd(&window);
        let root = unsafe { session.automation.ElementFromHandle(hwnd) }.map_err(|e| {
            format!(
                "无法从窗口句柄获取 UI Automation 元素：{}。该窗口可能不支持 UIA（如部分 UWP/受保护窗口）。",
                e.message()
            )
        })?;
        let max_nodes = if locator.index.is_some() {
            locator.max_nodes.max(200)
        } else {
            locator.max_nodes
        };
        let deadline = Instant::now() + Duration::from_millis(SCAN_TIMEOUT_MS);
        let (elements, _stop) =
            walk_elements(&session, &root, locator.depth, max_nodes, deadline, ctx);
        let (element, how) = locate_element(&elements, locator, ctx)?;
        Ok((window, element, how))
    }

    fn element_bounds(el: &IUIAutomationElement) -> Result<(i32, i32, i32, i32), String> {
        match unsafe { el.CurrentBoundingRectangle() } {
            Ok(r) => Ok(rect_tuple(r)),
            Err(e) => Err(format!("读取元素屏幕位置失败：{}", e.message())),
        }
    }

    /// 元素中心点在屏幕上的物理像素坐标；元素不可见/尺寸为 0 时返回错误。
    fn element_center(el: &IUIAutomationElement) -> Result<(i32, i32), String> {
        let (x, y, w, h) = element_bounds(el)?;
        if w <= 0 || h <= 0 {
            return Err("元素没有可点击区域（宽或高为 0，可能不可见或已折叠）。可先展开父节点或换一个控件。".into());
        }
        Ok((x + w / 2, y + h / 2))
    }

    fn element_label(el: &IUIAutomationElement) -> String {
        let role = unsafe { el.CurrentControlType() }
            .map(|id| control_type_name(id.0 as u32).to_string())
            .unwrap_or_else(|_| "Unknown".into());
        let name = unsafe { bstr_string(el.CurrentName()) };
        format!("{} {}", role, quote_field(&name))
    }

    // --------------------------------------------------------
    // 工具：ui_window_list
    // --------------------------------------------------------

    pub async fn ui_window_list(ctx: &ToolCtx, args: Value) -> ToolOutcome {
        let ctx = ctx.clone();
        spawn_blocking_tool(move || ui_window_list_impl(&ctx, &args)).await
    }

    fn ui_window_list_impl(ctx: &ToolCtx, args: &Value) -> ToolOutcome {
        if ctx.is_cancelled() {
            return ToolOutcome::err("操作已取消");
        }
        let filter = arg_str(args, "filter");
        let limit = arg_usize(args, "limit").unwrap_or(30).clamp(1, 200);

        let windows: Vec<WindowEntry> = enum_top_windows()
            .into_iter()
            .filter(|w| w.visible && !w.title.is_empty())
            .filter(|w| match filter.as_deref() {
                Some(f) => contains_ci(&w.title, f) || contains_ci(&w.class, f),
                None => true,
            })
            .collect();

        let total = windows.len();
        let mut lines: Vec<String> = Vec::new();
        lines.push(format!(
            "[ok] 可见顶层窗口 {total} 个{}",
            match filter.as_deref() {
                Some(f) => format!("（filter=“{f}”）"),
                None => String::new(),
            }
        ));
        for (i, w) in windows.iter().take(limit).enumerate() {
            lines.push(format!(
                "#{} hwnd=0x{:X} pid={} class={} title={}",
                i,
                w.hwnd as usize,
                w.pid,
                quote_field(&w.class),
                quote_field(&w.title)
            ));
        }
        if total > limit {
            lines.push(format!(
                "（只显示前 {limit} 个，共 {total} 个；可用 filter 缩小范围）"
            ));
        }
        if total == 0 {
            lines.push("没有匹配的可见窗口。请确认程序已打开且不是最小化到托盘。".into());
        }
        ToolOutcome::ok(limit_chars(&lines.join("\n"), 18_000))
            .with_display(format!("列出 {total} 个窗口"))
    }

    // --------------------------------------------------------
    // 工具：ui_focus_window
    // --------------------------------------------------------

    pub async fn ui_focus_window(ctx: &ToolCtx, args: Value) -> ToolOutcome {
        let ctx = ctx.clone();
        spawn_blocking_tool(move || ui_focus_window_impl(&ctx, &args)).await
    }

    fn ui_focus_window_impl(ctx: &ToolCtx, args: &Value) -> ToolOutcome {
        if ctx.is_cancelled() {
            return ToolOutcome::err("操作已取消");
        }
        let hwnd_arg = arg_str(args, "hwnd").or_else(|| {
            args.get("hwnd")
                .and_then(|v| v.as_i64())
                .map(|v| v.to_string())
        });
        let pid_arg = arg_usize(args, "pid");
        let title_arg = arg_str(args, "title");

        let found = if let Some(raw) = hwnd_arg.as_deref() {
            match classify_window_spec(Some(raw)) {
                WindowSpec::Hwnd(h) => {
                    let hwnd = HWND(h as *mut std::ffi::c_void);
                    if unsafe { IsWindow(hwnd) }.as_bool() {
                        Some(unsafe { read_window(hwnd) })
                    } else {
                        return ToolOutcome::err(format!(
                            "hwnd={raw} 不是有效窗口。请先用 ui_window_list 确认句柄。"
                        ));
                    }
                }
                WindowSpec::Title(t) => find_window_by_title(&t),
                WindowSpec::Foreground => None,
            }
        } else if let Some(pid) = pid_arg {
            enum_top_windows()
                .into_iter()
                .filter(|w| w.pid as usize == pid)
                .find(|w| w.visible && !w.title.is_empty())
        } else if let Some(title) = title_arg.as_deref() {
            find_window_by_title(title)
        } else {
            return ToolOutcome::err("请提供 title、pid 或 hwnd 之一来定位窗口。");
        };

        let Some(window) = found else {
            return ToolOutcome::err(
                "没有找到匹配的窗口。请先用 ui_window_list 列出窗口，确认 title / pid / hwnd。",
            );
        };

        let ok = force_foreground(window_hwnd(&window));
        let content = format!(
            "{} 窗口 hwnd=0x{:X} pid={} title={} class={}\n{}",
            if ok { "[ok]" } else { "[warn]" },
            window.hwnd as usize,
            window.pid,
            quote_field(&window.title),
            quote_field(&window.class),
            if ok {
                "已切到前台。".to_string()
            } else {
                "尝试切到前台但未能确认成功（Windows 前台锁限制）。可稍等再试，或先点击一下该窗口。"
                    .to_string()
            }
        );
        if ok {
            ToolOutcome::ok(content).with_display(format!("激活窗口 {}", window.title))
        } else {
            ToolOutcome::err(content).with_display("激活窗口失败")
        }
    }

    // --------------------------------------------------------
    // 工具：ui_tree
    // --------------------------------------------------------

    pub async fn ui_tree(ctx: &ToolCtx, args: Value) -> ToolOutcome {
        let ctx = ctx.clone();
        spawn_blocking_tool(move || ui_tree_impl(&ctx, &args)).await
    }

    fn ui_tree_impl(ctx: &ToolCtx, args: &Value) -> ToolOutcome {
        if ctx.is_cancelled() {
            return ToolOutcome::err("操作已取消");
        }

        // 与 ui_click / ui_act 的默认遍历参数保持一致，避免按 index 操作时对不上。
        let depth = arg_usize(args, "depth").unwrap_or(8).clamp(1, 12);
        let max_nodes = arg_usize(args, "max_nodes").unwrap_or(400).clamp(1, 2000);
        let timeout_ms = arg_usize(args, "timeout_ms")
            .unwrap_or(DEFAULT_TREE_TIMEOUT_MS as usize)
            .clamp(1_000, 60_000) as u64;
        let name_filter = arg_str(args, "name_filter");
        let role_filter = arg_str(args, "role_filter");

        let window = match resolve_window_by_args(args) {
            Ok(w) => w,
            Err(e) => return ToolOutcome::err(e),
        };
        let session = match UiaSession::new() {
            Ok(s) => s,
            Err(e) => return ToolOutcome::err(e),
        };
        let root = match unsafe { session.automation.ElementFromHandle(window_hwnd(&window)) } {
            Ok(e) => e,
            Err(e) => {
                return ToolOutcome::err(format!(
                    "无法从窗口句柄获取 UI Automation 元素：{}。该窗口可能不支持 UIA（如部分 UWP/受保护窗口）。",
                    e.message()
                ));
            }
        };

        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let (elements, stop) = walk_elements(&session, &root, depth, max_nodes, deadline, ctx);

        let mut lines: Vec<String> = Vec::new();
        lines.push(format!(
            "[ok] 窗口 hwnd=0x{:X} pid={} title={} class={}",
            window.hwnd as usize,
            window.pid,
            quote_field(&window.title),
            quote_field(&window.class)
        ));
        lines.push(format!(
            "遍历参数：depth={depth} max_nodes={max_nodes} timeout_ms={timeout_ms}"
        ));

        let mut shown = 0usize;
        let mut info_truncated = false;
        for (idx, el) in elements.iter().enumerate() {
            // 先只读名字/类型做过滤，命中后才读 bounds / patterns 等昂贵属性，避免白跑 UIA 调用。
            let (name, control_id) = unsafe {
                (
                    bstr_string(el.CurrentName()),
                    el.CurrentControlType().map(|id| id.0 as u32).unwrap_or(0),
                )
            };
            if let Some(nf) = name_filter.as_deref() {
                if !contains_ci(&name, nf) {
                    continue;
                }
            }
            if let Some(rf) = role_filter.as_deref() {
                if !role_matches(rf, control_id) {
                    continue;
                }
            }
            if Instant::now() >= deadline {
                info_truncated = true;
                break;
            }
            let info = collect_node_info(el, idx, 0);
            lines.push(format_node(&info));
            shown += 1;
            if shown >= max_nodes {
                break;
            }
        }

        match stop {
            WalkStop::Timeout => lines.push(format!(
                "[warn] 遍历超时（{timeout_ms}ms），以上为已收集的部分节点。可减小 depth / max_nodes 或稍后重试。"
            )),
            WalkStop::MaxNodes => {
                lines.push(format!(
                    "[warn] 已达到节点上限（max_nodes={max_nodes}）或队列过大，结果可能不完整。"
                ))
            }
            WalkStop::Cancelled => {
                lines.push("[cancelled] 操作已取消，以上为已收集的部分节点。".into());
            }
            WalkStop::None => {}
        }
        if info_truncated && !matches!(stop, WalkStop::Timeout) {
            lines.push(format!(
                "[warn] 读取节点属性超时（{timeout_ms}ms），只列出了前 {shown} 个节点的详细信息。"
            ));
        }
        if elements.is_empty() {
            lines.push(
                "没有读到任何控件（窗口可能最小化、无 UIA 提供者，或应用未启用无障碍支持）。"
                    .into(),
            );
        }
        lines.push("提示：节点 index 是 0 基 BFS 遍历序号，可直接传给 ui_click / ui_set_value / ui_invoke_pattern。".into());

        let content = limit_chars(&lines.join("\n"), 18_000);
        let summary = format!("控件树 {shown} 个节点");
        if matches!(stop, WalkStop::Cancelled) {
            ToolOutcome::err(content).with_display("控件树读取已取消")
        } else {
            ToolOutcome::ok(content).with_display(summary)
        }
    }

    // --------------------------------------------------------
    // 共享动作执行（ui_click / ui_set_value / ui_act 复用）
    // --------------------------------------------------------

    /// 点击元素：InvokePattern → LegacyIAccessible → SetFocus + 鼠标点中心。
    fn click_element(
        ctx: &ToolCtx,
        window: &WindowEntry,
        element: &IUIAutomationElement,
    ) -> Result<String, String> {
        if let Ok(pattern) = unsafe {
            element.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
        } {
            if (unsafe { pattern.Invoke() }).is_ok() {
                return Ok("InvokePattern".to_string());
            }
        }
        if let Ok(pattern) = unsafe {
            element.GetCurrentPatternAs::<IUIAutomationLegacyIAccessiblePattern>(
                UIA_LegacyIAccessiblePatternId,
            )
        } {
            if unsafe { pattern.DoDefaultAction() }.is_ok() {
                return Ok("LegacyIAccessible.DoDefaultAction".to_string());
            }
        }
        if !input_simulation_allowed(ctx) {
            return Err(
                "该控件不支持 InvokePattern / DoDefaultAction，需要鼠标点击兜底，但输入模拟已被禁用。\n修复：在设置里打开 tools.allow_input_simulation，或改用该控件支持的模式（看 ui_tree 的 patterns 字段）。"
                    .into(),
            );
        }
        let (x, y) = element_center(element)?;
        let focused = unsafe { element.SetFocus() }.is_ok();
        if !focused && !force_foreground(window_hwnd(window)) {
            return Err(
                "无法把目标窗口切到前台，元素也可能不可聚焦。请先手动点击该窗口或调用 ui_focus_window，然后重试。"
                    .into(),
            );
        }
        std::thread::sleep(Duration::from_millis(120));
        if ctx.is_cancelled() {
            return Err("操作已取消（未执行鼠标点击）".into());
        }
        mouse_click_at(x, y).map_err(|e| format!("鼠标点击兜底失败：{e}"))?;
        Ok(format!("鼠标点击元素中心 ({x},{y})，SetFocus={focused}"))
    }

    /// 给元素写值：ValuePattern.SetValue → 聚焦 + 点击中心 + Ctrl+A 覆盖输入。
    fn set_value_element(
        ctx: &ToolCtx,
        window: &WindowEntry,
        element: &IUIAutomationElement,
        value: &str,
    ) -> Result<String, String> {
        match unsafe {
            element.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
        } {
            Ok(pattern) => {
                let bstr = BSTR::from(value);
                match unsafe { pattern.SetValue(&bstr) } {
                    Ok(()) => return Ok("ValuePattern.SetValue".to_string()),
                    Err(e) => {
                        if !input_simulation_allowed(ctx) {
                            return Err(format!(
                                "ValuePattern.SetValue 失败：{}，且输入模拟已禁用无法兜底。\n修复：打开 tools.allow_input_simulation 后重试。",
                                e.message()
                            ));
                        }
                    }
                }
            }
            Err(_) => {
                if !input_simulation_allowed(ctx) {
                    return Err(
                        "控件不支持 ValuePattern，需要键盘输入兜底，但输入模拟已被禁用。\n修复：在设置里打开 tools.allow_input_simulation。"
                            .into(),
                    );
                }
            }
        }
        let (x, y) = element_center(element)?;
        let _ = unsafe { element.SetFocus() };
        if !force_foreground(window_hwnd(window)) {
            return Err(
                "无法把目标窗口切到前台，键盘输入可能落到别的窗口。请先手动激活目标窗口或调用 ui_focus_window。"
                    .into(),
            );
        }
        std::thread::sleep(Duration::from_millis(120));
        if ctx.is_cancelled() {
            return Err("操作已取消（未写入）".into());
        }
        mouse_click_at(x, y).map_err(|e| format!("点击输入框失败：{e}"))?;
        std::thread::sleep(Duration::from_millis(80));
        type_text_overwrite(value).map_err(|e| format!("输入文本失败：{e}"))?;
        Ok("SetFocus + 鼠标点击 + Ctrl+A 覆盖输入".to_string())
    }

    /// ui_act 的 type：编辑类控件优先 ValuePattern.SetValue，否则聚焦后用键盘直接输入
    /// （不 Ctrl+A，保留原有内容）。
    fn type_into_element(
        ctx: &ToolCtx,
        window: &WindowEntry,
        element: &IUIAutomationElement,
        text: &str,
    ) -> Result<String, String> {
        let control_id = unsafe { element.CurrentControlType() }
            .map(|id| id.0 as u32)
            .unwrap_or(0);
        let edit_like = matches!(control_id, 50004 | 50003 | 50030);
        if edit_like {
            if let Ok(pattern) = unsafe {
                element.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
            } {
                let bstr = BSTR::from(text);
                if (unsafe { pattern.SetValue(&bstr) }).is_ok() {
                    return Ok("ValuePattern.SetValue".to_string());
                }
            }
        }
        if !input_simulation_allowed(ctx) {
            return Err(
                "type 需要键盘/鼠标模拟，但输入模拟已被禁用。\n修复：在设置里打开 tools.allow_input_simulation。"
                    .into(),
            );
        }
        let focused = unsafe { element.SetFocus() }.is_ok();
        if !focused {
            let (x, y) = element_center(element)?;
            let _ = force_foreground(window_hwnd(window));
            std::thread::sleep(Duration::from_millis(100));
            mouse_click_at(x, y).map_err(|e| format!("点击输入控件失败：{e}"))?;
            std::thread::sleep(Duration::from_millis(80));
        }
        if ctx.is_cancelled() {
            return Err("操作已取消（未输入）".into());
        }
        type_plain_text(text).map_err(|e| format!("键盘输入失败：{e}"))?;
        Ok("SetFocus + enigo 键盘输入".to_string())
    }

    /// ui_act 的 scroll：ScrollPattern 上下滚动 → ScrollItemPattern 滚进视野 → 滚轮兜底。
    fn scroll_element(
        ctx: &ToolCtx,
        element: &IUIAutomationElement,
        dir: ScrollDir,
    ) -> Result<String, String> {
        if let Ok(pattern) = unsafe {
            element.GetCurrentPatternAs::<IUIAutomationScrollPattern>(UIA_ScrollPatternId)
        } {
            let vertical = match dir {
                ScrollDir::Up => ScrollAmount_LargeDecrement,
                ScrollDir::Down => ScrollAmount_LargeIncrement,
            };
            if (unsafe { pattern.Scroll(ScrollAmount_NoAmount, vertical) }).is_ok() {
                return Ok(format!(
                    "ScrollPattern（{}）",
                    match dir {
                        ScrollDir::Up => "向上",
                        ScrollDir::Down => "向下",
                    }
                ));
            }
        }
        if let Ok(pattern) = unsafe {
            element.GetCurrentPatternAs::<IUIAutomationScrollItemPattern>(UIA_ScrollItemPatternId)
        } {
            if (unsafe { pattern.ScrollIntoView() }).is_ok() {
                return Ok("ScrollItemPattern.ScrollIntoView".to_string());
            }
        }
        if !input_simulation_allowed(ctx) {
            return Err(
                "控件不支持 ScrollPattern，需要滚轮兜底，但输入模拟已被禁用。\n修复：在设置里打开 tools.allow_input_simulation。"
                    .into(),
            );
        }
        let (x, y) = element_center(element)?;
        let amount = match dir {
            ScrollDir::Up => 3,
            ScrollDir::Down => -3,
        };
        mouse_scroll_at(x, y, amount).map_err(|e| format!("滚轮滚动失败：{e}"))?;
        Ok("鼠标滚轮兜底".to_string())
    }

    /// 对元素执行一个 UIA 模式动作（invoke / toggle / expand / select）。
    fn pattern_action(element: &IUIAutomationElement, action: UiAction) -> Result<String, String> {
        match action {
            UiAction::Invoke => {
                let pattern = unsafe {
                    element.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
                }
                .map_err(|e| format!("控件不支持 InvokePattern：{}", e.message()))?;
                (unsafe { pattern.Invoke() })
                    .map_err(|e| format!("InvokePattern.Invoke 失败：{}", e.message()))?;
                Ok("InvokePattern.Invoke".to_string())
            }
            UiAction::Toggle => {
                let pattern = unsafe {
                    element.GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId)
                }
                .map_err(|e| format!("控件不支持 TogglePattern：{}", e.message()))?;
                (unsafe { pattern.Toggle() })
                    .map_err(|e| format!("TogglePattern.Toggle 失败：{}", e.message()))?;
                Ok("TogglePattern.Toggle".to_string())
            }
            UiAction::Expand => {
                let pattern = unsafe {
                    element.GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(
                        UIA_ExpandCollapsePatternId,
                    )
                }
                .map_err(|e| format!("控件不支持 ExpandCollapsePattern：{}", e.message()))?;
                match unsafe { pattern.Expand() } {
                    Ok(()) => Ok("ExpandCollapsePattern.Expand".to_string()),
                    Err(e) => {
                        // 有些控件已经展开时 Expand 会返回失败，尝试 Collapse 作为补偿。
                        (unsafe { pattern.Collapse() }).map_err(|_| {
                            format!("ExpandCollapsePattern 调用失败：{}", e.message())
                        })?;
                        Ok("ExpandCollapsePattern.Collapse（原状态已展开）".to_string())
                    }
                }
            }
            UiAction::Select => {
                let pattern = unsafe {
                    element.GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(
                        UIA_SelectionItemPatternId,
                    )
                }
                .map_err(|e| format!("控件不支持 SelectionItemPattern：{}", e.message()))?;
                (unsafe { pattern.Select() })
                    .map_err(|e| format!("SelectionItemPattern.Select 失败：{}", e.message()))?;
                Ok("SelectionItemPattern.Select".to_string())
            }
            other => Err(format!(
                "动作 {} 不是 UIA 模式动作（应为 invoke/toggle/expand/select）",
                other.as_str()
            )),
        }
    }

    fn apply_ui_action(
        ctx: &ToolCtx,
        window: &WindowEntry,
        element: &IUIAutomationElement,
        action: UiAction,
        value: Option<&str>,
        scroll_dir: ScrollDir,
    ) -> Result<String, String> {
        match action {
            UiAction::Click => click_element(ctx, window, element),
            UiAction::Type => type_into_element(ctx, window, element, value.unwrap_or("")),
            UiAction::SetValue => set_value_element(ctx, window, element, value.unwrap_or("")),
            UiAction::Invoke | UiAction::Toggle | UiAction::Expand | UiAction::Select => {
                pattern_action(element, action)
            }
            UiAction::Scroll => scroll_element(ctx, element, scroll_dir),
        }
    }

    /// 把 ui_drag 的端点解析成屏幕坐标：显式坐标直接用；{name} 用 UIA 找元素中心。
    fn resolve_point(
        ctx: &ToolCtx,
        window: &WindowEntry,
        spec: &PointSpec,
    ) -> Result<(i32, i32), String> {
        match spec {
            PointSpec::Xy(x, y) => Ok((*x, *y)),
            PointSpec::Name(name) => {
                let locator = Locator {
                    window: Some(format!("0x{:X}", window.hwnd as usize)),
                    index: None,
                    name: Some(name.clone()),
                    role: None,
                    depth: 8,
                    max_nodes: 400,
                };
                let (_, element, _how) = locate_in_window(&locator, ctx)?;
                element_center(&element)
            }
        }
    }

    fn describe_point(spec: &PointSpec) -> String {
        match spec {
            PointSpec::Xy(x, y) => format!("坐标({x},{y})"),
            PointSpec::Name(name) => format!("控件 {} 的中心", quote_field(name)),
        }
    }

    // --------------------------------------------------------
    // 工具：ui_act（复合动作）
    // --------------------------------------------------------

    pub async fn ui_act(ctx: &ToolCtx, args: Value) -> ToolOutcome {
        let ctx = ctx.clone();
        spawn_blocking_tool(move || ui_act_impl(&ctx, &args)).await
    }

    fn ui_act_impl(ctx: &ToolCtx, args: &Value) -> ToolOutcome {
        if ctx.is_cancelled() {
            return ToolOutcome::err("操作已取消");
        }
        let Some(action_raw) = arg_str(args, "action") else {
            return ToolOutcome::err(
                "缺少必填参数 action（click / type / set_value / invoke / toggle / expand / select / scroll）。",
            );
        };
        let Some(action) = UiAction::parse(&action_raw) else {
            return ToolOutcome::err(format!(
                "未知 action=“{action_raw}”。支持：click / type / set_value / invoke / toggle / expand / select / scroll。"
            ));
        };
        let value = arg_str(args, "value");
        if action.needs_value() && value.is_none() {
            return ToolOutcome::err(format!(
                "action={} 需要 value（要写入/输入的文本）。",
                action.as_str()
            ));
        }
        let scroll_dir = parse_scroll_dir(value.as_deref(), args.get("keys"));
        let locator = match Locator::parse(args) {
            Ok(l) => l,
            Err(e) => return ToolOutcome::err(e),
        };

        let mut last_error = String::new();
        // 最多两次：第一次直接做；失败后重新遍历树再试一次（界面可能刚刷新）。
        for attempt in 0..2u32 {
            if ctx.is_cancelled() {
                return ToolOutcome::err("操作已取消");
            }
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(350));
            }
            let window = match resolve_window(locator.window.as_deref()) {
                Ok(w) => w,
                Err(e) => {
                    last_error = e;
                    continue;
                }
            };
            let _ = force_foreground(window_hwnd(&window));
            std::thread::sleep(Duration::from_millis(if attempt == 0 { 120 } else { 250 }));

            let (window, element, how) = match locate_in_window(&locator, ctx) {
                Ok(v) => v,
                Err(e) => {
                    last_error = e;
                    continue;
                }
            };
            let label = element_label(&element);
            match apply_ui_action(ctx, &window, &element, action, value.as_deref(), scroll_dir) {
                Ok(strategy) => {
                    let mut extra = String::new();
                    if action == UiAction::Scroll {
                        extra.push_str(&format!(
                            "，方向：{}",
                            match scroll_dir {
                                ScrollDir::Up => "向上",
                                ScrollDir::Down => "向下",
                            }
                        ));
                    }
                    return ToolOutcome::ok(format!(
                        "[ok] ui_act {} 成功\n控件：{label}\n定位：{how}（第 {} 次尝试）\n窗口：{}\n策略：{strategy}{extra}",
                        action.as_str(),
                        attempt + 1,
                        quote_field(&window.title)
                    ))
                    .with_display(format!("ui_act {} 完成", action.as_str()));
                }
                Err(e) => last_error = e,
            }
        }

        ToolOutcome::err(format!(
            "[err] ui_act {} 失败（共尝试 2 次）：{last_error}\n建议：先 ui_tree 确认控件的 name/role/index 是否命中；网页元素优先用 cdp_click。",
            action.as_str()
        ))
        .with_display("ui_act 失败")
    }

    // --------------------------------------------------------
    // 工具：ui_drag
    // --------------------------------------------------------

    pub async fn ui_drag(ctx: &ToolCtx, args: Value) -> ToolOutcome {
        let ctx = ctx.clone();
        spawn_blocking_tool(move || ui_drag_impl(&ctx, &args)).await
    }

    fn ui_drag_impl(ctx: &ToolCtx, args: &Value) -> ToolOutcome {
        if ctx.is_cancelled() {
            return ToolOutcome::err("操作已取消");
        }
        if !input_simulation_allowed(ctx) {
            return ToolOutcome::err(
                "ui_drag 需要鼠标模拟，但输入模拟已被禁用。\n修复：在设置里打开 tools.allow_input_simulation。",
            );
        }
        let from = match parse_point_spec(args.get("from")) {
            Ok(p) => p,
            Err(e) => return ToolOutcome::err(format!("from 参数错误：{e}")),
        };
        let to = match parse_point_spec(args.get("to")) {
            Ok(p) => p,
            Err(e) => return ToolOutcome::err(format!("to 参数错误：{e}")),
        };
        let duration_ms = arg_usize(args, "duration_ms")
            .unwrap_or(400)
            .clamp(100, 5000) as u64;

        let window = match resolve_window(arg_str(args, "window").as_deref()) {
            Ok(w) => w,
            Err(e) => return ToolOutcome::err(e),
        };
        let _ = force_foreground(window_hwnd(&window));
        std::thread::sleep(Duration::from_millis(150));
        if ctx.is_cancelled() {
            return ToolOutcome::err("操作已取消");
        }

        let from_xy = match resolve_point(ctx, &window, &from) {
            Ok(p) => p,
            Err(e) => return ToolOutcome::err(format!("解析起点失败：{e}")),
        };
        let to_xy = match resolve_point(ctx, &window, &to) {
            Ok(p) => p,
            Err(e) => return ToolOutcome::err(format!("解析终点失败：{e}")),
        };
        if ctx.is_cancelled() {
            return ToolOutcome::err("操作已取消（未执行拖拽）");
        }

        if let Err(e) = mouse_drag_path(from_xy, to_xy, Duration::from_millis(duration_ms)) {
            return ToolOutcome::err(format!(
                "拖拽失败：{e}\n提示：确认坐标在可见屏幕范围内；端点用 {{name}} 时要求目标窗口在前台且控件可见。"
            ));
        }
        ToolOutcome::ok(format!(
            "[ok] 已拖拽鼠标：({},{}) → ({},{})，时长 {}ms（按下 → 分步移动 → 抬起）\n窗口：{}\n端点：{} / {}",
            from_xy.0,
            from_xy.1,
            to_xy.0,
            to_xy.1,
            duration_ms,
            quote_field(&window.title),
            describe_point(&from),
            describe_point(&to)
        ))
        .with_display(format!(
            "拖拽 ({},{})→({},{})",
            from_xy.0, from_xy.1, to_xy.0, to_xy.1
        ))
    }

    // --------------------------------------------------------
    // 工具：ui_click
    // --------------------------------------------------------

    pub async fn ui_click(ctx: &ToolCtx, args: Value) -> ToolOutcome {
        let ctx = ctx.clone();
        spawn_blocking_tool(move || ui_click_impl(&ctx, &args)).await
    }

    fn ui_click_impl(ctx: &ToolCtx, args: &Value) -> ToolOutcome {
        if ctx.is_cancelled() {
            return ToolOutcome::err("操作已取消");
        }
        let locator = match Locator::parse(args) {
            Ok(l) => l,
            Err(e) => return ToolOutcome::err(e),
        };
        let (window, element, how) = match locate_in_window(&locator, ctx) {
            Ok(v) => v,
            Err(e) => return ToolOutcome::err(e),
        };
        if ctx.is_cancelled() {
            return ToolOutcome::err("操作已取消");
        }
        let label = element_label(&element);
        match click_element(ctx, &window, &element) {
            Ok(strategy) => ToolOutcome::ok(format!(
                "[ok] 已通过 {strategy} 点击 {label}（定位：{how}，窗口：{}）",
                quote_field(&window.title)
            ))
            .with_display(format!("已点击 {label}")),
            Err(e) => ToolOutcome::err(e),
        }
    }

    // --------------------------------------------------------
    // 工具：ui_set_value
    // --------------------------------------------------------

    pub async fn ui_set_value(ctx: &ToolCtx, args: Value) -> ToolOutcome {
        let ctx = ctx.clone();
        spawn_blocking_tool(move || ui_set_value_impl(&ctx, &args)).await
    }

    fn ui_set_value_impl(ctx: &ToolCtx, args: &Value) -> ToolOutcome {
        if ctx.is_cancelled() {
            return ToolOutcome::err("操作已取消");
        }
        let Some(value) = arg_str(args, "value") else {
            return ToolOutcome::err("缺少必填参数 value（要写入的文本）。");
        };
        let locator = match Locator::parse(args) {
            Ok(l) => l,
            Err(e) => return ToolOutcome::err(e),
        };
        let (window, element, how) = match locate_in_window(&locator, ctx) {
            Ok(v) => v,
            Err(e) => return ToolOutcome::err(e),
        };
        let label = element_label(&element);
        match set_value_element(ctx, &window, &element, &value) {
            Ok(strategy) => ToolOutcome::ok(format!(
                "[ok] 已通过 {strategy} 写入 {label}（定位：{how}，窗口：{}），写入 {} 字符。",
                quote_field(&window.title),
                value.chars().count()
            ))
            .with_display(format!("写入 {label}")),
            Err(e) => ToolOutcome::err(e),
        }
    }

    // --------------------------------------------------------
    // 工具：ui_invoke_pattern
    // --------------------------------------------------------

    pub async fn ui_invoke_pattern(ctx: &ToolCtx, args: Value) -> ToolOutcome {
        let ctx = ctx.clone();
        spawn_blocking_tool(move || ui_invoke_pattern_impl(&ctx, &args)).await
    }

    fn ui_invoke_pattern_impl(ctx: &ToolCtx, args: &Value) -> ToolOutcome {
        if ctx.is_cancelled() {
            return ToolOutcome::err("操作已取消");
        }
        let Some(pattern) = arg_str(args, "pattern") else {
            return ToolOutcome::err(
                "缺少必填参数 pattern（invoke / toggle / select / expand / collapse）。",
            );
        };
        let locator = match Locator::parse(args) {
            Ok(l) => l,
            Err(e) => return ToolOutcome::err(e),
        };
        let (window, element, how) = match locate_in_window(&locator, ctx) {
            Ok(v) => v,
            Err(e) => return ToolOutcome::err(e),
        };
        let label = element_label(&element);
        let window_title = quote_field(&window.title);

        let (done, detail) = match pattern.to_ascii_lowercase().as_str() {
            "invoke" => {
                let result = unsafe {
                    element.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
                }
                .map_err(|e| e.message())
                .and_then(|p| (unsafe { p.Invoke() }).map_err(|e| e.message()));
                match result {
                    Ok(()) => (true, "InvokePattern.Invoke".to_string()),
                    Err(e) => (false, format!("控件不支持 InvokePattern 或调用失败：{e}")),
                }
            }
            "toggle" => {
                let result = unsafe {
                    element.GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId)
                }
                .map_err(|e| e.message())
                .and_then(|p| (unsafe { p.Toggle() }).map_err(|e| e.message()));
                match result {
                    Ok(()) => (true, "TogglePattern.Toggle".to_string()),
                    Err(e) => (false, format!("控件不支持 TogglePattern 或调用失败：{e}")),
                }
            }
            "select" => {
                let result = unsafe {
                    element.GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(
                        UIA_SelectionItemPatternId,
                    )
                }
                .map_err(|e| e.message())
                .and_then(|p| (unsafe { p.Select() }).map_err(|e| e.message()));
                match result {
                    Ok(()) => (true, "SelectionItemPattern.Select".to_string()),
                    Err(e) => (
                        false,
                        format!("控件不支持 SelectionItemPattern 或调用失败：{e}"),
                    ),
                }
            }
            "expand" | "collapse" => {
                let expand = pattern.eq_ignore_ascii_case("expand");
                let result = unsafe {
                    element.GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(
                        UIA_ExpandCollapsePatternId,
                    )
                }
                .map_err(|e| e.message())
                .and_then(|p| {
                    let r = if expand {
                        unsafe { p.Expand() }
                    } else {
                        unsafe { p.Collapse() }
                    };
                    r.map_err(|e| e.message())
                });
                let detail = if expand {
                    "ExpandCollapsePattern.Expand"
                } else {
                    "ExpandCollapsePattern.Collapse"
                };
                match result {
                    Ok(()) => (true, detail.to_string()),
                    Err(e) => (
                        false,
                        format!("控件不支持 ExpandCollapsePattern 或调用失败：{e}"),
                    ),
                }
            }
            other => {
                return ToolOutcome::err(format!(
                    "未知 pattern=“{other}”。支持：invoke / toggle / select / expand / collapse。"
                ));
            }
        };

        if done {
            ToolOutcome::ok(format!(
                "[ok] 已对 {label} 执行 {detail}（定位：{how}，窗口：{window_title}）"
            ))
            .with_display(format!("已执行 {pattern}"))
        } else {
            ToolOutcome::err(format!(
                "[err] {detail}\n提示：先调用 ui_tree 查看该节点的 patterns 字段，确认它支持哪种模式；也可以改用 ui_click。"
            ))
        }
    }
}

// ============================================================
// Unix（Linux/macOS）降级实现
// ============================================================

#[cfg(unix)]
mod platform {
    use std::process::Command;

    use neosonia_core::model::{ToolCtx, ToolOutcome};
    use serde_json::Value;

    use super::{arg_str, arg_usize, contains_ci, limit_chars};

    const NO_TREE_MSG: &str = "[err] 当前平台未实现控件树（UI Automation 仅有 Windows 实现）。\
请改用 screenshot + mouse_click 按坐标操作，或安装 xdotool / wmctrl 使用窗口级操作（ui_window_list / ui_focus_window）。";

    fn run(program: &str, args: &[&str]) -> Result<String, String> {
        let mut cmd = Command::new(program);
        crate::apply_no_window_std(&mut cmd);
        match cmd.args(args).output() {
            Ok(out) if out.status.success() => Ok(String::from_utf8_lossy(&out.stdout).to_string()),
            Ok(out) => {
                let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
                Err(format!("{program} 退出码 {:?}：{err}", out.status.code()))
            }
            Err(e) => Err(format!("无法运行 {program}：{e}")),
        }
    }

    fn wmctrl_available() -> bool {
        let mut cmd = Command::new("wmctrl");
        crate::apply_no_window_std(&mut cmd);
        cmd.arg("-m").output().is_ok()
    }

    struct LinuxWindow {
        hwnd: String,
        pid: Option<u32>,
        title: String,
    }

    fn list_windows() -> Result<Vec<LinuxWindow>, String> {
        // wmctrl -lp 输出： <hwnd> <desktop> <pid> <host> <title...>
        let out = run("wmctrl", &["-lp"]).map_err(|e| {
            format!(
                "读取窗口列表失败：{e}\n修复：安装 wmctrl（Debian/Ubuntu: sudo apt install wmctrl；\
Fedora: sudo dnf install wmctrl），或改用 screenshot + mouse_click。"
            )
        })?;
        let mut list = Vec::new();
        for line in out.lines() {
            let mut parts = line
                .splitn(5, char::is_whitespace)
                .filter(|s| !s.is_empty());
            let Some(hwnd) = parts.next() else { continue };
            let _desktop = parts.next();
            let pid = parts.next().and_then(|p| p.parse::<u32>().ok());
            let _host = parts.next();
            let title = parts.next().unwrap_or("").trim().to_string();
            if title.is_empty() {
                continue;
            }
            list.push(LinuxWindow {
                hwnd: hwnd.to_string(),
                pid,
                title,
            });
        }
        Ok(list)
    }

    pub async fn ui_window_list(ctx: &ToolCtx, args: Value) -> ToolOutcome {
        if ctx.is_cancelled() {
            return ToolOutcome::err("操作已取消");
        }
        let filter = arg_str(&args, "filter");
        let limit = arg_usize(&args, "limit").unwrap_or(30).clamp(1, 200);
        if !wmctrl_available() {
            return ToolOutcome::err(
                "未找到 wmctrl，无法枚举窗口。\n修复：sudo apt install wmctrl，或改用 screenshot + mouse_click。",
            );
        }
        let windows = match list_windows() {
            Ok(w) => w,
            Err(e) => return ToolOutcome::err(e),
        };
        let filtered: Vec<&LinuxWindow> = windows
            .iter()
            .filter(|w| match filter.as_deref() {
                Some(f) => contains_ci(&w.title, f),
                None => true,
            })
            .collect();
        let mut lines = vec![format!("[ok] 可见窗口 {} 个（wmctrl）", filtered.len())];
        for (i, w) in filtered.iter().take(limit).enumerate() {
            lines.push(format!(
                "#{i} hwnd={} pid={} title=\"{}\"",
                w.hwnd,
                w.pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into()),
                w.title.replace('"', "'")
            ));
        }
        if filtered.len() == 0 {
            lines.push("没有匹配的窗口。".into());
        }
        ToolOutcome::ok(limit_chars(&lines.join("\n"), 18_000))
            .with_display(format!("列出 {} 个窗口", filtered.len()))
    }

    pub async fn ui_focus_window(ctx: &ToolCtx, args: Value) -> ToolOutcome {
        if ctx.is_cancelled() {
            return ToolOutcome::err("操作已取消");
        }
        if !wmctrl_available() {
            return ToolOutcome::err(
                "未找到 wmctrl，无法激活窗口。\n修复：sudo apt install wmctrl，或改用 xdotool windowactivate。",
            );
        }
        let hwnd = arg_str(&args, "hwnd");
        let pid = arg_usize(&args, "pid");
        let title = arg_str(&args, "title");

        let windows = match list_windows() {
            Ok(w) => w,
            Err(e) => return ToolOutcome::err(e),
        };
        let target = if let Some(h) = hwnd {
            windows
                .into_iter()
                .find(|w| w.hwnd.eq_ignore_ascii_case(&h))
        } else if let Some(p) = pid {
            windows
                .into_iter()
                .find(|w| w.pid.map(|x| x as usize) == Some(p))
        } else if let Some(t) = title {
            windows.into_iter().find(|w| contains_ci(&w.title, &t))
        } else {
            return ToolOutcome::err("请提供 title、pid 或 hwnd 之一来定位窗口。");
        };
        let Some(target) = target else {
            return ToolOutcome::err("没有找到匹配的窗口。请先用 ui_window_list 确认。");
        };
        match run("wmctrl", &["-ia", &target.hwnd]) {
            Ok(_) => ToolOutcome::ok(format!(
                "[ok] 已激活窗口 hwnd={} title=\"{}\"",
                target.hwnd,
                target.title.replace('"', "'")
            ))
            .with_display(format!("激活 {}", target.title)),
            Err(e) => ToolOutcome::err(format!("激活窗口失败：{e}")),
        }
    }

    pub async fn ui_tree(_ctx: &ToolCtx, _args: Value) -> ToolOutcome {
        ToolOutcome::err(NO_TREE_MSG)
    }

    pub async fn ui_click(_ctx: &ToolCtx, _args: Value) -> ToolOutcome {
        ToolOutcome::err(format!(
            "{NO_TREE_MSG}\n需要点击某个坐标时用 mouse_click(x, y)；窗口级操作可先用 ui_window_list / ui_focus_window。"
        ))
    }

    pub async fn ui_set_value(_ctx: &ToolCtx, _args: Value) -> ToolOutcome {
        ToolOutcome::err(
            "[err] 当前平台未实现控件树与 ValuePattern。\n请改用 keyboard_type（先 mouse_click 点中输入框并 Ctrl+A 全选）。",
        )
    }

    pub async fn ui_invoke_pattern(_ctx: &ToolCtx, _args: Value) -> ToolOutcome {
        ToolOutcome::err(NO_TREE_MSG)
    }

    pub async fn ui_act(_ctx: &ToolCtx, _args: Value) -> ToolOutcome {
        ToolOutcome::err(format!(
            "{NO_TREE_MSG}\nui_act 依赖 UIA 定位控件；当前平台请组合 ui_window_list / ui_focus_window + screenshot + mouse_click。"
        ))
    }

    pub async fn ui_drag(_ctx: &ToolCtx, _args: Value) -> ToolOutcome {
        ToolOutcome::err(
            "[err] 当前平台未实现 ui_drag（需要 UIA 解析 {name} 端点）。\n可改用 mouse_drag 直接传 from_x/from_y/to_x/to_y（需要 xdotool 环境下的输入模拟）。",
        )
    }
}

// ============================================================
// 单测（纯逻辑，跨平台）
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_type_names_are_stable() {
        assert_eq!(control_type_name(0), "Unknown");
        assert_eq!(control_type_name(50000), "Button");
        assert_eq!(control_type_name(50004), "Edit");
        assert_eq!(control_type_name(50011), "MenuItem");
        assert_eq!(control_type_name(50032), "Window");
        assert_eq!(control_type_name(50033), "Pane");
        assert_eq!(control_type_name(99999), "Unknown");
    }

    #[test]
    fn window_spec_parsing() {
        assert_eq!(classify_window_spec(None), WindowSpec::Foreground);
        assert_eq!(classify_window_spec(Some("")), WindowSpec::Foreground);
        assert_eq!(
            classify_window_spec(Some("FOREGROUND")),
            WindowSpec::Foreground
        );
        assert_eq!(
            classify_window_spec(Some("0x1A2B")),
            WindowSpec::Hwnd(0x1A2B)
        );
        assert_eq!(
            classify_window_spec(Some("123456")),
            WindowSpec::Hwnd(123456)
        );
        assert_eq!(
            classify_window_spec(Some("记事本")),
            WindowSpec::Title("记事本".into())
        );
        assert_eq!(
            classify_window_spec(Some("notepad.exe - 记事本")),
            WindowSpec::Title("notepad.exe - 记事本".into())
        );
        assert!(parse_hwnd_spec("not a hwnd").is_none());
        assert!(parse_hwnd_spec("").is_none());
    }

    #[test]
    fn substring_matching_is_case_insensitive() {
        assert!(contains_ci("NeoSonia Main Window", "main"));
        assert!(contains_ci("NeoSonia", ""));
        assert!(!contains_ci("NeoSonia", "firefox"));
    }

    #[test]
    fn char_limited_truncation() {
        let s = "一二三四五六";
        assert_eq!(limit_chars(s, 10), s);
        let t = limit_chars(s, 3);
        assert!(t.contains("已截断"));
        assert!(t.starts_with("一二三"));
    }

    #[test]
    fn register_exposes_all_eight_tools() {
        let reg = ToolRegistry::new();
        register(&reg);
        let names = reg.names();
        for expected in [
            "ui_window_list",
            "ui_focus_window",
            "ui_tree",
            "ui_click",
            "ui_set_value",
            "ui_invoke_pattern",
            "ui_act",
            "ui_drag",
        ] {
            assert!(
                names.iter().any(|n| n == expected),
                "missing tool {expected}"
            );
        }
        assert_eq!(names.len(), 8);
        for spec in reg.specs() {
            assert_eq!(spec.parameters["type"], "object");
            assert!(!spec.description.is_empty());
        }
    }

    #[test]
    fn ui_action_parsing() {
        assert_eq!(UiAction::parse("click"), Some(UiAction::Click));
        assert_eq!(UiAction::parse(" TYPE "), Some(UiAction::Type));
        assert_eq!(UiAction::parse("set_value"), Some(UiAction::SetValue));
        assert_eq!(UiAction::parse("setvalue"), Some(UiAction::SetValue));
        assert_eq!(UiAction::parse("scroll"), Some(UiAction::Scroll));
        assert_eq!(UiAction::parse("nope"), None);
        assert!(UiAction::Type.needs_value());
        assert!(UiAction::SetValue.needs_value());
        assert!(!UiAction::Click.needs_value());
        assert_eq!(UiAction::Scroll.as_str(), "scroll");
    }

    #[test]
    fn point_spec_parsing() {
        assert_eq!(
            parse_point_spec(Some(&json!({"x": 10, "y": -20}))).unwrap(),
            PointSpec::Xy(10, -20)
        );
        assert_eq!(
            parse_point_spec(Some(&json!({"name": "文本编辑器"}))).unwrap(),
            PointSpec::Name("文本编辑器".into())
        );
        assert_eq!(
            parse_point_spec(Some(&json!([3, 4]))).unwrap(),
            PointSpec::Xy(3, 4)
        );
        assert_eq!(
            parse_point_spec(Some(&json!("100,200"))).unwrap(),
            PointSpec::Xy(100, 200)
        );
        assert_eq!(
            parse_point_spec(Some(&json!("某个按钮"))).unwrap(),
            PointSpec::Name("某个按钮".into())
        );
        assert!(parse_point_spec(None).is_err());
        assert!(parse_point_spec(Some(&json!({"x": 1}))).is_err());
        assert!(parse_point_spec(Some(&json!([1, 2, 3]))).is_err());
    }

    #[test]
    fn scroll_direction_parsing() {
        assert_eq!(parse_scroll_dir(Some("up"), None), ScrollDir::Up);
        assert_eq!(parse_scroll_dir(Some("DOWN"), None), ScrollDir::Down);
        assert_eq!(parse_scroll_dir(Some("向上"), None), ScrollDir::Up);
        assert_eq!(
            parse_scroll_dir(None, Some(&json!(["down"]))),
            ScrollDir::Down
        );
        assert_eq!(parse_scroll_dir(None, None), ScrollDir::Down);
        assert_eq!(parse_scroll_dir(Some("whatever"), None), ScrollDir::Down);
    }

    #[test]
    fn count_extraction_from_tree_text() {
        let text = "#3 Text name=\"0 个字符\" bounds=(1,2,3,4)\n#4 Text name=\"行 2，列 1\"";
        // 数字和单位之间允许有空格（UIA 状态文本常见写法）。
        assert_eq!(find_count_before_unit(text, "个字符"), Some(0));
        assert_eq!(find_count_before_unit(text, " 个字符"), Some(0));
        assert_eq!(find_count_before_unit(text, "不存在"), None);
        // 多位数字
        assert_eq!(
            find_count_before_unit("#9 Text name=\"128 个字符\"", "个字符"),
            Some(128)
        );
        // 单位前没有数字
        assert_eq!(
            find_count_before_unit("#9 Text name=\"个字符\"", "个字符"),
            None
        );
    }
}

// ============================================================
// 真实桌面窗口集成测试（需要本机有可见窗口，默认 #[ignore]）
// ============================================================

#[cfg(all(test, windows))]
mod real_ui_tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use neosonia_core::config::{Paths, Settings};
    use neosonia_core::model::{
        AgentEvent, AgentHost, MemoryApi, MemoryItem, MemoryStats, SubagentRequest, ToolCtx,
        ToolOutcome,
    };
    use neosonia_core::registry::ToolRegistry;
    use parking_lot::RwLock;
    use serde_json::{Value, json};

    use super::find_count_before_unit;
    use crate::uia::register;

    struct NullMemory;

    impl MemoryApi for NullMemory {
        fn remember(&self, _c: &str, _t: &[String], _i: f64, _s: &str) -> anyhow::Result<i64> {
            Ok(0)
        }
        fn recall(&self, _q: &str, _l: usize) -> anyhow::Result<Vec<MemoryItem>> {
            Ok(Vec::new())
        }
        fn recent(&self, _l: usize) -> anyhow::Result<Vec<MemoryItem>> {
            Ok(Vec::new())
        }
        fn all(&self, _k: Option<&str>, _l: usize) -> anyhow::Result<Vec<MemoryItem>> {
            Ok(Vec::new())
        }
        fn forget(&self, _id: i64) -> anyhow::Result<bool> {
            Ok(false)
        }
        fn update_importance(&self, _id: i64, _v: f64) -> anyhow::Result<()> {
            Ok(())
        }
        fn get_block(&self, _k: &str) -> anyhow::Result<Option<String>> {
            Ok(None)
        }
        fn set_block(&self, _k: &str, _v: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn blocks(&self) -> anyhow::Result<std::collections::BTreeMap<String, String>> {
            Ok(Default::default())
        }
        fn log_conversation(&self, _r: &str, _c: &str, _t: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn stats(&self) -> anyhow::Result<MemoryStats> {
            Ok(MemoryStats::default())
        }
    }

    struct TestHost {
        settings: Arc<RwLock<Settings>>,
        paths: Paths,
        memory: Arc<dyn MemoryApi>,
    }

    impl AgentHost for TestHost {
        fn settings(&self) -> Arc<RwLock<Settings>> {
            self.settings.clone()
        }
        fn paths(&self) -> Paths {
            self.paths.clone()
        }
        fn memory(&self) -> Arc<dyn MemoryApi> {
            self.memory.clone()
        }
        fn emit(&self, _event: AgentEvent) {}
        fn spawn_subagent(&self, _req: SubagentRequest) -> anyhow::Result<String> {
            anyhow::bail!("测试宿主不支持子代理")
        }
        fn is_cancelled(&self) -> bool {
            false
        }
    }

    fn test_ctx() -> ToolCtx {
        let mut settings = Settings::default();
        settings.tools.uia_enabled = true;
        settings.tools.allow_input_simulation = true;
        let paths = Paths::from_root(std::env::temp_dir().join("neosonia-automation-test"));
        let host = Arc::new(TestHost {
            settings: Arc::new(RwLock::new(settings)),
            paths,
            memory: Arc::new(NullMemory),
        });
        ToolCtx::new(host, "turn-test", "call-test")
    }

    fn first_hwnd(content: &str) -> Option<String> {
        for line in content.lines() {
            if let Some(pos) = line.find("hwnd=") {
                let rest = &line[pos + 5..];
                let hwnd: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
                if hwnd.starts_with("0x") {
                    return Some(hwnd);
                }
            }
        }
        None
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "需要本机桌面有可见窗口（会读取控件树，不点击）"]
    async fn real_uia_tools_smoke() {
        let ctx = test_ctx();
        let reg = ToolRegistry::new();
        register(&reg);
        assert_eq!(reg.len(), 8);

        let list = reg
            .call(&ctx, "ui_window_list", &json!({ "limit": 15 }))
            .await;
        println!("=== ui_window_list ===\nok={}\n{}", list.ok, list.content);
        assert!(list.ok, "ui_window_list 失败：{}", list.content);
        assert!(
            list.content.contains("hwnd="),
            "窗口列表为空：{}",
            list.content
        );

        // 优先用 Chromium 窗口（Electron/Chromium 场景的关键验证），没有再退回任意可见窗口。
        let edge_list = reg
            .call(
                &ctx,
                "ui_window_list",
                &json!({ "filter": "Edge", "limit": 5 }),
            )
            .await;
        println!(
            "=== ui_window_list(filter=Edge) ===\nok={}\n{}",
            edge_list.ok, edge_list.content
        );
        let edge_hwnd = if edge_list.ok {
            first_hwnd(&edge_list.content)
        } else {
            None
        };
        let (hwnd, is_chromium) = match edge_hwnd {
            Some(h) => (h, true),
            None => (
                first_hwnd(&list.content).expect("窗口列表里没有 hwnd"),
                false,
            ),
        };
        println!("本次测试使用窗口 hwnd={hwnd}，Chromium={is_chromium}");

        // 1) 先把目标窗口切前台并恢复显示：
        //    Chromium 系应用在后台/最小化时只暴露很浅的 UIA 树（懒加载），
        //    必须在窗口激活后再读控件树才能看到地址栏、工具栏按钮等。
        let focus = reg
            .call(&ctx, "ui_focus_window", &json!({ "hwnd": hwnd.clone() }))
            .await;
        println!(
            "=== ui_focus_window(hwnd={hwnd}) ===\nok={}\n{}",
            focus.ok, focus.content
        );
        std::thread::sleep(std::time::Duration::from_millis(900));

        // 2) 按句柄读控件树（最稳，不依赖标题/前台）。
        let mut tree = reg
            .call(
                &ctx,
                "ui_tree",
                &json!({ "window": hwnd.clone(), "depth": 8, "max_nodes": 500, "timeout_ms": 20000 }),
            )
            .await;
        // Chromium 懒加载有时要等一会儿才铺开整棵树：节点太少就再等 1.2 秒重读一次。
        if is_chromium && tree.content.lines().filter(|l| l.starts_with('#')).count() < 40 {
            std::thread::sleep(std::time::Duration::from_millis(1200));
            tree = reg
                .call(
                    &ctx,
                    "ui_tree",
                    &json!({ "window": hwnd.clone(), "depth": 8, "max_nodes": 500, "timeout_ms": 20000 }),
                )
                .await;
        }
        println!(
            "=== ui_tree(hwnd={hwnd}) ===\nok={}\n{}",
            tree.ok, tree.content
        );
        assert!(tree.ok, "ui_tree 失败：{}", tree.content);
        assert!(
            tree.content.contains("遍历参数"),
            "ui_tree 输出异常：{}",
            tree.content
        );
        assert!(
            !tree.content.contains("没有读到任何控件"),
            "ui_tree 没读到控件：{}",
            tree.content
        );
        if is_chromium {
            assert!(
                tree.content.contains("Button") || tree.content.contains("Pane"),
                "Chromium 控件树里没有常见控件：{}",
                tree.content
            );
            assert!(
                tree.content.contains("patterns=["),
                "节点缺少 patterns 字段：{}",
                tree.content
            );
        }

        // 3) 前台窗口的控件树（契约里的默认 window=foreground 路径）。
        let fg = reg
            .call(&ctx, "ui_tree", &json!({ "depth": 4, "max_nodes": 120 }))
            .await;
        println!("=== ui_tree(foreground) ===\nok={}\n{}", fg.ok, fg.content);
        assert!(fg.ok, "ui_tree(foreground) 失败：{}", fg.content);

        // 4) 按标题查控件树（契约里的 window=标题子串 路径）。
        if is_chromium {
            let by_title = reg
                .call(
                    &ctx,
                    "ui_tree",
                    &json!({ "window": "Edge", "depth": 5, "max_nodes": 200, "timeout_ms": 20000 }),
                )
                .await;
            println!(
                "=== ui_tree(window=Edge) ===\nok={}\n{}",
                by_title.ok, by_title.content
            );
            assert!(by_title.ok, "按标题读控件树失败：{}", by_title.content);
        }

        // 5) 在真实 Chromium 控件上验证 ui_click / ui_invoke_pattern 的 InvokePattern 路径。
        //    只挑“刷新 / 收藏夹 / 设置及其他”这类不破坏数据的按钮，且要求控件树里
        //    已经显示 patterns=[...invoke...]，避免退化成会受“Black Screen”置顶窗口影响的鼠标点击。
        if is_chromium {
            let mut candidate: Option<(usize, String)> = None;
            // 按优先级挑最安全的按钮：设置菜单 > 刷新 > 发送反馈 > 收藏夹。
            'outer: for key in ["设置及其他", "刷新", "主页", "收藏夹", "发送反馈", "返回"]
            {
                for line in tree.content.lines() {
                    if !line.starts_with('#') || !line.contains("Button") {
                        continue;
                    }
                    if !line.contains("patterns=[") || !line.contains("invoke") {
                        continue;
                    }
                    if !line.contains(key) {
                        continue;
                    }
                    if let Some(idx) = parse_node_index(line) {
                        candidate = Some((idx, line.to_string()));
                        break 'outer;
                    }
                }
            }
            match candidate {
                Some((idx, line)) => {
                    println!("安全测试目标：{line}");
                    let click = reg
                        .call(
                            &ctx,
                            "ui_click",
                            &json!({ "window": hwnd.clone(), "index": idx, "depth": 8, "max_nodes": 500 }),
                        )
                        .await;
                    println!(
                        "=== ui_click(index={idx}) ===\nok={}\n{}",
                        click.ok, click.content
                    );
                    assert!(click.ok, "ui_click 失败：{}", click.content);

                    let invoke = reg
                        .call(
                            &ctx,
                            "ui_invoke_pattern",
                            &json!({ "window": hwnd.clone(), "index": idx, "pattern": "invoke", "depth": 8, "max_nodes": 500 }),
                        )
                        .await;
                    println!(
                        "=== ui_invoke_pattern(index={idx}, pattern=invoke) ===\nok={}\n{}",
                        invoke.ok, invoke.content
                    );
                    assert!(invoke.ok, "ui_invoke_pattern 失败：{}", invoke.content);
                }
                None => println!("[skip] 控件树里没有找到带 invoke 模式的安全按钮，跳过点击验证。"),
            }
        }
    }

    /// 从 `#123 Button name=...` 这种节点行里取出 index。
    fn parse_node_index(line: &str) -> Option<usize> {
        let rest = line.strip_prefix('#')?;
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        digits.parse::<usize>().ok()
    }

    /// 从控件树文本里按 role + name 子串找节点并解析 `bounds=(x,y,w,h)`。
    fn find_node_bounds(
        content: &str,
        role: &str,
        name_contains: &str,
    ) -> Option<(i32, i32, i32, i32)> {
        for line in content.lines() {
            if !line.starts_with('#') || !line.contains(role) || !line.contains(name_contains) {
                continue;
            }
            let Some(pos) = line.find("bounds=(") else {
                continue;
            };
            let rest = &line[pos + "bounds=(".len()..];
            let Some(end) = rest.find(')') else {
                continue;
            };
            let nums: Vec<i32> = rest[..end]
                .split(',')
                .filter_map(|s| s.trim().parse::<i32>().ok())
                .collect();
            if nums.len() == 4 {
                return Some((nums[0], nums[1], nums[2], nums[3]));
            }
        }
        None
    }

    fn center_of(bounds: (i32, i32, i32, i32)) -> (i32, i32) {
        (bounds.0 + bounds.2 / 2, bounds.1 + bounds.3 / 2)
    }

    async fn run_tool(reg: &ToolRegistry, ctx: &ToolCtx, name: &str, args: Value) -> ToolOutcome {
        let pretty = serde_json::to_string(&args).unwrap_or_default();
        let out = reg.call(ctx, name, &args).await;
        println!("=== {name} {pretty} ===\nok={}\n{}", out.ok, out.content);
        out
    }

    /// 从 window 列表文本里提取所有 hwnd（用于区分“我们新开的窗口”和用户已有的窗口）。
    fn collect_hwnds(content: &str) -> Vec<String> {
        let mut out = Vec::new();
        for line in content.lines() {
            if let Some(pos) = line.find("hwnd=") {
                let rest = &line[pos + 5..];
                let hwnd: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
                if hwnd.starts_with("0x") {
                    out.push(hwnd);
                }
            }
        }
        out
    }

    /// 测试专用：把窗口挪到左上角并置顶（不激活），避开可能置顶的系统弹窗/其它窗口。
    /// 只用于我们测试自己新开的记事本/计算器，不用于产品工具。
    fn move_window_for_test(hwnd: &str) {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{
            HWND_TOP, SWP_NOACTIVATE, SWP_SHOWWINDOW, SetWindowPos,
        };

        let raw = hwnd.trim();
        let Some(hex) = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) else {
            return;
        };
        let Ok(v) = i64::from_str_radix(hex, 16) else {
            return;
        };
        unsafe {
            let _ = SetWindowPos(
                HWND(v as *mut std::ffi::c_void),
                HWND_TOP,
                80,
                80,
                1280,
                900,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
        }
    }

    /// 确保窗口在前台：先 ui_focus_window；失败则把窗口挪到左上角再点标题栏，最后重试。
    /// Windows 前台锁 + 更高权限的系统弹窗会拒绝纯 API 抢焦点，鼠标点击更可靠。
    async fn ensure_foreground(reg: &ToolRegistry, ctx: &ToolCtx, hwnd: &str) -> bool {
        let focus = reg
            .call(ctx, "ui_focus_window", &json!({ "hwnd": hwnd }))
            .await;
        println!(
            "=== ui_focus_window(ensure) ===\nok={}\n{}",
            focus.ok, focus.content
        );
        if focus.ok {
            return true;
        }

        // 挪到 (80,80) 并置顶，避开置顶的系统弹窗，然后点它自己的标题栏把它激活。
        move_window_for_test(hwnd);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let tree = reg
            .call(
                ctx,
                "ui_tree",
                &json!({ "window": hwnd, "depth": 2, "max_nodes": 20 }),
            )
            .await;
        if tree.ok {
            if let Some((x, y, w, _h)) = find_node_bounds(&tree.content, "Window", "name=") {
                let click_x = x + (w / 4).clamp(40, 160);
                let click_y = y + 12;
                println!("[verify] 点击窗口标题栏 ({click_x},{click_y}) 激活窗口");
                let _ = reg
                    .call(ctx, "mouse_click", &json!({ "x": click_x, "y": click_y }))
                    .await;
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
        let retry = reg
            .call(ctx, "ui_focus_window", &json!({ "hwnd": hwnd }))
            .await;
        println!(
            "=== ui_focus_window(ensure retry) ===\nok={}\n{}",
            retry.ok, retry.content
        );
        retry.ok
    }

    fn screenshot_file_path(content: &str) -> Option<String> {
        content
            .lines()
            .find(|l| l.starts_with("路径："))
            .map(|l| l.trim_start_matches("路径：").trim().to_string())
    }

    fn png_ok(path: &str) -> bool {
        match std::fs::read(path) {
            Ok(bytes) => bytes.len() > 500 && bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
            Err(_) => false,
        }
    }

    fn window_line_hwnd(line: &str) -> Option<String> {
        let pos = line.find("hwnd=")?;
        let rest = &line[pos + "hwnd=".len()..];
        let hwnd: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
        if hwnd.starts_with("0x") {
            Some(hwnd)
        } else {
            None
        }
    }

    fn window_line_pid(line: &str) -> Option<u32> {
        let pos = line.find("pid=")?;
        let rest = &line[pos + "pid=".len()..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        digits.parse::<u32>().ok()
    }

    /// 收集“标题/类名匹配、且 hwnd 不在 before 里”的窗口行。
    async fn collect_new_window_lines(
        reg: &ToolRegistry,
        ctx: &ToolCtx,
        needles: &[&str],
        before: &[String],
    ) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        for needle in needles {
            let out = reg
                .call(
                    ctx,
                    "ui_window_list",
                    &json!({ "filter": needle, "limit": 10 }),
                )
                .await;
            if !out.ok {
                continue;
            }
            for line in out.content.lines() {
                if !line.starts_with('#') {
                    continue;
                }
                let Some(h) = window_line_hwnd(line) else {
                    continue;
                };
                if before.contains(&h) {
                    continue;
                }
                if !lines.iter().any(|l| l == line) {
                    lines.push(line.to_string());
                }
            }
        }
        lines
    }

    /// 等待新开的应用窗口稳定下来。
    /// 新版记事本启动时会先出现一个短暂的 splash/恢复窗口再变主窗口，
    /// 所以第一次看到候选后再等 900ms 重新收集一次，返回稳定后的窗口行。
    async fn wait_new_app_windows(
        reg: &ToolRegistry,
        ctx: &ToolCtx,
        needles: &[&str],
        before: &[String],
        timeout_ms: u64,
    ) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut first_seen = false;
        loop {
            let current = collect_new_window_lines(reg, ctx, needles, before).await;
            if !current.is_empty() {
                if !first_seen {
                    first_seen = true;
                    println!("[verify] 发现候选窗口，等待 UI 稳定…");
                    tokio::time::sleep(Duration::from_millis(900)).await;
                    continue;
                }
                return current;
            }
            if Instant::now() >= deadline {
                return Vec::new();
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }

    /// 从候选里挑主窗口：标题最长的通常是主窗口（启动 splash 的标题很短）。
    fn pick_main_window_line(lines: &[String]) -> Option<String> {
        lines.iter().max_by_key(|l| l.len()).cloned()
    }

    /// 测试应用守卫：无论正常结束还是断言 panic，都会尽力关闭本次打开的窗口和进程，
    /// 避免在用户桌面上残留记事本/计算器窗口。
    struct AppGuard {
        children: Vec<std::process::Child>,
        hwnds: Vec<isize>,
        pids: Vec<u32>,
    }

    impl AppGuard {
        fn new() -> Self {
            Self {
                children: Vec::new(),
                hwnds: Vec::new(),
                pids: Vec::new(),
            }
        }

        fn launch(&mut self, program: &str) -> bool {
            match std::process::Command::new(program).spawn() {
                Ok(child) => {
                    self.children.push(child);
                    true
                }
                Err(_) => false,
            }
        }

        /// 记录一条 window 列表行里的 hwnd 与 pid（两者都用于清理，防止只关掉 splash 窗口）。
        fn track_window_line(&mut self, line: &str) {
            if let Some(hwnd) = window_line_hwnd(line) {
                if let Some(hex) = hwnd.strip_prefix("0x").or_else(|| hwnd.strip_prefix("0X")) {
                    if let Ok(v) = i64::from_str_radix(hex, 16) {
                        let h = v as isize;
                        if !self.hwnds.contains(&h) {
                            self.hwnds.push(h);
                        }
                    }
                }
            }
            if let Some(pid) = window_line_pid(line) {
                if pid != 0 && !self.pids.contains(&pid) {
                    self.pids.push(pid);
                }
            }
        }
    }

    impl Drop for AppGuard {
        fn drop(&mut self) {
            use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
            use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_CLOSE};

            for h in self.hwnds.drain(..) {
                unsafe {
                    let _ = PostMessageW(
                        HWND(h as *mut std::ffi::c_void),
                        WM_CLOSE,
                        WPARAM(0),
                        LPARAM(0),
                    );
                }
            }
            std::thread::sleep(Duration::from_millis(400));
            // 按窗口 pid 结束进程树：新版记事本/计算器是打包应用，光靠启动器 pid 关不掉。
            for pid in self.pids.drain(..) {
                let pid = pid.to_string();
                let _ = std::process::Command::new("taskkill")
                    .args(["/PID", pid.as_str(), "/T", "/F"])
                    .output();
            }
            for mut child in self.children.drain(..) {
                let pid = child.id().to_string();
                let _ = std::process::Command::new("taskkill")
                    .args(["/PID", pid.as_str(), "/T", "/F"])
                    .output();
                let _ = child.kill();
            }
            println!("[verify] 已清理测试窗口与进程");
        }
    }

    /// 真机输入验证（Black Screen 已关闭后应可用）：
    /// 记事本键盘输入 + 拖选、计算器鼠标点击 + ui_act/ui_drag 复合动作。
    /// 这是非破坏性测试，只操作新开的记事本/计算器窗口，结束后尽力关闭。
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "真机输入验证：会打开记事本/计算器并模拟键鼠（非破坏性）"]
    async fn real_input_and_ui_act() {
        let mut settings = Settings::default();
        settings.tools.uia_enabled = true;
        settings.tools.cdp_enabled = true;
        settings.tools.allow_input_simulation = true;
        settings.tools.allow_shell = true;
        let paths = Paths::from_root(std::env::temp_dir().join("neosonia-automation-test"));
        let host = Arc::new(TestHost {
            settings: Arc::new(RwLock::new(settings.clone())),
            paths: paths.clone(),
            memory: Arc::new(NullMemory),
        });
        let ctx = ToolCtx::new(host, "turn-input", "call-input");
        let reg = ToolRegistry::new();
        crate::register_all(&reg, &settings);
        assert!(
            reg.get("keyboard_type").is_some() && reg.get("screenshot").is_some(),
            "input/screen 工具未注册（检查 allow_input_simulation）"
        );
        assert!(reg.get("ui_act").is_some() && reg.get("ui_drag").is_some());

        let mut guard = AppGuard::new();
        let screenshot_dir = paths.screenshots.clone();
        let _ = std::fs::create_dir_all(&screenshot_dir);

        // 先记录已有窗口：只操作我们新开的记事本/计算器，绝不碰用户已经打开的窗口。
        let existing = reg
            .call(&ctx, "ui_window_list", &json!({ "limit": 100 }))
            .await;
        let existing_hwnds = if existing.ok {
            collect_hwnds(&existing.content)
        } else {
            Vec::new()
        };
        println!("[verify] 测试前已有窗口 {} 个", existing_hwnds.len());

        // ================= 记事本：键盘输入 =================
        guard.launch("notepad");
        let notepad_windows =
            wait_new_app_windows(&reg, &ctx, &["Notepad", "记事本"], &existing_hwnds, 20_000).await;
        assert!(!notepad_windows.is_empty(), "等不到新开的记事本窗口（20s）");
        for line in &notepad_windows {
            guard.track_window_line(line);
        }
        let main_line = pick_main_window_line(&notepad_windows).expect("没有记事本窗口行");
        let hwnd = window_line_hwnd(&main_line).expect("窗口行里没有 hwnd");
        println!("[verify] 记事本主窗口 hwnd={hwnd}：{main_line}");

        // 前台锁/残留系统弹窗可能拒绝纯 API 抢焦点：失败时点一下标题栏再重试。
        assert!(
            ensure_foreground(&reg, &ctx, &hwnd).await,
            "无法把记事本切到前台（ui_focus_window + 标题栏点击都失败）"
        );

        // 新版记事本会恢复上次未保存的标签内容（可能带着上次崩溃/被打断的文本），
        // 所以先 Ctrl+A + Delete 清空当前标签，保证断言从干净状态开始。
        let select_all = run_tool(
            &reg,
            &ctx,
            "keyboard_key",
            json!({ "key": "a", "modifiers": ["ctrl"] }),
        )
        .await;
        assert!(
            select_all.ok,
            "keyboard_key(ctrl+a) 失败：{}",
            select_all.content
        );
        let clear = run_tool(&reg, &ctx, "keyboard_key", json!({ "key": "delete" })).await;
        assert!(clear.ok, "keyboard_key(delete) 失败：{}", clear.content);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let cleaned = reg.call(&ctx, "ui_tree", &json!({ "window": hwnd })).await;
        let c0 = find_count_before_unit(&cleaned.content, "个字符");
        println!("[verify] 清空记事本后字符数 = {c0:?}");
        assert_eq!(c0, Some(0), "记事本没有清空：{:?}", cleaned.content);

        let typed = run_tool(&reg, &ctx, "keyboard_type", json!({ "text": "6*7=" })).await;
        assert!(typed.ok, "keyboard_type 失败：{}", typed.content);
        let enter = run_tool(&reg, &ctx, "keyboard_key", json!({ "key": "enter" })).await;
        assert!(enter.ok, "keyboard_key 失败：{}", enter.content);
        tokio::time::sleep(Duration::from_millis(400)).await;

        let tree1 = run_tool(&reg, &ctx, "ui_tree", json!({ "window": hwnd })).await;
        assert!(tree1.ok, "ui_tree 失败：{}", tree1.content);
        assert!(
            tree1.content.contains("行 2"),
            "回车没有产生第 2 行：{}",
            tree1.content
        );
        let c1 = find_count_before_unit(&tree1.content, "个字符");
        println!("[verify] 输入 6*7= + Enter 后字符数 = {c1:?}");
        assert!(
            c1.unwrap_or(0) >= 4,
            "记事本字符数异常（应 >=4）：{:?}",
            tree1.content
        );

        let shot1 = run_tool(
            &reg,
            &ctx,
            "screenshot",
            json!({ "save_path": screenshot_dir.join("real_input_notepad.png").to_string_lossy() }),
        )
        .await;
        assert!(shot1.ok, "screenshot 失败：{}", shot1.content);
        let shot1_path = screenshot_file_path(&shot1.content).expect("截图结果没有路径");
        assert!(png_ok(&shot1_path), "截图文件不是合法 PNG：{shot1_path}");
        println!("[verify] 记事本输入截图：{shot1_path}");

        // ================= ui_act type：聚焦输入控件后键盘输入 =================
        let act = run_tool(
            &reg,
            &ctx,
            "ui_act",
            json!({ "action": "type", "window": hwnd, "name": "文本编辑器", "value": "Hello ui_act" }),
        )
        .await;
        assert!(act.ok, "ui_act type 失败：{}", act.content);
        tokio::time::sleep(Duration::from_millis(400)).await;

        let tree2 = run_tool(&reg, &ctx, "ui_tree", json!({ "window": hwnd })).await;
        assert!(tree2.ok, "ui_tree 失败：{}", tree2.content);
        let c2 = find_count_before_unit(&tree2.content, "个字符");
        println!("[verify] ui_act type 之后字符数 = {c2:?}");
        assert!(
            c2.unwrap_or(0) > c1.unwrap_or(0),
            "ui_act type 没有增加字符数：{:?}",
            tree2.content
        );

        // ================= tools mouse_drag：拖选第一行后删除 =================
        let doc = find_node_bounds(&tree2.content, "Document", "文本编辑器")
            .or_else(|| find_node_bounds(&tree2.content, "Edit", "文本编辑器"))
            .expect("控件树里没有找到记事本文本区 bounds");
        println!("[verify] 记事本文本区 bounds={doc:?}");
        let drag_from = (doc.0 + 10, doc.1 + 14);
        let drag_to = (doc.0 + 320, doc.1 + 14);
        let drag = run_tool(
            &reg,
            &ctx,
            "mouse_drag",
            json!({ "from_x": drag_from.0, "from_y": drag_from.1, "to_x": drag_to.0, "to_y": drag_to.1 }),
        )
        .await;
        assert!(drag.ok, "mouse_drag 失败：{}", drag.content);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let del = run_tool(&reg, &ctx, "keyboard_key", json!({ "key": "delete" })).await;
        assert!(del.ok, "keyboard_key(delete) 失败：{}", del.content);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let tree3 = run_tool(&reg, &ctx, "ui_tree", json!({ "window": hwnd })).await;
        let c3 = find_count_before_unit(&tree3.content, "个字符");
        println!("[verify] mouse_drag 拖选 + Delete 之后字符数 = {c3:?}");
        assert!(
            c3.unwrap_or(u32::MAX) < c2.unwrap_or(0),
            "mouse_drag 拖选没有选中文本（字符数未下降）：{:?}",
            tree3.content
        );

        // ================= ui_drag：按 UIA 坐标全选后输入 XYZ =================
        let doc2 = find_node_bounds(&tree3.content, "Document", "文本编辑器")
            .or_else(|| find_node_bounds(&tree3.content, "Edit", "文本编辑器"))
            .expect("控件树里没有找到记事本文本区 bounds");
        let from2 = (doc2.0 + 40, doc2.1 + 40);
        let to2 = (doc2.0 + doc2.2 - 40, doc2.1 + doc2.3 - 40);
        let udrag = run_tool(
            &reg,
            &ctx,
            "ui_drag",
            json!({
                "window": hwnd,
                "from": { "x": from2.0, "y": from2.1 },
                "to": { "x": to2.0, "y": to2.1 },
                "duration_ms": 500
            }),
        )
        .await;
        assert!(udrag.ok, "ui_drag 失败：{}", udrag.content);
        let type_xyz = run_tool(&reg, &ctx, "keyboard_type", json!({ "text": "XYZ" })).await;
        assert!(type_xyz.ok, "keyboard_type(XYZ) 失败：{}", type_xyz.content);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let tree4 = run_tool(&reg, &ctx, "ui_tree", json!({ "window": hwnd })).await;
        let c4 = find_count_before_unit(&tree4.content, "个字符");
        println!("[verify] ui_drag 全选 + 输入 XYZ 之后字符数 = {c4:?}");
        assert_eq!(
            c4,
            Some(3),
            "ui_drag 拖选全选后输入 XYZ，字符数应为 3：{:?}",
            tree4.content
        );

        let shot2 = run_tool(
            &reg,
            &ctx,
            "screenshot",
            json!({ "save_path": screenshot_dir.join("real_input_ui_drag.png").to_string_lossy() }),
        )
        .await;
        assert!(shot2.ok, "screenshot 失败：{}", shot2.content);
        let shot2_path = screenshot_file_path(&shot2.content).expect("截图结果没有路径");
        assert!(png_ok(&shot2_path));
        println!("[verify] ui_drag 后截图：{shot2_path}");

        // ================= 计算器：鼠标点击 + ui_act 复合动作 =================
        // 刷新“已有窗口”快照，避免把用户原本开着的计算器当成我们的。
        let before_calc = reg
            .call(&ctx, "ui_window_list", &json!({ "limit": 100 }))
            .await;
        let before_calc_hwnds = if before_calc.ok {
            collect_hwnds(&before_calc.content)
        } else {
            Vec::new()
        };
        guard.launch("calc");
        let calc_windows = wait_new_app_windows(
            &reg,
            &ctx,
            &["计算器", "Calculator"],
            &before_calc_hwnds,
            20_000,
        )
        .await;
        assert!(!calc_windows.is_empty(), "等不到新开的计算器窗口（20s）");
        for line in &calc_windows {
            guard.track_window_line(line);
        }
        let calc_main = pick_main_window_line(&calc_windows).expect("没有计算器窗口行");
        let calc_hwnd = window_line_hwnd(&calc_main).expect("计算器窗口行里没有 hwnd");
        println!("[verify] 计算器主窗口 hwnd={calc_hwnd}：{calc_main}");
        assert!(
            ensure_foreground(&reg, &ctx, &calc_hwnd).await,
            "无法把计算器切到前台（ui_focus_window + 标题栏点击都失败）"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;

        let calc_tree = run_tool(
            &reg,
            &ctx,
            "ui_tree",
            json!({ "window": calc_hwnd, "depth": 8, "max_nodes": 400 }),
        )
        .await;
        assert!(calc_tree.ok, "计算器 ui_tree 失败：{}", calc_tree.content);
        println!(
            "[verify] 计算器初始显示：{:?}",
            calc_tree.content.lines().find(|l| l.contains("显示为"))
        );
        let six = find_node_bounds(&calc_tree.content, "Button", "name=\"六\"")
            .or_else(|| find_node_bounds(&calc_tree.content, "Button", "name=\"6\""))
            .expect("计算器里没有找到 6 按钮");
        let seven = find_node_bounds(&calc_tree.content, "Button", "name=\"七\"")
            .or_else(|| find_node_bounds(&calc_tree.content, "Button", "name=\"7\""))
            .expect("计算器里没有找到 7 按钮");
        let (sx, sy) = center_of(six);
        let (px, py) = center_of(seven);

        let click6 = run_tool(&reg, &ctx, "mouse_click", json!({ "x": sx, "y": sy })).await;
        assert!(click6.ok, "mouse_click(6) 失败：{}", click6.content);
        let mul = run_tool(
            &reg,
            &ctx,
            "ui_act",
            json!({ "action": "click", "window": calc_hwnd, "name": "乘以" }),
        )
        .await;
        assert!(mul.ok, "ui_act click(乘以) 失败：{}", mul.content);
        let click7 = run_tool(&reg, &ctx, "mouse_click", json!({ "x": px, "y": py })).await;
        assert!(click7.ok, "mouse_click(7) 失败：{}", click7.content);
        let eq = run_tool(
            &reg,
            &ctx,
            "ui_act",
            json!({ "action": "click", "window": calc_hwnd, "name": "等于" }),
        )
        .await;
        assert!(eq.ok, "ui_act click(等于) 失败：{}", eq.content);
        tokio::time::sleep(Duration::from_millis(500)).await;

        let calc_tree2 = run_tool(
            &reg,
            &ctx,
            "ui_tree",
            json!({ "window": calc_hwnd, "depth": 8, "max_nodes": 400 }),
        )
        .await;
        println!(
            "[verify] 计算器点击 6 * 7 = 之后显示：{:?}",
            calc_tree2.content.lines().find(|l| l.contains("显示为"))
        );
        assert!(
            calc_tree2.content.contains("显示为 42"),
            "计算器没有显示 42：{:?}",
            calc_tree2.content
        );

        let shot3 = run_tool(
            &reg,
            &ctx,
            "screenshot",
            json!({ "save_path": screenshot_dir.join("real_input_calculator.png").to_string_lossy() }),
        )
        .await;
        assert!(shot3.ok, "screenshot 失败：{}", shot3.content);
        let shot3_path = screenshot_file_path(&shot3.content).expect("截图结果没有路径");
        assert!(png_ok(&shot3_path));
        println!("[verify] 计算器 42 截图：{shot3_path}");

        println!("[verify] 截图汇总：{shot1_path}\n{shot2_path}\n{shot3_path}");
        // 显式释放：关闭记事本/计算器窗口并清理进程（panic 时也会由 Drop 兜底）。
        drop(guard);
    }
}
