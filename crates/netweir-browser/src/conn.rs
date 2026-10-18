//! The protocol client: one connection to Chrome, carrying every session.
//!
//! Over the pipe a launched Chrome reads, each message is a JSON object
//! followed by a NUL byte; over a WebSocket, to a browser already running,
//! each is a text frame. Either way, a reader routes replies to the command
//! waiting for their `id` and events to the handler of the session they
//! name, and a writer does the writing, so no async task waits on the
//! connection.

use std::collections::{BTreeSet, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

use crate::{Error, Result};

/// Methods whose side effects page scripts can see. Never sent.
const FORBIDDEN: &[&str] = &["Runtime.enable", "Console.enable", "Log.enable"];

/// Called on the reader thread for each event of a session.
pub(crate) type Handler = Arc<dyn Fn(&str, &Value) + Send + Sync>;

/// Sent to a session's handler, after which it gets nothing more.
pub(crate) const GONE: &str = "netweir.gone";

#[derive(Clone)]
pub(crate) struct Connection {
    shared: Arc<Shared>,
}

struct Shared {
    /// Each message ends in a NUL byte, which the pipe needs and a
    /// WebSocket frame drops.
    out: Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>,
    next_id: AtomicU64,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    closed: bool,
    /// Commands waiting for their answers, by id: the method, and the
    /// session it went to.
    pending: HashMap<u64, (String, String, oneshot::Sender<Result<Value>>)>,
    /// By session id; "" is the browser's own.
    handlers: HashMap<String, Handler>,
    /// Every method sent, for the test that nothing forbidden was.
    sent: BTreeSet<String>,
}

impl Connection {
    /// Over a pipe, to a launched `chrome`, if given: see [`closes_with`].
    pub(crate) fn new(
        input: impl Read + Send + 'static,
        output: impl Write + Send + 'static,
        chrome: Option<&std::process::Child>,
    ) -> Connection {
        let (shared, mut rx) = Shared::new();
        std::thread::Builder::new()
            .name("netweir-cdp-write".into())
            .spawn(move || {
                let mut output = output;
                while let Some(message) = rx.blocking_recv() {
                    if output.write_all(&message).is_err() {
                        break;
                    }
                }
            })
            .expect("can't start the CDP writer thread");
        let reader = shared.clone();
        let reader = std::thread::Builder::new()
            .name("netweir-cdp-read".into())
            .spawn(move || read_loop(BufReader::new(input), &reader))
            .expect("can't start the CDP reader thread");
        if let Some(chrome) = chrome {
            closes_with(chrome, &reader, &shared);
        }
        Connection { shared }
    }

    /// Over a WebSocket, to a browser already running. Needs a tokio
    /// runtime.
    pub(crate) fn over_websocket<S>(socket: tokio_tungstenite::WebSocketStream<S>) -> Connection
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (shared, mut rx) = Shared::new();
        let (mut sink, mut stream) = socket.split();
        tokio::spawn(async move {
            while let Some(mut message) = rx.recv().await {
                message.pop();
                let text = String::from_utf8(message).expect("serde_json writes UTF-8");
                if sink.send(Message::text(text)).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });
        let reader = shared.clone();
        tokio::spawn(async move {
            while let Some(Ok(message)) = stream.next().await {
                let value = match &message {
                    Message::Text(text) => serde_json::from_str::<Value>(text).ok(),
                    Message::Binary(bytes) => serde_json::from_slice::<Value>(bytes).ok(),
                    Message::Close(_) => break,
                    _ => None,
                };
                if let Some(value) = value {
                    reader.deliver(value);
                }
            }
            reader.close();
        });
        Connection { shared }
    }

    /// Sends `method` to `session` ("" for the browser) and waits for the
    /// result.
    pub(crate) async fn call(&self, session: &str, method: &str, params: Value) -> Result<Value> {
        if FORBIDDEN.contains(&method) {
            return Err(Error::Invalid(format!(
                "{method} is never sent: pages can detect it"
            )));
        }
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let mut message = json!({"id": id, "method": method, "params": params});
        if !session.is_empty() {
            message["sessionId"] = Value::from(session);
        }
        let mut bytes = serde_json::to_vec(&message).expect("a JSON value serialises");
        bytes.push(0);
        let (tx, rx) = oneshot::channel();
        {
            let mut state = self.shared.lock();
            if state.closed {
                return Err(Error::Closed);
            }
            state.sent.insert(method.to_string());
            state
                .pending
                .insert(id, (method.to_string(), session.to_string(), tx));
        }
        let sent = match &*self.shared.out.lock().unwrap_or_else(|e| e.into_inner()) {
            Some(out) => out.send(bytes).is_ok(),
            None => false,
        };
        if !sent {
            self.shared.lock().pending.remove(&id);
            return Err(Error::Closed);
        }
        rx.await.unwrap_or(Err(Error::Closed))
    }

    /// `call`, but an error naming `method` if Chrome hasn't answered within
    /// `limit`: a renderer kept busy by a page's script, or a Chrome that
    /// has stopped responding, may never answer at all.
    pub(crate) async fn call_within(
        &self,
        limit: std::time::Duration,
        session: &str,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        tokio::time::timeout(limit, self.call(session, method, params))
            .await
            .unwrap_or_else(|_| {
                Err(Error::Timeout(format!(
                    "Chrome didn't answer {method} within {}s",
                    limit.as_secs_f64()
                )))
            })
    }

    /// Sends without waiting for the reply, for the reader thread, which
    /// can't wait: answering a request Chrome is holding, say.
    pub(crate) fn send(&self, session: &str, method: &str, params: Value) {
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let mut message = json!({"id": id, "method": method, "params": params});
        if !session.is_empty() {
            message["sessionId"] = Value::from(session);
        }
        let mut bytes = serde_json::to_vec(&message).expect("a JSON value serialises");
        bytes.push(0);
        self.shared.lock().sent.insert(method.to_string());
        if let Some(out) = &*self.shared.out.lock().unwrap_or_else(|e| e.into_inner()) {
            let _ = out.send(bytes);
        }
    }

    pub(crate) fn on(&self, session: &str, handler: Handler) {
        let mut state = self.shared.lock();
        if state.closed {
            drop(state);
            handler(GONE, &Value::Null);
            return;
        }
        state.handlers.insert(session.to_string(), handler);
    }

    /// Stops events reaching the session's handler, which hears GONE.
    pub(crate) fn off(&self, session: &str) {
        let handler = self.shared.lock().handlers.remove(session);
        if let Some(handler) = handler {
            handler(GONE, &Value::Null);
        }
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.shared.lock().closed
    }

    pub(crate) fn methods_sent(&self) -> Vec<String> {
        self.shared.lock().sent.iter().cloned().collect()
    }

    /// Stops writing; the reader ends when Chrome closes its end.
    pub(crate) fn shut(&self) {
        self.shared
            .out
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
    }
}

impl Shared {
    fn new() -> (Arc<Shared>, mpsc::UnboundedReceiver<Vec<u8>>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            out: Mutex::new(Some(tx)),
            next_id: AtomicU64::new(1),
            state: Mutex::new(State::default()),
        });
        (shared, rx)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn deliver(&self, message: Value) {
        if let Some(id) = message.get("id").and_then(Value::as_u64) {
            let Some((method, _, tx)) = self.lock().pending.remove(&id) else {
                return;
            };
            let result = match message.get("error") {
                Some(e) => Err(Error::Protocol {
                    method,
                    message: e
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("failed")
                        .to_string(),
                }),
                None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
            };
            let _ = tx.send(result);
            return;
        }
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return;
        };
        let params = message.get("params").unwrap_or(&Value::Null);
        let session = message
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or("");
        // A detached target's session hears it from the browser's. Chrome
        // never answers what was sent to it, so that fails now.
        if method == "Target.detachedFromTarget"
            && let Some(gone) = params.get("sessionId").and_then(Value::as_str)
        {
            let stranded: Vec<_> = {
                let mut state = self.lock();
                let ids: Vec<u64> = state
                    .pending
                    .iter()
                    .filter(|(_, (_, s, _))| s == gone)
                    .map(|(id, _)| *id)
                    .collect();
                ids.iter()
                    .filter_map(|id| state.pending.remove(id))
                    .collect()
            };
            for (method, _, tx) in stranded {
                let _ = tx.send(Err(Error::Protocol {
                    method,
                    message: "the session went away before Chrome answered".into(),
                }));
            }
            let handler = self.lock().handlers.remove(gone);
            if let Some(handler) = handler {
                handler(GONE, &Value::Null);
            }
        }
        let handler = self.lock().handlers.get(session).cloned();
        if let Some(handler) = handler {
            handler(method, params);
        }
    }

    fn close(&self) {
        let (pending, handlers) = {
            let mut state = self.lock();
            state.closed = true;
            (
                std::mem::take(&mut state.pending),
                std::mem::take(&mut state.handlers),
            )
        };
        for (_, (_, _, tx)) in pending {
            let _ = tx.send(Err(Error::Closed));
        }
        for handler in handlers.into_values() {
            handler(GONE, &Value::Null);
        }
        self.out.lock().unwrap_or_else(|e| e.into_inner()).take();
    }
}

