//
//  mod.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! The Bugsee wire-format data model.
//!
//! Every type here serializes to the JSON documented in the cross-platform
//! report-bundle contract (`bugsee/report-bundle-structure`). Capture files use
//! the envelope `{"version":2,"events":[…]}`; `manifest.json` is version `1`.

pub mod crash;
pub mod entry;
pub mod enums;
pub mod envelope;
pub mod environment;
pub mod report;
pub mod scope;
