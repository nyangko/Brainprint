//! #51: porcelain v2 fixtures (no Git), the call protocol through the
//! runner seam, the bounded runner, and real temporary Git repositories.

use std::{
    path::PathBuf,
    process,
    sync::atomic::{AtomicU64, Ordering},
};

use super::*;

const SHA1: &str = "2834408a3e45a28e6081abb17910365b14d2b98e";
const HASH: &str = "78981922613b2afb6025042ff6bd878ac1994e85";

fn header(oid: &str) -> Vec<u8> {
    format!("# branch.oid {oid}\0# branch.head main\0").into_bytes()
}

fn ordinary_record(xy: &str, path: &str) -> String {
    format!("1 {xy} N... 100644 100644 100644 {HASH} {HASH} {path}\0")
}

fn status_of(records: &str) -> GitStatus {
    let mut output = header(SHA1);
    output.extend_from_slice(records.as_bytes());
    parse_porcelain_v2(&output).expect("parses")
}

fn entries_of(records: &str) -> Vec<GitEntry> {
    match status_of(records).observation {
        GitObservation::Dirty(entries) => entries,
        other => panic!("expected Dirty, got {other:?}"),
    }
}

fn only(records: &str) -> GitEntry {
    let mut entries = entries_of(records);
    assert_eq!(entries.len(), 1, "{entries:?}");
    entries.remove(0)
}

fn rejected(output: &[u8]) -> GitStatusError {
    parse_porcelain_v2(output).expect_err("must be rejected")
}

// ------------------------------------------------------------ fixtures

#[test]
fn ordinary_xy_columns_map_to_one_status() {
    for (xy, status) in [
        (".M", GitEntryStatus::Modified),
        ("M.", GitEntryStatus::Modified),
        ("MM", GitEntryStatus::Modified),
        (".T", GitEntryStatus::Modified),
        ("T.", GitEntryStatus::Modified),
        ("A.", GitEntryStatus::Added),
        ("AM", GitEntryStatus::Added),
        ("AT", GitEntryStatus::Added),
        ("AD", GitEntryStatus::Added),
        ("D.", GitEntryStatus::Deleted),
        (".D", GitEntryStatus::Deleted),
        ("MD", GitEntryStatus::Deleted),
        ("TD", GitEntryStatus::Deleted),
    ] {
        let entry = only(&ordinary_record(xy, "src/a.rs"));
        assert_eq!(entry.status, status, "{xy}");
        assert_eq!(entry.path, "src/a.rs");
        assert_eq!(entry.old_path, None);
    }
}

#[test]
fn renames_keep_their_origin_and_copies_are_added() {
    let rename =
        format!("2 R. N... 100644 100644 100644 {HASH} {HASH} R100 src/new name.rs\0src/old.rs\0");
    assert_eq!(
        only(&rename),
        GitEntry {
            path: "src/new name.rs".to_owned(),
            old_path: Some("src/old.rs".to_owned()),
            status: GitEntryStatus::Renamed,
        }
    );
    let renamed_then_modified =
        format!("2 RM N... 100644 100644 100644 {HASH} {HASH} R087 b.rs\0a.rs\0");
    assert_eq!(only(&renamed_then_modified).status, GitEntryStatus::Renamed);
    let copy = format!("2 C. N... 100644 100644 100644 {HASH} {HASH} C75 copy.rs\0orig.rs\0");
    assert_eq!(
        only(&copy),
        GitEntry {
            path: "copy.rs".to_owned(),
            old_path: None,
            status: GitEntryStatus::Added,
        }
    );
}

#[test]
fn every_unmerged_pair_is_a_conflict() {
    for xy in ["DD", "AU", "UD", "UA", "DU", "AA", "UU"] {
        let record = format!("u {xy} N... 100644 100644 100644 100644 {HASH} {HASH} {HASH} c.rs\0");
        assert_eq!(only(&record).status, GitEntryStatus::Conflicted, "{xy}");
    }
}

#[test]
fn untracked_directories_submodules_and_symlinks() {
    assert_eq!(only("? scratch/\0").path, "scratch");
    assert_eq!(only("? top level.txt\0").status, GitEntryStatus::Untracked);
    let submodule = format!("1 .M S.M. 160000 160000 160000 {HASH} {HASH} vendor/lib\0");
    assert_eq!(
        only(&submodule),
        GitEntry {
            path: "vendor/lib".to_owned(),
            old_path: None,
            status: GitEntryStatus::Modified,
        }
    );
    let symlink = format!("1 .M N... 120000 120000 120000 {HASH} {HASH} link\0");
    assert_eq!(only(&symlink).path, "link");
}

