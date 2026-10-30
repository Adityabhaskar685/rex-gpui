//! Keeps a local copy of one terminal block, off the UI thread.
//!
//! A task on the tokio runtime reads the block's data stream into a libghostty-vt terminal
//! that the pane locks to draw, and tells the pane when to redraw. The stream starts with
//! a Ghostty snapshot of the server's terminal, which libghostty-vt restores exactly.
//! Parsing a flood of output can take a whole core, so it can't run on the UI thread; and the
//! stream is read only as fast as it's parsed, so a backlog waits in the server, not here.
//! When the stream drops, the task reconnects and redraws from the server's copy.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use serde_json::{Value, json};

use crate::ghostty::Terminal;
use crate::rex::control::Control;
use crate::rex::data::{self, DataMessage, StreamItem, StreamSplitter};

/// The local copy of a terminal, shared between the mirror task and the pane.
pub struct Shared {
    pub term: Terminal,
    /// Goes up whenever `term` is replaced by a new one, which loses settings such as the
    /// default colors, so the pane knows to apply them again.
    pub generation: u64,
}

pub type SharedTerm = Arc<Mutex<Shared>>;

/// What the pane needs to hear about. Output itself goes straight into the shared grid.
pub enum MirrorEvent {
    /// The grid changed. Sent at most once until the pane takes it.
    Redraw,
    /// `None` once the copy is live; otherwise what's happening, such as "Connecting…".
    Status(Option<String>),
    /// The terminal's colors from the server.
    Theme(Value),
    Exited(i64),
}

/// The running copy. Dropping it stops the task and closes the stream.
pub struct Mirror {
    pub term: SharedTerm,
    pub events: async_channel::Receiver<MirrorEvent>,
    /// Cleared by the pane once it has redrawn, so the next change sends `Redraw` again.
    pub dirty: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Mirror {
    fn drop(&mut self) {
        self.task.abort();
    }
}

const FORMAT_CHUNK: u64 = 900_000;

pub fn start(control: Control, session_id: String, block_id: String) -> Mirror {
    let term = Terminal::new(80, 24).expect("creating a terminal");
    let term: SharedTerm = Arc::new(Mutex::new(Shared { term, generation: 0 }));
    let (tx, events) = async_channel::unbounded();
    let dirty = Arc::new(AtomicBool::new(false));
    let task = crate::runtime().spawn(run(control, session_id, block_id, term.clone(), tx, dirty.clone()));
    Mirror { term, events, dirty, task }
}

struct Pipe {
    control: Control,
    session_id: String,
    block_id: String,
    term: SharedTerm,
    tx: async_channel::Sender<MirrorEvent>,
    dirty: Arc<AtomicBool>,
}

impl Pipe {
    /// Send an event; false once the pane is gone.
    fn send(&self, event: MirrorEvent) -> bool {
        self.tx.try_send(event).is_ok()
    }

    fn changed(&self) -> bool {
        if self.dirty.swap(true, Ordering::AcqRel) {
            return !self.tx.is_closed();
        }
        self.send(MirrorEvent::Redraw)
    }
}

async fn run(
    control: Control,
    session_id: String,
    block_id: String,
    term: SharedTerm,
    tx: async_channel::Sender<MirrorEvent>,
    dirty: Arc<AtomicBool>,
) {
    let pipe = Pipe { control, session_id, block_id, term, tx, dirty };
    let mut delay = Duration::from_millis(500);
    loop {
        match stream_once(&pipe).await {
            // The block closed, or the pane went away.
            Ok(Ended::Done) => return,
            Ok(Ended::Lost(reason)) | Err(reason) => {
                if !pipe.send(MirrorEvent::Status(Some(format!("Reconnecting… ({reason:#})")))) {
                    return;
                }
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(10));
            }
        }
    }
}

enum Ended {
    Done,
    Lost(anyhow::Error),
}

