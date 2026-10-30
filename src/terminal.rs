//! One Rex terminal block: its data stream, a local `alacritty_terminal` model of the
//! screen, and the GPUI view that paints it and sends keys back.
//!
//! The server is the real terminal. The local model only mirrors its output for drawing, so
//! replies a program asks for, such as cursor reports, are left to the server.

use std::ops::Range;

use alacritty_terminal::event::{Event as TermEvent, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point as GridPoint, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Processor, Rgb};
use gpui::{
    App, Bounds, ClipboardItem, Context, FocusHandle, Focusable, Font, FontWeight, Hsla,
    KeyDownEvent, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels,
    Point, ScrollDelta, ScrollWheelEvent, SharedString, Task, TextRun, Window, canvas, div, fill, font, outline, point, prelude::*, px, size,
};
use serde_json::{Value, json};

use crate::keys;
use crate::theme::ui;
use crate::rex::control::Control;
use crate::rex::data::{self, DataMessage, StreamItem, StreamSplitter};

const FORMAT_CHUNK: u64 = 900_000;
/// Width kept free at the right of the grid for the scrollbar.
const SCROLLBAR_WIDTH: f32 = 10.;

pub struct Listener;

impl EventListener for Listener {
    fn send_event(&self, _event: TermEvent) {}
}

enum Phase {
    Connecting,
    /// Skipping the Ghostty snapshot at the head of the stream.
    Snapshot,
    /// Fetching the screen as VT text. Output that arrives meanwhile waits in `pending`;
    /// the first `included` bytes of it were already on screen when the fetch began.
    Redraw { pending: Vec<u8>, included: Option<usize> },
    Live,
    Closed(Option<String>),
}

pub struct TerminalPane {
    control: Control,
    pub session_id: String,
    pub block_id: String,
    term: Term<Listener>,
    processor: Processor,
    splitter: Option<StreamSplitter>,
    phase: Phase,
    /// The terminal's colors from the server, which override the app theme's.
    server_theme: Option<Value>,
    /// Scroll-wheel movement too small to make a whole line yet.
    scroll_remainder: f32,
    cell: Option<CellMetrics>,
    /// Where the grid was last painted, to turn mouse positions into cells.
    origin: Option<Point<Pixels>>,
    /// A left-button drag is selecting text locally rather than going to the program.
    selecting: bool,
    /// The grid's height when last painted, which is also the scrollbar's track.
    track_height: Option<Pixels>,
    /// The scrollbar thumb is being dragged, held this far below its top.
    scroll_drag: Option<Pixels>,
    /// A multi-line paste waiting for the user to confirm it. See `paste`.
    pending_paste: Option<String>,
    /// The size we last asked the server for.
    requested: Option<(u16, u16)>,
    exit_code: Option<i64>,
    /// What the header and tab show: a title a program set, else the program and directory.
    pub info: PaneInfo,
    focus_handle: FocusHandle,
    _stream: Task<()>,
}

#[derive(Default, Clone)]
pub struct PaneInfo {
    pub title: String,
    pub process: String,
    pub cwd: String,
    pub home: String,
}

impl PaneInfo {
    fn short_cwd(&self) -> String {
        match self.cwd.strip_prefix(&self.home) {
            Some(rest) if !self.home.is_empty() => format!("~{rest}"),
            _ => self.cwd.clone(),
        }
    }

    pub fn display_title(&self) -> String {
        if !self.title.trim().is_empty() {
            return self.title.trim().to_string();
        }
        match (self.process.is_empty(), self.cwd.is_empty()) {
            (true, true) => "Terminal".into(),
            (false, true) => self.process.clone(),
            (true, false) => self.short_cwd(),
            (false, false) => format!("{} {}", self.process, self.short_cwd()),
        }
    }

    fn apply_process(&mut self, process: &Value) {
        let foreground = &process["foreground"];
        if let Some(name) = foreground["name"].as_str() {
            self.process = name.to_string();
        }
        if let Some(cwd) = foreground["cwd"].as_str().filter(|c| !c.is_empty()) {
            self.cwd = cwd.to_string();
        }
        if let Some(home) = process["home"].as_str() {
            self.home = home.to_string();
        }
    }
}

#[derive(Clone, Copy)]
struct CellMetrics {
    width: Pixels,
    height: Pixels,
}

