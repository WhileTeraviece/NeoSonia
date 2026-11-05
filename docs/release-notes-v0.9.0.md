# NeoSonia v0.9.0 Release Notes

写于 2026-11-04，发布准备稿（明天打包）。

## 是什么

常驻 Windows 桌面的 AI Agent：右侧窄浮窗（`Alt+Space` 呼出），能读写文件、执行长命令、截屏、
模拟鼠标键盘、读取其它程序控件树、访问网页、直连 IMAP/SMTP 收发邮件；有人设、长期记忆、子代理、
MCP 工具与技能，支持网页远程访问（局域网设备）与多模式提示词隔离、命令风险审查。

- 前端：fenestra 0.41（纯 Rust 原生 GUI：winit + wgpu + vello + parley + taffy，无 webview）
- 后端：Rust（tokio 异步 agent 循环 + 工具执行器）
- 视觉：Firefox Nova 设计语言，全中文、零 emoji
- 平台：Windows 10/11 x64（Linux 构建可选，未随本包发布）

## 本版重点

### 本机体验
- **窗口健壮性**：显示时自检/重新停靠，修复"最小化后用 Alt+Space 变半个窗口"；禁止点任务栏图标缩进任务栏；管理员模式下不再弹 reg.exe（移除全部 `reg` 子进程，开机自启改 advapi32，fenestra 的"减少动效"检测改 `SystemParametersInfoW`）。
- **输入**：中文输入法（拼音）在中间位置输入时光标不再跳动、组合期间方向键可移动；Ctrl+V 粘贴图片/文件（`CF_HDROP`）；拖文件到窗口任意位置（Win32 `WM_DROPFILES` 兜底，管理员模式经 `ChangeWindowMessageFilterEx` 放行）。
- **数学与 Markdown**：行内/块级公式（分式/根式/上下标/希腊字母/aligned/cases/matrix/array，长公式横向滚动）；代码块语言标签 + 横向滚动；有序列表/长链接/引用/跨图片加粗不再溢出；模型交付图片缩略图与 `.lnk` 快捷方式卡。
- **API 可用性小球**：识别 DeepSeek/OpenAI/Anthropic/Groq/Mistral/Cohere，DeepSeek 走 Atom/HTML 专用适配，绿/黄/橙/红，输入框右下角显示；API 故障时模型无响应不空重试。
- **稳定性**：模型请求退避重试，连续 5 次未响应终止本轮；"停止 → 立刻再发"的新轮次不会被旧取消标志吞掉。

### 邮件
- IMAP/SMTP OAuth2（Microsoft/Google）设备码/授权码流程；IMAP 与 SMTP 双通道测试。
- 换账号原子替换（email 变化清旧凭证并同步 IMAP/SMTP 用户），登录失败保留旧凭证；退出登录仅清授权。
- 白名单（可加备注）、附件发送、收件箱滚动/详情长 URL 不溢出、返回收件箱图标按钮。

### 远程访问（局域网）
- `0.0.0.0` 绑定 + 访问地址展示 + 手工配对/自动配对 + 心跳重连 + 401 回配对。
- 手机端：附件上传按钮与拖放（浮层提示）、API 小球横栏、表格/公式/代码块、设备重命名、审批只读。
- **宽屏自适应**：左栏 340px 会话列表常驻，右栏对话/设备页互切；手机竖屏保持单栏。
- 桌面端处理审批/完成任务后，远程通过 WS 事件自动刷新（`review_resolved`/`review_cleared`/`turn_finished`）。

### 设置与管理
- 模型服务多 profile（服务器/模型/Key）、思考强度 4 档（不思考/low/high/max）、温度/最大 token。
- 命令审查 1–4 风险等级 + allow/deny TTL；子代理并发、MCP 服务器、技能、人设与记忆。
- 设备重命名、OAuth 诊断、开机自启（注册表 API）。

## 已知限制
- **管理员模式下从资源管理器拖放**：Windows UIPI 默认拦截；本版已用 `ChangeWindowMessageFilterEx` 放行 `WM_DROPFILES`/`WM_COPYDATA`，实测可用；若个别安全软件拦截，可用「附件」按钮或「复制文件 → Ctrl+V」。
- 远程上传进度为客户端估算；服务端不建附件元数据库（从文件名重建）；文本附件 >8000 字只内联前 8000 字并给完整路径。
- vendor/fenestra-core 与 vendor/fenestra-shell 为本地 patch（IME、文件拖放、窗口行为），升级 fenestra 需 rebase。

## 构建
```powershell
pwsh scripts\build-release.ps1          # 测试 + release 构建 + zip + sha256
pwsh scripts\build-release.ps1 -SkipTests
```
产物：`dist\NeoSonia-0.9.0-win64\` 与 `dist\NeoSonia-0.9.0-win64.zip`。
发布包不含 `config\`、`logs\`、`data\`（避免 API key / 邮箱 token 泄露）。

## 验证
见 `docs/verification.md`（v0.5–v0.9 各阶段真机证据、截图路径、测试计数）。
发布前最后基线：app 70、shell 18、core 130、mail 107、web 97、tools 129、fenestra-core input 7。

## 安装包（自解压 + 内置卸载）

- 产物：`dist\NeoSoniaSetup-0.9.0-win64.exe`（17.8 MB，内嵌 payload.zip + Uninstall.exe）。
- 安装：双击（或 `/S` 静默，`/D=<目录>` 指定位置）；默认 `%LOCALAPPDATA%\Programs\NeoSonia`，交互式安装可**浏览选择任意目录**（`/D=<目录>` 供静默安装）。
- **数据跟随安装位置**：`config\`、`data\`、`logs\`、`workspace\`、`personas\`、`skills\` 全部在安装目录下；
  未指定 `--home` 时应用默认使用 exe 所在目录（可写时），因此绿色/便携与安装版行为一致。
- 注册到 Windows「应用和功能」：`HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\NeoSonia`
  （DisplayName / DisplayVersion / Publisher / DisplayIcon / UninstallString / QuietUninstallString / EstimatedSize）。
- 快捷方式：开始菜单 `NeoSonia` 与 `卸载 NeoSonia`；桌面 `NeoSonia`（带 `--home "<安装目录>"`）。
- **卸载**：`<安装目录>\Uninstall.exe`（或「应用和功能」里点卸载，`/S` 静默），删除安装目录（含配置/聊天记录/workspace/skills）、注册表项与快捷方式。
- 构建：`powershell -ExecutionPolicy Bypass -File scripts\build-installer.ps1`（需已建 release exe；用 Windows 自带 csc，无需 Inno Setup）。
- 安装包**不含任何用户数据/密钥**：首次启动生成空配置。

## v0.9.5 图标与版本信息

- exe 现在内嵌多尺寸图标（16–256px）与 VERSIONINFO：资源管理器/任务栏/文件属性不再显示默认空白图标。
- 构建需要 Windows SDK `rc.exe`（`embed-resource` 自动定位）；非 Windows 目标自动跳过。
- `docs\verification.md` 有验证记录；`scripts\build-release.ps1` / `build-installer.ps1` 无需改动，自动带上新资源。
