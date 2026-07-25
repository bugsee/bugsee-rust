//
//  lib.rs
//  bugsee-reqwest
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! reqwest network capture.
//!
//! [`BugseeMiddleware`] plugs into a [`reqwest_middleware`] client and records a
//! `before` entry and a `complete`/`error` entry (sharing one id) per request,
//! into the Bugsee capture window → `network.json`. URLs and headers pass
//! through a default sanitizer that redacts credentials.

use std::time::Instant;

use bugsee::core::model::entry::{NetworkCustom, NetworkEntry};
use bugsee::core::model::enums::NetworkStage;
use bugsee::core::util::{epoch_ms, random_hex};
use bugsee::Bugsee;
use http::Extensions;
use reqwest::{Request, Response};
use reqwest_middleware::{Middleware, Next};
use serde_json::{json, Map, Value};

/// The identifier of the intercepted HTTP stack, recorded on each entry.
const MECHANISM: &str = "reqwest";

/// A reqwest middleware that captures request/response metadata into Bugsee.
#[derive(Default, Clone)]
pub struct BugseeMiddleware;

impl BugseeMiddleware {
    /// Create the middleware.
    pub fn new() -> Self {
        BugseeMiddleware
    }
}

#[async_trait::async_trait]
impl Middleware for BugseeMiddleware {
    async fn handle(
        &self,
        req: Request,
        extensions: &mut Extensions,
        next: Next<'_>,
    ) -> reqwest_middleware::Result<Response> {
        // 64-bit id so before/complete entries don't mispair under collision in
        // a busy capture window.
        let id = random_hex(8);
        let method = req.method().to_string();
        let url = sanitize_url(req.url());

        // Capture the in-memory request body. A streaming body can't be read
        // back as bytes, so `body()`/`as_bytes()` yields `None` there. Own
        // everything derived from `req` here — `body_bytes` borrows `req`, and
        // `next.run` below moves it, so the borrow must end first.
        let body_bytes = req.body().and_then(|b| b.as_bytes());
        let req_size = body_bytes.map(|b| b.len() as i64).unwrap_or(0);
        let content_type = req
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let (req_body, req_no_body_reason) =
            capture_request_body(body_bytes, content_type.as_deref());
        let headers = sanitize_headers(req.headers());

        Bugsee::capture_network(before_entry(
            &id,
            &method,
            &url,
            req_size,
            headers,
            req_body,
            req_no_body_reason,
        ));

        let start = Instant::now();
        let result = next.run(req, extensions).await;
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;

        match &result {
            Ok(resp) => {
                let status = resp.status().as_u16() as i32;
                let size = resp.content_length().unwrap_or(0) as i64;
                Bugsee::capture_network(complete_entry(
                    &id, &method, &url, size, status, elapsed_ms,
                ));
            }
            Err(err) => {
                Bugsee::capture_network(error_entry(&id, &method, &url, err.to_string()));
            }
        }
        result
    }
}

fn base_entry(id: &str, method: &str, url: &str, stage: NetworkStage) -> NetworkEntry {
    NetworkEntry {
        timestamp: epoch_ms(),
        id: id.to_string(),
        mechanism: MECHANISM.to_string(),
        url: url.to_string(),
        method: method.to_string(),
        stage,
        size: 0,
        redirect: false,
        status: 0,
        status_text: None,
        custom_error: None,
        event: None,
        custom: NetworkCustom::default(),
        is_override: false,
    }
}

fn before_entry(
    id: &str,
    method: &str,
    url: &str,
    size: i64,
    headers: Value,
    body: Option<String>,
    no_body_reason: Option<String>,
) -> NetworkEntry {
    let mut e = base_entry(id, method, url, NetworkStage::Before);
    e.size = size;
    e.custom.headers = Some(headers);
    e.custom.body = body;
    e.custom.no_body_reason = no_body_reason;
    e
}

fn complete_entry(
    id: &str,
    method: &str,
    url: &str,
    size: i64,
    status: i32,
    timings_ms: f64,
) -> NetworkEntry {
    let mut e = base_entry(id, method, url, NetworkStage::Complete);
    e.size = size;
    e.status = status;
    e.is_override = true;
    e.custom.timings = Some(json!({ "total": timings_ms }));
    // The response body is intentionally NOT captured: a reqwest `Response`
    // flowing through middleware must be returned to the caller with its body
    // stream intact, and reading it here would consume the stream and break the
    // request. We keep `size` from `content_length()` (when the server sent it)
    // but mark the body honestly as unread rather than implying it was empty.
    e.custom.no_body_reason = Some("cant_read_data".to_string());
    e
}

