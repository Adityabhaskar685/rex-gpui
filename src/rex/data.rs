//! Data connections: one WebSocket per block, carrying the block's output stream.

use anyhow::{Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Message;

use super::control::Control;
use super::endpoint::Endpoint;

const SUBPROTOCOL: &str = "rex.data.terminal.v1";

pub enum DataMessage {
    /// The channel nonce from the WELCOME, which marks Rex's own messages in the stream.
    Welcome { nonce: u64 },
    Bytes(Vec<u8>),
    /// The stream ended. `None` means it closed normally, such as when the block closed.
    Closed(Option<String>),
}

/// Ends the data stream when dropped, with a normal close, so the server lets go of it at
/// once. Without that, a new stream for the same block is refused until the old one times out.
pub struct StreamGuard {
    _cancel: oneshot::Sender<()>,
}

/// Open a terminal's data stream. Messages arrive on the returned channel until it closes,
/// or until the guard is dropped.
pub async fn open_terminal_stream(
    control: &Control,
    session_id: &str,
    block_id: &str,
) -> Result<(async_channel::Receiver<DataMessage>, StreamGuard)> {
    let reply = control
        .call(
            "data.connect",
            json!({
                "session_id": session_id,
                "block_id": block_id,
                "data_ref_id": "terminal",
                "subprotocol": SUBPROTOCOL,
            }),
        )
        .await?;
    let ticket = reply["ticket"]
        .as_str()
        .ok_or_else(|| anyhow!("data.connect returned no ticket"))?;
    let ticket = URL_SAFE_NO_PAD.decode(ticket)?;
    if ticket.len() != 16 {
        bail!("ticket is {} bytes, expected 16", ticket.len());
    }

    // Bounded, so the stream is read only as fast as its reader keeps up: a backlog stays
    // in the server rather than in this process's memory.
    let (tx, rx) = async_channel::bounded(16);
    let (cancel, cancelled) = oneshot::channel();
    let endpoint = control.endpoint.clone();
    crate::runtime().spawn(async move {
        let reason = match pump(endpoint, ticket, &tx, cancelled).await {
            Ok(()) => None,
            Err(err) => Some(format!("{err:#}")),
        };
        let _ = tx.send(DataMessage::Closed(reason)).await;
    });
    Ok((rx, StreamGuard { _cancel: cancel }))
}

async fn pump(
    endpoint: Endpoint,
    ticket: Vec<u8>,
    tx: &async_channel::Sender<DataMessage>,
    mut cancelled: oneshot::Receiver<()>,
) -> Result<()> {
    let mut ws = endpoint.connect("/api/data", SUBPROTOCOL).await?;
    let mut hello = vec![0x01];
    hello.extend_from_slice(&ticket);
    ws.send(Message::binary(hello)).await?;

    let welcome = loop {
        match ws.next().await.ok_or_else(|| anyhow!("closed during handshake"))?? {
            Message::Binary(bytes) => break bytes,
            Message::Close(frame) => bail!("closed during handshake: {frame:?}"),
            _ => continue,
        }
    };
    match welcome.first() {
        Some(0x81) if welcome.len() == 32 => {}
        Some(0x82) => bail!("data handshake rejected, reason {:#04x}", welcome.get(1).unwrap_or(&0)),
        _ => bail!("malformed data WELCOME"),
    }
    let nonce = u64::from_le_bytes(welcome[24..32].try_into()?);
    tx.send(DataMessage::Welcome { nonce }).await?;

    loop {
        let msg = tokio::select! {
            msg = ws.next() => msg,
            _ = &mut cancelled => {
                // Status 1000 ends the stream rather than leaving it waiting for a resume.
                let _ = ws.close(None).await;
                return Ok(());
            }
        };
        let Some(msg) = msg else { break };
        match msg? {
            Message::Binary(bytes) => tx.send(DataMessage::Bytes(bytes.to_vec())).await?,
            Message::Close(frame) => {
                if let Some(frame) = frame.filter(|f| u16::from(f.code) != 1000 && u16::from(f.code) != 4000) {
                    bail!("data stream closed: {} {}", u16::from(frame.code), frame.reason);
                }
                break;
            }
            _ => {}
        }
    }
    Ok(())
}

/// What the stream splitter found in a chunk of the terminal stream.
pub enum StreamItem {
    /// Program output to feed the terminal emulator.
    Output(Vec<u8>),
    /// A Rex message, `ESC _ rex1;<nonce>;<json> ESC \`, with a matching nonce.
    Rex(Value),
    /// The Ghostty snapshot at the head of the stream, complete.
    Snapshot(Vec<u8>),
}

enum State {
    /// Inside the binary snapshot, collecting it until the first Rex message.
    Snapshot { snapshot: Vec<u8> },
    Normal,
    Esc,
    Apc { buf: Vec<u8>, esc: bool },
}

/// Splits a terminal stream into program output and Rex messages. The stream starts with a
/// Ghostty snapshot, which ends where the first Rex message (a `resize`) begins.
pub struct StreamSplitter {
    prefix: Vec<u8>,
    state: State,
}

const MAX_APC: usize = 1 << 20;

impl StreamSplitter {
    pub fn new(nonce: u64) -> Self {
        Self {
            prefix: format!("rex1;{nonce};").into_bytes(),
            state: State::Snapshot { snapshot: Vec::new() },
        }
    }

    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<StreamItem>) {
        if let State::Snapshot { snapshot } = &mut self.state {
            let mut marker = vec![0x1b, b'_'];
            marker.extend_from_slice(&self.prefix);
            // Search only the new bytes, plus enough before them for a marker split in two.
            let from = snapshot.len().saturating_sub(marker.len() - 1);
            snapshot.extend_from_slice(bytes);
            if let Some(at) = find(&snapshot[from..], &marker).map(|i| from + i) {
                let rest = snapshot.split_off(at);
                out.push(StreamItem::Snapshot(std::mem::take(snapshot)));
                self.state = State::Normal;
                self.feed_live(&rest, out);
            }
            return;
        }
        self.feed_live(bytes, out);
    }

    fn feed_live(&mut self, bytes: &[u8], out: &mut Vec<StreamItem>) {
        let mut output = Vec::with_capacity(bytes.len());
        for &b in bytes {
            match &mut self.state {
                State::Snapshot { .. } => unreachable!(),
                State::Normal => {
                    if b == 0x1b {
                        self.state = State::Esc;
                    } else {
                        output.push(b);
                    }
                }
                State::Esc => {
                    if b == b'_' {
                        self.state = State::Apc { buf: Vec::new(), esc: false };
                    } else {
                        output.push(0x1b);
                        if b == 0x1b {
                            continue;
                        }
                        output.push(b);
                        self.state = State::Normal;
                    }
                }
                State::Apc { buf, esc } => {
                    if *esc && b == b'\\' {
                        let body = std::mem::take(buf);
                        self.state = State::Normal;
                        match body.strip_prefix(self.prefix.as_slice()) {
                            Some(json) => {
                                flush(&mut output, out);
                                if let Ok(value) = serde_json::from_slice(json) {
                                    out.push(StreamItem::Rex(value));
                                }
                            }
                            // An APC that a program printed. Pass it through untouched.
                            None => {
                                output.extend_from_slice(b"\x1b_");
                                output.extend_from_slice(&body);
                                output.extend_from_slice(b"\x1b\\");
                            }
                        }
                    } else {
                        if *esc {
                            buf.push(0x1b);
                        }
                        *esc = b == 0x1b;
                        if !*esc {
                            buf.push(b);
                        }
                        if buf.len() > MAX_APC {
                            output.extend_from_slice(b"\x1b_");
                            output.append(buf);
                            self.state = State::Normal;
                        }
                    }
                }
            }
        }
        flush(&mut output, out);
    }
}

