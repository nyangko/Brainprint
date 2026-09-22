//! The Python semantic runtime host: one child process, one connection.
//!
//! Implements [`SemanticRuntimeHost`] over a `pyright-typeserver`
//! process. The supervisor (#19 task 2) owns the lifecycle -- start,
//! idle unload, crash backoff, timeout, cancellation -- and this module
//! owns only what it takes to speak to the backend and to say honestly
//! whether it is still alive.
//!
//! [`HostConcurrency::Serial`], because task 5 confirmed one connection
//! and found `typeServer/connection` unimplemented even though the
//! protocol version advertises multi-connection. Wrapping a serial
//! server in parallel requests would add queueing, not throughput.

use std::{
    process::Child,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};

use super::{
    jsonrpc::{Client, RpcFailure, ServerHandler},
    protocol::{
        METHOD_NOT_FOUND, ProtocolCompatibility, PythonRequest, PythonResponse, SERVER_CANCELLED,
        method,
    },
};
use crate::runtime::{
    CancelToken, HostConcurrency, HostError, HostHealth, ResourceUsage, RuntimeRequest,
    RuntimeResponse, SemanticRuntimeHost,
};

/// How long a graceful `shutdown`/`exit` is given before the child is
/// killed.
const EXIT_GRACE: Duration = Duration::from_millis(2000);

// ---------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------

/// What Brainprint answers when the backend asks for configuration.
///
/// Deterministic and small on purpose. Pyright asks for the `python`,
/// `python.analysis` and `pyright` sections (#19 task 5); Brainprint is
/// the client, so it answers from project settings and never from a
/// developer's editor configuration. Anything not set here is simply
/// absent, which lets `pyrightconfig.json` on disk stay the source of
/// truth for the rest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PythonSettings {
    /// The interpreter, as a locator.
    pub python_path: Option<String>,
    pub venv_path: Option<String>,
    /// Extra import roots, relative to the project root.
    pub extra_paths: Vec<String>,
    pub type_checking_mode: Option<String>,
}

impl PythonSettings {
    /// The deterministic inputs this configuration contributes to a
    /// task 3 [`ConfigBasis`](crate::semantic_index::ConfigBasis).
    ///
    /// Sorted and fully spelled out, so adding a path or changing the
    /// interpreter moves the fingerprint.
    #[must_use]
    pub fn basis_inputs(&self) -> Vec<(String, String)> {
        let mut inputs = vec![
            (
                "python_path".to_owned(),
                self.python_path.clone().unwrap_or_default(),
            ),
            (
                "venv_path".to_owned(),
                self.venv_path.clone().unwrap_or_default(),
            ),
            (
                "type_checking_mode".to_owned(),
                self.type_checking_mode.clone().unwrap_or_default(),
            ),
        ];
        let mut extra = self.extra_paths.clone();
        extra.sort_unstable();
        inputs.push(("extra_paths".to_owned(), extra.join("\u{1f}")));
        inputs.sort();
        inputs
    }

    /// The value for one requested configuration section.
    ///
    /// An unknown section answers `null` -- the protocol's "the client
    /// has no value" -- rather than an invented default.
    #[must_use]
    pub fn section(&self, section: &str) -> Value {
        match section {
            "python" => {
                let mut value = json!({});
                if let Some(path) = &self.python_path {
                    value["pythonPath"] = json!(path);
                }
                if let Some(path) = &self.venv_path {
                    value["venvPath"] = json!(path);
                }
                value
            }
            "python.analysis" => {
                let mut value = json!({});
                if !self.extra_paths.is_empty() {
                    value["extraPaths"] = json!(self.extra_paths);
                }
                if let Some(mode) = &self.type_checking_mode {
                    value["typeCheckingMode"] = json!(mode);
                }
                value
            }
            // Pyright reads its own file from disk; Brainprint adds
            // nothing on top of it.
            "pyright" => json!({}),
            _ => Value::Null,
        }
    }
}

