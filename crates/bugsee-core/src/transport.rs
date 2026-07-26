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
///
/// `cached_endpoint` is a presigned upload URL a prior attempt obtained but
/// failed to PUT to; when set, delivery resumes at the PUT instead of re-creating
/// the issue (avoiding a duplicate issue / a `12003` drop of an un-uploaded
/// bundle). On a *transient* PUT failure the endpoint that should be persisted
/// for the next retry is written to `endpoint_out`; on any other outcome
/// `endpoint_out` is left `None` (the caller then clears any stale cache).
pub fn deliver(
    transport: &dyn Transport,
    app_token: &str,
    environment_json: &[u8],
    session: &Mutex<Option<String>>,
    report: &AssembledReport,
    cached_endpoint: Option<&str>,
    endpoint_out: &mut Option<String>,
) -> Result<(), TransportError> {
    // Resume at the PUT if we already have a presigned endpoint from a prior
    // attempt. A transient failure keeps it cached; any other failure (e.g. the
    // presigned URL expired → 403) falls through to recreate the issue.
    if let Some(endpoint) = cached_endpoint {
        match transport.upload_bundle(endpoint, &report.zip) {
            Ok(()) => return Ok(()),
            Err(TransportError::Transient(e)) => {
                *endpoint_out = Some(endpoint.to_string());
                return Err(TransportError::Transient(e));
            }
            Err(_) => { /* stale/expired endpoint — recreate the issue below */ }
        }
    }

    for attempt in 0..2 {
        // Ensure we hold an access token, never holding the lock across the
        // (blocking) network call: read under the lock, register outside it, then
        // briefly re-lock to store — so a future second locker of `session` can't
        // stall behind a token refresh.
        let token = {
            let existing = session.lock().unwrap_or_else(|e| e.into_inner()).clone();
            match existing {
                Some(t) => Some(t),
                None => {
                    let fresh = transport.register_session(app_token, environment_json)?;
                    let mut guard = session.lock().unwrap_or_else(|e| e.into_inner());
                    // Another path may have registered while we were unlocked;
                    // keep the existing token if so, else store ours.
                    Some(guard.get_or_insert(fresh).clone())
                }
            }
        };

        match transport.create_issue(app_token, token.as_deref(), &report.request_json) {
            Ok(endpoint) => {
                return match transport.upload_bundle(&endpoint, &report.zip) {
                    Ok(()) => Ok(()),
                    // Persist the endpoint so the retry resumes at the PUT rather
                    // than re-POSTing create_issue (duplicate issue / 12003 drop).
                    Err(TransportError::Transient(e)) => {
                        *endpoint_out = Some(endpoint);
                        Err(TransportError::Transient(e))
                    }
                    Err(e) => Err(e),
                };
            }
            Err(TransportError::SessionExpired) if attempt == 0 => {
                *session.lock().unwrap_or_else(|e| e.into_inner()) = None; // re-register, retry
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
