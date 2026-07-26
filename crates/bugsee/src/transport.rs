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

fn map_ureq_error(err: ureq::Error) -> TransportError {
    match err {
        ureq::Error::Status(code, resp) => {
            // Inspect the response envelope for Bugsee app error codes.
            let app_code = resp.into_json::<Value>().ok().and_then(|v| {
                v.get("error")
                    .and_then(|e| e.get("code"))
                    .and_then(|c| c.as_i64())
            });
            match (code, app_code) {
                (401, _) => TransportError::SessionExpired,
                (_, Some(12003)) => TransportError::DuplicateDropped,
                (_, Some(12004)) => TransportError::TooManySimilar { signatures: vec![] },
                (429, _) | (500..=599, _) => TransportError::Transient(format!("http {code}")),
                _ => TransportError::Permanent(format!("http {code}")),
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