impl TerminalPane {
    pub fn new(control: Control, session_id: String, block_id: String, cx: &mut Context<Self>) -> Self {
        let term = Term::new(term_config(), &TermSize::new(80, 24), Listener);
        let stream = cx.spawn({
            let control = control.clone();
            let (session_id, block_id) = (session_id.clone(), block_id.clone());
            async move |this, cx| {
                // Held for as long as this task runs, which is as long as the pane lives.
                let (rx, _guard) = match data::open_terminal_stream(&control, &session_id, &block_id).await {
                    Ok(stream) => stream,
                    Err(err) => {
                        let _ = this.update(cx, |this, cx| {
                            this.phase = Phase::Closed(Some(format!("{err:#}")));
                            cx.notify();
                        });
                        return;
                    }
                };
                let (title, process) = futures_util::join!(
                    control.block_call("com.superlogical.terminal.title", &session_id, &block_id, json!({})),
                    control.block_call("com.superlogical.terminal.process", &session_id, &block_id, json!({})),
                );
                let _ = this.update(cx, |this, cx| {
                    if let Ok(title) = title {
                        this.info.title = title["title"].as_str().unwrap_or_default().to_string();
                    }
                    if let Ok(process) = process {
                        this.info.apply_process(&process);
                    }
                    cx.notify();
                });
                while let Ok(msg) = rx.recv().await {
                    if this.update(cx, |this, cx| this.on_data(msg, cx)).is_err() {
                        break;
                    }
                }
            }
        });
        Self {
            control,
            session_id,
            block_id,
            term,
            processor: Processor::new(),
            splitter: None,
            phase: Phase::Connecting,
            server_theme: None,
            scroll_remainder: 0.,
            cell: None,
            origin: None,
            selecting: false,
            track_height: None,
            scroll_drag: None,
            pending_paste: None,
            requested: None,
            exit_code: None,
            info: PaneInfo::default(),
            focus_handle: cx.focus_handle(),
            _stream: stream,
        }
    }

    fn on_data(&mut self, msg: DataMessage, cx: &mut Context<Self>) {
        match msg {
            DataMessage::Welcome { nonce } => {
                self.splitter = Some(StreamSplitter::new(nonce));
                self.phase = Phase::Snapshot;
            }
            DataMessage::Bytes(bytes) => {
                let Some(splitter) = &mut self.splitter else { return };
                let mut items = Vec::new();
                splitter.feed(&bytes, &mut items);
                for item in items {
                    self.on_item(item);
                }
                if let Phase::Redraw { pending, included: included @ None } = &mut self.phase {
                    *included = Some(pending.len());
                    self.fetch_redraw(cx);
                }
            }
            DataMessage::Closed(reason) => self.phase = Phase::Closed(reason),
        }
        cx.notify();
    }

    /// A `block_event` from the server for this block, such as a new title.
    pub fn on_block_event(&mut self, name: &str, payload: &Value, cx: &mut Context<Self>) {
        match name {
            "title_changed" => self.info.title = payload["title"].as_str().unwrap_or_default().into(),
            "process_changed" => self.info.apply_process(payload),
            "pwd_changed" => {
                let pwd = payload["pwd"].as_str().unwrap_or_default();
                // OSC 7 reports file://host/path.
                let path = pwd.strip_prefix("file://").map_or(pwd, |rest| {
                    rest.find('/').map_or(rest, |slash| &rest[slash..])
                });
                if !path.is_empty() {
                    self.info.cwd = path.to_string();
                }
            }
            "child_exited" => self.exit_code = payload["exit_code"].as_i64(),
            _ => return,
        }
        cx.notify();
    }

    fn theme(&self) -> Theme {
        match &self.server_theme {
            Some(msg) => Theme::from_message(msg),
            None => Theme::default(),
        }
    }

    pub fn background(&self) -> Hsla {
        to_hsla(self.theme().background)
    }

    fn on_item(&mut self, item: StreamItem) {
        match item {
            StreamItem::SnapshotEnd => {
                self.phase = Phase::Redraw { pending: Vec::new(), included: None };
            }
            StreamItem::Output(bytes) => match &mut self.phase {
                Phase::Live => self.processor.advance(&mut self.term, &bytes),
                Phase::Redraw { pending, .. } => pending.extend_from_slice(&bytes),
                _ => {}
            },
            StreamItem::Rex(msg) => match msg["type"].as_str() {
                Some("resize") => {
                    let cols = msg["cols"].as_u64().unwrap_or(80).max(1) as usize;
                    let rows = msg["rows"].as_u64().unwrap_or(24).max(1) as usize;
                    self.term.resize(TermSize::new(cols, rows));
                }
                Some("theme") => self.server_theme = Some(msg.clone()),
                Some("child_exited") => self.exit_code = msg["exit_code"].as_i64(),
                _ => {}
            },
        }
    }

    /// Read the screen and scrollback as VT text, which stands in for the Ghostty snapshot.
    fn fetch_redraw(&mut self, cx: &mut Context<Self>) {
        let control = self.control.clone();
        let (session_id, block_id) = (self.session_id.clone(), self.block_id.clone());
        cx.spawn(async move |this, cx| {
            let content = fetch_vt(&control, &session_id, &block_id).await;
            let _ = this.update(cx, |this, cx| {
                let Phase::Redraw { pending, included } =
                    std::mem::replace(&mut this.phase, Phase::Live)
                else {
                    return;
                };
                match content {
                    Ok(content) => {
                        let size = TermSize::new(this.term.columns(), this.term.screen_lines());
                        this.term = Term::new(term_config(), &size, Listener);
                        this.processor = Processor::new();
                        this.processor.advance(&mut this.term, content.as_bytes());
                    }
                    Err(err) => log::warn!("reading the screen of {}: {err:#}", this.block_id),
                }
                let included = included.unwrap_or(0).min(pending.len());
                this.processor.advance(&mut this.term, &pending[included..]);
                cx.notify();
            });
        })
        .detach();
    }

