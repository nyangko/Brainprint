//! One live Roslyn language server, behind the task 2 host trait.
//!
//! The one thing here that no other backend needed: a **load barrier**.
//! Roslyn answers nothing useful about a project until it has finished
//! loading it, and it says when that happened
//! ([`method::PROJECT_INITIALIZATION_COMPLETE`]). So this host counts
//! those notifications and lets a caller wait for one, which is what
//! replaces the settle delay every other tier managed to avoid by other
//! means.

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
#[cfg(test)]
use serde_json::json;

use super::protocol::{
    CSharpRequest, CSharpResponse, METHOD_NOT_FOUND, ProjectExecutionTrust, REQUEST_CANCELLED,
    decode, method,
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
pub struct CSharpSettings;

impl CSharpSettings {
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
    settings: CSharpSettings,
    registrations: AtomicU64,
    /// How many times the server has said a project load finished.
    ///
    /// A count rather than a flag: a reload after a project-membership
    /// change sends another one, and "the number went up" is what a
    /// waiter is actually waiting for.
    completions: Mutex<u64>,
    signal: Condvar,
}

impl ClientHandler {
    pub(crate) fn new(settings: CSharpSettings) -> Self {
        Self {
            settings,
            registrations: AtomicU64::new(0),
            completions: Mutex::new(0),
            signal: Condvar::new(),
        }
    }

    fn completed(&self) {
        let mut count = self
            .completions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *count += 1;
        self.signal.notify_all();
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

    fn notification(&self, notified: &str, _params: &Value) {
        if notified == method::PROJECT_INITIALIZATION_COMPLETE {
            self.completed();
        }
    }
}

// ---------------------------------------------------------------------
// Host
// ---------------------------------------------------------------------

/// One live `Microsoft.CodeAnalysis.LanguageServer --stdio`.
pub struct CSharpHost {
    client: Client,
    child: Mutex<Option<Child>>,
    /// Read from the install manifest before launch: this server reports
    /// no `serverInfo`.
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

impl CSharpHost {
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

    /// How many project loads the server has reported finishing.
    #[must_use]
    pub fn load_completions(&self) -> u64 {
        *self
            .handler
            .completions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Block until the server has reported more load completions than
    /// `seen`, or the deadline passes.
    ///
    /// The barrier, and the reason there is no sleep anywhere in this
    /// tier. A caller records [`Self::load_completions`] *before* it
    /// asks for a load and waits for that number to move, so a
    /// completion that arrives between the two cannot be missed.
    ///
    /// Returns whether the count moved.
    #[must_use]
    pub fn wait_for_project_load(&self, seen: u64, timeout: Duration) -> bool {
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
        request: &CSharpRequest,
        cancel: &CancelToken,
    ) -> Result<CSharpResponse, HostError> {
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
            return Ok(CSharpResponse::Delivered);
        }
        match self.client.request(wire, params, cancel) {
            Ok(result) => {
                decode(request, &result).map_err(|error| HostError::new(error.to_string()))
            }
            Err(RpcFailure::Remote(error)) if error.code == METHOD_NOT_FOUND => {
                Ok(CSharpResponse::Unsupported(wire.to_owned()))
            }
            // Withdrawn, not answered. A caller that read this as an
            // empty answer would publish "nothing is there", which is
            // the one thing it certainly does not mean.
            Err(RpcFailure::Remote(error)) if error.code == REQUEST_CANCELLED => {
                Ok(CSharpResponse::Cancelled(wire.to_owned()))
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

impl SemanticRuntimeHost for CSharpHost {
    fn execute(
        &self,
        request: &RuntimeRequest,
        cancel: &CancelToken,
    ) -> Result<RuntimeResponse, HostError> {
        let decoded: CSharpRequest = serde_json::from_slice(&request.payload)
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

impl Drop for CSharpHost {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::{coordinates::Position, jsonrpc::testing};

    fn host(trust: ProjectExecutionTrust) -> (std::sync::mpsc::Sender<Vec<u8>>, CSharpHost) {
        let (to_client, pipe) = testing::Pipe::new();
        let (sink, _from_client) = testing::Sink::new();
        let handler = std::sync::Arc::new(ClientHandler::new(CSharpSettings));
        let client = Client::new(Box::new(pipe), Box::new(sink), handler.clone());
        (
            to_client,
            CSharpHost::new(client, None, "5.4.0-2.26179.14".into(), trust, handler),
        )
    }

    #[test]
    fn an_untrusted_workspace_cannot_put_a_project_load_on_the_wire() {
        let (to_client, host) = host(ProjectExecutionTrust::Untrusted);
        let error = host
            .call(
                &CSharpRequest::OpenSolution {
                    uri: "file:///w/x.sln".into(),
                },
                &CancelToken::new(),
            )
            .expect_err("untrusted must refuse");
        assert!(error.to_string().contains("UNTRUSTED"), "{error}");
        // And nothing reached the connection, which is the part that
        // matters: the refusal is before the send, not after it.
        assert!(
            !host
                .sent_methods()
                .contains(&method::SOLUTION_OPEN.to_owned())
        );
        end_stream(to_client, host);
    }

    #[test]
    fn an_untrusted_workspace_may_still_open_documents() {
        // Measured safe: driving the server with documents alone
        // executed no project code at all, and still answered
        // intra-document definitions.
        let (to_client, host) = host(ProjectExecutionTrust::Untrusted);
        assert_eq!(
            host.call(
                &CSharpRequest::OpenDocument {
                    uri: "file:///w/a.cs".into(),
                    text: "class A {}".into(),
                    version: 1,
                },
                &CancelToken::new(),
            )
            .expect("documents are not project execution"),
            CSharpResponse::Delivered
        );
        assert!(host.sent_methods().contains(&method::DID_OPEN.to_owned()));
        end_stream(to_client, host);
    }

    #[test]
    fn document_versions_are_monotonic_rather_than_wall_clock() {
        let (to_client, host) = host(ProjectExecutionTrust::Trusted);
        let versions: Vec<i64> = (0..4).map(|_| host.next_document_version()).collect();
        assert_eq!(versions, vec![1, 2, 3, 4]);
        end_stream(to_client, host);
    }

    #[test]
    fn the_load_barrier_is_a_count_that_moves_not_a_flag() {
        // A reload after a project-membership change sends another
        // completion, so a waiter has to know the number it started
        // from -- otherwise the *previous* load satisfies it.
        let (to_client, host) = host(ProjectExecutionTrust::Trusted);
        assert_eq!(host.load_completions(), 0);
        assert!(
            !host.wait_for_project_load(0, Duration::from_millis(30)),
            "nothing has completed yet"
        );

        host.handler
            .notification(method::PROJECT_INITIALIZATION_COMPLETE, &json!({}));
        assert_eq!(host.load_completions(), 1);
        assert!(host.wait_for_project_load(0, Duration::from_millis(30)));
        assert!(
            !host.wait_for_project_load(1, Duration::from_millis(30)),
            "a second load has not happened"
        );

        host.handler
            .notification(method::PROJECT_INITIALIZATION_COMPLETE, &json!({}));
        assert!(host.wait_for_project_load(1, Duration::from_millis(30)));
        end_stream(to_client, host);
    }

    #[test]
    fn an_unknown_notification_does_not_move_the_barrier() {
        let (to_client, host) = host(ProjectExecutionTrust::Trusted);
        host.handler
            .notification("textDocument/publishDiagnostics", &json!({}));
        assert_eq!(host.load_completions(), 0);
        end_stream(to_client, host);
    }

    #[test]
    fn a_definition_is_a_request_and_a_document_open_is_not() {
        let definition = CSharpRequest::Definition {
            uri: "file:///w/a.cs".into(),
            position: Position::new(1, 2),
        };
        assert!(!definition.is_notification());
        assert!(
            CSharpRequest::OpenDocument {
                uri: "file:///w/a.cs".into(),
                text: String::new(),
                version: 1,
            }
            .is_notification()
        );
    }

    fn end_stream(to_client: std::sync::mpsc::Sender<Vec<u8>>, host: CSharpHost) {
        drop(to_client);
        while host.health() != HostHealth::Crashed {
            thread::sleep(Duration::from_millis(5));
        }
        drop(host);
    }
}
