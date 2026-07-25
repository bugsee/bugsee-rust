//
//  record.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! On-disk record framing for capture part streams.
//!
//! Each record is `[u64 LE timestamp][u32 LE len][payload]`. The fixed 8-byte
//! timestamp header lets the export path filter by time without parsing the
//! payload, and the length prefix makes reads self-terminating: a zero-length
//! prefix (the zeroed tail of a grown file) or a length that runs past the
//! buffer (a torn trailing write) ends iteration cleanly. This is the
//! crash-recovery contract — a part flushed mid-write still parses up to its
//! last complete record.

/// Bytes of fixed header before each payload: `u64` timestamp + `u32` length.
pub const RECORD_HEADER_LEN: usize = 12;

/// Append a framed record for `payload` stamped at `ts` (epoch ms) to `out`.
pub fn frame(ts: i64, payload: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(&(ts as u64).to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
}

/// The framed size of a record carrying `payload_len` payload bytes.
pub fn framed_len(payload_len: usize) -> usize {
    RECORD_HEADER_LEN + payload_len
}

/// Iterator over complete records in a byte buffer. Terminates at the first
/// zero-length or truncated record.
pub struct RecordIter<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> RecordIter<'a> {
    /// Iterate records in `bytes`.
    pub fn new(bytes: &'a [u8]) -> Self {
        RecordIter { bytes, pos: 0 }
    }
}

impl<'a> Iterator for RecordIter<'a> {
    /// `(timestamp, payload)` for each complete record.
    type Item = (i64, &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos + RECORD_HEADER_LEN > self.bytes.len() {
            return None;
        }
        let ts = i64::from_le_bytes(self.bytes[self.pos..self.pos + 8].try_into().unwrap());
        let len =
            u32::from_le_bytes(self.bytes[self.pos + 8..self.pos + 12].try_into().unwrap()) as usize;
        if len == 0 {
            // Zeroed tail of a preallocated/grown file — end of real data.
            return None;
        }
        let start = self.pos + RECORD_HEADER_LEN;
        let end = start.checked_add(len)?;
        if end > self.bytes.len() {
            // Torn trailing write — stop before the incomplete record.
            return None;
        }
        self.pos = end;
        Some((ts, &self.bytes[start..end]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_and_iter_roundtrip() {
        let mut buf = Vec::new();
        frame(10, b"alpha", &mut buf);
        frame(20, b"beta", &mut buf);
        frame(30, b"", &mut buf); // note: empty payload => zero length => terminator
        let got: Vec<_> = RecordIter::new(&buf).map(|(t, p)| (t, p.to_vec())).collect();
        // The empty-payload record acts as a terminator, so only two are read.
        assert_eq!(got, vec![(10, b"alpha".to_vec()), (20, b"beta".to_vec())]);
    }

    #[test]
    fn zeroed_tail_terminates() {
        let mut buf = Vec::new();
        frame(1, b"x", &mut buf);
        buf.extend_from_slice(&[0u8; 64]); // grown/zeroed tail
        let got: Vec<_> = RecordIter::new(&buf).collect();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, 1);
    }

    #[test]
    fn torn_trailing_record_is_dropped() {
        let mut buf = Vec::new();
        frame(1, b"complete", &mut buf);
        // A header claiming 100 bytes but only a few present.
        buf.extend_from_slice(&2i64.to_le_bytes());
        buf.extend_from_slice(&100u32.to_le_bytes());
        buf.extend_from_slice(b"short");
        let got: Vec<_> = RecordIter::new(&buf).collect();
        assert_eq!(got.len(), 1, "torn record must be skipped");
        assert_eq!(got[0].1, b"complete");
    }

    #[test]
    fn partial_header_terminates() {
        let mut buf = Vec::new();
        frame(1, b"ok", &mut buf);
        buf.extend_from_slice(&[0u8; 3]); // fewer than RECORD_HEADER_LEN trailing bytes
        assert_eq!(RecordIter::new(&buf).count(), 1);
    }
}
