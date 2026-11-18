//! One Rex terminal block: a local libghostty-vt copy of the server's terminal, kept by the
//! mirror task, and the GPUI view that paints it and sends keys back.
//!
//! The server is the real terminal. The local copy only mirrors its output for drawing, so
//! replies a program asks for, such as cursor reports, are left to the server.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

use std::sync::atomic::{AtomicU32, Ordering};

use crate::ghostty::{
    self, CursorShape, RenderState, Rgb, Scroll, Search, Span, ViewportPoint, Width,
};
use gpui::{
    App, Bounds, ClipboardItem, Context, Entity, FocusHandle, Focusable, Font, FontWeight, Hsla,
    KeyDownEvent, Modifiers, ModifiersChangedEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, ScrollDelta, ScrollWheelEvent, SharedString, Subscription, Task,
    TextRun, Window, canvas, div, fill, font, outline, point, prelude::*, px, size,
};
use serde_json::{Value, json};

use crate::keymap::{Copy, Find, Paste, ScrollPageDown, ScrollPageUp};
use crate::keys;
use crate::mirror::{self, Mirror, MirrorEvent, SharedTerm};
use crate::rex::control::Control;
use crate::text_field::{TextField, TextFieldEvent};
use crate::theme::ui;
/// Width kept free at the right of the grid for the scrollbar.
const SCROLLBAR_WIDTH: f32 = 10.;

pub struct TerminalPane {
    control: Control,
    pub session_id: String,
    pub block_id: String,
    /// The local copy of the terminal, which the mirror task fills in.
    term: SharedTerm,
    /// What the copy is doing when it isn't live, such as "Connecting…".
    status: Option<String>,
    /// The terminal's colors from the server, which override the app theme's.
    server_theme: Option<Value>,
    /// Goes up with each new `server_theme`, to know when `theme_cache` is stale.
    server_theme_version: u64,
    theme_cache: RefCell<Option<(String, u64, Rc<Theme>)>>,
    /// Scroll-wheel movement too small to make a whole line yet.
    scroll_remainder: f32,
    cell: Option<CellMetrics>,
    /// Where the grid was last painted, to turn mouse positions into cells.
    origin: Option<Point<Pixels>>,
    /// Where a left-button drag that selects text started, while it lasts.
    select_anchor: Option<ViewportPoint>,
    /// The cell of the last mouse event sent to the program, to report moves between cells.
    last_mouse_cell: Option<(usize, usize)>,
    /// What the terminal showed at the last frame, read without holding its lock.
    render_state: RenderState,
    /// The colors last given to the terminal, and the terminal they were given to, so they
    /// are set again when either changes.
    applied_theme: Option<(u64, String)>,
    /// Where the grid was last laid out, to tell whether the pointer is over it.
    grid_bounds: Option<Bounds<Pixels>>,
    /// A Ctrl+click opened a link, so its release isn't for the program either.
    swallow_release: bool,
    /// The grid's height when last painted, which is also the scrollbar's track.
    track_height: Option<Pixels>,
    /// The scrollbar thumb is being dragged, held this far below its top.
    scroll_drag: Option<Pixels>,
    /// A multi-line paste waiting for the user to confirm it. See `paste`.
    pending_paste: Option<String>,
    /// The search bar, while it's open.
    search: Option<SearchBar>,
    /// What was last searched for, to offer again when the search bar opens.
    last_needle: String,
    /// The link under the pointer while Ctrl is held, which a click opens.
    hover_link: Option<Link>,
    /// The size we last asked the server for.
    requested: Option<(u16, u16)>,
    exit_code: Option<i64>,
    /// What the header and tab show: a title a program set, else the program and directory.
    pub info: PaneInfo,
    focus_handle: FocusHandle,
    _mirror: Mirror,
    _events: Task<()>,
}

#[derive(Default, Clone)]
pub struct PaneInfo {
    pub title: String,
    pub process: String,
    pub cwd: String,
    pub home: String,
}

