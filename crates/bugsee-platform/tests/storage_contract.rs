//! Storage contract suite — runs against [`MemoryStorage`].
//!
//! The same behaviors are required of `bugsee-platform-desktop::FsStorage`
//! (see that crate's tests). TDD gate for `DESIGN_PLATFORM.md` §6.1.

use bugsee_platform::{
    memory::MemoryStorage, Clock, Entropy, HttpError, HttpRequest, HttpTransport, Storage,
    StorageError, StoragePath, SystemClock, SystemEntropy,
};

fn p(s: &str) -> StoragePath {
    StoragePath::new(s)
}

fn assert_storage_contract(store: &dyn Storage) {
    // --- directories & files ---
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
    let meta = store.metadata(&p("parts/1/events.part")).unwrap();
    assert!(meta.is_file);
    assert_eq!(meta.len, 11);

    // --- rename as durability primitive ---
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

    // --- read_dir ---
    let mut kids = store.read_dir(&p("parts/1")).unwrap();
    kids.sort();
    assert!(kids.contains(&"events.part".into()));
    assert!(kids.contains(&"final.marker".into()));

    // --- hardlink: shared content ---
    let caps = store.probe_caps();
    if caps.hardlink {
        store
            .hard_link(&p("parts/1/events.part"), &p("snap/events.part"))
            .unwrap();
        assert_eq!(
            store.read_file(&p("snap/events.part")).unwrap(),
            b"hello world"
        );
        // Append via original must be visible through hardlink (shared blob).
        store
            .write_append(&p("parts/1/events.part"), b"!")
            .unwrap();
        assert_eq!(
            store.read_file(&p("snap/events.part")).unwrap(),
            b"hello world!"
        );
    }

    // --- reflink: at least succeeds when advertised ---
    if caps.reflink {
        store
            .write_file(&p("parts/1/a.bin"), b"abc")
            .unwrap();
        store
            .reflink_or_clone(&p("parts/1/a.bin"), &p("snap/a.bin"))
            .unwrap();
        assert_eq!(store.read_file(&p("snap/a.bin")).unwrap(), b"abc");
    }

    // --- symlink optional ---
    if caps.symlink {
        store
            .write_file(&p("parts/1/target.bin"), b"tgt")
            .unwrap();
        store
            .symlink(&p("parts/1/target.bin"), &p("snap/link.bin"))
            .unwrap();
        assert_eq!(store.read_file(&p("snap/link.bin")).unwrap(), b"tgt");
    }

    // --- remove ---
    store.remove_file(&p("parts/1/final.marker")).unwrap();
    assert!(!store.exists(&p("parts/1/final.marker")));
    assert!(matches!(
        store.read_file(&p("parts/1/final.marker")),
        Err(StorageError::NotFound)
    ));

    // --- exclusive create ---
    store
        .create_file_exclusive(&p("parts/1/excl.marker"), b"one")
        .unwrap();
    assert_eq!(
        store.read_file(&p("parts/1/excl.marker")).unwrap(),
        b"one",
        "exclusive create must persist the provided payload"
    );
    assert!(matches!(
        store.create_file_exclusive(&p("parts/1/excl.marker"), b"two"),
        Err(StorageError::AlreadyExists)
    ));
    let meta = store.metadata(&p("parts/1/excl.marker")).unwrap();
    assert!(meta.modified_unix_ms.is_some());

    // --- open_append ---
    {
        let mut sink = store.open_append(&p("parts/1/append.part")).unwrap();
        sink.write_all(b"a").unwrap();
        sink.write_all(b"b").unwrap();
        sink.flush().unwrap();
    }
    assert_eq!(store.read_file(&p("parts/1/append.part")).unwrap(), b"ab");

    // --- write_file through a hardlink shares the blob ---
    if store.probe_caps().hardlink {
        store.write_file(&p("parts/1/shared.bin"), b"v1").unwrap();
        store
            .hard_link(&p("parts/1/shared.bin"), &p("snap/shared.bin"))
            .unwrap();
        store.write_file(&p("parts/1/shared.bin"), b"v2").unwrap();
        assert_eq!(store.read_file(&p("snap/shared.bin")).unwrap(), b"v2");
    }

    store.remove_dir_all(&p("parts")).unwrap();
    assert!(!store.exists(&p("parts/1")));
}

#[test]
fn memory_storage_satisfies_contract() {
    assert_storage_contract(&MemoryStorage::new());
}

#[test]
fn storage_path_rejects_escape() {
    assert!(StoragePath::try_new("../x").is_none());
    assert!(StoragePath::try_new("/abs").is_none());
}

#[test]
fn system_clock_mono_nondecreasing() {
    let c = SystemClock;
    let a = c.mono_time_ms();
    let b = c.mono_time_ms();
    assert!(b >= a);
    assert!(c.unix_time_ms() > 0);
}

#[test]
fn system_entropy_fills() {
    let e = SystemEntropy;
    let mut a = [0u8; 32];
    let mut b = [0u8; 32];
    e.fill(&mut a).unwrap();
    e.fill(&mut b).unwrap();
    // Astronomically unlikely to be equal if entropy works.
    assert_ne!(a, b);
}

/// Contract: NotReady/Suspended are distinct from Transient for uploader policy.
struct ScriptedHttp {
    next: std::sync::Mutex<Vec<Result<bugsee_platform::HttpResponse, HttpError>>>,
}

impl HttpTransport for ScriptedHttp {
    fn request(
        &self,
        _req: HttpRequest<'_>,
    ) -> Result<bugsee_platform::HttpResponse, HttpError> {
        let mut q = self.next.lock().unwrap();
        q.pop().unwrap_or(Err(HttpError::Permanent("empty".into())))
    }
}

#[test]
fn http_error_taxonomy_distinguishes_lifecycle() {
    let http = ScriptedHttp {
        next: std::sync::Mutex::new(vec![
            Err(HttpError::Suspended),
            Err(HttpError::NotReady),
            Ok(bugsee_platform::HttpResponse {
                status: 200,
                headers: vec![],
                body: b"ok".to_vec(),
            }),
        ]),
    };
    let mk = || HttpRequest {
        method: "GET",
        url: "https://example.test/",
        headers: &[],
        body: &[],
    };
    // Vec pop — last pushed first
    assert!(matches!(http.request(mk()), Ok(_)));
    assert!(matches!(http.request(mk()), Err(HttpError::NotReady)));
    assert!(matches!(http.request(mk()), Err(HttpError::Suspended)));
}
