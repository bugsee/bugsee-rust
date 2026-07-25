//
//  envelope.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! The capture-file envelope: `{"version":2,"events":[…]}`. Every per-type
//! capture document in a bundle uses this wrapper (report-bundle-structure §3.3).

use serde::{Deserialize, Serialize};

/// The integer version of every capture envelope (distinct from the manifest
/// version, which is `1`).
pub const EVENTS_VERSION: u32 = 2;

/// A capture-file envelope wrapping a homogeneous list of entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope<T> {
    pub version: u32,
    pub events: Vec<T>,
}

impl<T> Envelope<T> {
    /// Wrap `events` with the current envelope version.
    pub fn new(events: Vec<T>) -> Self {
        Envelope {
            version: EVENTS_VERSION,
            events,
        }
    }
}
