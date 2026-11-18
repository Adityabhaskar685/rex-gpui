//! The quick switcher, opened with ctrl-shift-p: a search field over every session and its
//! tabs. Typing narrows the list; Up and Down move through it, and Enter goes to the
//! highlighted one.

use gpui::{
    App, AppContext as _, Context, Entity, Focusable, KeyDownEvent, MouseButton, ScrollStrategy,
    SharedString, Subscription, UniformListScrollHandle, Window, div, prelude::*, px, rgb,
    uniform_list,
};
use serde_json::json;

use super::{Workspace, icon, list_sessions};
use crate::keymap::QuickSwitch;
use crate::rex::view::SessionView;
use crate::text_field::{TextField, TextFieldEvent};
use crate::theme::ui;

pub struct Switcher {
    search: Entity<TextField>,
    /// The other sessions' tabs, once they've been read from the server.
    others: Vec<SessionView>,
    /// The highlighted item, in the filtered list.
    selected: usize,
    scroll: UniformListScrollHandle,
    _subscriptions: Vec<Subscription>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Session(String),
    Tab {
        session_id: String,
        window_id: String,
    },
}

#[derive(Clone, Debug)]
pub struct Item {
    pub title: String,
    /// The session a tab is in, or "Session" for a session.
    pub detail: String,
    pub target: Target,
    /// The tab or session this window shows now.
    pub current: bool,
}

/// The items whose title and detail hold every word of `query`, in any case.
pub fn filter(items: Vec<Item>, query: &str) -> Vec<Item> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    items
        .into_iter()
        .filter(|item| {
            let text = format!("{} {}", item.title, item.detail).to_lowercase();
            words.iter().all(|w| text.contains(w.as_str()))
        })
        .collect()
}

fn session_name(label: &str, id: &str) -> String {
    if label.is_empty() {
        id.to_string()
    } else {
        label.to_string()
    }
}

impl Workspace {
    pub(super) fn toggle_switcher(
        &mut self,
        _: &QuickSwitch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.switcher.take().is_some() {
            self.focused_block = None;
            cx.notify();
            return;
        }
        // Another dialog has the keyboard; the keys are for it.
        if self.typing() {
            cx.propagate();
            return;
        }
        let search = cx.new(|cx| TextField::new("Go to a session or tab", cx));
        let subscriptions = vec![
            // A new search starts at the top of the list.
            cx.observe(&search, |this, _, cx| {
                if let Some(switcher) = &mut this.switcher {
                    switcher.selected = 0;
                    switcher.scroll.scroll_to_item(0, ScrollStrategy::Top);
                }
                cx.notify();
            }),
            cx.subscribe_in(
                &search,
                window,
                |this, _, event: &TextFieldEvent, window, cx| match event {
                    TextFieldEvent::Submit => {
                        let selected = this.switcher.as_ref().map_or(0, |s| s.selected);
                        if let Some(item) = this.switcher_items(cx).get(selected).cloned() {
                            this.switch_to(item.target, window, cx);
                        }
                    }
                    TextFieldEvent::Cancel => this.close_switcher(cx),
                },
            ),
        ];
        window.focus(&search.focus_handle(cx));
        self.switcher = Some(Switcher {
            search,
            others: Vec::new(),
            selected: 0,
            scroll: UniformListScrollHandle::new(),
            _subscriptions: subscriptions,
        });
        self.host_menu = false;
        self.theme_menu = None;
        self.shortcuts = false;
        self.load_other_tabs(cx);
        cx.notify();
    }

    fn close_switcher(&mut self, cx: &mut Context<Self>) {
        self.switcher = None;
        // Hand the keyboard back to the focused pane on the next frame.
        self.focused_block = None;
        cx.notify();
    }

