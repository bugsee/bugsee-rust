//! `ureq`-backed [`HttpTransport`]. Desktop never emits NotReady/Suspended
//! unless we add hooks later (`DESIGN_PLATFORM.md` §6.2).
//!
//! HTTP status responses (including 4xx/5xx) are returned as [`HttpResponse`]
//! so upper layers can inspect bodies for Bugsee app-error envelopes. Only
//! socket/transport failures become [`HttpError`].

use std::io::Read;
use std::time::Duration;

use bugsee_platform::{HttpError, HttpRequest, HttpResponse, HttpTransport};

#[derive(Debug, Clone)]
pub struct UreqHttpTransport {
    agent: ureq::Agent,
}

impl UreqHttpTransport {
    pub fn new() -> Self {
        // Match the facade's prior timeout policy so a stalled peer cannot
        // wedge the single uploader thread forever.
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(15))
            .timeout_read(Duration::from_secs(30))
            .timeout_write(Duration::from_secs(30))
            .build();
        Self { agent }
    }
}

impl Default for UreqHttpTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpTransport for UreqHttpTransport {
    fn request(&self, req: HttpRequest<'_>) -> Result<HttpResponse, HttpError> {
        let mut builder = match req.method.to_ascii_uppercase().as_str() {
            "GET" => self.agent.get(req.url),
            "POST" => self.agent.post(req.url),
            "PUT" => self.agent.put(req.url),
            "DELETE" => self.agent.delete(req.url),
            other => {
                return Err(HttpError::Permanent(format!("unsupported method {other}")));
            }
        };
        for (k, v) in req.headers {
            builder = builder.set(k, v);
        }
        let result = if req.body.is_empty() && req.method.eq_ignore_ascii_case("GET") {
            builder.call()
        } else {
            builder.send_bytes(req.body)
        };
        match result {
            Ok(resp) => read_response(resp),
            Err(ureq::Error::Status(code, resp)) => {
                let mut headers = Vec::new();
                for h in resp.headers_names() {
                    if let Some(v) = resp.header(&h) {
                        headers.push((h, v.to_string()));
                    }
                }
                let mut body = Vec::new();
                let _ = resp.into_reader().read_to_end(&mut body);
                Ok(HttpResponse {
                    status: code,
                    headers,
                    body,
                })
            }
            Err(ureq::Error::Transport(t)) => Err(HttpError::Transient(t.to_string())),
        }
    }
}

fn read_response(resp: ureq::Response) -> Result<HttpResponse, HttpError> {
    let status = resp.status();
    let mut headers = Vec::new();
    for h in resp.headers_names() {
        if let Some(v) = resp.header(&h) {
            headers.push((h, v.to_string()));
        }
    }
    let mut body = Vec::new();
    let _ = resp.into_reader().read_to_end(&mut body);
    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}
