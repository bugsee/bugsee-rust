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
        let req_size = req
            .body()
            .and_then(|b| b.as_bytes())
            .map(|b| b.len() as i64)
            .unwrap_or(0);
        let headers = sanitize_headers(req.headers());

        Bugsee::capture_network(before_entry(&id, &method, &url, req_size, headers));

        let start = Instant::now();
        let result = next.run(req, extensions).await;
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;

        match &result {
            Ok(resp) => {
                let status = resp.status().as_u16() as i32;
                let size = resp.content_length().unwrap_or(0) as i64;
                Bugsee::capture_network(complete_entry(&id, &method, &url, size, status, elapsed_ms));
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

fn before_entry(id: &str, method: &str, url: &str, size: i64, headers: Value) -> NetworkEntry {
    let mut e = base_entry(id, method, url, NetworkStage::Before);
    e.size = size;
    e.custom.headers = Some(headers);
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
    ["token", "secret", "password", "api-key", "apikey", "auth", "session"]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_sensitive_query_params() {
        let url = reqwest::Url::parse("https://api.example.com/v1?token=secret&page=2").unwrap();
        let sanitized = sanitize_url(&url);
        assert!(sanitized.contains("token=%5BFILTERED%5D") || sanitized.contains("token=[FILTERED]"));
        assert!(sanitized.contains("page=2"), "non-sensitive params preserved: {sanitized}");
    }

    #[test]
    fn url_without_query_is_unchanged() {
        let url = reqwest::Url::parse("https://api.example.com/v1/users").unwrap();
        assert_eq!(sanitize_url(&url), "https://api.example.com/v1/users");
    }

    #[test]
    fn redacts_sensitive_headers() {
        let mut headers = http::HeaderMap::new();
        headers.insert(http::header::AUTHORIZATION, "Bearer abc123".parse().unwrap());
        headers.insert(http::header::CONTENT_TYPE, "application/json".parse().unwrap());
        let v = sanitize_headers(&headers);
        assert_eq!(v["authorization"], FILTERED);
        assert_eq!(v["content-type"], "application/json");
    }
}
