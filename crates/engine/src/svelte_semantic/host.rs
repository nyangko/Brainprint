//! One live Svelte language server, behind the task 2 host trait.
//!
//! Everything here is about keeping one child process in step with one
//! connection: answering the requests the *server* makes of the client,
//! turning a typed [`SvelteRequest`] into a frame and an answer back
//! into a typed [`SvelteResponse`], and stopping cleanly. Nothing here
//! decides what a fact means, and nothing here is persisted.

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
    INTERNAL_ERROR, METHOD_NOT_FOUND, SvelteRequest, SvelteResponse, UNOPENED_DOCUMENT, decode,
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

/// What Brainprint tells the backend when it asks for configuration.
///
/// Empty objects, which mean "no client overrides". The reasoning is the
/// same as the TypeScript backend's and matters more here: the whole
/// point of the tier is that the answer depends on the *project*, so
/// anything this client injected would have to appear in the
/// [`AnalysisContext`](crate::semantic::AnalysisContext) or two
/// Workspaces with identical sources could disagree.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SvelteSettings;

impl SvelteSettings {
    /// What to answer for one requested configuration section.
    #[must_use]
    pub fn section(self, section: &str) -> Value {
        match section {
            "svelte" | "javascript" | "typescript" | "css" | "less" | "scss" | "html" | "emmet"
            | "prettier" => json!({}),
            // A section this build has never seen gets `null`, which LSP
            // defines as "no value", rather than an empty object that
            // might read as a deliberate override.
            _ => Value::Null,
        }
    }

    /// The configuration inputs this client contributes to a
    /// publication basis.
    ///
    /// Empty, and that is the fact worth recording: nothing this client
    /// sends can change what the backend concludes. If a section is ever
    /// given a real value it belongs here, and the empty `Vec` is what
    /// makes that omission visible instead of silent.
    #[must_use]
    pub fn basis_inputs(self) -> Vec<(String, String)> {
        Vec::new()
    }
}

/// Answers the backend's requests and records what it registered.
pub(crate) struct ClientHandler {
    settings: SvelteSettings,
    /// How many dynamic capability registrations have arrived.
    /// Observation only -- Brainprint notifies regardless.
    registrations: AtomicU64,
}

impl ClientHandler {
    pub(crate) const fn new(settings: SvelteSettings) -> Self {
        Self {
            settings,
            registrations: AtomicU64::new(0),
        }
    }
}

