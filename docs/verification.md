# 验收记录（NeoSonia v0.1）

记录日期：2026-10-31。机器：Windows 11，i7-6700HQ / 16GB，显示器 3840x2160 @ 250%，
集显 Intel HD 530 + 独显 GTX 960M。

## 1. 构建

| 项 | 结果 |
| --- | --- |
| `cargo check --offline --workspace` | 0 error（依赖 447 个包全部离线编译） |
| `cargo build --offline -p neosonia-app` | 成功，产物 `target\debug\neosonia.exe` |
| 代码规模 | 55 个 Rust 文件 / 31,070 行（含 vendored fenestra-shell 补丁） |

## 2. 测试

`cargo test --offline --workspace` → **170 passed / 0 failed / 10 ignored**：

| crate | 通过 | 覆盖内容 |
| --- | --- | --- |
| neosonia-core | 54 | SSE 解析（按字节喂入/跨 chunk/[DONE]）、tool_calls 累积、usage/cached、记忆 CRUD + 分词 + 打分 + 淘汰、人设 md/toml、历史裁剪、取消补历史、子代理并发上限、内置工具 |
| neosonia-tools | 65 (+10 ignored) | 38 个工具的 schema/参数容错、命令超时/取消/后台任务/管道、文件搜索 glob+内容、路径沙箱、截图参数、键盘映射、HTML 解析、UIA 控件类型映射、CDP 目标解析 |
| neosonia-mail | 33 | MIME 多段/编码头/HTML 回退/附件、References/Message-ID、strip_html、白名单拦截、回复头构造、SMTP TLS 模式推断、去重文件、轮询停止、wait_for 取消 |
| neosonia-app | 18 | 设置草稿 JSON Pointer 写入/整数归一化、流式气泡合并、主题色板解析、快捷键解析/规范化、面板纯函数 |

真机（ignored，需要网络/桌面）：
- `NEOSONIA_VERIFY=1 cargo test -p neosonia-tools -- --ignored` → 5/5：截图（3840x2160 → 缩放到 1600 边）、
  web_search 回退 Bing 拿到真实结果、web_fetch/download、65 秒长命令流式 + 5.7 秒取消、search_files 命中。
- `NEOSONIA_CDP_TEST_PORT=9333 cargo test -p neosonia-tools --lib real_ -- --ignored` → 2/2：
  CDP 真机（targets / eval 改标题 / selector 与 text 两种点击 / 截图），
  UIA 真机（Edge 窗口控件树，InvokePattern 点击"刷新"）。

## 3. 端到端（真实模型）

```
neosonia.exe --home <sandbox> --ask "先一句话自我介绍，然后执行命令 echo neosonia-ok，把输出告诉我"
```
观察到的输出（节选）：

```
[user] 先一句话自我介绍，然后执行命令 echo neosonia-ok，把输出告诉我
我是 NeoSonia，一个能读写文件、执行命令、操作界面和访问网络的常驻桌面 AI Agent。...
[usage] in=11735 out=72
[tool] run_command echo neosonia-ok
[tool] run_command ok 5207ms
[usage] in=11912 out=35
--- turn finished (cancelled=false) ---
```

模型：DeepSeek V4.1-Flash（`deepseek-flash`，1M 上下文，思考模式 + 工具调用 + 图片输入能力）。

## 4. UI 真机

- 浮窗：右侧停靠（物理 2750,129 / 1060x1901，逻辑 424x760 @250%），DWM 圆角、无边框、置顶。
- 中文字体（Microsoft YaHei UI）在真实窗口渲染正常；headless 渲染只有内嵌 Inter（中文为方框，仅用于布局回归）。
- 截图：`logs/screenshot-chat.png`（对话页空态引导）、`logs/screenshot-settings.png`（设置页）。
- 首次启动无 API key 时自动打开设置页并提示配置（已验证）。

## 5. 环境相关（不是代码问题）

1. **图形后端**：本机 wgpu 选 Vulkan 时窗口呈现为空白（Intel HD 530 + Vulkan swapchain 合成问题），
   程序已在 Windows 默认锁 `WGPU_BACKEND=dx12`，probe 与主窗口均正常出图。
2. **HTTPS 中间人**：本机装了卡巴斯基，其证书不在 rustls/webpki 根库里，导致 `rustls` 请求
   报 `invalid peer certificate: UnknownIssuer`。已把 reqwest 切到 **native-tls**（Windows 证书库），
   之后模型请求、web 工具、图床下载全部正常。
3. **合成输入**：本机桌面上有一个全屏置顶的 `Black Screen`（pythonw，疑似用户的窗口锁定/护眼工具），
   它会拦截 `SendInput`/`SetCursorPos` 合成的鼠标键盘，所以"程序自己点自己"无法验证；
   `--ask` 无界面链路、UIA InvokePattern 点击、工具层全部已验证。
4. **缩放**：屏幕是 250% 缩放（3840x2160 / 2.5），424 逻辑像素 = 1060 物理像素；
   停靠按 `monitor.size()` + `scale_factor` 换算，多屏/不同缩放时同样成立。

## 6. 未完成/待补

- **邮件真账号联调**：IMAP/SMTP 代码与 33 个单测就绪，连接测试按钮可用；需要用户填真实
  授权码后才能做"收信 → 白名单判断 → 自动回复"的真机验证（本会话没有用户邮箱凭据）。
- **视觉输入**：默认模型支持图片输入，`screenshot` 工具可返回 base64，但"把截图作为图片消息
  喂给模型"的桥接还没接（当前只回路径，模型看不到像素）。
- **release 构建**：只做了 dev 构建（本机 release + thin LTO 预计 20+ 分钟），发布前建议
  `cargo build --release`。
- **托盘/热键在非交互桌面下的行为**：托盘创建成功（无告警），菜单点击因同一输入限制未真机点过。

---

# 第二阶段（v0.2）验收记录

日期：2026-11-01。环境：Windows 11，3840×2160 @ 250% DPI，DX12 后端，DeepSeek V4.1-Flash。

## 1. 编译与测试

| 命令 | 结果 |
| --- | --- |
| `cargo build --offline --workspace` | 0 error |
| `cargo test --offline --workspace` | **295 passed / 0 failed / 13 ignored**（app 31 + core 79 + mail 33 + tools 100 + web 52） |

13 个 ignored 是需要真机的测试（截图/键鼠/UIA/CDP/联网搜索/索引扫描），已单独跑过：
- `cargo test -p neosonia-tools -- --ignored --nocapture verify_` → **6 passed**（聚合搜索、screenshot 标记、capture_primary、screen_metrics、wait_for_window、长命令 67.6s/取消/stdin、index 真机）
- `cargo test -p neosonia-tools --lib real_ -- --ignored --test-threads=1` → **4 passed**（CDP + UIA + 真机键鼠拖拽）

## 2. 视觉输入 E2E（真实模型）

`neosonia.exe --home sandbox --ask "截个屏，看一下屏幕上是什么"`：

```
[tool] screenshot ok 13110ms: [ok] 截图已保存
[vision] 已把 1 张图片发给模型（screenshot）
[usage] in=29420 out=138
```

模型回答准确描述了当时的真实屏幕（DeepSeek Harness 窗口、NeoSonia 浮窗、记事本 `6*7=`、保存对话框、桌宠余额），
证明 `[screenshot:<路径>]` → 缩放 → 图片消息 → 模型看图 全链路可用。
日志：`logs/vision-e2e3.log`。

期间修掉真 bug：同一批并行工具调用里提前插入图片消息会形成
`assistant(tool_calls) → tool → user → tool` 非法序列（HTTP 400），已改为整批工具消息入历史后再注入图片。

## 3. 局域网远程访问 E2E

桌面端「设备」页：二维码 + 8 位一次性配对码 + 倒计时（`logs/ui/ui-remote.png`）。

`scripts/verify-remote.ps1` 实测（LAN 地址 `http://192.168.1.100:8787/pair#EXAMPLE1`）：

| 检查 | 结果 |
| --- | --- |
| 未鉴权 `GET /api/sessions` | 401 |
| `GET /`（内嵌 SPA） | 200，6468 bytes，CSP/no-referrer/nosniff/favicon data URI |
| `POST /api/pair` | 200，64 位 hex token + device |
| 配对码复用 | 401（一次性） |
| `GET /api/me` / `/api/sessions` | 200，设备名/版本/会话列表正确 |
| `POST /api/chat` | 202 `{"turn_id":"…"}` |
| `GET /api/screenshot/%2e%2e%2fdevices.json` | 400（防目录穿越） |
| WebSocket `/ws` | hello 帧 + `reasoning_delta`/`text_delta` 流式事件（`{"type":…,"data":…}`） |

