//! Platform capability traits for the Bugsee shared engine.
//!
//! See `DESIGN_PLATFORM.md` §6. Core depends on these traits; desktop/console
//! crates supply implementations. An in-memory [`memory::MemoryStorage`] is
//! provided for contract tests without touching the real filesystem.

#![forbid(unsafe_op_in_unsafe_fn)]

pub mod clock;
pub mod entropy;
pub mod error;
pub mod http;
pub mod memory;
pub mod path;
pub mod storage;

pub use clock::{Clock, SystemClock};
pub use entropy::{Entropy, SystemEntropy};
pub use error::{EntropyError, HttpError, StorageError};
pub use http::{HttpRequest, HttpResponse, HttpTransport};
pub use memory::MemoryStorage;
pub use path::StoragePath;
pub use storage::{AppendSink, Storage, StorageCaps, StorageFileMeta};
