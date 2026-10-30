//! `rex-gpui --check SESSION [COMMAND]`: mirror a session's focused terminal without a
//! window, the way a pane does, and compare the copy with the server's own rendering.

use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use serde_json::json;

use crate::mirror::{self, MirrorEvent};
use crate::rex::control::Control;

pub async fn run(control: Control, target: &str, command: Option<String>) -> Result<()> {
    let session_id = resolve(&control, target).await?;
    let reply = control.call("session.attach", json!({"session_id": session_id})).await?;
    let view = &reply["view"];
    let active = view["active_window_id"].as_str().unwrap_or_default();
    let window = view["windows"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|w| w["window_id"] == active)
        .context("no active window")?;
    let block_id = window["focused_block_id"].as_str().context("no focused block")?.to_string();

    let started = std::time::Instant::now();
    let copy = mirror::start(control.clone(), session_id.clone(), block_id.clone());
    // Wait for the copy to go live: restored from the snapshot, or redrawn.
    loop {
        match tokio::time::timeout(Duration::from_secs(5), copy.events.recv()).await {
            Ok(Ok(MirrorEvent::Status(None))) => break,
            Ok(Ok(MirrorEvent::Status(Some(status)))) if status.starts_with("Reconnecting") => bail!("{status}"),
            Ok(Ok(_)) => {}
            Ok(Err(_)) => bail!("the mirror stopped"),
            Err(_) => bail!("the copy didn't go live within 5 s"),
        }
    }
    eprintln!("live after {:.0} ms", started.elapsed().as_secs_f64() * 1000.);

    if let Some(command) = command {
        let data = format!("{command}\n");
        control
            .block_call("com.superlogical.terminal.write", &session_id, &block_id, json!({"data": data}))
            .await?;
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let server = control
        .block_call("com.superlogical.terminal.format", &session_id, &block_id, json!({"format": "text", "trim": true}))
        .await?;
    let server = trimmed_lines(server["content"].as_str().unwrap_or_default());
    let (local, size) = {
        let shared = copy.term.lock().unwrap();
        (trimmed_lines(&shared.term.plain_text()), (shared.term.cols(), shared.term.rows()))
    };
    // Both include scrollback; compare as many of the last lines as both have.
    let n = local.len().min(server.len());
    let (local, server) = (&local[local.len() - n..], &server[server.len() - n..]);
    println!("size: {}x{}", size.0, size.1);
    println!("--- local copy (last 12 lines) ---");
    for line in &local[n.saturating_sub(12)..] {
        println!("|{line}");
    }
    let mismatches: Vec<usize> = (0..n).filter(|&i| local[i] != server[i]).collect();
    if mismatches.is_empty() {
        println!("OK: the last {n} lines match the server");
        return Ok(());
    }
    for i in mismatches.iter().take(10) {
        println!("line {i}:\n  local : {:?}\n  server: {:?}", local[*i], server[*i]);
    }
    bail!("{} of {n} lines differ from the server", mismatches.len())
}

/// Lines with trailing spaces and trailing blank lines removed.
fn trimmed_lines(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = text.lines().map(|l| l.trim_end().to_string()).collect();
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines
}

async fn resolve(control: &Control, target: &str) -> Result<String> {
    let reply = control.call("session.list", json!({})).await?;
    reply["sessions"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|s| s["label"] == target || s["session_id"] == target)
        .and_then(|s| s["session_id"].as_str())
        .map(str::to_string)
        .with_context(|| format!("no session {target}"))
}