SPA 视觉：Firefox 截图配对页 `logs/remote/spa-pair.png`（Nova 风格、中文、零 emoji、配对码从 fragment 预填）；
Edge+CDP 截图聊天页 `logs/remote/spa-chat.png`（未生成时用 `crates/neosonia-web/scripts/verify.ps1` 的产物 `logs/screenshot-web.png`）。

注意：Windows 首次监听 `0.0.0.0` 会弹出一次防火墙授权；点"允许"后手机才能连上（只测本机可拒绝）。

## 4. 桌面 UI 验收（`logs/ui/`，1060×1901 = 424 逻辑宽 @2.5x）

| 截图 | 证明 |
| --- | --- |
| `ui-settings.png` | 设置页底部两个按钮不再错位：表单区独立滚动 + footer 固定（状态点/文字 + 重新载入/保存右对齐） |
| `ui-history.png` | 历史页：搜索框、会话列表（标题/条数/时间/人设/预览）、删除、新建、返回；搜索命中入口 |
| `ui-remote.png` | 设备页：开关、端口/仅局域网 chip、访问地址、二维码、8 位配对码 + `剩余 09:44` 倒计时、复制 |
| `ui-chat-math-final.png` / `ui-final-chat.png` | Markdown（标题/有序列表/加粗）+ 公式：`$E=mc^2$` 行内、`$$ \frac{a+b}{c} $$` 块级卡片 |

**公式渲染的实现约束（已修）**：分数堆叠/上下标小字这类嵌套元素在窄栏 + 滚动容器里会触发布局递归爆栈
（`thread 'main' has overflowed its stack`，进程直接退出）。现在采用**压平成一行等宽文本**：
行内公式嵌在正文里（`E=mc^2`），块级公式放在居中卡片里（`(a+b)/c`），语义保留、稳定不崩。

聊天还支持：图片缩略图（用户上传/截图）、工具卡片内显示截图缩略图（"AI 看到了什么"）、思考过程折叠、
消息进场动画、流式增量 60ms 合批、只渲染最近 80 条（更早的进历史页）。

## 5. 工具（75 个）

`tools` crate 57（文件 11 + 索引 3 + 命令 6 + 屏幕/输入 16 + UIA 8 + CDP 4 + 网页 4 + 系统 6 - 重叠计法见 `tool_catalog()`）
+ `neosonia-mail` 8 + core 内置 10（记忆 4 + 技能 4 + 子代理 2）= **75**。

真机证据：
- 聚合搜索「Rust 异步」2.7s → 百度 10 / 搜狗 3 / 必应 10 / 360 7，前 5 条均带来源引擎（不依赖 SearXNG）
- 本地索引：`reindex_files` 3 文件 20ms（扫描 5ms），`find_files "index-demo"` 3ms 命中 4 条（不依赖 Everything）
- 真机键鼠：记事本 `keyboard_type("6*7=")` → UIA 读回「行 2，列 1 / 5 个字符」；`mouse_drag` 拖选+Delete 12→0；
  `ui_act type` 5→12；`ui_drag` 全选+输入 XYZ → 3 字符；计算器 `显示为 42`
  （截图：`data/screenshots/real_input_{notepad,ui_drag,calculator}.png`）
- 关键环境结论：Windows 安全中心防火墙提示（PickerHost）在另一桌面时，所有合成键鼠都会被 UIPI 吞掉；
  先确保没有安全提示窗口再跑"计算机使用"。

## 6. 技能 / 会话历史 / 自启

- 技能：`sandbox/skills/{screen-review,mail-draft}/SKILL.md` 首启自动写入 + `.builtin-seeded`；设置页可从文件/URL 安装、卸载
- 会话历史：`data/conversations.sqlite3` 落库（标题=首条消息前 30 字），历史页可搜索/载入（消息回填到聊天流）/删除
- 开机自启：`HKCU\...\Run` 写入 `"neosonia.exe" --autostart`；自启时只跑后端不打开窗口（`boot_hidden`，不污染持久化设置）；
  手动启动立即显示窗口
- 快捷截图：`Ctrl+Alt+A`（可改）抓主屏 → 附件 + 显示窗口并聚焦

---

# 第三阶段（v0.3）验收记录

日期：2026-11-01。需求：后台命令 + 状态查询、鼠标左/右/中键与双击滚轮、带坐标网格的截图、MCP 支持、屏幕活动提示（HUD）。

## 1. 编译与测试

| 命令 | 结果 |
| --- | --- |
| `cargo build --offline --workspace` | 0 error |
| `cargo test --offline --workspace` | app 32 + core 82 + mail 33 + tools 108 + web 52 + mcp 34 = **341 passed / 0 failed / 17 ignored** |

真机 ignored 测试：
- `neosonia-tools`：`--ignored verify_` → **8 passed**（含新增的网格截图、记事本鼠标、后台任务状态流）
- `neosonia-mcp`：`--ignored live_demo` → **2 passed**（stdio + http/SSE）

## 2. 后台命令 + job_status（tools 真机）

```
run_command {background:true, command:'node -e "setInterval(()=>console.log(Date.now()),1000)"'}
→ job_status：
任务：j7754ee6e
状态：running
已运行：3.0s
最后 20 行输出：1793501943948 / 1793501944952
汇总：运行中 1，已结束 1。
→ kill_job 后：状态：cancelled，退出码：1，运行中 0。
```

## 3. 鼠标按键 / 双击 / 滚轮（tools 真机，UIA 断言）

- 左键点击定位：点文本 35% 处后输入 `[LEFT]` → 字符数 66→72，保存后内容 `... iota [LEFT]kappa lambda mu`（光标定位到中间，不是覆盖）
- 双击选词：`double=true` 后输入 `WORDSEL` → 字符数 72→75，原选中词被替换
- 右键菜单截图：`%TEMP%\...\neosonia-tools-verify-notepad-*\data\screenshots\shot-20261101-110513-15e58e6a.png`（剪切/复制/粘贴/全选可见）
- 滚轮：`direction=down amount=8` 前后两张截图内容上移（line-01..22 → line-03..24）

## 4. 带坐标网格的截图（tools 真机）

`screenshot {grid:true}` → 1600x900：
- 100px 细白线 / 500px 粗橙线，上边 x 刻度 0/500/1000/1500/2000/2500/3000，左边 y 刻度 500/1000/1500/2000
- 返回文本带 `[screenshot:<绝对路径>]` + "网格 100px/500px、原点屏幕左上角、图上坐标 = 物理坐标 × factor、scale=2.50"
- 截图：`%TEMP%\...\neosonia-tools-verify-shot-*\data\screenshots\shot-20261101-110545-7b7cf217.png`

## 5. MCP（端到端，app 真实调用）

配置（`sandbox/config/settings.toml`）：
```toml
[mcp]
enabled = true
[[mcp.servers]]
name = "demo"
transport = "stdio"
command = "node"
args = ["E:/NeoSonia/scripts/mcp-demo-server.mjs"]
```

`neosonia.exe --home sandbox --ask "先调用 mcp_list_servers … 然后用 mcp_demo_add 计算 2 加 3"`：

```
[mcp] 1 台服务器 / 3 个工具
[mcp] demo -> ok=true tools=3
[tool] mcp_list_servers ok 0ms: MCP 客户端：共 1 台服务器，已连接 1 台，工具 3 个（全局上限 100 个）。
[tool] mcp_demo_add {"a":2,"b":3}
[tool] mcp_demo_add ok 1ms: {"sum":5}
模型回答：已连接 demo（pid 15004），提供 mcp_demo_echo、mcp_demo_add、mcp_demo_sleep；add(2,3) 返回 {"sum":5}。
```

GUI 启动日志同样有 `MCP 服务器已连接 server=demo tools=3` / `MCP 已启动 servers=1 tools=3`。
MCP 客户端另测：stdio 超时（1s 上限）、重连后 `add(20,22)=42`、shutdown 后中文"未连接"错误；http 传输（SSE 回包）`add` 返回 5。

## 6. 屏幕活动提示（HUD）

实现：主线程惰性创建 5 个置顶/点击穿透/不抢焦点的分层窗口（4 条边 + 顶部胶囊），直接画到窗口 DC（不依赖消息泵），由 app Tick 处理 0.9s 自动隐藏；设置 `ui.computer_activity_indicator` 可关。

真机帧（3840x2160 全屏截图，像素取样验证）：
- 截图工具执行中：屏幕四边像素 = RGB(173,147,239) 紫色（#b89cff），顶部胶囊「NeoSonia 正在查看屏幕」
- 操作类工具（ui_window_list）结束：屏幕四边 = RGB(91,141,97)（橙色 #ff9565 混合后的取样），胶囊「NeoSonia 已完成」
- 帧文件：`logs/ui/hud3-1.png`（执行中）、`logs/ui/hud3-3.png`（完成）

