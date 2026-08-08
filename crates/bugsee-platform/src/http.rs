//! Low-level HTTP for platform backends (`DESIGN_PLATFORM.md` §6.2).
//!
//! Distinct from `bugsee-core::Transport` (the 3-step Bugsee API). Core's
//! session/issue/upload adapter will call this trait once wired.

use crate::error::HttpError;

#[derive(Debug, Clone)]
pub struct HttpRequest<'a> {
    pub method: &'a str,
    pub url: &'a str,
    pub headers: &'a [(&'a str, &'a str)],
    pub body: &'a [u8],
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

pub trait HttpTransport: Send + Sync {
    fn request(&self, req: HttpRequest<'_>) -> Result<HttpResponse, HttpError>;
}
