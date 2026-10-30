//! The Rex control connection: JSON requests, responses, and events over one WebSocket.
//!
//! Socket I/O runs on the shared tokio runtime. `Control::call` and the event receiver only
//! use runtime-agnostic channels, so GPUI's executor can await them directly.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};

use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

use super::endpoint::{CONNECT_TIMEOUT, Endpoint, Unreachable};

pub const API_REVISION: u64 = 7;

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value>>>>>;

/// A server event, unwrapped from its `{"type": "event", "payload": ...}` envelope.
#[derive(Debug, Clone)]
pub struct Event {
    pub kind: String,
    pub payload: Value,
}

#[derive(Clone)]
pub struct Control {
    /// Text messages to send. `None` closes the connection.
    outgoing: mpsc::UnboundedSender<Option<String>>,
    pending: Pending,
    next_id: Arc<AtomicU64>,
    pub client_id: String,
    pub endpoint: Endpoint,
}

impl Control {
    /// Connect to the first of `endpoints` that answers and complete the hello/welcome
    /// exchange. With `autostart`, a missing local server is started first, as the CLI does.
    pub async fn connect(
        endpoints: &[Endpoint],
        autostart: bool,
    ) -> Result<(Self, async_channel::Receiver<Event>)> {
        let mut last_error = anyhow!("no address to connect to");
        // HTTPS and HTTP on the same port are one TCP target: if it didn't answer for one,
        // don't wait for it again for the other.
        let mut unreachable = std::collections::HashSet::new();
        for endpoint in endpoints {
            if let Endpoint::Url { host, port, .. } = endpoint
                && unreachable.contains(&Unreachable {
                    host: host.clone(),
                    port: *port,
                })
            {
                continue;
            }
            if let (true, Endpoint::Unix(socket)) = (autostart, endpoint)
                && !socket.exists()
            {
                start_server().await?;
            }
            match endpoint.connect("/api/control", "rex.control.v1").await {
                Ok(ws) => return Self::handshake(ws, endpoint.clone()).await,
                Err(err) => {
                    if let Some(target) = err.chain().find_map(|e| e.downcast_ref::<Unreachable>())
                    {
                        unreachable.insert(target.clone());
                    }
                    last_error = err.context(format!("connecting to {endpoint}"));
                }
            }
        }
        Err(last_error)
    }

    async fn handshake(
        mut ws: super::endpoint::Ws,
        endpoint: Endpoint,
    ) -> Result<(Self, async_channel::Receiver<Event>)> {
        let hello = json!({
            "type": "hello",
            "api_revision": API_REVISION,
            "api_min_revision": API_REVISION,
            "client": {
                "kind": "app",
                "platform": "linux",
                "name": "rex-gpui",
                "version": env!("CARGO_PKG_VERSION"),
            },
        });
        let exchange = async {
            ws.send(Message::text(hello.to_string())).await?;
            loop {
                match ws
                    .next()
                    .await
                    .ok_or_else(|| anyhow!("server closed before welcome"))??
                {
                    Message::Text(text) => break anyhow::Ok(serde_json::from_str::<Value>(&text)?),
                    Message::Close(frame) => bail!("server refused the connection: {frame:?}"),
                    _ => continue,
                }
            }
        };
        let welcome = tokio::time::timeout(CONNECT_TIMEOUT, exchange)
            .await
            .context("the server didn't send a welcome")??;
        if welcome["type"] != "welcome" || welcome["protocol"] != "rex.control.v1" {
            bail!("unexpected welcome: {welcome}");
        }
        let client_id = welcome["client"]["id"]
            .as_str()
            .unwrap_or_default()
            .to_string();

        let (outgoing, mut outgoing_rx) = mpsc::unbounded_channel::<Option<String>>();
        let (events_tx, events_rx) = async_channel::unbounded();
        let pending: Pending = Default::default();
        let (mut sink, mut stream) = ws.split();

        tokio::spawn(async move {
            while let Some(Some(text)) = outgoing_rx.recv().await {
                if sink.send(Message::text(text)).await.is_err() {
                    return;
                }
            }
            let _ = sink.send(Message::Close(None)).await;
        });

        let reader_pending = pending.clone();
        tokio::spawn(async move {
            while let Some(Ok(msg)) = stream.next().await {
                let Message::Text(text) = msg else { continue };
                let Ok(msg) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                match msg["type"].as_str() {
                    Some("response") => {
                        let id = msg["id"].as_str().unwrap_or_default();
                        let reply = reader_pending.lock().unwrap().remove(id);
                        if let Some(reply) = reply {
                            let result = match msg.get("error") {
                                Some(err) if !err.is_null() => Err(anyhow!(
                                    "{}: {}",
                                    err["code"].as_str().unwrap_or("error"),
                                    err["message"].as_str().unwrap_or_default()
                                )),
                                _ => Ok(msg.get("payload").cloned().unwrap_or(Value::Null)),
                            };
                            let _ = reply.send(result);
                        }
                    }
                    Some("event") => {
                        let inner = &msg["payload"];
                        let event = Event {
                            kind: inner["type"].as_str().unwrap_or_default().to_string(),
                            payload: inner["payload"].clone(),
                        };
                        // A connection kept only for calls, such as the local one that holds
                        // the host list, has no one reading its events. Drop them.
                        let _ = events_tx.send(event).await;
                    }
                    _ => {}
                }
            }
            // Fail every call still waiting, so nothing hangs on a dead connection.
            for (_, reply) in reader_pending.lock().unwrap().drain() {
                let _ = reply.send(Err(anyhow!("control connection closed")));
            }
            events_tx.close();
        });

        let control = Self {
            outgoing,
            pending,
            next_id: Arc::new(AtomicU64::new(1)),
            client_id,
            endpoint,
        };
        Ok((control, events_rx))
    }

