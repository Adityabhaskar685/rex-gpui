//! Switching between Rex servers: the host menu under the title bar's host chip, and the
//! Add Remote Host dialog.
//!
//! Saved hosts live on the local server (`hosts.list`, `hosts.put`), the same list that
//! `rex hosts` edits, so a host added here works with `rex -S LABEL` and the other way round.

use gpui::{
    AppContext as _, Context, Entity, Focusable, FontWeight, MouseButton, SharedString,
    Subscription, div, prelude::*, px, rgb,
};
use serde_json::{Value, json};

use super::{Workspace, icon};
use crate::rex::control::Control;
use crate::rex::endpoint::Endpoint;
use crate::text_field::{TextField, TextFieldEvent};
use crate::theme::ui;

#[derive(Clone, Debug)]
pub struct Host {
    pub id: String,
    pub label: String,
    pub endpoints: Vec<String>,
}

impl Host {
    fn from_value(v: &Value) -> Option<Self> {
        Some(Self {
            id: v["id"].as_str()?.to_string(),
            label: v["label"].as_str()?.to_string(),
            endpoints: v["endpoints"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|e| e.as_str().map(str::to_string))
                .collect(),
        })
    }
}

pub struct AddHostDialog {
    label: Entity<TextField>,
    address: Entity<TextField>,
    error: Option<String>,
    busy: bool,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    /// The second line of the host chip: the saved host's label, or this machine's name.
    pub(super) fn host_title(&self) -> String {
        match &self.current_host {
            Some(host) => host.label.clone(),
            None => self.host_name.clone(),
        }
    }

    /// The local server, which keeps the host list. Connects to it once when this window
    /// shows a remote server.
    async fn home(
        this: &gpui::WeakEntity<Self>,
        cx: &mut gpui::AsyncApp,
    ) -> anyhow::Result<Control> {
        let existing = this.update(cx, |this, _| {
            if this.control.endpoint.is_local() {
                Some(this.control.clone())
            } else {
                this.home.clone()
            }
        })?;
        if let Some(control) = existing {
            return Ok(control);
        }
        let (control, _events) = crate::runtime()
            .spawn(async { Control::connect(&[Endpoint::local()], true).await })
            .await??;
        this.update(cx, |this, _| this.home = Some(control.clone()))?;
        Ok(control)
    }

