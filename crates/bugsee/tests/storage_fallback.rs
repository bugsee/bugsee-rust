//
//  storage_fallback.rs
//  bugsee
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! The facade on read-only / ephemeral hosts: `launch` must succeed and handled
//! errors must still go out when no data directory is usable.
//!
//! One test function on purpose: the SDK is a process-wide singleton and the
//! scenarios steer it through process environment variables.

#![cfg(unix)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bugsee::core::MockTransport;
use bugsee::{Bugsee, DataDirSource, LaunchOptions, LogLevel, StorageMode, DATA_DIR_ENV};

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bugsee-fallback-{}",
        bugsee::core::util::random_hex(8)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn launch_degrades_through_the_resolution_order() {
    let s = scratch();
    // A path nothing can be created at.
    let file = s.join("blocker");
    std::fs::write(&file, b"x").unwrap();
    let blocked = file.join("data");

    // Keep every default away from the real home directory and temp dir.
    std::env::remove_var("HOME");
    std::env::remove_var("XDG_DATA_HOME");
    std::env::set_var("TMPDIR", &file);
    std::env::remove_var(DATA_DIR_ENV);

    assert!(Bugsee::storage_status().is_none(), "nothing before launch");

    // 1. A writable explicit directory is used as given.
    let good = s.join("explicit");
    let transport = Arc::new(MockTransport::default());
    let guard = Bugsee::launch_with(
        LaunchOptions::new("TOKEN_A")
            .data_dir(&good)
            .with_transport(transport.clone()),
    )
    .expect("launch");
    let status = Bugsee::storage_status().expect("status");
    assert_eq!(status.mode, StorageMode::Disk);
    assert_eq!(status.source, DataDirSource::Explicit);
    assert_eq!(status.data_dir.as_deref(), Some(good.as_path()));
    assert!(status.crash_recovery_available());
    assert!(!status.may_be_ephemeral());
    drop(guard);

    // 2. An unusable explicit directory falls through to the environment variable.
    let from_env = s.join("from-env");
    std::env::set_var(DATA_DIR_ENV, &from_env);
    let guard = Bugsee::launch_with(
        LaunchOptions::new("TOKEN_A")
            .data_dir(&blocked)
            .with_transport(transport.clone()),
    )
    .expect("launch");
    let status = Bugsee::storage_status().expect("status");
    assert_eq!(status.source, DataDirSource::Environment);
    assert_eq!(status.data_dir.as_deref(), Some(from_env.as_path()));
    assert_eq!(status.skipped.len(), 1);
    drop(guard);
    std::env::remove_var(DATA_DIR_ENV);

    // 3. Nothing usable anywhere: memory mode, and reports still go out.
    let transport = Arc::new(MockTransport::default());
    let guard = Bugsee::launch_with(
        LaunchOptions::new("TOKEN_A")
            .data_dir(&blocked)
            .with_transport(transport.clone()),
    )
    .expect("an unwritable environment must not fail launch");
    let status = Bugsee::storage_status().expect("status");
    assert_eq!(status.mode, StorageMode::Memory);
    assert_eq!(status.source, DataDirSource::None);
    assert!(status.data_dir.is_none());
    assert!(!status.crash_recovery_available());
    assert!(!status.skipped.is_empty());

    Bugsee::event("ignored_without_a_window");
    Bugsee::capture_message(LogLevel::Error, "handled error with no disk");
    Bugsee::create_report().upload();
    assert!(Bugsee::flush(Duration::from_secs(10)));
    assert_eq!(
        transport.uploaded_bundles.lock().unwrap().len(),
        2,
        "both reports must be delivered from memory"
    );
    drop(guard);

    let _ = std::fs::remove_dir_all(&s);
}
