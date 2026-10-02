//! #50 (I6 task 1): caller-observed Git state → the #20 task 3
//! `WorkRuntime`, on the Workspace's worker thread.
//!
//! Runs no command: HEAD, entries and verification are the caller's
//! observations. Every input is validated before anything is written, and
//! a `New` WorkItem is created only after the Workspace was seen able to
//! take a baseline, so a failure leaves stored state as it was.

// #52 grew `WorkFailureWire` (the per-command results) past the lint's
// size; it is built once per Work request, never in a hot loop.
#![allow(clippy::result_large_err)]

use std::path::Path;

use brainprint_core::{
    VerificationJobId, WorkItemId, WorkspaceId,
    protocol::{
        query::{QueryErrorWire, WorkItemSourceKindWire},
        work::{
            GitEntryStatusWire, GitEntryWire, GitObservationWire, WorkErrorWire, WorkFailureWire,
            WorkNotReadyWire, WorkOperationWire, WorkOutcomeWire, WorkRecordedWire, WorkResponse,
            WorkResultInputWire, WorkStartItemWire, WorkStartWire, WorkStartedWire,
        },
    },
};
use brainprint_engine::{
    git_observation::{
        self, GitEntry, GitEntryStatus, GitObservation, GitObservationError, ResolvedPaths,
    },
    knowledge::{
        ExpectedResultBasis, KnowledgeError, NewWorkItem, NotReady, ResultObservation,
        StartObservation, WorkError, WorkItemSourceKind, WorkRuntime, WorkspaceKnowledgeStore,
    },
    paths::WorkspacePaths,
    query_surface::{CoreError, NotInitialized},
    resource::ResourceStore,
};

use super::{convert_out, lifecycle::IndexBasis};

/// #55: what a verified result is held to, beside its wire input. Empty
/// for every ordinary write.
#[derive(Debug, Clone, Default)]
pub(super) struct WorkContext {
    /// The basis the verification proved current; the write happens only
    /// at exactly it.
    pub expected_basis: Option<ExpectedResultBasis>,
    /// The managed Job the result's verification summary came from.
    pub verification_job: Option<VerificationJobId>,
}

impl WorkContext {
    pub(super) fn at(basis: &IndexBasis) -> Self {
        Self {
            expected_basis: Some(ExpectedResultBasis {
                index_incarnation: basis.index_incarnation,
                workspace_revision: basis.workspace_revision.clone(),
                generation_no: basis.generation_no,
            }),
            verification_job: None,
        }
    }
}

pub(super) fn run(
    workspace: WorkspaceId,
    root: &Path,
    operation: WorkOperationWire,
    context: WorkContext,
) -> WorkResponse {
    let paths = WorkspacePaths::from_root(root);
    let outcome = match operation {
        WorkOperationWire::Start(start) => start_work(workspace, &paths, start),
        WorkOperationWire::Result(input) => record_result(workspace, &paths, input, context),
    };
    outcome.unwrap_or_else(WorkResponse::Failed)
}

/// #55, protocol 8: the Workspace is no longer at the basis a
/// verification proved; nothing was stored. A fixed message, no detail.
pub(super) const BASIS_CHANGED: &str =
    "the workspace changed since the verification; nothing was stored";

pub(super) fn basis_changed() -> WorkResponse {
    internal(BASIS_CHANGED)
}

pub(super) fn workspace_error(error: QueryErrorWire) -> WorkResponse {
    failed(WorkErrorWire::Workspace(error))
}

pub(super) fn internal(message: &str) -> WorkResponse {
    failed(WorkErrorWire::Internal {
        message: message.to_owned(),
    })
}

fn failed(error: WorkErrorWire) -> WorkResponse {
    WorkResponse::Failed(failure(error))
}

pub(super) const fn failure(error: WorkErrorWire) -> WorkFailureWire {
    WorkFailureWire {
        error,
        created_work_item: None,
        verification: None,
    }
}

