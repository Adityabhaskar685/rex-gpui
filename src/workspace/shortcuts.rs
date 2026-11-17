//! The keyboard shortcuts overlay, opened with ctrl-shift-k or the title bar's keyboard
//! button. It lists the window's shortcuts from `SHORTCUTS` and the terminal's from
//! `terminal::SHORTCUTS`, so it always matches what the keys do.

use gpui::{Context, FontWeight, MouseButton, Window, div, prelude::*, px, rgb};

use super::{SHORTCUTS, ToggleShortcuts, Workspace};
use crate::theme::ui;

impl Workspace {
    pub(super) fn toggle_shortcuts(
        &mut self,
        _: &ToggleShortcuts,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.shortcuts = !self.shortcuts;
        if self.shortcuts {
            self.host_menu = false;
            // Take the keyboard from the terminal, so Escape closes this.
            window.focus(&self.focus_handle);
        } else {
            // Hand the keyboard back to the focused pane on the next frame.
            self.focused_block = None;
        }
        cx.notify();
    }

    pub(super) fn render_shortcuts(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let groups = SHORTCUTS
            .iter()
            .copied()
            .chain([("Terminal", crate::terminal::SHORTCUTS)]);
        let mut columns = div().flex().flex_row().flex_wrap().gap_x_8().gap_y_4();
        for (group, entries) in groups {
            let mut list = div().w(px(300.)).flex().flex_col().gap_1().child(
                div()
                    .pb_1()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(ui().text_faint))
                    .child(group),
            );
            for &(keys, what) in entries {
                list = list.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_4()
                        .child(div().text_sm().text_color(rgb(ui().text)).child(what))
                        .child(keycaps(keys)),
                );
            }
            columns = columns.child(list);
        }

        div()
            .id("shortcuts-backdrop")
            // Nothing behind an overlay gets the mouse, not even the scroll wheel.
            .occlude()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::black().opacity(0.45))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.toggle_shortcuts(&ToggleShortcuts, window, cx);
                }),
            )
            .child(
                div()
                    .id("shortcuts")
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .max_w(px(700.))
                    .max_h(gpui::relative(0.85))
                    .overflow_y_scroll()
                    .p_5()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .rounded(px(12.))
                    .border_1()
                    .border_color(rgb(ui().tab_active_border))
                    .bg(rgb(ui().menu))
                    .shadow_lg()
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .items_center()
                            .child(div().text_base().font_weight(FontWeight::SEMIBOLD).child("Keyboard Shortcuts"))
                            .child(div().text_xs().text_color(rgb(ui().text_faint)).child("Esc to close")),
                    )
                    .child(columns)
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(ui().text_faint))
                            .child("Other keys go to the program in the terminal, and to your Rex config's key bindings."),
                    ),
            )
    }
}

/// Keys such as `ctrl-shift-t` drawn as key caps: Ctrl, Shift, T.
fn keycaps(keys: &str) -> impl IntoElement {
    div()
        .flex()
        .flex_none()
        .gap_1()
        .children(caps(keys).into_iter().map(|key| {
            div()
                .px_1p5()
                .py_0p5()
                .rounded(px(5.))
                .border_1()
                .border_color(rgb(ui().tab_active_border))
                .bg(rgb(ui().tab_active))
                .text_xs()
                .text_color(rgb(ui().text_dim))
                .child(key)
        }))
}

/// The caps for `keys`: one per modifier, then one for the key or mouse action, as in
/// `ctrl-shift-t` → Ctrl, Shift, T and `double-click` → Double-click.
fn caps(keys: &str) -> Vec<String> {
    let mut caps = Vec::new();
    let mut rest = keys;
    while let Some((first, after)) = rest.split_once('-') {
        let modifier = match first {
            "ctrl" => "Ctrl",
            "shift" => "Shift",
            "alt" => "Alt",
            _ => break,
        };
        caps.push(modifier.to_string());
        rest = after;
    }
    caps.push(match rest {
        "pageup" => "Page Up".into(),
        "pagedown" => "Page Down".into(),
        k if k.chars().count() == 1 => k.to_uppercase(),
        k => capitalize(k),
    });
    caps
}

fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::caps;

    #[test]
    fn names_keys() {
        assert_eq!(caps("ctrl-shift-t"), ["Ctrl", "Shift", "T"]);
        assert_eq!(caps("shift-pageup"), ["Shift", "Page Up"]);
        assert_eq!(caps("ctrl-tab"), ["Ctrl", "Tab"]);
        assert_eq!(caps("double-click"), ["Double-click"]);
        assert_eq!(caps("shift-drag"), ["Shift", "Drag"]);
    }
}
