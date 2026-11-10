//! Reject requests that a web page made, so an open browser tab cannot spend the
//! user's credentials through the local proxy.
//!
//! The proxy swaps whatever credentials a request carries for the stored provider's,
//! so anything that can reach the port gets to use them. CLI clients never send
//! `Origin`, while browsers attach it to every POST (including `no-cors` ones that
//! skip preflight) and to every WebSocket handshake, so rejecting it stops a page from
//! calling the proxy cross-site. A DNS-rebinding page is same-origin and can read
//! GET responses, but it has to reach us under its own domain name, so on a loopback
//! listener only hosts no public DNS can point at are accepted.
//!
//! Only headers are read: the request itself passes through untouched.

use super::server::ProxyState;
use axum::{
    extract::{Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use std::net::IpAddr;

/// Claude Desktop's namespace already requires its gateway token, which a page
/// cannot know, so the desktop app is not held to the header rules.
const TOKEN_PROTECTED_PREFIX: &str = "/claude-desktop/";

pub(super) async fn reject_browser_requests(
    State(state): State<ProxyState>,
    request: Request,
    next: Next,
) -> Response {
    let listen_address = state.config.read().await.listen_address.clone();
    let client_host = crate::settings::get_proxy_client_host();
    match check_request(
        request.uri().path(),
        request.headers(),
        &listen_address,
        client_host.as_deref(),
    ) {
        Ok(()) => next.run(request).await,
        Err(reason) => {
            log::warn!(
                "[ProxyServer] rejected {} {}: {reason}",
                request.method(),
                request.uri().path()
            );
            (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "error": {
                        "type": "forbidden",
                        "message": format!("CC Switch local proxy: {reason}"),
                    }
                })),
            )
                .into_response()
        }
    }
}

fn check_request(
    path: &str,
    headers: &HeaderMap,
    listen_address: &str,
    client_host: Option<&str>,
) -> Result<(), &'static str> {
    if !path.starts_with(TOKEN_PROTECTED_PREFIX) && headers.contains_key(header::ORIGIN) {
        return Err("requests from web pages are not accepted");
    }
    if should_enforce_host_header(listen_address) {
        if let Some(host) = headers.get(header::HOST) {
            let host = host.to_str().ok().and_then(host_name);
            if !host.is_some_and(|host| is_local_host(&host, listen_address, client_host)) {
                return Err("unexpected Host header");
            }
        }
    }
    Ok(())
}

/// 监听回环或通配网卡（`0.0.0.0` / `::`）时都必须校验 Host 头，
/// 防止本机浏览器通过 DNS rebinding（如 `evil.example -> 127.0.0.1`）绕过同源限制。
/// 仅当显式绑定在独立非回环网卡（如 `192.168.1.5`）时放行该局域网接入主机名。
fn should_enforce_host_header(listen_address: &str) -> bool {
    crate::services::proxy::is_loopback_host(listen_address)
}

