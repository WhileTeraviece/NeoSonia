//! MCP 服务器 / 工具命名规则与配置过滤。
//!
//! 模型看到的工具名必须符合函数名约束（字母、数字、下划线、连字符，最多 64 字符），
//! 而 MCP 服务器里的工具名可能带空格、点号甚至中文，所以统一在这里清洗。

use std::collections::BTreeSet;

use neosonia_core::{McpServerConfig, McpSettings, McpTransport};

/// 工具名长度上限（OpenAI / DeepSeek 的函数名上限是 64 个字符）。
pub(crate) const MAX_TOOL_NAME_LEN: usize = 64;

/// 保留 ASCII 字母数字、`-`、`_`，其余字符替换成 `_`，连续 `_` 合并成一个。
///
/// 不修剪首尾下划线：默认前缀 `mcp_demo_` 结尾的 `_` 必须保住，
/// 否则拼出来的工具名会粘连（`mcp_demoecho`）。
pub(crate) fn sanitize_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut previous_underscore = false;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' {
            out.push(ch);
            previous_underscore = false;
        } else if !previous_underscore {
            out.push('_');
            previous_underscore = true;
        }
    }
    out
}

/// 远端工具名清洗：在 [`sanitize_name`] 基础上去掉首尾下划线；清洗后为空则用 `tool`。
pub(crate) fn sanitize_tool_segment(raw: &str) -> String {
    let cleaned = sanitize_name(raw.trim());
    let trimmed = cleaned.trim_matches('_');
    if trimmed.is_empty() {
        "tool".to_string()
    } else {
        trimmed.to_string()
    }
}

/// 默认前缀：`mcp_<服务器名>_`。
pub(crate) fn default_prefix(server_name: &str) -> String {
    format!("mcp_{}_", sanitize_name(server_name.trim()))
}

/// 实际前缀：配置了 `tool_prefix` 就用它（同样清洗），否则用默认前缀。
pub(crate) fn resolve_prefix(config: &McpServerConfig) -> String {
    let custom = config.tool_prefix.trim();
    if custom.is_empty() {
        default_prefix(&config.name)
    } else {
        sanitize_name(custom)
    }
}

/// 完整工具名 = 前缀 + 清理后的远端名；超过 64 字符时按字节截断
/// （清洗后只剩 ASCII，按字节截断是安全的）。
pub(crate) fn full_tool_name(prefix: &str, remote_name: &str) -> String {
    let mut name = format!("{prefix}{}", sanitize_tool_segment(remote_name));
    if name.len() > MAX_TOOL_NAME_LEN {
        name.truncate(MAX_TOOL_NAME_LEN);
    }
    if name.is_empty() {
        "tool".to_string()
    } else {
        name
    }
}

/// 单台服务器配置的结构性检查；返回中文原因（`None` 表示可以用）。
pub(crate) fn validate_config(config: &McpServerConfig) -> Option<String> {
    let name = config.name.trim();
    if name.is_empty() {
        return Some("服务器名称为空。".to_string());
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Some(format!("服务器名称「{name}」只能包含字母、数字、下划线。"));
    }
    match config.transport {
        McpTransport::Stdio => {
            if config.command.trim().is_empty() {
                return Some(format!(
                    "服务器「{name}」没有配置 command（stdio 传输需要可执行文件）。"
                ));
            }
        }
        McpTransport::Http => {
            let url = config.url.trim();
            if url.is_empty() {
                return Some(format!("服务器「{name}」没有配置 url（http 传输需要地址）。"));
            }
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return Some(format!(
                    "服务器「{name}」的 url 必须以 http:// 或 https:// 开头：{url}"
                ));
            }
        }
    }
    None
}

/// 启动计划：可以启动 / 配置有问题 / 名称重复。
#[derive(Clone, Debug)]
pub(crate) enum ConfigPlan {
    /// 结构性合法，要真正去连。
    Start(McpServerConfig),
    /// 不启动，但要在 `servers()` 里显示原因（禁用、缺 command、url 非法……）。
    Broken { config: McpServerConfig, error: String },
    /// 名称与前面的条目重复，忽略（只在日志里 warn）。
    Duplicate { name: String },
}