    /// Paste `text`. A program that turned on bracketed paste gets it as one block. Without
    /// it, a shell runs each line as it arrives, so a multi-line paste waits for a yes first.
    fn paste(&mut self, text: String, cx: &mut Context<Self>) {
        self.term.selection = None;
        self.selecting = false;
        let lines = text.trim_end_matches(['\n', '\r']).lines().count();
        if lines > 1 && !self.term.mode().contains(TermMode::BRACKETED_PASTE) {
            self.pending_paste = Some(text);
            cx.notify();
            return;
        }
        self.send_event("paste", json!({"text": text}));
    }

    fn confirm_paste(&mut self, as_one_line: bool, cx: &mut Context<Self>) {
        if let Some(text) = self.pending_paste.take() {
            let text = if as_one_line {
                text.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" ")
            } else {
                text
            };
            self.send_event("paste", json!({"text": text}));
        }
        cx.notify();
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ks = &event.keystroke;
        let m = &ks.modifiers;
        if self.pending_paste.is_some() {
            match ks.key.as_str() {
                "enter" => self.confirm_paste(false, cx),
                "escape" => {
                    self.pending_paste = None;
                    cx.notify();
                }
                _ => {}
            }
            cx.stop_propagation();
            return;
        }
        // Clipboard keys stay with the client, as in most Linux terminals.
        if m.control && m.shift && !m.alt && ks.key == "v" {
            if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                self.paste(text, cx);
            }
            cx.stop_propagation();
            return;
        }
        if m.control && m.shift && !m.alt && ks.key == "c" {
            let text = self.term.selection_to_string().unwrap_or_default();
            if !text.is_empty() {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
            cx.stop_propagation();
            return;
        }
        if m.shift && !m.control && matches!(ks.key.as_str(), "pageup" | "pagedown") {
            let rows = self.term.screen_lines() as i32;
            let delta = if ks.key == "pageup" { rows } else { -rows };
            self.term.scroll_display(Scroll::Delta(delta));
            cx.notify();
            cx.stop_propagation();
            return;
        }
        let Some(key) = keys::client_key(ks, event.is_held) else { return };
        cx.stop_propagation();
        if self.term.selection.take().is_some() {
            cx.notify();
        }
        // Typing returns the view to the bottom, as other terminals do.
        if self.term.grid().display_offset() != 0 {
            self.term.scroll_display(Scroll::Bottom);
            cx.notify();
        }
        self.control.send(
            "client.key",
            json!({"session_id": self.session_id, "block_id": self.block_id, "key": key}),
        );
    }

    fn send_event(&self, kind: &str, payload: Value) {
        self.control.send(
            "block.event",
            json!({
                "session_id": self.session_id,
                "block_id": self.block_id,
                "event": {"type": kind, "payload": payload},
            }),
        );
    }

    fn on_scroll(&mut self, event: &ScrollWheelEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(cell) = self.cell else { return };
        let mode = *self.term.mode();
        // Full-screen programs and programs that read the mouse get real wheel events. The
        // server's terminal turns them into arrow keys for programs that want that.
        if mode.intersects(TermMode::ALT_SCREEN | TermMode::MOUSE_MODE) && !event.modifiers.shift {
            let (dx, dy, unit) = match event.delta {
                ScrollDelta::Lines(d) => (-d.x, -d.y, "line"),
                ScrollDelta::Pixels(d) => (-f32::from(d.x), -f32::from(d.y), "pixel"),
            };
            if dx == 0.0 && dy == 0.0 {
                return;
            }
            let (x, y) = self.cell_at(event.position).unwrap_or((0, 0));
            self.send_event(
                "wheel",
                json!({
                    "delta_x": dx, "delta_y": dy, "delta_mode": unit,
                    "x": x, "y": y, "mods": keys::mods_string(&event.modifiers),
                }),
            );
            return;
        }
        // A wheel notch scrolls 3 lines, as in Ghostty. Touchpads report pixels; keep the
        // part of a line they haven't made up yet so slow swipes still move.
        let lines = match event.delta {
            ScrollDelta::Lines(delta) => delta.y * scroll_multiplier(),
            ScrollDelta::Pixels(delta) => delta.y / cell.height,
        };
        let lines = lines + self.scroll_remainder;
        let whole = lines.trunc();
        self.scroll_remainder = lines - whole;
        if whole != 0. {
            self.term.scroll_display(Scroll::Delta(whole as i32));
            cx.notify();
        }
    }

    /// The cell under a window position, as `(column, row)` on screen, clamped to the grid.
    fn cell_at(&self, position: Point<Pixels>) -> Option<(usize, usize)> {
        let (origin, cell) = (self.origin?, self.cell?);
        let col = ((position.x - origin.x) / cell.width).floor().max(0.0) as usize;
        let row = ((position.y - origin.y) / cell.height).floor().max(0.0) as usize;
        Some((
            col.min(self.term.columns().saturating_sub(1)),
            row.min(self.term.screen_lines().saturating_sub(1)),
        ))
    }

    fn grid_point(&self, position: Point<Pixels>) -> Option<(GridPoint, Side)> {
        let (col, row) = self.cell_at(position)?;
        let cell = self.cell?;
        let within = (position.x - self.origin?.x) / cell.width - col as f32;
        let side = if within < 0.5 { Side::Left } else { Side::Right };
        let line = Line(row as i32 - self.term.grid().display_offset() as i32);
        Some((GridPoint::new(line, Column(col)), side))
    }

    /// Whether mouse events belong to the program. Shift keeps them local, for selecting.
    fn program_wants_mouse(&self, modifiers: &Modifiers) -> bool {
        self.term.mode().intersects(TermMode::MOUSE_MODE) && !modifiers.shift
    }

    fn send_mouse(&self, action: &str, button: Option<MouseButton>, held: Option<MouseButton>, position: Point<Pixels>, modifiers: &Modifiers) {
        let Some((x, y)) = self.cell_at(position) else { return };
        let name = |b: MouseButton| match b {
            MouseButton::Left => "left",
            MouseButton::Right => "right",
            MouseButton::Middle => "middle",
            _ => "unknown",
        };
        let mut payload = json!({"action": action, "x": x, "y": y, "mods": keys::mods_string(modifiers)});
        if let Some(button) = button {
            payload["button"] = json!(name(button));
        }
        if let Some(held) = held {
            payload["buttons"] = json!({name(held): true});
        }
        self.send_event("mouse", payload);
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle);
        if self.program_wants_mouse(&event.modifiers) {
            self.send_mouse("press", Some(event.button), Some(event.button), event.position, &event.modifiers);
            return;
        }
        if event.button == MouseButton::Middle {
            if let Some(text) = cx.read_from_primary().and_then(|item| item.text()) {
                self.paste(text, cx);
            }
            return;
        }
        if event.button != MouseButton::Left {
            return;
        }
        let Some((point, side)) = self.grid_point(event.position) else { return };
        let kind = match event.click_count {
            2 => SelectionType::Semantic,
            n if n >= 3 => SelectionType::Lines,
            _ => SelectionType::Simple,
        };
        self.term.selection = Some(Selection::new(kind, point, side));
        self.selecting = true;
        cx.notify();
    }

    /// Scroll so the thumb's top follows `y`, measured from the top of the track.
    fn scroll_to_thumb(&mut self, top: f32, cx: &mut Context<Self>) {
        let Some(track) = self.track_height else { return };
        let history = self.term.grid().history_size() as f32;
        let total = history + self.term.screen_lines() as f32;
        // The thumb's top marks the first visible line, counting from the oldest one.
        let first_line = (top / f32::from(track) * total).clamp(0.0, history);
        let offset = (history - first_line).round() as i32;
        let current = self.term.grid().display_offset() as i32;
        if offset != current {
            self.term.scroll_display(Scroll::Delta(offset - current));
            cx.notify();
        }
    }

    fn on_scrollbar_down(&mut self, event: &MouseDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        let (Some(origin), Some(track)) = (self.origin, self.track_height) else { return };
        let history = self.term.grid().history_size() as f32;
        let total = history + self.term.screen_lines() as f32;
        let thumb_height = f32::from(track) * self.term.screen_lines() as f32 / total;
        let y = f32::from(event.position.y - origin.y);
        let thumb_top = self.thumb_top(track);
        // Grabbing the thumb keeps its offset under the pointer; clicking the track centers it.
        let grab = if (thumb_top..thumb_top + thumb_height).contains(&y) { y - thumb_top } else { thumb_height / 2. };
        self.scroll_drag = Some(px(grab));
        self.scroll_to_thumb(y - grab, cx);
    }

    fn thumb_top(&self, track: Pixels) -> f32 {
        let history = self.term.grid().history_size() as f32;
        let total = history + self.term.screen_lines() as f32;
        let first_line = history - self.term.grid().display_offset() as f32;
        f32::from(track) * first_line / total
    }

    /// End a drag-selection, keeping what it selected as the primary selection.
    fn finish_selection(&mut self, cx: &mut Context<Self>) {
        self.selecting = false;
        // Like other X11 and Wayland terminals, a selection becomes the primary selection.
        if let Some(text) = self.term.selection_to_string().filter(|t| !t.is_empty()) {
            cx.write_to_primary(ClipboardItem::new_string(text));
        } else {
            self.term.selection = None;
        }
        cx.notify();
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if let (Some(grab), Some(origin)) = (self.scroll_drag, self.origin) {
            if event.pressed_button == Some(MouseButton::Left) {
                self.scroll_to_thumb(f32::from(event.position.y - origin.y - grab), cx);
            } else {
                self.scroll_drag = None;
            }
            return;
        }
        // The button came up somewhere this pane didn't hear about, such as over a menu.
        if self.selecting && event.pressed_button != Some(MouseButton::Left) {
            self.finish_selection(cx);
            return;
        }
        if self.selecting {
            if let (Some((point, side)), Some(selection)) =
                (self.grid_point(event.position), self.term.selection.as_mut())
            {
                selection.update(point, side);
                cx.notify();
            }
            return;
        }
        // Report drags; plain motion only matters to programs in any-motion mode.
        let wants_motion = event.pressed_button.is_some()
            || self.term.mode().contains(TermMode::MOUSE_MOTION);
        if wants_motion && self.program_wants_mouse(&event.modifiers) {
            self.send_mouse("motion", None, event.pressed_button, event.position, &event.modifiers);
        }
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.scroll_drag.take().is_some() {
            return;
        }
        if self.selecting {
            self.finish_selection(cx);
            return;
        }
        if self.program_wants_mouse(&event.modifiers) {
            self.send_mouse("release", Some(event.button), None, event.position, &event.modifiers);
        }
    }

    /// Ask the server for the size that fills `bounds`, when it differs from the last request.
    fn request_size(&mut self, bounds: Bounds<Pixels>, cell: CellMetrics) {
        self.origin = Some(bounds.origin);
        self.track_height = Some(bounds.size.height);
        let width = bounds.size.width - px(SCROLLBAR_WIDTH);
        let cols = ((width / cell.width).floor() as u16).max(2);
        let rows = ((bounds.size.height / cell.height).floor() as u16).max(1);
        if self.requested == Some((cols, rows)) || !matches!(self.phase, Phase::Live | Phase::Redraw { .. }) {
            return;
        }
        self.requested = Some((cols, rows));
        self.control.send(
            "com.superlogical.terminal.resize",
            json!({
                "session_id": self.session_id,
                "block_id": self.block_id,
                "args": {
                    "columns": cols,
                    "rows": rows,
                    "cell_width_px": f32::from(cell.width).round().max(1.0) as u16,
                    "cell_height_px": f32::from(cell.height).round().max(1.0) as u16,
                },
            }),
        );
    }

    pub fn status(&self) -> Option<String> {
        match &self.phase {
            Phase::Connecting | Phase::Snapshot => Some("Connecting…".into()),
            Phase::Closed(Some(reason)) => Some(format!("Disconnected: {reason}")),
            Phase::Closed(None) => Some("Closed".into()),
            _ => self.exit_code.map(|code| format!("Process exited with code {code}")),
        }
    }
}