/// The host part of a Host header, lowercased, without port, brackets or trailing dot.
fn host_name(value: &str) -> Option<String> {
    let authority: http::uri::Authority = value.trim().parse().ok()?;
    let host = authority
        .host()
        .trim_matches(['[', ']'])
        .trim_end_matches('.');
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

fn normalize_host(value: &str) -> String {
    value
        .trim()
        .trim_matches(['[', ']'])
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

/// Names a rebinding page cannot use: IP literals (rebinding needs a domain),
/// `localhost` and the reserved/private-use `.localhost` / `.local` / `.internal` / `.ts.net`
/// suffixes (Tailscale MagicDNS is allowed even when `client_host` is configured as a `100.x` IP),
/// plus the explicit `client_host` or hostname listener the user configured.
fn is_local_host(host: &str, listen_address: &str, client_host: Option<&str>) -> bool {
    host.parse::<IpAddr>().is_ok()
        || host == "localhost"
        || [".localhost", ".local", ".internal", ".ts.net"]
            .iter()
            .any(|suffix| host.len() > suffix.len() && host.ends_with(suffix))
        || client_host.is_some_and(|client| normalize_host(client) == host)
        || (!crate::services::proxy::is_loopback_host(listen_address)
            && normalize_host(listen_address) == host)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, value.parse().unwrap());
        }
        map
    }

    #[test]
    fn cli_requests_pass() {
        for host in ["127.0.0.1:15721", "localhost:15721", "[::1]:15721"] {
            let h = headers(&[("host", host)]);
            assert_eq!(check_request("/v1/messages", &h, "127.0.0.1", None), Ok(()));
        }
        // No Host at all (HTTP/1.0 clients) is not a browser.
        assert_eq!(
            check_request("/v1/messages", &HeaderMap::new(), "127.0.0.1", None),
            Ok(())
        );
    }

    #[test]
    fn any_origin_is_rejected_outside_claude_desktop() {
        for origin in ["https://evil.example", "null", "tauri://localhost"] {
            let h = headers(&[("host", "127.0.0.1:15721"), ("origin", origin)]);
            for path in ["/v1/messages", "/v1/responses", "/v1beta/models", "/status"] {
                assert!(check_request(path, &h, "127.0.0.1", None).is_err());
            }
            // LAN listeners are covered too: the Origin rule does not depend on Host.
            assert!(check_request("/v1/messages", &h, "0.0.0.0", None).is_err());
        }
    }

    #[test]
    fn claude_desktop_keeps_its_token_check_only() {
        let h = headers(&[("host", "127.0.0.1:15721"), ("origin", "app://claude")]);
        assert_eq!(
            check_request("/claude-desktop/v1/messages", &h, "127.0.0.1", None),
            Ok(())
        );
        // Rebinding still applies there: the Host rule is not exempted.
        let h = headers(&[("host", "rebind.evil.example:15721")]);
        assert!(check_request("/claude-desktop/v1/models", &h, "127.0.0.1", None).is_err());
    }

    #[test]
    fn rebinding_hosts_are_rejected_on_loopback_listener() {
        for host in [
            "rebind.evil.example:15721",
            "evil.example.",
            "localhost.evil.example",
        ] {
            let h = headers(&[("host", host)]);
            assert!(
                check_request("/status", &h, "127.0.0.1", None).is_err(),
                "{host}"
            );
            assert!(check_request("/status", &h, "::1", None).is_err(), "{host}");
            assert!(
                check_request("/status", &h, "[::ffff:127.0.0.1]", None).is_err(),
                "{host}"
            );
        }
    }

    #[test]
    fn local_names_and_configured_client_host_pass() {
        for host in [
            "host.docker.internal:15721",
            "my-mac.local:15721",
            "app.localhost:15721",
            "LOCALHOST.:15721",
            "172.25.144.1:15721",
        ] {
            let h = headers(&[("host", host)]);
            assert_eq!(
                check_request("/v1/messages", &h, "127.0.0.1", None),
                Ok(()),
                "{host}"
            );
        }
        let h = headers(&[("host", "Devbox.Example.com:15721")]);
        assert_eq!(
            check_request(
                "/v1/messages",
                &h,
                "127.0.0.1",
                Some(" devbox.example.com ")
            ),
            Ok(())
        );
    }

    #[test]
    fn lan_listener_accepts_any_host_name() {
        let h = headers(&[("host", "devbox.corp.example:15721")]);
        assert_eq!(
            check_request("/v1/messages", &h, "192.168.1.5", None),
            Ok(())
        );
    }

    #[test]
    fn wildcard_listener_enforces_host_guard_and_allows_tailnet() {
        for listen in ["0.0.0.0", "::", "[::]"] {
            for rejected in [
                "rebind.evil.example:15721",
                "devbox.corp.example:15721",
                "evil.example.:15721",
                "ts.net:15721",
                "evilts.net:15721",
                "foo.ts.net.evil.example:15721",
            ] {
                let h = headers(&[("host", rejected)]);
                assert!(
                    check_request("/v1/messages", &h, listen, None).is_err(),
                    "{listen} should reject {rejected}"
                );
            }

            for allowed in [
                "127.0.0.1:15721",
                "100.64.0.1:15721",
                "my-node.tail123.ts.net:15721",
                "host.docker.internal:15721",
                "my-mac.local:15721",
            ] {
                let h = headers(&[("host", allowed)]);
                assert_eq!(
                    check_request("/v1/messages", &h, listen, Some("100.64.0.1")),
                    Ok(()),
                    "{listen} should allow {allowed}"
                );
            }

            let h = headers(&[("host", "custom.tailnet.example:15721")]);
            assert_eq!(
                check_request("/v1/messages", &h, listen, Some("custom.tailnet.example")),
                Ok(()),
                "{listen} should allow configured client_host"
            );
        }
    }
}