/// Follow one data stream from its handshake until it ends.
async fn stream_once(pipe: &Pipe) -> Result<Ended, anyhow::Error> {
    if !pipe.send(MirrorEvent::Status(Some("Connecting…".into()))) {
        return Ok(Ended::Done);
    }
    let (rx, _guard) = data::open_terminal_stream(&pipe.control, &pipe.session_id, &pipe.block_id).await?;
    let mut splitter = None;
    let mut live = false;
    let mut announced = false;
    let mut items = Vec::new();

    while let Ok(msg) = rx.recv().await {
        match msg {
            DataMessage::Welcome { nonce } => splitter = Some(StreamSplitter::new(nonce)),
            DataMessage::Closed(None) => {
                pipe.send(MirrorEvent::Status(Some("Closed".into())));
                return Ok(Ended::Done);
            }
            DataMessage::Closed(Some(reason)) => return Ok(Ended::Lost(anyhow::anyhow!(reason))),
            DataMessage::Bytes(bytes) => {
                let Some(splitter) = splitter.as_mut() else { continue };
                items.clear();
                splitter.feed(&bytes, &mut items);
                let mut redraw_from_vt = false;
                {
                    let mut shared = pipe.term.lock().unwrap();
                    for item in items.drain(..) {
                        match item {
                            StreamItem::Snapshot(snapshot) => match Terminal::from_snapshot(&snapshot) {
                                Ok(term) => {
                                    shared.term = term;
                                    shared.generation += 1;
                                    live = true;
                                }
                                Err(err) => {
                                    // A snapshot format this library doesn't know, say after a
                                    // Rex update: read the screen as VT text instead.
                                    log::warn!("restoring {} from its snapshot: {err:#}", pipe.block_id);
                                    redraw_from_vt = true;
                                }
                            },
                            StreamItem::Output(bytes) if live => shared.term.write(&bytes),
                            // Before a redraw, output is already part of the screen it reads.
                            StreamItem::Output(_) => {}
                            StreamItem::Rex(msg) => match msg["type"].as_str() {
                                Some("resize") => {
                                    let cols = msg["cols"].as_u64().unwrap_or(80) as u16;
                                    let rows = msg["rows"].as_u64().unwrap_or(24) as u16;
                                    let width = msg["cell_width_px"].as_u64().unwrap_or(0) as u32;
                                    let height = msg["cell_height_px"].as_u64().unwrap_or(0) as u32;
                                    shared.term.resize(cols, rows, width, height);
                                }
                                Some("theme") => {
                                    pipe.send(MirrorEvent::Theme(msg));
                                }
                                Some("child_exited") => {
                                    pipe.send(MirrorEvent::Exited(msg["exit_code"].as_i64().unwrap_or(-1)));
                                }
                                _ => {}
                            },
                        }
                    }
                }
                if redraw_from_vt {
                    // Output that arrives meanwhile waits in the stream and is applied after.
                    match fetch_vt(&pipe.control, &pipe.session_id, &pipe.block_id).await {
                        Ok(content) => {
                            let mut shared = pipe.term.lock().unwrap();
                            let mut term = Terminal::new(shared.term.cols(), shared.term.rows())?;
                            term.write(content.as_bytes());
                            shared.term = term;
                            shared.generation += 1;
                        }
                        Err(err) => log::warn!("reading the screen of {}: {err:#}", pipe.block_id),
                    }
                    live = true;
                }
                // Live from the snapshot or redraw on; tell the pane once.
                if live && !announced {
                    announced = true;
                    if !pipe.send(MirrorEvent::Status(None)) {
                        return Ok(Ended::Done);
                    }
                }
                if !pipe.changed() {
                    return Ok(Ended::Done);
                }
            }
        }
    }
    Ok(Ended::Lost(anyhow::anyhow!("stream ended")))
}

/// The screen and scrollback as VT text.
pub async fn fetch_vt(control: &Control, session_id: &str, block_id: &str) -> Result<String> {
    let mut args = json!({"format": "vt", "max_bytes": FORMAT_CHUNK});
    for extra in [
        "charsets", "cursor", "hyperlink", "keyboard", "kitty_keyboard", "modes", "palette",
        "protection", "pwd", "scrolling_region", "style", "tabstops",
    ] {
        args[format!("extra_{extra}")] = json!(true);
    }
    let reply = control
        .block_call("com.superlogical.terminal.format", session_id, block_id, args)
        .await?;
    let mut content = reply["content"].as_str().unwrap_or_default().to_string();
    if let Some(id) = reply["id"].as_u64() {
        loop {
            let part = control
                .block_call(
                    "com.superlogical.terminal.blob_read",
                    session_id,
                    block_id,
                    json!({"id": id, "offset": content.len(), "max_bytes": FORMAT_CHUNK}),
                )
                .await?;
            let data = part["data"].as_str().unwrap_or_default();
            content.push_str(data);
            if part["eof"].as_bool().unwrap_or(true) || data.is_empty() {
                break;
            }
        }
    }
    Ok(content)
}
