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

    /// Open a top-level span with the given operation.
    ///
    /// The returned span has no parent; call [`Span::start_span`] on it to open
    /// nested children that reference it via `parentSpanId`.
    pub fn start_span(&self, operation: impl Into<String>) -> Span {
        Span {
            tx: Arc::clone(&self.inner),
            span_id: random_hex(4),
            parent_span_id: None,
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
    /// `Some` for a child span (points at its parent's `span_id`); `None` for a
    /// top-level span opened directly from the transaction.
    parent_span_id: Option<String>,
    operation: String,
    description: Option<String>,
    status: Status,
    start_ms: i64,
    start_instant: Instant,
}

impl Span {
    /// Open a child span nested under this one.
    ///
    /// The child carries `parent_span_id = Some(self.span_id)`, so the flat span
    /// list on the transaction reconstructs into a tree via `spanId`/
    /// `parentSpanId`. The child records onto the same transaction as its parent
    /// (they share the transaction state), so finish children before the parent.
    pub fn start_span(&self, operation: impl Into<String>) -> Span {
        Span {
            tx: Arc::clone(&self.tx),
            span_id: random_hex(4),
            parent_span_id: Some(self.span_id.clone()),
            operation: operation.into(),
            description: None,
            status: Status::Ok,
            start_ms: epoch_ms(),
            start_instant: Instant::now(),
        }
    }

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
    ///
    /// A span MUST be finished before its [`Transaction`] (and a child span
    /// before its parent). Once the transaction has finished, its state is gone,
    /// so a late `finish` is silently dropped — no panic and no corruption, but
    /// the span is simply not recorded.
    pub fn finish(self) {
        let end_ms = epoch_ms();
        let Ok(mut guard) = self.tx.lock() else {
            return;
        };
        let Some(state) = guard.as_mut() else {
            return; // transaction already finished — drop this span gracefully
        };
        let duration_nanos = self.start_instant.elapsed().as_nanos() as i64;
        let start_offset_ns = self
            .start_instant
            .saturating_duration_since(state.start_instant)
            .as_nanos() as i64;
        state.spans.push(CoreSpan {
            span_id: self.span_id,
            parent_span_id: self.parent_span_id,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A parent span and a child opened from it both record onto the same
    /// transaction, and the child references the parent via `parent_span_id`
    /// while the parent has none.
    #[test]
    fn child_span_records_parent_span_id() {
        let tx = Transaction::start("MainScreen".to_string(), "ui.load".to_string());

        let parent = tx.start_span("db.query");
        let parent_id = parent.span_id.clone();
        let child = parent.start_span("db.row_decode");
        let child_id = child.span_id.clone();

        // Finish children before parents, and both before the transaction.
        child.finish();
        parent.finish();

        let guard = tx.inner.lock().unwrap();
        let state = guard.as_ref().expect("transaction still in-flight");
        assert_eq!(state.spans.len(), 2, "both spans recorded");

        let parent_span = state
            .spans
            .iter()
            .find(|s| s.span_id == parent_id)
            .expect("parent recorded");
        let child_span = state
            .spans
            .iter()
            .find(|s| s.span_id == child_id)
            .expect("child recorded");

        assert_eq!(
            parent_span.parent_span_id, None,
            "top-level span has no parent"
        );
        assert_eq!(
            child_span.parent_span_id.as_deref(),
            Some(parent_id.as_str()),
            "child points at its parent's span_id"
        );
        assert!(child_span.finished && parent_span.finished);
    }

    /// A span finished after its transaction is a graceful no-op (the state is
    /// already gone), not a panic.
    #[test]
    fn span_finished_after_transaction_is_dropped() {
        let tx = Transaction::start("Screen".to_string(), "ui.load".to_string());
        let span = tx.start_span("late");
        let inner = Arc::clone(&tx.inner);
        tx.finish(); // takes the state; submit_transaction runs without an SDK, harmless here
        span.finish(); // must not panic
        assert!(
            inner.lock().unwrap().is_none(),
            "transaction state consumed"
        );
    }
}
