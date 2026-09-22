//! One live rust-analyzer, behind the task 2 host trait.
//!
//! The barrier here is a *transition* rather than an event. The server
//! does not announce "the project is loaded"; it announces how it is,
//! repeatedly, through [`method::SERVER_STATUS`] --
//! `quiescent: false` while it is working and `quiescent: true` when it
//! has settled. So this host counts the settlings and lets a caller
//! wait for the number to move, which is what replaces the settle delay
//! that this tier, like every other, refuses to have.
//!
//! The notification is only sent because the handshake asked for it;
//! see [`initialize_params`](super::protocol::initialize_params). Without
//! that one capability there is no deterministic way to know a reload
//! finished, and the whole lifecycle would rest on a sleep.

use std::{
    collections::BTreeMap,
    process::Child,
    sync::{
        Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;

use super::protocol::{
    METHOD_NOT_FOUND, ProjectExecutionTrust, REQUEST_CANCELLED, RustRequest, RustResponse, decode,
    method,
};
use crate::{
    lsp::jsonrpc::{Client, RpcFailure, ServerHandler},
    runtime::{
        CancelToken, HostConcurrency, HostError, HostHealth, ResourceUsage, RuntimeRequest,
        RuntimeResponse, SemanticRuntimeHost,
    },
};

/// How long a `shutdown` gets before the child is killed.
const EXIT_GRACE: Duration = Duration::from_secs(5);

/// What Brainprint answers when the backend asks for configuration.
///
/// `null` for every section, which LSP defines as "no value". The
/// reasoning is the same as every other tier's: the answer must depend
/// on the *project*, so anything this client injected would have to
/// appear in the [`AnalysisContext`](crate::semantic::AnalysisContext)
/// or two Workspaces with identical sources could disagree.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RustSettings;

impl RustSettings {
    #[must_use]
    pub fn section(self, _section: &str) -> Value {
        Value::Null
    }

    /// The configuration inputs this client contributes to a basis.
    /// Empty, and the empty `Vec` is what makes that visible rather than
    /// silent.
    #[must_use]
    pub fn basis_inputs(self) -> Vec<(String, String)> {
        Vec::new()
    }
}

/// Answers the backend's requests and watches for the load barrier.
pub(crate) struct ClientHandler {
    settings: RustSettings,
    registrations: AtomicU64,
    /// How many times the server has settled.
    ///
    /// A count rather than a flag, because `quiescent: true` is a
    /// recurring state rather than a one-off event: a reload makes it
    /// false and then true again, and "the number went up" is what a
    /// waiter is actually waiting for. Counting only the false -> true
    /// edge means a server that repeats `true` cannot make a waiter
    /// believe new work finished.
    completions: Mutex<u64>,
    /// Whether the last status said it was working.
    working: Mutex<bool>,
    signal: Condvar,
}

impl ClientHandler {
    pub(crate) fn new(settings: RustSettings) -> Self {
        Self {
            settings,
            registrations: AtomicU64::new(0),
            completions: Mutex::new(0),
            working: Mutex::new(false),
            signal: Condvar::new(),
        }
    }

    /// Record one status notification, counting only a settling.
    fn status(&self, quiescent: bool) {
        let mut working = self
            .working
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if quiescent {
            // Only a transition counts. Two `true`s in a row are one
            // settling, and a waiter that treated the second as fresh
            // work would return before the work it asked for began.
            if *working {
                *working = false;
                drop(working);
                let mut count = self
                    .completions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *count += 1;
                self.signal.notify_all();
            }
        } else {
            *working = true;
        }
    }
}

impl ServerHandler for ClientHandler {
    fn request(&self, requested: &str, params: &Value) -> Option<Value> {
        match requested {
            method::CONFIGURATION => {
                let items = params.get("items").and_then(Value::as_array);
                Some(Value::Array(
                    items
                        .map(|items| {
                            items
                                .iter()
                                .map(|item| {
                                    self.settings.section(
                                        item.get("section").and_then(Value::as_str).unwrap_or(""),
                                    )
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                ))
            }
            method::REGISTER_CAPABILITY => {
                self.registrations.fetch_add(1, Ordering::SeqCst);
                Some(Value::Null)
            }
            method::UNREGISTER_CAPABILITY => Some(Value::Null),
            _ => None,
        }
    }

    fn notification(&self, notified: &str, params: &Value) {
        if notified == method::SERVER_STATUS {
            self.status(super::protocol::is_quiescent(Some(params)));
        }
    }
}

// ---------------------------------------------------------------------
// Host
// ---------------------------------------------------------------------

/// One live `Microsoft.CodeAnalysis.LanguageServer --stdio`.
pub struct RustHost {
    client: Client,
    child: Mutex<Option<Child>>,
    /// Read from the handshake: unlike the Roslyn server, this one
    /// reports `serverInfo`.
    server_version: String,
    /// What this process is permitted to do. Carried on the host so a
    /// project-loading request cannot reach the wire through some other
    /// path.
    trust: ProjectExecutionTrust,
    handler: std::sync::Arc<ClientHandler>,
    /// Monotonic document version, so `didChange` ordering is
    /// deterministic rather than wall-clock.
    document_version: AtomicU64,
    /// What this connection has handed the server, by URI.
    ///
    /// The text and not merely the name, because a re-synchronization
    /// has to be a `didChange` that says what it replaces: LSP allows
    /// one `didOpen` per document, and the measured server declares
    /// incremental sync and exits on a change with no range. Only this
    /// connection knows any of it -- a restarted server holds nothing.
    opened: Mutex<BTreeMap<String, String>>,
    stopped: AtomicBool,
}

impl RustHost {
    pub(crate) fn new(
        client: Client,
        child: Option<Child>,
        server_version: String,
        trust: ProjectExecutionTrust,
        handler: std::sync::Arc<ClientHandler>,
    ) -> Self {
        Self {
            client,
            child: Mutex::new(child),
            server_version,
            trust,
            handler,
            document_version: AtomicU64::new(1),
            opened: Mutex::new(BTreeMap::new()),
            stopped: AtomicBool::new(false),
        }
    }

    #[must_use]
    pub fn server_version(&self) -> &str {
        &self.server_version
    }

    #[must_use]
    pub const fn trust(&self) -> ProjectExecutionTrust {
        self.trust
    }

    /// The next document version. Strictly increasing, per host.
    #[must_use]
    pub fn next_document_version(&self) -> i64 {
        i64::try_from(self.document_version.fetch_add(1, Ordering::SeqCst)).unwrap_or(i64::MAX)
    }

    /// What this connection last handed the server for `uri`, recording
    /// `text` as what it holds now.
    ///
    /// `None` the first time a URI is seen -- which is exactly the
    /// `didOpen` / `didChange` decision, and for `didChange` also what
    /// the replacement replaces.
    pub fn exchange_text(&self, uri: &str, text: &str) -> Option<String> {
        self.opened
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(uri.to_owned(), text.to_owned())
    }

    /// How many times the server has settled since it started.
    #[must_use]
    pub fn load_completions(&self) -> u64 {
        *self
            .handler
            .completions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Block until the server has settled more times than `seen`, or
    /// the deadline passes.
    ///
    /// The barrier, and the reason there is no sleep anywhere in this
    /// tier. A caller records [`Self::load_completions`] *before* it
    /// asks for work and waits for that number to move, so a settling
    /// that arrives between the two cannot be missed.
    ///
    /// Returns whether the count moved.
    #[must_use]
    pub fn wait_for_quiescent(&self, seen: u64, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut count = self
            .handler
            .completions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while *count <= seen {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            let (next, timed_out) = self
                .handler
                .signal
                .wait_timeout(count, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            count = next;
            if timed_out.timed_out() && *count <= seen {
                return false;
            }
        }
        true
    }

    #[must_use]
    pub fn sent_methods(&self) -> Vec<String> {
        self.client.sent_methods()
    }

    /// Ask one typed question, or make one statement.
    ///
    /// # Errors
    /// When the trust gate refuses the request, the connection fails, or
    /// the answer cannot be read.
    pub fn call(
        &self,
        request: &RustRequest,
        cancel: &CancelToken,
    ) -> Result<RustResponse, HostError> {
        // The trust gate, at the last possible moment. Placing it here
        // rather than at the caller means there is exactly one door, and
        // a future caller cannot forget to check.
        if request.loads_projects() && !self.trust.may_load_projects() {
            return Err(HostError::new(format!(
                "refusing {}: this Workspace is {} for project execution",
                request.wire().0,
                self.trust
            )));
        }
        let (wire, params) = request.wire();
        if request.is_notification() {
            self.client
                .notify(wire, params)
                .map_err(|failure| HostError::new(failure.to_string()))?;
            return Ok(RustResponse::Delivered);
        }
        match self.client.request(wire, params, cancel) {
            Ok(result) => {
                decode(request, &result).map_err(|error| HostError::new(error.to_string()))
            }
            Err(RpcFailure::Remote(error)) if error.code == METHOD_NOT_FOUND => {
                Ok(RustResponse::Unsupported(wire.to_owned()))
            }
            // Withdrawn, not answered. A caller that read this as an
            // empty answer would publish "nothing is there", which is
            // the one thing it certainly does not mean.
            Err(RpcFailure::Remote(error)) if error.code == REQUEST_CANCELLED => {
                Ok(RustResponse::Cancelled(wire.to_owned()))
            }
            Err(failure) => Err(HostError::new(failure.to_string())),
        }
    }

    fn child_gone(&self) -> bool {
        let mut child = self
            .child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match child.as_mut() {
            Some(process) => matches!(process.try_wait(), Ok(Some(_)) | Err(_)),
            None => false,
        }
    }
}

impl SemanticRuntimeHost for RustHost {
    fn execute(
        &self,
        request: &RuntimeRequest,
        cancel: &CancelToken,
    ) -> Result<RuntimeResponse, HostError> {
        let decoded: RustRequest = serde_json::from_slice(&request.payload)
            .map_err(|error| HostError::new(format!("undecodable request payload: {error}")))?;
        let answer = self.call(&decoded, cancel)?;
        let payload = serde_json::to_vec(&answer)
            .map_err(|error| HostError::new(format!("unencodable response: {error}")))?;
        Ok(RuntimeResponse { payload })
    }

    fn health(&self) -> HostHealth {
        if self.stopped.load(Ordering::SeqCst) || !self.client.is_open() || self.child_gone() {
            return HostHealth::Crashed;
        }
        HostHealth::Healthy
    }

    fn shutdown(&self) {
        if self.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        let cancel = CancelToken::new();
        let _ = self
            .client
            .request(method::SHUTDOWN, None, &cancel)
            .map_err(|_| ());
        let _ = self.client.notify(method::EXIT, None);

        let mut child = self
            .child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(process) = child.as_mut() {
            let deadline = Instant::now() + EXIT_GRACE;
            loop {
                match process.try_wait() {
                    Ok(Some(_)) | Err(_) => break,
                    Ok(None) if Instant::now() >= deadline => {
                        let _ = process.kill();
                        let _ = process.wait();
                        break;
                    }
                    Ok(None) => thread::sleep(Duration::from_millis(25)),
                }
            }
        }
        drop(child);
        self.client.close();
    }

    fn concurrency(&self) -> HostConcurrency {
        // Serial. One Roslyn workspace per process, and a project load
        // is a whole-solution operation; overlapping queries with a
        // reload is how a caller gets an answer about a solution state
        // that no longer exists.
        HostConcurrency::Serial
    }

    fn resource_usage(&self) -> ResourceUsage {
        ResourceUsage::UNAVAILABLE
    }
}

impl Drop for RustHost {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::{ClientHandler, RustHost, RustSettings};
    use crate::{
        lsp::jsonrpc::{
            Client, ServerHandler,
            testing::{Pipe, Sink},
        },
        runtime::{CancelToken, SemanticRuntimeHost},
        rust_semantic::protocol::{ProjectExecutionTrust, RustRequest, method},
    };

    /// A host over an in-process pipe. The sender and receiver are
    /// returned so they outlive the client: dropping the receiver makes
    /// every write fail, which would look like a refusal.
    type Connected = (
        Arc<ClientHandler>,
        RustHost,
        std::sync::mpsc::Sender<Vec<u8>>,
        std::sync::mpsc::Receiver<Vec<u8>>,
    );

    fn connected(trust: ProjectExecutionTrust) -> Connected {
        let (sender, reader) = Pipe::new();
        let (writer, received) = Sink::new();
        let handler = Arc::new(ClientHandler::new(RustSettings));
        let client = Client::new(Box::new(reader), Box::new(writer), handler.clone());
        let host = RustHost::new(client, None, "1.98.1".to_owned(), trust, handler.clone());
        (handler, host, sender, received)
    }

    /// The barrier counts settlings, not statuses.
    ///
    /// The server reports how it is, repeatedly. A waiter asked for the
    /// *work it requested* to finish, so only a false -> true edge may
    /// move the count: a repeated `true` would otherwise let a waiter
    /// return before the work it asked for had even begun.
    #[test]
    fn only_a_settling_moves_the_barrier() {
        let (handler, host, _sender, _received) = connected(ProjectExecutionTrust::Trusted);
        assert_eq!(host.load_completions(), 0);

        // Already settled at startup: nothing has been asked for yet.
        handler.notification(method::SERVER_STATUS, &json!({ "quiescent": true }));
        assert_eq!(host.load_completions(), 0, "a status is not a settling");
        handler.notification(method::SERVER_STATUS, &json!({ "quiescent": true }));
        assert_eq!(host.load_completions(), 0);

        // Work, then settle.
        handler.notification(method::SERVER_STATUS, &json!({ "quiescent": false }));
        assert_eq!(host.load_completions(), 0, "still working");
        handler.notification(method::SERVER_STATUS, &json!({ "quiescent": true }));
        assert_eq!(host.load_completions(), 1);

        // And again, which is what a reload looks like.
        handler.notification(method::SERVER_STATUS, &json!({ "quiescent": false }));
        handler.notification(method::SERVER_STATUS, &json!({ "quiescent": true }));
        assert_eq!(host.load_completions(), 2);
    }

    /// Waiting on a settling that already happened returns at once.
    #[test]
    fn the_barrier_does_not_miss_a_settling_it_was_not_watching_for() {
        let (handler, host, _sender, _received) = connected(ProjectExecutionTrust::Trusted);
        let seen = host.load_completions();
        handler.notification(method::SERVER_STATUS, &json!({ "quiescent": false }));
        handler.notification(method::SERVER_STATUS, &json!({ "quiescent": true }));
        assert!(host.wait_for_quiescent(seen, std::time::Duration::from_millis(50)));
    }

    /// A settling that never comes is a timeout, not a hang.
    #[test]
    fn the_barrier_gives_up_rather_than_waiting_forever() {
        let (_handler, host, _sender, _received) = connected(ProjectExecutionTrust::Trusted);
        assert!(!host.wait_for_quiescent(0, std::time::Duration::from_millis(30)));
    }

    /// An unrelated notification does not move it.
    #[test]
    fn an_unknown_notification_does_not_move_the_barrier() {
        let (handler, host, _sender, _received) = connected(ProjectExecutionTrust::Trusted);
        handler.notification(method::SERVER_STATUS, &json!({ "quiescent": false }));
        handler.notification(method::PROGRESS, &json!({ "token": "x" }));
        handler.notification("textDocument/publishDiagnostics", &json!({}));
        assert_eq!(host.load_completions(), 0);
    }

    /// The trust gate is in the host, so a caller cannot forget it.
    #[test]
    fn an_untrusted_workspace_cannot_put_a_reload_on_the_wire() {
        let (_handler, host, _sender, _received) = connected(ProjectExecutionTrust::Untrusted);
        let refused = host.call(&RustRequest::ReloadWorkspace, &CancelToken::new());
        assert!(
            refused.is_err(),
            "a reload re-reads the project's manifests"
        );
        assert!(
            !host
                .sent_methods()
                .contains(&method::RELOAD_WORKSPACE.to_owned()),
            "and nothing reached the wire"
        );
    }

    /// Untrusted still answers about documents.
    #[test]
    fn an_untrusted_workspace_may_still_open_documents() {
        let (_handler, host, _sender, _received) = connected(ProjectExecutionTrust::Untrusted);
        assert!(
            host.call(
                &RustRequest::OpenDocument {
                    uri: "file:///w/a.rs".into(),
                    text: "fn a() {}".into(),
                    version: 1,
                },
                &CancelToken::new(),
            )
            .is_ok()
        );
    }

    /// Document versions are monotonic, and per connection.
    #[test]
    fn document_versions_are_monotonic_rather_than_wall_clock() {
        let (_handler, host, _sender, _received) = connected(ProjectExecutionTrust::Trusted);
        let versions: Vec<i64> = (0..3).map(|_| host.next_document_version()).collect();
        assert_eq!(versions, vec![1, 2, 3]);
    }

    /// What the connection holds is the connection's, and a fresh one
    /// holds nothing.
    #[test]
    fn a_connection_remembers_only_what_it_was_handed() {
        let (_handler, host, _sender, _received) = connected(ProjectExecutionTrust::Trusted);
        assert_eq!(host.exchange_text("file:///w/a.rs", "one"), None);
        assert_eq!(
            host.exchange_text("file:///w/a.rs", "two").as_deref(),
            Some("one")
        );
        let (_other_handler, other, _other_sender, _other_received) =
            connected(ProjectExecutionTrust::Trusted);
        assert_eq!(
            other.exchange_text("file:///w/a.rs", "one"),
            None,
            "a restarted server has opened nothing"
        );
    }

    /// A closed connection is an unhealthy host.
    ///
    /// The peer going away is exactly what a crash looks like from
    /// here, and the supervisor grades on health rather than on having
    /// been told.
    #[test]
    fn a_closed_connection_is_unhealthy() {
        let (_handler, host, sender, received) = connected(ProjectExecutionTrust::Trusted);
        assert_eq!(host.health(), crate::runtime::HostHealth::Healthy);
        // End the peer's stream, which is what a dead child does.
        drop(sender);
        drop(received);
        // Give the reader thread the closed stream it is waiting on.
        for _ in 0..200 {
            if host.health() == crate::runtime::HostHealth::Crashed {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("a closed connection should read as crashed");
    }
}