## 7. 已知限制

- MCP：改配置需重启应用；服务器反向请求（sampling/roots）不支持；MCP 结果里的图片不转视觉输入；restart 后已删除的旧工具名仍留在注册表（调用返回中文错误）；同服务器请求串行。
- Windows 上 `command="npx"` 不可用，需 `command="cmd"` + `args=["/c","npx",...]`（或 `command="node"` 直接跑包入口）。
- 后台命令被 kill 后，Windows 有时残留一个空的 conhost 控制台窗口（不影响功能）。
- HUD 的边条/胶囊是 GDI 绘制，非亚像素抗锯齿；圆形/文字在极端 DPI 下会略显粗糙。

---

# 第四阶段（v0.4）验收记录

日期：2026-11-01。需求：前端性能、附件条修复、Alt+Space 热键修复 + Alt+Space+S 截图、托盘首开不卡、加载态、手动终止、Thunderbird 式 OAuth2 邮箱、提示词隔离模式、命令审查（1-4 分 + 记忆）、思考强度滑块、设备入口、计算机使用自测。

## 1. 编译与测试

| 命令 | 结果 |
| --- | --- |
| `cargo check --offline --workspace` | 0 error |
| `cargo test --offline --workspace` | app 35 + core 100 + mail 53 + tools 108 + web 67 + mcp 34 = **397 passed / 0 failed / 18 ignored** |

真机 ignored 测试另行跑过（tools 8 / mcp 2 / mail 1 device-code）。

## 2. 热键修复（用户报的核心 bug）

根因（两层）：
1. v0.3 用两个 `global-hotkey` 服务分别轮询，**共用一个全局事件 channel**，先轮询的服务把另一个服务的按键事件取走 → Alt+Space 事件被截图服务的 poll 吞掉；
2. 旧的"隐藏窗口"用 `SW_HIDE`，实测 Windows 上真正 Hide 之后 `RegisterHotKey` 的热键就再也收不到消息（第一下有效、之后全失效）。

修复：
- 改成自己的 `RegisterHotKey` + **独立快捷键线程**（隐藏消息窗口 + 消息循环），主线程只读位掩码；
- "隐藏"改为**把窗口移到屏幕外 (-12000,-12000)**，保留热键注册；"显示"重新停靠 + 聚焦；
- 截图改为后台线程（之前 `xcap` 抓屏在主线程同步执行 6-10 秒，会卡住 UI 并让后续热键看起来"失灵"）；
- 组合键：按主键后 1.5s 内按 `S` = 立即截图并附加到聊天框（`GetAsyncKeyState` 低位判定，快速点按不会漏）。

真机验证（注入键盘事件放行后）：
```
initial: onscreen(2750,129)
Alt+Space #1 -> offscreen(-12000)
Alt+Space #2 -> onscreen(2750,129)
Alt+Space #3 -> offscreen(-12000)
Alt+Space #4 -> onscreen(2750,129)
chord: Alt+Space then S -> pos=onscreen shots=1
Ctrl+Alt+A ×2 -> shots 2
```

## 3. 性能（用户报"切页面卡 1-2 秒、滚动卡"）

| 指标 | 修复前 | 修复后 |
| --- | --- | --- |
| 页面切换（点击导航→下一帧完成） | 1-2 秒 | **p50=1.0ms / p95=4.5ms**（干净日志 n=16：Chat 0.5-1.5 / Mail 0.6-1.0 / Memory 0.7-1.2 / Persona 2.0-3.1 / Settings 1.7-4.5；另一轮 n=8 p50=3.2ms、p95=4.4ms） |
| 聊天列表 | 每帧构建 80 条消息 | **虚拟化**，只构建视口内几条 |
| 远程 SPA（手机） | 首屏无骨架、逐条 append | `t_first_paint=868ms`、`t_sessions_fetch=104ms`；首屏 200 条 + 分页加载更早 + 骨架屏 |
| 设置页首帧 | 一次构建全部 10 个 section | 折叠面板（默认折叠 5 个），首帧只建标题行 |
| 托盘响应 | 等 Tick（最多 250ms+） | 托盘线程直接投递（40ms 轮询） |
| 截图快捷键 | 主线程阻塞 6-10s | 后台线程，UI 不卡 |

其他：聊天快照按 `items_version` 缓存、流式增量仍 60ms 批处理、主题缓存、执行中脉冲点 + 2px 加载条、`NEOSONIA_PROFILE=1` 输出 `view built`/`page switch`/`frame rate` 供基准测试。

## 4. 提示词隔离模式

三种模式（`daily` / `coding` / `computer_use`）按契约注入（详见 `docs/contracts-v04.md`）；模型可用 `switch_mode` 工具主动切换，设置页有三张卡片，顶栏有模式 chip。
- daily：人设 + 记忆 + 文件/搜索；计算机操作段按需（本轮调用过截图/键鼠/UIA 后下一轮注入一次）
- coding：无人设；编程规范 + 文件/命令；计算机操作按需
- computer_use：无人设；计算机操作 + 文件 + 命令全展开
技能安装后 LLM 自动分类写回 `SKILL.md` 的 `modes`，只注入当前模式适用的技能。

## 5. 命令审查（1-4 分）

new `core/review.rs`：独立无人设评分提示（temperature 0，max_tokens 200，可配 model）、class 分类器、`data/review_state.json` 的 allow/deny TTL 记忆。
- 1 分直接执行；2 分确认后同类 1 天自动执行；3 分每次确认；4 分强调风险、拒绝后可"1 天内拒绝同类"
- 桌面端弹窗（等级色/原因/同类标识/三个按钮），局域网设备页只读展示

## 6. 邮箱 OAuth2

- Microsoft device code 真机请求拿到 `user_code: B4R8VV67F`（900 秒有效，未完成授权）；
- Google loopback PKCE 授权 URL 形态已单测校验（S256 challenge、redirect_uri 127.0.0.1 随机端口、mail.google.com scope）；
- IMAP/SMTP 在 `mail.oauth.enabled` 时自动 XOAUTH2；设置页有「用 Microsoft 登录 / 用 Google 登录」按钮。

## 7. 其余

- 附件条：独立 × 按钮、文件名中间省略（12+…+8 字符，支持中文/emoji）；
- 思考强度滑块：low/medium/high 三档；
- 设备入口：顶栏分享图标 / 托盘菜单 / 设置页按钮 / 未启用时页头提示条；
- 手动终止：顶栏与输入区「停止」→ core 每 delta 检查取消、工具与审批等待可中断，emit `Cancelled`，UI 显示「已停止」。

## 8. 性能口径说明与已知限制

- **页面切换**：p50≈1ms、p95≈4.5ms（本机 250% DPI，Intel HD530 + DX12）。
- **空闲帧率**：约 3-4 fps 是设计值（250ms Tick 才重绘一次，省电/省 CPU）；动画/流式输出时按 60ms 增量批处理重绘，不以此判断卡顿。
- **冷启动**（进程→窗口停靠）：干净环境实测 16.3 / 17.8 / 24.2 / 29.5 秒，波动大，主要耗时在 wgpu(DX12) 首帧管线编译与窗口创建；**不是**页面切换/托盘响应慢。后续优化方向：wgpu 管线缓存（pipeline cache）落盘、或启动时先显示占位骨架。
- 隐藏窗口 = 移到屏幕外，截图脚本判断"隐藏"要看窗口 rect.x < -5000。
- 审查弹窗的 UI 逻辑（等级色/原因/三个按钮/拒绝并记忆）已实现并有 core 侧测试（4 档分支 + allow/deny 记忆）；本次未在真机截到弹窗画面（会话里没有触发到需要写操作的工具调用）。
- web-remote 的 SPA 截图（6 张）在 `logs/screenshot-web*.png`。
## 9. 真机 E2E（computer-use 方式，由 automation 执行）

| 用例 | 结果 | 证据 |
| --- | --- | --- |
| 02 启动 + 窗口/渲染基线 | **PASS** | `logs/e2e-screenshots/02-launch-baseline.png`（等 UIA 出现「连接其他设备」且标题不带「未响应」后才截图；冷启动约 20s） |
| 04 顶栏「连接其他设备」→ 设备页 | **PASS** | UIA 读回 `Button name="连接其他设备" patterns=[invoke]`，点击后日志 `page switch page=Remote`，设备页读到「扫码配对/配对码/访问地址」；截图 `04-remote-panel.png`（二维码 + 配对码 + 剩余 08:56） |
| 03 设置页 MCP/技能（UIA 名字定位） | **FAIL（能力缺口）** | 设置页 205 个 UIA 节点里文本 `Name` 全为空（fenestra 未把静态文本暴露给无障碍），`name_filter` 匹配不到「MCP 服务器」/「技能」；按钮类有名字可点。现场：`logs/e2e-failures/03-mcp.tree.txt`、`03a-settings-page.png` |

