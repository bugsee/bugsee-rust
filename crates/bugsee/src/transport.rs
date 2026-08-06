//
//  transport.rs
//  bugsee
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! The default synchronous HTTP transport (ureq). Implements the 3-step
//! delivery: register session → create issue → PUT bundle to the presigned URL.

use std::time::Duration;

use bugsee_core::transport::{Transport, TransportError};
use serde_json::{Map, Value};

/// Default API base URL.
pub const DEFAULT_BASE_URL: &str = "https://api.bugsee.com/v2";

/// A ureq-backed [`Transport`].
pub struct HttpTransport {
    base_url: String,
    agent: ureq::Agent,
}

impl HttpTransport {
    /// Build a transport against `endpoint` or the default base URL.
    pub fn new(endpoint: Option<String>) -> Self {
        // Use one Agent with explicit timeouts so a stalled connection can never
        // wedge the single uploader thread forever (the bare `ureq::get/post/put`
        // helpers use a default agent with NO timeouts). The read/write timeouts
        // are per-socket-operation, so they trip on a stalled peer without
        // penalizing a slow-but-progressing large-bundle PUT.
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(15))
            .timeout_read(Duration::from_secs(30))
            .timeout_write(Duration::from_secs(30))
            .build();
        HttpTransport {
            base_url: endpoint.unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            agent,
        }
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

/// Map an `ok:false` envelope onto a [`TransportError`].
///
/// The API signals APPLICATION errors with an `ok:false` body and — this is the
/// part that matters — an HTTP **200**. `ureq` only yields `Error::Status` for
/// 4xx/5xx, so an envelope consulted solely from [`map_ureq_error`] is invisible
/// on the status the server actually uses for them. Both paths funnel here.
///
/// The message is carried into the error rather than dropped: a rejected report
/// is deleted from the queue (`runtime.rs`, "Duplicate / permanent — abandon the
/// report"), so whatever the server said here is the only account of why.
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

/// `Err` when the body carries `ok:false`, whatever the HTTP status was.
fn check_envelope(v: &Value) -> Result<(), TransportError> {
    if v.get("ok").and_then(serde_json::Value::as_bool) == Some(false) {
        return Err(map_app_error(v));
    }
    Ok(())
}

fn map_ureq_error(err: ureq::Error) -> TransportError {
    match err {
        ureq::Error::Status(code, resp) => {
            // Inspect the response envelope for Bugsee app error codes.
            let body: Option<Value> = resp.into_json().ok();
            let (app_code, message) = body.as_ref().map(app_error_parts).unwrap_or((None, None));
            match (code, app_code) {
                (401, _) => TransportError::SessionExpired,
                (_, Some(12003)) => TransportError::DuplicateDropped,
                (_, Some(12004)) => TransportError::TooManySimilar { signatures: vec![] },
                (429, _) | (500..=599, _) => TransportError::Transient(format!("http {code}")),
                _ => TransportError::Permanent(match message {
                    Some(m) => format!("http {code}: {m}"),
                    None => format!("http {code}"),
                }),
            }
        }
        // Transport/IO errors are retryable.
        other => TransportError::Transient(other.to_string()),
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
        let resp = self
            .agent
            .post(&format!("{}/sessions", self.base_url))
            .set("Content-Type", "application/json")
            .set("x-client-type", "rust")
            .send_json(body)
            .map_err(map_ureq_error)?;
        let v: Value = resp
            .into_json()
            .map_err(|e| TransportError::Transient(e.to_string()))?;
        // An application error arrives as HTTP 200 + `ok:false`; without this
        // the miss falls through to the opaque "no access_token" below.
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
        // Merge app_token + access_token into the request.json body.
        let mut body: Map<String, Value> = serde_json::from_slice(request_json)
            .map_err(|e| TransportError::Permanent(e.to_string()))?;
        body.insert("app_token".into(), Value::from(app_token));
        if let Some(t) = access_token {
            body.insert("access_token".into(), Value::from(t));
        }
        let url = format!("{}/issues?app_token={}", self.base_url, app_token);
        let resp = self
            .agent
            .post(&url)
            .set("Content-Type", "application/json")
            .set("x-client-type", "rust")
            .send_json(Value::Object(body))
            .map_err(map_ureq_error)?;
        let v: Value = resp
            .into_json()
            .map_err(|e| TransportError::Transient(e.to_string()))?;
        // The load-bearing one. A rejected issue (e.g. a validation failure on
        // some environment field) comes back 200 + `ok:false`, and without this
        // it degraded to "no endpoint in response" — the report was then dropped
        // with the server's explanation discarded.
        check_envelope(&v)?;
        v.get("result")
            .and_then(|r| r.get("endpoint"))
            .and_then(|e| e.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| TransportError::Permanent("no endpoint in response".into()))
    }

    fn upload_bundle(&self, endpoint: &str, zip: &[u8]) -> Result<(), TransportError> {
        self.agent
            .put(endpoint)
            .set("Content-Length", &zip.len().to_string())
            .send_bytes(zip)
            .map_err(map_ureq_error)?;
        Ok(())
    }
}
