//! The TypeScript / JavaScript semantic backend.
//!
//! One `tsc --lsp --stdio` child process -- the TypeScript 7 native
//! language server -- answers for TypeScript, JavaScript, TSX and JSX
//! alike. There is no second backend, no second graph, and no
//! `typescript_*` query surface: after the task 4 merge the existing
//! `callers`, `references`, impact and prepared-inspection APIs are what
//! answer, enriched rather than duplicated.
//!
//! The layering:
//!
//! ```text
//! SemanticRuntimeSupervisor            (#19 task 2, backend-neutral)
//!         ↓ SemanticBackendLauncher
//! TypeScriptLauncher → TypeScriptHost  (launcher, host)
//!         ↓ TypeScriptRequest / TypeScriptResponse
//! typescript-go LSP server             (protocol)
//!         ↓ adapter
//! SemanticEvidence                     (#19 task 1)
//!         ↓ SemanticIndex candidate    (#19 task 3)
//!         ↓ merge                      (#19 task 4)
//! the one canonical Brainprint graph
//! ```
//!
//! [`crate::lsp`] carries the framing and the coordinate mapping, shared
//! with the Python backend rather than reimplemented -- but nothing
//! TypeScript-specific went into it. Method names, the compatibility
//! class, the project model and the dependency-environment policy all
//! live here, because they are what this server means and not what LSP
//! means.
//!
//! ## What the transport probe settled
//!
//! The design issue named a `tsserver` family, written while TypeScript
//! was mid-port. #19 task 10 opened by measuring the artifact instead of
//! trusting that, and Candidate A -- the native LSP -- answered every
//! P0 capability, so Candidate B was never built. See [`protocol`] for
//! the recorded version, command line and handshake, and
//! [`WATCHER_DECISION`] for the filesystem-synchronization measurement.

pub mod host;
pub mod launcher;
pub mod protocol;

use crate::resolution::Support;

/// How Brainprint tells this backend that the Workspace moved.
///
/// #19 task 10 required this be measured rather than copied from the
/// Python backend, and specifically flagged reports of a native-LSP
/// `Created` notification leaving stale module state. It was measured,
/// on 7.0.2, three ways over the same fixture -- a new export added to
/// an existing module and imported, a brand-new module created and
/// imported, and a module deleted:
///
/// ```text
/// server-side watching (client declares no didChangeWatchedFiles)  PASS
/// client-side workspace/didChangeWatchedFiles                      PASS
/// textDocument/didOpen + didChange overlay                         PASS
/// ```
///
/// All three were correct, and all three stayed correct with *no*
/// settle delay between the change and the query. The reported stale
/// export state did not reproduce on this version.
///
/// Client-side watched-file notifications win anyway, for a reason the
/// pass/fail table does not show. The other two each fail a requirement
/// the task states outright:
///
/// * A document overlay would make an editor buffer a second reality
///   alongside the Workspace filesystem. It also bought nothing: the
///   server already reads the same bytes Brainprint indexed.
/// * The server's own watcher works, but it is an OS watcher on a
///   timeline Brainprint cannot observe. There is no moment at which
///   Brainprint *knows* the backend has seen a change, so publishing
///   currentness would be a guess that happened to be right on a local
///   APFS volume and need not be on a network mount or another platform.
///
/// A notification gives what neither does: JSON-RPC over one ordered
/// stdio connection delivers it strictly before any request sent after
/// it, so "the backend has caught up" is a fact about message order
/// rather than about elapsed time. That ordering is this backend's
/// synchronization barrier, and it is why there is no snapshot number
/// here -- see [`FRESHNESS_MODEL`].
pub const WATCHER_DECISION: &str =
    "client-side workspace/didChangeWatchedFiles, no document overlay";