/// On Windows, any process the program starts while a Chrome launches
/// inherits Chrome's end of the pipe (see launch::start), and while one
/// holds it the reader never reads Chrome's exit. So once Chrome's process
/// has ended, the reader's blocked read is cancelled, which ends it as the
/// pipe's end would. A read only blocks once the pipe is empty, and a dead
/// Chrome writes nothing more, so nothing Chrome sent is lost. Elsewhere
/// Chrome's end is never inherited, and the pipe's end is enough.
#[cfg(windows)]
fn closes_with(
    chrome: &std::process::Child,
    reader: &std::thread::JoinHandle<()>,
    shared: &Arc<Shared>,
) {
    use std::os::windows::io::{AsHandle, AsRawHandle};
    use windows_sys::Win32::System::IO::CancelSynchronousIo;
    use windows_sys::Win32::System::Threading::{INFINITE, WaitForSingleObject};

    let (Ok(chrome), Ok(reader)) = (
        chrome.as_handle().try_clone_to_owned(),
        reader.as_handle().try_clone_to_owned(),
    ) else {
        return;
    };
    let shared = shared.clone();
    let _ = std::thread::Builder::new()
        .name("netweir-chrome-exit".into())
        .spawn(move || {
            // SAFETY: both handles are open, owned by this thread.
            unsafe { WaitForSingleObject(chrome.as_raw_handle() as _, INFINITE) };
            // Between reads the cancel misses, so it's tried until the
            // reader has ended.
            while !shared.lock().closed {
                unsafe { CancelSynchronousIo(reader.as_raw_handle() as _) };
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        });
}

#[cfg(not(windows))]
fn closes_with(_: &std::process::Child, _: &std::thread::JoinHandle<()>, _: &Arc<Shared>) {}

fn read_loop(mut input: impl BufRead, shared: &Shared) {
    let mut message = Vec::new();
    loop {
        message.clear();
        match input.read_until(0, &mut message) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if message.last() == Some(&0) {
            message.pop();
        }
        // Known limit: a message that isn't JSON is dropped; Chrome only sends
        // JSON, and one bad message shouldn't end the session.
        if let Ok(value) = serde_json::from_slice::<Value>(&message) {
            shared.deliver(value);
        }
    }
    shared.close();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn forbidden_methods_are_refused_without_being_sent() {
        let (input, _keep) = std::io::pipe().unwrap();
        let conn = Connection::new(input, std::io::sink(), None);
        let err = conn
            .call("", "Runtime.enable", json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Invalid(_)), "{err}");
        assert!(conn.methods_sent().is_empty());
    }

    #[tokio::test]
    async fn a_session_that_detaches_fails_what_was_waiting_on_it() {
        let (input, mut writer) = std::io::pipe().unwrap();
        let conn = Connection::new(input, std::io::sink(), None);
        let waiting = tokio::spawn({
            let conn = conn.clone();
            async move { conn.call("gone", "Page.getFrameTree", json!({})).await }
        });
        let other = tokio::spawn({
            let conn = conn.clone();
            async move { conn.call("kept", "Page.getFrameTree", json!({})).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        writer
            .write_all(
                b"{\"method\":\"Target.detachedFromTarget\",\"params\":{\"sessionId\":\"gone\"}}\0",
            )
            .unwrap();
        let r = tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
            .await
            .expect("still waiting on a session that's gone")
            .unwrap();
        assert!(matches!(r, Err(Error::Protocol { .. })), "{r:?}");
        assert!(!other.is_finished(), "another session's command waits on");
        drop(writer);
    }

    #[tokio::test]
    async fn a_closed_pipe_fails_what_was_waiting() {
        let (input, writer) = std::io::pipe().unwrap();
        let conn = Connection::new(input, std::io::sink(), None);
        let waiting = tokio::spawn({
            let conn = conn.clone();
            async move { conn.call("", "Browser.getVersion", json!({})).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(writer);
        assert_eq!(waiting.await.unwrap(), Err(Error::Closed));
        assert!(conn.is_closed());
        assert_eq!(
            conn.call("", "Browser.getVersion", json!({})).await,
            Err(Error::Closed)
        );
    }

    /// A Chrome that exits is read as gone though another process still
    /// holds its end of the pipe, and what it sent before is still read.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_chrome_that_exits_closes_the_pipe_someone_else_holds() {
        let (input, mut held) = std::io::pipe().unwrap();
        let mut chrome = std::process::Command::new("cmd")
            .args(["/c", "ping -n 3 127.0.0.1 >nul"])
            .spawn()
            .unwrap();
        let conn = Connection::new(input, std::io::sink(), Some(&chrome));
        let answered = tokio::spawn({
            let conn = conn.clone();
            async move { conn.call("", "Browser.getVersion", json!({})).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        held.write_all(b"{\"id\":1,\"result\":{}}\0").unwrap();
        assert_eq!(answered.await.unwrap(), Ok(json!({})));
        let waiting = tokio::spawn({
            let conn = conn.clone();
            async move { conn.call("", "Browser.getVersion", json!({})).await }
        });
        chrome.wait().unwrap();
        let r = tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
            .await
            .expect("Chrome's exit went unread");
        assert_eq!(r.unwrap(), Err(Error::Closed));
        assert!(conn.is_closed());
        drop(held);
    }
}
