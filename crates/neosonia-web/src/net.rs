//! 局域网地址判定与展示用 URL。
//!
//! 安全策略：`remote.lan_only = true`（默认）时，只允许来自私有网段/回环/链路
//! 本地的连接；公网来源一律 403，避免用户误把 0.0.0.0 暴露到公网后被人白嫖。

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// 判断一个 IP 是否属于"局域网"（RFC1918 私有段 / 回环 / 链路本地 / IPv6 ULA）。
///
/// 覆盖契约要求的：10/8、172.16/12、192.168/16、127/8、169.254/16、
/// fc00::/7、::1；另外把 IPv6 链路本地 fe80::/10 也算作局域网。
pub fn is_lan_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_lan_v4(v4),
        IpAddr::V6(v6) => is_lan_v6(v6),
    }
}

fn is_lan_v4(ip: Ipv4Addr) -> bool {
    if ip.is_loopback() || ip.is_link_local() || ip.is_private() {
        return true;
    }
    // `is_private` 不包含 0.0.0.0/8，这里不放行；也不包含运营商级 NAT 100.64/10，
    // 那个不算"同一局域网"，保持拒绝。
    matches!(ip.octets(), [169, 254, ..])
}

fn is_lan_v6(ip: Ipv6Addr) -> bool {
    if ip.is_loopback() {
        return true;
    }
    // IPv4-mapped（::ffff:192.168.1.2）：按内层 v4 判断。
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_lan_v4(v4);
    }
    let seg = ip.segments();
    // fc00::/7：唯一本地地址。
    if (seg[0] & 0xfe00) == 0xfc00 {
        return true;
    }
    // fe80::/10：链路本地。
    if (seg[0] & 0xffc0) == 0xfe80 {
        return true;
    }
    false
}

/// 本机最可能的局域网 IP；失败回退回环地址（远程端仍可通过 127.0.0.1 本机访问）。
pub fn best_lan_ip() -> IpAddr {
    local_ip_address::local_ip().unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
}

/// 根据监听地址计算对外的展示 URL：
/// - 监听具体 IP → 用它；
/// - 监听回环 → 127.0.0.1；
/// - 监听 `0.0.0.0` / `::` → 本机局域网 IP。
pub fn base_url(bind: IpAddr, port: u16) -> String {
    let ip = if bind.is_unspecified() {
        best_lan_ip()
    } else {
        bind
    };
    format!("http://{}", SocketAddr::new(ip, port))
}

/// 主机名（展示用）。取不到时返回 "NeoSonia 主机"。
pub fn hostname() -> String {
    for key in ["COMPUTERNAME", "HOSTNAME"] {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim().to_string();
            if !v.is_empty() {
                return v;
            }
        }
    }
    "NeoSonia 主机".to_string()
}

/// 规范化对外访问地址：只接受 http(s)://host[:port][/]，去尾斜杠；
/// 拒绝空 host、`0.0.0.0`、`[::]`、`::` 这类不可访问的 host。
///
/// 返回 `Some("http://192.168.1.10:8787")`；非法返回 `None`（调用方回退自动探测）。
pub fn normalize_advertise_url(raw: &str) -> Option<String> {
    let text = raw.trim();
    let lower = text.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return None;
    }
    let scheme_end = text.find("://")?;
    let scheme = &text[..scheme_end];
    let rest = &text[scheme_end + 3..];
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let authority = authority.trim();
    if authority.is_empty() {
        return None;
    }
    // 去掉 userinfo（不允许 URL 里带凭据）
    if authority.contains('@') {
        return None;
    }
    // host 部分（处理 IPv6 的 [..] 写法）
    let host = if authority.starts_with('[') {
        let end = authority.find(']')?;
        &authority[..=end]
    } else {
        authority.split(':').next().unwrap_or("")
    };
    let host_lower = host.to_ascii_lowercase();
    if host_lower.is_empty()
        || host_lower == "0.0.0.0"
        || host_lower == "[::]"
        || host_lower == "::"
    {
        return None;
    }
    // 只保留 scheme://authority（路径固定为 "/"），统一小写 scheme、去尾斜杠。
    Some(format!("{}://{}", scheme.to_ascii_lowercase(), authority))
}

/// 计算最终对外根 URL：
/// - `advertise` 传了合法值 → 用它（app/bridge 传 LAN IP，最可靠）；
/// - 否则根据监听地址推导：`0.0.0.0`/`::` → 本机 LAN IP；具体 IP → 它自己。
pub fn advertise_base(advertise: Option<&str>, bind: IpAddr, port: u16) -> String {
    if let Some(raw) = advertise {
        if let Some(normalized) = normalize_advertise_url(raw) {
            return normalized;
        }
        if !raw.trim().is_empty() {
            tracing::warn!("advertise_url 不合法（{raw}），已回退自动探测本机 LAN IP");
        }
    }
    base_url(bind, port)
}