/// This backend's in-flight freshness currency, or rather the absence of
/// one.
///
/// The Python backend carries a `typeServer/getSnapshot` number and
/// retries a batch the server declares stale. The TypeScript native LSP
/// exposes no equivalent: no project version, no result id, and no
/// server-side staleness rejection. The task is explicit that a snapshot
/// number must not be invented when the backend does not expose one, so
/// none is.
///
/// What replaces it is the ordering barrier in [`WATCHER_DECISION`]: a
/// request issued after a watched-file notification on the same
/// connection is answered against the changed filesystem. Nothing
/// process-local is persisted either way -- task 3 freshness stays
/// Resource revisions, config fingerprint, environment fingerprint,
/// inventory fingerprint and analysis profile, exactly as before.
pub const FRESHNESS_MODEL: &str = "connection ordering; the backend exposes no snapshot token";

/// How well overload resolution is covered.
///
/// SUPPORTED, and measured rather than assumed. This was the task's
/// named design-stop risk: if the public LSP boundary could not prove
/// which overload a call site selected, the alternative was compiler
/// internals, and the instruction was to stop and report the conflict
/// rather than reach for them.
///
/// It did not come to that. On the fixture's
/// `parse(value: string): StringResult` / `parse(value: number):
/// NumberResult` pair, `textDocument/definition` at `parse("a")`
/// answered the *string* overload's declaration line and at `parse(1)`
/// the *number* overload's -- two different targets from the same name,
/// chosen by argument type, from the public boundary. `textDocument/
/// hover` corroborated each with the selected signature, and
/// `signatureHelp` returned the whole declaration set with the server's
/// own active index.
///
/// So the three things the task asks Brainprint to distinguish all come
/// from the server: the overload *declaration set* from `signatureHelp`,
/// the *call-site selection* from `definition`, and the *implementation*
/// declaration as the one signature `definition` never selects. None of
/// it is decided by matching parameter text here.
pub const OVERLOAD_SUPPORT: Support = Support::Supported;

/// How well `IMPLEMENTS` is covered.
///
/// SUPPORTED, and the clearest difference from the Python tier. #19
/// task 5 measured `textDocument/implementation` answering
/// `MethodNotFound` on Pyright, which is why Python had to *derive*
/// inheritance in task 7 with its own evidence. The TypeScript server
/// declares `implementationProvider` and answers: asked at the `Runner`
/// interface the fixture declares, it returned both implementing
/// classes across two files.
pub const IMPLEMENTS_SUPPORT: Support = Support::Supported;

/// How well a call site's bound target is covered.
///
/// A direct call binds to a declaration and is recorded `STATIC`. A call
/// through a typed receiver binds to the *declaration the receiver's
/// type names*, which in a language with subclassing is not necessarily
/// the code that runs, so it is recorded `UNKNOWN` rather than claimed
/// as a static target. Both answers are exact; only one of them is the
/// capability, hence PARTIAL.
///
/// The measurement that makes this worth stating: the fixture declares
/// `Alpha`, `Beta` and `Gamma`, each with an unrelated `run()`, and a
/// function taking a `Beta`. `b.run()` resolved to `Beta.run` and to
/// nothing else. That is a real type-directed answer, not a name match
/// -- and it is still a declaration, not a dispatch target.
pub const CALL_TARGET_SUPPORT: Support = Support::Partial;

/// How well JavaScript type resolution is covered.
///
/// PARTIAL, and deliberately not rounded up because the server returns
/// something for every position. Measured on the fixture's `js/`
/// directory: a JSDoc `@param {string}` does produce a real signature
/// (`function measure(name: string): number`), and a `@type` annotation
/// a real object shape. But an unannotated parameter hovers as `any`,
/// and `obj[name]()` yields nothing a relation could anchor to.
///
/// Those are honest gaps and stay gaps. The rule the task states and
/// this tier follows: a false zero is unacceptable, an explicit
/// PARTIAL is not.
pub const JAVASCRIPT_TYPE_SUPPORT: Support = Support::Partial;