结论：**按钮能被 computer-use 点中**（invoke pattern 生效、页面切换正确），但**静态文字读不到**——模型读界面要靠截图视觉。已列入已知限制，修复需要给 fenestra 的 Text 节点补 accessibility name（下一阶段）。

另外：app stderr 会周期性打印 `ICU4X data error: No segmentation model for complex script: Chinese/Japanese`（来自文本分段缺少 CJK 数据模型，中文渲染正常，只是噪音），不影响功能。
---

# 第五阶段（v0.5）验收记录

日期：2026-11-01。需求：继续优化切页/按钮卡顿、上下文压缩（默认 1M）、模型自切模式、DeepSeek 搜索、托盘/任务栏图标与快捷键文案、窗口显隐流畅、默认输出 token、logo、稳定性崩溃、OAuth 授权码、设备配对、人设可视化编辑 + 新建、计算机使用拖动（按住→松开）。

## 1. 编译与测试

| 命令 | 结果 |
| --- | --- |
| `cargo build --offline -p neosonia-app` | 通过（含 vendored fenestra-shell 补丁） |
| `cargo test --offline --workspace` | **426 passed / 0 failed / 22 ignored**（app 38、core 104、mail 53、tools 124、web 73、mcp 34） |
| `node crates/neosonia-web/scripts/test-spa-core.cjs` | 13 passed |

## 2. 前端性能（真机 Intel HD530 / DX12 / 250% DPI / 窗口 1060x1901）

帧耗时（shell 新增 `NEOSONIA_FRAME_LOG=1`，`[frame] total/view/layout/paint/present`）：

| 阶段 | p50 | p95 | 说明 |
| --- | --- | --- | --- |
| view 构建 | 0.7ms | 1.1ms | 我们的视图构建 |
| layout | 5.3ms | 148.8ms | 首次构建含文本排版；预热后稳态 5-10ms |
| paint（vello 场景编码） | 37.7ms | 54.5ms | 集显上 vello 的固定成本，仍是主要开销 |
| present（GPU 渲染+呈现） | 9.1ms | 89.6ms | |

页面切换（`page switch` 日志，点击导航 → 新页构建完成）：**0.7-4ms**；automation 真机独立测量 p95=3.1ms、`view built` 33 样本 max 3.9ms。修复前首次切设置页会出现 1.6s 布局尖峰。

已做的优化：
1. **字形预热**：启动 4s 后后台线程把界面用到的 979 个中文字形做一次 shaping（只 layout，不上 GPU，实测 38-108ms），首帧布局尖峰从 1606ms 降到 ~10ms 级；`scripts/gen-warmup-glyphs.ps1` 可从源码重新生成预热字符表。
2. **切页两段式**：点导航先渲染当前页 + 「正在加载…」遮罩（首帧极便宜），下一帧再构建目标页；首次打开重页面不再看起来卡死（截图 `logs/ui/v5/02-page-loading.png`）。
3. 聊天虚拟化、设置折叠、主题缓存、截图后台线程（v0.4 已做）。
4. `NEOSONIA_AA=msaa8|msaa16` 可切抗锯齿模式，供进一步调优。

## 3. 窗口显隐与任务栏/托盘

- 隐藏改为 **真 `set_visible(false)`**；shell 内新增"隐藏时跳过 build/paint/present"（隐藏窗口上 present 可能阻塞主线程）。
- 显示延迟实测 **~130-141ms**、隐藏 ~102ms。
- **Alt+Space 第二次唤不出（用户报的 bug，本轮真机复现并修复）**：隐藏窗口会吞掉 Alt 的抬起事件，导致 Win32 `RegisterHotKey` 的按键状态卡住。修复：热键触发时若修饰键仍按着，先挂起，等抬起再 toggle（`hotkey::modifier_down` + `pending_toggle`）。验证：连按 3 次 → visible → hidden → visible → hidden，三次都有 dispatch。
- **只在窗口弹出时显示任务栏图标**：隐藏时 `WS_EX_TOOLWINDOW`，显示时 `WS_EX_APPWINDOW`。
- 任务栏/Alt-Tab 图标每次显示时用项目 logo 重设（修"有时变成默认 exe 图标"）。
- 托盘提示文案跟随实际快捷键设置（改了快捷键同步更新）。
- 顶栏左上角换成项目 logo（截图 `logs/ui/v5/01-chat-logo.png`）。

## 4. 稳定性（用户报"执行操作时有时崩溃"）

automation 真机复现了崩溃并抓到栈：`accesskit_consumer-0.38.0/src/tree.rs:75: Focused ID #... is not in the node list`（主线程，保存人设后重建 UI 树时焦点节点消失）。修复两层：
1. `access::tree_update` 校验 focus id 是否在本次节点列表里，不在就退回 root（根因）；
2. `push_access_tree` 包 `catch_unwind`，万一再 panic 就只记录一行并禁用无障碍，不带崩主线程。
另外：主线程消息处理与视图构建已包 `catch_unwind` + 降级界面；panic 写 `logs/crash-<时间>.txt`（含 backtrace）；工具执行 panic 由 core 隔离成中文工具错误。
烟测：新建人设/保存/重新扫描/切页 全程进程存活（之后再未出现 crash 文件）。

## 5. 上下文压缩 / 模型 / 搜索

- **上下文压缩**（core）：默认 1M 上下文；历史超 `context_threshold_tokens=700_000` 时把最近 40 条以外折叠成 ≤1500 字中文摘要（system 消息放最前），保留最近 40 条原文；失败降级"更早内容已省略"不中断；原始消息不删、摘要存 SQLite `conv_summaries` 并缓存；UI 提示 `上下文已压缩：约 X → Y tokens`。设置页有开关 + 阈值展示（截图 `logs/ui/v5/12-mail-oauth.png` 上半部分）。
- **默认输出 token 8192 → 32768**，已保存配置自动迁移（`Settings::load_or_create`）；设置页实测显示 32768、不再溢出，并显示"DeepSeek 单次输出上限 384K"（截图同前）。
- **DeepSeek 官方搜索**：`web.search_provider = "deepseek"` 走 `POST {base}/v1/tools/search`（summary 优先、rerank 排序、来源 DeepSeek），无 key/失败自动回退聚合搜索并注明原因；`web_fetch` 直连失败或正文过短时用 `POST /v1/tools/fetch` 兜底。无 key 回退路径已真机跑通（6.8s、5 条真实结果）。
- 工作模式：模型可用 `switch_mode` 工具在任务中主动切换（v0.4 已实现，v0.5 保持）。

## 6. 人设可视化编辑 + 新建（真机 PASS）

automation 用计算机使用方式验证：点「新建人设」→ 填 4 个输入框 → 点「保存人设」→ `sandbox/personas/e2e-auto.md` 落盘 237 字节，含名称与正文 → 删除并重新扫描，全程无崩溃。截图 `logs/ui/e2e-v05/05-persona.png`。

## 7. 计算机使用拖动（真机 PASS）

新工具 `mouse_press` → 10 次 `mouse_move`（每步 30ms）→ `mouse_release`：窗口实际位移 **(120,60)**（rect 1100,52 → 1220,112），进程存活；截图 `logs/ui/e2e-v05/03-drag-after.png`。证实"按住不放 → 移动 → 松开"链路可用，可用于真实拖拽/长按/组合键（另新增 `keyboard_press`/`keyboard_release`、`mouse_drag_path.hold_ms`）。

## 8. 配对与远程连接（web 侧 73 测试 + 6 张截图）

- 扫码 `/pair#code` **自动填码 + 自动配对**并直接进聊天；已有 token 先验证直达；401 自动回配对页提示。
- WS 心跳 25s，连续 3 次无响应才判定断开；静默重试 2 次后显示横幅 + 立即重试；1→2→5→10→15s 退避；重连恢复会话/未读/审查；设备页显示连接状态与「断开本设备」。
- 静态资源 `no-cache`/`immutable` + 内容哈希版本号。
- 截图：`logs/screenshot-web-pair.png`、`screenshot-web.png`（自动配对后聊天）、`screenshot-web-reconnect.png`（断线横幅）、`screenshot-web-device.png`（连接状态）等。

## 9. 真机 E2E 汇总（automation）

