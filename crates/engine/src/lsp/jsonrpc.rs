//! A `Content-Length`-framed JSON-RPC client over a byte stream.
//!
//! Backend-neutral on purpose: it knows about framing, request ids,
//! response correlation, server-initiated requests and notifications,
//! and nothing about any server, any language, or LSP itself. The
//! transport is two
//! streams, so a test can drive it over an in-process pipe and a
//! launcher can drive it over a child process's stdio without either
//! side knowing the difference.
//!
//! Request ids live and die here. Nothing outside this module can see
//! one, which is why no backend request id can reach a Resource,
//! Symbol, Relation or piece of
//! [`SemanticEvidence`](crate::semantic::SemanticEvidence).

use std::{
    collections::HashMap,
    error::Error,
    fmt,
    io::{BufRead, BufReader, Read, Write},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};

use serde_json::{Value, json};

use crate::runtime::CancelToken;

/// How often a waiting request re-checks its cancel token.
///
/// A poll rather than an interrupt because the backend cannot be
/// interrupted anyway: #19 task 5 measured `$/cancelRequest` being
/// accepted and then ignored, with the full answer arriving afterwards.
const CANCEL_POLL: Duration = Duration::from_millis(20);

/// An error the peer reported, in its own words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteError {
    pub code: i64,
    pub message: String,
}

/// Why a call did not return an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcFailure {
    /// The peer answered with an error object.
    Remote(RemoteError),
    /// The stream broke, or the peer went away.
    Closed,
    /// Writing or reading the stream failed.
    Transport(String),
    /// The caller's [`CancelToken`] was set while waiting.
    Cancelled,
}

impl RpcFailure {
    /// The peer's error code, when the peer is what failed.
    #[must_use]
    pub const fn code(&self) -> Option<i64> {
        match self {
            Self::Remote(error) => Some(error.code),
            _ => None,
        }
    }
}

impl fmt::Display for RpcFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Remote(error) => {
                write!(formatter, "peer error {}: {}", error.code, error.message)
            }
            Self::Closed => formatter.write_str("the connection is closed"),
            Self::Transport(detail) => write!(formatter, "transport failure: {detail}"),
            Self::Cancelled => formatter.write_str("the request was cancelled"),
        }
    }
}

impl Error for RpcFailure {}

/// What answers the peer's own requests and receives its notifications.
pub trait ServerHandler: Send + Sync {
    /// Answer a peer-initiated request. `None` means "method not
    /// found", which is answered as such rather than left hanging: a
    /// peer waiting forever on an unanswered request is a deadlock.
    fn request(&self, method: &str, params: &Value) -> Option<Value>;

    fn notification(&self, method: &str, params: &Value);
}

/// A handler that answers nothing and ignores everything.
pub struct IgnoreServer;

impl ServerHandler for IgnoreServer {
    fn request(&self, _method: &str, _params: &Value) -> Option<Value> {
        None
    }
    fn notification(&self, _method: &str, _params: &Value) {}
}

type Slot = Option<Result<Value, RemoteError>>;

struct Inner {
    writer: Mutex<Box<dyn Write + Send>>,
    pending: Mutex<HashMap<i64, Slot>>,
    answered: Condvar,
    next_id: AtomicI64,
    open: AtomicBool,
    /// Every method this client has put on the wire, in order. Small,
    /// bounded by the request plan, and the only way to assert that a
    /// forbidden notification was never sent.
    sent: Mutex<Vec<String>>,
    malformed: AtomicU64,
}

impl Inner {
    fn send(&self, message: &Value) -> Result<(), RpcFailure> {
        if !self.open.load(Ordering::SeqCst) {
            return Err(RpcFailure::Closed);
        }
        let body = serde_json::to_vec(message)
            .map_err(|error| RpcFailure::Transport(error.to_string()))?;
        let mut writer = self
            .writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        write!(writer, "Content-Length: {}\r\n\r\n", body.len())
            .and_then(|()| writer.write_all(&body))
            .and_then(|()| writer.flush())
            .map_err(|error| RpcFailure::Transport(error.to_string()))
    }