impl ServerHandler for ClientHandler {
    fn request(&self, requested: &str, params: &Value) -> Option<Value> {
        match requested {
            // Must be answered, or the connection wedges.
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

/// One live `node .../svelte-language-server/bin/server.js --stdio`.
pub struct SvelteHost {
    client: Client,
    /// `None` for a host driven over an in-process transport in tests.
    child: Mutex<Option<Child>>,
    /// Read from the install manifest before launch: this server
    /// reports no `serverInfo`, so there is nothing in the protocol to
    /// read it from.
    server_version: String,
    handler: std::sync::Arc<ClientHandler>,
    stopped: AtomicBool,
}

impl SvelteHost {
    pub(crate) fn new(
        client: Client,
        child: Option<Child>,
        server_version: String,
        handler: std::sync::Arc<ClientHandler>,
    ) -> Self {
        Self {
            client,
            child: Mutex::new(child),
            server_version,
            handler,
            stopped: AtomicBool::new(false),
        }
    }

    /// The version the install manifest declared.
    #[must_use]
    pub fn server_version(&self) -> &str {
        &self.server_version
    }

    /// How many dynamic capability registrations the server has made.
    #[must_use]
    pub fn registrations(&self) -> u64 {
        self.handler.registrations.load(Ordering::SeqCst)
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
    ///
    /// # Errors
    /// When the connection fails or the answer cannot be read.
    pub fn call(
        &self,
        request: &SvelteRequest,
        cancel: &CancelToken,
    ) -> Result<SvelteResponse, HostError> {
        let (wire, params) = request.wire();
        if request.is_notification() {
            self.client
                .notify(wire, params)
                .map_err(|failure| HostError::new(failure.to_string()))?;
            return Ok(SvelteResponse::Delivered);
        }
        match self.client.request(wire, params, cancel) {
            Ok(result) => {
                decode(request, &result).map_err(|error| HostError::new(error.to_string()))
            }
            Err(RpcFailure::Remote(error)) if error.code == METHOD_NOT_FOUND => {
                Ok(SvelteResponse::Unsupported(wire.to_owned()))
            }
            // The measured failure for a document nobody synchronized.
            // Kept as its own answer rather than folded into a generic
            // backend error, because it is a statement about *this
            // tier's* ordering and not about the component.
            Err(RpcFailure::Remote(error))
                if error.code == INTERNAL_ERROR && error.message.contains(UNOPENED_DOCUMENT) =>
            {
                Ok(SvelteResponse::Unsynchronized(
                    request.uri().unwrap_or_default().to_owned(),
                ))
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

impl SemanticRuntimeHost for SvelteHost {
    fn execute(
        &self,
        request: &RuntimeRequest,
        cancel: &CancelToken,
    ) -> Result<RuntimeResponse, HostError> {
        let decoded: SvelteRequest = serde_json::from_slice(&request.payload)
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
        // Serial. The server keeps one TypeScript language service per
        // project and regenerates intermediate representations on
        // demand; one question at a time is what keeps that bounded.
        HostConcurrency::Serial
    }

    fn resource_usage(&self) -> ResourceUsage {
        ResourceUsage::UNAVAILABLE
    }
}

impl Drop for SvelteHost {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::{coordinates::Position, jsonrpc::testing};

    #[test]
    fn every_section_the_measured_server_asks_for_is_answered() {
        let settings = SvelteSettings;
        for section in ["svelte", "javascript", "typescript", "css", "html"] {
            assert_eq!(settings.section(section), json!({}), "{section}");
        }
        assert_eq!(settings.section("something-new"), Value::Null);
        assert!(settings.basis_inputs().is_empty());
    }

    #[test]
    fn a_configuration_request_gets_one_answer_per_item() {
        let handler = ClientHandler::new(SvelteSettings);
        let answer = handler
            .request(
                method::CONFIGURATION,
                &json!({ "items": [{ "section": "svelte" }, { "section": "typescript" }]}),
            )
            .expect("configuration must be answered");
        // A shorter array wedges the server.
        assert_eq!(answer.as_array().map(Vec::len), Some(2));
    }

    #[test]
    fn no_document_sync_method_can_reach_the_wire() {
        let (to_client, pipe) = testing::Pipe::new();
        let (sink, from_client) = testing::Sink::new();
        let handler = std::sync::Arc::new(ClientHandler::new(SvelteSettings));
        let client = Client::new(Box::new(pipe), Box::new(sink), handler.clone());
        let host = SvelteHost::new(client, None, "0.18.4".into(), handler);

        let answer = host
            .call(
                &SvelteRequest::WatchedFilesChanged {
                    changes: Vec::new(),
                },
                &CancelToken::new(),
            )
            .expect("delivered");
        assert_eq!(answer, SvelteResponse::Delivered);

        // It really went on the wire, and it really carried no id.
        let raw = from_client.recv().expect("frame");
        let sent: Value = serde_json::from_slice(&raw).expect("json");
        assert_eq!(sent["method"], method::DID_CHANGE_WATCHED_FILES);
        assert!(sent.get("id").is_none());

        for forbidden in super::super::protocol::FORBIDDEN_SYNC_METHODS {
            assert!(
                !host.sent_methods().contains(&forbidden.to_owned()),
                "{forbidden}"
            );
        }
        end_stream(to_client, host);
    }

    #[test]
    fn a_definition_is_a_request_and_a_watched_change_is_not() {
        let definition = SvelteRequest::Definition {
            uri: "file:///a.svelte".into(),
            position: Position::new(1, 2),
        };
        assert!(!definition.is_notification());
        assert_eq!(definition.uri(), Some("file:///a.svelte"));
    }

    /// Retire a test host without waiting on a peer that will never
    /// answer `shutdown`.
    fn end_stream(to_client: std::sync::mpsc::Sender<Vec<u8>>, host: SvelteHost) {
        drop(to_client);
        while host.health() != HostHealth::Crashed {
            thread::sleep(Duration::from_millis(5));
        }
        drop(host);
    }
}
