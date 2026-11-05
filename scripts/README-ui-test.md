# NeoSonia UI 测试脚本（v0.4）

真机 UI 驱动、性能基准、端到端冒烟。全部是 Windows PowerShell 5.1 脚本，位于 `scripts/`。

| 文件 | 作用 |
| --- | --- |
| `ui-drive.ps1` | UI 驱动库（dot-source 使用）：找窗口、点击、滚轮、键盘输入、截图、启停 app、读日志 |
| `perf-bench.ps1` | 性能基准：冷启动、帧构建耗时、页面切换延迟、帧率，输出 JSON + markdown |
| `e2e-smoke.ps1` | 端到端冒烟：基线截图、设置页、MCP 分组、折叠面板、设备页可达，输出截图路径 |

## 运行前提

- Windows PowerShell 5.1（`powershell.exe`，不是 PowerShell 7 的 `pwsh`）。
- app 可执行文件：默认 `E:\NeoSonia\target\debug\neosonia.exe`，可用 `-AppExe` 覆盖。
  **性能脚本要求这个构建含性能日志**（`view built` / `page switch` / `frame rate`），
  旧构建会明确报“无数据”（不会编造数字）。
- 独占这台机器的前台：其它进程如果同时 `Stop-Process neosonia` 或启动第二个 NeoSonia 实例，
  `FindWindow` 会拿到别人的窗口，点击/截图/性能数据都不可信。跑之前确认没有别的 agent 在操作 app。
- 默认 home 是 `E:\NeoSonia\sandbox`（sandbox 里 `config/settings.toml` 的 `[mcp]` 段配了 demo 服务器）。

## 用法

```powershell
# 性能基准（约 40 秒：慢启动 + 预热 + 15 次导航点击 + 14 秒采样）
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\perf-bench.ps1

# 端到端冒烟（约 40 秒，输出 7 张截图）
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\e2e-smoke.ps1

# dot-source 驱动库，自己写小测试
. .\scripts\ui-drive.ps1
$app = Start-NeoApp -HomeDir "E:\NeoSonia\sandbox" -Profile
Invoke-NeoFocus | Out-Null
Invoke-NeoClickLogical 177 68          # 点导航「设置」（逻辑坐标，自动按 DPI 换算）
Save-NeoScreenshot "E:\NeoSonia\logs\ui\demo.png"
Stop-NeoApp -App $app
```

## ui-drive.ps1 主要函数

| 函数 | 说明 |
| --- | --- |
| `Get-NeoWindow` | 返回 `Hwnd/Left/Top/Width/Height/Dpi/Scale/LogicalWidth/LogicalHeight`；窗口不在返回 `$null`。FindWindow 失败时会按标题枚举兜底 |
| `Invoke-NeoFocus` | 强制前台：置顶 + `AttachThreadInput` + `SetForegroundWindow`。**截图/点击前先调它** |
| `Invoke-NeoClick <x> <y>` | 物理像素点击（相对窗口左上角） |
| `Invoke-NeoClickLogical <x> <y>` | 逻辑像素点击（96 DPI 基准，内部按窗口 DPI 缩放）——测试脚本都用这个 |
| `Invoke-NeoWheel <notches>` | 在窗口中心滚动（正=下，负=上）。先 hover/MOVE 再滚（见下方“踩坑”） |
| `Send-NeoKeys <text>` | `SendInput` + `KEYEVENTF_UNICODE` 输入文本（支持中文） |
| `Send-NeoHotkey <spec>` | 例如 `Alt+Space`；**无法触发 NeoSonia 自己的全局热键**，见限制 |
| `Send-NeoKeyScroll <key>` | 键盘滚动备选：PageDown/PageUp/Home/End |
| `Save-NeoScreenshot <path>` | 截窗口区域 PNG（自动建目录），返回路径 |
| `Start-NeoApp [-HomeDir] [-Profile]` | 杀旧实例、清日志、启动、等 `dock attempt`，返回 `Process/Log/StartupMs/Window/DockLine`。`-Profile` 会设 `NEOSONIA_PROFILE=1` |
| `Stop-NeoApp -App $app` | 结束 app，并递归清理它的子进程（含 MCP 的 node server）；只清本 app 的子进程，不误杀别的实例 |
| `Get-NeoLogLast / Wait-NeoLog / Wait-NeoDock / Wait-NeoPage` | 读日志 / 等日志 / 等 dock / 等 `page switch page=X`（可只查本次新增行） |
| `Get-NeoLogLineCount` | 当前日志行数（用于“只看本次操作之后新增的日志”） |
| `Test-NeoHotkeyHookInstalled` | 从日志判断热键钩子状态：`键盘钩子` / `退回模式（global-hotkey）` / `$null` |

坐标约定：带 `Logical` 的是逻辑像素（窗口内部布局坐标，96 DPI 基准），不带的是物理像素。
本机是 250% 缩放（DPI 240、窗口 1060x1901 物理 / 424x760.4 逻辑），换缩放不用改测试。

## perf-bench.ps1 口径