    /// Queue a request now and return a future for its response. Requests go out in the
    /// order this is called, which matters for ordered methods such as `client.key`. A
    /// request that gets no response within `REQUEST_TIMEOUT` fails.
    pub fn request(
        &self,
        method: &str,
        payload: Value,
    ) -> impl Future<Output = Result<Value>> + 'static {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id.clone(), tx);
        let request = json!({"type": "request", "id": id, "method": method, "payload": payload});
        let sent = self.outgoing.send(Some(request.to_string()));
        let (method, pending) = (method.to_string(), self.pending.clone());
        // The deadline needs tokio's timer, but callers also await this from GPUI's executor,
        // so wait on the runtime and hand the caller the task, which any executor can await.
        let waiting = crate::runtime().spawn(async move {
            // Whatever happens, the request's slot goes once this returns. Normally the
            // response took it already.
            let _cleanup = Defer(|| {
                pending.lock().unwrap().remove(&id);
            });
            sent.map_err(|_| anyhow!("control connection closed"))?;
            tokio::time::timeout(REQUEST_TIMEOUT, rx)
                .await
                .map_err(|_| {
                    anyhow!(
                        "{method}: no response within {}s",
                        REQUEST_TIMEOUT.as_secs()
                    )
                })?
                .map_err(|_| anyhow!("control connection closed"))?
                .with_context(|| format!("{method} failed"))
        });
        async move {
            waiting
                .await
                .map_err(|err| anyhow!("request task failed: {err}"))?
        }
    }

    pub async fn call(&self, method: &str, payload: Value) -> Result<Value> {
        self.request(method, payload).await
    }

    /// Call a block method such as `com.superlogical.terminal.write`.
    pub async fn block_call(
        &self,
        method: &str,
        session_id: &str,
        block_id: &str,
        args: Value,
    ) -> Result<Value> {
        self.call(
            method,
            json!({"session_id": session_id, "block_id": block_id, "args": args}),
        )
        .await
    }

    /// Leave the server with a close frame, which ends this client at once instead of
    /// keeping it for a reconnect, so its blocks pass to other clients.
    pub fn close(&self) {
        let _ = self.outgoing.send(None);
    }

    /// Fire a request and log a failure instead of returning it.
    pub fn send(&self, method: &'static str, payload: Value) {
        let response = self.request(method, payload);
        crate::runtime().spawn(async move {
            if let Err(err) = response.await {
                log::warn!("{err:#}");
            }
        });
    }
}

/// Start a detached server with `rex server start`, the way the CLI's `--autostart` does.
async fn start_server() -> Result<()> {
    let status = tokio::process::Command::new("rex")
        .args(["server", "start"])
        .kill_on_drop(true)
        .status();
    let status = tokio::time::timeout(Duration::from_secs(15), status)
        .await
        .context("`rex server start` didn't finish within 15s")?
        .context("running `rex server start`")?;
    if !status.success() {
        bail!("`rex server start` exited with {status}");
    }
    Ok(())
}

/// How long a request may wait for its response. Long enough for the slowest call, reading
/// a large screen with `terminal.format`.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Runs a closure when dropped.
struct Defer<F: FnMut()>(F);

impl<F: FnMut()> Drop for Defer<F> {
    fn drop(&mut self) {
        (self.0)()
    }
}
