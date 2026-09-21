//! I1 (#15) end-to-end acceptance tests.
//!
//! This is I1 acceptance *infrastructure* (#15 task 12), not new product
//! behavior: it drives the real `brainprintd`/`brainprint` binaries over
//! real local IPC against a real filesystem (and, where relevant, a real
//! `git worktree`), rather than calling engine/daemon APIs in-process the
//! way tasks 1-11's unit/integration tests already do. Those unit tests
//! are not re-listed here; this file exists to prove the *whole stack*
//! (CLI -> core protocol -> daemon -> engine) actually holds together for
//! the representative scenarios #15's completion criteria name.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{self, Child, Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

use brainprint_core::ProjectId;
use brainprint_engine::{
    generation::{GenerationState, GenerationStore},
    paths::WorkspacePaths,
    schema,
};

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

fn temp_dir(label: &str) -> PathBuf {
    let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
    let path = env::temp_dir().join(format!(
        "brainprint-i1-accept-{label}-{}-{sequence}",
        process::id()
    ));
    fs::create_dir_all(&path).expect("scratch directory should be created");
    path
}

/// A scratch directory (daemon `$HOME` or a Workspace root) removed on drop.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn create(label: &str) -> Self {
        Self(temp_dir(label))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn daemon_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_brainprintd"))
}

/// `brainprint` always lands next to `brainprintd` in the same target
/// directory -- there is no `CARGO_BIN_EXE_*` for another package's
/// binary, so this derives the sibling path directly instead.
fn cli_bin() -> PathBuf {
    let mut path = daemon_bin();
    let name = if cfg!(windows) {
        "brainprint.exe"
    } else {
        "brainprint"
    };
    path.set_file_name(name);
    assert!(
        path.is_file(),
        "the brainprint CLI binary must be built alongside brainprintd -- \
         run `cargo build --workspace` (or `cargo test --workspace`) first: {}",
        path.display()
    );
    path
}

/// A running `brainprintd` child. Dropping it kills the process
/// non-gracefully (`SIGKILL` on Unix, `TerminateProcess` on Windows) --
/// deliberately, since several scenarios here want to prove recovery from
/// exactly that (a crash), not a clean shutdown.
struct DaemonGuard(Child);

impl DaemonGuard {
    fn spawn(home: &Path) -> Self {
        let child = Command::new(daemon_bin())
            .env("HOME", home)
            .env("USERPROFILE", home)
            // Real `brainprintd` resolves its home via
            // `GlobalPaths::discover()`, which -- correctly, for real
            // single-daemon-per-user usage -- still prefers
            // `$XDG_RUNTIME_DIR` over the resolved home. Left set, every
            // test process on a CI runner that exports it (common on
            // Linux) would collide on the one real session runtime
            // directory instead of this test's isolated scratch `home`.
            .env_remove("XDG_RUNTIME_DIR")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("brainprintd should spawn");
        Self(child)
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn run_cli(home: &Path, args: &[&str]) -> Output {
    Command::new(cli_bin())
        .args(args)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env_remove("XDG_RUNTIME_DIR")
        .output()
        .expect("brainprint should run")
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Poll `brainprint status` until the daemon answers, instead of racing a
/// fixed sleep against daemon startup.
fn wait_for_daemon_ready(home: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let output = run_cli(home, &["status"]);
        if output.status.success() {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "daemon never became ready within the timeout: {}",
                stderr_of(&output)
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Pull `"  <label> <value>"` out of `brainprint init`/`status` output.
fn field<'a>(text: &'a str, label: &str) -> &'a str {
    text.lines()
        .find_map(|line| line.trim().strip_prefix(label))
        .map(str::trim)
        .unwrap_or_else(|| panic!("expected a {label:?} line in:\n{text}"))
}

fn run_git(args: &[&str], cwd: &Path) {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "brainprint-acceptance")
        .env("GIT_AUTHOR_EMAIL", "acceptance@brainprint.invalid")
        .env("GIT_COMMITTER_NAME", "brainprint-acceptance")
        .env("GIT_COMMITTER_EMAIL", "acceptance@brainprint.invalid")
        .output()
        .expect("git should be installed and runnable for acceptance tests");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn real_git_repo(workspace: &Path) {
    run_git(&["init", "-q"], workspace);
    run_git(&["commit", "-q", "--allow-empty", "-m", "init"], workspace);
}

fn add_worktree(main_repo: &Path, worktree: &Path, branch: &str) {
    run_git(
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            branch,
            &worktree.to_string_lossy(),
        ],
        main_repo,
    );
}

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).expect("path should canonicalize")
}

