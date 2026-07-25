//
//  mod.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! Disk-backed capture: record framing, the part store (rotation / eviction /
//! hard-link snapshot), and snapshot export into per-type JSON documents.

pub mod export;
pub mod record;
pub mod store;

pub use export::{export_report, ChannelDocument};
pub use store::{PartStore, WindowCaps};
