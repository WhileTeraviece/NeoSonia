# neosonia-mail

NeoSonia 的内置邮件能力（IMAP 收信 / SMTP 发信），**不依赖 Outlook、win32com 或任何本机邮件客户端**。
所有连接参数都在设置页可改；密码/授权码只存本机。

## 模块

- `oauth`：OAuth2 登录（Microsoft device code / Google loopback PKCE）+ token 刷新 + XOAUTH2；
  交互流程与刷新走 [`mailledger-oauth`](https://docs.rs/mailledger-oauth) 0.0.2（MIT），外面有一层薄适配。
- `service`：`MailService`（IMAP 收信 / SMTP 发信 / 连接诊断），阻塞网络操作都放进
  `tokio::task::spawn_blocking` + `tokio::time::timeout`，不阻塞 UI。
- `parse`：MIME 解析（HTML 转纯文本、附件标记、Message-ID/References）。
- `poller`：后台轮询新邮件，通过 `MailEvent` 推给 app；去重存 `data/mail_seen.json`。
- `tools`：8 个工具（`mail_send/list/read/reply/mark_read/folders/test_connection/wait_for`）+
  白名单/发件限制。
- `presets`：服务商预设（Outlook/Gmail/163/126/QQ/自定义）+ `apply_oauth_result` / `migrate_settings`。
- `attachments`：发信附件校验与 MIME 推断。

## OAuth 客户端 ID（v0.6.5）

**Microsoft 默认使用 Microsoft Office 的公共客户端 ID `d3590ed6-52b3-4102-aeff-aad2292ab01c`**
（用户指定；这是广为人知的 Microsoft 第一方 public client，授权页会显示 **“Microsoft Office”**，
不是 Thunderbird，也不是 NeoSonia）。它不需要 `client_secret`，`tenant=common`
（个人 outlook.com + 企业 M365 都能登）。该 ID 由 Microsoft 维护，**可能被调整/限流/要求额外同意，
风险自负**；它不属于 NeoSonia，我们不能修改授权页显示名。

- **想显示自己的应用名**：自建 Azure 应用并把 `mail.oauth.client_id` 改成自己的 Application (client) ID；
- **Google**：没有可稳妥复用的公开桌面客户端 ID，必须自建；`client_id` + `client_secret` 都填；
- Thunderbird 的 client_id 已彻底移除，不再伪装成 Thunderbird。

### 可选：自建 Azure 应用（显示自己的应用名）

1. <https://portal.azure.com> → Microsoft Entra ID → App registrations → **New registration**；
   Name 填 `NeoSonia`；Supported account types 按需要选。
2. 记下 **Application (client) ID** → 填 `mail.oauth.client_id`；
   `tenant` 填 `common` / `organizations` / `consumers` / 具体租户 ID。
3. Authentication → **Allow public client flows = Yes**（device code 必需；无需 redirect URI）。
4. API permissions → **Office 365 Exchange Online** → Delegated：
   `IMAP.AccessAsUser.All`、`SMTP.Send`（`offline_access`、`openid profile email` 默认有）。
5. 企业/学校租户若限制用户同意，让管理员 **Grant admin consent**。
6. device code **默认 15 分钟内有效，建议 400 秒内完成**，超时重新点登录即可。
7. Azure public client **不需要 client_secret**（`client_secret` 留空）。

### Google（Gmail，必须自建）

1. <https://console.cloud.google.com/apis/credentials> → Create credentials → **OAuth client ID**；
2. Application type 选 **Desktop app**（不要选 Web application）；
3. 记下 **client id** 和 **client secret** → 分别填 `mail.oauth.client_id` / `client_secret`；
   Google 桌面客户端换 token 时**需要 client_secret**。
4. OAuth consent screen 添加 scope `https://mail.google.com/`（`openid email profile` 一并加上）；
5. Desktop app 允许 loopback 任意端口，不需要手填 redirect URI（程序用 `http://127.0.0.1:<随机端口>`）。

> Google 的 access_token 不是 JWT，且 `mailledger-oauth` 的 `Token` 不含 id_token，
> 所以授权后**无法自动拿到邮箱地址**：请先在设置页填写邮箱地址（账号/发件人），
> 否则 `apply_oauth_result` 会提示“请先填写邮箱地址”，但 host 等参数仍会回填。
> Microsoft 会先尝试从 access_token JWT 的 `upn/unique_name` 兜底。

## OAuth2 配置（`settings.toml` 的 `[mail.oauth]`，字段由 core 定义）

```toml
[mail.oauth]
enabled = false                 # true = 用 OAuth2 替代密码登录
provider = "microsoft"          # microsoft / google / custom
client_id = ""                  # Microsoft 留空 = Microsoft Office 公共客户端；Google 必填；填了就是自己的 Azure 应用
client_secret = ""              # 可选：Microsoft public client 留空；Google Desktop app 需要
tenant = "common"               # 仅 Microsoft：common / organizations / consumers / 租户 ID
email = ""                      # 授权后回写的邮箱地址（拿不到时请手动填）
refresh_token = ""              # 登录成功后由程序写入，不要手填
access_token = ""               # 缓存，可留空
expires_at_ms = 0               # access_token 过期时间（Unix 毫秒）
auth_url = ""                   # Custom 必填；Microsoft 时是 devicecode 端点覆盖
token_url = ""                  # Custom 必填；留空用内置端点
scope = ""                      # 留空用默认 scope
allow_insecure_tls = false      # 仅排障：HTTPS 证书失败时降级重试一次（危险，默认 false）
```

- Microsoft：`client_id` 留空 → 自动用 `d3590ed6-52b3-4102-aeff-aad2292ab01c`（显示 Microsoft Office）；
  填自己的 Azure client id → 显示自己的应用名。
- Google / Custom：`client_id` 为空时点登录会报：
  > 未配置 OAuth2 client_id：Google 请先在设置页「高级」里填写自建「桌面应用」OAuth 客户端的
  > client_id（和 client_secret）；Custom 请填写自己的 OAuth 客户端 ID

`enabled = true` 后 `MailService` 在 IMAP/SMTP 连接时自动取 token（剩余不足 5 分钟自动刷新），
IMAP 用 `AUTHENTICATE XOAUTH2`，SMTP 用 `Mechanism::Xoauth2`。
`imap.user` / `smtp.user` / `smtp.from` 留空时会回退到 `oauth.email`。

## 推荐配置 A：Outlook / Microsoft 365（device code）

```toml
[mail]
enabled = true
provider = "outlook"

[mail.oauth]
enabled = true
provider = "microsoft"
client_id = ""                  # 留空 = Microsoft Office 公共客户端（显示 Microsoft Office）；填自己的 Azure client id 显示自己的应用名
client_secret = ""
tenant = "common"
email = "you@outlook.com"
refresh_token = ""
access_token = ""
expires_at_ms = 0

[mail.imap]
host = "outlook.office365.com"
port = 993
tls = true

[mail.smtp]
host = "smtp.office365.com"
port = 587
implicit_tls = false
starttls = true
from_name = "NeoSonia"
```

## 推荐配置 B：Gmail（loopback + PKCE）

```toml
[mail.oauth]
enabled = true
provider = "google"
client_id = "<你的 Google Cloud Desktop app client id>"
client_secret = "<对应的 client secret>"
tenant = ""
email = "you@gmail.com"

[mail.imap]
host = "imap.gmail.com"
port = 993
tls = true

[mail.smtp]
host = "smtp.gmail.com"
port = 587
implicit_tls = false
starttls = true
```

## 服务商预设（`neosonia_mail::presets()`）

设置页「邮箱类型」下拉直接用 `presets()`，选中后 `apply_preset()` 自动填 host/port/加密方式
（不动账号/密码）：

| id | 中文名 | IMAP | SMTP | 登录方式 |
| --- | --- | --- | --- | --- |
| `outlook` | Outlook / Microsoft 365 | `outlook.office365.com:993` 直连 TLS | `smtp.office365.com:587` STARTTLS | OAuth2（推荐）或密码 |
| `gmail` | Gmail | `imap.gmail.com:993` 直连 TLS | `smtp.gmail.com:587` STARTTLS | OAuth2（推荐）或应用专用密码 |
| `163` | 网易 163 | `imap.163.com:993` 直连 TLS | `smtp.163.com:465` 直连 TLS | 授权码 |
| `126` | 网易 126 | `imap.126.com:993` 直连 TLS | `smtp.126.com:465` 直连 TLS | 授权码 |
| `qq` | QQ 邮箱 | `imap.qq.com:993` 直连 TLS | `smtp.qq.com:465` 直连 TLS | 授权码（16 位，不是 QQ 密码） |
| `custom` | 自定义 / 企业 / 自建 | 自己填 | 自己填 | 密码或自定义 OAuth2 |

163/126/QQ 必须在网页邮箱开启 IMAP/SMTP 并使用**授权码**；163/126 还要求客户端登录后发送
IMAP `ID`：本程序会在 993 直连 TLS 下自动发送
`ID ("name" "NeoSonia" "version" "..." "vendor" "NeoSonia")`。若报 `Unsafe Login`，
请确认端口 993、`tls=true`（不要用 143/STARTTLS）。

## app 侧调用（bridge / 设置页）

```rust
use neosonia_mail::{
    apply_oauth_result, apply_preset, clear_oauth_authorization, migrate_settings, presets,
    test_connection, OAuth, OAuthProvider, MailService,
};

// 0) 启动/热更新后修复旧配置（幂等）：有 token 无 host、mail.enabled=false、
//    旧 Thunderbird client_id、Microsoft scope 缺 offline_access、oauth.email 与 imap.user 不一致。
if let Some(note) = migrate_settings(&mut settings) {
    save_settings(&settings);   // app 负责落盘并重启 poller
}

// 1) 点「用 Microsoft 登录」/「用 Google 登录」：必须用 settings 里的完整配置
let flow = OAuth::start_flow_with_settings(&settings.mail.oauth).await?;
//    flow.authorize_url 用 open::that 打开
//    flow.user_code   显示给用户（Microsoft；15 分钟内有效）
//    flow.redirect_uri 是 Google/Custom 的 loopback 地址
let result = flow.wait.await??;   // OAuthResult

// 2) 写回 settings：自动填 oauth.* + imap/smtp host/port/tls + 用户名/发件人 + mail.enabled=true。
//    换账号（result.email != 旧 oauth.email）会先清旧凭证，再强制把 imap.user/smtp.user/from 换成新邮箱；
//    失败（缺 refresh_token）不会修改任何旧配置。
let note = apply_oauth_result(&mut settings, &result)?;
save_settings(&settings);

// 3) 测连接（IMAP 登录+未读数 / SMTP EHLO/AUTH；OAuth 状态里带 aud/scp/needs_reauth）
let report = test_connection(&settings).await;
if report.all_ok() { /* 成功 */ } else { /* report.imap/smtp.message, report.oauth.status_message */ }

// 4) 「退出登录（清除授权）」按钮：只清 oauth 凭证，保留 imap/smtp host/port，可继续用密码/授权码
let note = clear_oauth_authorization(&mut settings);
save_settings(&settings);
```

- `OAuthFlow.wait` 是 `JoinHandle<anyhow::Result<OAuthResult>>`，app 要 `.await??`；
- `expires_at_ms` 是 **Unix 毫秒**；`refresh_token` 必须写回并保存；
- `MailService::oauth_status()` 返回 `OAuthStatus`：
  `audience/scopes/has_imap_scope/has_smtp_scope/needs_reauth/status_message`（不含 token 明文）；
  `needs_reauth=true` 时 UI 显示「重新授权」；
- poller 检测到 `needs_reauth` 时会清空 access_token（保留 refresh_token）并退避 10 分钟，
  app 应在事件后保存 settings。

## mailledger-oauth 集成说明（v0.6.5）

- 交互流程：**Microsoft device code 由本 crate 实现**（devicecode/轮询请求体可控，保证 scope 带
  `offline_access`、token 响应保留 refresh_token）；Google/Custom 用 `AuthorizationCodeFlow` + PKCE。
  （v0.6.5 起 mailledger 的 DeviceFlow 不再用于 Microsoft，只保留其 Google PKCE、refresh、SASL；
  Google/Custom 的 `AuthorizationCodeFlow::with_pkce()` 仍由库提供，loopback 回调服务器由本 crate 提供。）
- token 刷新：`OAuthClient::refresh_token`（同步 API 内部用独立线程 runtime 调库）；
  失败且 `allow_insecure_tls=true` 时降级到本 crate 的 reqwest blocking 重试一次。
- SASL：`build_xoauth2()` 内部调用 `sasl::xoauth2_response` 并解码回原始字符串
  （imap/lettre 会自己做 base64），单测覆盖与库输出一致。
- 端点/scope 全部来自 settings（`Provider::new` + `with_device_auth_url` + `with_default_scopes`），
  不写死 tenant/scope。
- 限制：库的 `Token` 不含 `id_token`，所以邮箱按 `oauth.email → access_token JWT(upn/unique_name/
  email/preferred_username)` 兜底；仍为空时 `apply_oauth_result` 不阻断（host 照填），
  状态提示“请先在设置里填写邮箱地址”。

## OAuth 授权后仍不可用？排查顺序

1. `settings.toml` 的 `[mail] enabled`、`[mail.imap].host`、`[mail.smtp].host` 是否为空
   （v0.5 真实故障：有 token 无 host、enabled=false；用 `migrate_settings` 修复）。
2. 点「测试连接」：IMAP/SMTP 分别给结果；OAuth 状态会显示 aud/scp/缺哪个权限/是否需重新授权。
3. token 过期会自动刷新；旧 token 认证失败会强制刷新后重试一次；仍失败通常是 refresh_token
   失效（改密码/管理员撤销/长期未用）→ 重新授权。
4. scope 不对：Microsoft 需要 `IMAP.AccessAsUser.All` + `SMTP.Send` + `offline_access`。
5. 端口/加密：Outlook IMAP 993 直连 TLS、SMTP 587 STARTTLS；993 失败不会回退 STARTTLS。
6. 企业租户可能禁用 IMAP/SMTP OAuth 或要求管理员同意。

## 常见错误

| 现象/错误 | 原因 | 处理 |
| --- | --- | --- |
| 提示 `未配置 OAuth2 client_id` | 该 provider 没有默认公开客户端（Google/Custom） | Google 自建 Desktop app 填 client_id+client_secret；Custom 填自己的 client_id |
| 授权页显示 Microsoft Office | Microsoft 默认使用 Office 公共 client_id | 想显示自己的应用名：自建 Azure 应用并填 `mail.oauth.client_id`；公共 ID 被 Microsoft 调整/限流的风险自负 |
| `AADSTS700016 application ... not found` | client_id / tenant 填错 | 检查 `mail.oauth.client_id` / `tenant` |
| `AADSTS65001 consent required` / `AADSTS500011` | 账号/管理员未同意 scope | 换 `common` 登录、管理员 Grant admin consent、补权限 |
| `AADSTS7000218 ... client_secret` | 应用配成了机密客户端 | Azure → Authentication 允许 public client flows |
| `invalid_grant` | refresh_token 失效 | 重新点登录 |
| `expired_token` | device code 过期（默认 15 分钟） | 重新生成 user_code |
| Google `redirect_uri_mismatch` | OAuth client 建成了 Web 类型 | 重建为 Desktop app |
| Google `invalid_client` / 换 token 报缺 secret | Desktop app 需要 client_secret | 填 `mail.oauth.client_secret` |
| `User is authenticated but not connected` | token 缺 IMAP 权限/受众不对 | 重新授权；检查 scope 与 tenant |
| 连接超时/网络错误 | 访问 `login.microsoftonline.com` 等需要代理 | 配系统代理后重试 |
| QQ/163 登录失败 | 它们不支持 OAuth2 | 用授权码密码方式 |

## OAuth HTTPS 栈 / allow_insecure_tls

- 库内部用 reqwest（native-tls 特性统一后 Windows 走 SChannel，使用系统根证书/代理）；
  另保留本 crate 的 reqwest blocking 作为降级路径。
- `allow_insecure_tls = true` 仅在 HTTPS 证书失败时降级 `danger_accept_invalid_certs` 重试一次，
  并 `tracing::warn!`；默认 false，只建议 Kaspersky 等 MITM 环境排障时临时打开。

## IMAP 连接方式

`tls=true` 时只有 **143 端口**走 STARTTLS；其它端口（993 等）**只走直连 TLS，不回退 STARTTLS**；
`tls=false` 只走明文。三种模式错误分别标注，不会混入 `10060` 误导信息。

## 发信附件（v0.6.2）

`mail_send` 可选参数 `attachments: [绝对路径...]`：最多 10 个、单个 ≤25MB、合计 ≤40MB；
邮件组装为 `multipart/mixed`（正文 + 附件），MIME 按扩展名推断，中文文件名按 RFC2231 编码。
无附件时保持单段 `text/plain`。

## 白名单备注（v0.6.2）

`settings.mail.whitelist_notes: BTreeMap<String, String>`（地址→备注，支持 `@domain`）。
`mail_list` / `mail_read` / `mail_wait_for` 会在发件人后附「白名单备注：…」。

## 安全提示

- `refresh_token`、`access_token`、`client_secret` 只写本地 `settings.toml`（或 `env:` 引用），
  不要打进日志；本 crate 日志只记状态不记 token。
- 不要长期开启 `allow_insecure_tls`。
- 建议在 Azure/Google 里限制应用权限，定期清理未使用的 OAuth 客户端。