    /// Read the sessions, and the tabs of every session but this one, for the list.
    fn load_other_tabs(&mut self, cx: &mut Context<Self>) {
        let control = self.control.clone();
        let current = self.session_id();
        cx.spawn(async move |this, cx| {
            let Ok(sessions) = list_sessions(&control).await else {
                return;
            };
            let mut others = Vec::new();
            for session in sessions
                .iter()
                .filter(|s| Some(&s.session_id) != current.as_ref())
            {
                let reply = control
                    .call("session.view", json!({"session_id": session.session_id}))
                    .await;
                match reply.map(serde_json::from_value::<SessionView>) {
                    Ok(Ok(view)) => others.push(view),
                    Ok(Err(err)) => log::debug!("reading {}'s tabs: {err}", session.session_id),
                    Err(err) => log::debug!("reading {}'s tabs: {err:#}", session.session_id),
                }
            }
            let _ = this.update(cx, |this, cx| {
                this.sessions = sessions;
                if let Some(switcher) = &mut this.switcher {
                    switcher.others = others;
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Everything the switcher offers, filtered by its search: this session's tabs, then
    /// each other session followed by its tabs.
    fn switcher_items(&self, cx: &App) -> Vec<Item> {
        let Some(switcher) = &self.switcher else {
            return Vec::new();
        };
        let mut items = Vec::new();
        if let Some(view) = &self.view {
            let name = session_name(&view.label, &view.session_id);
            for w in &view.windows {
                items.push(Item {
                    title: self.window_title(w, cx),
                    detail: name.clone(),
                    target: Target::Tab {
                        session_id: view.session_id.clone(),
                        window_id: w.window_id.clone(),
                    },
                    current: view.active_window_id.as_ref() == Some(&w.window_id),
                });
            }
        }
        let current = self.session_id();
        for session in self
            .sessions
            .iter()
            .filter(|s| Some(&s.session_id) != current.as_ref())
        {
            let name = session_name(&session.label, &session.session_id);
            items.push(Item {
                title: name.clone(),
                detail: "Session".into(),
                target: Target::Session(session.session_id.clone()),
                current: false,
            });
            let view = switcher
                .others
                .iter()
                .find(|v| v.session_id == session.session_id);
            for w in view.into_iter().flat_map(|v| &v.windows) {
                let title = if w.label.is_empty() {
                    "Terminal".to_string()
                } else {
                    w.label.clone()
                };
                items.push(Item {
                    title,
                    detail: name.clone(),
                    target: Target::Tab {
                        session_id: session.session_id.clone(),
                        window_id: w.window_id.clone(),
                    },
                    current: false,
                });
            }
        }
        filter(items, &switcher.search.read(cx).text)
    }

    fn switch_to(&mut self, target: Target, _window: &mut Window, cx: &mut Context<Self>) {
        let current = self.session_id();
        match target {
            Target::Session(session_id) => {
                if current.as_ref() != Some(&session_id) {
                    self.attach(session_id, cx);
                }
            }
            Target::Tab {
                session_id,
                window_id,
            } => {
                // Focus the tab first, so the session opens on it.
                self.control.send(
                    "session.focus_window",
                    json!({"session_id": session_id, "window_id": window_id}),
                );
                if current.as_ref() != Some(&session_id) {
                    self.attach(session_id, cx);
                }
            }
        }
        self.close_switcher(cx);
    }

    /// Up and Down move the highlight; the text field leaves them alone.
    pub(super) fn on_switcher_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let count = self.switcher_items(cx).len();
        let Some(switcher) = &mut self.switcher else {
            return;
        };
        let selected = match event.keystroke.key.as_str() {
            "down" => (switcher.selected + 1).min(count.saturating_sub(1)),
            "up" => switcher.selected.saturating_sub(1),
            _ => return,
        };
        switcher.selected = selected;
        switcher
            .scroll
            .scroll_to_item(selected, ScrollStrategy::Top);
        cx.stop_propagation();
        cx.notify();
    }

    pub(super) fn render_switcher(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(switcher) = &self.switcher else {
            return div().into_any_element();
        };
        let items = self.switcher_items(cx);
        let count = items.len();
        let selected = switcher.selected.min(count.saturating_sub(1));

        let list = uniform_list(
            "switcher-list",
            count,
            cx.processor(move |_this, range: std::ops::Range<usize>, _window, cx| {
                range
                    .map(|i| {
                        let item = items[i].clone();
                        let target = item.target.clone();
                        let is_session = matches!(item.target, Target::Session(_));
                        div()
                            .id(SharedString::from(format!("switch-{i}")))
                            .flex()
                            .items_center()
                            .gap_2()
                            .h(px(32.))
                            .px_2()
                            .rounded(px(7.))
                            .cursor_pointer()
                            .text_color(rgb(ui().text))
                            .when(i == selected, |d| d.bg(rgb(ui().tab_active)))
                            .when(i != selected, |d| d.hover(|d| d.bg(rgb(ui().hover))))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.switch_to(target.clone(), window, cx)
                            }))
                            .child(icon(
                                if is_session {
                                    "icons/server.svg"
                                } else {
                                    "icons/terminal.svg"
                                },
                                14.,
                                ui().text_dim,
                            ))
                            .child(div().flex_1().min_w(px(0.)).truncate().child(item.title))
                            .when(item.current, |d| {
                                d.child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(ui().accent))
                                        .child("current"),
                                )
                            })
                            .child(
                                div()
                                    .flex_none()
                                    .max_w(px(160.))
                                    .truncate()
                                    .text_xs()
                                    .text_color(rgb(ui().text_faint))
                                    .child(item.detail),
                            )
                    })
                    .collect()
            }),
        )
        .track_scroll(switcher.scroll.clone())
        .h(px((count.clamp(1, 12) * 32) as f32));

        let panel = div()
            .id("switcher")
            .mt(px(56.))
            .w(px(520.))
            .max_w(gpui::relative(0.9))
            .p_1()
            .flex()
            .flex_col()
            .gap_1()
            .rounded(px(10.))
            .border_1()
            .border_color(rgb(ui().tab_active_border))
            .bg(rgb(ui().menu))
            .shadow_lg()
            .text_sm()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(div().p_1().child(switcher.search.clone()))
            .when(count == 0, |d| {
                d.child(
                    div()
                        .px_2()
                        .py_2()
                        .text_xs()
                        .text_color(rgb(ui().text_faint))
                        .child("Nothing matches"),
                )
            })
            .when(count > 0, |d| d.child(list));

        div()
            .id("switcher-backdrop")
            // Nothing behind an overlay gets the mouse, not even the scroll wheel.
            .occlude()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .justify_center()
            .items_start()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.close_switcher(cx)),
            )
            .child(panel)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(title: &str, detail: &str) -> Item {
        Item {
            title: title.into(),
            detail: detail.into(),
            target: Target::Session(title.into()),
            current: false,
        }
    }

    #[test]
    fn filters_by_every_word_in_title_and_session() {
        let items = vec![
            item("vim src/main.rs", "work"),
            item("htop", "work"),
            item("vim notes.md", "home"),
        ];
        let titles = |query| {
            filter(items.clone(), query)
                .into_iter()
                .map(|i| i.title)
                .collect::<Vec<_>>()
        };
        assert_eq!(titles("VIM"), ["vim src/main.rs", "vim notes.md"]);
        assert_eq!(titles("vim work"), ["vim src/main.rs"]);
        assert_eq!(titles("  "), ["vim src/main.rs", "htop", "vim notes.md"]);
        assert!(titles("nothing").is_empty());
    }
}