/// 监听地址选择：`override_bind` 非空优先，否则用 settings 里的 bind；两者都空 → 0.0.0.0。
pub fn choose_bind(override_bind: Option<&str>, settings_bind: &str) -> String {
    let override_text = override_bind.map(str::trim).filter(|s| !s.is_empty());
    let settings_text = settings_bind.trim();
    if let Some(text) = override_text {
        text.to_string()
    } else if !settings_text.is_empty() {
        settings_text.to_string()
    } else {
        "0.0.0.0".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("测试 IP 应可解析")
    }

    #[test]
    fn private_v4_ranges_allowed() {
        for s in [
            "10.0.0.1",
            "10.255.255.254",
            "172.16.0.1",
            "172.31.255.254",
            "192.168.1.10",
            "127.0.0.1",
            "169.254.10.10",
        ] {
            assert!(is_lan_ip(ip(s)), "{s} 应判定为局域网");
        }
    }

    #[test]
    fn public_v4_ranges_rejected() {
        for s in [
            "8.8.8.8",
            "1.1.1.1",
            "172.32.0.1",
            "192.169.0.1",
            "203.0.113.7",
            "100.64.0.1",
        ] {
            assert!(!is_lan_ip(ip(s)), "{s} 不应判定为局域网");
        }
    }

    #[test]
    fn ipv6_rules() {
        for s in ["::1", "fc00::1", "fd12:3456::99", "fe80::1", "::ffff:192.168.1.5", "::ffff:127.0.0.1"]
        {
            assert!(is_lan_ip(ip(s)), "{s} 应判定为局域网");
        }
        for s in ["2001:4860:4860::8888", "240e::1", "::ffff:8.8.8.8"] {
            assert!(!is_lan_ip(ip(s)), "{s} 不应判定为局域网");
        }
    }

    #[test]
    fn base_url_uses_loopback_for_loopback_bind() {
        let url = base_url(IpAddr::V4(Ipv4Addr::LOCALHOST), 8787);
        assert_eq!(url, "http://127.0.0.1:8787");
    }

    #[test]
    fn base_url_formats_ipv6() {
        let url = base_url(ip("::1"), 8787);
        assert_eq!(url, "http://[::1]:8787");
    }

    #[test]
    fn normalize_advertise_url_accepts_lan_and_strips_slash() {
        assert_eq!(
            normalize_advertise_url("http://192.168.1.10:8787/").as_deref(),
            Some("http://192.168.1.10:8787")
        );
        assert_eq!(
            normalize_advertise_url("  HTTP://192.168.1.10:8787  ").as_deref(),
            Some("http://192.168.1.10:8787")
        );
        assert_eq!(
            normalize_advertise_url("https://neosonia.local:8787/path/x").as_deref(),
            Some("https://neosonia.local:8787")
        );
        assert_eq!(
            normalize_advertise_url("http://[fd12:3456::99]:8787").as_deref(),
            Some("http://[fd12:3456::99]:8787")
        );
    }

    #[test]
    fn normalize_advertise_url_rejects_unusable_values() {
        for bad in [
            "",
            "192.168.1.2:8787",
            "ftp://192.168.1.2:8787",
            "http://0.0.0.0:8787",
            "http://[::]:8787",
            "http://:8787",
            "http://user:pass@192.168.1.2:8787",
        ] {
            assert!(
                normalize_advertise_url(bad).is_none(),
                "{bad:?} 不应被接受为访问地址"
            );
        }
    }

    #[test]
    fn advertise_base_prefers_valid_override() {
        let bind = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
        // 传了合法地址（例如 app 探测到的 LAN IP）→ 必须优先用它，而不是 127.0.0.1。
        assert_eq!(
            advertise_base(Some("http://192.168.1.10:8787/"), bind, 8787),
            "http://192.168.1.10:8787"
        );
        // 非法/空 → 回退：绑 0.0.0.0 时探测 LAN IP（测试环境取到什么算什么，断言格式）。
        let fallback = advertise_base(Some("0.0.0.0"), bind, 8787);
        assert!(fallback.starts_with("http://"), "{fallback}");
        assert!(!fallback.contains("0.0.0.0"), "{fallback}");
        // 绑具体回环地址时回退到 127.0.0.1（本机可用，手机不可用——由 app 传 advertise 修复）。
        assert_eq!(
            advertise_base(None, IpAddr::V4(Ipv4Addr::LOCALHOST), 8787),
            "http://127.0.0.1:8787"
        );
    }

    #[test]
    fn choose_bind_override_priority() {
        // 手机连不上的根因：settings 里 bind=127.0.0.1 时，app 传 override 0.0.0.0 必须生效。
        assert_eq!(choose_bind(Some("0.0.0.0"), "127.0.0.1"), "0.0.0.0");
        assert_eq!(choose_bind(Some("  "), "127.0.0.1"), "127.0.0.1");
        assert_eq!(choose_bind(None, "192.168.1.10"), "192.168.1.10");
        assert_eq!(choose_bind(None, "   "), "0.0.0.0");
        assert_eq!(choose_bind(Some("::"), ""), "::");
    }
}