    /// Stop taking work and fail everyone still waiting.
    ///
    /// An answer that already arrived is kept. The peer legitimately
    /// answers and then closes -- a server that exits right after its
    /// last reply is normal -- and throwing that reply away would turn
    /// a completed request into a spurious transport failure.
    fn close(&self) {
        self.open.store(false, Ordering::SeqCst);
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending.retain(|_, slot| slot.is_some());
        drop(pending);
        self.answered.notify_all();
    }
}

/// One JSON-RPC connection.
pub struct Client {
    inner: Arc<Inner>,
}

impl Client {
    /// Start a client over `reader`/`writer`, with `handler` answering
    /// whatever the peer asks.
    ///
    /// Spawns one reader thread, which exits when the stream ends.
    pub fn new(
        reader: Box<dyn Read + Send>,
        writer: Box<dyn Write + Send>,
        handler: Arc<dyn ServerHandler>,
    ) -> Self {
        let inner = Arc::new(Inner {
            writer: Mutex::new(writer),
            pending: Mutex::new(HashMap::new()),
            answered: Condvar::new(),
            next_id: AtomicI64::new(1),
            open: AtomicBool::new(true),
            sent: Mutex::new(Vec::new()),
            malformed: AtomicU64::new(0),
        });
        let reading = Arc::clone(&inner);
        thread::spawn(move || {
            read_loop(&reading, BufReader::new(reader), handler.as_ref());
            reading.close();
        });
        Self { inner }
    }

    /// Whether the connection is still usable.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.inner.open.load(Ordering::SeqCst)
    }

    /// How many unreadable frames arrived. A malformed message is
    /// counted and skipped; it never fails a pending request and never
    /// desynchronizes the stream.
    #[must_use]
    pub fn malformed_frames(&self) -> u64 {
        self.inner.malformed.load(Ordering::SeqCst)
    }

    /// Every method sent, in order.
    #[must_use]
    pub fn sent_methods(&self) -> Vec<String> {
        self.inner
            .sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn notify(&self, method: &str, params: Option<Value>) -> Result<(), RpcFailure> {
        self.record(method);
        let mut message = json!({ "jsonrpc": "2.0", "method": method });
        if let Some(params) = params {
            message["params"] = params;
        }
        self.inner.send(&message)
    }

    /// Send a request and wait for its answer.
    ///
    /// Returns [`RpcFailure::Cancelled`] as soon as `cancel` is set. The
    /// peer is told with `$/cancelRequest`, but the answer is not waited
    /// for: task 5 proved Pyright finishes the work regardless, and a
    /// late answer is simply dropped when it arrives.
    pub fn request(
        &self,
        method: &str,
        params: Option<Value>,
        cancel: &CancelToken,
    ) -> Result<Value, RpcFailure> {
        self.record(method);
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);

        {
            let mut pending = self.lock_pending();
            if !self.inner.open.load(Ordering::SeqCst) {
                return Err(RpcFailure::Closed);
            }
            pending.insert(id, None);
        }

        let mut message = json!({ "jsonrpc": "2.0", "id": id, "method": method });
        if let Some(params) = params {
            message["params"] = params;
        }
        if let Err(failure) = self.inner.send(&message) {
            self.lock_pending().remove(&id);
            return Err(failure);
        }

        let mut pending = self.lock_pending();
        loop {
            match pending.get(&id) {
                // The reader filled the slot.
                Some(Some(_)) => {
                    let answer = pending.remove(&id).flatten().expect("a filled slot");
                    return answer.map_err(RpcFailure::Remote);
                }
                // The connection closed and cleared every slot.
                None => return Err(RpcFailure::Closed),
                Some(None) => {}
            }
            if cancel.is_cancelled() {
                pending.remove(&id);
                drop(pending);
                // Best effort, and explicitly not depended on.
                let _ = self.inner.send(
                    &json!({ "jsonrpc": "2.0", "method": "$/cancelRequest", "params": { "id": id } }),
                );
                return Err(RpcFailure::Cancelled);
            }
            let (next, _) = self
                .inner
                .answered
                .wait_timeout(pending, CANCEL_POLL)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pending = next;
        }
    }

    /// Stop accepting work and wake every waiter.
    pub fn close(&self) {
        self.inner.close();
    }

    fn record(&self, method: &str) {
        self.inner
            .sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(method.to_owned());
    }

    fn lock_pending(&self) -> std::sync::MutexGuard<'_, HashMap<i64, Slot>> {
        self.inner
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn read_loop(inner: &Arc<Inner>, mut reader: impl BufRead, handler: &dyn ServerHandler) {
    loop {
        let Some(body) = read_frame(&mut reader, inner) else {
            return;
        };
        let Ok(message) = serde_json::from_slice::<Value>(&body) else {
            // Consumed exactly the framed bytes, so the stream is still
            // aligned: count it and read the next frame.
            inner.malformed.fetch_add(1, Ordering::SeqCst);
            continue;
        };
        dispatch(inner, &message, handler);
    }
}