    pub(super) fn load_hosts(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let result = async {
                let home = Self::home(&this, cx).await?;
                home.call("hosts.list", json!({})).await
            }
            .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(reply) => {
                        this.hosts = reply["hosts"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Host::from_value)
                            .collect();
                        if let Some(label) = this.pending_host_label.take() {
                            this.current_host = this
                                .hosts
                                .iter()
                                .find(|h| h.label.eq_ignore_ascii_case(&label))
                                .cloned();
                        }
                    }
                    Err(err) => this.notice = Some(format!("Can't read saved hosts: {err:#}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn toggle_host_menu(&mut self, cx: &mut Context<Self>) {
        self.host_menu = !self.host_menu;
        self.confirm_remove = None;
        if self.host_menu {
            self.notice = None;
            self.refresh_sessions(cx);
            self.load_hosts(cx);
        }
        cx.notify();
    }

    /// Connect this window to `host`, or to the local server for `None`, and leave the
    /// current server.
    fn switch_host(&mut self, host: Option<Host>, cx: &mut Context<Self>) {
        let endpoints = match &host {
            None => Ok(vec![Endpoint::local()]),
            Some(host) => host.endpoints.iter().try_fold(Vec::new(), |mut all, e| {
                all.extend(Endpoint::candidates(e)?);
                anyhow::Ok(all)
            }),
        };
        let endpoints = match endpoints {
            Ok(endpoints) => endpoints,
            Err(err) => {
                self.notice = Some(format!("{err:#}"));
                cx.notify();
                return;
            }
        };
        let name = host
            .as_ref()
            .map_or_else(|| self.host_name.clone(), |h| h.label.clone());
        self.notice = Some(format!("Connecting to {name}…"));
        cx.notify();

        let local = host.is_none();
        cx.spawn(async move |this, cx| {
            let result = crate::runtime()
                .spawn(async move { Control::connect(&endpoints, local).await })
                .await;
            let result = match result {
                Ok(result) => result,
                Err(err) => Err(err.into()),
            };
            let _ = this.update(cx, |this, cx| match result {
                Ok((control, events)) => {
                    let old = std::mem::replace(&mut this.control, control);
                    // Keep the local connection for the host list rather than closing it.
                    if old.endpoint.is_local() {
                        this.home = Some(old);
                    } else {
                        old.close();
                    }
                    if this.control.endpoint.is_local() {
                        this.home = None;
                    }
                    this.host_name = this.control.endpoint.host_name();
                    // The next launch without -S comes back to this host.
                    let label = host.as_ref().map(|h| h.label.clone());
                    if let Err(err) = crate::settings::set("last_host", serde_json::json!(label)) {
                        log::warn!("saving the last host: {err:#}");
                    }
                    this.current_host = host;
                    this.panes.clear();
                    this.view = None;
                    this.sessions.clear();
                    this.focused_block = None;
                    this.error = None;
                    this.notice = None;
                    this.host_menu = false;
                    this.add_host = None;
                    this.start(events, None, cx);
                    cx.notify();
                }
                Err(err) => {
                    let message = format!("Can't connect to {name}: {err:#}");
                    match &mut this.add_host {
                        Some(dialog) => {
                            dialog.busy = false;
                            dialog.error = Some(message);
                        }
                        None => this.notice = Some(message),
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn remove_host(&mut self, id: String, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let result = async {
                let home = Self::home(&this, cx).await?;
                home.call("hosts.remove", json!({"id": id})).await
            }
            .await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(_) => this.load_hosts(cx),
                Err(err) => {
                    this.notice = Some(format!("{err:#}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn open_add_host(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) {
        let label = cx.new(|cx| TextField::new("devbox", cx));
        let address =
            cx.new(|cx| TextField::new("devbox.tailnet.ts.net or http://10.0.0.5:7297", cx));
        let subscriptions = [&label, &address]
            .into_iter()
            .map(|field| {
                cx.subscribe_in(
                    field,
                    window,
                    |this, _, event: &TextFieldEvent, window, cx| match event {
                        TextFieldEvent::Submit => this.submit_add_host(window, cx),
                        TextFieldEvent::Cancel => {
                            this.add_host = None;
                            cx.notify();
                        }
                    },
                )
            })
            .collect();
        window.focus(&label.focus_handle(cx));
        self.add_host = Some(AddHostDialog {
            label,
            address,
            error: None,
            busy: false,
            _subscriptions: subscriptions,
        });
        self.host_menu = false;
        cx.notify();
    }

    /// Save the host on the local server, then connect to it.
    fn submit_add_host(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) {
        let Some(dialog) = &mut self.add_host else {
            return;
        };
        let label = dialog.label.read(cx).text.trim().to_string();
        let address = dialog.address.read(cx).text.trim().to_string();
        // Enter in the label field moves on to the address.
        if !label.is_empty() && address.is_empty() {
            window.focus(&dialog.address.focus_handle(cx));
            return;
        }
        let problem = if label.is_empty() {
            Some("Give the host a name.".to_string())
        } else if let Err(err) = Endpoint::candidates(&address) {
            Some(format!("{err:#}"))
        } else if address.starts_with('/') || address.starts_with("unix://") {
            Some("A remote host needs a network address, not a socket path.".into())
        } else {
            None
        };
        if let Some(problem) = problem {
            dialog.error = Some(problem);
            cx.notify();
            return;
        }
        dialog.busy = true;
        dialog.error = None;
        cx.notify();

        let added_from = Endpoint::local().host_name();
        cx.spawn(async move |this, cx| {
            let result = async {
                let home = Self::home(&this, cx).await?;
                home.call(
                    "hosts.put",
                    json!({"host": {"label": label, "endpoints": [address], "added_from": added_from}}),
                )
                .await
            }
            .await;
            let _ = this.update(cx, |this, cx| match result.map(|reply| Host::from_value(&reply["host"])) {
                Ok(Some(host)) => {
                    this.load_hosts(cx);
                    this.switch_host(Some(host), cx);
                }
                Ok(None) => {}
                Err(err) => {
                    if let Some(dialog) = &mut this.add_host {
                        dialog.busy = false;
                        dialog.error = Some(format!("{err:#}"));
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn render_host_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current_session = self.session_id();
        let item = |id: SharedString| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap_2()
                .h(px(30.))
                .px_2()
                .rounded(px(7.))
                .cursor_pointer()
                .hover(|d| d.bg(rgb(ui().hover)))
        };
        let heading = |text: String| {
            div()
                .px_2()
                .pt_2()
                .pb_1()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(ui().text_faint))
                .child(text)
        };
        let check = |on: bool| {
            div()
                .w(px(14.))
                .flex_none()
                .text_color(rgb(ui().text))
                .child(if on { "✓" } else { "" })
        };

        let mut menu = div()
            .id("host-menu")
            .absolute()
            .top(px(42.))
            .left(px(40.))
            .w(px(320.))
            .max_h(px(520.))
            .overflow_y_scroll()
            .p_1()
            .flex()
            .flex_col()
            .rounded(px(10.))
            .border_1()
            .border_color(rgb(ui().tab_active_border))
            .bg(rgb(ui().menu))
            .shadow_lg()
            .text_sm()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(heading(format!("Sessions on {}", self.host_title())));

        for session in &self.sessions {
            let is_current = Some(&session.session_id) == current_session.as_ref();
            let session_id = session.session_id.clone();
            let label = if session.label.is_empty() {
                session.session_id.clone()
            } else {
                session.label.clone()
            };
            menu = menu.child(
                item(SharedString::from(format!("menu-{}", session.session_id)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.host_menu = false;
                        if !is_current {
                            this.attach(session_id.clone(), cx);
                        }
                        cx.notify();
                    }))
                    .child(check(is_current))
                    .child(div().truncate().child(label)),
            );
        }
        menu = menu
            .child(
                item("menu-new-session".into())
                    .text_color(rgb(ui().text_dim))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.host_menu = false;
                        this.new_session(&super::NewSession, window, cx);
                    }))
                    .child(check(false))
                    .child("New Session"),
            )
            .child(
                item("menu-rename-session".into())
                    .text_color(rgb(ui().text_dim))
                    .on_click(cx.listener(|this, _, window, cx| this.open_rename(window, cx)))
                    .child(check(false))
                    .child(div().flex_1().child("Rename Session…"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(ui().text_faint))
                            .child("Ctrl+Shift+R"),
                    ),
            )
            .child(div().h(px(1.)).mx_2().my_1().bg(rgb(ui().card_border)))
            .child(heading("Hosts".into()))
            .child(
                item("menu-local".into())
                    .on_click(cx.listener(|this, _, _, cx| {
                        if this.current_host.is_some() {
                            this.switch_host(None, cx);
                        } else {
                            this.host_menu = false;
                            cx.notify();
                        }
                    }))
                    .child(check(self.current_host.is_none()))
                    .child(icon("icons/server.svg", 14., ui().text_dim))
                    .child(div().flex_1().child(Endpoint::local().host_name()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(ui().text_faint))
                            .child("this machine"),
                    ),
            );

        for host in &self.hosts {
            let is_current = self.current_host.as_ref().is_some_and(|h| h.id == host.id);
            let connect = host.clone();
            let remove = host.id.clone();
            let confirming = self.confirm_remove.as_ref() == Some(&host.id);
            menu = menu.child(
                item(SharedString::from(format!("menu-{}", host.id)))
                    .group(SharedString::from(format!("menu-{}", host.id)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if is_current {
                            this.host_menu = false;
                            cx.notify();
                        } else {
                            this.switch_host(Some(connect.clone()), cx);
                        }
                    }))
                    .child(check(is_current))
                    .child(icon("icons/server.svg", 14., ui().text_dim))
                    .child(div().flex_none().child(host.label.clone()))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_xs()
                            .text_color(rgb(ui().text_faint))
                            .child(host.endpoints.first().cloned().unwrap_or_default()),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("remove-{}", host.id)))
                            .invisible()
                            .group_hover(SharedString::from(format!("menu-{}", host.id)), |d| {
                                d.visible()
                            })
                            .px_1()
                            .rounded(px(4.))
                            .text_color(rgb(ui().text_faint))
                            .hover(|d| d.text_color(rgb(ui().danger)))
                            .when(confirming, |d| d.visible().text_color(rgb(ui().danger)))
                            // The first click asks; a second one removes.
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                if this.confirm_remove.as_ref() == Some(&remove) {
                                    this.confirm_remove = None;
                                    this.remove_host(remove.clone(), cx);
                                } else {
                                    this.confirm_remove = Some(remove.clone());
                                    cx.notify();
                                }
                            }))
                            .child(if confirming { "Remove?" } else { "×" }),
                    ),
            );
        }

        menu = menu.child(
            item("menu-add-host".into())
                .text_color(rgb(ui().text_dim))
                .on_click(cx.listener(|this, _, window, cx| this.open_add_host(window, cx)))
                .child(check(false))
                .child(icon("icons/plus.svg", 14., ui().text_dim))
                .child("Add Remote Host…"),
        );
        if let Some(notice) = &self.notice {
            menu = menu.child(
                div()
                    .px_2()
                    .py_2()
                    .text_xs()
                    .text_color(rgb(ui().text_dim))
                    .child(notice.clone()),
            );
        }

        // A backdrop that closes the menu when clicked outside it.
        div()
            .id("host-menu-backdrop")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.host_menu = false;
                    cx.notify();
                }),
            )
            .child(menu)
    }

    pub(super) fn render_add_host(
        &self,
        dialog: &AddHostDialog,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let field = |label: &'static str, input: Entity<TextField>| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().text_xs().text_color(rgb(ui().text_dim)).child(label))
                .child(input)
        };
        let button = |id: &'static str, text: &'static str, primary: bool| {
            div()
                .id(id)
                .px_3()
                .py_1()
                .rounded(px(7.))
                .text_sm()
                .cursor_pointer()
                .map(|d| {
                    if primary {
                        d.bg(rgb(ui().accent))
                            .text_color(rgb(0xffffff))
                            .hover(|d| d.opacity(0.9))
                    } else {
                        d.bg(rgb(ui().tab_active))
                            .text_color(rgb(ui().text))
                            .hover(|d| d.bg(rgb(ui().tab_active_border)))
                    }
                })
                .child(text)
        };

        div()
            .id("add-host-backdrop")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::black().opacity(0.45))
            // A click outside the dialog closes it; one inside stays there.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.add_host = None;
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .child(
                div()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .w(px(420.))
                    .p_4()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .rounded(px(12.))
                    .border_1()
                    .border_color(rgb(ui().tab_active_border))
                    .bg(rgb(ui().menu))
                    .shadow_lg()
                    .child(
                        div()
                            .text_base()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Add Remote Host"),
                    )
                    .child(div().text_xs().text_color(rgb(ui().text_dim)).child(
                        "A Rex server with a network listener, such as a VM on your tailnet. \
                         The host is saved to the same list as `rex hosts`.",
                    ))
                    .child(field("Name", dialog.label.clone()))
                    .child(field("Address", dialog.address.clone()))
                    .when_some(dialog.error.clone(), |d, error| {
                        d.child(div().text_xs().text_color(rgb(ui().danger)).child(error))
                    })
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(button("add-host-cancel", "Cancel", false).on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.add_host = None;
                                    cx.notify();
                                }),
                            ))
                            .child(
                                button(
                                    "add-host-submit",
                                    if dialog.busy {
                                        "Connecting…"
                                    } else {
                                        "Add & Connect"
                                    },
                                    true,
                                )
                                .on_click(cx.listener(
                                    |this, _, window, cx| this.submit_add_host(window, cx),
                                )),
                            ),
                    ),
            )
    }
}