| 用例 | 结果 |
| --- | --- |
| 02 顶栏「连接其他设备」→ 设备页 | **PASS**（UIA invoke + `page switch page=Remote`） |
| 03 按住拖拽（mouse_press→move→release） | **PASS**（窗口位移 120,60，进程存活） |
| 05 人设新建 | **PASS**（文件落盘 + 内容断言） |
| 04 切页流畅度 | 实际数据 PASS（4 页 max 3.1ms）；测试脚本因"点对话时已在对话页"少一条日志，判为口径问题 |
| 01 窗口显隐 | 首轮 FAIL（第二次 Alt+Space 不恢复）→ **已修复并复测通过**（visible→hidden→visible→hidden） |
| 06 附件/加载态 | 首轮因 accesskit 崩溃中断 → 崩溃已修复；附件条在 v0.4 已截图验证 |

## 10. 仍存在的限制

- **paint ~38ms/帧**（vello 场景编码，集显固定成本）→ 交互时约 20fps，还达不到"系统组件级"60fps；下一步可考虑降低场景节点数（合并圆角/边框）、vello 场景缓存或换更低分辨率渲染再做上采样。
- **冷启动 16-30s**（DX12 首帧管线编译），未在本轮解决。
- UIA 静态文本 `Name` 仍为空（v0.4 已知限制），computer-use 读文字靠截图。
- 邮箱 OAuth 授权码显示已实现（地址 + 8 位代码 + 自动复制），本轮未抓到可视化截图；Microsoft 完整授权需要用户真实登录。

---

# 第六阶段（v0.6）验收记录（进行中）

日期：2026-11-02。用户列出约 28 项：前端图标/顶栏高度/平滑滚动、管理员运行时 cmd 黑窗、托盘描述、截图先抓后开窗、手机连不上、全部设置图形化、输入框焦点、截图提示词、键鼠审查 2 分、后台任务+自动隐藏窗口、交付物（图片/视频/文件/链接）、强制切模式、本会话不折叠、切页过渡动画、历史按钮回对话页、Work Plan、子代理数量图标、连续点击、多 API 配置、多邮箱/自定义账号、长需求输入框、数学公式、会话隔离、新建会话默认日常、托盘图标、同类 2 分自动放行、长历史分段渲染预加载、Alt+Space 不应截图、OAuth 显示名、OAuth 后可用。

## 已完成并验证（本轮）

| 项 | 状态 | 证据 |
| --- | --- | --- |
| 手机连不上（核心根因） | **已修** | bridge 强制 `bind=0.0.0.0` + advertise 用本机 LAN IP + 首次自动加 Windows 防火墙规则；app 日志 `addr=0.0.0.0:8787 advertise=http://192.168.1.100:8787`；真机 `GET http://192.168.1.100:8787/healthz` → **200**，返回 `bind=0.0.0.0:8787, advertise_url=http://192.168.1.100:8787, lan_ip=192.168.1.100`；web-remote 另用 web_probe 在 8798 端口用 LAN IP 验证 healthz/pair/me/sessions 全 200 |
| 配置迁移 | 已做 | `remote.bind: 127.0.0.1 → 0.0.0.0`、`max_tokens 8192 → 32768`、`screenshot_chord 默认 ""`（Alt+Space 不再截图）、`mail.provider` 默认 custom |
| 托盘图标坏了 | 已修 | 原 `assets/icon-32.png` 只有 117 字节（几乎空白→黑块）；用 PIL 从 icon-256 重生成 32/64，并制作 `tray-32.png/tray-64.png`（深色圆角底+logo），tray.rs 改用 tray-32 |
| 截图顺序（先截屏再开窗） | 已改 | `screenshot_to_model` 不再先 show_window；抓屏在后台线程，`ScreenshotDone` 才 show_window + 附到输入框 |
| 截图延迟 | 已改 | bridge 抓屏改走 tools 新增的 Windows GDI 快路径（BitBlt+快速 PNG），xcap 仅兜底 |
| 托盘描述与实际快捷键一致 | 已改 | tooltip 用 `{hotkey}` + `{screenshot_hotkey}` 实时生成（默认显示 Ctrl+Alt+A），设置变更即时更新 |
| 长需求溢出输入框 | 已改 | 输入框改 `text_area` 多行（min_height 88，占位文案提示 Shift+Enter 换行） |
| 新建会话残留上段记录 | 已修 | `ClearChat` 清空 items/attachments/工具卡、回对话页、**默认切回日常模式**、重置 items_version |
| 历史会话按钮 | 已修 | `HistorySelect` 先切回对话页 |
| 切页过渡动画 | 已加 | 内容区 160ms 淡入+上移 keyframes；切页同时有「正在加载…」两段式 |
| 子代理数量图标 | 已加 | 顶栏显示 user 图标 + 数量（运行中优先显示 active），来自 `AgentEvent::SubagentCount` |
| 上下文压缩 | 已做（v0.5 起） | 默认 1M 上下文、70 万 tokens 触发摘要、保留最近 40 条 |
| 多 API 配置 | 已完成（core+UI 待接） | core：`ApiProfile`/`profiles`/`active_profile`/`effective_server()`/增删改切换 API；设置页 UI 由 ui-panels 按 schema 接入 |
| 键鼠/计算机操作审查 2 分 | 已完成 | core：键鼠/截图/UIA/CDP 固定 level 2、跳过 LLM 评分、class=computer_use、一次确认 24h 放行；`python/py/python3` 统一 `shell:python` 修"同意后反复授权" |
| 提示词：强制切模式 + daily 展开计算机操作 | 已完成 | 三模式共有「模式切换（硬性要求）」；daily 默认展开基础版（内置截图/键鼠/UIA 工具，明确禁止 python/PIL 截图）；本会话用过一次计算机工具后不再折叠（按会话记忆）；coding 提示写完可切 computer_use 测试 |
| Work Plan | 已完成（core 工具+事件+持久化）；UI 面板 ui-panels 接入中 | `update_plan` 工具、`PlanUpdated` 事件、`conv_plans` 表、切会话恢复 |
| 交付物（图片/视频/文件/链接） | 已做（渲染已实现，未真机验证） | core `deliver` 工具 + `Delivered` 事件（校验存在/大小/敏感文件拒绝）；app chat 新增交付卡：图片直接预览、视频/文件显示卡片、每项「打开」「打开路径」、链接点击默认浏览器打开 |
| 后台任务 + 自动隐藏窗口 | 已完成 | core `set_window_visibility` 工具 + hook + 回合结束/审批前自动恢复；app 收到 `WindowVisibilityRequested` 便隐藏/恢复主窗口（紫色 HUD 是独立窗口不受影响）；tools 另有 `window_hide_app` |
| 连续点击 | 已完成（tools） | `mouse_click_sequence`（最多 30 点、步间 120ms、支持 right/double） |
| cmd 黑窗（管理员运行） | 已完成（tools） | 统一 `CREATE_NO_WINDOW`；tools 128 测试全绿 |
| 多邮箱/自定义账号 | 已完成（mail） | 预设 outlook/gmail/163/126/qq/custom、163/QQ 授权码提示、IMAP ID 命令自报 NeoSonia、`apply_preset` |
| OAuth 后不可用 | 已修（mail+app） | 真实配置证据：token 有值但 imap.host 空 + mail.enabled=false；`apply_oauth_result` 按 provider 回填 host/port/tls + enabled=true；`migrate_settings` 启动时自动修旧配置（bridge 已接并保存）；XOAUTH2 报文、token refresh 重试有单测 |
| OAuth 显示名 Thunderbird | 说明+可配置 | 显示名由 client_id 决定；已提供 `oauth.client_id` 覆盖与 Azure 注册步骤（IMAP ID 自报 NeoSonia） |
| 数学公式 | 已完成（mcp） | aligned/cases/matrix/pmatrix/bmatrix 真正多行网格；`\sqrt[3]` 修复；字体缺字形默认 ASCII 化（不出方框、不触发递归爆栈）；`NEOSONIA_MATH_UNICODE=1` 可切完整 Unicode；math 测试 11 全绿 |
| AI 总想用 python 截图 | 已修（提示词） | daily 基础版明确"不要用 python/PIL 截图，直接用内置 screenshot/ui_screenshot" |

## 编译与测试

- `cargo check --offline --workspace` → **EXIT=0**
- `cargo test --offline --workspace` → **473 passed / 0 failed / 26 ignored**（app 44、core 118、mail 70、tools 128、web 79、mcp 34）
- `cargo build --offline -p neosonia-app` → 成功（tools 注册数 75：本地 67 + core 5 + mcp 3）

## 本轮尚未验证/未完成