// --- fresh non-Git ---------------------------------------------------

#[test]
fn fresh_non_git_install_init_restart_preserve_identity() {
    let home = ScratchDir::create("non-git-home");
    let workspace = ScratchDir::create("non-git-ws");

    let daemon = DaemonGuard::spawn(home.path());
    wait_for_daemon_ready(home.path());

    let install = run_cli(home.path(), &["install"]);
    assert!(install.status.success(), "{}", stderr_of(&install));
    assert!(stdout_of(&install).contains("installed"));

    let init = run_cli(home.path(), &["init", &workspace.path().to_string_lossy()]);
    assert!(init.status.success(), "{}", stderr_of(&init));
    let init_out = stdout_of(&init);
    assert!(init_out.starts_with("initialized:"));
    assert_eq!(field(&init_out, "git:"), "false");
    let project_id = field(&init_out, "project:").to_owned();
    let workspace_id = field(&init_out, "workspace:").to_owned();

    // Simulate a daemon restart (non-graceful, worst case).
    drop(daemon);
    let restarted = DaemonGuard::spawn(home.path());
    wait_for_daemon_ready(home.path());

    let reinit = run_cli(home.path(), &["init", &workspace.path().to_string_lossy()]);
    assert!(reinit.status.success(), "{}", stderr_of(&reinit));
    let reinit_out = stdout_of(&reinit);
    assert!(reinit_out.starts_with("already initialized:"));
    assert_eq!(field(&reinit_out, "project:"), project_id);
    assert_eq!(field(&reinit_out, "workspace:"), workspace_id);

    drop(restarted);
}

// --- fresh Git ---------------------------------------------------------

#[test]
fn fresh_git_install_init_restart_preserves_project_home() {
    let home = ScratchDir::create("git-home");
    let workspace = ScratchDir::create("git-ws");
    real_git_repo(workspace.path());

    let daemon = DaemonGuard::spawn(home.path());
    wait_for_daemon_ready(home.path());
    run_cli(home.path(), &["install"]);

    let init = run_cli(home.path(), &["init", &workspace.path().to_string_lossy()]);
    assert!(init.status.success(), "{}", stderr_of(&init));
    let init_out = stdout_of(&init);
    assert_eq!(field(&init_out, "git:"), "true");
    let project_id = field(&init_out, "project:").to_owned();
    let workspace_id = field(&init_out, "workspace:").to_owned();

    let workspace_paths = WorkspacePaths::from_root(canonical(workspace.path()));
    assert!(
        workspace_paths.project_db.is_file(),
        "project-home must own project.db"
    );

    drop(daemon);
    let restarted = DaemonGuard::spawn(home.path());
    wait_for_daemon_ready(home.path());

    let reinit = run_cli(home.path(), &["init", &workspace.path().to_string_lossy()]);
    assert!(reinit.status.success(), "{}", stderr_of(&reinit));
    let reinit_out = stdout_of(&reinit);
    assert_eq!(field(&reinit_out, "project:"), project_id);
    assert_eq!(field(&reinit_out, "workspace:"), workspace_id);
    assert!(workspace_paths.project_db.is_file());

    drop(restarted);
}

// --- Git worktree --------------------------------------------------------

