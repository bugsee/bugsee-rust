//
//  options.rs
//  bugsee
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! Typed launch configuration. Mirrors the mobile SDK option surface, adapted
//! to a builder rather than a string-keyed registry.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bugsee_core::capture::WindowCaps;
use bugsee_core::transport::Transport;

/// Configuration passed to [`crate::Bugsee::launch_with`].
pub struct LaunchOptions {
    pub(crate) app_token: String,
    pub(crate) data_dir: Option<PathBuf>,
    pub(crate) max_window: Duration,
    pub(crate) max_bytes: u64,
    pub(crate) rotate_interval: Duration,
    pub(crate) endpoint: Option<String>,
    pub(crate) transport: Option<Arc<dyn Transport>>,
}

impl LaunchOptions {
    /// Options for `app_token` with sensible defaults.
    pub fn new(app_token: impl Into<String>) -> Self {
        LaunchOptions {
            app_token: app_token.into(),
            data_dir: None,
            max_window: Duration::from_secs(60),
            max_bytes: 8 << 20,
            rotate_interval: Duration::from_secs(1),
            endpoint: None,
            transport: None,
        }
    }

    /// Override the on-disk data directory (defaults to a per-app temp dir).
    pub fn data_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.data_dir = Some(dir.into());
        self
    }

    /// Maximum wall-time span of captured context retained.
    pub fn max_window(mut self, window: Duration) -> Self {
        self.max_window = window;
        self
    }

    /// Maximum on-disk bytes retained across the capture window.
    pub fn max_bytes(mut self, bytes: u64) -> Self {
        self.max_bytes = bytes;
        self
    }

    /// Override the API base URL (defaults to `https://api.bugsee.com/v2`).
    pub fn endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }

    /// Inject a custom transport (e.g. for tests), bypassing the default HTTP one.
    pub fn with_transport(mut self, transport: Arc<dyn Transport>) -> Self {
        self.transport = Some(transport);
        self
    }

    pub(crate) fn caps(&self) -> WindowCaps {
        WindowCaps {
            max_window_ms: self.max_window.as_millis() as i64,
            max_bytes: self.max_bytes,
        }
    }

    pub(crate) fn resolved_data_dir(&self) -> PathBuf {
        self.data_dir.clone().unwrap_or_else(|| {
            let mut dir = std::env::temp_dir();
            dir.push("bugsee");
            // Isolate by app token so multiple apps don't collide.
            dir.push(short_token(&self.app_token));
            dir
        })
    }
}

fn short_token(token: &str) -> String {
    token.chars().filter(|c| c.is_ascii_alphanumeric()).take(16).collect()
}