1. **热键回归**：最后两轮真机脚本里 Alt+Space / Ctrl+Alt+A 没有产生 `hotkey dispatch` 日志（应用已注册 `全局快捷键已注册`、线程就绪，但注入事件未触发）。最可能是注入脚本与前台窗口/焦点状态有关（v0.5 同样注入可触发），需要单独复测并确认；`hotkey.rs` 里 RegisterHotKey + 独立线程实现未回退。
2. **截图热键端到端**：改为"先抓屏后开窗"后，本轮未抓到成功截图文件（bridge 已切 GDI 快路径，但测试时窗口未成功隐藏、注入未触发），需要热键恢复后复测。
3. 平滑滚动、长历史分段渲染+方向预加载、设置页"全部字段"补齐、多 API/邮件/计划三个新 UI 面板（ui-panels 正在做）、OAuth 可视化截图、deliver 真机截图：未完成/未验证。
4. web verify.ps1 的 7 张 LAN 截图未跑（web 侧 LAN 自测已用 web_probe 全绿）。

## 附：web-remote v0.6 LAN 修复真机证据（app 侧，非探针）

真实 app（bridge 接好 `bind=0.0.0.0` + `advertise_url=LAN IP` 后）：
```
Get-NetTCPConnection -LocalPort 8787
  LocalAddress=0.0.0.0  LocalPort=8787  OwningProcess=<neosonia>
curl http://192.168.1.100:8787/healthz
  -> 200 {"ok":true,"bind":"0.0.0.0:8787","advertise_url":"http://192.168.1.100:8787",
          "lan_ip":"192.168.1.100","lan_only":true,...}
curl http://192.168.1.100:8787/pair -> 200 text/html（配对页 LAN 可达）
```
web_probe LAN 全链路（全部经 `http://192.168.1.100:8799`）：
- `/healthz`、`/pair`、`/api/me`（server.bind=0.0.0.0:8799、advertise_url=LAN、client_ip=LAN）、sessions、分页 236-240→231-235 连续、review pending 2、截图 200、穿越 400、缓存头正确；
- WS 经 LAN：hello 带 `resume{conv_id:"probe-long",since_seq:240}` + risk_review + pong/cancel，`frames=8 hello=true`；
- `cargo test -p neosonia-web` → **79 passed**，`verify.ps1` exit 0。

7 张 LAN 截图：`logs/screenshot-web-pair.png`、`screenshot-web-pair-help.png`（连不上？排障折叠：当前访问地址/防火墙 netsh 命令/同 Wi-Fi/关 VPN/AP 隔离）、`screenshot-web.png`（扫码自动配对后聊天）、`screenshot-web-earlier.png`、`screenshot-web-review.png`、`screenshot-web-reconnect.png`、`screenshot-web-device.png`（客户端 IP / 最后心跳 / 服务监听 / 访问地址 / 首次绘制 338ms）。

## v0.6.1 修复：前端动画重复播放 / 一直闪动

**根因（两处）**：
1. 切页过渡用了 `Keyframes` —— fenestra 的 `Keyframes` 是**循环动画**（专为脉冲/呼吸设计，没有"播放一次"选项）。挂在页面容器上，每一帧都从 0 重新采样并永久循环，等于整页不停淡入淡出。**改为完全静态**，切页反馈保留「正在加载…」两段式 + 导航高亮。
2. 编辑框 `.autofocus()`：fenestra 在输入框聚焦时会用**光标闪烁**让 `frame.animating` 永久为 true，shell 于是按最大帧率不停重绘整个窗口（实测 idle 时每 80ms 一帧、paint ~50ms，白耗 CPU 且看起来"一直在动"）。**去掉每帧 autofocus**（需要输入时点一下输入框即可，行为与普通应用一致），光标不再常驻闪烁。

**验证**（`NEOSONIA_FRAME_LOG=1` + 帧差分）：
- 修复前：idle 时 3 秒内持续出帧（间隔 ~80-180ms），截图差分显示左侧竖线（光标）与页面区域持续变化；
- 修复后：idle 只剩 Tick 驱动的 ~4 帧/秒（像素完全一致，无可见闪动），不再有循环动画；`paint` 从 ~50-130ms/帧降到 ~30ms/帧。
- `cargo build -p neosonia-app` 通过。

**注意**：如果旧实例还在跑（管理员启动的那个），需要先从托盘退出再启动新版本才生效。

## v0.6.2 补充：tools 真机验收（管理员会话）

**reg.exe 根因确认**：tools crate 无 reg/regedit 调用；真凶是 app 的 `autostart.rs` 3 处 `Command::new("reg")`，已改为统一 helper（CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP）。

**tools 加固**：`lib.rs` 统一入口 `apply_no_window_std/tokio`；生产代码 10 个 `Command::new` 全覆盖（shell/run_command/background_task/taskkill/kill/notify/xdotool/uia 降级）；防回退单测 `no_console_flash_audit`（每个 Command::new 4 行内必须有 apply_no_window 且禁 reg）。
真机（管理员 PowerShell 会话）：执行 `cmd /c echo`、`powershell Write-Output`、`reg query HKCU\...`、`notify` 后**控制台窗口 0 新增**，输出采集正常。

**截图提速（release）**：GDI 新快路径（StretchBlt 直接缩放 + PNG Fast/NoFilter）：空桌面 **175/176/184ms**、负载下 210–240ms（xcap 323–389ms），较改造前 566–701ms **约 3x**，达成 <300ms；顺带修了 BI_RGB alpha=0 导致 PNG 全透明的问题（强制 alpha=255）。

**window_hide_app / window_is_hidden（沙箱真机 PASS）**：隐藏→截图无窗口、恢复→窗口出现；HUD 类名 `NeoSoniaHud*` 显式排除（单测覆盖），只隐藏主窗口。

**mouse_click_sequence（真机 PASS）**：记事本 3 点连点 3/3、320ms，输入字符数 66→69 证明落点正确。
**background_task**：label 显示、running/cancelled 状态正确。

**闸门**：`cargo check -p neosonia-tools` 0 warning；`cargo test -p neosonia-tools` **129 passed**；`tool_catalog()` = **67**。

## v0.7（API 状态 / 输入体验 / 稳定性）

### 1. API 可用性小球（本地 + 远程）
- core 新增 `provider_status`：按 `effective_server().base_url` 识别 DeepSeek / OpenAI / Anthropic / Groq / Mistral / Cohere；标准厂商走 Statuspage v2 `/api/v2/status.json`+`summary.json`。
- **DeepSeek 专用适配**：`status.deepseek.com` 不是 Statuspage（v2 接口 404），改为优先解析 **Atom/RSS 最新条目**（`/feed.atom`→`/atom.xml`→`/rss.xml`），失败再抓 HTML **当前状态区**可见文本；中英文关键词映射 none/minor/major/critical→绿/黄/橙/红；最新条目 resolved/已恢复→green（历史故障不误报）；unknown 不误报。
- 本地：输入框右下角状态点（绿/黄/橙/红，unknown 隐藏，点击打开状态页），启动刷新 + 60s 周期。
- 远程：`/api/me` 带 `server.provider_status`（3s 超时、失败 null），后台 50s 预热；SPA 顶栏小球 + 设备页「API 状态」行与图例（绿色也显示最近事件 detail）。

### 2. 思考强度 4 档
`reasoning_effort`：`不思考(空串) / low / high / max`；滑块 4 档，默认 high，旧 `medium` 兼容显示在 high 档。

### 3. 邮箱换账号与退出登录
- `apply_oauth_result`：email 变化→先清旧凭证再写新账号，并**强制覆盖** `imap.user / smtp.user / smtp.from`；登录失败**不动**旧凭证（只置 needs_reauth）。
- `migrate_settings`：`oauth.email` 与 imap/smtp 用户不一致时按 oauth.email 覆盖（启动自动修复旧配置）。
- 新增 `clear_oauth_authorization()` + 设置页「退出登录（清除授权）」：清 OAuth 凭证、enabled=false，保留服务器/账号/client_id。

### 4. 输入框粘贴
输入框工具行「粘贴」按钮：剪贴板图片→存 `uploads/paste-<ts>.png` 作为附件；文本→填入输入框。

### 5. IME 光标（vendor fenestra-core）
真机症状：多行输入框中间位置打拼音时，每打 N 个字母光标后跳 N 格、确定时中文插在最终位置、组合期间 ←/→ 无效。
根因：parley `raw_text()` 含 IME 预编辑文本，(1) `EditorState::sync()` 每帧拿它和已提交值比较→清组合/光标跳末尾；(2) winit 把拼音按键作为 Text 事件送入后旧逻辑当已提交文本插入；(3) 组合期间方向键走了已提交文本分支。
修复：`sync/snapshot/on_input` 统一用 `text()`；新增 `InputEvent::ImeCommit` + `handle_commit`（先 `clear_compose` 再插入提交文本）；组合期间忽略 Text 事件、←/→ 移动预编辑光标、Backspace/Delete/Enter/Escape 只消费。`[patch.crates-io] fenestra-core = { path = "vendor/fenestra-core" }`。
单测：vendored core `input::` 7 passed + app 层 `ime_composition_never_pollutes_composer_value`。