#[test]
fn git_worktree_main_and_secondary_share_project_id_survive_restart() {
    let home = ScratchDir::create("worktree-home");
    let main_repo = ScratchDir::create("worktree-main");
    real_git_repo(main_repo.path());

    let daemon = DaemonGuard::spawn(home.path());
    wait_for_daemon_ready(home.path());
    run_cli(home.path(), &["install"]);

    let main_init = run_cli(home.path(), &["init", &main_repo.path().to_string_lossy()]);
    assert!(main_init.status.success(), "{}", stderr_of(&main_init));
    let main_out = stdout_of(&main_init);
    let main_project_id = field(&main_out, "project:").to_owned();
    let main_workspace_id = field(&main_out, "workspace:").to_owned();

    let secondary = ScratchDir::create("worktree-secondary");
    // `git worktree add` needs the target to not already exist as a
    // non-empty directory.
    fs::remove_dir(secondary.path()).expect("placeholder dir should be removable");
    add_worktree(main_repo.path(), secondary.path(), "feature-acceptance");

    let secondary_init = run_cli(home.path(), &["init", &secondary.path().to_string_lossy()]);
    assert!(
        secondary_init.status.success(),
        "{}",
        stderr_of(&secondary_init)
    );
    let secondary_out = stdout_of(&secondary_init);
    assert_eq!(field(&secondary_out, "project:"), main_project_id);
    assert_ne!(field(&secondary_out, "workspace:"), main_workspace_id);

    let main_paths = WorkspacePaths::from_root(canonical(main_repo.path()));
    let secondary_paths = WorkspacePaths::from_root(canonical(secondary.path()));
    assert!(main_paths.project_db.is_file());
    assert!(!secondary_paths.project_db.exists());
    assert!(secondary_paths.workspace_db.is_file());
    assert!(secondary_paths.index_db.is_file());

    // Restart, then reopen both -- the relationship must survive.
    drop(daemon);
    let restarted = DaemonGuard::spawn(home.path());
    wait_for_daemon_ready(home.path());

    let main_reinit = run_cli(home.path(), &["init", &main_repo.path().to_string_lossy()]);
    assert!(main_reinit.status.success());
    let main_reinit_out = stdout_of(&main_reinit);
    assert_eq!(field(&main_reinit_out, "project:"), main_project_id);
    assert_eq!(field(&main_reinit_out, "workspace:"), main_workspace_id);

    let secondary_reinit = run_cli(home.path(), &["init", &secondary.path().to_string_lossy()]);
    assert!(secondary_reinit.status.success());
    let secondary_reinit_out = stdout_of(&secondary_reinit);
    assert_eq!(field(&secondary_reinit_out, "project:"), main_project_id);
    assert_eq!(
        field(&secondary_reinit_out, "workspace:"),
        field(&secondary_out, "workspace:")
    );
    assert!(main_paths.project_db.is_file());
    assert!(!secondary_paths.project_db.exists());

    drop(restarted);
}

// --- duplicate identity --------------------------------------------------

#[test]
fn duplicate_workspace_id_at_different_live_locator_is_explicit_conflict() {
    let home = ScratchDir::create("duplicate-home");
    let original = ScratchDir::create("duplicate-original");
    real_git_repo(original.path());

    let daemon = DaemonGuard::spawn(home.path());
    wait_for_daemon_ready(home.path());
    run_cli(home.path(), &["install"]);

    let original_init = run_cli(home.path(), &["init", &original.path().to_string_lossy()]);
    assert!(original_init.status.success());

    // Copy `.brainprint/workspace.toml` (identity only, not the DBs) to a
    // second, still-live location -- the original is left untouched, so
    // this is a duplicate/copy, never a move.
    let copy = ScratchDir::create("duplicate-copy");
    real_git_repo(copy.path());
    let original_paths = WorkspacePaths::from_root(canonical(original.path()));
    let copy_paths = WorkspacePaths::from_root(canonical(copy.path()));
    fs::create_dir_all(&copy_paths.root).expect("copy .brainprint root should be created");
    fs::copy(&original_paths.identity_file, &copy_paths.identity_file)
        .expect("identity file should copy");

    let copy_init = run_cli(home.path(), &["init", &copy.path().to_string_lossy()]);
    assert!(
        !copy_init.status.success(),
        "the same WorkspaceID at a second live locator must never be silently merged"
    );
    assert!(
        stderr_of(&copy_init).contains("already registered"),
        "expected an explicit registry conflict, got: {}",
        stderr_of(&copy_init)
    );

    // The original registration must be completely unaffected.
    let original_status = run_cli(home.path(), &["init", &original.path().to_string_lossy()]);
    assert!(original_status.status.success());
    assert!(stdout_of(&original_status).starts_with("already initialized:"));

    drop(daemon);
}

// --- directory move --------------------------------------------------

#[test]
fn directory_move_repairs_locator_and_preserves_identity() {
    let home = ScratchDir::create("move-home");
    let original = ScratchDir::create("move-original");
    real_git_repo(original.path());

    let daemon = DaemonGuard::spawn(home.path());
    wait_for_daemon_ready(home.path());
    run_cli(home.path(), &["install"]);

    let init = run_cli(home.path(), &["init", &original.path().to_string_lossy()]);
    assert!(init.status.success());
    let init_out = stdout_of(&init);
    let project_id = field(&init_out, "project:").to_owned();
    let workspace_id = field(&init_out, "workspace:").to_owned();

    // A real `mv`: the old locator becomes genuinely unreachable.
    let new_path = temp_dir("move-target");
    fs::remove_dir(&new_path).expect("placeholder target dir should be removable");
    fs::rename(original.path(), &new_path).expect("directory move should succeed");

    let moved_init = run_cli(home.path(), &["init", &new_path.to_string_lossy()]);
    assert!(
        moved_init.status.success(),
        "a genuine directory move must repair the locator, not conflict: {}",
        stderr_of(&moved_init)
    );
    let moved_out = stdout_of(&moved_init);
    assert!(moved_out.starts_with("already initialized:"));
    assert_eq!(field(&moved_out, "project:"), project_id);
    assert_eq!(field(&moved_out, "workspace:"), workspace_id);

    let _ = fs::remove_dir_all(&new_path);
    drop(daemon);
}