fn error_entry(id: &str, method: &str, url: &str, error: String) -> NetworkEntry {
    let mut e = base_entry(id, method, url, NetworkStage::Error);
    e.is_override = true;
    e.custom.error = Some(error);
    e
}

/// Query-parameter names whose values are redacted.
fn is_sensitive_param(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    matches!(
        n.as_str(),
        "token"
            | "access_token"
            | "api_key"
            | "apikey"
            | "key"
            | "password"
            | "passwd"
            | "secret"
            | "auth"
            | "signature"
            | "sig"
    )
}

/// Header names whose values are redacted — an explicit list plus a substring
/// heuristic covering the common credential-bearing headers (`x-auth-token`,
/// `api-key`, `x-session-token`, …).
fn is_sensitive_header(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    if matches!(
        n.as_str(),
        "authorization" | "proxy-authorization" | "cookie" | "set-cookie"
    ) {
        return true;
    }
    // `www-authenticate` is a public challenge header, not a secret.
    if n == "www-authenticate" {
        return false;
    }
    [
        "token", "secret", "password", "api-key", "apikey", "auth", "session",
    ]
    .iter()
    .any(|needle| n.contains(needle))
}

const FILTERED: &str = "[FILTERED]";

/// Redact URL-embedded credentials (`user:pass@host`) and sensitive query
/// parameters, preserving the rest of the URL.
pub fn sanitize_url(url: &reqwest::Url) -> String {
    let mut out = url.clone();

    // Collect redacted query pairs before mutating (borrow ends here).
    let has_query = out.query().is_some();
    let pairs: Vec<(String, String)> = if has_query {
        out.query_pairs()
            .map(|(k, v)| {
                if is_sensitive_param(&k) {
                    (k.into_owned(), FILTERED.to_string())
                } else {
                    (k.into_owned(), v.into_owned())
                }
            })
            .collect()
    } else {
        Vec::new()
    };

    // Strip any embedded username/password so credentials never reach the wire.
    let _ = out.set_username("");
    let _ = out.set_password(None);

    if has_query {
        out.query_pairs_mut().clear().extend_pairs(pairs);
    }
    out.to_string()
}

/// Copy request headers into a JSON object, redacting sensitive ones.
pub fn sanitize_headers(headers: &http::HeaderMap) -> Value {
    let mut map = Map::new();
    for (name, value) in headers {
        let key = name.as_str().to_string();
        let val = if is_sensitive_header(name.as_str()) {
            FILTERED.to_string()
        } else {
            value.to_str().unwrap_or("").to_string()
        };
        map.insert(key, Value::from(val));
    }
    Value::Object(map)
}

/// Maximum request-body size captured inline. Bodies larger than this are
/// dropped (with `no_body_reason = size_too_large`) rather than truncated, so a
/// partial payload never masquerades as the whole request.
const BODY_CAP: usize = 20 * 1024;

/// Decide whether an in-memory request body is captured inline, and if not, why.
///
/// Returns `(body, no_body_reason)` where exactly one side is `Some`:
/// - `body` — the UTF-8 payload (scrubbed of obvious inline credentials).
/// - `no_body_reason` — a snake_case reason the body was omitted:
///   - `no_data` — no in-memory body (empty request, or a streaming body reqwest
///     can't hand back as bytes),
///   - `no_content_type` — the request carried no `Content-Type`,
///   - `size_too_large` — the body exceeded [`BODY_CAP`],
///   - `cant_read_data` — the bytes were not valid UTF-8 (e.g. a binary body).
fn capture_request_body(
    body: Option<&[u8]>,
    content_type: Option<&str>,
) -> (Option<String>, Option<String>) {
    let Some(bytes) = body else {
        return (None, Some("no_data".to_string()));
    };
    if bytes.is_empty() {
        return (None, Some("no_data".to_string()));
    }
    let Some(content_type) = content_type else {
        return (None, Some("no_content_type".to_string()));
    };
    if bytes.len() > BODY_CAP {
        return (None, Some("size_too_large".to_string()));
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => (Some(sanitize_body(content_type, text)), None),
        Err(_) => (None, Some("cant_read_data".to_string())),
    }
}

