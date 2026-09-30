use std::{
    env, process,
    sync::atomic::{AtomicU64, Ordering},
};

use brainprint_engine::output_capture::{HeadTail, MAX_HEAD, MAX_TAIL};

use super::*;

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn create() -> Self {
        let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let path =
            env::temp_dir().join(format!("brainprint-artifacts-{}-{sequence}", process::id()));
        fs::create_dir_all(&path).expect("test dir should be created");
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn open_store(dir: &TestDir, limits: ArtifactLimits) -> ArtifactStore {
    let store = ArtifactStore::with_limits(&dir.0, limits);
    store
        .initialize_after_lock()
        .expect("store should initialize");
    store
}

/// Never periodic at a power of two, so a wrong offset shows.
fn pattern(i: u64, seed: u8) -> u8 {
    ((i % 251) as u8).wrapping_add(seed)
}

fn stream(total: u64, seed: u8) -> RetainedStream {
    let mut collector = HeadTail::default();
    let mut chunk = vec![0_u8; 64 * 1024 + 7];
    let mut at = 0;
    while at < total {
        let size = chunk.len().min((total - at) as usize);
        for (offset, byte) in chunk[..size].iter_mut().enumerate() {
            *byte = pattern(at + offset as u64, seed);
        }
        collector.push(&chunk[..size]);
        at += size as u64;
    }
    collector.finish()
}

fn read_all(
    store: &ArtifactStore,
    handle: &str,
    stream: ArtifactStream,
    part: ArtifactPart,
) -> Vec<u8> {
    // Odd page size: continuation must restore the part byte for byte.
    const PAGE: usize = 512 * 1024 + 3;
    let mut all = Vec::new();
    loop {
        let page = store
            .read_part(handle, stream, part, all.len() as u64, PAGE)
            .expect("read should succeed");
        if page.is_empty() {
            return all;
        }
        all.extend_from_slice(&page);
    }
}

fn available(store: &ArtifactStore, handle: &str) -> bool {
    match store.read_part(handle, ArtifactStream::Stdout, ArtifactPart::Head, 0, 1) {
        Ok(_) => true,
        Err(ArtifactError::Unavailable) => false,
        Err(other) => panic!("unexpected {other:?}"),
    }
}

fn root_entries(store: &ArtifactStore) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(&store.root)
        .expect("root should list")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

const SMALL: ArtifactLimits = ArtifactLimits {
    max_bytes: 1_000,
    max_artifacts: 3,
};

#[test]
fn production_bounds_are_the_issue_contract() {
    assert_eq!(MAX_RETAINED_BYTES, 64 * 1024 * 1024);
    assert_eq!(MAX_ARTIFACTS, 64);
    assert_eq!(MAX_COMMAND_BYTES, 16 * 1024 * 1024);
    assert_eq!(ArtifactLimits::PRODUCTION.max_bytes, MAX_RETAINED_BYTES);
    assert_eq!(ArtifactLimits::PRODUCTION.max_artifacts, MAX_ARTIFACTS);

    let dir = TestDir::create();
    let store = open_store(&dir, ArtifactLimits::PRODUCTION);
    assert_eq!(
        store.reserve(MAX_COMMAND_BYTES + 1).unwrap_err(),
        ArtifactError::OverCommandLimit
    );
    // More than was reserved is not accepted either.
    let reservation = store.reserve(10).expect("reserve");
    assert_eq!(
        reservation
            .commit(&stream(11, 0), &stream(0, 0))
            .unwrap_err(),
        ArtifactError::OverCommandLimit
    );
    assert_eq!(store.accounting(), Accounting::default());
}

#[test]
fn a_new_store_touches_nothing_until_initialized() {
    let dir = TestDir::create();
    let stale = dir.0.join("artifacts").join("verification");
    fs::create_dir_all(&stale).expect("stale dir");
    fs::write(stale.join("stale-file"), b"left by a crash").expect("stale file");

    let store = ArtifactStore::new(&dir.0);
    assert!(stale.join("stale-file").is_file(), "constructor purged");
    assert_eq!(store.reserve(1).unwrap_err(), ArtifactError::Unavailable);

    store.initialize_after_lock().expect("initialize");
    assert!(root_entries(&store).is_empty(), "stale file survived");
    assert_eq!(store.accounting(), Accounting::default());
}

#[cfg(unix)]
#[test]
fn directory_is_0700_and_payload_0600() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = TestDir::create();
    let store = open_store(&dir, SMALL);
    store
        .insert(&stream(10, 1), &stream(10, 2))
        .expect("insert");
    let mode = |path: &Path| fs::metadata(path).expect("metadata").permissions().mode() & 0o777;
    assert_eq!(mode(&store.root), 0o700);
    let entries = root_entries(&store);
    assert_eq!(entries.len(), 1);
    assert_eq!(mode(&store.root.join(&entries[0])), 0o600);
}