- 冷启动：`Start-Process` 计时到日志出现 `dock attempt`；同时给出“日志首行 -> dock”的纯 app 内部耗时。
- `view built`：`NEOSONIA_PROFILE=1` 时每帧一条（`ms=` 字段），同时给“全部（含启动首帧）”与
  “稳态（预热之后）”两组 p50/p95/max/avg。
- `page switch`：每次导航点击一条，按页面分组统计 p50/p95/max。
- `frame rate`：每 5 秒一条（`NEOSONIA_PROFILE=1` 才有）；另用 view built 行数/运行时长估算平均帧率。
- 输出：`logs/perf-report.json`（机器可读）+ `logs/perf-report.md`（表格 + 达标判定）。
- 阈值在脚本里（默认：冷启动 <= 4000ms、view p50 <= 8ms、p95 <= 16.7ms、page switch p95 <= 120ms、帧率 >= 55fps），
  按需改 `$thresholds`。

## e2e-smoke.ps1 校准参数（build 19:11:28 实测，250% DPI）

脚本顶部参数是 250% 缩放 + sandbox 默认配置下的实测值。改主题/字体缩放/设置项数量后，
先跑一次看 `logs/ui/e2e/` 里的截图，再按图微调：

| 参数 | 实测值 | 含义 |
| --- | --- | --- |
| `-NavY` / `-NavSettingsX` | 68 / 201 | 导航条 y 与「设置」tab 的 x（逻辑像素）；5 个 tab 的 x 依次约 19/62/106/157/201 |
| `-DeviceScrollNotches` | 48 | 从设置页顶部滚到「连接其他设备（手机/平板）」行所需滚轮格数（每格 40 逻辑像素） |
| `-DeviceButtonX/Y` | 345 / 221 | 「打开设备页」按钮位置；没点中会自动用顶栏分享图标兜底（逻辑 x=288） |
| `-McpScrollNotches` | 100 | 从顶部滚到「MCP 服务器」分组所需格数（90 格是工具权限，120 格是子代理+技能） |
| `-McpHeaderX/Y` | 160 / 210 | 「MCP 服务器」标题行的点击位置 |
| `-BottomScrollNotches` | 200 | 滚到底所需格数（会 clamp，给大一点没关系） |
| `-SkillHeaderX/Y` | 160 / 110 | 滚到底后「技能」折叠标题的点击位置 |

顶栏图标（逻辑坐标，y=24）：分享/连接其他设备 288、时钟 327、眼睛 363、关闭 401。

## 性能口径补充

- `frame rate` 是每 5 秒一档的渲染帧数/时间。**空闲时 app 只靠 250ms Tick 触发重绘（≈4fps 是设计值）**，
  所以空闲 fps 不能直接当卡顿证据；要测动画/流式输出期间的帧率才有意义。
- `page switch` 是“点击导航 → 下一帧构建完成”的耗时；实测 p50≈1ms、p95≈4.5ms（250% DPI、本机），
  说明页面切换本身不是瓶颈。
- 冷启动（进程启动 → `dock attempt`）本机实测 16–30s 波动较大，受机器负载/并发影响，建议在独占环境取中位数。

## 踩坑记录（真机）

1. **截图前必须强制前台**：app 刚 dock 时窗口可能被别的窗口盖住，`CopyFromScreen` 会截到别的程序。
   `Invoke-NeoFocus` 用 `AttachThreadInput` + `SetForegroundWindow` 解决。
2. **滚轮需要先 hover**：fenestra 的滚动容器只有收到鼠标在该窗口内的移动事件后才响应滚轮；
   `Invoke-NeoWheel` 会先 `SetCursorPos` 再发一次 `MOUSEEVENTF_MOVE`，然后分块发滚轮（每块 <= 6 格）。
   如果某天滚轮仍不生效，备选：`Send-NeoKeyScroll PageDown`（先点一下内容区给焦点）或拖右侧滚动条。
3. **`FindWindowW(null, "NeoSonia")` 偶尔返回 0**（新窗口刚创建/重建时），`Find()` 里有按标题枚举的兜底，
   `Get-NeoWindow` 还会再重试一次。
4. **Windows canonicalize 的 `\\?\` 前缀**：Node 等外部程序不认，脚本里传路径给 node 时要去掉（以前踩过）。
5. **多个 NeoSonia 实例**：`FindWindowW` 只按标题找，无法区分实例；并行测试时结果不可信，必须独占。

## 全局快捷键为什么不能自动化验证

app 的 Windows 全局快捷键走键盘钩子（日志 `全局快捷键已启用（键盘钩子）`），钩子会忽略带
`LLKHF_INJECTED` 的注入事件；`SendInput` 发出的按键一定带这个标志，所以 **`Send-NeoHotkey` 无法触发
NeoSonia 自己的 Alt+Space**。要验证热键只能：

- 检查日志里钩子已安装（`Test-NeoHotkeyHookInstalled`）；
- 用 `--ask` 模式让 agent 走 `screenshot` 工具验证截图链路；
- 人工按一次。

`Send-NeoHotkey` 仍保留，对不筛注入事件的普通程序/系统组合键有用。
