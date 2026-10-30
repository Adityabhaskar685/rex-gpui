//! A small single-line text field, enough for a host's label and address.
//!
//! It takes typed characters from key events rather than through an input method, so it
//! suits ASCII input such as host names, not composed text.

use crate::theme::ui;
use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, KeyDownEvent, MouseButton, SharedString,
    Window, div, prelude::*, px, rgb,
};

pub enum TextFieldEvent {
    Submit,
    Cancel,
}

pub struct TextField {
    pub text: String,
    placeholder: SharedString,
    focus_handle: FocusHandle,
}

impl EventEmitter<TextFieldEvent> for TextField {}

impl TextField {
    pub fn new(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        Self {
            text: String::new(),
            placeholder: placeholder.into(),
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        self.text = text.into();
        self
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ks = &event.keystroke;
        let m = &ks.modifiers;
        match ks.key.as_str() {
            "enter" => cx.emit(TextFieldEvent::Submit),
            "escape" => cx.emit(TextFieldEvent::Cancel),
            "backspace" if m.control => self.text.clear(),
            "backspace" => {
                self.text.pop();
            }
            "v" if m.control => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    self.text.push_str(text.trim());
                }
            }
            "tab" => return,
            _ => match ks.key_char.as_deref() {
                Some(text) if !m.control && !m.platform => self.text.push_str(text),
                _ => return,
            },
        }
        cx.stop_propagation();
        cx.notify();
    }
}

impl Focusable for TextField {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TextField {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus_handle.is_focused(window);
        let empty = self.text.is_empty();
        div()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, _| window.focus(&this.focus_handle)),
            )
            .flex()
            .items_center()
            .h(px(32.))
            .px_2()
            .rounded(px(7.))
            .border_1()
            .border_color(rgb(if focused {
                ui().accent
            } else {
                ui().tab_active_border
            }))
            .bg(rgb(ui().window))
            .text_sm()
            .cursor_text()
            .child(
                div()
                    .text_color(rgb(if empty { ui().text_faint } else { ui().text }))
                    .child(if empty && !focused {
                        self.placeholder.to_string()
                    } else {
                        self.text.clone()
                    }),
            )
            .when(focused, |d| {
                d.child(div().w(px(1.5)).h(px(16.)).ml(px(1.)).bg(rgb(ui().text)))
            })
    }
}
