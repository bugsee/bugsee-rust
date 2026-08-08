//! Desktop platform backends: `std::fs` storage + `ureq` HTTP.
//!
//! Implements `DESIGN_PLATFORM.md` capability traits for Linux / macOS / Windows.

#![forbid(unsafe_op_in_unsafe_fn)]

mod fs_storage;
mod ureq_http;

pub use fs_storage::FsStorage;
pub use ureq_http::UreqHttpTransport;