/// Answers the backend's requests and records its notifications.
pub(crate) struct ClientHandler {
    settings: PythonSettings,
    /// How many `typeServer/snapshotChanged` notifications have
    /// arrived. An invalidation signal only: it publishes nothing, and
    /// task 3's basis validation stays the authority on freshness.
    snapshot_changes: Arc<AtomicU64>,
}

impl ClientHandler {
    pub(crate) fn new(settings: PythonSettings, snapshot_changes: Arc<AtomicU64>) -> Self {
        Self {
            settings,
            snapshot_changes,
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
            // Dynamic registration is accepted so the backend's file
            // watcher registers; Brainprint decides what to notify.
            method::REGISTER_CAPABILITY | method::UNREGISTER_CAPABILITY => Some(Value::Null),
            _ => None,
        }
    }

    fn notification(&self, notified: &str, _params: &Value) {
        if notified == method::SNAPSHOT_CHANGED {
            self.snapshot_changes.fetch_add(1, Ordering::SeqCst);
        }
    }
}

// ---------------------------------------------------------------------
// Host
// ---------------------------------------------------------------------

/// One live `pyright-typeserver`.
pub struct PyrightHost {
    client: Client,
    /// `None` for a host driven over an in-process transport in tests.
    child: Mutex<Option<Child>>,
    protocol_version: String,
    compatibility: ProtocolCompatibility,
    snapshot_changes: Arc<AtomicU64>,
    stopped: AtomicBool,
}

impl PyrightHost {
    pub(crate) fn new(
        client: Client,
        child: Option<Child>,
        protocol_version: String,
        compatibility: ProtocolCompatibility,
        snapshot_changes: Arc<AtomicU64>,
    ) -> Self {
        Self {
            client,
            child: Mutex::new(child),
            protocol_version,
            compatibility,
            snapshot_changes,
            stopped: AtomicBool::new(false),
        }
    }

    /// The protocol version the backend reported at startup.
    #[must_use]
    pub fn protocol_version(&self) -> &str {
        &self.protocol_version
    }

    #[must_use]
    pub const fn compatibility(&self) -> ProtocolCompatibility {
        self.compatibility
    }

    /// How many times the backend has invalidated its snapshot.
    #[must_use]
    pub fn snapshot_changes(&self) -> u64 {
        self.snapshot_changes.load(Ordering::SeqCst)
    }

    /// Every method this host has put on the wire.
    #[must_use]
    pub fn sent_methods(&self) -> Vec<String> {
        self.client.sent_methods()
    }