pub async fn fetch_vt(control: &Control, session_id: &str, block_id: &str) -> anyhow::Result<String> {
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

pub fn term_config() -> Config {
    Config { scrolling_history: 10_000, ..Config::default() }
}

impl Focusable for TerminalPane {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

pub fn terminal_font() -> Font {
    font(std::env::var("REX_FONT").unwrap_or_else(|_| "Hack Nerd Font Mono".into()))
}

/// Lines per wheel notch: `REX_SCROLL_LINES`, else 3.
fn scroll_multiplier() -> f32 {
    std::env::var("REX_SCROLL_LINES").ok().and_then(|s| s.parse().ok()).unwrap_or(3.)
}

pub fn font_size() -> Pixels {
    px(std::env::var("REX_FONT_SIZE").ok().and_then(|s| s.parse().ok()).unwrap_or(14.0))
}

impl Render for TerminalPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let font = terminal_font();
        let font_size = font_size();
        let cell = {
            let text = window.text_system();
            let id = text.resolve_font(&font);
            let width = text.advance(id, font_size, 'm').map(|s| s.width).unwrap_or(font_size * 0.6);
            let height = (font_size * 1.25).round();
            CellMetrics { width, height }
        };
        self.cell = Some(cell);

        let focused = self.focus_handle.is_focused(window);
        let frame = Frame::capture(&self.term, &self.theme(), focused);
        let background = frame.background;
        let status = self.status();
        let entity = cx.entity();
        let scrollbar = self.render_scrollbar(cx);
        let paste_prompt = self.pending_paste.as_ref().map(|text| self.render_paste_prompt(text, cx));

        div()
            .id(SharedString::from(self.block_id.clone()))
            .track_focus(&self.focus_handle)
            .key_context("Terminal")
            .on_key_down(cx.listener(Self::on_key_down))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::on_mouse_up))
            // A drag that ends outside the pane still has to end.
            .on_mouse_up_out(MouseButton::Left, cx.listener(|this, _, _, cx| {
                this.scroll_drag = None;
                if this.selecting {
                    this.finish_selection(cx);
                }
            }))
            .size_full()
            .relative()
            .overflow_hidden()
            .bg(background)
            .child(
                canvas(
                    move |bounds, _window, cx| {
                        entity.update(cx, |this, _| this.request_size(bounds, cell));
                    },
                    move |bounds, (), window, cx| frame.paint(bounds, cell, font, font_size, window, cx),
                )
                .size_full(),
            )
            .children(scrollbar)
            .children(paste_prompt)
            .children(status.map(|status| {
                div()
                    .absolute()
                    .bottom_0()
                    .right_0()
                    .px_2()
                    .py_1()
                    .text_xs()
                    .text_color(gpui::white().opacity(0.7))
                    .bg(gpui::black().opacity(0.5))
                    .child(status)
            }))
    }
}