fn flush(output: &mut Vec<u8>, out: &mut Vec<StreamItem>) {
    if !output.is_empty() {
        out.push(StreamItem::Output(std::mem::take(output)));
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(splitter: &mut StreamSplitter, chunks: &[&[u8]]) -> Vec<String> {
        let mut out = Vec::new();
        for chunk in chunks {
            splitter.feed(chunk, &mut out);
        }
        out.into_iter()
            .map(|item| match item {
                StreamItem::Output(b) => format!("out:{}", String::from_utf8_lossy(&b)),
                StreamItem::Rex(v) => format!("rex:{}", v["type"].as_str().unwrap()),
                StreamItem::Snapshot(b) => format!("snap:{}", b.len()),
            })
            .collect()
    }

    #[test]
    fn skips_snapshot_and_splits_messages_across_chunks() {
        let mut s = StreamSplitter::new(42);
        let items = run(
            &mut s,
            &[
                b"GHOSTSNP\x00\x1b_junk\x01\x1b",
                b"_rex1;4",
                b"2;{\"type\":\"resize\"}\x1b",
                b"\\hello\x1b[1m\x1b_other\x1b\\x",
                b"\x1b_rex1;7;{\"type\":\"theme\"}\x1b\\",
            ],
        );
        assert_eq!(
            items,
            [
                "snap:16",
                "rex:resize",
                "out:hello\x1b[1m\x1b_other\x1b\\x",
                "out:\x1b_rex1;7;{\"type\":\"theme\"}\x1b\\",
            ]
        );
    }
}