impl PaneInfo {
    /// The directory with the home directory written as `~`, only on a path boundary, so
    /// `/home/amit` isn't `~it` for a home of `/home/am`.
    fn short_cwd(&self) -> String {
        let home = self.home.trim_end_matches('/');
        match self.cwd.strip_prefix(home) {
            Some(rest) if !home.is_empty() && (rest.is_empty() || rest.starts_with('/')) => {
                format!("~{rest}")
            }
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

struct SearchBar {
    field: Entity<TextField>,
    /// The needle the search has, to notice when the field's text changes.
    needle: String,
    /// Matches in the screen and scrollback, and which one is selected, from the last frame.
    total: usize,
    selected: Option<usize>,
    _subscriptions: [Subscription; 2],
}

/// A link on screen: one row's cells, and where they lead.
#[derive(Clone, Debug, PartialEq)]
struct Link {
    row: usize,
    cols: Range<usize>,
    url: String,
}

#[derive(Clone, Copy)]
struct CellMetrics {
    width: Pixels,
    height: Pixels,
}

impl TerminalPane {
    pub fn new(
        control: Control,
        session_id: String,
        block_id: String,
        cx: &mut Context<Self>,
    ) -> Self {
        let mirror = mirror::start(control.clone(), session_id.clone(), block_id.clone());
        let events = cx.spawn({
            let control = control.clone();
            let (session_id, block_id) = (session_id.clone(), block_id.clone());
            let (rx, dirty) = (mirror.events.clone(), mirror.dirty.clone());
            async move |this, cx| {
                let (title, process) = futures_util::join!(
                    control.block_call(
                        "com.superlogical.terminal.title",
                        &session_id,
                        &block_id,
                        json!({})
                    ),
                    control.block_call(
                        "com.superlogical.terminal.process",
                        &session_id,
                        &block_id,
                        json!({})
                    ),
                );
                let _ = this.update(cx, |this, cx| {
                    if let Ok(title) = title {
                        this.info.title = title["title"].as_str().unwrap_or_default().to_string();
                    }
                    if let Ok(process) = process {
                        this.info.apply_process(&process);
                    }
                    cx.emit(InfoChanged);
                    cx.notify();
                });
                while let Ok(event) = rx.recv().await {
                    let updated = this.update(cx, |this, cx| {
                        match event {
                            // Take the redraw before drawing, so a change made while drawing
                            // asks for another one.
                            MirrorEvent::Redraw => {
                                dirty.store(false, std::sync::atomic::Ordering::Release)
                            }
                            MirrorEvent::Status(status) => this.status = status,
                            MirrorEvent::Theme(theme) => {
                                this.server_theme = Some(theme);
                                this.server_theme_version += 1;
                            }
                            MirrorEvent::Exited(code) => this.exit_code = Some(code),
                        }
                        cx.notify();
                    });
                    if updated.is_err() {
                        break;
                    }
                }
            }
        });
        Self {
            control,
            session_id,
            block_id,
            term: mirror.term.clone(),
            status: Some("Connecting…".into()),
            server_theme: None,
            server_theme_version: 0,
            theme_cache: RefCell::new(None),
            scroll_remainder: 0.,
            cell: None,
            origin: None,
            select_anchor: None,
            last_mouse_cell: None,
            render_state: RenderState::new().expect("creating a render state"),
            applied_theme: None,
            grid_bounds: None,
            swallow_release: false,
            track_height: None,
            scroll_drag: None,
            pending_paste: None,
            search: None,
            last_needle: String::new(),
            hover_link: None,
            requested: None,
            exit_code: None,
            info: PaneInfo::default(),
            focus_handle: cx.focus_handle(),
            _mirror: mirror,
            _events: events,
        }
    }

    /// Lock the local copy of the terminal. Lock once per statement: the guard lives to the
    /// end of the statement, and a second lock in it would wait forever.
    fn term(&self) -> std::sync::MutexGuard<'_, mirror::Shared> {
        self.term.lock().unwrap()
    }

    /// A `block_event` from the server for this block, such as a new title.
    pub fn on_block_event(&mut self, name: &str, payload: &Value, cx: &mut Context<Self>) {
        match name {
            "title_changed" => {
                self.info.title = payload["title"].as_str().unwrap_or_default().into()
            }
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
            // A program copied text (OSC 52), such as Claude Code after a selection made with
            // the mouse, or the server's own selection ended. The server can't reach this
            // machine's clipboard, so put the text there.
            "clipboard_written" => {
                self.copy_from_server("com.superlogical.terminal.clipboard_read", payload, cx)
            }
            "selection_copied" => {
                self.copy_from_server("com.superlogical.terminal.blob_read", payload, cx)
            }
            _ => return,
        }
        cx.emit(InfoChanged);
        cx.notify();
    }

    /// Put text the server reports as copied on the clipboard. Large text isn't in the event;
    /// it's read in parts with `read_method` instead.
    fn copy_from_server(&self, read_method: &'static str, payload: &Value, cx: &mut Context<Self>) {
        if let Some(text) = payload["text"].as_str() {
            log::debug!(
                "copying {} bytes from the terminal to the clipboard",
                text.len()
            );
            cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
            return;
        }
        let (Some(id), true) = (
            payload["id"].as_u64(),
            payload["bytes"].as_u64().unwrap_or(0) > 0,
        ) else {
            return;
        };
        let control = self.control.clone();
        let (session_id, block_id) = (self.session_id.clone(), self.block_id.clone());
        cx.spawn(async move |_, cx| {
            let mut text = String::new();
            loop {
                let args = json!({"id": id, "offset": text.len(), "max_bytes": 900_000});
                let part = match control
                    .block_call(read_method, &session_id, &block_id, args)
                    .await
                {
                    Ok(part) => part,
                    Err(err) => {
                        log::warn!("reading copied text: {err:#}");
                        return;
                    }
                };
                let data = part["data"].as_str().unwrap_or_default();
                text.push_str(data);
                if part["eof"].as_bool().unwrap_or(true) || data.is_empty() {
                    break;
                }
            }
            let _ = cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_string(text)));
        })
        .detach();
    }

    /// The terminal's colors, built once per app theme and server theme rather than on
    /// every frame.
    fn theme(&self) -> Rc<Theme> {
        let app_theme = crate::theme::current();
        let mut cache = self.theme_cache.borrow_mut();
        if let Some((name, version, theme)) = cache.as_ref()
            && *name == app_theme.name
            && *version == self.server_theme_version
        {
            return theme.clone();
        }
        let theme = Rc::new(match &self.server_theme {
            Some(msg) => Theme::from_message(msg),
            None => Theme::default(),
        });
        *cache = Some((
            app_theme.name.to_string(),
            self.server_theme_version,
            theme.clone(),
        ));
        theme
    }

    pub fn background(&self) -> Hsla {
        to_hsla(self.theme().background)
    }

    /// Paste `text`. A program that turned on bracketed paste gets it as one block. Without
    /// it, a shell runs each line as it arrives, so a multi-line paste waits for a yes first.
    fn paste(&mut self, text: String, cx: &mut Context<Self>) {
        self.term().term.clear_selection();
        self.select_anchor = None;
        let lines = text.trim_end_matches(['\n', '\r']).lines().count();
        if lines > 1 && !self.term().term.dec_mode(BRACKETED_PASTE) {
            self.pending_paste = Some(text);
            cx.notify();
            return;
        }
        self.send_event("paste", json!({"text": text}));
    }

    fn confirm_paste(&mut self, as_one_line: bool, cx: &mut Context<Self>) {
        if let Some(text) = self.pending_paste.take() {
            let text = if as_one_line {
                text.lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ")
            } else {
                text
            };
            self.send_event("paste", json!({"text": text}));
        }
        cx.notify();
    }

    /// Whether the terminal itself has the keyboard, not the search bar inside it.
    fn has_keyboard(&self, window: &Window) -> bool {
        self.focus_handle.is_focused(window)
    }