impl TerminalPane {
    /// A thin scrollbar over the right edge, when there's scrollback to scroll through.
    fn render_scrollbar(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let track = self.track_height?;
        let history = self.term.grid().history_size();
        if history == 0 || self.term.mode().contains(TermMode::ALT_SCREEN) {
            return None;
        }
        let total = (history + self.term.screen_lines()) as f32;
        let height = (f32::from(track) * self.term.screen_lines() as f32 / total).max(24.);
        let top = self.thumb_top(track).min(f32::from(track) - height);
        let active = self.scroll_drag.is_some() || self.term.grid().display_offset() != 0;
        let group = SharedString::from(format!("scrollbar-{}", self.block_id));
        Some(
            div()
                .id(SharedString::from(format!("scrollbar-track-{}", self.block_id)))
                .group(group.clone())
                .absolute()
                .top_0()
                .right_0()
                .w(px(SCROLLBAR_WIDTH))
                .h(track)
                .cursor_default()
                .on_mouse_down(MouseButton::Left, cx.listener(Self::on_scrollbar_down))
                .child(
                    div()
                        .absolute()
                        .top(px(top))
                        .right(px(2.))
                        .w(px(6.))
                        .h(px(height))
                        .rounded_full()
                        .bg(Hsla::from(gpui::rgb(ui().text_dim)).opacity(if active { 0.55 } else { 0.3 }))
                        .group_hover(group, |d| d.bg(Hsla::from(gpui::rgb(ui().text_dim)).opacity(0.7))),
                ),
        )
    }