// --- project-home missing --------------------------------------------

#[test]
fn project_home_missing_is_explicit_not_silently_recreated() {
    let home = ScratchDir::create("missing-home");
    let workspace = ScratchDir::create("missing-ws");

    let daemon = DaemonGuard::spawn(home.path());
    wait_for_daemon_ready(home.path());
    run_cli(home.path(), &["install"]);

    let init = run_cli(home.path(), &["init", &workspace.path().to_string_lossy()]);
    assert!(init.status.success());

    let workspace_paths = WorkspacePaths::from_root(canonical(workspace.path()));
    fs::remove_file(&workspace_paths.project_db)
        .expect("project.db fixture removal should succeed");

    let reinit = run_cli(home.path(), &["init", &workspace.path().to_string_lossy()]);
    assert!(
        !reinit.status.success(),
        "a missing project-home store must be rejected explicitly"
    );
    let message = stderr_of(&reinit);
    assert!(
        message.contains("missing") && message.contains("recovery"),
        "expected an explicit missing/recovery-required message, got: {message}"
    );
    assert!(
        !workspace_paths.project_db.exists(),
        "a missing project-home store must never be silently recreated"
    );

    drop(daemon);
}

// --- DB identity mismatch --------------------------------------------

#[test]
fn db_identity_mismatch_is_explicitly_rejected() {
    let home = ScratchDir::create("mismatch-home");
    let daemon = DaemonGuard::spawn(home.path());
    wait_for_daemon_ready(home.path());
    run_cli(home.path(), &["install"]);

    // Wrong project_uid bound in project.db.
    let workspace_a = ScratchDir::create("mismatch-project-uid");
    real_git_repo(workspace_a.path());
    let init_a = run_cli(
        home.path(),
        &["init", &workspace_a.path().to_string_lossy()],
    );
    assert!(init_a.status.success());
    let paths_a = WorkspacePaths::from_root(canonical(workspace_a.path()));
    {
        let opened = schema::project::open(&paths_a.project_db).expect("project.db should reopen");
        let other = ProjectId::generate();
        opened
            .connection
            .execute(
                "UPDATE db_meta SET project_uid = ?1 WHERE id = 0",
                [other.to_bytes().to_vec()],
            )
            .expect("tampering update should succeed");
    }
    let reinit_a = run_cli(
        home.path(),
        &["init", &workspace_a.path().to_string_lossy()],
    );
    assert!(
        !reinit_a.status.success(),
        "a project_uid mismatch must be rejected"
    );
    assert!(
        stderr_of(&reinit_a).contains("project.db is bound to project"),
        "expected an explicit project identity mismatch message, got: {}",
        stderr_of(&reinit_a)
    );

    // Wrong DbKind in workspace.db.
    let workspace_b = ScratchDir::create("mismatch-db-kind");
    let init_b = run_cli(
        home.path(),
        &["init", &workspace_b.path().to_string_lossy()],
    );
    assert!(init_b.status.success());
    let paths_b = WorkspacePaths::from_root(canonical(workspace_b.path()));
    {
        let opened =
            schema::workspace::open(&paths_b.workspace_db).expect("workspace.db should reopen");
        opened
            .connection
            .execute("UPDATE db_meta SET db_kind = ?1 WHERE id = 0", ["index"])
            .expect("tampering update should succeed");
    }
    let reinit_b = run_cli(
        home.path(),
        &["init", &workspace_b.path().to_string_lossy()],
    );
    assert!(
        !reinit_b.status.success(),
        "a wrong DbKind must be rejected"
    );
    // The daemon deliberately genericizes storage-level errors rather
    // than forwarding a raw driver message (#15 task 10) -- the
    // contractually-important fact is that it is rejected, not silently
    // accepted.
    assert!(
        stderr_of(&reinit_b).contains("internal storage error"),
        "expected an explicit (if generic) storage error, got: {}",
        stderr_of(&reinit_b)
    );

    drop(daemon);
}