#[test]
fn head_forms() {
    assert_eq!(status_of("").head.as_deref(), Some(SHA1));
    let sha256 = "a".repeat(64);
    let mut output = header(&sha256);
    output.extend_from_slice(b"? x\0");
    assert_eq!(
        parse_porcelain_v2(&output).expect("sha256").head,
        Some(sha256)
    );
    let unborn = parse_porcelain_v2(&header("(initial)")).expect("unborn");
    assert_eq!(unborn.head, None);
    assert_eq!(unborn.observation, GitObservation::Clean);
}

#[test]
fn no_entries_is_clean_never_unknown() {
    assert_eq!(status_of("").observation, GitObservation::Clean);
}

#[test]
fn output_order_does_not_change_the_fingerprint() {
    let records = [
        ordinary_record(".M", "b.rs"),
        "? a.txt\0".to_owned(),
        format!("u UU N... 100644 100644 100644 100644 {HASH} {HASH} {HASH} c.rs\0"),
    ];
    let forward = entries_of(&records.concat());
    let backward = entries_of(&records.iter().rev().cloned().collect::<String>());
    assert_eq!(forward, backward);
    assert_eq!(
        git_observation::fingerprint(&forward),
        git_observation::fingerprint(&backward)
    );
}

#[test]
fn malformed_output_fails_the_whole_observation() {
    let unparsable = |output: &[u8]| {
        assert!(
            matches!(rejected(output), GitStatusError::Unparsable(_)),
            "{:?}",
            String::from_utf8_lossy(output)
        );
    };
    unparsable(b"? a.txt\0");
    unparsable(format!("# branch.oid {SHA1}\0# branch.oid {SHA1}\0").as_bytes());
    unparsable(b"# branch.oid ABCDEF\0");
    unparsable(format!("# branch.oid {}\0", SHA1.to_uppercase()).as_bytes());
    unparsable(b"# branch.oid 1234\0");
    for record in [
        "1 .M N...\0".to_owned(),
        ordinary_record("..", "a.rs"),
        ordinary_record("XY", "a.rs"),
        format!("2 R. N... 100644 100644 100644 {HASH} {HASH} R100 new.rs\0"),
        format!("2 R. N... 100644 100644 100644 {HASH} {HASH} X100 new.rs\0old.rs\0"),
        "! ignored.txt\0".to_owned(),
        "z what\0".to_owned(),
        ordinary_record(".M", "../escape.rs"),
        ordinary_record(".M", "/abs.rs"),
    ] {
        let mut output = header(SHA1);
        output.extend_from_slice(record.as_bytes());
        unparsable(&output);
    }

    let mut output = header(SHA1);
    output.extend_from_slice(b"? caf\xe9.txt\0");
    assert_eq!(rejected(&output), GitStatusError::UnrepresentablePath);

    let mut output = header(SHA1);
    for index in 0..=GIT_ENTRY_BOUND {
        output.extend_from_slice(format!("? f{index}\0").as_bytes());
    }
    assert_eq!(rejected(&output), GitStatusError::TooManyEntries);
}

// ------------------------------------------------------------ protocol