fn start_work(
    workspace: WorkspaceId,
    paths: &WorkspacePaths,
    start: WorkStartWire,
) -> Result<WorkResponse, WorkFailureWire> {
    let (dirty, entries) =
        git_observation::dirty_observation(&git_observation_in(start.git)?).map_err(observation)?;
    let runtime = open(workspace, paths)?;
    // Opened only after `WorkRuntime::open` proved index.db exists and is
    // bound: `ResourceStore::open` would create a missing file.
    let ResolvedPaths {
        resources,
        unresolved,
    } = ResourceStore::open(&paths.index_db)
        .and_then(|store| git_observation::resolve_active(&store, &entries))
        .map_err(|error| failure(internal_error("resolve paths", &error)))?;

    let (work_item, created) = match start.work_item {
        WorkStartItemWire::Existing(work_item) => (work_item, None),
        WorkStartItemWire::New(new) => {
            runtime
                .check_baseline_ready()
                .map_err(|error| failure(work_error(error, None)))?;
            let created = runtime
                .create(&NewWorkItem {
                    source_kind: source_kind_in(new.source_kind),
                    source_ref: new.source_ref,
                    title: new.title,
                    goal: new.goal,
                })
                .map_err(|error| failure(work_error(error, None)))?;
            (created.uid, Some(created.uid))
        }
    };

    let state = runtime
        .start(
            work_item,
            &StartObservation {
                head: start.head,
                dirty,
                preexisting_dirty: resources,
                owner_agent: start.owner_agent,
            },
        )
        .map_err(|error| WorkFailureWire {
            error: work_error(error, Some(work_item)),
            created_work_item: created,
            verification: None,
        })?;
    Ok(WorkResponse::Started(WorkStartedWire {
        workspace_id: workspace,
        working_state: convert_out::working_state_wire(state),
        unresolved_paths: unresolved,
    }))
}

/// #52: every check `record_result` makes on the input alone, so a
/// verification request is refused before a command runs. An `Observe`
/// is checked once it is resolved, after the commands.
pub(super) fn check_result_input(input: &WorkResultInputWire) -> Result<(), WorkFailureWire> {
    if input.git != GitObservationWire::Observe {
        git_observation::dirty_observation(&git_observation_in(input.git.clone())?)
            .map_err(observation)?;
    }
    change_set_fingerprint(input.change_set.clone()).map(|_| ())
}

fn change_set_fingerprint(
    change_set: Option<Vec<GitEntryWire>>,
) -> Result<Option<String>, WorkFailureWire> {
    change_set
        .map(|entries| {
            let entries: Vec<GitEntry> = entries.into_iter().map(git_entry_in).collect();
            git_observation::canonical_entries(&entries)
                .map(|canonical| git_observation::fingerprint(&canonical))
        })
        .transpose()
        .map_err(observation)
}

fn record_result(
    workspace: WorkspaceId,
    paths: &WorkspacePaths,
    input: WorkResultInputWire,
    context: WorkContext,
) -> Result<WorkResponse, WorkFailureWire> {
    if input.verification.is_some() {
        return Err(failure(internal_message(
            "a verification request reached the write unrun",
        )));
    }
    let (remaining_dirty, _) =
        git_observation::dirty_observation(&git_observation_in(input.git)?).map_err(observation)?;
    let change_set_fingerprint = change_set_fingerprint(input.change_set)?;
    let observation = ResultObservation {
        summary: input.summary,
        commit_id: input.commit_id,
        change_set_fingerprint,
        verification_summary: input.verification_summary,
        remaining_dirty,
        verification_job: context.verification_job,
        expected_basis: context.expected_basis,
    };
    let runtime = open(workspace, paths)?;
    let work_item = input.work_item;
    let fail = |error| failure(work_error(error, Some(work_item)));

    let (status, result) = match input.outcome {
        WorkOutcomeWire::Partial => {
            // A partial result never moves the status, so it is read
            // before the write and nothing fallible follows the write.
            let item = WorkspaceKnowledgeStore::open(&paths.workspace_db)
                .and_then(|store| store.get_work_item(work_item))
                .map_err(|error| fail(error.into()))?
                .ok_or_else(|| failure(WorkErrorWire::WorkItemNotFound { work_item }))?;
            let result = runtime
                .record_partial(work_item, &observation)
                .map_err(fail)?;
            (item.status, result)
        }
        WorkOutcomeWire::Complete | WorkOutcomeWire::Abandon => {
            let snapshot = if input.outcome == WorkOutcomeWire::Complete {
                runtime.complete(work_item, &observation, None)
            } else {
                runtime.abandon(work_item, &observation, None)
            }
            .map_err(fail)?;
            let result = snapshot
                .result
                .ok_or_else(|| failure(internal_message("a finished WorkItem has no result")))?;
            (snapshot.item.status, result)
        }
    };
    Ok(WorkResponse::Recorded(WorkRecordedWire {
        workspace_id: workspace,
        status: convert_out::work_item_status_wire(status),
        result: convert_out::work_result_wire(result),
        verification: None,
    }))
}

pub(super) fn open(
    workspace: WorkspaceId,
    paths: &WorkspacePaths,
) -> Result<WorkRuntime, WorkFailureWire> {
    WorkRuntime::open(workspace, &paths.workspace_db, &paths.index_db)
        .map_err(|error| failure(work_error(error, None)))
}

// ------------------------------------------------------------- errors

