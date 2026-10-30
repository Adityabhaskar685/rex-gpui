//! The window, styled after Rex.app: a title bar with the host and the session's Rex windows
//! as tabs, an optional sidebar, and the active window's blocks as rounded cards laid out by
//! the fractions the server gives.

use std::collections::HashMap;

use anyhow::Result;
use gpui::{
    AnyElement, AnyView, StyleRefinement, App, Bounds, Context, CursorStyle, Decorations, Entity, FocusHandle, Focusable,
    FontWeight, HitboxBehavior, MouseButton, Pixels, Point, ResizeEdge, SharedString, Size,
    Task, Window, actions, canvas, div, point, prelude::*, px, relative, rgb, svg,
};
use serde_json::{Value, json};

use crate::rex::control::{Control, Event};
use crate::rex::view::{SessionSummary, SessionView, WindowView};
use crate::terminal::TerminalPane;

mod hosts;
mod rename;
mod theme_menu;
use hosts::{AddHostDialog, Host};
use rename::RenameDialog;
use theme_menu::ThemeMenu;

actions!(
    rex,
    [
        NewWindow,
        NewSession,
        SplitRight,
        SplitDown,
        ClosePane,
        NextWindow,
        PreviousWindow,
        NextPane,
        NextSession,
        ToggleZoom,
        ToggleSidebar,
        RenameSession,
        Quit,
    ]
);

pub fn bind_keys(cx: &mut App) {
    use gpui::KeyBinding;
    cx.bind_keys([
        KeyBinding::new("ctrl-shift-t", NewWindow, None),
        KeyBinding::new("ctrl-shift-n", NewSession, None),
        KeyBinding::new("ctrl-shift-d", SplitRight, None),
        KeyBinding::new("ctrl-shift-e", SplitDown, None),
        KeyBinding::new("ctrl-shift-w", ClosePane, None),
        KeyBinding::new("ctrl-tab", NextWindow, None),
        KeyBinding::new("ctrl-shift-tab", PreviousWindow, None),
        KeyBinding::new("ctrl-shift-o", NextPane, None),
        KeyBinding::new("ctrl-shift-s", NextSession, None),
        KeyBinding::new("ctrl-shift-z", ToggleZoom, None),
        KeyBinding::new("ctrl-shift-b", ToggleSidebar, None),
        KeyBinding::new("ctrl-shift-r", RenameSession, None),
        KeyBinding::new("ctrl-shift-q", Quit, None),
    ]);
}

const SHELL: &str = "com.superlogical.terminal.shell";

/// Colors for the window chrome. Terminals draw with their own theme.
use crate::theme::ui;

pub struct Workspace {
    control: Control,
    sessions: Vec<SessionSummary>,
    view: Option<SessionView>,
    panes: HashMap<String, Entity<TerminalPane>>,
    /// The block whose pane we last focused, to move keyboard focus when the server's changes.
    focused_block: Option<String>,
    sidebar: bool,
    host_name: String,
    /// The saved host this window is connected to, or `None` for the local server.
    current_host: Option<Host>,
    /// The local server, which keeps the host list, when `control` is a remote one.
    home: Option<Control>,
    hosts: Vec<Host>,
    /// The label the window was started with, matched against `hosts` once they load.
    pending_host_label: Option<String>,
    host_menu: bool,
    add_host: Option<AddHostDialog>,
    rename: Option<RenameDialog>,
    theme_menu: Option<ThemeMenu>,
    /// A message about connecting to another host, shown in the host menu.
    notice: Option<String>,
    error: Option<String>,
    focus_handle: FocusHandle,
    _events: Task<()>,
}