    fn copy(&mut self, _: &Copy, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.term().term.selection_text().unwrap_or_default();
        // With nothing selected, the keys go to the program, so a binding such as ctrl-c can
        // copy a selection and still interrupt.
        if text.is_empty() || !self.has_keyboard(window) {
            cx.propagate();
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    fn paste_clipboard(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        // In the search bar, the keys paste into the bar.
        if !self.has_keyboard(window) {
            cx.propagate();
            return;
        }
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.paste(text, cx);
        }
    }

    fn scroll_page(&mut self, up: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.has_keyboard(window) {
            cx.propagate();
            return;
        }
        let rows = self.term().term.rows() as isize;
        self.term()
            .term
            .scroll(Scroll::Delta(if up { -rows } else { rows }));
        cx.notify();
    }

    /// Open the search bar, or give it the keyboard back.
    fn find(&mut self, _: &Find, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(bar) = &self.search {
            window.focus(&bar.field.focus_handle(cx));
            return;
        }
        let needle = self.last_needle.clone();
        let field = cx.new(|cx| TextField::new("Find", cx).with_text(needle));
        let subscriptions = [
            cx.observe(&field, |this, field, cx| {
                let text = field.read(cx).text.clone();
                this.search_for(text, cx);
            }),
            cx.subscribe_in(&field, window, |this, _, event, window, cx| match event {
                TextFieldEvent::Submit => this.select_match(true, cx),
                TextFieldEvent::Cancel => this.close_search(window, cx),
            }),
        ];
        window.focus(&field.focus_handle(cx));
        let text = field.read(cx).text.clone();
        self.search = Some(SearchBar {
            field,
            needle: String::new(),
            total: 0,
            selected: None,
            _subscriptions: subscriptions,
        });
        self.search_for(text, cx);
    }

    /// Search for `needle`, from the newest output back, when it isn't already the needle.
    fn search_for(&mut self, needle: String, cx: &mut Context<Self>) {
        let Some(bar) = &mut self.search else { return };
        if bar.needle == needle {
            return;
        }
        bar.needle = needle.clone();
        self.last_needle = needle.clone();
        {
            let mut shared = self.term();
            let mirror::Shared {
                term,
                generation,
                search,
            } = &mut *shared;
            if let Some(search) = live_search(search, term, *generation, &needle) {
                search.set_needle(term, &needle);
                search.run(term);
                // Show the newest match, as a search from the prompt wants.
                if search.total() > 0 {
                    search.select(term, true);
                }
            }
        }
        cx.notify();
    }

    /// Move to the next match, toward older output when `older`.
    fn select_match(&mut self, older: bool, cx: &mut Context<Self>) {
        let Some(bar) = &self.search else { return };
        let needle = bar.needle.clone();
        {
            let mut shared = self.term();
            let mirror::Shared {
                term,
                generation,
                search,
            } = &mut *shared;
            if let Some(search) = live_search(search, term, *generation, &needle) {
                search.select(term, older);
            }
        }
        cx.notify();
    }

    fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search = None;
        self.term().search = None;
        window.focus(&self.focus_handle);
        cx.notify();
    }

    /// Keys for the search bar that its text field leaves alone: Shift+Enter and the arrows
    /// move between matches.
    fn on_search_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(bar) = &self.search else { return };
        if !bar.field.focus_handle(cx).is_focused(window) {
            return;
        }
        let ks = &event.keystroke;
        let older = match ks.key.as_str() {
            "enter" if ks.modifiers.shift => false,
            "up" => true,
            "down" => false,
            _ => return,
        };
        cx.stop_propagation();
        self.select_match(older, cx);
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // Keys the search bar didn't use aren't for the program.
        if !self.has_keyboard(window) {
            return;
        }
        let ks = &event.keystroke;
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
        // Copy, paste, and the client's other keys are actions, bound in `keymap`.
        let Some(key) = keys::client_key(ks, event.is_held) else {
            return;
        };
        cx.stop_propagation();
        {
            let mut shared = self.term();
            // Typing clears a selection and returns the view to the bottom, as in Ghostty.
            if shared.term.has_selection() || !shared.term.at_bottom() {
                shared.term.clear_selection();
                shared.term.scroll(Scroll::Bottom);
                cx.notify();
            }
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

    fn on_scroll(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(cell) = self.cell else { return };
        let to_program = {
            let shared = self.term();
            shared.term.alternate_screen() || shared.term.mouse_tracking()
        };
        // Full-screen programs and programs that read the mouse get real wheel events. The
        // server's terminal turns them into arrow keys for programs that want that.
        if to_program && !event.modifiers.shift {
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
            // Wheel up is positive here; scrolling back is negative there.
            self.term().term.scroll(Scroll::Delta(-whole as isize));
            cx.notify();
        }
    }

    /// The cell under a window position, as `(column, row)` on screen, clamped to the grid.
    fn cell_at(&self, position: Point<Pixels>) -> Option<(usize, usize)> {
        let (origin, cell) = (self.origin?, self.cell?);
        let col = ((position.x - origin.x) / cell.width).floor().max(0.0) as usize;
        let row = ((position.y - origin.y) / cell.height).floor().max(0.0) as usize;
        let shared = self.term();
        let (cols, rows) = (shared.term.cols() as usize, shared.term.rows() as usize);
        Some((
            col.min(cols.saturating_sub(1)),
            row.min(rows.saturating_sub(1)),
        ))
    }

    fn viewport_point(&self, position: Point<Pixels>) -> Option<ViewportPoint> {
        let (x, y) = self.cell_at(position)?;
        Some(ViewportPoint {
            x: x as u16,
            y: y as u16,
        })
    }

    /// Whether mouse events belong to the program. Shift keeps them local, for selecting.
    fn program_wants_mouse(&self, modifiers: &Modifiers) -> bool {
        self.term().term.mouse_tracking() && !modifiers.shift
    }

    fn send_mouse(
        &self,
        action: &str,
        button: Option<MouseButton>,
        held: Option<MouseButton>,
        position: Point<Pixels>,
        modifiers: &Modifiers,
    ) {
        let Some((x, y)) = self.cell_at(position) else {
            return;
        };
        let name = |b: MouseButton| match b {
            MouseButton::Left => "left",
            MouseButton::Right => "right",
            MouseButton::Middle => "middle",
            _ => "unknown",
        };
        let mut payload =
            json!({"action": action, "x": x, "y": y, "mods": keys::mods_string(modifiers)});
        if let Some(button) = button {
            payload["button"] = json!(name(button));
        }
        if let Some(held) = held {
            payload["buttons"] = json!({name(held): true});
        }
        self.send_event("mouse", payload);
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle);
        self.swallow_release = false;
        if event.button == MouseButton::Left
            && event.modifiers.control
            && !event.modifiers.shift
            && let Some(link) = self.link_at(event.position)
        {
            cx.open_url(&link.url);
            self.swallow_release = true;
            return;
        }
        if self.program_wants_mouse(&event.modifiers) {
            self.last_mouse_cell = self.cell_at(event.position);
            self.send_mouse(
                "press",
                Some(event.button),
                Some(event.button),
                event.position,
                &event.modifiers,
            );
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
        let Some(point) = self.viewport_point(event.position) else {
            return;
        };
        match event.click_count {
            1 => {
                // A plain click clears the selection; dragging from here selects.
                self.term().term.clear_selection();
                self.select_anchor = Some(point);
            }
            2 => {
                self.term().term.select_word(point);
                self.finish_selection(cx);
            }
            _ => {
                self.term().term.select_line(point);
                self.finish_selection(cx);
            }
        }
        cx.notify();
    }

    /// Scroll so the thumb's top follows `y`, measured from the top of the track.
    fn scroll_to_thumb(&mut self, top: f32, cx: &mut Context<Self>) {
        let Some(track) = self.track_height else {
            return;
        };
        let mut shared = self.term();
        let bar = shared.term.scrollbar();
        // The thumb's top marks the first visible row.
        let max = bar.total.saturating_sub(bar.len) as f32;
        let row = (top / f32::from(track) * bar.total as f32)
            .clamp(0.0, max)
            .round() as u64;
        if row != bar.offset {
            shared.term.scroll(Scroll::Row(row as usize));
            cx.notify();
        }
    }

    fn on_scrollbar_down(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        let (Some(origin), Some(track)) = (self.origin, self.track_height) else {
            return;
        };
        let (thumb_top, thumb_height) = self.thumb(track);
        let y = f32::from(event.position.y - origin.y);
        // Grabbing the thumb keeps its offset under the pointer; clicking the track centers it.
        let grab = if (thumb_top..thumb_top + thumb_height).contains(&y) {
            y - thumb_top
        } else {
            thumb_height / 2.
        };
        self.scroll_drag = Some(px(grab));
        self.scroll_to_thumb(y - grab, cx);
    }

    /// The scrollbar thumb's top and height on a track this tall.
    fn thumb(&self, track: Pixels) -> (f32, f32) {
        let bar = self.term().term.scrollbar();
        let total = bar.total.max(1) as f32;
        let track = f32::from(track);
        (
            track * bar.offset as f32 / total,
            track * bar.len as f32 / total,
        )
    }

    /// End a drag-selection, keeping what it selected as the primary selection.
    fn finish_selection(&mut self, cx: &mut Context<Self>) {
        self.select_anchor = None;
        // Like other X11 and Wayland terminals, a selection becomes the primary selection.
        let text = self.term().term.selection_text().filter(|t| !t.is_empty());
        if let Some(text) = text {
            cx.write_to_primary(ClipboardItem::new_string(text));
        }
        cx.notify();
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A pane border is being dragged across this pane: the moves aren't for its program.
        if crate::workspace::dragging_border() {
            return;
        }
        let ctrl = event.modifiers.control && !event.modifiers.shift;
        self.update_hover_link(event.position, ctrl && event.pressed_button.is_none(), cx);
        if let (Some(grab), Some(origin)) = (self.scroll_drag, self.origin) {
            if event.pressed_button == Some(MouseButton::Left) {
                self.scroll_to_thumb(f32::from(event.position.y - origin.y - grab), cx);
            } else {
                self.scroll_drag = None;
            }
            return;
        }
        if let Some(anchor) = self.select_anchor {
            // The button came up somewhere this pane didn't hear about, such as over a menu.
            if event.pressed_button != Some(MouseButton::Left) {
                self.finish_selection(cx);
                return;
            }
            if let Some(point) = self.viewport_point(event.position)
                && (point != anchor || self.term().term.has_selection())
            {
                self.term().term.select(anchor, point);
                cx.notify();
            }
            return;
        }
        // Report drags; plain motion only matters to programs in any-motion mode.
        let wants_motion =
            event.pressed_button.is_some() || self.term().term.dec_mode(ANY_MOTION_MOUSE);
        if wants_motion && self.program_wants_mouse(&event.modifiers) {
            // Report a move only when it reaches another cell, as terminals do. Otherwise a
            // click with a slight wobble reaches the program as a drag, and it may treat it
            // as one: start a selection or scroll instead of acting on the click.
            let cell = self.cell_at(event.position);
            if cell == self.last_mouse_cell {
                return;
            }
            self.last_mouse_cell = cell;
            self.send_mouse(
                "motion",
                None,
                event.pressed_button,
                event.position,
                &event.modifiers,
            );
        }
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.swallow_release) {
            return;
        }
        if self.scroll_drag.take().is_some() {
            return;
        }
        if self.select_anchor.is_some() {
            self.finish_selection(cx);
            return;
        }
        if self.program_wants_mouse(&event.modifiers) {
            self.last_mouse_cell = self.cell_at(event.position);
            self.send_mouse(
                "release",
                Some(event.button),
                None,
                event.position,
                &event.modifiers,
            );
        }
    }

