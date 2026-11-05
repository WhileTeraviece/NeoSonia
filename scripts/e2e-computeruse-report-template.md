# NeoSonia 计算机使用 E2E 报告模板

> 这份是人工可读的报告骨架；正式运行时由
> `crates/neosonia-tools/tests/e2e_computeruse.rs` 自动生成到 `logs/e2e-computeruse.md`。
> 用例编号与内容固定，失败用例追加「失败现场」。

## 概述

- 测试目标：vX.Y 成品 exe（`target\debug\neosonia.exe`）
- 数据目录：`--home E:\NeoSonia\sandbox`（独立 sandbox，不污染真实配置/聊天）
- 环境：Windows 11 / exe 构建时间 / 屏幕分辨率 / 是否有安全提示弹窗
- 方式：neosonia-tools 的 `ui_* / mouse_* / keyboard_* / screenshot` 真实模拟输入 + UIA 读回断言

## 用例汇总

| # | 用例 | 结果 | 证据（UIA 读回 / 截图） |
| --- | --- | --- | --- |
| 01 | 全局热键 | 跳过 | 低级键盘钩子忽略 SendInput 注入事件，无法用模拟输入验证 |
| 02 | 启动 + 基线 | 通过/失败 | 窗口 hwnd/pid/标题；`02-launch-baseline.png` |
| 03 | 设置页折叠面板 | 通过/失败 | 点击「设置」→ 展开「MCP 服务器」/「技能」后出现的文本节点 |
| 04 | 连接其他设备 → 设备页 | 通过/失败 | 顶栏「连接其他设备」点击结果；设备页「扫码配对/配对码/访问地址」文本 |
| 05 | 聊天输入「你好」 | 通过/失败 | 「你好」用户气泡文本节点 |
| 06 | 拖动标题栏 | 通过/失败 | 拖拽前后 Window bounds（位移 ≥10px） |
| 07 | 关闭 app | 通过/失败 | WM_CLOSE 后 `ui_window_list` 中窗口消失 |

**N 通过 / M 失败 / K 跳过**

## 01 全局热键（跳过）

- 原因：app 的全局快捷键走低级键盘钩子，钩子忽略带 `LLKHF_INJECTED` 的注入事件；
  SendInput 发出的按键一定带该标志，因此模拟输入无法触发 NeoSonia 自己的 Alt+Space。
- 人工验证方式：`NEOSONIA_HOTKEY_ALLOW_INJECTED=1` 调试开关 + 人工按一次，或查日志确认钩子已安装。

## 02 启动 app、等待窗口、基线截图

- 步骤：spawn `neosonia.exe --home <sandbox>` → 轮询 `ui_window_list`（标题 NeoSonia 且 pid=本次启动进程）→ `ui_focus_window` → 等 UIA 树出现「连接其他设备」且标题不带「(未响应)」→ screenshot。
- UIA 证据：窗口 hwnd / pid / 标题、UI 渲染完成标记。
- 截图：`logs/e2e-screenshots/02-launch-baseline.png`

## 03 设置页折叠面板（新布局：底栏 5 项 + 右侧 mode chip）

- 步骤：进入设置页（优先点底栏「设置」；失败则点顶栏 mode chip「日常/编程/计算机使用」）→ 点「MCP 服务器」header → 验证 body 出现 → 点「技能」header → 验证 body 出现。
- UIA 断言：
  - MCP 展开后含 `启用 MCP 服务器` / `Model Context Protocol` / `还没有配置 MCP 服务器` 之一；
  - 技能展开后含 `技能目录` / `还没有安装技能` / `打开目录` 之一。
- 截图：`03a-settings-page.png`、`03b-settings-mcp.png`、`03c-settings-skills.png`

## 04 顶栏「连接其他设备」→ 设备页

- 步骤：点顶栏 share_2 按钮（「连接其他设备」，在时钟左边）→ 验证页面切换。
- UIA 断言：`扫码配对` / `配对码` / `访问地址`（设备页独有，设置页不会误判）。
- 截图：`04-remote-panel.png`

## 05 聊天输入「你好」

- 步骤：点底栏「对话」→ 定位 Edit「输入指令，回车发送」并聚焦 → `keyboard_type("你好")` → `keyboard_key(enter)` → 等 3s。
- UIA 断言：控件树里出现「你好」文本（用户气泡）。
- 截图：`05-chat-user-bubble.png`

## 06 拖动窗口标题栏

- 步骤：读 Window bounds → `ui_drag` 从标题栏中段向左拖 40px → 重读 bounds。
- UIA 断言：窗口 x 位移 ≥10px。
- 截图：`06-drag-after.png`

## 07 关闭 app

- 步骤：`PostMessage(WM_CLOSE)` → 最多等 8s → 超时 `taskkill /PID <自己的 pid> /T /F`。
- UIA 断言：`ui_window_list` 中该窗口消失。
- 说明：`kill_process` 工具会拒绝杀 NeoSonia 自身，故测试用 WM_CLOSE/taskkill 仅针对本次启动的 pid。

## 环境限制

- 桌面存在系统安全提示（UAC/防火墙 PickerHost 弹窗）时，所有合成键鼠都会被 UIPI 挡掉，测试会失败——先确保没有弹窗。
- 多实例互相 Stop-Process/抢前台会污染结果：本套件只认自己 spawn 的 pid，不按标题抓窗口。
- 首次启动 wgpu/字体初始化可能需要 20~60s，黑屏期截图无证据价值，已用「UIA 树出现顶栏按钮」作为就绪条件。
- 全局热键不在自动化范围（见 01）。

## 失败现场

每个失败用例自动留下：

- `logs/e2e-failures/<case>.tree.txt`：失败瞬间的完整 UIA 树 dump（含 role/name/bounds/patterns）
- `logs/e2e-screenshots/<case>-fail.png`：失败瞬间截图

排查顺序：先看截图确认页面状态 → 再看 tree 里控件名/bounds 是否与预期一致（布局改动时最常见）→ 再看 `[tool-err]` 行。
