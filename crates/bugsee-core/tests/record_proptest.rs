//
//  record_proptest.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! Property tests for the on-disk record framing (`capture::record`). Two
//! invariants: framed records round-trip through `RecordIter`, and iteration
//! over *arbitrary* bytes never panics or reads out of bounds (the
//! crash-recovery contract must survive torn/garbage trailing writes).

use bugsee_core::capture::record::{frame, RecordIter, RECORD_HEADER_LEN};
use proptest::prelude::*;

proptest! {
    /// Framing N records then iterating yields exactly those records back.
    /// Payloads are non-empty because a zero-length record is, by contract, a
    /// stream terminator (see `record.rs`).
    #[test]
    fn frame_then_iter_roundtrips_nonempty_payloads(
        records in proptest::collection::vec(
            (any::<i64>(), proptest::collection::vec(any::<u8>(), 1..64usize)),
            0..32usize,
        )
    ) {
        let mut buf = Vec::new();
        for (ts, payload) in &records {
            frame(*ts, payload, &mut buf);
        }

        // The framed buffer is exactly header + payload per record.
        let expected_len: usize =
            records.iter().map(|(_, p)| RECORD_HEADER_LEN + p.len()).sum();
        prop_assert_eq!(buf.len(), expected_len);

        let got: Vec<(i64, Vec<u8>)> =
            RecordIter::new(&buf).map(|(t, p)| (t, p.to_vec())).collect();
        let expected: Vec<(i64, Vec<u8>)> =
            records.iter().map(|(t, p)| (*t, p.clone())).collect();
        prop_assert_eq!(got, expected);
    }

    /// `RecordIter` over ANY byte vector must terminate without panicking and
    /// must only ever hand back sub-slices that live inside the input buffer.
    #[test]
    fn iter_never_panics_or_reads_out_of_bounds(
        bytes in proptest::collection::vec(any::<u8>(), 0..1024usize)
    ) {
        let base = bytes.as_ptr() as usize;
        let mut consumed = 0usize;
        for (_ts, payload) in RecordIter::new(&bytes) {
            // Payload must be a sub-slice of `bytes`.
            let start = payload.as_ptr() as usize;
            prop_assert!(start >= base);
            prop_assert!(start + payload.len() <= base + bytes.len());
            // Each yielded record accounts for at least a full header.
            consumed += RECORD_HEADER_LEN + payload.len();
        }
        prop_assert!(consumed <= bytes.len());

        // count() must also complete (proves iteration terminates).
        let n = RecordIter::new(&bytes).count();
        prop_assert!(n <= bytes.len() / RECORD_HEADER_LEN + 1);
    }
}