#[test]
fn head_and_tail_are_byte_exact_with_step_1_metadata() {
    let dir = TestDir::create();
    let store = open_store(&dir, ArtifactLimits::PRODUCTION);
    let whole = stream(3 * 1024 * 1024 + 5, 7);
    let truncated = stream(9 * 1024 * 1024 + 12_345, 11);

    for (stdout, stderr) in [(&whole, &truncated), (&truncated, &whole)] {
        let stored = store.insert(stdout, stderr).expect("insert");
        assert_eq!(stored.stdout, StreamMeta::of(stdout));
        assert_eq!(stored.stderr, StreamMeta::of(stderr));
        for (which, original) in [
            (ArtifactStream::Stdout, stdout),
            (ArtifactStream::Stderr, stderr),
        ] {
            let head = read_all(&store, &stored.handle, which, ArtifactPart::Head);
            let tail = read_all(&store, &stored.handle, which, ArtifactPart::Tail);
            assert!(head == original.head(), "{which:?} head differs");
            assert!(tail == original.tail(), "{which:?} tail differs");
        }
        // A head read asking for more never runs on into the tail.
        let head = store
            .read_part(
                &stored.handle,
                ArtifactStream::Stdout,
                ArtifactPart::Head,
                0,
                usize::MAX,
            )
            .expect("read");
        assert_eq!(head.len() as u64, stored.stdout.head_bytes);
    }

    assert!(!whole.truncated && whole.tail().is_empty());
    assert!(truncated.truncated);
    assert_eq!(
        (truncated.head().len(), truncated.tail().len()),
        (MAX_HEAD, MAX_TAIL)
    );
    let bytes = 2 * (whole.head().len() + truncated.head().len() + truncated.tail().len()) as u64;
    assert_eq!(store.accounting().completed_bytes, bytes);
}

#[test]
fn a_read_makes_an_artifact_most_recently_used() {
    let dir = TestDir::create();
    let store = open_store(&dir, SMALL);
    let insert = |seed| {
        store
            .insert(&stream(10, seed), &stream(0, 0))
            .expect("insert")
            .handle
    };
    let (a, b, c) = (insert(1), insert(2), insert(3));
    assert!(available(&store, &a));
    let d = insert(4);

    assert!(!available(&store, &b), "b was least recently used");
    for handle in [&a, &c, &d] {
        assert!(available(&store, handle));
    }
    assert_eq!(root_entries(&store).len(), 3);
    assert_eq!(store.accounting().completed_count, 3);
}

#[test]
fn the_65th_completed_artifact_evicts_the_oldest() {
    let dir = TestDir::create();
    let store = open_store(&dir, ArtifactLimits::PRODUCTION);
    let handles: Vec<String> = (0..=MAX_ARTIFACTS)
        .map(|seed| {
            store
                .insert(&stream(1, seed as u8), &stream(1, 0))
                .expect("insert")
                .handle
        })
        .collect();

    assert!(!available(&store, &handles[0]));
    assert!(handles[1..].iter().all(|handle| available(&store, handle)));
    assert_eq!(store.accounting().completed_count, MAX_ARTIFACTS);
    assert_eq!(store.accounting().completed_bytes, 2 * MAX_ARTIFACTS as u64);
    assert_eq!(root_entries(&store).len(), MAX_ARTIFACTS);
}

#[test]
fn the_byte_bound_evicts_oldest_first_and_keeps_the_rest_exact() {
    let dir = TestDir::create();
    let store = open_store(
        &dir,
        ArtifactLimits {
            max_bytes: 100,
            max_artifacts: 10,
        },
    );
    let a = store.insert(&stream(30, 1), &stream(10, 2)).expect("a");
    let b = store.insert(&stream(20, 3), &stream(20, 4)).expect("b");
    let c_out = stream(25, 5);
    let c = store.insert(&c_out, &stream(5, 6)).expect("c");

    assert!(!available(&store, &a.handle));
    assert_eq!(
        store.accounting(),
        Accounting {
            completed_count: 2,
            completed_bytes: 70,
            reserved_count: 0,
            reserved_bytes: 0,
        }
    );
    assert_eq!(
        read_all(
            &store,
            &b.handle,
            ArtifactStream::Stderr,
            ArtifactPart::Head
        ),
        stream(20, 4).head()
    );
    assert_eq!(
        read_all(
            &store,
            &c.handle,
            ArtifactStream::Stdout,
            ArtifactPart::Head
        ),
        c_out.head()
    );
}

