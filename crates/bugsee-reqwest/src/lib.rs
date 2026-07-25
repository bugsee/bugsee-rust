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
    // Preserve the body/no_body_reason pairing invariant (exactly one is set):
    // a failed request never captured a body.
    e.custom.no_body_reason = Some("no_data".to_string());
    e
}

/// Whether a key (query parameter, form field, or JSON object key) names a value
/// that must be redacted. Used by URL, form-body, and recursive JSON redaction.
///
/// Matching normalizes away case and separators so `accessToken`, `access_token`,
/// and `access-token` all collapse to the same stem, then applies a substring
/// heuristic (like [`is_sensitive_header`]) rather than exact equality — exact
/// matching silently leaked common shapes like `accessToken`/`authorization`.
fn is_sensitive_param(name: &str) -> bool {
    // Lowercase and drop non-alphanumerics: accessToken/access_token/access-token
    // → "accesstoken"; x-api-key → "xapikey".
    let n: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();

    // Short, ambiguous tokens matched exactly to avoid over-redacting words that
    // merely contain them (e.g. "keyword", "monkey", "design").
    if matches!(n.as_str(), "key" | "sig" | "pin" | "otp") {
        return true;
    }
    // Credential-bearing stems; substring so compounds are covered
    // (clientSecret, refreshToken, x-session-id, csrfToken, …).
    const STEMS: &[&str] = &[
        "password",
        "passwd",
        "secret",
        "token",
        "apikey",
        "auth",
        "session",
        "cookie",
        "credential",
        "signature",
        "csrf",
        "xsrf",
        "bearer",
    ];
    STEMS.iter().any(|stem| n.contains(stem))
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

    // Redact sensitive key=value pairs in the fragment (e.g. an OAuth implicit
    // flow's `#access_token=…`). Non key=value fragments (SPA routes like
    // `#/dashboard`) are left intact by the form-style redactor.
    let fragment = out
        .fragment()
        .filter(|f| f.contains('='))
        .map(redact_form_encoded);

    // Strip any embedded username/password so credentials never reach the wire.
    let _ = out.set_username("");
    let _ = out.set_password(None);

    if has_query {
        out.query_pairs_mut().clear().extend_pairs(pairs);
    }
    if let Some(f) = fragment {
        out.set_fragment(Some(&f));
    }
    out.to_string()
}