    fn render_paste_prompt(&self, text: &str, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let lines = text.trim_end_matches(['\n', '\r']).lines().count();
        let button = |id: &'static str, label: &'static str, primary: bool| {
            div()
                .id(id)
                .px_3()
                .py_1()
                .rounded(px(6.))
                .text_xs()
                .cursor_pointer()
                .map(|d| {
                    if primary {
                        d.bg(gpui::rgb(ui().accent)).text_color(gpui::white()).hover(|d| d.opacity(0.9))
                    } else {
                        d.bg(gpui::rgb(ui().tab_active)).hover(|d| d.bg(gpui::rgb(ui().tab_active_border)))
                    }
                })
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(label)
        };
        div()
            .absolute()
            .bottom(px(12.))
            .left(px(12.))
            .right(px(12.))
            .p_3()
            .flex()
            .flex_col()
            .gap_2()
            .rounded(px(10.))
            .border_1()
            .border_color(gpui::rgb(ui().tab_active_border))
            .bg(gpui::rgb(ui().menu))
            .shadow_lg()
            .text_sm()
            .text_color(gpui::rgb(ui().text))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(format!("Paste {lines} lines?"))
            .child(
                div()
                    .text_xs()
                    .text_color(gpui::rgb(ui().text_dim))
                    .child("The program here hasn't turned on bracketed paste, so a shell runs each line as a command as soon as it arrives."),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .justify_end()
                    .child(button("paste-cancel", "Cancel  Esc", false).on_click(cx.listener(|this, _, _, cx| {
                        this.pending_paste = None;
                        cx.notify();
                    })))
                    .child(button("paste-join", "Paste as one line", false).on_click(cx.listener(|this, _, _, cx| this.confirm_paste(true, cx))))
                    .child(button("paste-all", "Paste  Enter", true).on_click(cx.listener(|this, _, _, cx| this.confirm_paste(false, cx)))),
            )
    }
}

/// One cell's look, resolved to colors, so painting needs no access to the terminal.
#[derive(Clone, Copy, PartialEq)]
struct CellStyle {
    fg: Hsla,
    bg: Option<Hsla>,
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
}

struct Segment {
    row: usize,
    col: usize,
    /// How many columns each character advances. 2 for a wide character.
    width: usize,
    text: String,
    style: CellStyle,
}