### 6. 代码块横向滚动
`chat.rs` 围栏解析（```/~~~/{lang}/未闭合兜底/行内反引号不误判）+ 自定义卡片：右上角语言标签、等宽、显式内容宽度 + `scroll_x()` 横向滚动、不换行、剥离零宽空格、稳定滚动 id、滚动条预留高度。证据：`logs/ui/code-block-scroll-0.png`（未滚动）/`-1.png`（横向滚动后，内容左移 + thumb + rust 标签），像素差 3877px。

### 7. reg.exe 真正根因（v0.6.2 未查到的部分）
Process Monitor 抓到 `"reg" query "HKCU\Control Panel\Desktop\WindowMetrics" /v MinAnimate`：真凶是 **vendored `vendor/fenestra-shell/src/reduce_motion.rs`** 的"减少动效"检测，在窗口创建 + **每次重新聚焦**时 spawn `reg query`（即使 CREATE_NO_WINDOW 也会产生 reg.exe 进程，管理员模式更明显）。
修复：改为进程内 `SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION)`，零子进程；全仓 `Command::new("reg")` = 0。

### 8. 窗口状态自检
每次显示窗口：`restore_from_minimized()` + `dock_once_force()` 重新停靠；Tick 检测到被最小化立即恢复——修复"最小化后 Alt+Space 变半个窗口/几何错乱"与"点任务栏图标缩进任务栏"。

### 9. 其它修复
- 用户气泡按内容估算宽度 + 文本宽度约束：修复长消息出框、压到模型消息/思考面板上。
- 长消息/LaTeX 超长无空格串插零宽空格换行（math.rs 入口统一剔除零宽字符，避免 `fr?ac`）。
- DeepSeek 状态误报（历史 feed 关键词被当当前状态）修复。

### 测试汇总
core 130、app 59、mail 107（1 ignored）、web 91（Node 32）、tools 129；exe `target\debug\neosonia.exe` 2026-11-04 18:24。

## v0.8（本地渲染收口）与 v0.7.1（远程前端）

### 本地渲染（chat.rs）
- **代码块**：卡片宽=父容器可用宽（`w_full+min_w(0)`），长行只影响内部 `scroll_x` 内容；headless `build_frame` 断言卡片宽 ≤ 424−24、`x0≥0`。
- **有序列表 / 长链接**：fenestra_markdown 列表项把段落放进无宽度约束 row（根因）→ chat.rs 自行排版列表项（marker + `col().grow().min_w(0)`）；去掉整段零宽空格预插（会破坏 `](url)` 链接目标），改为 markdown 感知断行；裸长 URL 折行；markdown 统一 `.on_link(Msg::OpenLink)`。
- **交付物**：新增 `PathRoots{home=paths.root, workspace, cwd}` 解析相对路径；图片 `image_rgba8` 缩略图，失败退化文件卡；`.lnk/.url` 快捷方式卡（整行 `Msg::OpenPath`）。
- 数学块横向滚动保持；`cargo test -p neosonia-app` **65 passed**。

### 输入（本机）
- 移除「粘贴」按钮；**Ctrl+V** 粘图片→附件（文本走原生粘贴）；**拖文件到窗口任意位置**→附件。

### 远程前端（web）
- 输入框上方横栏：`上传附件` 按钮 + 聚合进度条 + API 四色小球；顶栏只留连接状态点（在线绿/重连黄/离线红）。
- 整区拖拽上传（dragover 高亮，只处理 Files）。
- Markdown：`[text](url)`、裸 URL（含中文标点边界）、自动链接、原始 `<a>` 渲染为可点击；`file:///` 代码样式不可点；有序列表缩进/编号 + 长 URL 折行不溢出。
- `cargo test -p neosonia-web` **92 passed**；Node spa-core 16 / spa-markdown 22；`verify.ps1` 14 张 LAN 截图 exit 0。
- 截图：`logs/screenshot-web-toolbar.png`（横栏 + 上传按钮 + API 小球）、`logs/screenshot-web-links.png`（链接/有序列表渲染）。

### v0.8b（本地 markdown 收口）
- **跨行内图片的加粗**：行内图片把正文切段后，跨图片的 `**` 会两段各自不配对（星号泄漏、加粗失效）。新增 `balance_bold_across_media`：顺序维护加粗状态，图片前文本 `**` 为奇数时末尾补 `**`、图片后开头补 `**` 重开，图片卡仍独立。守护测试 `bold_survives_inline_image_split`（含 headless 像素墨水量断言加粗确实更重）。
- **blockquote 溢出**：fenestra_markdown 的引用行是无宽度约束 row。改为 chat.rs 自排版 `>`/`>>` 行：左侧 3px 竖线 + `col().grow().min_w(0)` 内容列、行内 markdown 照常（链接可点击/长 URL 折行）、嵌套按 `(depth-1)*10px` 缩进、引用块内每行独立段落。守护测试 `blockquotes_wrap_within_panel`（长中文/长 URL/嵌套引用均 `max_right ≤ 424`）+ `parse_quote_line`。
- 测试：`cargo test -p neosonia-app` **68 passed**；预览 `logs/ui/chat-preview-v2.png`。

## v0.9（拖放 / 邮件滚动 / 远程同步）

### 本机文件拖放（vendor/fenestra-shell）
- core 路由验证：harness `dispatch(InputEvent::FileDrop)` → `Msg::AttachmentsPicked` 通过。
- OS 层没送 `DroppedFile`（winit OLE 在透明/置顶无边框窗口上实测收不到）→ 新增 `vendor/fenestra-shell/src/dragdrop.rs`：`DragAcceptFiles(hwnd,TRUE)` + `SetWindowLongPtrW` 子类化，`WM_DROPFILES` 里 `DragQueryFileW` 取全部路径 → 全局队列 → `EventLoopProxy` 唤醒 → runner 走与 `WindowEvent::DroppedFile` 相同的 `input_main(InputEvent::FileDrop)`；`NEOSONIA_DROP_LOG=1` 输出 warn 日志。
- 真机（跨进程 `PostMessage(WM_DROPFILES)` 等价投递）验证：聊天页出现附件卡 `chat-preview.png · 图片 · 65.0 KB`，日志 4 行。
- 移除「粘贴」按钮，Ctrl+V 粘图片（文本走原生）；拖到窗口任意位置均可。

### 邮件滚动（panels/mail.rs）
- 根因：滚动区子卡片是 flex 子项默认 `flex-shrink:1`，被压到视口高度（648==688）→ `can_scroll_y=false`，滚轮路由找不到目标。
- 修复：滚动区卡片统一 `.shrink0()`（`inbox_card` + 公共 `card()`），工具栏 `.shrink0()` 且在滚动区外；三套独立滚动区 + 稳定 id。回归测试 `mail_scroll_layout_is_bounded_and_wheel_routes`（26 封卡片高>视口、可滚、滚轮命中、滚到底 728/760）。

### 标题栏
- 执行命令时 phase 过长折成两行撑破 header 压到导航：改为按宽度保守截断 14 字符 + 标题列 `min_w(0)/min_h(0)/overflow_hidden`。

### 远程前端（web）
- 拖放上传：拖放区扩到 `#app-view` + document 兜底；`dragover` 一律 `preventDefault`；文件判定 `files/items/types` 三选一；全屏浮层「松开上传 N 个文件」+ 虚线高亮。
- 远程自动同步：`ReviewHub::resolve/clear` 广播 `ReviewResolution`，WS 推 `review_resolved`/`review_cleared`；SPA 收帧后清横幅 + 强刷会话/状态；`turn_finished` 强制重拉；`assistant_message` 刷新列表；`visibilitychange` 恢复同步/重连。
- 测试：`cargo test -p neosonia-web` **96 passed**；`verify.ps1` 14 张截图 exit 0。

### 测试汇总（最终）
app **70 passed**、shell **18 passed**（含 dragdrop）、core 130、mail 107、web 96、tools 129、fenestra-core input 7。

## v0.9.1 收尾：拖放修复确认 + Release 准备