    /// Pressing or letting go of Ctrl shows or hides the link under the pointer.
    fn on_modifiers_changed(
        &mut self,
        event: &ModifiersChangedEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let position = window.mouse_position();
        let over = self.grid_bounds.is_some_and(|b| b.contains(&position));
        let ctrl = event.modifiers.control && !event.modifiers.shift;
        self.update_hover_link(position, ctrl && over, cx);
    }

    fn update_hover_link(&mut self, position: Point<Pixels>, active: bool, cx: &mut Context<Self>) {
        let link = if active { self.link_at(position) } else { None };
        if link != self.hover_link {
            self.hover_link = link;
            cx.notify();
        }
    }

    /// The link at a window position: a hyperlink a program made with OSC 8, else a URL in
    /// the text. Only web and mail links count, so output can't open other kinds of things.
    fn link_at(&self, position: Point<Pixels>) -> Option<Link> {
        let (col, row) = self.cell_at(position)?;
        {
            let shared = self.term();
            let at = |x: usize| {
                shared.term.hyperlink_at(ViewportPoint {
                    x: x as u16,
                    y: row as u16,
                })
            };
            if let Some(url) = at(col).filter(|url| openable(url)) {
                let cols = shared.term.cols() as usize;
                let same = |x: usize| at(x).as_deref() == Some(url.as_str());
                let mut start = col;
                while start > 0 && same(start - 1) {
                    start -= 1;
                }
                let mut end = col + 1;
                while end < cols && same(end) {
                    end += 1;
                }
                return Some(Link {
                    row,
                    cols: start..end,
                    url,
                });
            }
        }
        let (cols, url) = url_in_row(self.render_state.rows().get(row)?, col)?;
        Some(Link { row, cols, url })
    }