struct Frame {
    background: Hsla,
    segments: Vec<Segment>,
    backgrounds: Vec<(usize, Range<usize>, Hsla)>,
    cursor: Option<(usize, usize, CursorShape, Hsla, bool)>,
}

impl Frame {
    fn capture(term: &Term<Listener>, theme: &Theme, focused: bool) -> Self {
        let content = term.renderable_content();
        let colors = content.colors;
        let default_bg = theme.resolve(Color::Named(NamedColor::Background), colors);
        let offset = content.display_offset as i32;
        let selection = content.selection;
        let selection_bg = to_hsla(theme.selection);
        let selection_fg = theme.selection_fg.map(to_hsla);
        let mut segments: Vec<Segment> = Vec::new();
        let mut backgrounds: Vec<(usize, Range<usize>, Hsla)> = Vec::new();

        for indexed in content.display_iter {
            let cell = indexed.cell;
            let row = (indexed.point.line.0 + offset) as usize;
            let col = indexed.point.column.0;
            if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                continue;
            }
            let mut fg = theme.resolve(cell.fg, colors);
            let mut bg = theme.resolve(cell.bg, colors);
            if cell.flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if cell.flags.contains(Flags::DIM) {
                fg = fg.opacity(0.66);
            }
            if selection.is_some_and(|s| s.contains(indexed.point)) {
                bg = selection_bg;
                if let Some(selection_fg) = selection_fg {
                    fg = selection_fg;
                }
            }
            let wide = cell.flags.contains(Flags::WIDE_CHAR);
            let width = if wide { 2 } else { 1 };
            let bg = (bg != default_bg).then_some(bg);
            if let Some(bg) = bg {
                match backgrounds.last_mut() {
                    Some((r, cols, color)) if *r == row && cols.end == col && *color == bg => {
                        cols.end = col + width
                    }
                    _ => backgrounds.push((row, col..col + width, bg)),
                }
            }
            if cell.c == ' ' || cell.c == '\0' || cell.flags.contains(Flags::HIDDEN) {
                continue;
            }
            let style = CellStyle {
                fg,
                bg: None,
                bold: cell.flags.contains(Flags::BOLD),
                italic: cell.flags.contains(Flags::ITALIC),
                underline: cell.flags.intersects(Flags::ALL_UNDERLINES),
                strike: cell.flags.contains(Flags::STRIKEOUT),
            };
            let mut text = String::new();
            text.push(cell.c);
            if let Some(zerowidth) = cell.zerowidth() {
                text.extend(zerowidth);
            }
            match segments.last_mut() {
                Some(seg)
                    if seg.row == row
                        && seg.width == width
                        && seg.style == style
                        && seg.col + seg.text.chars().count() * width == col
                        && cell.zerowidth().is_none() =>
                {
                    seg.text.push(cell.c)
                }
                _ => segments.push(Segment { row, col, width, text, style }),
            }
        }

        let cursor = (content.cursor.shape != CursorShape::Hidden && offset == 0).then(|| {
            let color = theme.cursor(colors);
            let p: GridPoint = content.cursor.point;
            (p.line.0.max(0) as usize, p.column.0, content.cursor.shape, color, focused)
        });

        Self { background: default_bg, segments, backgrounds, cursor }
    }

    fn paint(
        self,
        bounds: Bounds<Pixels>,
        cell: CellMetrics,
        font: Font,
        font_size: Pixels,
        window: &mut Window,
        cx: &mut App,
    ) {
        let origin = bounds.origin;
        let at = |row: usize, col: usize| {
            point(origin.x + cell.width * col as f32, origin.y + cell.height * row as f32)
        };

        for (row, cols, color) in &self.backgrounds {
            let size = size(cell.width * cols.len() as f32, cell.height);
            window.paint_quad(fill(Bounds::new(at(*row, cols.start), size), *color));
        }

        if let Some((row, col, shape, color, focused)) = self.cursor {
            let origin = at(row, col);
            let quad = match (shape, focused) {
                (CursorShape::Beam, _) => fill(Bounds::new(origin, size(px(2.), cell.height)), color),
                (CursorShape::Underline, _) => fill(
                    Bounds::new(point(origin.x, origin.y + cell.height - px(2.)), size(cell.width, px(2.))),
                    color,
                ),
                (_, true) => fill(Bounds::new(origin, size(cell.width, cell.height)), color.opacity(0.7)),
                (_, false) => outline(Bounds::new(origin, size(cell.width, cell.height)), color, gpui::BorderStyle::Solid),
            };
            window.paint_quad(quad);
        }

        let text_system = window.text_system().clone();
        for seg in self.segments {
            let mut font = font.clone();
            if seg.style.bold {
                font.weight = FontWeight::BOLD;
            }
            if seg.style.italic {
                font.style = gpui::FontStyle::Italic;
            }
            let run = TextRun {
                len: seg.text.len(),
                font,
                color: seg.style.fg,
                background_color: None,
                underline: seg.style.underline.then(|| gpui::UnderlineStyle {
                    thickness: px(1.),
                    color: Some(seg.style.fg),
                    wavy: false,
                }),
                strikethrough: seg.style.strike.then(|| gpui::StrikethroughStyle {
                    thickness: px(1.),
                    color: Some(seg.style.fg),
                }),
            };
            let line = text_system.shape_line(
                seg.text.into(),
                font_size,
                &[run],
                Some(cell.width * seg.width as f32),
            );
            let _ = line.paint(at(seg.row, seg.col), cell.height, window, cx);
        }
    }
}