impl Workspace {
    pub fn new(
        control: Control,
        events: async_channel::Receiver<Event>,
        target: Option<String>,
        host_label: Option<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            host_name: control.endpoint.host_name(),
            control,
            sessions: Vec::new(),
            view: None,
            panes: HashMap::new(),
            focused_block: None,
            sidebar: false,
            current_host: None,
            home: None,
            hosts: Vec::new(),
            pending_host_label: None,
            host_menu: false,
            add_host: None,
            rename: None,
            theme_menu: None,
            notice: None,
            error: None,
            focus_handle: cx.focus_handle(),
            _events: Task::ready(()),
        };
        this.start(events, target, cx);
        // Show the saved host's name, when the window was started with one's label.
        if let Some(label) = host_label.filter(|_| !this.control.endpoint.is_local()) {
            this.pending_host_label = Some(label);
            this.load_hosts(cx);
        }
        this
    }

    /// Follow `self.control`'s events and attach to `target`, or the first session.
    fn start(&mut self, events: async_channel::Receiver<Event>, target: Option<String>, cx: &mut Context<Self>) {
        self._events = cx.spawn(async move |this, cx| {
            while let Ok(event) = events.recv().await {
                if this.update(cx, |this, cx| this.on_event(event, cx)).is_err() {
                    return;
                }
            }
            let _ = this.update(cx, |this, cx| {
                this.error = Some("Lost the connection to the Rex server".into());
                cx.notify();
            });
        });

        let control = self.control.clone();
        cx.spawn(async move |this, cx| {
            let result = async {
                control.call("session.attach_all", json!({"event_scope": "session"})).await?;
                let sessions = list_sessions(&control).await?;
                let session_id = match pick_session(&sessions, target.as_deref()) {
                    Some(id) => id,
                    None => create_session(&control, target.as_deref()).await?,
                };
                anyhow::Ok(session_id)
            }
            .await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(session_id) => this.attach(session_id, cx),
                Err(err) => {
                    this.error = Some(format!("{err:#}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn session_id(&self) -> Option<String> {
        self.view.as_ref().map(|v| v.session_id.clone())
    }

    fn attach(&mut self, session_id: String, cx: &mut Context<Self>) {
        let previous = self.session_id();
        let control = self.control.clone();
        cx.spawn(async move |this, cx| {
            let result = async {
                if let Some(previous) = previous.filter(|p| *p != session_id) {
                    let _ = control.call("session.detach", json!({"session_id": previous})).await;
                }
                let reply = control.call("session.attach", json!({"session_id": session_id})).await?;
                // Follow the terminal sizes this window asks for, unless another client owns them.
                control
                    .call("session.own_block", json!({"session_id": session_id, "mode": "request"}))
                    .await?;
                let view: SessionView = serde_json::from_value(reply["view"].clone())?;
                let sessions = list_sessions(&control).await?;
                anyhow::Ok((view, sessions))
            }
            .await;
            let _ = this.update(cx, |this, cx| match result {
                Ok((view, sessions)) => {
                    this.panes.clear();
                    this.focused_block = None;
                    this.sessions = sessions;
                    this.set_view(view, cx);
                }
                Err(err) => {
                    this.error = Some(format!("{err:#}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn set_view(&mut self, view: SessionView, cx: &mut Context<Self>) {
        if self.view.as_ref().is_some_and(|v| v.session_id == view.session_id && v.revision > view.revision) {
            return;
        }
        // Keep panes for blocks still in a layout, including ones a zoom hides, so their
        // streams stay open; make panes for new ones.
        let placed: Vec<String> = view
            .windows
            .iter()
            .flat_map(|w| w.all_blocks())
            .collect();
        self.panes.retain(|id, _| placed.contains(id));
        for block_id in placed {
            if !self.panes.contains_key(&block_id) {
                let control = self.control.clone();
                let session_id = view.session_id.clone();
                let pane = cx.new(|cx| TerminalPane::new(control, session_id, block_id.clone(), cx));
                // Titles show in the tabs and sidebar, so redraw when a pane's changes. Not on
                // every change to the pane, which would redraw every pane with it.
                cx.subscribe(&pane, |_, _, _: &crate::terminal::InfoChanged, cx| cx.notify()).detach();
                self.panes.insert(block_id, pane);
            }
        }
        self.view = Some(view);
        self.error = None;
        cx.notify();
    }

    fn refresh_view(&mut self, cx: &mut Context<Self>) {
        let Some(session_id) = self.session_id() else { return };
        let control = self.control.clone();
        cx.spawn(async move |this, cx| {
            match control.call("session.view", json!({"session_id": session_id})).await {
                Ok(reply) => {
                    if let Ok(view) = serde_json::from_value::<SessionView>(reply) {
                        let _ = this.update(cx, |this, cx| this.set_view(view, cx));
                    }
                }
                Err(err) => log::warn!("{err:#}"),
            }
        })
        .detach();
    }

    fn refresh_sessions(&mut self, cx: &mut Context<Self>) {
        let control = self.control.clone();
        cx.spawn(async move |this, cx| {
            if let Ok(sessions) = list_sessions(&control).await {
                let _ = this.update(cx, |this, cx| {
                    this.sessions = sessions;
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn on_event(&mut self, event: Event, cx: &mut Context<Self>) {
        match event.kind.as_str() {
            "session_created" | "session_moved" => self.refresh_sessions(cx),
            "hosts_changed" => self.load_hosts(cx),
            "session_destroyed" => {
                let gone = event.payload["session_id"].as_str().map(str::to_string);
                self.sessions.retain(|s| Some(&s.session_id) != gone.as_ref());
                if gone.is_some() && gone == self.session_id() {
                    self.view = None;
                    self.panes.clear();
                    match self.sessions.first() {
                        Some(next) => self.attach(next.session_id.clone(), cx),
                        None => cx.quit(),
                    }
                }
                cx.notify();
            }
            "session_event" => {
                let session_id = event.payload["session_id"].as_str().unwrap_or_default();
                if Some(session_id) != self.session_id().as_deref() {
                    return;
                }
                let inner = &event.payload["event"];
                let payload = &inner["payload"];
                match inner["type"].as_str() {
                    Some("session_view_changed") => {
                        let revision = payload["revision"].as_u64().unwrap_or(0);
                        if self.view.as_ref().is_none_or(|v| revision > v.revision) {
                            self.refresh_view(cx);
                        }
                    }
                    Some("session_label_changed") => {
                        if let Some(view) = &mut self.view {
                            view.label = payload["label"].as_str().unwrap_or_default().into();
                        }
                        self.refresh_sessions(cx);
                    }
                    Some("block_event") => {
                        let block_id = payload["block_id"].as_str().unwrap_or_default();
                        if let Some(pane) = self.panes.get(block_id) {
                            let name = payload["name"].as_str().unwrap_or_default().to_string();
                            let body = payload["payload"].clone();
                            pane.update(cx, |pane, cx| pane.on_block_event(&name, &body, cx));
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn call(&self, method: &'static str, mut payload: Value) {
        let Some(session_id) = self.session_id() else { return };
        payload["session_id"] = json!(session_id);
        self.control.send(method, payload);
    }

    fn active_window(&self) -> Option<&WindowView> {
        let view = self.view.as_ref()?;
        let active = view.active_window_id.as_ref();
        view.windows.iter().find(|w| Some(&w.window_id) == active).or(view.windows.first())
    }

    fn focused_block_id(&self) -> Option<String> {
        self.active_window()?.focused_block_id.clone()
    }

    fn window_title(&self, window: &WindowView, cx: &App) -> String {
        if !window.label.is_empty() && !window.label.starts_with("Window ") {
            return window.label.clone();
        }
        window
            .focused_block_id
            .as_ref()
            .and_then(|id| self.panes.get(id))
            .map(|pane| pane.read(cx).info.display_title())
            .unwrap_or_else(|| "Terminal".into())
    }

    fn split(&mut self, anchor: Option<String>, direction: &'static str, cx: &mut Context<Self>) {
        let Some(session_id) = self.session_id() else { return };
        let control = self.control.clone();
        cx.spawn(async move |_, _| {
            let cwd = match &anchor {
                Some(block_id) => current_dir(&control, &session_id, block_id).await,
                None => None,
            };
            let result = control
                .call(
                    "session.new_split",
                    json!({
                        "session_id": session_id,
                        "anchor_id": anchor,
                        "direction": direction,
                        "side": "after",
                        "focus": true,
                        "layout": {"block": {"flavor": SHELL, "options": {"cwd": cwd.unwrap_or_default()}}},
                    }),
                )
                .await;
            if let Err(err) = result {
                log::warn!("{err:#}");
            }
        })
        .detach();
    }

    fn new_window(&mut self, _: &NewWindow, _: &mut Window, cx: &mut Context<Self>) {
        let Some(session_id) = self.session_id() else { return };
        let anchor = self.focused_block_id();
        let control = self.control.clone();
        cx.spawn(async move |_, _| {
            let cwd = match &anchor {
                Some(block_id) => current_dir(&control, &session_id, block_id).await,
                None => None,
            };
            let payload = json!({
                "session_id": session_id,
                "layout": {"block": {"flavor": SHELL, "options": {"cwd": cwd.unwrap_or_default()}}},
            });
            if let Err(err) = control.call("session.new_window", payload).await {
                log::warn!("{err:#}");
            }
        })
        .detach();
    }

    fn new_session(&mut self, _: &NewSession, _: &mut Window, cx: &mut Context<Self>) {
        let control = self.control.clone();
        cx.spawn(async move |this, cx| match create_session(&control, None).await {
            Ok(id) => {
                let _ = this.update(cx, |this, cx| this.attach(id, cx));
            }
            Err(err) => log::warn!("{err:#}"),
        })
        .detach();
    }

    fn next_session(&mut self, _: &NextSession, _: &mut Window, cx: &mut Context<Self>) {
        let current = self.session_id();
        let index = self.sessions.iter().position(|s| Some(&s.session_id) == current.as_ref());
        let next = index.map_or(0, |i| (i + 1) % self.sessions.len().max(1));
        if let Some(next) = self.sessions.get(next).filter(|s| Some(&s.session_id) != current.as_ref()) {
            self.attach(next.session_id.clone(), cx);
        }
    }

    fn close_block(&self, block_id: String) {
        self.call("block.close", json!({"block_id": block_id}));
    }

    fn zoom_block(&self, block_id: String) {
        self.call("session.zoom_block", json!({"block_id": block_id}));
    }

    fn render_titlebar(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let client_side = matches!(window.window_decorations(), Decorations::Client { .. });
        let view = self.view.as_ref();
        let active = view.and_then(|v| v.active_window_id.clone());
        let session_label = view.map(|v| v.label.clone()).unwrap_or_else(|| "Rex".into());

        let tabs = view.into_iter().flat_map(|v| v.windows.iter()).map(|w| {
            let is_active = Some(&w.window_id) == active.as_ref();
            let window_id = w.window_id.clone();
            div()
                .id(SharedString::from(w.window_id.clone()))
                .flex()
                .items_center()
                .gap_2()
                .h(px(30.))
                .px_3()
                .min_w(px(0.))
                .max_w(px(260.))
                .rounded(px(9.))
                .border_1()
                .cursor_pointer()
                .map(|tab| {
                    if is_active {
                        tab.bg(rgb(ui().tab_active)).border_color(rgb(ui().tab_active_border)).text_color(rgb(ui().text))
                    } else {
                        tab.border_color(gpui::transparent_black())
                            .text_color(rgb(ui().text_dim))
                            .hover(|tab| tab.bg(rgb(ui().hover)))
                    }
                })
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(move |this, _, _, _| {
                    this.call("session.focus_window", json!({"window_id": window_id}));
                }))
                .child(icon("icons/terminal.svg", 14., if is_active { ui().text } else { ui().text_dim }))
                .child(div().truncate().child(self.window_title(w, cx)))
        });

        div()
            .id("titlebar")
            .flex()
            .flex_row()
            .items_center()
            .h(px(44.))
            .flex_none()
            .px_3()
            .gap_2()
            .text_sm()
            // Empty space in the bar moves the window, as a title bar does.
            .when(client_side, |bar| {
                bar.on_mouse_down(MouseButton::Left, |event, window, _| {
                    if event.click_count == 2 {
                        window.zoom_window();
                    } else {
                        window.start_window_move();
                    }
                })
                .on_mouse_down(MouseButton::Right, |event, window, _| {
                    window.show_window_menu(event.position)
                })
            })
            .child(
                icon_button("sidebar", "icons/sidebar.svg", self.sidebar)
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_sidebar(&ToggleSidebar, window, cx))),
            )
            .child(
                div()
                    .id("host")
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_0p5()
                    .rounded(px(8.))
                    .cursor_pointer()
                    .hover(|d| d.bg(rgb(ui().hover)))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .when(self.host_menu, |d| d.bg(rgb(ui().tab_active)))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_host_menu(cx)))
                    .child(icon("icons/server.svg", 16., ui().text_dim))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .line_height(px(15.))
                            .child(div().text_color(rgb(ui().text)).font_weight(FontWeight::SEMIBOLD).child(session_label))
                            .child(div().text_xs().text_color(rgb(ui().text_faint)).child(self.host_title())),
                    ),
            )
            .child(div().flex().flex_1().min_w(px(0.)).gap_1().items_center().children(tabs))
            .child(
                icon_button("theme", "icons/palette.svg", self.theme_menu.is_some())
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_theme_menu(window, cx))),
            )
            .child(
                icon_button("new-window", "icons/plus.svg", false)
                    .on_click(cx.listener(|this, _, window, cx| this.new_window(&NewWindow, window, cx))),
            )
            .when(client_side, |bar| bar.child(window_controls(window)))
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.session_id();
        let active = self.view.as_ref().and_then(|v| v.active_window_id.clone());
        let mut list = div().flex().flex_col().gap_0p5().px_2().pt_1();
        for session in &self.sessions {
            let is_current = Some(&session.session_id) == current.as_ref();
            let session_id = session.session_id.clone();
            list = list.child(
                div()
                    .id(SharedString::from(session.session_id.clone()))
                    .px_2()
                    .pt_2()
                    .pb_1()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(if is_current { ui().text_dim } else { ui().text_faint }))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.session_id().as_ref() != Some(&session_id) {
                            this.attach(session_id.clone(), cx);
                        }
                    }))
                    .child(if session.label.is_empty() { session.session_id.clone() } else { session.label.clone() }),
            );
            if !is_current {
                continue;
            }
            for w in self.view.iter().flat_map(|v| v.windows.iter()) {
                let is_active = Some(&w.window_id) == active.as_ref();
                let window_id = w.window_id.clone();
                list = list.child(
                    div()
                        .id(SharedString::from(format!("side-{}", w.window_id)))
                        .flex()
                        .items_center()
                        .gap_2()
                        .h(px(30.))
                        .px_2()
                        .rounded(px(8.))
                        .cursor_pointer()
                        .text_color(rgb(if is_active { ui().text } else { ui().text_dim }))
                        .when(is_active, |d| d.bg(rgb(ui().tab_active)))
                        .when(!is_active, |d| d.hover(|d| d.bg(rgb(ui().hover))))
                        .on_click(cx.listener(move |this, _, _, _| {
                            this.call("session.focus_window", json!({"window_id": window_id}));
                        }))
                        .child(icon("icons/terminal.svg", 14., if is_active { ui().text } else { ui().text_dim }))
                        .child(div().truncate().child(self.window_title(w, cx))),
                );
            }
        }
        div()
            .id("sidebar")
            .w(px(230.))
            .flex_none()
            .h_full()
            .overflow_y_scroll()
            .text_sm()
            .child(list)
    }

    fn render_window(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The cards sit `GAP` apart, and `GAP / 2` in from each edge of this area plus its own.
        const GAP: f32 = 8.;
        let mut area = div().relative().flex_1().h_full().min_w(px(0.)).mr(px(GAP / 2.)).mb(px(GAP / 2.));
        let Some(rex_window) = self.active_window().cloned() else {
            let message = self.error.clone().unwrap_or_else(|| "Connecting to Rex…".into());
            return area
                .flex()
                .items_center()
                .justify_center()
                .text_color(rgb(ui().text_faint))
                .child(message);
        };

        // Give keyboard focus to the block the server says is focused, when that changes.
        if rex_window.focused_block_id != self.focused_block {
            if let Some(pane) = rex_window.focused_block_id.as_ref().and_then(|id| self.panes.get(id)) {
                window.focus(&pane.focus_handle(cx));
            }
            // Panes are cached views, so the two whose cursors change must be told to redraw.
            for id in [&self.focused_block, &rex_window.focused_block_id].into_iter().flatten() {
                if let Some(pane) = self.panes.get(id) {
                    pane.update(cx, |_, cx| cx.notify());
                }
            }
            self.focused_block = rex_window.focused_block_id.clone();
        }

        let zoomed = rex_window.zoomed_block_id.is_some();
        for (block_id, [x, y, w, h]) in rex_window.placed_blocks() {
            let Some(pane) = self.panes.get(&block_id).cloned() else { continue };
            let focused = Some(&block_id) == rex_window.focused_block_id.as_ref();
            area = area.child(
                div()
                    .absolute()
                    .left(relative(x))
                    .top(relative(y))
                    .w(relative(w))
                    .h(relative(h))
                    .p(px(GAP / 2.))
                    .child(self.render_card(block_id, pane, focused, zoomed, cx)),
            );
        }
        area
    }

    fn render_card(
        &self,
        block_id: String,
        pane: Entity<TerminalPane>,
        focused: bool,
        zoomed: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (title, background) = {
            let pane = pane.read(cx);
            (pane.info.display_title(), pane.background())
        };
        let group = SharedString::from(format!("card-{block_id}"));
        let button = |id: &'static str, path: &'static str| {
            icon_button(id, path, false)
                .invisible()
                .group_hover(group.clone(), |d| d.visible())
        };
        let (split_right, split_down, zoom, close) =
            (block_id.clone(), block_id.clone(), block_id.clone(), block_id.clone());

        div()
            .id(SharedString::from(format!("card-{block_id}")))
            .group(group.clone())
            .size_full()
            .flex()
            .flex_col()
            .rounded(px(12.))
            .overflow_hidden()
            .border_1()
            .border_color(rgb(if focused { ui().card_border_focused } else { ui().card_border }))
            .bg(background)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, _| {
                    if !focused {
                        this.call("session.focus_block", json!({"block_id": block_id}));
                    }
                }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .h(px(34.))
                    .flex_none()
                    .pl_3()
                    .pr_2()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(if focused { ui().text } else { ui().text_dim }))
                    .child(icon("icons/terminal.svg", 14., if focused { ui().text } else { ui().text_dim }))
                    .child(div().flex_1().min_w(px(0.)).truncate().child(title))
                    .child(button("split-right", "icons/split-right.svg").on_click(cx.listener(
                        move |this, _, _, cx| this.split(Some(split_right.clone()), "horizontal", cx),
                    )))
                    .child(button("split-down", "icons/split-down.svg").on_click(cx.listener(
                        move |this, _, _, cx| this.split(Some(split_down.clone()), "vertical", cx),
                    )))
                    .child(
                        button("zoom", if zoomed { "icons/minimize.svg" } else { "icons/maximize.svg" })
                            .on_click(cx.listener(move |this, _, _, _| this.zoom_block(zoom.clone()))),
                    )
                    .child(
                        button("close", "icons/close.svg")
                            .on_click(cx.listener(move |this, _, _, _| this.close_block(close.clone()))),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .pl_2()
                    .pr_1()
                    .pb_1()
                    // A cached view: a pane that hasn't changed reuses its last frame instead of
                    // drawing again whenever another pane, or the window, redraws.
                    .child(AnyView::from(pane).cached(StyleRefinement::default().size_full())),
            )
    }

    fn toggle_sidebar(&mut self, _: &ToggleSidebar, _: &mut Window, cx: &mut Context<Self>) {
        self.sidebar = !self.sidebar;
        cx.notify();
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::count_render("workspace");
        if let Some(view) = &self.view {
            window.set_window_title(&format!("{} — Rex", view.label));
        }
        let content = div()
            .relative()
            .key_context("Workspace")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::new_window))
            .on_action(cx.listener(Self::new_session))
            .on_action(cx.listener(Self::next_session))
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(cx.listener(|this, _: &RenameSession, window, cx| this.open_rename(window, cx)))
            .on_action(cx.listener(|this, _: &ClosePane, _, _| {
                if let Some(id) = this.focused_block_id() {
                    this.close_block(id);
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleZoom, _, _| {
                if let Some(id) = this.focused_block_id() {
                    this.zoom_block(id);
                }
            }))
            .on_action(cx.listener(|this, _: &SplitRight, _, cx| {
                this.split(this.focused_block_id(), "horizontal", cx)
            }))
            .on_action(cx.listener(|this, _: &SplitDown, _, cx| {
                this.split(this.focused_block_id(), "vertical", cx)
            }))
            .on_action(cx.listener(|this, _: &NextWindow, _, _| {
                this.call("session.focus_next_window", json!({}))
            }))
            .on_action(cx.listener(|this, _: &PreviousWindow, _, _| {
                this.call("session.focus_previous_window", json!({}))
            }))
            .on_action(cx.listener(|this, _: &NextPane, _, _| {
                this.call("session.focus_next_pane", json!({}))
            }))
            .on_action(cx.listener(|_, _: &Quit, _, cx| cx.quit()))
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(ui().window))
            .text_color(rgb(ui().text))
            .font_family("Ubuntu Sans")
            .child(self.render_titlebar(window, cx))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h(px(0.))
                    .pl(px(4.))
                    .when(self.sidebar, |d| d.child(self.render_sidebar(cx)))
                    .child(self.render_window(window, cx)),
            )
            .when(self.host_menu, |d| d.child(self.render_host_menu(cx)))
            .when_some(self.theme_menu.as_ref(), |d, menu| d.child(self.render_theme_menu(menu, cx)))
            .when_some(self.add_host.as_ref(), |d, dialog| d.child(self.render_add_host(dialog, cx)))
            .when_some(self.rename.as_ref(), |d, dialog| d.child(self.render_rename(dialog, cx)));
        window_frame(content.into_any_element(), window)
    }
}

fn icon(path: &'static str, size: f32, color: u32) -> impl IntoElement {
    svg().path(path).size(px(size)).flex_none().text_color(rgb(color))
}

fn icon_button(id: &'static str, path: &'static str, active: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .size(px(26.))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded(px(7.))
        .cursor_pointer()
        .when(active, |d| d.bg(rgb(ui().tab_active)))
        .hover(|d| d.bg(rgb(ui().tab_active)))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(icon(path, 15., ui().text_dim))
}

/// Minimize, maximize, and close, in the style of GNOME's title bars.
fn window_controls(window: &Window) -> impl IntoElement {
    let controls = window.window_controls();
    let button = |id: &'static str, path: &'static str, hover_bg: u32, hover_fg: u32| {
        div()
            .id(id)
            .group(id)
            .size(px(24.))
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .rounded_full()
            .bg(rgb(ui().tab_active))
            .text_color(rgb(ui().text))
            .hover(move |d| d.bg(rgb(hover_bg)).text_color(rgb(hover_fg)))
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            // GPUI fills an SVG with the SVG element's own text color, not its parent's.
            .child(
                svg()
                    .path(path)
                    .size(px(14.))
                    .text_color(rgb(ui().text))
                    .group_hover(id, move |d| d.text_color(rgb(hover_fg))),
            )
    };
    div()
        .flex()
        .items_center()
        .gap(px(10.))
        .pl_3()
        .when(controls.minimize, |d| {
            d.child(
                button("minimize", "icons/win-minimize.svg", ui().tab_active_border, ui().text)
                    .on_click(|_, window, _| window.minimize_window()),
            )
        })
        .when(controls.maximize, |d| {
            let path = if window.is_maximized() { "icons/win-restore.svg" } else { "icons/win-maximize.svg" };
            d.child(
                button("maximize", path, ui().tab_active_border, ui().text)
                    .on_click(|_, window, _| window.zoom_window()),
            )
        })
        .child(
            button("close", "icons/win-close.svg", ui().danger, ui().window)
                .on_click(|_, window, _| window.remove_window()),
        )
}

const SHADOW: Pixels = px(10.);
const ROUNDING: Pixels = px(12.);

/// Draw the window's own frame when the compositor leaves decorations to the client, as
/// GNOME on Wayland does: a shadow that doubles as resize handles, and rounded corners.
fn window_frame(content: AnyElement, window: &mut Window) -> AnyElement {
    let Decorations::Client { tiling } = window.window_decorations() else {
        window.set_client_inset(px(0.));
        return div().size_full().child(content).into_any_element();
    };
    window.set_client_inset(SHADOW);
    let inner = div()
        .size_full()
        .overflow_hidden()
        .cursor(CursorStyle::Arrow)
        .border_color(rgb(ui().card_border))
        .when(!(tiling.top || tiling.left), |d| d.rounded_tl(ROUNDING))
        .when(!(tiling.top || tiling.right), |d| d.rounded_tr(ROUNDING))
        .when(!(tiling.bottom || tiling.left), |d| d.rounded_bl(ROUNDING))
        .when(!(tiling.bottom || tiling.right), |d| d.rounded_br(ROUNDING))
        .when(!tiling.top, |d| d.border_t_1())
        .when(!tiling.bottom, |d| d.border_b_1())
        .when(!tiling.left, |d| d.border_l_1())
        .when(!tiling.right, |d| d.border_r_1())
        .when(!tiling.is_tiled(), |d| {
            d.shadow(vec![gpui::BoxShadow {
                color: gpui::black().opacity(0.45),
                blur_radius: SHADOW / 2.,
                spread_radius: px(0.),
                offset: point(px(0.), px(0.)),
            }])
        })
        .on_mouse_move(|_, _, cx| cx.stop_propagation())
        .child(content);

    div()
        .id("window-frame")
        .size_full()
        .bg(gpui::transparent_black())
        .child(
            canvas(
                |_, window, _| {
                    let size = window.window_bounds().get_bounds().size;
                    window.insert_hitbox(Bounds::new(point(px(0.), px(0.)), size), HitboxBehavior::Normal)
                },
                |_, hitbox, window, _| {
                    let size = window.window_bounds().get_bounds().size;
                    let Some(edge) = resize_edge(window.mouse_position(), size) else { return };
                    let style = match edge {
                        ResizeEdge::Top | ResizeEdge::Bottom => CursorStyle::ResizeUpDown,
                        ResizeEdge::Left | ResizeEdge::Right => CursorStyle::ResizeLeftRight,
                        ResizeEdge::TopLeft | ResizeEdge::BottomRight => CursorStyle::ResizeUpLeftDownRight,
                        ResizeEdge::TopRight | ResizeEdge::BottomLeft => CursorStyle::ResizeUpRightDownLeft,
                    };
                    window.set_cursor_style(style, &hitbox);
                },
            )
            .size_full()
            .absolute(),
        )
        .when(!tiling.top, |d| d.pt(SHADOW))
        .when(!tiling.bottom, |d| d.pb(SHADOW))
        .when(!tiling.left, |d| d.pl(SHADOW))
        .when(!tiling.right, |d| d.pr(SHADOW))
        .on_mouse_move(|_, window, _| window.refresh())
        .on_mouse_down(MouseButton::Left, |event, window, _| {
            let size = window.window_bounds().get_bounds().size;
            if let Some(edge) = resize_edge(event.position, size) {
                window.start_window_resize(edge);
            }
        })
        .child(inner)
        .into_any_element()
}

fn resize_edge(pos: Point<Pixels>, size: Size<Pixels>) -> Option<ResizeEdge> {
    let (left, right) = (pos.x < SHADOW, pos.x > size.width - SHADOW);
    let (top, bottom) = (pos.y < SHADOW, pos.y > size.height - SHADOW);
    Some(match (top, bottom, left, right) {
        (true, _, true, _) => ResizeEdge::TopLeft,
        (true, _, _, true) => ResizeEdge::TopRight,
        (_, true, true, _) => ResizeEdge::BottomLeft,
        (_, true, _, true) => ResizeEdge::BottomRight,
        (true, ..) => ResizeEdge::Top,
        (_, true, ..) => ResizeEdge::Bottom,
        (_, _, true, _) => ResizeEdge::Left,
        (_, _, _, true) => ResizeEdge::Right,
        _ => return None,
    })
}

async fn list_sessions(control: &Control) -> Result<Vec<SessionSummary>> {
    let reply = control.call("session.list", json!({})).await?;
    Ok(serde_json::from_value(reply["sessions"].clone()).unwrap_or_default())
}

/// The session named by `target`, as an ID or label, or else the first one.
fn pick_session(sessions: &[SessionSummary], target: Option<&str>) -> Option<String> {
    match target {
        Some(t) => sessions.iter().find(|s| s.session_id == t || s.label == t),
        None => sessions.first(),
    }
    .map(|s| s.session_id.clone())
}

async fn create_session(control: &Control, label: Option<&str>) -> Result<String> {
    // A remote server's home is its own, so let it pick the directory.
    let cwd = if control.endpoint.is_local() { std::env::var("HOME").unwrap_or_default() } else { String::new() };
    let mut payload = json!({
        "initial_windows": [{"layout": {"block": {"flavor": SHELL, "options": {"cwd": cwd}}}}],
    });
    if let Some(label) = label {
        payload["label"] = json!(label);
    }
    let reply = control.call("session.create", payload).await?;
    Ok(reply["session_id"].as_str().unwrap_or_default().to_string())
}

/// The directory a new split or window should start in: where the anchor's program is.
async fn current_dir(control: &Control, session_id: &str, block_id: &str) -> Option<String> {
    let process = control
        .block_call("com.superlogical.terminal.process", session_id, block_id, json!({}))
        .await
        .ok()?;
    process["foreground"]["cwd"].as_str().filter(|c| !c.is_empty()).map(str::to_string)
}