    /// Ask the server for the size that fills `bounds`, when it differs from the last request.
    fn request_size(&mut self, bounds: Bounds<Pixels>, cell: CellMetrics) {
        self.origin = Some(bounds.origin);
        self.grid_bounds = Some(bounds);
        self.track_height = Some(bounds.size.height);
        let width = bounds.size.width - px(SCROLLBAR_WIDTH);
        let cols = ((width / cell.width).floor() as u16).max(2);
        let rows = ((bounds.size.height / cell.height).floor() as u16).max(1);
        if self.requested == Some((cols, rows)) || self.status.is_some() {
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
        self.status.clone().or_else(|| {
            self.exit_code
                .map(|code| format!("Process exited with code {code}"))
        })
    }
}

/// The search of the terminal in `slot`, made again for a terminal that replaced the one it
/// searched, as after a reconnect.
fn live_search<'a>(
    slot: &'a mut Option<(u64, Search)>,
    term: &ghostty::Terminal,
    generation: u64,
    needle: &str,
) -> Option<&'a mut Search> {
    if slot.as_ref().is_none_or(|(g, _)| *g != generation) {
        *slot = None;
        match Search::new(term) {
            Ok(mut search) => {
                search.set_needle(term, needle);
                *slot = Some((generation, search));
            }
            Err(err) => log::warn!("{err:#}"),
        }
    }
    slot.as_mut().map(|(_, search)| search)
}

/// Whether a link is one a click may open: a web or mail address.
fn openable(url: &str) -> bool {
    let url = url.to_ascii_lowercase();
    ["http://", "https://", "ftp://", "mailto:"]
        .iter()
        .any(|scheme| url.starts_with(scheme) && url.len() > scheme.len())
}