    /// Ask one typed question.
    pub fn call(
        &self,
        request: &PythonRequest,
        cancel: &CancelToken,
    ) -> Result<PythonResponse, HostError> {
        let (wire, params) = request.wire();
        if request.is_notification() {
            self.client
                .notify(wire, params)
                .map_err(|failure| HostError::new(failure.to_string()))?;
            return Ok(PythonResponse::Delivered);
        }
        match self.client.request(wire, params, cancel) {
            Ok(result) => request
                .decode(&result)
                .map_err(|error| HostError::new(error.to_string())),
            // A stale snapshot is an answer about currentness, not a
            // failure: the batch layer retries the whole batch on a
            // fresh snapshot.
            Err(RpcFailure::Remote(error)) if error.code == SERVER_CANCELLED => {
                Ok(PythonResponse::SnapshotStale)
            }
            // An unimplemented method is a capability fact, not zero
            // findings. `textDocument/implementation` answers this.
            Err(RpcFailure::Remote(error)) if error.code == METHOD_NOT_FOUND => {
                Ok(PythonResponse::Unsupported(wire.to_owned()))
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

impl SemanticRuntimeHost for PyrightHost {
    fn execute(
        &self,
        request: &RuntimeRequest,
        cancel: &CancelToken,
    ) -> Result<RuntimeResponse, HostError> {
        let decoded: PythonRequest = serde_json::from_slice(&request.payload)
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
        // resources; a cancelled token is not used here because a
        // shutdown that waits forever is worse than a kill.
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
        HostConcurrency::Serial
    }

    fn resource_usage(&self) -> ResourceUsage {
        // Not observable from safe Rust here, and a fabricated zero
        // would be worse than saying so. Task 9 measures externally.
        ResourceUsage::UNAVAILABLE
    }
}

impl Drop for PyrightHost {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configuration_answers_only_the_sections_pyright_asks_for() {
        let settings = PythonSettings {
            python_path: Some("/venv/bin/python".to_owned()),
            venv_path: None,
            extra_paths: vec!["src".to_owned()],
            type_checking_mode: Some("standard".to_owned()),
        };
        let handler = ClientHandler::new(settings, Arc::new(AtomicU64::new(0)));
        let answer = handler
            .request(
                method::CONFIGURATION,
                &json!({"items": [
                    {"section": "python"},
                    {"section": "python.analysis"},
                    {"section": "pyright"},
                    {"section": "editor.fontSize"},
                ]}),
            )
            .expect("answered");
        let items = answer.as_array().expect("one value per item");
        assert_eq!(items.len(), 4);
        assert_eq!(items[0]["pythonPath"], "/venv/bin/python");
        assert!(items[0].get("venvPath").is_none(), "unset stays absent");
        assert_eq!(items[1]["extraPaths"][0], "src");
        assert_eq!(items[1]["typeCheckingMode"], "standard");
        assert_eq!(items[2], json!({}));
        assert_eq!(
            items[3],
            Value::Null,
            "an unknown section gets no invented default"
        );
    }

    #[test]
    fn configuration_is_deterministic_for_the_same_settings() {
        let settings = PythonSettings {
            python_path: Some("/venv/bin/python".to_owned()),
            extra_paths: vec!["b".to_owned(), "a".to_owned()],
            ..PythonSettings::default()
        };
        let handler = ClientHandler::new(settings.clone(), Arc::new(AtomicU64::new(0)));
        let again = ClientHandler::new(settings, Arc::new(AtomicU64::new(0)));
        let params = json!({"items": [{"section": "python"}, {"section": "python.analysis"}]});
        assert_eq!(
            handler.request(method::CONFIGURATION, &params),
            again.request(method::CONFIGURATION, &params)
        );
    }

    #[test]
    fn settings_contribute_sorted_config_basis_inputs() {
        let settings = PythonSettings {
            python_path: Some("/venv/bin/python".to_owned()),
            extra_paths: vec!["b".to_owned(), "a".to_owned()],
            ..PythonSettings::default()
        };
        let reordered = PythonSettings {
            extra_paths: vec!["a".to_owned(), "b".to_owned()],
            ..settings.clone()
        };
        assert_eq!(settings.basis_inputs(), reordered.basis_inputs());

        let changed = PythonSettings {
            extra_paths: vec!["a".to_owned(), "c".to_owned()],
            ..settings.clone()
        };
        assert_ne!(settings.basis_inputs(), changed.basis_inputs());
    }

    #[test]
    fn a_snapshot_changed_notification_only_counts() {
        let changes = Arc::new(AtomicU64::new(0));
        let handler = ClientHandler::new(PythonSettings::default(), Arc::clone(&changes));
        handler.notification(method::SNAPSHOT_CHANGED, &json!({"old": 3, "new": 4}));
        handler.notification(method::SNAPSHOT_CHANGED, &json!({"old": 4, "new": 5}));
        handler.notification("window/logMessage", &json!({}));
        assert_eq!(changes.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn unknown_peer_requests_are_declined_rather_than_answered() {
        let handler = ClientHandler::new(PythonSettings::default(), Arc::new(AtomicU64::new(0)));
        assert!(
            handler
                .request("window/showMessageRequest", &json!({}))
                .is_none()
        );
        assert_eq!(
            handler.request(method::REGISTER_CAPABILITY, &json!({})),
            Some(Value::Null)
        );
    }
}