/// Default colors, from the server's `theme` messages or built in.
#[derive(Clone)]
struct Theme {
    foreground: Rgb,
    background: Rgb,
    cursor: Option<Rgb>,
    selection: Rgb,
    selection_fg: Option<Rgb>,
    palette: [Rgb; 256],
}

impl Default for Theme {
    /// The app theme's colors.
    fn default() -> Self {
        let theme = crate::theme::current();
        let colors = &theme.term;
        Self {
            foreground: rgb(colors.foreground),
            background: rgb(colors.background),
            cursor: Some(rgb(colors.cursor)),
            selection: rgb(colors.selection),
            selection_fg: colors.selection_fg.map(rgb),
            palette: default_palette(&colors.ansi),
        }
    }
}

impl Theme {
    fn from_message(msg: &Value) -> Self {
        let mut theme = Self::default();
        let parse = |v: &Value| v.as_str().and_then(parse_hex);
        if let Some(c) = parse(&msg["foreground"]) {
            theme.foreground = c;
        }
        if let Some(c) = parse(&msg["background"]) {
            theme.background = c;
        }
        if let Some(c) = parse(&msg["cursor"]) {
            theme.cursor = Some(c);
        }
        for entry in msg["palette"].as_array().into_iter().flatten() {
            let Some((index, color)) = entry.as_str().and_then(|e| e.split_once('=')) else { continue };
            if let (Ok(index), Some(color)) = (index.parse::<usize>(), parse_hex(color)) {
                if index < 256 {
                    theme.palette[index] = color;
                }
            }
        }
        theme
    }

    fn resolve(&self, color: Color, colors: &Colors) -> Hsla {
        let rgb = match color {
            Color::Spec(rgb) => rgb,
            Color::Indexed(i) => colors[i as usize].unwrap_or(self.palette[i as usize]),
            Color::Named(name) => colors[name].unwrap_or_else(|| self.named(name)),
        };
        to_hsla(rgb)
    }

    fn named(&self, name: NamedColor) -> Rgb {
        match name {
            NamedColor::Foreground | NamedColor::BrightForeground => self.foreground,
            NamedColor::DimForeground => self.foreground,
            NamedColor::Background => self.background,
            NamedColor::Cursor => self.cursor.unwrap_or(self.foreground),
            other => {
                let index = other as usize;
                if index < 16 {
                    self.palette[index]
                } else {
                    // The dim colors follow the 8 normal ones in alacritty's numbering.
                    let base = self.palette[(index - NamedColor::DimBlack as usize) % 8];
                    Rgb { r: base.r * 2 / 3, g: base.g * 2 / 3, b: base.b * 2 / 3 }
                }
            }
        }
    }

    fn cursor(&self, colors: &Colors) -> Hsla {
        to_hsla(colors[NamedColor::Cursor].unwrap_or(self.cursor.unwrap_or(self.foreground)))
    }
}

fn rgb(hex: u32) -> Rgb {
    Rgb { r: (hex >> 16) as u8, g: (hex >> 8) as u8, b: hex as u8 }
}

fn parse_hex(s: &str) -> Option<Rgb> {
    let s = s.strip_prefix('#').unwrap_or(s);
    (s.len() == 6).then(|| u32::from_str_radix(s, 16).ok().map(rgb)).flatten()
}

fn to_hsla(c: Rgb) -> Hsla {
    gpui::Rgba { r: c.r as f32 / 255., g: c.g as f32 / 255., b: c.b as f32 / 255., a: 1. }.into()
}

/// The 256-color palette: the theme's 16 colors, then the standard cube and gray ramp.
fn default_palette(ansi: &[u32; 16]) -> [Rgb; 256] {
    let mut palette = [Rgb::default(); 256];
    for (i, &hex) in ansi.iter().enumerate() {
        palette[i] = rgb(hex);
    }
    let level = |v: usize| if v == 0 { 0 } else { (55 + v * 40) as u8 };
    for i in 0..216 {
        palette[16 + i] = Rgb { r: level(i / 36), g: level(i / 6 % 6), b: level(i % 6) };
    }
    for i in 0..24 {
        let v = (8 + i * 10) as u8;
        palette[232 + i] = Rgb { r: v, g: v, b: v };
    }
    palette
}