/// The URL in a row of cells that covers column `col`, with the columns it spans.
fn url_in_row(cells: &[ghostty::Cell], col: usize) -> Option<(Range<usize>, String)> {
    // The row's characters, each with the column it's drawn in.
    let mut chars: Vec<(char, usize)> = Vec::new();
    for (c, cell) in cells.iter().enumerate() {
        match cell.width {
            Width::Spacer => {}
            _ if cell.text.is_empty() => chars.push((' ', c)),
            _ => chars.extend(cell.text.chars().map(|ch| (ch, c))),
        }
    }
    let lower: Vec<char> = chars
        .iter()
        .map(|(ch, _)| ch.to_ascii_lowercase())
        .collect();
    let starts_with = |i: usize, s: &str| {
        s.chars()
            .enumerate()
            .all(|(k, c)| lower.get(i + k) == Some(&c))
    };
    let url_char = |c: char| {
        c.is_ascii_graphic() && !matches!(c, '<' | '>' | '"' | '`' | '\\' | '^' | '{' | '}' | '|')
            || c.is_alphanumeric()
    };
    let mut i = 0;
    while i < chars.len() {
        let after_boundary = i == 0 || !chars[i - 1].0.is_alphanumeric();
        let scheme = ["https://", "http://", "ftp://", "mailto:"]
            .into_iter()
            .find(|s| after_boundary && starts_with(i, s));
        let Some(scheme) = scheme else {
            i += 1;
            continue;
        };
        let mut end = i;
        while end < chars.len() && url_char(chars[end].0) {
            end += 1;
        }
        // Punctuation that ends a sentence, and brackets the URL didn't open, aren't part
        // of it: `(see https://example.com/a).`
        while let Some(&(last, _)) = chars[i..end].last() {
            let text = || chars[i..end].iter().map(|(c, _)| *c);
            let unbalanced = |open: char, close: char| {
                last == close
                    && text().filter(|&c| c == open).count()
                        < text().filter(|&c| c == close).count()
            };
            if matches!(last, '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '*')
                || unbalanced('(', ')')
                || unbalanced('[', ']')
            {
                end -= 1;
            } else {
                break;
            }
        }
        if end > i + scheme.chars().count() {
            let span = chars[i].1..chars[end - 1].1 + 1;
            if span.contains(&col) {
                return Some((span, chars[i..end].iter().map(|(c, _)| *c).collect()));
            }
        }
        i = end.max(i + 1);
    }
    None
}

/// The mouse actions a terminal pane handles itself, for the shortcuts overlay. Keep this
/// beside the mouse handlers, which implement them. Its keys are in `keymap`.
pub const MOUSE_SHORTCUTS: &[(&str, &str)] = &[
    ("ctrl-click", "Open a link"),
    ("drag", "Select text"),
    ("double-click", "Select a word"),
    ("triple-click", "Select a line"),
    ("shift-drag", "Select, even when a program has the mouse"),
    ("middle-click", "Paste the primary selection"),
];

/// Search match colors: every match, and the selected one.
const MATCH: u32 = 0xe5c07b;
const SELECTED_MATCH: u32 = 0xff9e3b;

/// The cells of `span` on each row it covers, on a grid `cols` wide.
fn span_rows(span: Span, cols: usize) -> Vec<(usize, Range<usize>)> {
    let (first, last) = (span.start.y as usize, span.end.y as usize);
    (first..=last)
        .map(|row| {
            let start = if row == first {
                span.start.x as usize
            } else {
                0
            };
            let end = if row == last {
                span.end.x as usize + 1
            } else {
                cols
            };
            (row, start..end.max(start))
        })
        .collect()
}

/// DEC private modes the pane checks.
const BRACKETED_PASTE: u16 = 2004;
const ANY_MOTION_MOUSE: u16 = 1003;

/// The pane's title, program, or directory changed, which the tabs and sidebar show.
pub struct InfoChanged;

impl gpui::EventEmitter<InfoChanged> for TerminalPane {}

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
    std::env::var("REX_SCROLL_LINES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3.)
}

/// The terminal text size in pixels, as bits of an `f32`; 0 until it's first read.
static FONT_SIZE: AtomicU32 = AtomicU32::new(0);
const FONT_SIZES: std::ops::RangeInclusive<f32> = 6.0..=72.0;

/// The size to start from: `REX_FONT_SIZE`, else 14.
fn default_font_size() -> f32 {
    std::env::var("REX_FONT_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|size| FONT_SIZES.contains(size))
        .unwrap_or(14.0)
}

/// The terminal text size: the one last chosen with the font size keys, else the default.
pub fn font_size_f32() -> f32 {
    match FONT_SIZE.load(Ordering::Relaxed) {
        0 => {
            let size = crate::settings::get("font_size")
                .and_then(|v| v.as_f64())
                .map(|v| v as f32)
                .filter(|size| FONT_SIZES.contains(size))
                .unwrap_or_else(default_font_size);
            FONT_SIZE.store(size.to_bits(), Ordering::Relaxed);
            size
        }
        bits => f32::from_bits(bits),
    }
}

pub fn font_size() -> Pixels {
    px(font_size_f32())
}

/// Change the text size of every terminal, and keep it for the next run.
pub fn set_font_size(size: f32) {
    let size = size.round().clamp(*FONT_SIZES.start(), *FONT_SIZES.end());
    FONT_SIZE.store(size.to_bits(), Ordering::Relaxed);
    if let Err(err) = crate::settings::set("font_size", json!(size)) {
        log::warn!("saving the font size: {err:#}");
    }
}

pub fn reset_font_size() {
    FONT_SIZE.store(default_font_size().to_bits(), Ordering::Relaxed);
    if let Err(err) = crate::settings::set("font_size", Value::Null) {
        log::warn!("saving the font size: {err:#}");
    }
}

impl Render for TerminalPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::count_render("pane");
        let font = terminal_font();
        let font_size = font_size();
        let cell = {
            let text = window.text_system();
            let id = text.resolve_font(&font);
            let width = text
                .advance(id, font_size, 'm')
                .map(|s| s.width)
                .unwrap_or(font_size * 0.6);
            let height = (font_size * 1.25).round();
            CellMetrics { width, height }
        };
        self.cell = Some(cell);

        // Keyboard focus is in this pane, on the terminal or its search bar.
        let focused = self.focus_handle.contains_focused(window, cx);
        let theme = self.theme();
        let mut matches = (Vec::new(), None);
        {
            let term = self.term.clone();
            let mut shared = term.lock().unwrap();
            // A new terminal, or new colors, needs the default colors set again.
            let key = (shared.generation, theme.key.clone());
            if self.applied_theme.as_ref() != Some(&key) {
                shared.term.set_default_colors(
                    theme.foreground,
                    theme.background,
                    theme.cursor,
                    &theme.palette,
                );
                // Cached rows hold colors resolved with the old defaults.
                self.render_state.invalidate();
                self.applied_theme = Some(key);
            }
            // Copy what's on screen, then draw from the copy without the lock.
            self.render_state.update(&shared.term);
            // Catch the search up with new output and where the view is scrolled to.
            if let Some(bar) = &mut self.search {
                let mirror::Shared {
                    term,
                    generation,
                    search,
                } = &mut *shared;
                if let Some(search) = live_search(search, term, *generation, &bar.needle) {
                    search.run(term);
                    bar.total = search.total();
                    bar.selected = search.selected_index();
                    matches = search.visible_matches(term);
                }
            }
        }
        let mut frame = Frame::capture(&self.render_state.frame(), &theme, focused);
        let cols = self.render_state.rows().first().map_or(0, Vec::len);
        let (found, selected) = matches;
        for (span, color) in found
            .iter()
            .filter(|span| Some(**span) != selected)
            .map(|span| (span, Hsla::from(gpui::rgb(MATCH)).opacity(0.35)))
            .chain(
                selected
                    .iter()
                    .map(|span| (span, Hsla::from(gpui::rgb(SELECTED_MATCH)).opacity(0.7))),
            )
        {
            for (row, range) in span_rows(*span, cols) {
                frame.highlights.push((row, range, color));
            }
        }
        frame.link = self.hover_link.as_ref().map(|link| {
            (
                link.row,
                link.cols.clone(),
                Hsla::from(gpui::rgb(ui().accent)),
            )
        });
        let background = frame.background;
        let status = self.status();
        let entity = cx.entity();
        let scrollbar = self.render_scrollbar(cx);
        let paste_prompt = self
            .pending_paste
            .as_ref()
            .map(|text| self.render_paste_prompt(text, cx));
        let search_bar = self
            .search
            .as_ref()
            .map(|bar| self.render_search_bar(bar, cx));

        div()
            .id(SharedString::from(self.block_id.clone()))
            .track_focus(&self.focus_handle)
            .key_context("Terminal")
            .on_key_down(cx.listener(Self::on_key_down))
            .capture_key_down(cx.listener(Self::on_search_key))
            .on_modifiers_changed(cx.listener(Self::on_modifiers_changed))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::paste_clipboard))
            .on_action(cx.listener(Self::find))
            .on_action(
                cx.listener(|this, _: &ScrollPageUp, window, cx| {
                    this.scroll_page(true, window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &ScrollPageDown, window, cx| {
                this.scroll_page(false, window, cx)
            }))
            .when(self.hover_link.is_some(), |d| d.cursor_pointer())
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::on_mouse_up))
            // A drag that ends outside the pane still has to end.
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.scroll_drag = None;
                    if this.select_anchor.is_some() {
                        this.finish_selection(cx);
                    }
                }),
            )
            .size_full()
            .relative()
            .overflow_hidden()
            .bg(background)
            .child(
                canvas(
                    move |bounds, _window, cx| {
                        entity.update(cx, |this, _| this.request_size(bounds, cell));
                    },
                    move |bounds, (), window, cx| {
                        frame.paint(bounds, cell, font, font_size, window, cx)
                    },
                )
                .size_full(),
            )
            .children(scrollbar)
            .children(search_bar)
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
        let (has_history, alternate, at_bottom) = {
            let shared = self.term();
            let bar = shared.term.scrollbar();
            (
                bar.total > bar.len,
                shared.term.alternate_screen(),
                shared.term.at_bottom(),
            )
        };
        if !has_history || alternate {
            return None;
        }
        let (top, height) = self.thumb(track);
        let height = height.max(24.);
        let top = top.min(f32::from(track) - height);
        let active = self.scroll_drag.is_some() || !at_bottom;
        let group = SharedString::from(format!("scrollbar-{}", self.block_id));
        Some(
            div()
                .id(SharedString::from(format!(
                    "scrollbar-track-{}",
                    self.block_id
                )))
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
                        .bg(Hsla::from(gpui::rgb(ui().text_dim)).opacity(if active {
                            0.55
                        } else {
                            0.3
                        }))
                        .group_hover(group, |d| {
                            d.bg(Hsla::from(gpui::rgb(ui().text_dim)).opacity(0.7))
                        }),
                ),
        )
    }

    fn render_search_bar(
        &self,
        bar: &SearchBar,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let count = match (bar.needle.is_empty(), bar.total) {
            (true, _) => String::new(),
            (false, 0) => "No matches".into(),
            (false, total) => format!("{} of {total}", bar.selected.map_or(0, |i| i + 1)),
        };
        let button = |id: &'static str, path: &'static str| {
            div()
                .id(id)
                .size(px(26.))
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .rounded(px(7.))
                .cursor_pointer()
                .hover(|d| d.bg(gpui::rgb(ui().tab_active)))
                .child(
                    gpui::svg()
                        .path(path)
                        .size(px(15.))
                        .text_color(gpui::rgb(ui().text_dim)),
                )
        };
        div()
            .absolute()
            .top(px(8.))
            .right(px(SCROLLBAR_WIDTH + 8.))
            .w(px(360.))
            .max_w(gpui::relative(0.9))
            .p_1()
            .flex()
            .items_center()
            .gap_1()
            .rounded(px(10.))
            .border_1()
            .border_color(gpui::rgb(ui().tab_active_border))
            .bg(gpui::rgb(ui().menu))
            .shadow_lg()
            .text_color(gpui::rgb(ui().text))
            // Clicks on the bar aren't for the terminal under it.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_move(|_, _, cx| cx.stop_propagation())
            .child(div().flex_1().min_w(px(0.)).child(bar.field.clone()))
            .child(
                div()
                    .flex_none()
                    .px_1()
                    .text_xs()
                    .text_color(gpui::rgb(ui().text_dim))
                    .child(count),
            )
            .child(
                button("search-older", "icons/chevron-up.svg")
                    .on_click(cx.listener(|this, _, _, cx| this.select_match(true, cx))),
            )
            .child(
                button("search-newer", "icons/chevron-down.svg")
                    .on_click(cx.listener(|this, _, _, cx| this.select_match(false, cx))),
            )
            .child(
                button("search-close", "icons/close.svg")
                    .on_click(cx.listener(|this, _, window, cx| this.close_search(window, cx))),
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
                        d.bg(gpui::rgb(ui().accent))
                            .text_color(gpui::white())
                            .hover(|d| d.opacity(0.9))
                    } else {
                        d.bg(gpui::rgb(ui().tab_active))
                            .hover(|d| d.bg(gpui::rgb(ui().tab_active_border)))
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
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
}

struct Segment {
    /// Every cell in the segment is one character, so they can be drawn as one run.
    single: bool,
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
    /// Search matches to color behind the text.
    highlights: Vec<(usize, Range<usize>, Hsla)>,
    /// A link to underline, under the pointer while Ctrl is held.
    link: Option<(usize, Range<usize>, Hsla)>,
}

impl Frame {
    fn capture(frame: &ghostty::Frame, theme: &Theme, focused: bool) -> Self {
        let default_fg = to_hsla(frame.foreground);
        let default_bg = to_hsla(frame.background);
        let selection_bg = to_hsla(theme.selection);
        let selection_fg = theme.selection_fg.map(to_hsla);
        let mut segments: Vec<Segment> = Vec::new();
        let mut backgrounds: Vec<(usize, Range<usize>, Hsla)> = Vec::new();

        for (row, cells) in frame.rows.iter().enumerate() {
            for (col, cell) in cells.iter().enumerate() {
                if cell.width == Width::Spacer {
                    continue;
                }
                let mut fg = cell.fg.map_or(default_fg, to_hsla);
                let mut bg = cell.bg.map(to_hsla);
                if cell.inverse {
                    let old_fg = fg;
                    fg = bg.unwrap_or(default_bg);
                    bg = Some(old_fg);
                }
                if cell.faint {
                    fg = fg.opacity(0.66);
                }
                if cell.selected {
                    bg = Some(selection_bg);
                    if let Some(selection_fg) = selection_fg {
                        fg = selection_fg;
                    }
                }
                let width = if cell.width == Width::Wide { 2 } else { 1 };
                if let Some(bg) = bg.filter(|bg| *bg != default_bg) {
                    match backgrounds.last_mut() {
                        Some((r, cols, color)) if *r == row && cols.end == col && *color == bg => {
                            cols.end = col + width
                        }
                        _ => backgrounds.push((row, col..col + width, bg)),
                    }
                }
                if cell.text.is_empty() || cell.text == " " || cell.invisible {
                    continue;
                }
                let style = CellStyle {
                    fg,
                    bold: cell.bold,
                    italic: cell.italic,
                    underline: cell.underline,
                    strike: cell.strikethrough,
                };
                // A cell with combining marks or an emoji sequence draws on its own.
                let single = cell.text.chars().count() == 1;
                match segments.last_mut() {
                    Some(seg)
                        if single
                            && seg.single
                            && seg.row == row
                            && seg.width == width
                            && seg.style == style
                            && seg.col + seg.text.chars().count() * width == col =>
                    {
                        seg.text.push_str(&cell.text)
                    }
                    _ => segments.push(Segment {
                        row,
                        col,
                        width,
                        text: cell.text.clone(),
                        style,
                        single,
                    }),
                }
            }
        }

        let cursor = frame.cursor.as_ref().map(|c| {
            let color = to_hsla(
                frame
                    .cursor_color
                    .or(theme.cursor)
                    .unwrap_or(frame.foreground),
            );
            (c.y as usize, c.x as usize, c.shape, color, focused)
        });

        Self {
            background: default_bg,
            segments,
            backgrounds,
            cursor,
            highlights: Vec::new(),
            link: None,
        }
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
            point(
                origin.x + cell.width * col as f32,
                origin.y + cell.height * row as f32,
            )
        };

        for (row, cols, color) in &self.backgrounds {
            let size = size(cell.width * cols.len() as f32, cell.height);
            window.paint_quad(fill(Bounds::new(at(*row, cols.start), size), *color));
        }

        for (row, cols, color) in &self.highlights {
            let size = size(cell.width * cols.len() as f32, cell.height);
            window.paint_quad(fill(Bounds::new(at(*row, cols.start), size), *color));
        }
        if let Some((row, cols, color)) = &self.link {
            let start = at(*row, cols.start);
            let line = Bounds::new(
                point(start.x, start.y + cell.height - px(1.)),
                size(cell.width * cols.len() as f32, px(1.)),
            );
            window.paint_quad(fill(line, *color));
        }

        if let Some((row, col, shape, color, focused)) = self.cursor {
            let origin = at(row, col);
            let quad = match (shape, focused) {
                (CursorShape::Bar, _) => {
                    fill(Bounds::new(origin, size(px(2.), cell.height)), color)
                }
                (CursorShape::Underline, _) => fill(
                    Bounds::new(
                        point(origin.x, origin.y + cell.height - px(2.)),
                        size(cell.width, px(2.)),
                    ),
                    color,
                ),
                (CursorShape::Block, true) => fill(
                    Bounds::new(origin, size(cell.width, cell.height)),
                    color.opacity(0.7),
                ),
                // An unfocused pane, or a program asking for it, gets a hollow block.
                _ => outline(
                    Bounds::new(origin, size(cell.width, cell.height)),
                    color,
                    gpui::BorderStyle::Solid,
                ),
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

/// Default colors: the app theme's, with any the server set for this terminal on top.
#[derive(Clone)]
struct Theme {
    foreground: Rgb,
    background: Rgb,
    cursor: Option<Rgb>,
    selection: Rgb,
    selection_fg: Option<Rgb>,
    palette: [Rgb; 256],
    /// Tells two sets of colors apart, to know when the terminal needs new ones.
    key: String,
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
            key: theme.name.to_string(),
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
            let Some((index, color)) = entry.as_str().and_then(|e| e.split_once('=')) else {
                continue;
            };
            if let (Ok(index), Some(color)) = (index.parse::<usize>(), parse_hex(color))
                && index < 256
            {
                theme.palette[index] = color;
            }
        }
        theme.key = format!("{}/{msg}", theme.key);
        theme
    }
}

fn rgb(hex: u32) -> Rgb {
    ((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

fn parse_hex(s: &str) -> Option<Rgb> {
    let s = s.strip_prefix('#').unwrap_or(s);
    (s.len() == 6)
        .then(|| u32::from_str_radix(s, 16).ok().map(rgb))
        .flatten()
}

fn to_hsla((r, g, b): Rgb) -> Hsla {
    gpui::Rgba {
        r: r as f32 / 255.,
        g: g as f32 / 255.,
        b: b as f32 / 255.,
        a: 1.,
    }
    .into()
}

/// The 256-color palette: the theme's 16 colors, then the standard cube and gray ramp.
fn default_palette(ansi: &[u32; 16]) -> [Rgb; 256] {
    let mut palette = [(0, 0, 0); 256];
    for (i, &hex) in ansi.iter().enumerate() {
        palette[i] = rgb(hex);
    }
    let level = |v: usize| if v == 0 { 0 } else { (55 + v * 40) as u8 };
    for i in 0..216 {
        palette[16 + i] = (level(i / 36), level(i / 6 % 6), level(i % 6));
    }
    for i in 0..24 {
        let v = (8 + i * 10) as u8;
        palette[232 + i] = (v, v, v);
    }
    palette
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortens_home_only_on_a_path_boundary() {
        let info = |cwd: &str| PaneInfo {
            cwd: cwd.into(),
            home: "/home/am".into(),
            ..Default::default()
        };
        assert_eq!(info("/home/am").short_cwd(), "~");
        assert_eq!(info("/home/am/src").short_cwd(), "~/src");
        assert_eq!(info("/home/amit").short_cwd(), "/home/amit");
        assert_eq!(info("/tmp").short_cwd(), "/tmp");
    }
    fn row(text: &str) -> Vec<ghostty::Cell> {
        let mut term = ghostty::Terminal::new(80, 1).unwrap();
        term.write(text.as_bytes());
        let mut state = RenderState::new().unwrap();
        state.update(&term);
        state.frame().rows[0].clone()
    }

    #[test]
    fn finds_urls_in_text() {
        let text = "see (https://example.com/a_(b)?q=1). or mailto:me@x.org, done";
        let cells = row(text);
        let url = |col| url_in_row(&cells, col).map(|(_, url)| url);
        let start = text.find("https").unwrap();
        assert_eq!(url(start).as_deref(), Some("https://example.com/a_(b)?q=1"));
        assert_eq!(
            url(start + 10).as_deref(),
            Some("https://example.com/a_(b)?q=1")
        );
        assert_eq!(url(start - 1), None);
        assert_eq!(url(text.find(").").unwrap() + 1), None);
        assert_eq!(
            url(text.find("mailto").unwrap() + 3).as_deref(),
            Some("mailto:me@x.org")
        );
        assert_eq!(url(0), None);
        assert_eq!(
            url_in_row(&cells, start).map(|(cols, _)| cols),
            Some(start..start + "https://example.com/a_(b)?q=1".len())
        );
        // A scheme on its own, or inside a word, isn't a link.
        assert_eq!(url_in_row(&row("xhttps://a.b https:// "), 3), None);
        assert_eq!(url_in_row(&row("xhttps://a.b https:// "), 14), None);
    }

    #[test]
    fn opens_only_web_and_mail_links() {
        assert!(openable("https://example.com"));
        assert!(openable("HTTP://example.com"));
        assert!(openable("mailto:me@example.com"));
        assert!(!openable("file:///etc/passwd"));
        assert!(!openable("javascript:alert(1)"));
        assert!(!openable("https://"));
    }

    #[test]
    fn spans_cover_wrapped_rows() {
        let p = |x, y| ViewportPoint { x, y };
        let one_row = Span {
            start: p(2, 1),
            end: p(5, 1),
        };
        assert_eq!(span_rows(one_row, 10), vec![(1, 2..6)]);
        let wrapped = Span {
            start: p(7, 0),
            end: p(1, 2),
        };
        assert_eq!(
            span_rows(wrapped, 10),
            vec![(0, 7..10), (1, 0..10), (2, 0..2)]
        );
    }

    use crate::rex::data::StreamSplitter;

    /// `cargo test --release -- --ignored --nocapture parse_throughput`
    #[test]
    #[ignore]
    fn parse_throughput() {
        let line = b"the quick brown fox jumps over the lazy dog 0123456789\r\n";
        let chunk: Vec<u8> = line.iter().copied().cycle().take(64 * 1024).collect();
        let chunks = 800; // about 50 MB
        let mut splitter = StreamSplitter::new(7);
        let mut items = Vec::new();
        splitter.feed(
            b"\x1b_rex1;7;{\"type\":\"resize\",\"cols\":120,\"rows\":40}\x1b\\",
            &mut items,
        );

        let start = std::time::Instant::now();
        for _ in 0..chunks {
            items.clear();
            splitter.feed(&chunk, &mut items);
        }
        let split = start.elapsed();

        let mut term = ghostty::Terminal::new(120, 40).unwrap();
        let start = std::time::Instant::now();
        for _ in 0..chunks {
            term.write(&chunk);
        }
        let parse = start.elapsed();
        let mb = (chunk.len() * chunks) as f64 / 1e6;
        println!(
            "splitter: {:.0} MB/s, terminal parser: {:.0} MB/s",
            mb / split.as_secs_f64(),
            mb / parse.as_secs_f64()
        );
    }
}
