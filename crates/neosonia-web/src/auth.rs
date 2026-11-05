//! 鉴权中间件。
//!
//! - [`lan_guard`]：`remote.lan_only = true` 时只放行私有网段/回环/链路本地来源；
//! - [`require_device`]：`/api/*` 必须带 `Authorization: Bearer <token>`，
//!   校验 `sha256(token)` 命中设备表后把设备信息塞进请求扩展
//!   （handler 用 `Extension<AuthDevice>` 取）。
//!
//! WebSocket 不能给浏览器加 Authorization 头，所以 `/ws?token=` 在 handler 里单独
//! 校验（见 [`crate::ws`]）。**任何日志都不打印 token 原文。**

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use neosonia_core::TrustedDevice;

use crate::server::Inner;

/// 已鉴权设备（放进 request extensions）。
#[derive(Clone, Debug)]
pub struct AuthDevice(pub TrustedDevice);

/// 拒绝响应：JSON `{"error": "..."}`。
pub fn reject(status: StatusCode, message: &str) -> Response {
    (status, axum::Json(serde_json::json!({ "error": message }))).into_response()
}

/// 从请求扩展里取 peer 地址（需要 `axum::serve(...into_make_service_with_connect_info())`）。
pub fn peer_addr(req: &Request) -> Option<SocketAddr> {
    req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0)
}

/// 从 `Authorization: Bearer xxx` 里取 token。
pub fn bearer_token(req: &Request) -> Option<String> {
    let raw = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())?
        .trim();
    let (scheme, token) = raw.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    if token.is_empty() { None } else { Some(token.to_string()) }
}

/// 局域网来源校验（最外层）。
pub async fn lan_guard(State(inner): State<Arc<Inner>>, req: Request, next: Next) -> Response {
    let lan_only = inner.settings.read().remote.lan_only;
    if !lan_only {
        return next.run(req).await;
    }
    match peer_addr(&req) {
        Some(addr) if crate::net::is_lan_ip(addr.ip()) => next.run(req).await,
        Some(addr) => {
            tracing::warn!(peer = %addr.ip(), "拒绝非局域网来源的远程访问请求");
            reject(
                StatusCode::FORBIDDEN,
                "只接受局域网内的设备连接（remote.lan_only = true）。",
            )
        }
        None => reject(StatusCode::FORBIDDEN, "无法确定来源地址，已拒绝。"),
    }
}

/// 设备 token 校验。
pub async fn require_device(
    State(inner): State<Arc<Inner>>,
    mut req: Request,
    next: Next,
) -> Response {
    let token = bearer_token(&req);
    let addr = peer_addr(&req).map(|a| a.ip().to_string());
    let device = token
        .as_deref()
        .and_then(|t| inner.devices.authenticate(t, addr.as_deref()));
    match device {
        Some(device) => {
            req.extensions_mut().insert(AuthDevice(device));
            next.run(req).await
        }
        None => reject(
            StatusCode::UNAUTHORIZED,
            "未授权：请先用配对码配对这台设备（token 缺失、无效或已被撤销）。",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request as HttpRequest;

    fn request(headers: &[(&str, &str)]) -> Request {
        let mut builder = HttpRequest::builder().uri("/api/me");
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        builder.body(axum::body::Body::empty()).expect("构造请求")
    }

    #[test]
    fn bearer_parsing() {
        let req = request(&[("authorization", "Bearer abc123")]);
        assert_eq!(bearer_token(&req).as_deref(), Some("abc123"));

        let req = request(&[("authorization", "bearer   xyz  ")]);
        assert_eq!(bearer_token(&req).as_deref(), Some("xyz"));

        let req = request(&[("authorization", "Basic abc")]);
        assert!(bearer_token(&req).is_none());

        let req = request(&[("authorization", "Bearer ")]);
        assert!(bearer_token(&req).is_none());

        let req = request(&[]);
        assert!(bearer_token(&req).is_none());
    }

    #[test]
    fn reject_shape_is_json_error() {
        let res = reject(StatusCode::UNAUTHORIZED, "未授权");
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }
}