// --- generation/restart --------------------------------------------

#[test]
fn generation_orphan_building_reconciled_stable_preserved_across_restart() {
    let home = ScratchDir::create("generation-home");
    let workspace = ScratchDir::create("generation-ws");

    let daemon = DaemonGuard::spawn(home.path());
    wait_for_daemon_ready(home.path());
    run_cli(home.path(), &["install"]);
    let init = run_cli(home.path(), &["init", &workspace.path().to_string_lossy()]);
    assert!(init.status.success());

    let workspace_paths = WorkspacePaths::from_root(canonical(workspace.path()));

    let (stable_id, orphan_id) = {
        let mut store =
            GenerationStore::open(&workspace_paths.index_db).expect("index.db should reopen");
        store
            .bootstrap_clock("rev-1")
            .expect("bootstrap should succeed");
        let stable = store
            .begin_generation("rev-1")
            .expect("begin should succeed");
        store
            .publish_stable(stable.id)
            .expect("publish should succeed");
        let orphan = store
            .begin_generation("rev-1")
            .expect("begin should succeed");
        // Left BUILDING here -- simulates a crash before publish/abort.
        (stable.id, orphan.id)
    };

    drop(daemon); // simulated crash
    let restarted = DaemonGuard::spawn(home.path());
    wait_for_daemon_ready(home.path());

    // Reopen via the real daemon -- this is exactly what reconciles the
    // orphan.
    let reinit = run_cli(home.path(), &["init", &workspace.path().to_string_lossy()]);
    assert!(reinit.status.success());

    let store = GenerationStore::open(&workspace_paths.index_db).expect("index.db should reopen");
    let current = store
        .current_stable()
        .expect("current lookup should succeed")
        .expect("the STABLE generation must survive the restart");
    assert_eq!(current.id, stable_id);

    let orphan_after = store
        .get_generation(orphan_id)
        .expect("lookup should succeed")
        .expect("the orphan generation should still exist, just no longer BUILDING");
    assert_eq!(orphan_after.state, GenerationState::Aborted);

    drop(restarted);
}

// --- daemon/runtime --------------------------------------------------

#[test]
fn daemon_start_stop_restart_stale_recovery_and_concurrent_clients() {
    let home = ScratchDir::create("runtime-home");

    let daemon = DaemonGuard::spawn(home.path());
    wait_for_daemon_ready(home.path());

    // Several concurrent clients.
    let handles: Vec<_> = (0..5)
        .map(|index| {
            let home_path = home.path().to_path_buf();
            thread::spawn(move || {
                let output = run_cli(&home_path, &["status"]);
                assert!(
                    output.status.success(),
                    "concurrent client {index} failed: {}",
                    stderr_of(&output)
                );
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("client thread should not panic");
    }

    // Simulate a crash: non-graceful kill leaves the socket/pipe and lock
    // runtime artifacts behind.
    drop(daemon);

    let down = run_cli(home.path(), &["status"]);
    assert!(!down.status.success());
    assert!(stderr_of(&down).contains("not running"));

    // Restart must recover from the stale artifacts, not fail forever.
    let restarted = DaemonGuard::spawn(home.path());
    wait_for_daemon_ready(home.path());
    let status = run_cli(home.path(), &["status"]);
    assert!(status.status.success(), "{}", stderr_of(&status));
    assert!(stdout_of(&status).contains("brainprintd"));

    drop(restarted);
}

// --- CLI dependency boundary --------------------------------------------

#[test]
fn cli_dependency_excludes_engine_daemon_rusqlite() {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/daemon should be nested two levels under the workspace root")
        .to_path_buf();

    let output = Command::new("cargo")
        .args([
            "tree",
            "--locked",
            "-p",
            "brainprint-cli",
            "--edges",
            "normal",
        ])
        .current_dir(&workspace_root)
        .output()
        .expect("cargo tree should run");
    assert!(
        output.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let tree = String::from_utf8_lossy(&output.stdout);

    for forbidden in ["brainprint-engine", "brainprint-daemon", "rusqlite"] {
        assert!(
            !tree.contains(forbidden),
            "brainprint-cli must never depend on {forbidden} (CLI direct DB write is \
             structurally impossible without it), but cargo tree shows:\n{tree}"
        );
    }
}
