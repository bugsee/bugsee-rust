//
//  perf.rs
//  bugsee
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! APM handles: start a transaction, open child spans, finish them. On finish a
//! completed [`bugsee_core::model::perf::Transaction`] is captured into the
//! window and exported to `performance.json`.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use bugsee_core::model::perf::{Span as CoreSpan, Status, Transaction as CoreTransaction};
use bugsee_core::util::{epoch_ms, random_hex};

/// Shared mutable state of an in-flight transaction.
struct TxState {
    trace_id: String,
    name: String,
    operation: String,
    start_ms: i64,
    start_instant: Instant,
    status: Status,
    spans: Vec<CoreSpan>,
}

/// A running APM transaction. Finish it to capture the timing.
pub struct Transaction {
    inner: Arc<Mutex<Option<TxState>>>,
}

impl Transaction {
    pub(crate) fn start(name: String, operation: String) -> Self {
        Transaction {
            inner: Arc::new(Mutex::new(Some(TxState {
                trace_id: random_hex(8),
                name,
                operation,
                start_ms: epoch_ms(),
                start_instant: Instant::now(),
                status: Status::Ok,
                spans: Vec::new(),
            }))),
        }
    }

    /// Open a child span with the given operation.
    pub fn start_span(&self, operation: impl Into<String>) -> Span {
        Span {
            tx: Arc::clone(&self.inner),
            span_id: random_hex(4),
            operation: operation.into(),
            description: None,
            status: Status::Ok,
            start_ms: epoch_ms(),
            start_instant: Instant::now(),
        }
    }

    /// Set the transaction status (default `Ok`).
    pub fn set_status(&self, status: Status) {
        if let Ok(mut guard) = self.inner.lock() {
            if let Some(state) = guard.as_mut() {
                state.status = status;
            }
        }
    }

    /// Finish the transaction and capture it. Consumes the handle.
    pub fn finish(self) {
        let end_ms = epoch_ms();
        let Ok(mut guard) = self.inner.lock() else {
            return;
        };
        let Some(state) = guard.take() else {
            return;
        };
        let duration_nanos = state.start_instant.elapsed().as_nanos() as i64;
        let transaction = CoreTransaction {
            timestamp: end_ms,
            trace_id: Some(state.trace_id),
            name: Some(state.name),
            operation: Some(state.operation),
            status: state.status,
            start_timestamp_ms: state.start_ms,
            end_timestamp_ms: end_ms,
            duration_nanos,
            is_snapshot: false,
            app_version: None,
            app_build: 0,
            spans: state.spans,
        };
        crate::api::submit_transaction(transaction);
    }
}

/// A running span within a transaction. Finish it to record its timing.
pub struct Span {
    tx: Arc<Mutex<Option<TxState>>>,
    span_id: String,
    operation: String,
    description: Option<String>,
    status: Status,
    start_ms: i64,
    start_instant: Instant,
}

impl Span {
    /// Attach a human-readable description (SQL, URL, path…).
    pub fn set_description(&mut self, description: impl Into<String>) -> &mut Self {
        self.description = Some(description.into());
        self
    }

    /// Set the span status (default `Ok`).
    pub fn set_status(&mut self, status: Status) -> &mut Self {
        self.status = status;
        self
    }

    /// Finish the span, recording it on its transaction. Consumes the handle.
    pub fn finish(self) {
        let end_ms = epoch_ms();
        let Ok(mut guard) = self.tx.lock() else {
            return;
        };
        let Some(state) = guard.as_mut() else {
            return; // transaction already finished
        };
        let duration_nanos = self.start_instant.elapsed().as_nanos() as i64;
        let start_offset_ns =
            self.start_instant.saturating_duration_since(state.start_instant).as_nanos() as i64;
        state.spans.push(CoreSpan {
            span_id: self.span_id,
            parent_span_id: None,
            operation: self.operation,
            description: self.description,
            status: self.status,
            start_timestamp_ms: self.start_ms,
            end_timestamp_ms: end_ms,
            duration_nanos,
            start_offset_ns: Some(start_offset_ns),
            finished: true,
            attributes: Default::default(),
        });
    }
}
