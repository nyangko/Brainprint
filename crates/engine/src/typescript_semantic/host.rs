//! One live TypeScript native LSP server, behind the task 2 host trait.
//!
//! Everything here is about keeping one child process in step with one
//! connection: answering the requests the *server* makes of the client,
//! turning a typed [`TypeScriptRequest`] into a frame and an answer back
//! into a typed [`TypeScriptResponse`], and stopping cleanly. Nothing
//! here decides what a fact means -- that is the adapter's job -- and
//! nothing here is persisted.

use std::{
    process::Child,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};

use super::protocol::{
    METHOD_NOT_FOUND, PositionEncodingChoice, TypeScriptRequest, TypeScriptResponse, decode, method,
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

/// What Brainprint tells the backend when it asks for configuration.
///
/// The measured server asks for four sections during `initialized`:
/// `js/ts`, `typescript`, `javascript` and `editor`. It is a language
/// service being asked to behave like an editor's, and Brainprint is not
/// an editor: every section is answered with an empty object, which
/// means "no client overrides -- use the project's own settings".
///
/// That is deliberate and it is identity-relevant. The whole point of
/// this tier is that the answer depends on the *project*, so anything
/// this client injected on top would have to appear in the
/// [`AnalysisContext`](crate::semantic::AnalysisContext) or two
/// Workspaces with identical sources could disagree. Keeping the
/// overrides empty keeps that surface at zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TypeScriptSettings;

impl TypeScriptSettings {
    /// What to answer for one requested configuration section.
    #[must_use]
    pub fn section(self, section: &str) -> Value {
        match section {
            "js/ts" | "typescript" | "javascript" | "editor" => json!({}),
            // A section this build has never seen gets `null`, which
            // LSP defines as "no value", rather than an empty object
            // that might read as a deliberate override.
            _ => Value::Null,
        }
    }

    /// The configuration inputs this client contributes to a
    /// publication basis.
    ///
    /// Empty, and that is the fact worth recording: nothing this client
    /// sends can change what the backend concludes, so nothing about it
    /// needs to be fingerprinted. If a section is ever given a real
    /// value it belongs here, and the empty `Vec` is what makes that
    /// omission visible instead of silent.
    #[must_use]
    pub fn basis_inputs(self) -> Vec<(String, String)> {
        Vec::new()
    }
}

/// Answers the backend's requests and records what it registered.
pub(crate) struct ClientHandler {
    settings: TypeScriptSettings,
    /// How many dynamic capability registrations have arrived.
    ///
    /// Observation only. The measured server registers a
    /// `didChangeWatchedFiles` glob over the workspace once it has
    /// loaded a project, which is how it says "I am watching through
    /// you now". Brainprint notifies regardless -- it does not wait for
    /// the registration and does not filter against the glob -- so this
    /// is telemetry and a test hook, never a gate.
    registrations: AtomicU64,
}

impl ClientHandler {
    pub(crate) const fn new(settings: TypeScriptSettings) -> Self {
        Self {
            settings,
            registrations: AtomicU64::new(0),
        }
    }
}

impl ServerHandler for ClientHandler {
    fn request(&self, requested: &str, params: &Value) -> Option<Value> {
        match requested {
            // Must be answered. The probe measured the server waiting
            // on this reply during `initialized`; leaving it unanswered
            // wedged the connection and the next `shutdown` timed out.
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

    fn notification(&self, _notified: &str, _params: &Value) {}
}

// ---------------------------------------------------------------------
// Host
// ---------------------------------------------------------------------

/// One live `tsc --lsp --stdio`.
pub struct TypeScriptHost {
    client: Client,
    /// `None` for a host driven over an in-process transport in tests.
    child: Mutex<Option<Child>>,
    server_name: String,
    server_version: String,
    encoding: PositionEncodingChoice,
    handler_registrations: std::sync::Arc<ClientHandler>,
    stopped: AtomicBool,
}

impl TypeScriptHost {
    pub(crate) fn new(
        client: Client,
        child: Option<Child>,
        server_name: String,
        server_version: String,
        encoding: PositionEncodingChoice,
        handler: std::sync::Arc<ClientHandler>,
    ) -> Self {
        Self {
            client,
            child: Mutex::new(child),
            server_name,
            server_version,
            encoding,
            handler_registrations: handler,
            stopped: AtomicBool::new(false),
        }
    }

    /// What the server called itself at startup.
    #[must_use]
    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// The version the server reported at startup.
    ///
    /// This is the backend version that goes into
    /// [`ToolchainIdentity`](crate::semantic::ToolchainIdentity) -- read
    /// from the running process, not from the package manifest, so a
    /// manifest that disagrees with the binary cannot silently define
    /// the identity.
    #[must_use]
    pub fn server_version(&self) -> &str {
        &self.server_version
    }

    /// The position encoding the handshake settled on.
    #[must_use]
    pub const fn encoding(&self) -> PositionEncodingChoice {
        self.encoding
    }

    /// How many dynamic capability registrations the server has made.
    #[must_use]
    pub fn registrations(&self) -> u64 {
        self.handler_registrations
            .registrations
            .load(Ordering::SeqCst)
    }

    /// Every method this host has put on the wire.
    ///
    /// Exists so the "filesystem truth, no editor overlay" decision is
    /// checkable by a test rather than only documented.
    #[must_use]
    pub fn sent_methods(&self) -> Vec<String> {
        self.client.sent_methods()
    }

    /// Ask one typed question.
    pub fn call(
        &self,
        request: &TypeScriptRequest,
        cancel: &CancelToken,
    ) -> Result<TypeScriptResponse, HostError> {
        let (wire, params) = request.wire();
        if request.is_notification() {
            self.client
                .notify(wire, params)
                .map_err(|failure| HostError::new(failure.to_string()))?;
            return Ok(TypeScriptResponse::Delivered);
        }
        match self.client.request(wire, params, cancel) {
            Ok(result) => {
                decode(request, &result).map_err(|error| HostError::new(error.to_string()))
            }
            // An unimplemented method is a capability fact, not zero
            // findings. The measured 7.0.2 implements everything this
            // adapter sends, so reaching here means a server outside the
            // compatibility class -- exactly the case that must not
            // arrive as an empty result.
            Err(RpcFailure::Remote(error)) if error.code == METHOD_NOT_FOUND => {
                Ok(TypeScriptResponse::Unsupported(wire.to_owned()))
            }
            Err(failure) => Err(HostError::new(failure.to_string())),
        }
    }

    /// Whether the child has exited.
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

impl SemanticRuntimeHost for TypeScriptHost {
    fn execute(
        &self,
        request: &RuntimeRequest,
        cancel: &CancelToken,
    ) -> Result<RuntimeResponse, HostError> {
        let decoded: TypeScriptRequest = serde_json::from_slice(&request.payload)
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
        // Ask nicely first, so the backend can release its own
        // resources. Note what is *not* here: no `$/cancelRequest` for
        // anything still in flight, because the measured server ignores
        // it (see `protocol::CANCELLATION_HONOURED`). The grace window
        // and then the kill are what actually bound this.
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
        // Serial. The server is internally concurrent and would accept
        // overlap, but it does not honour `$/cancelRequest`, so an
        // abandoned request keeps running with no way to reclaim it.
        // One at a time bounds that to one.
        HostConcurrency::Serial
    }

    fn resource_usage(&self) -> ResourceUsage {
        // Not observable from safe Rust here, and a fabricated zero
        // would be worse than saying so.
        ResourceUsage::UNAVAILABLE
    }
}

impl Drop for TypeScriptHost {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::{coordinates::PositionEncoding, jsonrpc::testing};

    #[test]
    fn every_section_the_measured_server_asks_for_is_answered() {
        let settings = TypeScriptSettings;
        // Verbatim from the probe's `workspace/configuration` request.
        for section in ["js/ts", "typescript", "javascript", "editor"] {
            assert_eq!(settings.section(section), json!({}), "{section}");
        }
        assert_eq!(settings.section("something-new"), Value::Null);
        assert!(settings.basis_inputs().is_empty());
    }

    #[test]
    fn a_configuration_request_gets_one_answer_per_item() {
        let handler = ClientHandler::new(TypeScriptSettings);
        let answer = handler
            .request(
                method::CONFIGURATION,
                &json!({ "items": [
                    { "section": "js/ts" },
                    { "section": "typescript" },
                    { "section": "javascript" },
                    { "section": "editor" },
                ]}),
            )
            .expect("configuration must be answered");
        // One reply per item, in order: a shorter array wedges the
        // server, which the probe observed as a hung `shutdown`.
        assert_eq!(answer.as_array().map(Vec::len), Some(4));
    }

    #[test]
    fn capability_registration_is_accepted_and_counted() {
        let handler = ClientHandler::new(TypeScriptSettings);
        assert_eq!(
            handler.request(method::REGISTER_CAPABILITY, &json!({ "registrations": [] })),
            Some(Value::Null)
        );
        assert_eq!(handler.registrations.load(Ordering::SeqCst), 1);
        assert_eq!(
            handler.request(method::UNREGISTER_CAPABILITY, &json!({})),
            Some(Value::Null)
        );
        // A request this client does not implement gets no answer,
        // which the transport reports as MethodNotFound rather than
        // inventing a result.
        assert_eq!(
            handler.request("window/showMessageRequest", &json!({})),
            None
        );
    }

    #[test]
    fn a_watched_file_notification_needs_no_reply() {
        let (to_client, pipe) = testing::Pipe::new();
        let (sink, from_client) = testing::Sink::new();
        let handler = std::sync::Arc::new(ClientHandler::new(TypeScriptSettings));
        let client = Client::new(Box::new(pipe), Box::new(sink), handler.clone());
        let host = TypeScriptHost::new(
            client,
            None,
            "typescript-go".into(),
            "7.0.2".into(),
            PositionEncodingChoice::negotiated(PositionEncoding::Utf8),
            handler,
        );
        let answer = host
            .call(
                &TypeScriptRequest::WatchedFilesChanged {
                    changes: Vec::new(),
                },
                &CancelToken::new(),
            )
            .expect("delivered");
        assert_eq!(answer, TypeScriptResponse::Delivered);
        assert!(
            host.sent_methods()
                .contains(&method::DID_CHANGE_WATCHED_FILES.to_owned())
        );
        // It really went on the wire, and it really carried no id --
        // a notification the server would try to answer would leave a
        // reply nothing is waiting for and desynchronize the queue.
        let raw = from_client.recv().expect("frame");
        let sent: Value = serde_json::from_slice(&raw).expect("json");
        assert_eq!(sent["method"], method::DID_CHANGE_WATCHED_FILES);
        assert!(sent.get("id").is_none());
        end_stream(to_client, host);
    }

    #[test]
    fn no_document_sync_method_can_reach_the_wire() {
        let (to_client, pipe) = testing::Pipe::new();
        let (sink, _from_client) = testing::Sink::new();
        let handler = std::sync::Arc::new(ClientHandler::new(TypeScriptSettings));
        let client = Client::new(Box::new(pipe), Box::new(sink), handler.clone());
        let host = TypeScriptHost::new(
            client,
            None,
            "typescript-go".into(),
            "7.0.2".into(),
            PositionEncodingChoice::negotiated(PositionEncoding::Utf8),
            handler,
        );
        let _ = host.call(
            &TypeScriptRequest::WatchedFilesChanged {
                changes: Vec::new(),
            },
            &CancelToken::new(),
        );
        let sent = host.sent_methods();
        for forbidden in super::super::protocol::FORBIDDEN_SYNC_METHODS {
            assert!(!sent.contains(&forbidden.to_owned()), "{forbidden}");
        }
        end_stream(to_client, host);
    }

    /// Retire a test host without waiting on a peer that will never
    /// answer `shutdown`.
    ///
    /// Dropping the sender ends the byte stream, the reader thread
    /// closes the client, and the drop-time `shutdown` fails fast
    /// instead of blocking. A real backend answers; a `testing::Pipe`
    /// has nobody behind it.
    fn end_stream(to_client: std::sync::mpsc::Sender<Vec<u8>>, host: TypeScriptHost) {
        drop(to_client);
        while host.health() != HostHealth::Crashed {
            thread::sleep(Duration::from_millis(5));
        }
        drop(host);
    }
}
