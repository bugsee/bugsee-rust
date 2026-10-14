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

    // 2. A directory the host asked for is FINAL: unusable means memory mode, not
    //    some other location on disk (here the environment variable is writable
    //    and must stay untouched).
    let from_env = s.join("from-env");
    std::env::set_var(DATA_DIR_ENV, &from_env);
    let guard = Bugsee::launch_with(
        LaunchOptions::new("TOKEN_A")
            .data_dir(&blocked)
            .with_transport(transport.clone()),
    )
    .expect("launch");
    let status = Bugsee::storage_status().expect("status");
    assert_eq!(status.mode, StorageMode::Memory);
    assert!(status.explicit_rejected.is_some());
    assert!(!from_env.exists(), "must not fall back to another location");
    drop(guard);

    // 3. Without an explicit directory the environment variable is used.
    let guard =
        Bugsee::launch_with(LaunchOptions::new("TOKEN_A").with_transport(transport.clone()))
            .expect("launch");
    let status = Bugsee::storage_status().expect("status");
    assert_eq!(status.source, DataDirSource::Environment);
    assert_eq!(status.data_dir.as_deref(), Some(from_env.as_path()));
    drop(guard);
    std::env::remove_var(DATA_DIR_ENV);

    // 4. Nothing usable anywhere (no home, temp dir unusable): memory mode, and
    //    reports still go out.
    let transport = Arc::new(MockTransport::default());
    let guard =
        Bugsee::launch_with(LaunchOptions::new("TOKEN_A").with_transport(transport.clone()))
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

    // 5. A requested directory that cannot take new files but already holds a
    //    queued report (a full or read-only volume): memory mode, yet the
    //    report a previous run left there is still delivered.
    {
        use bugsee::core::model::report::TimeWindow;
        use std::os::unix::fs::PermissionsExt;

        let ro = s.join("ro-volume");
        let env = bugsee::core::model::environment::Environment::detect_with(
            "t",
            &Default::default(),
            &Default::default(),
        );
        let queued = bugsee::core::reporting::assemble_docs(
            Vec::new(),
            TimeWindow { start: 0, end: 1 },
            &bugsee::core::reporting::manual_upload_meta(),
            &env,
            None,
            &[],
            "TOKEN_A",
            1,
        )
        .unwrap();
        bugsee::core::queue::enqueue(&ro, &queued).unwrap();
        for d in [ro.join("queue"), ro.clone()] {
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o555)).unwrap();
        }
        if std::fs::write(ro.join("probe"), b"x").is_err() {
            let transport = Arc::new(MockTransport::default());
            let guard = Bugsee::launch_with(
                LaunchOptions::new("TOKEN_A")
                    .data_dir(&ro)
                    .with_transport(transport.clone()),
            )
            .expect("launch");
            let status = Bugsee::storage_status().expect("status");
            assert_eq!(status.mode, StorageMode::Memory);
            assert!(status.explicit_rejected.is_some());
            let start = std::time::Instant::now();
            while transport.uploaded_bundles.lock().unwrap().is_empty()
                && start.elapsed() < Duration::from_secs(10)
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            assert_eq!(
                transport.uploaded_bundles.lock().unwrap().len(),
                1,
                "the stranded report must be delivered"
            );
            drop(guard);
        }
        for d in [ro.join("queue"), ro.clone()] {
            let _ = std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o755));
        }
    }

    let _ = std::fs::remove_dir_all(&s);
}