#[test]
fn four_maximal_reservations_fit_and_a_fifth_fails_at_once() {
    let dir = TestDir::create();
    let store = open_store(&dir, ArtifactLimits::PRODUCTION);
    let completed = store
        .insert(&stream(10, 1), &stream(10, 2))
        .expect("completed")
        .handle;

    let held: Vec<_> = (0..4)
        .map(|_| store.reserve(MAX_COMMAND_BYTES).expect("reserve"))
        .collect();
    // Room for the fourth came from the completed artifact, not a wait.
    assert!(!available(&store, &completed));
    let full = store.accounting();
    assert_eq!(full.reserved_count, 4);
    assert_eq!(full.reserved_bytes, MAX_RETAINED_BYTES);
    assert_eq!(
        full.completed_bytes + full.reserved_bytes,
        MAX_RETAINED_BYTES
    );

    assert_eq!(
        store.reserve(MAX_COMMAND_BYTES).unwrap_err(),
        ArtifactError::CapacityUnavailable
    );
    assert_eq!(
        store.reserve(1).unwrap_err(),
        ArtifactError::CapacityUnavailable
    );
    assert_eq!(store.accounting(), full, "a failed reservation leaked");

    // The in-flight ones were untouched and still commit.
    let mut held = held.into_iter();
    let first = held.next().expect("held");
    let stored = first
        .commit(&stream(100, 3), &stream(0, 0))
        .expect("commit");
    assert!(available(&store, &stored.handle));
    drop(held);
    assert_eq!(
        store.accounting(),
        Accounting {
            completed_count: 1,
            completed_bytes: 100,
            reserved_count: 0,
            reserved_bytes: 0,
        }
    );
}

#[test]
fn the_count_bound_counts_reservations_too() {
    let dir = TestDir::create();
    let store = open_store(&dir, SMALL);
    let _held: Vec<_> = (0..3).map(|_| store.reserve(1).expect("reserve")).collect();
    assert_eq!(
        store.reserve(1).unwrap_err(),
        ArtifactError::CapacityUnavailable
    );
}

#[test]
fn a_failed_write_publishes_nothing_and_leaks_nothing() {
    let dir = TestDir::create();
    let store = open_store(&dir, SMALL);
    let kept_out = stream(40, 1);
    let kept = store.insert(&kept_out, &stream(0, 0)).expect("kept");
    let before = store.accounting();

    store.state().fail_write = true;
    assert_eq!(
        store.insert(&stream(50, 2), &stream(50, 3)).unwrap_err(),
        ArtifactError::Io(ArtifactIoOp::Write, io::ErrorKind::Other)
    );

    assert_eq!(store.accounting(), before);
    assert_eq!(store.state().completed.len(), 1);
    assert_eq!(store.state().lru.len(), 1);
    assert_eq!(root_entries(&store).len(), 1, "temp file left behind");
    assert_eq!(
        read_all(
            &store,
            &kept.handle,
            ArtifactStream::Stdout,
            ArtifactPart::Head
        ),
        kept_out.head()
    );
}

#[test]
fn a_handle_is_only_ever_looked_up_never_a_path() {
    let dir = TestDir::create();
    let store = open_store(&dir, SMALL);
    fs::write(store.root.join("evil"), b"not an artifact").expect("decoy");
    fs::write(dir.0.join("artifact"), b"not an artifact").expect("decoy");
    store.insert(&stream(10, 1), &stream(0, 0)).expect("insert");
    let before = store.state().file_opens;

    for handle in [
        "../../etc/passwd",
        "../artifact",
        "evil",
        "0.payload",
        "/absolute/path",
        "C:\\foo",
        &uuid::Uuid::new_v4().to_string(),
    ] {
        for stream in [ArtifactStream::Stdout, ArtifactStream::Stderr] {
            for part in [ArtifactPart::Head, ArtifactPart::Tail] {
                assert_eq!(
                    store.read_part(handle, stream, part, 0, 16).unwrap_err(),
                    ArtifactError::Unavailable,
                    "{handle}"
                );
            }
        }
    }
    assert_eq!(
        store.state().file_opens,
        before,
        "a handle reached the filesystem"
    );
}

#[test]
fn shutdown_and_restart_expire_every_handle() {
    let dir = TestDir::create();
    let store = open_store(&dir, SMALL);
    let old = store
        .insert(&stream(10, 1), &stream(0, 0))
        .expect("insert")
        .handle;
    let in_flight = store.reserve(10).expect("reserve");

    store.shutdown();
    assert!(!store.root.exists());
    assert!(!available(&store, &old));
    assert_eq!(
        in_flight.commit(&stream(10, 2), &stream(0, 0)).unwrap_err(),
        ArtifactError::Unavailable
    );
    assert_eq!(store.accounting(), Accounting::default());

    // A crash leaves files; the next daemon's store purges them.
    let crashed = open_store(&dir, SMALL);
    let left = crashed
        .insert(&stream(10, 3), &stream(0, 0))
        .expect("insert")
        .handle;
    let next = open_store(&dir, SMALL);
    assert!(!available(&next, &left));
    assert!(root_entries(&next).is_empty());
}

#[test]
fn errors_name_no_path_and_no_output() {
    for error in [
        ArtifactError::Unavailable,
        ArtifactError::OverCommandLimit,
        ArtifactError::CapacityUnavailable,
        ArtifactError::Io(ArtifactIoOp::Read, io::ErrorKind::NotFound),
    ] {
        let text = error.to_string();
        assert!(!text.contains('/') && !text.contains('\\'), "{text}");
    }
    let dir = TestDir::create();
    let debug = format!("{:?}", open_store(&dir, SMALL));
    assert!(!debug.contains(&*dir.0.to_string_lossy()), "{debug}");
}
