# NeoSonia

常驻 Windows / Linux 桌面的 AI Agent。右侧一条窄浮窗，全局快捷键随叫随到；
能读写文件、执行长时间命令、截屏、模拟鼠标键盘、读取其它程序的控件树并点击、
访问网页、直连 IMAP/SMTP 收发邮件。有人设、有长期记忆、能开子代理。

- **前端**：[fenestra](https://github.com/richer-richard/fenestra) 0.41（纯 Rust 原生 GUI：
  winit + wgpu + vello + parley + taffy，没有 webview、没有 HTML/CSS）
- **后端**：Rust（tokio 异步 agent 循环 + 工具执行器）
- **视觉**：Firefox Nova 设计语言（紫为唯一强调色、带紫味中性色、4px 网格、r12/r16），
  全中文界面，**零 emoji**

```
+--------------------------------------------------+
|  N  NeoSonia            [停止] [新对话] [隐藏]     |  48px 标题栏（可拖动）
+--------------------------------------------------+
|  对话   邮件   记忆   人设   设置                  |  40px 导航（2px 品牌发丝）
+--------------------------------------------------+
|  你                                             |  |
|                    +---------------------+      |  |
|                    | 帮我把下载目录整理一下 |      |  |
|                    +---------------------+      |  |
|  [点] run_command                        完成 1.2s |  |
|      powershell -NoProfile ...                  |  |
|  已按日期分好 3 个目录，移动 42 个文件            |  |
+--------------------------------------------------+
|  > 输入指令，回车发送                     [发送] |  输入区
|  就绪                              812 / 340 tok |
+--------------------------------------------------+
```

## 功能

### 系统整合
- **右侧浮窗**：无边框、圆角、始终置顶、不进任务栏；宽度/屏高占比/边距/透明度可调。
  圆角走 Windows 11 DWM 原生圆角，阴影由系统合成器提供。
- **全局快捷键**：默认 `Alt+Space` 显示/隐藏，设置页可改成任意组合（`Ctrl+Alt+S`、`F8`…）。
- **系统托盘**：显示/隐藏窗口、退出（可在设置里关掉）。
- **单进程常驻**：窗口隐藏后 agent 仍在后台执行（长命令、邮件轮询都不中断）。
- 顶部状态行实时显示当前阶段（思考 / 调用工具 / 等待命令输出）、token 用量。

### 工具（模型可调用，注册 75 个）
| 类别 | 工具 |
| --- | --- |
| 文件 | `read_file` `write_file` `edit_file` `append_file` `list_dir` `search_files` `file_info` `make_dir` `copy_path` `move_path` `delete_path` |
| 本地索引 | `find_files`（类 Everything 的文件名毫秒级搜索）`reindex_files` `index_stats` |
| 命令 | `run_command`（默认 1800s 超时、可设 0 无限、流式输出、可取消）`list_jobs` `read_job_output` `send_job_input` `kill_job` `wait` |
| 屏幕/输入 | `list_displays` `screenshot`（带 `[screenshot:路径]` 标记供视觉输入）`screen_metrics` `screen_size` `mouse_move` `mouse_click` `mouse_drag` `mouse_drag_path` `mouse_scroll` `keyboard_type` `keyboard_key` `keyboard_combo` `clipboard_read` `clipboard_write` `wait_for_window` |
| 界面自动化 | `ui_act`（按控件名复合操作：click/type/set_value/invoke/scroll）`ui_click` `ui_drag` `ui_tree` `ui_set_value` `ui_invoke_pattern` `ui_window_list` `ui_focus_window`（Windows UI Automation，Electron/Chromium 应用也能读到按钮） |
| 网页 | `web_search`（**国内可用**：百度/搜狗/必应中国/360 并行聚合去重排序，仿 SearXNG，不依赖自建服务）`web_fetch` `web_links` `download_file` |
| 浏览器/前端调试 | `cdp_targets` `cdp_eval` `cdp_click` `cdp_screenshot`（Chrome DevTools Protocol） |
| 系统 | `system_info` `foreground_window` `open_path` `notify` `list_processes` `kill_process` |
| 记忆/技能 | `memory_remember` `memory_recall` `memory_list` `memory_forget`；`skill_list` `skill_load` `skill_install` `skill_uninstall` |
| 子代理 | `spawn_subagent`（可前台等待拿摘要，也可后台跑）`subagent_list` |
| 邮件 | `mail_send` `mail_list` `mail_read` `mail_reply` `mail_mark_read` `mail_folders` `mail_test_connection` `mail_wait_for` |

> 文件搜索**不依赖 Everything**（自实现 walkdir + globset + 正则内容搜索 + 自建 SQLite 文件名索引），
> 网页搜索**不依赖 SearXNG**（直连国内可访问的多个引擎并聚合）。

### 视觉输入（模型真的"看见"屏幕）
- 工具结果里带 `[screenshot:<绝对路径>]` 标记时，引擎会把截图（按 `agent.vision_max_edge` 等比缩小）
  转成图片消息发给模型；模型因此能看懂屏幕、界面截图、报错窗口。
- 默认模型 DeepSeek V4.1-Flash 支持图片输入；`agent.vision=false` 可关闭。
- 聊天里上传的图片同样作为图片消息发给模型；上传的文本/代码文件内容会贴在 prompt 里，
  文件夹会列出顶层条目，其它文件给绝对路径交给文件工具读。

### 计算机操作（截图 → 看 → 点/打字/拖拽）
- 推荐流程：`screenshot`（会自动附图给模型）→ `ui_act`/`ui_click` 按控件名操作 →
  再 screenshot/uia_tree 验证结果；拿不到控件时才用坐标点击。
- 坐标换算是显式的：截图会被缩放，工具输出里有原始尺寸/屏幕尺寸/scale，模型按比例换算物理像素。
- 支持拖拽：`ui_drag`（按控件名）/ `mouse_drag` / `mouse_drag_path`（分步移动，模拟人手）。
- 注意：Windows 上后台进程抢前台必须 `AttachThreadInput`（`ui_act`/`ui_drag` 已内置强制前台）。

### 对话历史与搜索
- 每个会话落库（`data/conversations.sqlite3`），标题取首条消息前 30 字。
- 标题栏「时钟」按钮打开历史：按标题/正文搜索、载入历史会话（消息回填到聊天流）、删除、新建。
- 远端设备同样能列出/搜索/打开历史会话。

### 技能（Skill）
- `skills/<id>/SKILL.md`：YAML front-matter（`id/name/description/when_to_use`）+ 正文说明；
  同目录可放脚本/模板，`skill_load` 会把正文和文件清单交给模型。
- 安装：设置页「从文件安装…」（选 `.md`）或「从 URL 安装」（http/https 下载）；
  内置两个示例技能 `screen-review`、`mail-draft`（首次启动写入）。
- 引擎会在系统提示里列出可用技能（名字 + 适用场景），模型需要时先 `skill_load` 再照做。

### 局域网远程（手机/平板/别的电脑）
- 设置页「远程访问」开启后监听 `0.0.0.0:8787`（可改），桌面端「设备」页显示**二维码 + 8 位一次性配对码**。
- 手机扫码（或手动打开局域网地址）→ 输入设备名 → 配对成功后 token 只存在该设备浏览器里，
  以后**直接访问**即可，不用再扫码；服务端只存 token 的 sha256。
- 远程页面（独立 Web 前端，Firefox Nova 风格、离线可用、不引 CDN）支持：会话列表/搜索、
  聊天流式输出、工具调用卡片、截图查看、设备信息。
- 安全：只接受同一局域网（私有网段）连接、配对码一次性 + 10 分钟过期、Bearer 鉴权、
  截图路径防目录穿越、CSP；可随时在「设备」页撤销某台设备。
- **Windows 首次开启会弹一次防火墙授权**（neosonia.exe 访问网络），点"允许"手机才能连上；
  只在本机测试可以拒绝（loopback 仍然可用）。

### 快捷截图发给模型
- 默认 `Ctrl+Alt+A`：抓主屏 → 作为附件放进输入框 → 自动显示窗口并聚焦，打个问题回车即可。
- 也可以点标题栏的眼睛按钮，或用聊天输入区的截图按钮。

### 数学公式
- 聊天里的 Markdown 支持 `$...# NeoSonia

常驻 Windows / Linux 桌面的 AI Agent。右侧一条窄浮窗，全局快捷键随叫随到；
能读写文件、执行长时间命令、截屏、模拟鼠标键盘、读取其它程序的控件树并点击、
访问网页、直连 IMAP/SMTP 收发邮件。有人设、有长期记忆、能开子代理。

- **前端**：[fenestra](https://github.com/richer-richard/fenestra) 0.41（纯 Rust 原生 GUI：
  winit + wgpu + vello + parley + taffy，没有 webview、没有 HTML/CSS）
- **后端**：Rust（tokio 异步 agent 循环 + 工具执行器）
- **视觉**：Firefox Nova 设计语言（紫为唯一强调色、带紫味中性色、4px 网格、r12/r16），
  全中文界面，**零 emoji**

```
+--------------------------------------------------+
|  N  NeoSonia            [停止] [新对话] [隐藏]     |  48px 标题栏（可拖动）
+--------------------------------------------------+
|  对话   邮件   记忆   人设   设置                  |  40px 导航（2px 品牌发丝）
+--------------------------------------------------+
|  你                                             |  |
|                    +---------------------+      |  |
|                    | 帮我把下载目录整理一下 |      |  |
|                    +---------------------+      |  |
|  [点] run_command                        完成 1.2s |  |
|      powershell -NoProfile ...                  |  |
|  已按日期分好 3 个目录，移动 42 个文件            |  |
+--------------------------------------------------+
|  > 输入指令，回车发送                     [发送] |  输入区
|  就绪                              812 / 340 tok |
+--------------------------------------------------+
```

## 功能

### 系统整合
- **右侧浮窗**：无边框、圆角、始终置顶、不进任务栏；宽度/屏高占比/边距/透明度可调。
  圆角走 Windows 11 DWM 原生圆角，阴影由系统合成器提供。
- **全局快捷键**：默认 `Alt+Space` 显示/隐藏，设置页可改成任意组合（`Ctrl+Alt+S`、`F8`…）。
- **系统托盘**：显示/隐藏窗口、退出（可在设置里关掉）。
- **单进程常驻**：窗口隐藏后 agent 仍在后台执行（长命令、邮件轮询都不中断）。
- 顶部状态行实时显示当前阶段（思考 / 调用工具 / 等待命令输出）、token 用量。

### 工具（模型可调用，注册 75 个）
| 类别 | 工具 |
| --- | --- |
| 文件 | `read_file` `write_file` `edit_file` `append_file` `list_dir` `search_files` `file_info` `make_dir` `copy_path` `move_path` `delete_path` |
| 本地索引 | `find_files`（类 Everything 的文件名毫秒级搜索）`reindex_files` `index_stats` |
| 命令 | `run_command`（默认 1800s 超时、可设 0 无限、流式输出、可取消）`list_jobs` `read_job_output` `send_job_input` `kill_job` `wait` |
| 屏幕/输入 | `list_displays` `screenshot`（带 `[screenshot:路径]` 标记供视觉输入）`screen_metrics` `screen_size` `mouse_move` `mouse_click` `mouse_drag` `mouse_drag_path` `mouse_scroll` `keyboard_type` `keyboard_key` `keyboard_combo` `clipboard_read` `clipboard_write` `wait_for_window` |
| 界面自动化 | `ui_act`（按控件名复合操作：click/type/set_value/invoke/scroll）`ui_click` `ui_drag` `ui_tree` `ui_set_value` `ui_invoke_pattern` `ui_window_list` `ui_focus_window`（Windows UI Automation，Electron/Chromium 应用也能读到按钮） |
| 网页 | `web_search`（**国内可用**：百度/搜狗/必应中国/360 并行聚合去重排序，仿 SearXNG，不依赖自建服务）`web_fetch` `web_links` `download_file` |
| 浏览器/前端调试 | `cdp_targets` `cdp_eval` `cdp_click` `cdp_screenshot`（Chrome DevTools Protocol） |
| 系统 | `system_info` `foreground_window` `open_path` `notify` `list_processes` `kill_process` |
| 记忆/技能 | `memory_remember` `memory_recall` `memory_list` `memory_forget`；`skill_list` `skill_load` `skill_install` `skill_uninstall` |
| 子代理 | `spawn_subagent`（可前台等待拿摘要，也可后台跑）`subagent_list` |
| 邮件 | `mail_send` `mail_list` `mail_read` `mail_reply` `mail_mark_read` `mail_folders` `mail_test_connection` `mail_wait_for` |

> 文件搜索**不依赖 Everything**（自实现 walkdir + globset + 正则内容搜索 + 自建 SQLite 文件名索引），
> 网页搜索**不依赖 SearXNG**（直连国内可访问的多个引擎并聚合）。

### 视觉输入（模型真的"看见"屏幕）
- 工具结果里带 `[screenshot:<绝对路径>]` 标记时，引擎会把截图（按 `agent.vision_max_edge` 等比缩小）
  转成图片消息发给模型；模型因此能看懂屏幕、界面截图、报错窗口。
- 默认模型 DeepSeek V4.1-Flash 支持图片输入；`agent.vision=false` 可关闭。
- 聊天里上传的图片同样作为图片消息发给模型；上传的文本/代码文件内容会贴在 prompt 里，
  文件夹会列出顶层条目，其它文件给绝对路径交给文件工具读。

### 计算机操作（截图 → 看 → 点/打字/拖拽）
- 推荐流程：`screenshot`（会自动附图给模型）→ `ui_act`/`ui_click` 按控件名操作 →
  再 screenshot/uia_tree 验证结果；拿不到控件时才用坐标点击。
- 坐标换算是显式的：截图会被缩放，工具输出里有原始尺寸/屏幕尺寸/scale，模型按比例换算物理像素。
- 支持拖拽：`ui_drag`（按控件名）/ `mouse_drag` / `mouse_drag_path`（分步移动，模拟人手）。
- 注意：Windows 上后台进程抢前台必须 `AttachThreadInput`（`ui_act`/`ui_drag` 已内置强制前台）。

### 对话历史与搜索
- 每个会话落库（`data/conversations.sqlite3`），标题取首条消息前 30 字。
- 标题栏「时钟」按钮打开历史：按标题/正文搜索、载入历史会话（消息回填到聊天流）、删除、新建。
- 远端设备同样能列出/搜索/打开历史会话。

### 技能（Skill）
- `skills/<id>/SKILL.md`：YAML front-matter（`id/name/description/when_to_use`）+ 正文说明；
  同目录可放脚本/模板，`skill_load` 会把正文和文件清单交给模型。
- 安装：设置页「从文件安装…」（选 `.md`）或「从 URL 安装」（http/https 下载）；
  内置两个示例技能 `screen-review`、`mail-draft`（首次启动写入）。
- 引擎会在系统提示里列出可用技能（名字 + 适用场景），模型需要时先 `skill_load` 再照做。

### 局域网远程（手机/平板/别的电脑）
- 设置页「远程访问」开启后监听 `0.0.0.0:8787`（可改），桌面端「设备」页显示**二维码 + 8 位一次性配对码**。
- 手机扫码（或手动打开局域网地址）→ 输入设备名 → 配对成功后 token 只存在该设备浏览器里，
  以后**直接访问**即可，不用再扫码；服务端只存 token 的 sha256。
- 远程页面（独立 Web 前端，Firefox Nova 风格、离线可用、不引 CDN）支持：会话列表/搜索、
  聊天流式输出、工具调用卡片、截图查看、设备信息。
- 安全：只接受同一局域网（私有网段）连接、配对码一次性 + 10 分钟过期、Bearer 鉴权、
  截图路径防目录穿越、CSP；可随时在「设备」页撤销某台设备。
- **Windows 首次开启会弹一次防火墙授权**（neosonia.exe 访问网络），点"允许"手机才能连上；
  只在本机测试可以拒绝（loopback 仍然可用）。

### 快捷截图发给模型
- 默认 `Ctrl+Alt+A`：抓主屏 → 作为附件放进输入框 → 自动显示窗口并聚焦，打个问题回车即可。
- 也可以点标题栏的眼睛按钮，或用聊天输入区的截图按钮。

### 数学公式
- 聊天里的 Markdown 支持 `$...$` 行内公式与 `$$...$$` 块级公式（**压平成一行等宽文本**显示，例如 `E=mc^2`、`(a+b)/c`；不做分数堆叠——嵌套排版在窄栏 + 滚动容器里会触发 fenestra 布局递归爆栈，已用压平方案规避）：
  `\frac`、上下标、`\sqrt`、`\sum/\prod/\int`（带上下限）、希腊字母、常用运算符、`\text{}`、`\left(\right)`；
  复杂矩阵降级为等宽文本显示。

### 开机自启
- 设置页「开机自启」写入 `HKCU\...\Run`（Linux 写 `~/.config/autostart/*.desktop`）。
- 自启时带 `--autostart`：**只跑后端，不打开窗口**（托盘和全局快捷键随时唤出）；
  手动双击启动则立即显示窗口。手动运行 `neosonia.exe --autostart` 也可以模拟。

### 性能与动效
- 主题/色板缓存（不再每帧重建）、流式增量按 60ms 合批、聊天流只渲染最近 80 条（更早的用历史页看）、
  Tick 250ms、工具卡片结果只在完成时处理。
- 动效：导航下划线/按钮颜色过渡、消息进场、历史覆盖层与 Toast 淡入、托盘与窗口图标（logo）。



### v0.5 总览：性能、稳定性与交互打磨

**前端流畅度**
- **切页两段式**：点导航立刻显示「正在加载…」，下一帧再构建目标页；首次打开文字多的页面不会看起来卡死。
- **字形预热**：启动 4 秒后后台把界面用到的全部中文字形排版一遍（只做 shaping、不上 GPU，约 40-110ms），把 parley 字体回退缓存热起来；设置页首帧布局从 ~1.6s 降到 ~5-10ms。
- 实测（本机 Intel HD530 / DX12 / 250% DPI）：页面切换 p50≈1ms、p95≈4.5ms；`view` 构建 p50≈0.7ms；单帧 `paint` p50≈38ms、`present` p50≈9ms（vello 在集显上的固定成本）。
- 调试开关：`NEOSONIA_FRAME_LOG=1` 输出 `[frame] total/view/layout/paint/present`；`NEOSONIA_AA=msaa8|msaa16` 换抗锯齿模式。

**窗口与图标**
- 隐藏 = 真 `set_visible(false)`（shell 隐藏时跳过渲染，不阻塞主线程）；显示延迟实测 ~130ms。
- **只在窗口弹出时出现在任务栏**（隐藏时 `WS_EX_TOOLWINDOW`）；任务栏/Alt-Tab 图标用项目 logo 并在每次显示时重设，修掉"有时变成默认 exe 图标"。
- 顶栏左上角换成项目 logo；托盘提示文案跟随实际快捷键设置（不再写死 Alt+Space）。

**稳定性**
- 主线程消息处理与视图构建包 `catch_unwind`：单个面板/事件 panic 只记日志 + 提示，不再整个应用退出，并显示降级界面；panic 自动写 `logs/crash-<时间>.txt`（含 backtrace）。
- 工具执行 panic 隔离（core）：转成中文工具错误返回模型，Agent 继续下一轮；截图（xcap）串行化 + 后台线程。

**上下文与模型**
- **上下文压缩**：默认 1M 上下文；历史超过 70 万 tokens 时自动把旧消息摘要成一段（≤1500 字）放最前、保留最近 40 条原文；失败降级不中断；UI 提示、web 可见，设置页可开关。
- 默认最大输出 token 8192 → **32768**（已保存的旧配置自动迁移）；DeepSeek 单次输出上限 384K。
- **DeepSeek 官方联网搜索**：`web.search_provider = "deepseek"` 时用官方 Search API（`POST /v1/tools/search`，实时结果 + rerank 分数），失败/无 key 自动回退聚合搜索；`web_fetch` 直连失败或正文过短时用 DeepSeek Fetch API 兜底。
- 工作模式可由模型用 `switch_mode` 工具在任务中主动切换。

**人设可视化编辑**：人设页新增「新建人设」与卡片「编辑」；同页编辑 id/名称/描述/正文，保存/取消/删除，内置人设禁删，id 校验与重名中文提示。

**邮箱 OAuth 授权码**：点「用 Microsoft 登录」后设置页直接显示浏览器地址 + 8 位授权代码并自动复制——之前只有"已打开浏览器"没有代码，无法完成授权。Google 仍走 loopback 自动回调。

**计算机使用**：新增 `mouse_press` / `mouse_release` / `keyboard_press` / `keyboard_release`（按住→多次移动→松开，真实拖拽/长按/组合键）；`mouse_drag_path` 增加 `hold_ms`；工具数 58 → 62。

**设备配对与连接**：扫码 `/pair#<code>` 自动填码并自动配对直接进聊天；已有 token 先验证直达；401 自动回配对页提示；WS 心跳 25s、连续 3 次无响应才显示断开、静默重试 2 次后横幅 + 1/2/5/10/15s 退避重连，重连恢复会话/未读/审查；设备页显示连接状态与「断开本设备」。

### 全局快捷键（v0.4 重写，已修复"按一次就不灵"）
- `Alt+Space`：显示 / 隐藏窗口。**隐藏是把窗口移到屏幕外**（不是 SW_HIDE），这样全局热键始终有效，唤回也更快。
- `Alt+Space` 之后 1.5 秒内按 `S`：立即截图 → 打开窗口 → 截图附加在聊天输入框（`ui.screenshot_chord` / `ui.chord_timeout_ms` 可改）。
- 单键截图快捷键默认 `Ctrl+Alt+A`，可在设置页改。
- 实现：`RegisterHotKey` + 独立消息线程（v0.3 的两个 `global-hotkey` 服务共用一个全局事件通道，先轮询者会吞掉另一个的按键——这是旧版本 Alt+Space 失效的根因）。

### 性能（v0.4）
- 聊天列表**虚拟化**（`virtual_rows_variable`）：只构建视口内的消息，长会话滚动不再掉帧；快照按版本号缓存，流式输出不重复克隆。
- 设置页「技能 / 本地索引 / 邮件 / 远程访问 / 人设与记忆」改为**折叠面板**（默认折叠、展开才构建内容），首帧不再一次性建上千节点。
- 托盘菜单事件由独立线程直接投递（40ms），点托盘立刻响应，不再等主循环 Tick。
- 截图（`Ctrl+Alt+A` / 组合键）改为**后台线程**抓屏 + 编码，主线程不卡。
- 执行中：顶栏状态点脉冲 + 导航下 2px 加载条；页面切换/构建耗时写入日志（`NEOSONIA_PROFILE=1` 时每帧都记）。

### 提示词隔离模式（v0.4）
- 三种模式，模型可主动用 `switch_mode` 工具切换，设置页也有三张卡片：
  - **日常（daily）**：人设 + 长期记忆 + 文件/搜索提示；计算机操作提示词按需（本轮用过截图/键鼠/UIA 后下一轮注入一次）。
  - **编程（coding）**：不注入人设，专注编程规范；文件/命令提示保留，计算机操作按需。
  - **计算机使用（computer_use）**：不注入人设；计算机操作 + 文件 + 命令提示全部展开（深度 ComputerUse）。
- 基础安全提示词在任何模式下都保留。技能按模式注入：安装技能时自动用 LLM 分类并写入 `SKILL.md` 的 `modes`（daily/coding/computer_use），只列当前模式适用的技能；`skill_load` 不受限。

### 命令审查（风险评分，v0.4）
- 默认开启，可在 设置 → 命令审查 关闭（关闭 = 完全权限）。
- 有副作用的操作执行前，由**独立、无人设的同一模型**打分（1-4）：
  - **1 分**：直接执行；
  - **2 分**：弹窗确认，允许后同类操作 **1 天内自动执行**；
  - **3 分**：每次都弹窗确认；
  - **4 分**：弹窗且强调风险，拒绝后**同类操作 1 天内自动拒绝**；弹窗里可选「拒绝并 1 天内拒绝同类」。
- "同类操作"按类别记忆：文件按顶层目录（如 `C:/Windows`）、命令按语义（删除/网络上传/安装/注册表/提权…）+ 目标、邮件按域名、键鼠按窗口。
- 弹窗内容：风险等级、工具名、参数摘要、原因、同类操作标识；局域网设备页只读展示待审列表（远程不能批准）。

### 邮箱 OAuth2 登录（Thunderbird 式，v0.4）
- 设置 → 邮件里有「用 Microsoft 登录」「用 Google 登录」，点一下打开浏览器授权，**不需要手填密码/授权码**：
  - Microsoft/Outlook/M365：device code 流程（显示 8 位码，浏览器输入）；
  - Gmail：loopback + PKCE（自动开本地回调）。
- 登录后 refresh token 存在本地配置，IMAP/SMTP 自动用 XOAUTH2；内置 Thunderbird 的公开 client_id，也可在 `[mail.oauth]` 里换成自己的应用。详见 `crates/neosonia-mail/README.md`。

### 连接其他设备入口（v0.4 补齐）
- 顶栏第二个图标（分享图标）= 连接其他设备，直达「设备」页；
- 托盘菜单「连接其他设备…」；
- 设置页「界面」小节里也有「打开设备页」按钮，未启用时页头还有提示条。
### 后台命令与状态查询（v0.3）
- 长命令/服务用 `run_command {background:true}` 丢到后台，立即返回任务 id（不再占住一轮对话）。
- `job_status {job_id?}`：**一次调用**看所有后台任务的状态（running / finished / cancelled / failed）、已运行时长、退出码、最后 20 行输出；`job_id` 可选，只看某一个。
- 配套：`read_job_output {job_id, wait_ms}` 看完整输出并阻塞等待、`send_job_input` 给交互式程序喂输入、`kill_job` 终止、`list_jobs` 列简表。
- 实测：node 后台任务 3 秒后 `job_status` 显示 running + 尾部时间戳；`kill_job` 后变 cancelled。

### 鼠标 / 键盘 / 带坐标的截图（v0.3）
- `mouse_click {button:"left"|"right"|"middle", double:true, count:N}`：左键、右键、中键、**左键双击**；`mouse_scroll {direction:"up"|"down", amount:N}` 滚轮。
- `screenshot {grid:true}`：截图叠加**坐标网格**（100px 细线 / 500px 粗线 + 上边 x 刻度、左边 y 刻度，物理像素），返回里给出 grid 间距、原点、缩放因子和 `[screenshot:路径]` 标记 —— 模型可以照着刻度读出精确坐标再点，不用猜。
- 真机验证：记事本左键点中间后输入标记（字符数 66→72、文件内容证明光标定位）；`double=true` 双击选中单词后被输入替换；右键弹出上下文菜单（截图证据）；滚轮滚动前后内容上移。

### 屏幕活动提示（HUD，v0.3）
- 模型调用「看屏幕 / 鼠标 / 键盘 / UIA / CDP」类工具时，主屏四周亮起一圈提示条 + 顶部居中胶囊：
  - 紫色 = 正在查看屏幕（`NeoSonia 正在查看屏幕`）
  - 橙色 = 正在操作（`NeoSonia 正在操作鼠标/键盘/界面/窗口`）
  - 工具结束后显示 `NeoSonia 已完成`，0.9 秒自动消失；点击穿透、不抢焦点。
- 设置页可关闭：界面 →「计算机操作时屏幕提示」（`ui.computer_activity_indicator`）。

### MCP（Model Context Protocol，v0.3）
- 支持连接 **MCP 服务器**（stdio 子进程 / Streamable HTTP），把服务器提供的工具注册成 `mcp_<server>_<tool>` 直接给模型用，另有 `mcp_list_servers` 查连接状态。
- 配置在 `config/settings.toml`：
```toml
[mcp]
enabled = true
max_total_tools = 100

[[mcp.servers]]
name = "filesystem"                # 工具名前缀 mcp_filesystem_
enabled = true
transport = "stdio"                # stdio | http
command = "cmd"                    # Windows 上 npx/npm 要用 cmd /c 包一层（见下）
args = ["/c", "npx", "-y", "@modelcontextprotocol/server-filesystem", "E:/workspace"]
startup_timeout_secs = 120         # npx 首次要联网下包，给足
call_timeout_secs = 120

# HTTP 方式：
# [[mcp.servers]]
# name = "remote"
# transport = "http"
# url = "http://127.0.0.1:3000/mcp"
```
- 设置页有「MCP 服务器」小节显示每台的连接状态/工具数/错误，可单台重启；**改配置需要重启应用生效**。
- Windows 注意：`command = "npx"` 起不来（Node 只提供 `npx.cmd`，CreateProcess 不认 `.cmd`），请用 `command="cmd"` + `args=["/c","npx",...]`，或直接 `command="node"` 指向包入口。
- 实测：内置 demo server（stdio）连上 → `mcp_demo_echo/add/sleep` 三个工具注册 → 模型调用 `mcp_demo_add {a:2,b:3}` 返回 `{"sum":5}`；HTTP 传输（SSE 回包）也测过。
### 长任务
`run_command` 不是"30 秒就断开"的实现：默认超时 30 分钟（可配置为 0 = 无限），
执行期间标准输出实时流到界面；可以转后台跑，再用 `read_job_output wait_ms=...` 阻塞等结果，
`send_job_input` 还能给交互式程序喂输入。

### 人设与长期记忆
- 人设文件：`personas/*.md`（YAML front-matter：`id/name/description/initial_memory` + 正文即 system prompt）。
  内置 `default.md`，设置页/人设页可切换与重新扫描。
- 长期记忆：SQLite（`data/memory.sqlite3`），三类内容——事实/偏好/事件、
  固定块（persona/human）、对话流水。检索用 关键词 + 标签 + 重要度 + 时间衰减 打分，
  每轮对话前自动注入相关记忆，模型可主动 `memory_remember`。

### 邮件
- **纯 IMAP 收 + SMTP 发**，不依赖 Outlook、不用本机邮件客户端、不用 COM。
- 连接参数（主机/端口/TLS/账号/应用密码/发件人）全部在设置页填写，可测试连接。
- **白名单**：只有白名单地址（支持 `someone@corp.com` 精确、`@corp.com` 后缀）的来信才会
  自动回复；给白名单外的人发信默认被拦截（可在设置里放开）。
- 后台轮询（间隔可调）+ 去重（`data/mail_seen.json`），白名单来信自动交给 agent 起草回复。
- `mail_wait_for`：阻塞等待某人的来信（最长 10 分钟），配合"等命令结果"的工作流。

## 构建

需要 Rust 1.85+（MSVC 工具链）。Windows 上需要 VS 2022 Build Tools；Linux 需要
`libxkbcommon`、`libwayland`/X11、`libopenssl`（native-tls）等常见桌面依赖。

```powershell
cd E:\NeoSonia
cargo build --release
# 产物：target\release\neosonia.exe
```

开发运行：

```powershell
cargo run -p neosonia-app
# 指定独立的配置/数据目录（便于测试）：
cargo run -p neosonia-app -- --home E:\NeoSonia\sandbox
```

测试：

```powershell
cargo test --workspace
```

## 配置

首次启动会生成 `config/settings.toml`（也可用 `--home` 换根目录）。
最小可用配置：

```toml
[server]
base_url = "https://api.deepseek.com/v1"   # 任何 OpenAI 兼容端点
model = "deepseek-chat"
api_key = "sk-..."                          # 也可以用 env:DEEPSEEK_API_KEY
reasoning_effort = "high"                   # DeepSeek 思考模式

[ui]
hotkey = "Alt+Space"
width = 424.0
always_on_top = true

[tools]
command_timeout_secs = 1800                 # 0 = 不限时

[mail]
enabled = true
whitelist = ["boss@example.com", "@myteam.com"]
poll_interval_secs = 300
auto_reply = true

[mail.imap]
host = "imap.qq.com"
port = 993
tls = true
user = "you@qq.com"
password = "env:NEOSONIA_IMAP_PASSWORD"     # 建议用环境变量，别写明文

[mail.smtp]
host = "smtp.qq.com"
port = 465
implicit_tls = true
user = "you@qq.com"
password = "env:NEOSONIA_SMTP_PASSWORD"
from = "you@qq.com"
from_name = "NeoSonia"
```

常见邮箱参数：

| 邮箱 | IMAP | SMTP |
| --- | --- | --- |
| QQ | `imap.qq.com:993` TLS | `smtp.qq.com:465` 隐式 TLS（需"授权码"，不是登录密码） |
| 163 | `imap.163.com:993` TLS | `smtp.163.com:465` 隐式 TLS（需授权码） |
| Gmail | `imap.gmail.com:993` TLS | `smtp.gmail.com:587` STARTTLS（需应用专用密码） |
| Outlook.com | `outlook.office365.com:993` TLS | `smtp.office365.com:587` STARTTLS（需应用密码） |

密钥优先级：`config/settings.toml` > 环境变量（`NEOSONIA_API_KEY` / `DEEPSEEK_API_KEY` / `OPENAI_API_KEY`）。

## 目录结构

```
crates/
  neosonia-core/   # 配置、LLM 客户端、agent 循环、子代理、记忆、人设、工具协议（不含具体工具）
  neosonia-tools/  # 文件/命令/屏幕/输入/系统/网页/UIA/CDP 工具实现
  neosonia-mail/   # IMAP+SMTP 服务、白名单、轮询、邮件工具
  neosonia-app/    # fenestra 原生 GUI（浮窗、聊天流、设置/记忆/邮件/人设面板、快捷键）
vendor/
  fenestra-shell/  # 打了补丁的 fenestra shell：置顶/无边框/透明/物理定位/窗口显隐停靠
docs/
  fenestra-api.md     # 从源码逐条核对的 fenestra 0.41 API 手册
  ui-design.md        # Firefox Nova 设计规范（色板/间距/组件/线框）
  contracts.md        # 四个 crate 的公共契约
data/            # 运行期数据：memory.sqlite3、screenshots/、mail_seen.json
personas/        # 人设 markdown
workspace/       # agent 的默认工作目录
logs/            # 运行日志
```

## 安全

- 所有文件工具默认限制在 `workspace/` 内（`allow_outside` 显式开启才越界）。
- 危险操作（删除、复制移动、结束进程、执行任意命令）在 `agent.approval_mode = "ask"` 时会
  在界面上请求确认；默认 `auto`（全都放行），可改成 `ask` 收紧。
- 邮件发送受白名单约束；自动回复只对白名单来信触发。
- 密钥只写本地配置文件（`config/settings.toml`），界面里只显示脱敏值。

## 开发/排查小工具

程序除了正常启动，还支持几个排查用的命令行模式（都不影响正常使用）：

```powershell
# 无界面跑一轮 agent，把思考/工具调用/回复打到 stdout（联调、脚本自动化）
neosonia.exe --home E:\NeoSonia\sandbox --ask "看看 workspace 里有什么文件"

# 只开一个最小 fenestra 窗口（排查 GPU/窗口/合成问题）
neosonia.exe --probe

# HTTP/TLS 连通性探针（排查代理、证书、防火墙）
neosonia.exe --http-probe https://api.deepseek.com/models
```

环境变量：

| 变量 | 作用 |
| --- | --- |
| `NEOSONIA_API_KEY` / `DEEPSEEK_API_KEY` / `OPENAI_API_KEY` | 没写进配置时用它们当密钥 |
| `NEOSONIA_HOME` | 覆盖配置/数据根目录 |
| `NEOSONIA_WIN_TRANSPARENT=0/1` | 强制关闭/打开透明窗口 |
| `NEOSONIA_WIN_DECORATIONS=1` | 打开系统标题栏（调试用） |
| `NEOSONIA_WIN_TOPMOST=0` | 关闭置顶 |
| `NEOSONIA_NO_DOCK=1` | 不自动停靠到屏幕右侧 |
| `NEOSONIA_DUMP_UI=1` | 启动时把界面用 headless 渲染成 `logs/headless-ui.png` |
| `NEOSONIA_PAGE=settings\|memory\|mail\|persona` | 启动时直接打开某页（配合 DUMP_UI 做视觉回归） |
| `NEOSONIA_GPU_LOG=1` | 打印所选 GPU 后端与 surface 能力 |
| `WGPU_BACKEND` | 覆盖图形后端（Windows 默认锁 `dx12`，见下） |
| `RUST_LOG` | 日志级别，默认 `warn`，写成 `neosonia=info` 可看详细信息 |

## 常见问题

**窗口是全白 / 什么都没有。**
混合显卡（Intel 集显 + NVIDIA 独显）机器上，wgpu 默认可能选到 Vulkan 设备，渲染出的
swapchain 在集显驱动的桌面上会呈现为空白。NeoSonia 在 Windows 上默认把 `WGPU_BACKEND`
锁成 `dx12`（可用环境变量覆盖）。用 `NEOSONIA_GPU_LOG=1` 能看到实际后端和 surface 能力。

**模型请求失败：连接失败 / invalid peer certificate。**
如果装了会做 HTTPS 中间人的安全软件（卡巴斯基、企业代理等），rustls 的根证书库不认它的证书，
请求会失败。NeoSonia 的 HTTP 客户端用的是 **native-tls**（走 Windows 证书库），所以正常情况
不会出现；若仍失败，先用 `--http-probe https://api.deepseek.com/models` 定位是网络、代理还是证书问题。

**全局快捷键没反应。**
快捷键可能被别的软件占用（注册失败时界面会提示）。换个组合（例如 `Ctrl+Alt+S`）在设置页重新应用即可。
另外全局快捷键在 Wayland 下不受支持。

**透明/圆角效果。**
在只支持不透明 surface 的驱动上（本机 Intel HD 530 + DX12 就是），窗口用 Windows 11 的
DWM 原生圆角实现浮层轮廓，`ui.opacity` 会失效（保持 1.0）。支持 alpha 合成的机器上把
`ui.opacity` 调到 0.9~0.98 可得到半透明效果。

**鼠标/键盘模拟无效。**
如果桌面上有全屏置顶的"黑屏/锁定"类工具（例如某些护眼、防偷窥、屏幕锁定程序），它会截获
合成的鼠标键盘事件，`mouse_click`/`keyboard_type` 就不会生效。关掉那类工具再试。
UI Automation（`ui_tree`/`ui_click`）走的是无障碍接口，一般不受影响。

## 验收记录

一份逐项的实测记录（构建、170 个测试、真实模型端到端、UIA/CDP 真机、UI 截图、环境限制）见 [docs/verification.md](docs/verification.md)。

## 构建与发布

```powershell
# 开发构建（快）
cargo build --offline -p neosonia-app

# 运行（不要用普通权限与管理员混跑；--home 指定数据目录）
target\debug\neosonia.exe --home E:\NeoSonia

# Release 包（测试 + release 构建 + zip + sha256）
pwsh scripts\build-release.ps1
```

- Release profile：`opt-level = 3`、`lto = "thin"`、`strip = "symbols"`（见根 `Cargo.toml`）。
- 产物：`dist\NeoSonia-<版本>-win64\`（exe + assets + README + docs）与 `dist\NeoSonia-<版本>-win64.zip`。
- **发布包不包含** `config\`、`logs\`、`data\`、`sandbox\`：避免 API key / 邮箱 token 泄露。
- 版本号来自根 `Cargo.toml` 的 `[package] version`（当前 0.9.0）。
- 已知限制：管理员模式下向窗口拖放文件受 Windows UIPI 限制（已用 `ChangeWindowMessageFilterEx` 放行，个别安全软件仍可能拦截，可用「附件」按钮或「复制文件 → Ctrl+V」）。

## 测试

```powershell
cargo test --offline --workspace     # 全量（core/mail/tools/web/mcp/app）
cargo test --offline -p fenestra-shell --lib   # 窗口/拖放兜底单测
```

### 安装包（Windows）

```powershell
# 1) 先构建 release exe
powershell -ExecutionPolicy Bypass -File scripts\build-release.ps1
# 2) 生成自解压安装包（内嵌卸载程序，注册到「应用和功能」）
powershell -ExecutionPolicy Bypass -File scripts\build-installer.ps1
```

产物：`dist\NeoSoniaSetup-0.9.0-win64.exe`。静默安装 `/S [/D=<目录>]`，默认装到
`%LOCALAPPDATA%\Programs\NeoSonia`；配置/聊天记录/workspace/personas/skills/logs 均在安装目录下；
卸载用安装目录里的 `Uninstall.exe`（「应用和功能」里也可卸载）。

## 许可

MIT License。详见 [LICENSE](LICENSE)。第三方组件许可见 [NOTICE.md](NOTICE.md)。

第三方依赖遵循各自许可证（fenestra 系列、winit、wgpu、vello、parley、taffy、tokio、axum 等，
均为 MIT / Apache-2.0）；`vendor/` 下的 fenestra-core / fenestra-shell 为本地 patch 版本，
其上游许可证随源码保留。