/// 把配置切成启动计划。
///
/// 保留所有条目（便于 UI / `mcp_list_servers` 解释为什么某台没起来），
/// 只对结构性合法的条目返回 [`ConfigPlan::Start`]。
pub(crate) fn plan_configs(mcp: &McpSettings) -> Vec<ConfigPlan> {
    let mut plans = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for config in &mcp.servers {
        let name = config.name.trim().to_string();
        if name.is_empty() {
            let mut config = config.clone();
            config.name = String::new();
            plans.push(ConfigPlan::Broken {
                config,
                error: "服务器名称为空，已跳过。".to_string(),
            });
            continue;
        }
        if !seen.insert(name.clone()) {
            plans.push(ConfigPlan::Duplicate { name });
            continue;
        }
        let mut config = config.clone();
        config.name = name.clone();
        if !config.enabled {
            plans.push(ConfigPlan::Broken {
                config,
                error: "已在配置中禁用（enabled = false）。".to_string(),
            });
            continue;
        }
        match validate_config(&config) {
            None => plans.push(ConfigPlan::Start(config)),
            Some(error) => plans.push(ConfigPlan::Broken { config, error }),
        }
    }
    plans
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_weird_names() {
        assert_eq!(sanitize_name("demo"), "demo");
        assert_eq!(sanitize_name("mcp_demo_"), "mcp_demo_");
        assert_eq!(sanitize_name("my server!"), "my_server_");
        assert_eq!(sanitize_name("a  b"), "a_b");
        assert_eq!(sanitize_name("中文"), "_");
    }

    #[test]
    fn sanitize_tool_segment_trims_and_falls_back() {
        assert_eq!(sanitize_tool_segment("my tool!"), "my_tool");
        assert_eq!(sanitize_tool_segment("  read.file  "), "read_file");
        assert_eq!(sanitize_tool_segment("中文"), "tool");
        assert_eq!(sanitize_tool_segment("___"), "tool");
        assert_eq!(sanitize_tool_segment("keep-dashes_ok"), "keep-dashes_ok");
    }

    #[test]
    fn default_and_custom_prefix() {
        let mut config = McpServerConfig::default();
        config.name = "demo".into();
        assert_eq!(resolve_prefix(&config), "mcp_demo_");

        config.tool_prefix = "gh_".into();
        assert_eq!(resolve_prefix(&config), "gh_");

        config.tool_prefix = "  ".into();
        assert_eq!(resolve_prefix(&config), "mcp_demo_");
    }

    #[test]
    fn full_name_is_capped_and_readable() {
        assert_eq!(full_tool_name("mcp_demo_", "add"), "mcp_demo_add");
        let long = "x".repeat(200);
        let name = full_tool_name("mcp_demo_", &long);
        assert_eq!(name.len(), MAX_TOOL_NAME_LEN);
        assert!(name.starts_with("mcp_demo_"));
    }

    #[test]
    fn plan_filters_disabled_and_invalid() {
        let mut mcp = McpSettings::default();
        mcp.enabled = true;

        let mut ok = McpServerConfig::default();
        ok.name = " demo ".into();
        ok.command = "node".into();

        let mut disabled = McpServerConfig::default();
        disabled.name = "off".into();
        disabled.enabled = false;
        disabled.command = "node".into();

        let mut bad_url = McpServerConfig::default();
        bad_url.name = "remote".into();
        bad_url.transport = McpTransport::Http;
        bad_url.url = "ftp://example.com/mcp".into();

        let duplicate = ok.clone();

        let mut bad_name = McpServerConfig::default();
        bad_name.name = "bad name".into();
        bad_name.command = "node".into();

        let mut no_command = McpServerConfig::default();
        no_command.name = "empty".into();

        mcp.servers = vec![ok, disabled, bad_url, duplicate, bad_name, no_command];
        let plans = plan_configs(&mcp);
        assert_eq!(plans.len(), 6);

        let startable: Vec<String> = plans
            .iter()
            .filter_map(|plan| match plan {
                ConfigPlan::Start(config) => Some(config.name.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(startable, vec!["demo".to_string()], "只有 demo 应该被启动");

        assert!(plans.iter().any(|plan| matches!(
            plan,
            ConfigPlan::Broken { config, error }
                if config.name == "off" && error.contains("禁用")
        )));
        assert!(plans.iter().any(|plan| matches!(
            plan,
            ConfigPlan::Broken { config, error }
                if config.name == "remote" && error.contains("http")
        )));
        assert!(plans.iter().any(|plan| matches!(
            plan,
            ConfigPlan::Broken { config, error }
                if config.name == "bad name" && error.contains("下划线")
        )));
        assert!(plans.iter().any(|plan| matches!(
            plan,
            ConfigPlan::Broken { config, error }
                if config.name == "empty" && error.contains("command")
        )));
        assert!(plans
            .iter()
            .any(|plan| matches!(plan, ConfigPlan::Duplicate { name } if name == "demo")));

        // 名称已去掉首尾空白，工具前缀不会带空格
        let demo = plans
            .iter()
            .find_map(|plan| match plan {
                ConfigPlan::Start(config) if config.name == "demo" => Some(config),
                _ => None,
            })
            .expect("demo 应该可启动");
        assert_eq!(resolve_prefix(demo), "mcp_demo_");
    }

    #[test]
    fn plan_keeps_empty_name_as_broken() {
        let mut mcp = McpSettings::default();
        let mut config = McpServerConfig::default();
        config.name = "   ".into();
        config.command = "node".into();
        mcp.servers = vec![config];
        let plans = plan_configs(&mcp);
        assert_eq!(plans.len(), 1);
        assert!(matches!(
            &plans[0],
            ConfigPlan::Broken { error, .. } if error.contains("名称为空")
        ));
    }
}
