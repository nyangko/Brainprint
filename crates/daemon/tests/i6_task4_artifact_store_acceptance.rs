//! #53 step 2 acceptance: the daemon-wide ephemeral artifact store's
//! lifecycle against real `Server` binds -- purge only after singleton
//! ownership, clean-shutdown removal, restart expiry, and nothing ever
//! written into a Workspace or the global data directory.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{self, Command},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use brainprint_core::protocol::{self, InitRequest, Request, Response};
use brainprint_daemon::{
    artifacts::{ArtifactError, ArtifactPart, ArtifactStore, ArtifactStream},
    client, runtime_paths,
    server::{Server, StartError},
};
use brainprint_engine::{
    output_capture::{HeadTail, RetainedStream},
    paths::GlobalPaths,
};

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn create(label: &str) -> Self {
        let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "brainprint-i6-task4-{label}-{}-{sequence}",
            process::id()
        ));
        fs::create_dir_all(&path).expect("test dir should be created");
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn artifact_dir(global_paths: &GlobalPaths) -> PathBuf {
    global_paths
        .runtime_root()
        .join("artifacts")
        .join("verification")
}

fn entries(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut found: Vec<_> = fs::read_dir(dir)
        .expect("artifact dir should list")
        .map(|entry| {
            let entry = entry.expect("entry");
            (
                entry.file_name().to_string_lossy().into_owned(),
                fs::read(entry.path()).expect("payload should read"),
            )
        })
        .collect();
    found.sort();
    found
}

fn retained(bytes: &[u8]) -> RetainedStream {
    let mut collector = HeadTail::default();
    collector.push(bytes);
    collector.finish()
}

fn insert(store: &ArtifactStore, stdout: &[u8]) -> String {
    store
        .insert(&retained(stdout), &retained(b""))
        .expect("insert should succeed")
        .handle
}

fn read_head(store: &ArtifactStore, handle: &str) -> Result<Vec<u8>, ArtifactError> {
    store.read_part(handle, ArtifactStream::Stdout, ArtifactPart::Head, 0, 1024)
}

#[tokio::test]
async fn a_successful_startup_purges_what_a_crash_left() {
    let home = TestDir::create("purge");
    let global_paths = GlobalPaths::from_home(&home.0);
    let dir = artifact_dir(&global_paths);
    fs::create_dir_all(&dir).expect("stale dir");
    fs::write(dir.join("stale-file"), b"left by a crash").expect("stale file");

    let server = Server::bind(&global_paths).await.expect("bind");

    assert!(dir.is_dir());
    assert!(entries(&dir).is_empty(), "stale artifact survived startup");
    let accounting = server.query_runtime().artifacts().accounting();
    assert_eq!(
        (accounting.completed_count, accounting.completed_bytes),
        (0, 0)
    );
    server.cleanup();
}

#[tokio::test]
async fn a_second_daemon_never_touches_the_live_daemons_artifacts() {
    let home = TestDir::create("second");
    let global_paths = GlobalPaths::from_home(&home.0);
    let mut first = Server::bind(&global_paths).await.expect("first bind");
    let runtime = first.query_runtime();
    let handle = insert(runtime.artifacts(), b"first daemon output");
    let dir = artifact_dir(&global_paths);
    let before = entries(&dir);
    assert_eq!(before.len(), 1);

    let serve_task = tokio::spawn(async move {
        let _ = first.serve().await;
    });
    tokio::time::sleep(Duration::from_millis(20)).await;

    let error = Server::bind(&global_paths)
        .await
        .expect_err("a second instance must not bind");
    assert!(matches!(error, StartError::AlreadyRunning));

    assert_eq!(
        entries(&dir),
        before,
        "second startup changed live artifacts"
    );
    assert_eq!(
        read_head(runtime.artifacts(), &handle).expect("still readable"),
        b"first daemon output"
    );
    serve_task.abort();
}