static NEXT: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn create(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "brainprint-git-status-{label}-{}-{}",
            process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("test dir");
        Self(plain_path(&fs::canonicalize(&path).expect("canonical")))
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A root with a bare `.git` directory, observed only through the seam.
fn seam_root(label: &str) -> TestDir {
    let dir = TestDir::create(label);
    fs::create_dir(dir.0.join(".git")).expect(".git");
    dir
}

fn scripted(
    root: &Path,
    toplevel: &Path,
    statuses: Vec<Vec<u8>>,
    calls: &mut Vec<GitCall>,
) -> Result<GitStatus, GitStatusError> {
    let mut statuses = statuses.into_iter();
    observe_with(root, |call| {
        calls.push(call);
        Ok(match call {
            GitCall::TopLevel => format!("{}\n", toplevel.display()).into_bytes(),
            GitCall::Status => statuses.next().expect("no more status calls expected"),
        })
    })
}

#[test]
fn success_is_exactly_three_calls() {
    let root = seam_root("three-calls");
    let mut calls = Vec::new();
    let status = scripted(
        &root.0,
        &root.0,
        vec![header(SHA1), header(SHA1)],
        &mut calls,
    )
    .expect("observed");
    assert_eq!(status.head.as_deref(), Some(SHA1));
    assert_eq!(
        calls,
        vec![GitCall::TopLevel, GitCall::Status, GitCall::Status]
    );
}

#[test]
fn differing_reads_fail_without_a_further_call() {
    let root = seam_root("changed");
    let mut calls = Vec::new();
    let mut second = header(SHA1);
    second.extend_from_slice(b"? new.txt\0");
    assert_eq!(
        scripted(&root.0, &root.0, vec![header(SHA1), second], &mut calls),
        Err(GitStatusError::ChangedDuringObservation)
    );
    assert_eq!(calls.len(), 3);
}

#[test]
fn a_foreign_toplevel_stops_before_status() {
    let root = seam_root("boundary");
    let elsewhere = TestDir::create("boundary-elsewhere");
    let mut calls = Vec::new();
    assert_eq!(
        scripted(&root.0, &elsewhere.0, Vec::new(), &mut calls),
        Err(GitStatusError::WorkspaceBoundaryMismatch)
    );
    assert_eq!(calls, vec![GitCall::TopLevel]);
}

#[test]
fn no_dot_git_calls_nothing() {
    let root = TestDir::create("not-git");
    let mut calls = Vec::new();
    assert_eq!(
        scripted(&root.0, &root.0, Vec::new(), &mut calls),
        Err(GitStatusError::NotAGitWorkspace)
    );
    assert!(calls.is_empty());
}

#[test]
fn a_bad_first_read_stops_before_the_second() {
    let root = seam_root("bad-first");
    let mut calls = Vec::new();
    assert!(matches!(
        scripted(&root.0, &root.0, vec![b"garbage\0".to_vec()], &mut calls),
        Err(GitStatusError::Unparsable(_))
    ));
    assert_eq!(calls, vec![GitCall::TopLevel, GitCall::Status]);
}

// -------------------------------------------------------- bounded runner

#[test]
fn an_unstartable_program_is_unavailable() {
    let command = Command::new("brainprint-no-such-program-51");
    assert!(matches!(
        run_bounded(command, GIT_CALL_TIMEOUT, GIT_OUTPUT_CAP),
        Err(GitStatusError::GitUnavailable(_))
    ));
}

#[cfg(unix)]
fn shell(script: &str) -> Command {
    let mut command = Command::new("sh");
    command
        .args(["-c", script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

#[cfg(unix)]
#[test]
fn a_hung_process_is_killed_at_the_deadline() {
    let started = Instant::now();
    assert_eq!(
        run_bounded(
            shell("sleep 30"),
            Duration::from_millis(200),
            GIT_OUTPUT_CAP
        ),
        Err(GitStatusError::Timeout)
    );
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[cfg(unix)]
#[test]
fn output_past_the_cap_is_refused() {
    assert_eq!(
        run_bounded(shell("head -c 200000 /dev/zero"), GIT_CALL_TIMEOUT, 1000),
        Err(GitStatusError::OutputTooLarge)
    );
}

#[cfg(unix)]
#[test]
fn a_failing_exit_keeps_a_bounded_stderr() {
    let failed = run_bounded(
        shell("printf 'fatal: nope' >&2; head -c 5000 /dev/zero >&2; exit 3"),
        GIT_CALL_TIMEOUT,
        GIT_OUTPUT_CAP,
    );
    let Err(GitStatusError::GitFailed { exit_code, stderr }) = failed else {
        panic!("expected GitFailed, got {failed:?}")
    };
    assert_eq!(exit_code, Some(3));
    assert!(stderr.starts_with("fatal: nope"));
    assert!(stderr.len() <= STDERR_CAP);
}

// ------------------------------------------------- real Git repositories

fn git(cwd: &Path, args: &[&str]) -> process::Output {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "brainprint-test")
        .env("GIT_AUTHOR_EMAIL", "test@brainprint.invalid")
        .env("GIT_COMMITTER_NAME", "brainprint-test")
        .env("GIT_COMMITTER_EMAIL", "test@brainprint.invalid")
        .output()
        .expect("git must be installed for these tests")
}

fn git_ok(cwd: &Path, args: &[&str]) -> String {
    let output = git(cwd, args);
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn write(root: &Path, path: &str, text: &str) {
    let full = root.join(path);
    fs::create_dir_all(full.parent().expect("parent")).expect("dirs");
    fs::write(full, text).expect("write");
}

/// A repository with one commit holding a.txt, b.txt, c.txt, conf.txt.
fn committed_repo(label: &str) -> TestDir {
    let repo = TestDir::create(label);
    git_ok(&repo.0, &["init", "-q"]);
    git_ok(&repo.0, &["config", "core.autocrlf", "false"]);
    for name in ["a.txt", "b.txt", "c.txt", "conf.txt"] {
        write(&repo.0, name, &format!("{name}\n"));
    }
    git_ok(&repo.0, &["add", "."]);
    git_ok(&repo.0, &["commit", "-qm", "init"]);
    repo
}

fn dirty_entries(status: &GitStatus) -> Vec<(GitEntryStatus, String, Option<String>)> {
    match &status.observation {
        GitObservation::Dirty(entries) => entries
            .iter()
            .map(|entry| (entry.status, entry.path.clone(), entry.old_path.clone()))
            .collect(),
        other => panic!("expected Dirty, got {other:?}"),
    }
}

#[test]
fn a_clean_repository_is_clean_with_its_head() {
    let repo = committed_repo("clean");
    let status = observe(&repo.0).expect("observed");
    assert_eq!(status.observation, GitObservation::Clean);
    assert_eq!(status.head, Some(git_ok(&repo.0, &["rev-parse", "HEAD"])));
}

#[test]
fn a_dirty_work_tree_maps_every_kind_of_change() {
    let repo = committed_repo("dirty");
    let main = git_ok(&repo.0, &["rev-parse", "--abbrev-ref", "HEAD"]);
    git_ok(&repo.0, &["checkout", "-qb", "side"]);
    write(&repo.0, "conf.txt", "side\n");
    git_ok(&repo.0, &["commit", "-qam", "side"]);
    git_ok(&repo.0, &["checkout", "-q", &main]);
    write(&repo.0, "conf.txt", "main\n");
    git_ok(&repo.0, &["commit", "-qam", "main"]);
    assert!(
        !git(&repo.0, &["merge", "-q", "side"]).status.success(),
        "conflict expected"
    );

    write(&repo.0, "a.txt", "changed\n");
    fs::remove_file(repo.0.join("b.txt")).expect("delete");
    git_ok(&repo.0, &["mv", "c.txt", "c2.txt"]);
    write(&repo.0, "newdir/one.txt", "1\n");
    write(&repo.0, "top.txt", "t\n");
    write(&repo.0, "staged.txt", "s\n");
    git_ok(&repo.0, &["add", "staged.txt"]);
    write(&repo.0, "staged.txt", "s2\n");
    // Brainprint's own data directory never shows up.
    write(&repo.0, ".brainprint/data/index.db", "x");

    let status = observe(&repo.0).expect("observed");
    assert_eq!(status.head, Some(git_ok(&repo.0, &["rev-parse", "HEAD"])));
    assert_eq!(
        dirty_entries(&status),
        vec![
            (GitEntryStatus::Modified, "a.txt".to_owned(), None),
            (GitEntryStatus::Deleted, "b.txt".to_owned(), None),
            (
                GitEntryStatus::Renamed,
                "c2.txt".to_owned(),
                Some("c.txt".to_owned())
            ),
            (GitEntryStatus::Conflicted, "conf.txt".to_owned(), None),
            (GitEntryStatus::Untracked, "newdir".to_owned(), None),
            (GitEntryStatus::Added, "staged.txt".to_owned(), None),
            (GitEntryStatus::Untracked, "top.txt".to_owned(), None),
        ]
    );
}

#[test]
fn further_edits_to_a_dirty_path_keep_the_fingerprint() {
    // The #50 entry-level ceiling, kept deliberately.
    let repo = committed_repo("content-ceiling");
    write(&repo.0, "a.txt", "first edit\n");
    let first = observe(&repo.0).expect("first");
    write(&repo.0, "a.txt", "second, different edit\n");
    let second = observe(&repo.0).expect("second");
    assert_eq!(first, second);
}

#[test]
fn an_unborn_repository_has_no_head() {
    let repo = TestDir::create("unborn");
    git_ok(&repo.0, &["init", "-q"]);
    write(&repo.0, "new.txt", "n\n");
    let status = observe(&repo.0).expect("observed");
    assert_eq!(status.head, None);
    assert_eq!(
        dirty_entries(&status),
        vec![(GitEntryStatus::Untracked, "new.txt".to_owned(), None)]
    );
}

#[test]
fn a_linked_worktree_sees_only_itself() {
    let repo = committed_repo("worktree-main");
    let holder = TestDir::create("worktree-holder");
    let linked = holder.0.join("linked");
    git_ok(
        &repo.0,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            &linked.to_string_lossy(),
        ],
    );
    let linked = plain_path(&fs::canonicalize(&linked).expect("linked"));
    write(&repo.0, "a.txt", "main only\n");
    write(&linked, "b.txt", "linked only\n");
    assert_eq!(
        dirty_entries(&observe(&linked).expect("linked")),
        vec![(GitEntryStatus::Modified, "b.txt".to_owned(), None)]
    );
    assert_eq!(
        dirty_entries(&observe(&repo.0).expect("main")),
        vec![(GitEntryStatus::Modified, "a.txt".to_owned(), None)]
    );
}

#[test]
fn an_inherited_git_environment_cannot_redirect_the_observation() {
    let repo = committed_repo("env-target");
    let other = committed_repo("env-other");
    write(&other.0, "a.txt", "other repo change\n");
    let polluted: Vec<(OsString, OsString)> = std::env::vars_os()
        .chain([
            ("GIT_DIR".into(), other.0.join(".git").into_os_string()),
            ("GIT_WORK_TREE".into(), other.0.clone().into_os_string()),
            (
                "GIT_INDEX_FILE".into(),
                other.0.join(".git/index").into_os_string(),
            ),
        ])
        .collect();
    let status = observe_with(&repo.0, |call| {
        run_bounded(
            git_command(&repo.0, call.args(), polluted.clone()),
            GIT_CALL_TIMEOUT,
            GIT_OUTPUT_CAP,
        )
    })
    .expect("observed");
    assert_eq!(status.observation, GitObservation::Clean);
    assert_eq!(status.head, Some(git_ok(&repo.0, &["rev-parse", "HEAD"])));
}

#[test]
fn observing_does_not_write_the_index() {
    let repo = committed_repo("index-untouched");
    write(&repo.0, "a.txt", "changed\n");
    let index = repo.0.join(".git/index");
    let before = (
        fs::read(&index).expect("index"),
        fs::metadata(&index)
            .expect("meta")
            .modified()
            .expect("mtime"),
    );
    observe(&repo.0).expect("observed");
    let after = (
        fs::read(&index).expect("index"),
        fs::metadata(&index)
            .expect("meta")
            .modified()
            .expect("mtime"),
    );
    assert_eq!(before, after);
}

#[test]
fn a_work_tree_configured_elsewhere_is_a_boundary_mismatch() {
    let repo = committed_repo("core-worktree");
    let elsewhere = TestDir::create("core-worktree-elsewhere");
    git_ok(
        &repo.0,
        &["config", "core.worktree", &elsewhere.0.to_string_lossy()],
    );
    assert_eq!(
        observe(&repo.0),
        Err(GitStatusError::WorkspaceBoundaryMismatch)
    );
}

#[test]
fn verbatim_windows_paths_are_handed_to_git_plain() {
    assert_eq!(
        plain_path(Path::new(r"\\?\C:\work\repo")),
        PathBuf::from(r"C:\work\repo")
    );
    assert_eq!(
        plain_path(Path::new(r"\\?\UNC\server\share\repo")),
        PathBuf::from(r"\\server\share\repo")
    );
    assert_eq!(
        plain_path(Path::new("/tmp/repo")),
        PathBuf::from("/tmp/repo")
    );
}

#[test]
fn a_plain_directory_is_not_a_git_workspace() {
    let dir = TestDir::create("plain");
    assert_eq!(observe(&dir.0), Err(GitStatusError::NotAGitWorkspace));
}
