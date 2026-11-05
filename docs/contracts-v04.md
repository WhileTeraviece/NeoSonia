# 第四阶段（v0.4）契约与约定

日期：2026-11-01。本文件只记录**跨 crate 的新增/变更接口**，避免各写各的。
旧契约见 `docs/contracts.md`、`docs/contracts-phase2.md`，仍然有效。

## 0. 配置（core，已冻结，勿改字段名）

```rust
// neosonia_core::config
pub enum AgentMode { Daily, Coding, ComputerUse }   // daily / coding / computer_use
impl AgentMode { pub fn label(&self) -> &'static str; pub fn id(&self) -> &'static str; }

pub struct ReviewSettings {
    pub enabled: bool,            // 默认 true；false = 完全权限（旧行为）
    pub review_all_tools: bool,   // 连只读工具也审
    pub allow_ttl_secs: u64,      // 2 分授权后同类自动执行有效期，默认 86400
    pub deny_ttl_secs: u64,       // 4 分拒绝后同类自动拒绝有效期，默认 86400
    pub model: String,            // 空 = 主模型
    pub timeout_secs: u64,
}

pub enum OAuthProvider { Microsoft, Google, Custom }
pub struct MailOAuthSettings {
    pub enabled: bool, provider: OAuthProvider, client_id: String, tenant: String,
    pub email: String, refresh_token: String, access_token: String, expires_at_ms: i64,
    pub auth_url: String, token_url: String, scope: String,
}

// Settings 新增字段：ui.screenshot_chord（默认 "S"）、ui.chord_timeout_ms（默认 1500）、
// agent.mode（默认 Daily）、review、mail.oauth
```

## 1. core（core-llm 负责）

- 工具：`switch_mode { mode: "daily"|"coding"|"computer_use" }`。
- 事件新增：
  - `ModeChanged { mode: String, label: String }`
  - `RiskReviewed { tool: String, level: u8, class: String, reason: String }`
  - `ApprovalRequested` **扩展**（保留旧字段）：`risk_level: u8, risk_reason: String, class: String, emphasized: bool`
- 审批 API：`AgentEngine::approve(call_id, ok)` 保留；新增
  `approve_decision(call_id, RiskChoice::{Approve, Deny, DenyClass})`。
- `SkillSummary` 新增 `modes: Vec<String>`（值域 daily/coding/computer_use）。
- `ConversationStore` 新增只读：`messages_before(conv_id, before_seq: Option<i64>, limit) -> Vec<(i64, ChatMessage)>`（seq 升序）、`last_seq(conv_id) -> i64`。
- 模式提示词策略：
  - daily：人设 + 记忆 + 搜索/文件；计算机使用段落**按需**（本轮用过计算机类工具后下次请求追加）
  - coding：**无人设**；基础安全 + 编程规范 + 文件/命令；计算机使用按需
  - computer_use：**无人设**；基础安全 + 计算机使用/文件/命令全部展开
- 技能安装后用 LLM 自动分类，写 `SKILL.md` front-matter `modes: [..]`；注入按当前模式过滤。

## 2. app（Lead 负责）

- 全局快捷键：`WH_KEYBOARD_LL` 钩子（忽略注入事件），主键切换显隐，主键后 `screenshot_chord`（默认 S）在 `chord_timeout_ms` 内 = 立即截图发模型。
- 托盘事件由独立线程直接投递 `Msg`（40ms 轮询），菜单含「显示/隐藏窗口」「连接其他设备…」「退出」。
- 聊天列表用 `virtual_rows_variable` 虚拟化 + 快照缓存（`items_version`）。
- 设置页折叠面板由 ui-panels 负责，首帧只构建标题行。
- 审查弹窗：`AppState.pending_risk: Option<RiskPrompt>`，按钮发 `Msg::RiskDecision { call_id, decision }`。
- 顶栏：设备入口按钮（OpenPage(Remote)）、模式 chip、忙碌脉冲点 + 2px 加载条。

## 3. web（web-remote 负责）

- 进程级 `neosonia_web::review_hub()`：`push/resolve/clear/pending`，`GET /api/review/pending`，WS `risk_review`。
- `/api/me` server 增加 `mode`/`mode_label`。
- 会话消息分页：`GET /api/sessions/{id}?before=<seq>&limit=100`（用 core 的 messages_before/last_seq）。
- SPA：骨架屏、rAF 合批流式、>200 条虚拟化、失败重试。

## 4. mail（mail 负责）

- `neosonia_mail::OAuth`：`start_flow(provider, client_id, tenant) -> OAuthFlow`（Microsoft=device code，Google/Custom=loopback PKCE）、`access_token()` 自动刷新。
- `build_xoauth2(user, token)`；IMAP/SMTP 在 `mail.oauth.enabled` 时走 XOAUTH2。
- app：`Msg::MailOAuthStart("microsoft"|"google")` → 打开浏览器 → 授权完成后写回 `settings.mail.oauth`（refresh_token/email/expires_at_ms）并把状态显示在设置页。