#[tokio::test]
async fn cleanup_removes_the_store_and_a_restart_expires_old_handles() {
    let home = TestDir::create("restart");
    let global_paths = GlobalPaths::from_home(&home.0);
    let dir = artifact_dir(&global_paths);

    let first = Server::bind(&global_paths).await.expect("first bind");
    let old = insert(first.query_runtime().artifacts(), b"before cleanup");
    first.cleanup();
    assert!(!dir.exists(), "clean shutdown left the artifact directory");
    assert_eq!(
        read_head(first.query_runtime().artifacts(), &old),
        Err(ArtifactError::Unavailable)
    );
    drop(first);
    // Windows: dropping a listener aborts its pipe-instance tasks; the
    // pipe name is free only once the runtime has dropped them.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // A crash (no cleanup) leaves files; the next start purges them.
    let crashed = Server::bind(&global_paths).await.expect("second bind");
    let left = insert(crashed.query_runtime().artifacts(), b"left by a crash");
    drop(crashed);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(entries(&dir).len(), 1);

    let next = Server::bind(&global_paths).await.expect("third bind");
    let runtime = next.query_runtime();
    let store = runtime.artifacts();
    for handle in [&old, &left] {
        assert_eq!(read_head(store, handle), Err(ArtifactError::Unavailable));
    }
    assert!(entries(&dir).is_empty());
    next.cleanup();
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "brainprint-test")
        .env("GIT_AUTHOR_EMAIL", "test@brainprint.invalid")
        .env("GIT_COMMITTER_NAME", "brainprint-test")
        .env("GIT_COMMITTER_EMAIL", "test@brainprint.invalid")
        .output()
        .expect("git must be installed for these tests");
    assert!(output.status.success(), "git {args:?}");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Every file under `root` whose name or bytes carry the artifact.
fn files_holding(root: &Path, marker: &[u8]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).expect("dir should list") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                pending.push(path);
            } else if path
                .extension()
                .is_some_and(|ext| ext == "payload" || ext == "tmp")
                || fs::read(&path)
                    .is_ok_and(|bytes| bytes.windows(marker.len()).any(|at| at == marker))
            {
                found.push(path);
            }
        }
    }
    found
}

#[tokio::test]
async fn artifacts_live_under_the_runtime_root_never_in_a_workspace() {
    let home = TestDir::create("workspace-home");
    let workspace = TestDir::create("workspace");
    let global_paths = GlobalPaths::from_home(&home.0);
    fs::write(workspace.0.join("app.rs"), "pub fn app() {}\n").expect("source");
    git(&workspace.0, &["init", "-q"]);
    git(&workspace.0, &["add", "."]);
    git(&workspace.0, &["commit", "-qm", "init"]);

    let mut server = Server::bind(&global_paths).await.expect("bind");
    let runtime = server.query_runtime();
    let endpoint = runtime_paths::resolve(&global_paths);
    let serve_task = tokio::spawn(async move {
        let _ = server.serve().await;
    });
    let mut connection = client::connect(&endpoint).await.expect("connect");
    client::handshake(&mut connection, "test-client")
        .await
        .expect("handshake");
    protocol::framing::write_message(
        &mut connection,
        &Request::Init(InitRequest {
            path: workspace.0.to_string_lossy().into_owned(),
        }),
    )
    .await
    .expect("write");
    let response: Response = protocol::framing::read_message(&mut connection)
        .await
        .expect("read");
    assert!(matches!(response, Response::Init(_)), "{response:?}");
    let status_before = git(&workspace.0, &["status", "--porcelain", "--ignored"]);

    let marker = b"RAW-ARTIFACT-MARKER-53-STEP-2";
    let handle = insert(runtime.artifacts(), marker);

    assert!(files_holding(&workspace.0, marker).is_empty());
    assert!(files_holding(&global_paths.data_dir, marker).is_empty());
    assert_eq!(
        git(&workspace.0, &["status", "--porcelain", "--ignored"]),
        status_before
    );
    let held = files_holding(&global_paths.runtime_root(), marker);
    assert_eq!(held.len(), 1);
    assert!(held[0].starts_with(artifact_dir(&global_paths)));
    assert_eq!(
        read_head(runtime.artifacts(), &handle).expect("read"),
        marker
    );

    serve_task.abort();
}