- **桌面拖放（管理员 & 普通权限均可用）**：根本原因是 Windows UIPI（非提权资源管理器 → 提权窗口的 OLE 拖放被拒绝，禁止光标且无事件）。修复组合：`RevokeDragDrop`（撤销 winit OLE 目标）+ `DragAcceptFiles` + `ChangeWindowMessageFilterEx(WM_DROPFILES/WM_COPYDATA/0x49, MSGFLT_ALLOW)` + `WM_DROPFILES` 子类化 → 全局队列 → `EventLoopProxy` 唤醒事件循环 → 与 `DroppedFile` 相同路径投递。用户实测：**管理员/普通权限拖放均正常**。
- **Ctrl+V 粘贴**：图片 → 附件；非图片时读剪贴板 `CF_HDROP` 文件列表 → 附件（管理员模式替代路径）。
- **邮件列表不刷新**：poller 收到 `NewMail` 只弹 toast、不刷新列表；`msg::MailRefresh` 已由 bridge 在 NewMail 时主动发出，app 侧 `refresh_mail` 加 loading 去重，窗口显示/切到邮件页时也会刷新一次。
- **Release 准备**：版本 0.9.0；`[profile.release]`（opt-level 3 / thin LTO / strip symbols）；`scripts/build-release.ps1`（测试 + release 构建 + `dist/NeoSonia-<ver>-win64.zip` + SHA256，不打包 config/logs/data）；`LICENSE`（MIT）；`docs/release-notes-v0.9.0.md`；README 增加构建/发布/测试章节；根目录 82 个历史日志归档到 `logs/archive/`；旧 exe 副本清理。

## v0.9.2 安装包（自解压 + 卸载）

- `scripts/build-installer.ps1`：payload.zip（exe+assets+Uninstall.exe+docs+空数据目录骨架）→ csc 编译自解压安装器；不依赖 Inno Setup/7-Zip。
- 安装目录默认 `%LOCALAPPDATA%\Programs\NeoSonia`；**数据目录（config/data/logs/workspace/personas/skills）跟随安装位置**（`Paths::resolve` 新增 exe 目录回退；安装目录不可写时回退系统数据目录）。
- Windows「应用和功能」注册项 `HKCU\...\Uninstall\NeoSonia`（DisplayName/Version/Publisher/Icon/UninstallString/QuietUninstallString/EstimatedSize）；实测 `winget list --name NeoSonia` 可见 `NeoSonia v0.9.0`。
- 开始菜单 `NeoSonia` / `卸载 NeoSonia`、桌面快捷方式（带 `--home`）。
- 卸载：`Uninstall.exe`（`/S` 静默）→ 复制自身到 %TEMP% 后删除安装目录（含数据）、注册表、快捷方式；实测目录/注册表/快捷方式全部移除。
- **发布前已清空全部测试用户数据**：config（API key/邮箱凭证）、data（聊天记录/设备/记忆）、sandbox、workspace、personas、skills、logs；备份在 `E:\NeoSonia-backup-20261105-181052\`（不入包）。
- 安装包内 `config/settings.toml` 首启生成，`api_key/refresh_token/email` 均为空。

### v0.9.3 安装位置可选

- 交互式安装启动后弹出「NeoSonia 安装程序」窗口：安装位置文本框（默认 `%LOCALAPPDATA%\Programs\NeoSonia`）+「浏览…」目录选择 + 安装/取消；选择目录后自动追加 `\NeoSonia`（若未选到产品目录）。
- 静默安装保持 `/S`（默认位置）与 `/S /D=<目录>`（指定位置）。
- 实测（UIA 枚举 + PrintWindow 截图）：窗口控件为 标题 / 安装位置 / 路径文本框 / 浏览… / 说明文字 / 安装 / 取消，默认路径正确；截图 `logs/installer-picker2.png`。
- 静默安装→卸载回归通过（目录/注册表/快捷方式全部清理）；随后重新静默安装到默认位置，`winget list --name NeoSonia` = `NeoSonia v0.9.0`、注册表 DisplayName/InstallLocation 正常。

### v0.9.4 安装位置浏览卡死修复

- 现象：点「浏览…」唤起资源管理器目录选择对话框时，安装程序显示“未响应”。
- 根因：WinForms 主入口 **缺少 `[STAThread]`**，`FolderBrowserDialog`（SHBrowseForFolder）在 MTA 线程上会阻塞该线程；另表单 `TopMost=true` 会让对话框被压在顶层窗口后。
- 修复：`Setup.Main` 加 `[STAThread]`；去掉表单 `TopMost`，浏览对话框以非模态宿主打开并在返回后 `Activate/BringToFront`。
- 验证：UIA 找到「浏览…」并以 BM_CLICK 触发后，出现「浏览文件夹」窗口（class #32770），安装程序 `Responding=True`（不再未响应）；开始菜单/静默安装卸载回归保持通过。
- 安装路径也可以直接在文本框输入/粘贴（支持 `%LOCALAPPDATA%` 默认、`/D=` 静默指定）。

### v0.9.5 exe 图标/版本信息（资源管理器与任务栏）

- 现象：安装后主程序在资源管理器/任务栏是默认空白图标，托盘正常。
- 根因：exe 没有 PE 资源里的 ICON / VERSIONINFO；窗口与托盘图标是运行时用 PNG 设置的，而资源管理器、任务栏、文件属性读的是 exe 内嵌资源。
- 修复：`crates/neosonia-app/build.rs` 用 `embed-resource` 在 Windows 目标下编译 `res/app.rc`：
  - `1 ICON "res/neosonia.ico"`（16/24/32/48/64/128/256 多尺寸 PNG 条目，由 assets/icon-256.png 生成）；
  - `VERSIONINFO`：ProductName/FileDescription/FileVersion/CompanyName/LegalCopyright（版本取自 `CARGO_PKG_VERSION`，随发布版本自动更新）。
- 验证：`Icon.ExtractAssociatedIcon` = 32×32 橙色 logo；`VersionInfo` 显示 `NeoSonia / NeoSonia 桌面 AI Agent / 0.9.0`；重装后安装目录 exe 同样带图标与版本信息。
- 附带产物：`dist\NeoSonia-0.9.0-win64.zip`（便携版，含新图标 exe）。

## v0.9.6 人设编辑正文滚动修复

- 现象：人设编辑/新建时正文区滚不动；第一次"内部滚动容器"方案反而导致"字不滚、框滚"。
- 根因：人设页滚动容器里的编辑器卡片是 flex 子项，默认 `flex-shrink:1` 被压到视口高度 → `content_height == viewport` → `scroll_y` 不生效（与邮件列表同一类问题）。正文 `text_area` 本身会随内容长高，被压缩后文字与框的相对关系错乱。
- 修复：撤销正文内部滚动容器；给编辑器卡片加 `.min_h(0).shrink0()`，让页面级 `scroll_y` 真正生效——字和框一起随页面滚动。
- 回归测试 `persona_editor_page_scrolls_long_body`：编辑器卡片高 > 视口、正文区不被压缩、页面可滚、鼠标停在正文可见区域时滚轮命中页面滚动容器、滚动后正文上移。
- `cargo test -p neosonia-app` **74 passed**。

## v0.9.7 人设正文：框固定、字在框内滚动

- 需求澄清：要"外层滚到底也不够看全 → 内层文本框自己滚（框不动、字滚）"。
- 根因（上一版失败原因）：`text_area` 放进固定高度 `scroll_y` 容器时没有 `shrink0`，被压缩到视口高、文字被裁掉，于是只有容器在滚、字不动。
- 修复：正文框 420px 固定高度 + `scroll_y`（id `persona-edit-body-scroll`），内部 `text_area` 加 `.shrink0()` 保持完整内容高度 → 框固定、文字在框内滚动；编辑器卡片仍保留 `shrink0` 让整页可滚到按钮区。
- 回归测试 `persona_body_inner_scroll_keeps_text_moving`：正文高 > 框高（text_area 未被压扁）、框可滚、滚轮命中框、滚动后文字上移而框位置不变。
- 真机截图：`logs/ui/persona-inner2-before.png` / `persona-inner2-after.png`（框位置不变、框内文字上移）。

## v0.9.8 人设正文：固定外框 + 框内滚动（最终版）

- 现象演进：内层滚动容器无 shrink0 → 框滚字不滚；页面级滚动 → 框跟着页面走；最终确定需求是"框固定、字在框内滚动、内容不出框"。
- 关键发现：`text_area` 自身的边框/背景属于"内容"，在滚动容器里会随文字一起滚出视野，看起来就是"框在滚、字出框"。
- 修复：
  - 外层自绘固定框（420px，`border + bg + rounded + overflow_hidden`，id `persona-edit-body-frame`）；
  - 框内 `scroll_y` 容器（id `persona-edit-body-scroll`）滚动 `text_area`；
  - `text_area` 加 `shrink0` 保持完整内容高度，并隐藏自身边框/背景（`border(0) + bg(surface)`），避免"框跟着滚"。
- 真机验证：`logs/ui/persona-frame-b.png`——页面与外框位置不动，框内文字滚到第 8 条/回答格式，文字在框边被裁切、不出框。
- 测试 `persona_body_inner_scroll_keeps_text_moving` 覆盖内层滚动与框位置不变。
