//
//  transport.rs
//  bugsee
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! The default synchronous HTTP transport. Implements the 3-step delivery
//! (register session → create issue → PUT bundle) on top of
//! [`bugsee_platform::HttpTransport`] (desktop: [`UreqHttpTransport`]).

use std::sync::Arc;

use bugsee_core::transport::{Transport, TransportError};
use bugsee_platform::{HttpError, HttpRequest, HttpTransport as PlatformHttp};
use bugsee_platform_desktop::UreqHttpTransport;
use serde_json::{Map, Value};

/// Default API base URL.
pub const DEFAULT_BASE_URL: &str = "https://api.bugsee.com/v2";

/// A [`PlatformHttp`]-backed 3-step [`Transport`].
pub struct HttpTransport {
    base_url: String,
    http: Arc<dyn PlatformHttp>,
}

impl HttpTransport {
    /// Build a transport against `endpoint` or the default base URL.
    pub fn new(endpoint: Option<String>) -> Self {
        Self::with_http(endpoint, Arc::new(UreqHttpTransport::new()))
    }

    /// Inject a custom low-level HTTP backend (tests / hosts).
    pub fn with_http(endpoint: Option<String>, http: Arc<dyn PlatformHttp>) -> Self {
        HttpTransport {
            base_url: endpoint.unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            http,
        }
    }

    fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<(u16, Vec<u8>), TransportError> {
        let resp = self
            .http
            .request(HttpRequest {
                method,
                url,
                headers,
                body,
            })
            .map_err(map_http_error)?;
        Ok((resp.status, resp.body))
    }
}

/// Pull `(code, message)` out of a `{"error":{"code":…,"message":…}}` envelope.
fn app_error_parts(v: &Value) -> (Option<i64>, Option<String>) {
    let err = v.get("error");
    let code = err
        .and_then(|e| e.get("code"))
        .and_then(serde_json::Value::as_i64);
    let message = err
        .and_then(|e| e.get("message"))
        .and_then(serde_json::Value::as_str)
        .map(|m| m.trim())
        .filter(|m| !m.is_empty())
        .map(str::to_string);
    (code, message)
}

fn map_app_error(v: &Value) -> TransportError {
    let (code, message) = app_error_parts(v);
    match code {
        Some(12003) => TransportError::DuplicateDropped,
        Some(12004) => TransportError::TooManySimilar { signatures: vec![] },
        _ => TransportError::Permanent(match (code, message) {
            (Some(c), Some(m)) => format!("server error {c}: {m}"),
            (Some(c), None) => format!("server error {c}"),
            (None, Some(m)) => format!("server error: {m}"),
            (None, None) => "server rejected the request (no code or message)".to_string(),
        }),
    }
}

fn check_envelope(v: &Value) -> Result<(), TransportError> {
    if v.get("ok").and_then(serde_json::Value::as_bool) == Some(false) {
        return Err(map_app_error(v));
    }
    Ok(())
}

fn map_http_error(err: HttpError) -> TransportError {
    match err {
        HttpError::NotReady | HttpError::Suspended => {
            // Phase 1c will teach the uploader not to burn abandon budget.
            // Until then, treat as transient so we still retry.
            TransportError::Transient(err.to_string())
        }
        HttpError::Transient(s) => TransportError::Transient(s),
        HttpError::Permanent(s) => TransportError::Permanent(s),
    }
}

fn map_status(status: u16, body: &[u8]) -> Result<(), TransportError> {
    if (200..300).contains(&status) {
        return Ok(());
    }
    let parsed: Option<Value> = serde_json::from_slice(body).ok();
    let (app_code, message) = parsed
        .as_ref()
        .map(app_error_parts)
        .unwrap_or((None, None));
    match (status, app_code) {
        (401, _) => Err(TransportError::SessionExpired),
        (_, Some(12003)) => Err(TransportError::DuplicateDropped),
        (_, Some(12004)) => Err(TransportError::TooManySimilar { signatures: vec![] }),
        (429, _) | (500..=599, _) => Err(TransportError::Transient(format!("http {status}"))),
        _ => Err(TransportError::Permanent(match message {
            Some(m) => format!("http {status}: {m}"),
            None => format!("http {status}"),
        })),
    }
}

impl Transport for HttpTransport {
    fn register_session(
        &self,
        app_token: &str,
        environment_json: &[u8],
    ) -> Result<String, TransportError> {
        let env: Value = serde_json::from_slice(environment_json).unwrap_or(Value::Null);
        let body = serde_json::json!({ "app_token": app_token, "environment": env });
        let body_bytes = serde_json::to_vec(&body).unwrap_or_default();
        let url = format!("{}/sessions", self.base_url);
        let (status, resp_body) = self.request(
            "POST",
            &url,
            &[
                ("Content-Type", "application/json"),
                ("x-client-type", "rust"),
            ],
            &body_bytes,
        )?;
        map_status(status, &resp_body)?;
        let v: Value = serde_json::from_slice(&resp_body)
            .map_err(|e| TransportError::Transient(e.to_string()))?;
        check_envelope(&v)?;
        v.get("result")
            .and_then(|r| r.get("access_token"))
            .and_then(|t| t.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| TransportError::Permanent("no access_token in response".into()))
    }

    fn create_issue(
        &self,
        app_token: &str,
        access_token: Option<&str>,
        request_json: &[u8],
    ) -> Result<String, TransportError> {
        let mut body: Map<String, Value> = serde_json::from_slice(request_json)
            .map_err(|e| TransportError::Permanent(e.to_string()))?;
        body.insert("app_token".into(), Value::from(app_token));
        if let Some(t) = access_token {
            body.insert("access_token".into(), Value::from(t));
        }
        let body_bytes = serde_json::to_vec(&Value::Object(body)).unwrap_or_default();
        let url = format!("{}/issues?app_token={}", self.base_url, app_token);
        let (status, resp_body) = self.request(
            "POST",
            &url,
            &[
                ("Content-Type", "application/json"),
                ("x-client-type", "rust"),
            ],
            &body_bytes,
        )?;
        map_status(status, &resp_body)?;
        let v: Value = serde_json::from_slice(&resp_body)
            .map_err(|e| TransportError::Transient(e.to_string()))?;
        check_envelope(&v)?;
        v.get("result")
            .and_then(|r| r.get("endpoint"))
            .and_then(|e| e.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| TransportError::Permanent("no endpoint in response".into()))
    }

    fn upload_bundle(&self, endpoint: &str, zip: &[u8]) -> Result<(), TransportError> {
        let len = zip.len().to_string();
        let (status, body) =
            self.request("PUT", endpoint, &[("Content-Length", len.as_str())], zip)?;
        map_status(status, &body)
    }
}