/// Redact the values of sensitive keys in an `&`-joined `key=value` string
/// (form-urlencoded body or a query-like URL fragment), preserving the rest.
fn redact_form_encoded(body: &str) -> String {
    body.split('&')
        .map(|pair| match pair.split_once('=') {
            Some((k, _)) if is_sensitive_param(k) => format!("{k}={FILTERED}"),
            _ => pair.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&")
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
///   - `cant_read_data` — the bytes were not valid UTF-8, or a JSON body failed
///     to parse (so it can't be safely key-redacted),
///   - `unsupported_content_type` — a content type we don't know how to redact
///     (only form-urlencoded and JSON are captured; everything else is dropped to
///     avoid leaking secrets/PII in an opaque payload).
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
        Ok(text) => sanitize_body(content_type, text),
        Err(_) => (None, Some("cant_read_data".to_string())),
    }
}

/// Scrub a captured request body by content type, returning `(body, reason)`.
///
/// Only the two well-understood, redactable shapes are captured; anything else
/// is dropped rather than risk leaking secrets/PII:
/// - `application/x-www-form-urlencoded` — sensitive keys redacted.
/// - `application/json` (or `*+json`) — sensitive keys redacted recursively.
/// - anything else — not captured (`unsupported_content_type`).
fn sanitize_body(content_type: &str, body: &str) -> (Option<String>, Option<String>) {
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();

    if mime == "application/x-www-form-urlencoded" {
        let redacted = body
            .split('&')
            .map(|pair| match pair.split_once('=') {
                Some((k, _)) if is_sensitive_param(k) => format!("{k}={FILTERED}"),
                _ => pair.to_string(),
            })
            .collect::<Vec<_>>()
            .join("&");
        (Some(redacted), None)
    } else if mime == "application/json" || mime.ends_with("+json") {
        match serde_json::from_str::<Value>(body) {
            Ok(mut value) => {
                redact_json(&mut value);
                (Some(value.to_string()), None)
            }
            Err(_) => (None, Some("cant_read_data".to_string())),
        }
    } else {
        (None, Some("unsupported_content_type".to_string()))
    }
}

/// Recursively redact the values of sensitive keys in a JSON document.
fn redact_json(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                if is_sensitive_param(k) {
                    *v = Value::from(FILTERED);
                } else {
                    redact_json(v);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact_json),
        _ => {}
    }
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
        // The size cap is checked before content-type dispatch, so an over-cap
        // body is reported as too-large regardless of its (here unsupported) type.
        let big = vec![b'x'; BODY_CAP + 1];
        let (body, reason) = capture_request_body(Some(&big), Some("text/plain"));
        assert!(body.is_none(), "over-cap body is dropped, not truncated");
        assert_eq!(reason.as_deref(), Some("size_too_large"));

        // Exactly at the cap, with a supported (redactable) content type, is
        // still captured.
        let at_cap = vec![b'x'; BODY_CAP];
        let (body, reason) =
            capture_request_body(Some(&at_cap), Some("application/x-www-form-urlencoded"));
        assert!(body.is_some());
        assert_eq!(reason, None);
    }

    #[test]
    fn unsupported_content_type_body_is_dropped() {
        // A body we can't structurally redact (e.g. text/plain, XML, binary) is
        // not captured verbatim — that would risk leaking secrets/PII.
        let (body, reason) = capture_request_body(Some(b"hello world"), Some("text/plain"));
        assert!(body.is_none());
        assert_eq!(reason.as_deref(), Some("unsupported_content_type"));
    }

    #[test]
    fn json_body_redacts_sensitive_keys_recursively() {
        let (body, reason) = capture_request_body(
            Some(br#"{"user":"alice","password":"hunter2","nested":{"api_key":"sk-1"},"list":[{"token":"t"}]}"#),
            Some("application/json"),
        );
        assert_eq!(reason, None);
        let body = body.unwrap();
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["user"], "alice", "non-sensitive kept");
        assert_eq!(v["password"], FILTERED, "top-level secret redacted");
        assert_eq!(v["nested"]["api_key"], FILTERED, "nested secret redacted");
        assert_eq!(
            v["list"][0]["token"], FILTERED,
            "secret inside array redacted"
        );
        assert!(!body.contains("hunter2") && !body.contains("sk-1"));
    }

    #[test]
    fn malformed_json_body_is_dropped_not_leaked() {
        let (body, reason) =
            capture_request_body(Some(br#"{"password": "hunter2""#), Some("application/json"));
        assert!(body.is_none(), "unparseable JSON is not captured verbatim");
        assert_eq!(reason.as_deref(), Some("cant_read_data"));
    }

    #[test]
    fn sensitive_param_covers_camelcase_and_extended_keys() {
        // The old exact-match set leaked all of these; the normalized-substring
        // matcher must catch them (query params, form fields, and JSON keys).
        for k in [
            "accessToken",
            "access-token",
            "refreshToken",
            "authorization",
            "Authorization",
            "sessionId",
            "session_token",
            "Cookie",
            "clientSecret",
            "x-api-key",
            "csrfToken",
        ] {
            assert!(is_sensitive_param(k), "{k} must be treated as sensitive");
        }
        // …without over-redacting ordinary field names.
        for k in ["username", "page", "count", "message", "email", "designId"] {
            assert!(!is_sensitive_param(k), "{k} must NOT be redacted");
        }
    }

    #[test]
    fn url_fragment_credentials_are_redacted() {
        // OAuth implicit-flow tokens live in the fragment; they must not leak.
        let url = reqwest::Url::parse(
            "https://app.example/callback#access_token=SECRET&token_type=bearer&state=xyz",
        )
        .unwrap();
        let sanitized = sanitize_url(&url);
        assert!(
            !sanitized.contains("SECRET"),
            "fragment token leaked: {sanitized}"
        );
        assert!(
            sanitized.contains("state=xyz"),
            "non-sensitive kept: {sanitized}"
        );

        // A non key=value fragment (an SPA route) is preserved intact.
        let route = reqwest::Url::parse("https://app.example/#/dashboard/settings").unwrap();
        assert!(sanitize_url(&route).ends_with("#/dashboard/settings"));
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