/// Best-effort scrub of obvious inline credentials in a captured request body.
///
/// Unlike headers, body payloads have no uniform key/value shape, so this is
/// intentionally minimal: for `application/x-www-form-urlencoded` bodies it
/// redacts the values of sensitive keys (reusing the query-parameter rules);
/// other content types pass through unchanged. Deep/structured (e.g. nested
/// JSON) body scrubbing is out of scope here and noted as a known limitation.
fn sanitize_body(content_type: &str, body: &str) -> String {
    let is_form = content_type
        .split(';')
        .next()
        .map(|mime| {
            mime.trim()
                .eq_ignore_ascii_case("application/x-www-form-urlencoded")
        })
        .unwrap_or(false);
    if !is_form {
        return body.to_string();
    }
    body.split('&')
        .map(|pair| match pair.split_once('=') {
            Some((k, _)) if is_sensitive_param(k) => format!("{k}={FILTERED}"),
            _ => pair.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_sensitive_query_params() {
        let url = reqwest::Url::parse("https://api.example.com/v1?token=secret&page=2").unwrap();
        let sanitized = sanitize_url(&url);
        assert!(
            sanitized.contains("token=%5BFILTERED%5D") || sanitized.contains("token=[FILTERED]")
        );
        assert!(
            sanitized.contains("page=2"),
            "non-sensitive params preserved: {sanitized}"
        );
    }

    #[test]
    fn url_without_query_is_unchanged() {
        let url = reqwest::Url::parse("https://api.example.com/v1/users").unwrap();
        assert_eq!(sanitize_url(&url), "https://api.example.com/v1/users");
    }

    #[test]
    fn redacts_sensitive_headers() {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::AUTHORIZATION,
            "Bearer abc123".parse().unwrap(),
        );
        headers.insert(
            http::header::CONTENT_TYPE,
            "application/json".parse().unwrap(),
        );
        let v = sanitize_headers(&headers);
        assert_eq!(v["authorization"], FILTERED);
        assert_eq!(v["content-type"], "application/json");
    }

    #[test]
    fn captures_decodable_request_body() {
        let (body, reason) =
            capture_request_body(Some(b"{\"a\":1}"), Some("application/json; charset=utf-8"));
        assert_eq!(body.as_deref(), Some("{\"a\":1}"));
        assert_eq!(reason, None);
    }

    #[test]
    fn no_body_reason_no_data_when_absent_or_empty() {
        assert_eq!(
            capture_request_body(None, Some("application/json"))
                .1
                .as_deref(),
            Some("no_data")
        );
        assert_eq!(
            capture_request_body(Some(b""), Some("application/json"))
                .1
                .as_deref(),
            Some("no_data")
        );
    }

    #[test]
    fn no_body_reason_no_content_type() {
        let (body, reason) = capture_request_body(Some(b"payload"), None);
        assert!(body.is_none());
        assert_eq!(reason.as_deref(), Some("no_content_type"));
    }

    #[test]
    fn no_body_reason_size_too_large() {
        let big = vec![b'x'; BODY_CAP + 1];
        let (body, reason) = capture_request_body(Some(&big), Some("text/plain"));
        assert!(body.is_none(), "over-cap body is dropped, not truncated");
        assert_eq!(reason.as_deref(), Some("size_too_large"));

        // Exactly at the cap is still captured.
        let at_cap = vec![b'x'; BODY_CAP];
        let (body, reason) = capture_request_body(Some(&at_cap), Some("text/plain"));
        assert!(body.is_some());
        assert_eq!(reason, None);
    }

    #[test]
    fn no_body_reason_cant_read_data_for_non_utf8() {
        let (body, reason) =
            capture_request_body(Some(&[0xff, 0xfe, 0x00]), Some("application/octet-stream"));
        assert!(body.is_none());
        assert_eq!(reason.as_deref(), Some("cant_read_data"));
    }

    #[test]
    fn form_body_redacts_sensitive_values() {
        let (body, reason) = capture_request_body(
            Some(b"user=alice&password=hunter2&page=2"),
            Some("application/x-www-form-urlencoded"),
        );
        assert_eq!(reason, None);
        let body = body.unwrap();
        assert!(body.contains("user=alice"), "non-sensitive kept: {body}");
        assert!(body.contains("page=2"), "non-sensitive kept: {body}");
        assert!(
            body.contains(&format!("password={FILTERED}")),
            "secret redacted: {body}"
        );
        assert!(!body.contains("hunter2"), "secret value scrubbed: {body}");
    }
}