fn observation(error: GitObservationError) -> WorkFailureWire {
    failure(match error {
        GitObservationError::TooManyEntries { .. } => WorkErrorWire::BoundExceeded {
            what: "git entries".to_owned(),
        },
        other => WorkErrorWire::InvalidObservation {
            reason: other.to_string(),
        },
    })
}

/// `work_item` names the request's WorkItem for a not-found error.
pub(super) fn work_error(error: WorkError, work_item: Option<WorkItemId>) -> WorkErrorWire {
    if let (
        WorkError::Knowledge(KnowledgeError::NotFound {
            what: "work_item", ..
        }),
        Some(work_item),
    ) = (&error, work_item)
    {
        return WorkErrorWire::WorkItemNotFound { work_item };
    }
    let workspace = |error: CoreError| WorkErrorWire::Workspace(convert_out::core_error(error));
    match error {
        WorkError::MissingDatabase { db: "workspace.db" } => workspace(CoreError::NotInitialized(
            NotInitialized::WorkspaceDbMissing,
        )),
        WorkError::MissingDatabase { .. } => {
            workspace(CoreError::NotInitialized(NotInitialized::IndexDbMissing))
        }
        WorkError::UnboundWorkspace { db } => workspace(CoreError::NotInitialized(
            NotInitialized::WorkspaceUnbound { db },
        )),
        WorkError::WorkspaceMismatch {
            db,
            expected,
            found,
        } => workspace(CoreError::WorkspaceBindingMismatch {
            db,
            expected,
            found,
        }),
        WorkError::WorkspaceNotReady(reason) => WorkErrorWire::WorkspaceNotReady(match reason {
            NotReady::ClockNotBootstrapped => WorkNotReadyWire::ClockNotBootstrapped,
            NotReady::NoStableGeneration => WorkNotReadyWire::NoStableGeneration,
            NotReady::StableBehind {
                stable_basis,
                current,
            } => WorkNotReadyWire::StableBehind {
                stable_basis,
                current,
            },
        }),
        WorkError::InvalidObservation(reason) => WorkErrorWire::InvalidObservation { reason },
        WorkError::BoundExceeded { what } => WorkErrorWire::BoundExceeded {
            what: what.to_owned(),
        },
        WorkError::ResultBasisChanged => internal_message(BASIS_CHANGED),
        WorkError::Knowledge(KnowledgeError::InvalidTransition { what, reason }) => {
            WorkErrorWire::InvalidTransition {
                reason: format!("{what}: {reason}"),
            }
        }
        other => internal_error("work", &other),
    }
}

fn internal_error(operation: &str, error: &dyn std::error::Error) -> WorkErrorWire {
    eprintln!("brainprintd: {operation}: {error}");
    internal_message("internal daemon error; see daemon logs for detail")
}

pub(super) fn internal_message(message: &str) -> WorkErrorWire {
    WorkErrorWire::Internal {
        message: message.to_owned(),
    }
}

// --------------------------------------------------------- conversions

/// `Observe` is replaced by the handler's own observation (#51) before
/// the write is dispatched; one reaching here is never stored as-is.
fn git_observation_in(observation: GitObservationWire) -> Result<GitObservation, WorkFailureWire> {
    Ok(match observation {
        GitObservationWire::Unknown => GitObservation::Unknown,
        GitObservationWire::Clean => GitObservation::Clean,
        GitObservationWire::Dirty { entries } => {
            GitObservation::Dirty(entries.into_iter().map(git_entry_in).collect())
        }
        GitObservationWire::Observe => {
            return Err(failure(internal_message(
                "an Observe request reached the write unresolved",
            )));
        }
    })
}

fn git_entry_in(entry: GitEntryWire) -> GitEntry {
    GitEntry {
        path: entry.path,
        old_path: entry.old_path,
        status: match entry.status {
            GitEntryStatusWire::Added => GitEntryStatus::Added,
            GitEntryStatusWire::Modified => GitEntryStatus::Modified,
            GitEntryStatusWire::Deleted => GitEntryStatus::Deleted,
            GitEntryStatusWire::Renamed => GitEntryStatus::Renamed,
            GitEntryStatusWire::Untracked => GitEntryStatus::Untracked,
            GitEntryStatusWire::Conflicted => GitEntryStatus::Conflicted,
        },
    }
}

const fn source_kind_in(kind: WorkItemSourceKindWire) -> WorkItemSourceKind {
    match kind {
        WorkItemSourceKindWire::Issue => WorkItemSourceKind::Issue,
        WorkItemSourceKindWire::ExternalTask => WorkItemSourceKind::ExternalTask,
        WorkItemSourceKindWire::UserRequest => WorkItemSourceKind::UserRequest,
    }
}
