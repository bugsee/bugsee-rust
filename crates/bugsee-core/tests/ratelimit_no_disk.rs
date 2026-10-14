//
//  ratelimit_no_disk.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! A rate-limited storm must be cheap: reports the limiter denies never read the
//! on-disk blacklist. Own test binary, because the read counter is process-wide.

use std::sync::Arc;
use std::time::Duration;

use bugsee_core::model::enums::IssueType;
use bugsee_core::queue::blacklist_read_count;
use bugsee_core::ratelimit::{Rate, RateLimitConfig};
use bugsee_core::reporting::manual_upload_meta;
use bugsee_core::{MockTransport, Recorder, RecorderConfig};

#[test]
fn denied_storm_reports_never_touch_the_disk_blacklist() {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "bugsee-nodisk-{}",
        bugsee_core::util::random_hex(8)
    ));
    std::fs::create_dir_all(&dir).unwrap();

    let transport = Arc::new(MockTransport::default());
    let mut cfg = RecorderConfig::new(&dir, "T");
    cfg.rate_limit = RateLimitConfig::default()
        .per_signature(Rate::new(2, Duration::from_secs(3600)))
        .global(Rate::new(1000, Duration::from_secs(1)));
    let r = Recorder::launch(cfg, transport.clone()).unwrap();
    assert!(r.flush(Duration::from_secs(10))); // launch-time recovery is done

    let before = blacklist_read_count();
    for _ in 0..300 {
        let mut m = manual_upload_meta();
        m.issue_type = IssueType::Error;
        m.signatures = vec!["hot".into()];
        r.report(m, None);
    }
    assert!(r.flush(Duration::from_secs(30)));
    let reads = blacklist_read_count() - before;

    // (A few of the 300 may be shed earlier still by the bounded report queue.)
    let denied = r.suppressed_reports();
    assert!((200..=298).contains(&denied), "denied {denied}");
    // Only the two admitted reports may pay for the disk read.
    assert!(reads <= 2, "{reads} blacklist reads for a 300-report storm");
    drop(r);
    let _ = std::fs::remove_dir_all(&dir);
}
