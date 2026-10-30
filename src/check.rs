//! `rex-gpui --check SESSION`: mirror a session's focused terminal without a window and
//! compare the local grid with the server's own plain-text rendering of the screen.

use std::time::Duration;

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::Term;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::vte::ansi::Processor;
use anyhow::{Context as _, Result, bail};
use serde_json::json;

use crate::rex::control::Control;
use crate::rex::data::{self, DataMessage, StreamItem, StreamSplitter};
use crate::mirror::fetch_vt;
use crate::terminal::{Listener, term_config};

pub async fn run(control: Control, target: &str, command: Option<String>) -> Result<()> {
    let reply = control.call("session.attach", json!({"session_id": resolve(&control, target).await?})).await?;
    let view = &reply["view"];
    let session_id = view["session_id"].as_str().context("no session id")?.to_string();
    let active = view["active_window_id"].as_str().unwrap_or_default();
    let window = view["windows"].as_array().into_iter().flatten()
        .find(|w| w["window_id"] == active).context("no active window")?;
    let block_id = window["focused_block_id"].as_str().context("no focused block")?.to_string();

    let (rx, _guard) = data::open_terminal_stream(&control, &session_id, &block_id).await?;
    let mut term = Term::new(term_config(), &TermSize::new(80, 24), Listener);
    let mut processor: Processor = Processor::new();
    let mut splitter = None;
    let mut pending: Option<Vec<u8>> = None;
    let mut redrawn = false;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut typed = command.is_none();
    loop {
        let msg = match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(msg)) => msg,
            Ok(Err(_)) => break,
            Err(_) => break,
        };
        match msg {
            DataMessage::Welcome { nonce } => splitter = Some(StreamSplitter::new(nonce)),
            DataMessage::Closed(reason) => bail!("stream closed: {reason:?}"),
            DataMessage::Bytes(bytes) => {
                let mut items = Vec::new();
                splitter.as_mut().context("bytes before WELCOME")?.feed(&bytes, &mut items);
                for item in items {
                    match item {
                        StreamItem::SnapshotEnd => pending = Some(Vec::new()),
                        StreamItem::Rex(msg) if msg["type"] == "resize" => {
                            let cols = msg["cols"].as_u64().unwrap_or(80) as usize;
                            let rows = msg["rows"].as_u64().unwrap_or(24) as usize;
                            term.resize(TermSize::new(cols, rows));
                        }
                        StreamItem::Rex(_) => {}
                        StreamItem::Output(bytes) if redrawn => processor.advance(&mut term, &bytes),
                        StreamItem::Output(bytes) => {
                            if let Some(p) = &mut pending {
                                p.extend_from_slice(&bytes)
                            }
                        }
                    }
                }
                if let (Some(p), false) = (&pending, redrawn) {
                    let included = p.len();
                    let content = fetch_vt(&control, &session_id, &block_id).await?;
                    eprintln!("redraw: {} bytes of VT", content.len());
                    let size = TermSize::new(term.columns(), term.screen_lines());
                    term = Term::new(term_config(), &size, Listener);
                    processor = Processor::new();
                    processor.advance(&mut term, content.as_bytes());
                    processor.advance(&mut term, &p[included..]);
                    redrawn = true;
                }
                if redrawn && !typed {
                    typed = true;
                    let data = format!("{}\n", command.as_deref().unwrap_or_default());
                    control.block_call("com.superlogical.terminal.write", &session_id, &block_id, json!({"data": data})).await?;
                }
            }
        }
    }

    let server = control
        .block_call("com.superlogical.terminal.format", &session_id, &block_id, json!({"format": "text"}))
        .await?;
    let server: Vec<&str> = server["content"].as_str().unwrap_or_default().lines().collect();
    let local = screen_text(&term);
    // `format` includes scrollback, so compare the screen against its last lines.
    let tail = &server[server.len().saturating_sub(local.len())..];
    let trimmed_local: Vec<&str> = local.iter().map(|l| l.trim_end()).collect();
    let trimmed_tail: Vec<&str> = tail.iter().map(|l| l.trim_end()).collect();
    let mut start = 0;
    while start < trimmed_local.len() && trimmed_local[start..].len() > trimmed_tail.len() {
        start += 1;
    }
    let local_cmp: Vec<&str> = trimmed_local.iter().rev().skip_while(|l| l.is_empty()).collect::<Vec<_>>().into_iter().rev().copied().collect();
    let server_cmp: Vec<&str> = trimmed_tail.iter().rev().skip_while(|l| l.is_empty()).collect::<Vec<_>>().into_iter().rev().copied().collect();
    let n = local_cmp.len().min(server_cmp.len());
    let mismatches: Vec<usize> = (0..n)
        .filter(|&i| local_cmp[local_cmp.len() - n + i] != server_cmp[server_cmp.len() - n + i])
        .collect();
    println!("size: {}x{}  scrollback: {}", term.columns(), term.screen_lines(), term.grid().history_size());
    println!("--- local screen ---");
    for line in &local_cmp {
        println!("|{line}");
    }
    if mismatches.is_empty() {
        println!("OK: the last {n} lines match the server");
        Ok(())
    } else {
        for i in mismatches {
            println!("line {i}:\n  local : {:?}\n  server: {:?}", local_cmp[local_cmp.len() - n + i], server_cmp[server_cmp.len() - n + i]);
        }
        bail!("local screen differs from the server")
    }
}

async fn resolve(control: &Control, target: &str) -> Result<String> {
    let reply = control.call("session.list", json!({})).await?;
    reply["sessions"].as_array().into_iter().flatten()
        .find(|s| s["label"] == target || s["session_id"] == target)
        .and_then(|s| s["session_id"].as_str())
        .map(str::to_string)
        .with_context(|| format!("no session {target}"))
}

fn screen_text(term: &Term<Listener>) -> Vec<String> {
    (0..term.screen_lines())
        .map(|row| {
            (0..term.columns())
                .map(|col| &term.grid()[Point::new(Line(row as i32), Column(col))])
                .filter(|cell| !cell.flags.contains(alacritty_terminal::term::cell::Flags::WIDE_CHAR_SPACER))
                .map(|cell| cell.c)
                .filter(|c| *c != '\0')
                .collect()
        })
        .collect()
}
