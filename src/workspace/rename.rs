//! Renaming the attached session with `session.set_label`.

use gpui::{
    AppContext as _, Context, Entity, Focusable, FontWeight, MouseButton, Subscription, Window,
    div, prelude::*, px, rgb,
};
use serde_json::json;

use super::Workspace;
use crate::text_field::{TextField, TextFieldEvent};
use crate::theme::ui;

pub struct RenameDialog {
    field: Entity<TextField>,
    error: Option<String>,
    _subscription: Subscription,
}

impl Workspace {
    pub(super) fn open_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = &self.view else { return };
        let label = view.label.clone();
        let field = cx.new(|cx| TextField::new("Session name", cx).with_text(label));
        let subscription = cx.subscribe_in(
            &field,
            window,
            |this, _, event: &TextFieldEvent, _, cx| match event {
                TextFieldEvent::Submit => this.submit_rename(cx),
                TextFieldEvent::Cancel => {
                    this.rename = None;
                    cx.notify();
                }
            },
        );
        window.focus(&field.focus_handle(cx));
        self.rename = Some(RenameDialog {
            field,
            error: None,
            _subscription: subscription,
        });
        self.host_menu = false;
        cx.notify();
    }

    fn submit_rename(&mut self, cx: &mut Context<Self>) {
        let session_id = self.session_id();
        let (Some(dialog), Some(session_id)) = (&self.rename, session_id) else {
            return;
        };
        let label = dialog.field.read(cx).text.trim().to_string();
        let control = self.control.clone();
        cx.spawn(async move |this, cx| {
            // An empty label gives the session back its default name.
            let result = control
                .call(
                    "session.set_label",
                    json!({"session_id": session_id, "label": label}),
                )
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(_) => {
                        this.rename = None;
                        this.refresh_sessions(cx);
                        this.refresh_view(cx);
                    }
                    Err(err) => {
                        if let Some(dialog) = &mut this.rename {
                            dialog.error = Some(format!("{err:#}"));
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn render_rename(
        &self,
        dialog: &RenameDialog,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
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
            .id("rename-backdrop")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::black().opacity(0.45))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .w(px(360.))
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
                            .child("Rename Session"),
                    )
                    .child(dialog.field.clone())
                    .when_some(dialog.error.clone(), |d, error| {
                        d.child(div().text_xs().text_color(rgb(ui().danger)).child(error))
                    })
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                button("rename-cancel", "Cancel", false).on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.rename = None;
                                        cx.notify();
                                    },
                                )),
                            )
                            .child(
                                button("rename-submit", "Rename", true)
                                    .on_click(cx.listener(|this, _, _, cx| this.submit_rename(cx))),
                            ),
                    ),
            )
    }
}