/// Read one frame's body, or `None` at end of stream.
fn read_frame(reader: &mut impl BufRead, inner: &Arc<Inner>) -> Option<Vec<u8>> {
    let mut length: Option<usize> = None;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return None,
            Ok(_) => {}
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().ok();
        }
    }
    let Some(length) = length else {
        // A header block with no usable length: nothing can be read
        // from it, and guessing a body length would desynchronize the
        // stream for good.
        inner.malformed.fetch_add(1, Ordering::SeqCst);
        return Some(Vec::new());
    };
    let mut body = vec![0_u8; length];
    reader.read_exact(&mut body).ok()?;
    Some(body)
}

/// Route one decoded frame.
///
/// The shape of a JSON-RPC message is decided by which of `id` and
/// `method` are present, so those are what this branches on -- and `id`
/// is kept as the [`Value`] it is, never narrowed to an integer.
///
/// That distinction is load-bearing and cost #19 task 10 real debugging
/// to find. JSON-RPC 2.0 and LSP both allow an id to be a string, and
/// the TypeScript native server uses one: its `workspace/configuration`
/// request arrives as `"id":"ts1"`. An earlier version read the id as an
/// `i64`, so a string id read as *absent*, the request was dispatched as
/// a notification, and no reply was ever sent. The server then waited
/// forever for a configuration answer and stopped replying to anything
/// -- including `shutdown` -- with a healthy connection, a live child
/// and no error anywhere. Pyright happens to use integer ids, which is
/// the only reason this survived task 5.
fn dispatch(inner: &Arc<Inner>, message: &Value, handler: &dyn ServerHandler) {
    // A JSON-RPC `null` id is not an id. It appears on an error reply to
    // a request the peer could not parse, which matches nothing pending.
    let id = message.get("id").filter(|id| !id.is_null());
    let method = message.get("method").and_then(Value::as_str);

    match (id, method) {
        // A request from the peer. Always answered, and answered with
        // the peer's own id echoed back verbatim -- a reply carrying a
        // re-encoded id matches nothing and is the same deadlock.
        (Some(id), Some(method)) => {
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            let reply = match handler.request(method, &params) {
                Some(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                None => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": format!("method not found: {method}") },
                }),
            };
            let _ = inner.send(&reply);
        }
        // A response to one of our requests. Our own ids are integers,
        // because this client mints them; a response carrying anything
        // else answers nothing this client sent.
        (Some(id), None) => {
            let Some(id) = id.as_i64() else {
                inner.malformed.fetch_add(1, Ordering::SeqCst);
                return;
            };
            let answer = if let Some(error) = message.get("error") {
                Err(RemoteError {
                    code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                    message: error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                })
            } else {
                Ok(message.get("result").cloned().unwrap_or(Value::Null))
            };
            let mut pending = inner
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // An unknown id is a late answer to something already
            // cancelled. Dropping it is the whole point.
            if let Some(slot) = pending.get_mut(&id) {
                *slot = Some(answer);
            }
            drop(pending);
            inner.answered.notify_all();
        }
        (None, Some(method)) => {
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            handler.notification(method, &params);
        }
        (None, None) => {
            inner.malformed.fetch_add(1, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::{
        io,
        sync::mpsc::{Receiver, Sender, channel},
    };

    /// An in-process byte stream, so the client can be driven without a
    /// child process.
    pub struct Pipe {
        rx: Receiver<Vec<u8>>,
        buffer: Vec<u8>,
        at: usize,
    }

    impl Pipe {
        pub fn new() -> (Sender<Vec<u8>>, Self) {
            let (tx, rx) = channel();
            (
                tx,
                Self {
                    rx,
                    buffer: Vec::new(),
                    at: 0,
                },
            )
        }
    }

    impl Read for Pipe {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            while self.at == self.buffer.len() {
                match self.rx.recv() {
                    Ok(next) => {
                        self.buffer = next;
                        self.at = 0;
                    }
                    // Every sender is gone: end of stream.
                    Err(_) => return Ok(0),
                }
            }
            let taken = (self.buffer.len() - self.at).min(out.len());
            out[..taken].copy_from_slice(&self.buffer[self.at..self.at + taken]);
            self.at += taken;
            Ok(taken)
        }
    }

    /// A writer that hands every frame body to a channel.
    pub struct Sink {
        tx: Sender<Vec<u8>>,
        partial: Vec<u8>,
    }

    impl Sink {
        pub fn new() -> (Self, Receiver<Vec<u8>>) {
            let (tx, rx) = channel();
            (
                Self {
                    tx,
                    partial: Vec::new(),
                },
                rx,
            )
        }
    }

    impl Write for Sink {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.partial.extend_from_slice(data);
            while let Some(at) = find(&self.partial, b"\r\n\r\n") {
                let header = String::from_utf8_lossy(&self.partial[..at]).into_owned();
                let Some(length) = header
                    .lines()
                    .find_map(|line| line.strip_prefix("Content-Length: "))
                    .and_then(|raw| raw.trim().parse::<usize>().ok())
                else {
                    self.partial.drain(..at + 4);
                    continue;
                };
                if self.partial.len() < at + 4 + length {
                    break;
                }
                let body = self.partial[at + 4..at + 4 + length].to_vec();
                self.partial.drain(..at + 4 + length);
                let _ = self.tx.send(body);
            }
            Ok(data.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    /// Frame a JSON message the way a server would.
    pub fn frame(message: &Value) -> Vec<u8> {
        let body = serde_json::to_vec(message).expect("serializable");
        let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        out.extend_from_slice(&body);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{testing::*, *};
    use std::sync::mpsc::Receiver;

    struct Recording {
        requests: Mutex<Vec<(String, Value)>>,
        notifications: Mutex<Vec<String>>,
        answer: Option<Value>,
    }

    impl Recording {
        fn new(answer: Option<Value>) -> Arc<Self> {
            Arc::new(Self {
                requests: Mutex::new(Vec::new()),
                notifications: Mutex::new(Vec::new()),
                answer,
            })
        }
    }

    impl ServerHandler for Recording {
        fn request(&self, method: &str, params: &Value) -> Option<Value> {
            self.requests
                .lock()
                .expect("lock")
                .push((method.to_owned(), params.clone()));
            self.answer.clone()
        }
        fn notification(&self, method: &str, _params: &Value) {
            self.notifications
                .lock()
                .expect("lock")
                .push(method.to_owned());
        }
    }

    /// A client wired to a scripted peer.
    fn wired(
        handler: Arc<dyn ServerHandler>,
    ) -> (Client, std::sync::mpsc::Sender<Vec<u8>>, Receiver<Vec<u8>>) {
        let (to_client, pipe) = Pipe::new();
        let (sink, from_client) = Sink::new();
        (
            Client::new(Box::new(pipe), Box::new(sink), handler),
            to_client,
            from_client,
        )
    }

    fn body(raw: &[u8]) -> Value {
        serde_json::from_slice(raw).expect("json")
    }

    #[test]
    fn a_request_is_framed_and_its_response_correlated() {
        let (client, to_client, from_client) = wired(Arc::new(IgnoreServer));
        let worker = thread::spawn(move || {
            let sent = body(&from_client.recv().expect("sent"));
            assert_eq!(sent["method"], "typeServer/getSnapshot");
            assert_eq!(sent["jsonrpc"], "2.0");
            let id = sent["id"].clone();
            to_client
                .send(frame(&json!({"jsonrpc": "2.0", "id": id, "result": 7})))
                .expect("send");
        });
        let answer = client
            .request("typeServer/getSnapshot", None, &CancelToken::new())
            .expect("answer");
        assert_eq!(answer, json!(7));
        worker.join().expect("worker");
    }

    #[test]
    fn interleaved_responses_reach_the_right_caller() {
        let (client, to_client, from_client) = wired(Arc::new(IgnoreServer));
        let client = Arc::new(client);
        let worker = thread::spawn(move || {
            let first = body(&from_client.recv().expect("first"));
            let second = body(&from_client.recv().expect("second"));
            // Answered out of order on purpose.
            to_client
                .send(frame(
                    &json!({"jsonrpc": "2.0", "id": second["id"], "result": "second"}),
                ))
                .expect("send");
            to_client
                .send(frame(
                    &json!({"jsonrpc": "2.0", "id": first["id"], "result": "first"}),
                ))
                .expect("send");
        });
        let a = Arc::clone(&client);
        let first = thread::spawn(move || a.request("a", None, &CancelToken::new()));
        // Ordering the two sends is what makes "out of order" meaningful.
        thread::sleep(Duration::from_millis(50));
        let second = client.request("b", None, &CancelToken::new()).expect("b");
        assert_eq!(second, json!("second"));
        assert_eq!(first.join().expect("join").expect("a"), json!("first"));
        worker.join().expect("worker");
    }

    #[test]
    fn a_peer_error_comes_back_with_its_code() {
        let (client, to_client, from_client) = wired(Arc::new(IgnoreServer));
        // The peer stays reachable, so the assertion below is about the
        // error and not about the scripted server having gone away.
        let peer = to_client.clone();
        thread::spawn(move || {
            let sent = body(&from_client.recv().expect("sent"));
            peer.send(frame(&json!({
                "jsonrpc": "2.0", "id": sent["id"],
                "error": {"code": -32802, "message": "server cancelled"},
            })))
            .expect("send");
        });
        let failure = client
            .request("typeServer/resolveImport", None, &CancelToken::new())
            .expect_err("error");
        assert_eq!(failure.code(), Some(-32802));
        assert!(
            client.is_open(),
            "a peer error does not break the transport"
        );
    }

    #[test]
    fn a_peer_request_is_always_answered() {
        let handler = Recording::new(Some(json!([{"pythonPath": "/usr/bin/python3"}])));
        let (client, to_client, from_client) = wired(handler.clone());
        to_client
            .send(frame(&json!({
                "jsonrpc": "2.0", "id": 100, "method": "workspace/configuration",
                "params": {"items": [{"section": "python"}]},
            })))
            .expect("send");
        let reply = body(&from_client.recv().expect("reply"));
        assert_eq!(reply["id"], 100);
        assert_eq!(reply["result"][0]["pythonPath"], "/usr/bin/python3");
        assert_eq!(handler.requests.lock().expect("lock").len(), 1);
        drop(client);
    }

    #[test]
    fn a_peer_request_with_a_string_id_is_answered_with_that_same_id() {
        // The regression that cost #19 task 10 a debugging session. The
        // TypeScript native server asks for configuration with
        // `"id":"ts1"`. Read as an integer, the id is *absent*, the
        // request looks like a notification, and no reply is sent -- so
        // the server waits forever for its answer and silently stops
        // replying to everything, `shutdown` included.
        let handler = Recording::new(Some(json!([{}])));
        let (client, to_client, from_client) = wired(handler.clone());
        to_client
            .send(frame(&json!({
                "jsonrpc": "2.0", "id": "ts1", "method": "workspace/configuration",
                "params": {"items": [{"section": "typescript"}]},
            })))
            .expect("send");
        let reply = body(
            &from_client
                .recv()
                .expect("a string id must still be answered"),
        );
        assert_eq!(reply["id"], "ts1", "the peer's own id, echoed verbatim");
        assert!(reply.get("result").is_some());
        assert_eq!(handler.requests.lock().expect("lock").len(), 1);
        drop(client);
    }

    #[test]
    fn a_null_id_is_not_an_id() {
        // A `null` id appears on an error reply to a request the peer
        // could not parse. It answers nothing pending, and treating it
        // as an id 0 would fill a slot that a real request owns.
        let handler = Recording::new(None);
        let (client, to_client, _from_client) = wired(handler.clone());
        to_client
            .send(frame(&json!({
                "jsonrpc": "2.0", "id": Value::Null,
                "error": { "code": -32700, "message": "Parse error" },
            })))
            .expect("send");
        for _ in 0..100 {
            if client.malformed_frames() > 0 {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(client.malformed_frames(), 1);
        drop(client);
    }

    #[test]
    fn an_unhandled_peer_request_gets_method_not_found_rather_than_silence() {
        let (client, to_client, from_client) = wired(Arc::new(IgnoreServer));
        to_client
            .send(frame(
                &json!({"jsonrpc": "2.0", "id": 5, "method": "window/showMessageRequest"}),
            ))
            .expect("send");
        let reply = body(&from_client.recv().expect("reply"));
        assert_eq!(reply["id"], 5);
        assert_eq!(reply["error"]["code"], -32601);
        drop(client);
    }

    #[test]
    fn notifications_reach_the_handler() {
        let handler = Recording::new(None);
        let (client, to_client, _from_client) = wired(handler.clone());
        to_client
            .send(frame(&json!({
                "jsonrpc": "2.0", "method": "typeServer/snapshotChanged",
                "params": {"old": 3, "new": 4},
            })))
            .expect("send");
        for _ in 0..100 {
            if !handler.notifications.lock().expect("lock").is_empty() {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            handler.notifications.lock().expect("lock").as_slice(),
            ["typeServer/snapshotChanged"]
        );
        drop(client);
    }

    #[test]
    fn a_malformed_frame_is_counted_and_the_next_one_still_works() {
        let (client, to_client, from_client) = wired(Arc::new(IgnoreServer));
        // Well-framed bytes that are not JSON, so the stream stays
        // aligned and the frame after it must still be understood.
        let junk = b"Content-Length: 10\r\n\r\n{not json}";
        to_client.send(junk.to_vec()).expect("send");
        let peer = to_client.clone();
        let worker = thread::spawn(move || {
            let sent = body(&from_client.recv().expect("sent"));
            peer.send(frame(
                &json!({"jsonrpc": "2.0", "id": sent["id"], "result": 11}),
            ))
            .expect("send");
        });
        let answer = client
            .request("typeServer/getSnapshot", None, &CancelToken::new())
            .expect("answer");
        assert_eq!(answer, json!(11));
        assert_eq!(client.malformed_frames(), 1);
        assert!(client.is_open());
        worker.join().expect("worker");
    }

    #[test]
    fn end_of_stream_closes_the_client_and_fails_waiters() {
        let (to_client, pipe) = Pipe::new();
        let (sink, _from_client) = Sink::new();
        let client = Client::new(Box::new(pipe), Box::new(sink), Arc::new(IgnoreServer));
        drop(to_client);
        for _ in 0..200 {
            if !client.is_open() {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(!client.is_open(), "the reader saw end of stream");
        assert_eq!(
            client.request("anything", None, &CancelToken::new()),
            Err(RpcFailure::Closed)
        );
    }

    #[test]
    fn cancelling_returns_promptly_and_leaves_the_connection_usable() {
        let (client, to_client, from_client) = wired(Arc::new(IgnoreServer));
        let cancel = CancelToken::new();
        let token = cancel.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            token.cancel();
        });
        let failure = client
            .request(
                "textDocument/references",
                None,
                &CancelToken::clone(&cancel),
            )
            .expect_err("cancelled");
        assert_eq!(failure, RpcFailure::Cancelled);

        // The original request, then the best-effort cancellation.
        let first = body(&from_client.recv().expect("request"));
        let cancelled = body(&from_client.recv().expect("cancel"));
        assert_eq!(cancelled["method"], "$/cancelRequest");
        assert_eq!(cancelled["params"]["id"], first["id"]);

        // A late answer to the abandoned request is dropped, and the
        // next request still works.
        to_client
            .send(frame(
                &json!({"jsonrpc": "2.0", "id": first["id"], "result": "too late"}),
            ))
            .expect("send");
        let worker = thread::spawn(move || {
            let next = body(&from_client.recv().expect("next"));
            to_client
                .send(frame(
                    &json!({"jsonrpc": "2.0", "id": next["id"], "result": "fresh"}),
                ))
                .expect("send");
        });
        assert_eq!(
            client
                .request("typeServer/getSnapshot", None, &CancelToken::new())
                .expect("fresh"),
            json!("fresh")
        );
        worker.join().expect("worker");
    }

    #[test]
    fn every_method_put_on_the_wire_is_recorded() {
        let (client, _to_client, _from_client) = wired(Arc::new(IgnoreServer));
        client
            .notify("workspace/didChangeWatchedFiles", Some(json!({})))
            .expect("notify");
        client.close();
        assert_eq!(
            client.sent_methods(),
            ["workspace/didChangeWatchedFiles".to_owned()]
        );
    }
}
