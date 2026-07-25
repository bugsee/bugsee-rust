//
//  transport.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! The delivery contract: register session → create issue → PUT bundle to the
//! returned presigned URL (report-bundle-structure §2). The trait abstracts the
//! network so the pipeline is testable without a live server; a recording
//! [`MockTransport`] backs the tests.

use std::sync::Mutex;

use crate::reporting::AssembledReport;

/// A terminal or retryable delivery failure.
#[derive(Debug)]
pub enum TransportError {
    /// The held session/access token was rejected; caller should re-register.
    SessionExpired,
    /// Server reported a similar crash already exists — drop silently.
    DuplicateDropped,
    /// Server reported too many similar crashes — blacklist these signatures.
    TooManySimilar { signatures: Vec<String> },
    /// A transient failure worth retrying (network / 5xx / 429).
    Transient(String),
    /// A permanent failure — abandon the report.
    Permanent(String),
}

/// The three server interactions a report delivery needs.
pub trait Transport: Send + Sync {
    /// Exchange the app token for a short-lived access token.
    fn register_session(
        &self,
        app_token: &str,
        environment_json: &[u8],
    ) -> Result<String, TransportError>;

    /// Create the issue (body = `request.json`); returns the presigned upload URL.
    fn create_issue(
        &self,
        app_token: &str,
        access_token: Option<&str>,
        request_json: &[u8],
    ) -> Result<String, TransportError>;

    /// Upload the raw bundle ZIP to the presigned URL.
    fn upload_bundle(&self, endpoint: &str, zip: &[u8]) -> Result<(), TransportError>;
}

/// Deliver one assembled report through `transport`, holding/refreshing the
/// session token in `session`. Retries once on session expiry.
pub fn deliver(
    transport: &dyn Transport,
    app_token: &str,
    environment_json: &[u8],
    session: &Mutex<Option<String>>,
    report: &AssembledReport,
) -> Result<(), TransportError> {
    for attempt in 0..2 {
        // Ensure we hold an access token.
        let token = {
            let mut guard = session.lock().unwrap();
            if guard.is_none() {
                *guard = Some(transport.register_session(app_token, environment_json)?);
            }
            guard.clone()
        };

        match transport.create_issue(app_token, token.as_deref(), &report.request_json) {
            Ok(endpoint) => return transport.upload_bundle(&endpoint, &report.zip),
            Err(TransportError::SessionExpired) if attempt == 0 => {
                *session.lock().unwrap() = None; // force re-register, retry
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    Err(TransportError::Permanent("session retry exhausted".into()))
}

/// A recording transport for tests: captures every created issue body and
/// uploaded bundle.
#[derive(Default)]
pub struct MockTransport {
    pub sessions: Mutex<u32>,
    pub created_requests: Mutex<Vec<Vec<u8>>>,
    pub uploaded_bundles: Mutex<Vec<Vec<u8>>>,
}

impl Transport for MockTransport {
    fn register_session(
        &self,
        _app_token: &str,
        _environment_json: &[u8],
    ) -> Result<String, TransportError> {
        *self.sessions.lock().unwrap() += 1;
        Ok("mock-access-token".into())
    }

    fn create_issue(
        &self,
        _app_token: &str,
        _access_token: Option<&str>,
        request_json: &[u8],
    ) -> Result<String, TransportError> {
        self.created_requests
            .lock()
            .unwrap()
            .push(request_json.to_vec());
        Ok("https://uploads.example/mock-presigned".into())
    }

    fn upload_bundle(&self, _endpoint: &str, zip: &[u8]) -> Result<(), TransportError> {
        self.uploaded_bundles.lock().unwrap().push(zip.to_vec());
        Ok(())
    }
}
