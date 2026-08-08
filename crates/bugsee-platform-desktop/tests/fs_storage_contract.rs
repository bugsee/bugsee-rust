//! FsStorage must satisfy the same contract as MemoryStorage.

use bugsee_platform::{Storage, StorageError, StoragePath};
use bugsee_platform_desktop::FsStorage;

fn p(s: &str) -> StoragePath {
    StoragePath::new(s)
}

fn assert_storage_contract(store: &dyn Storage) {
    store.create_dir_all(&p("parts/1")).unwrap();
    assert!(store.exists(&p("parts")));
    assert!(store.exists(&p("parts/1")));

    store
        .write_append(&p("parts/1/events.part"), b"hello")
        .unwrap();
    store
        .write_append(&p("parts/1/events.part"), b" world")
        .unwrap();
    assert_eq!(
        store.read_file(&p("parts/1/events.part")).unwrap(),
        b"hello world"
    );

    store
        .write_file(&p("parts/1/tmp.marker"), b"ready")
        .unwrap();
    store
        .rename(&p("parts/1/tmp.marker"), &p("parts/1/final.marker"))
        .unwrap();
    assert!(!store.exists(&p("parts/1/tmp.marker")));
    assert_eq!(
        store.read_file(&p("parts/1/final.marker")).unwrap(),
        b"ready"
    );

    let caps = store.probe_caps();
    if caps.hardlink {
        store
            .hard_link(&p("parts/1/events.part"), &p("snap/events.part"))
            .unwrap();
        assert_eq!(
            store.read_file(&p("snap/events.part")).unwrap(),
            b"hello world"
        );
        store
            .write_append(&p("parts/1/events.part"), b"!")
            .unwrap();
        assert_eq!(
            store.read_file(&p("snap/events.part")).unwrap(),
            b"hello world!"
        );
    } else {
        // Volume without hardlinks: hard_link must fail with Unsupported or Other.
        let err = store
            .hard_link(&p("parts/1/events.part"), &p("snap/events.part"))
            .unwrap_err();
        assert!(
            matches!(err, StorageError::Unsupported | StorageError::Other(_)),
            "unexpected {err:?}"
        );
    }

    // Reflink may be unsupported on this volume — that's OK.
    let _ = store.reflink_or_clone(&p("parts/1/events.part"), &p("snap/rl.part"));

    store.remove_file(&p("parts/1/final.marker")).unwrap();
    store
        .create_file_exclusive(&p("parts/1/excl.marker"), b"one")
        .unwrap();
    assert_eq!(
        store.read_file(&p("parts/1/excl.marker")).unwrap(),
        b"one"
    );
    assert!(matches!(
        store.create_file_exclusive(&p("parts/1/excl.marker"), b"two"),
        Err(StorageError::AlreadyExists)
    ));
    assert!(store
        .metadata(&p("parts/1/excl.marker"))
        .unwrap()
        .modified_unix_ms
        .is_some());
    {
        let mut sink = store.open_append(&p("parts/1/append.part")).unwrap();
        sink.write_all(b"a").unwrap();
        sink.flush().unwrap();
    }
    if store.probe_caps().hardlink {
        store.write_file(&p("parts/1/shared.bin"), b"v1").unwrap();
        store
            .hard_link(&p("parts/1/shared.bin"), &p("snap/shared.bin"))
            .unwrap();
        store.write_file(&p("parts/1/shared.bin"), b"v2").unwrap();
        assert_eq!(store.read_file(&p("snap/shared.bin")).unwrap(), b"v2");
    }
    store.remove_dir_all(&p("parts")).unwrap();
}

#[test]
fn fs_storage_satisfies_contract() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStorage::new(dir.path()).unwrap();
    assert_storage_contract(&store);
}
