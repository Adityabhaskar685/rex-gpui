//! The theme picker under the title bar's palette button: a search field over every theme,
//! built-in and Ghostty, and a button to import Ghostty theme files.

use std::sync::Arc;

use gpui::{
    AppContext as _, Context, Entity, Focusable, MouseButton, PathPromptOptions, SharedString,
    Subscription, Window, div, prelude::*, px, rgb, uniform_list,
};

use super::Workspace;
use crate::text_field::{TextField, TextFieldEvent};
use crate::theme::{self, AppTheme, Source, ui};

pub struct ThemeMenu {
    search: Entity<TextField>,
    /// A message from the last import, shown under the list.
    notice: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub(super) fn toggle_theme_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.theme_menu.take().is_some() {
            cx.notify();
            return;
        }
        let search = cx.new(|cx| TextField::new("Search themes", cx));
        let subscriptions = vec![
            // Redraw the list as the search changes.
            cx.observe(&search, |_, _, cx| cx.notify()),
            cx.subscribe_in(
                &search,
                window,
                |this, _, event: &TextFieldEvent, window, cx| match event {
                    TextFieldEvent::Submit => {
                        if let Some(first) = this.matching_themes(cx).first().cloned() {
                            this.apply_theme(&first.name, window, cx);
                        }
                    }
                    TextFieldEvent::Cancel => {
                        this.theme_menu = None;
                        cx.notify();
                    }
                },
            ),
        ];
        window.focus(&search.focus_handle(cx));
        self.theme_menu = Some(ThemeMenu {
            search,
            notice: None,
            _subscriptions: subscriptions,
        });
        self.host_menu = false;
        cx.notify();
    }

    fn matching_themes(&self, cx: &gpui::App) -> Vec<Arc<AppTheme>> {
        let query = self
            .theme_menu
            .as_ref()
            .map(|menu| menu.search.read(cx).text.to_lowercase())
            .unwrap_or_default();
        let words: Vec<&str> = query.split_whitespace().collect();
        theme::all()
            .into_iter()
            .filter(|t| {
                let name = t.name.to_lowercase();
                words.iter().all(|w| name.contains(w))
            })
            .collect()
    }

    fn apply_theme(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        theme::select(name);
        self.theme_menu = None;
        // Every view reads the theme while it renders.
        window.refresh();
        cx.notify();
    }

    fn import_themes(&mut self, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Import".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = chosen.await else {
                return;
            };
            let result = theme::import(&paths);
            let _ = this.update(cx, |this, cx| {
                let notice = match &result {
                    Ok(name) => {
                        theme::select(name);
                        format!(
                            "Imported {} theme{}; now using {name}.",
                            paths.len(),
                            if paths.len() == 1 { "" } else { "s" }
                        )
                    }
                    Err(err) => format!("{err:#}"),
                };
                if let Some(menu) = &mut this.theme_menu {
                    menu.notice = Some(notice);
                }
                cx.notify();
            });
            // The new theme changes every view.
            let _ = cx.update(|cx| cx.refresh_windows());
        })
        .detach();
    }

    pub(super) fn render_theme_menu(
        &self,
        menu: &ThemeMenu,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let themes = self.matching_themes(cx);
        let count = themes.len();
        let current = theme::current().name.clone();

        let list = uniform_list(
            "theme-list",
            count,
            cx.processor(move |_this, range: std::ops::Range<usize>, _window, cx| {
                range
                    .map(|i| {
                        let t = themes[i].clone();
                        let name = t.name.to_string();
                        let is_current = t.name == current;
                        // The background with a few of the theme's colors on it.
                        let swatch = div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap(px(2.))
                            .px(px(4.))
                            .h(px(16.))
                            .rounded(px(4.))
                            .border_1()
                            .border_color(rgb(ui().card_border))
                            .bg(rgb(t.term.background))
                            .children([1, 2, 3, 4, 5].map(|c| {
                                div().size(px(6.)).rounded_full().bg(rgb(t.term.ansi[c]))
                            }));
                        div()
                            .id(SharedString::from(format!("theme-{name}")))
                            .flex()
                            .items_center()
                            .gap_2()
                            .h(px(30.))
                            .px_2()
                            .rounded(px(7.))
                            .cursor_pointer()
                            .text_color(rgb(ui().text))
                            .hover(|d| d.bg(rgb(ui().hover)))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.apply_theme(&name, window, cx)
                            }))
                            .child(div().w(px(14.)).flex_none().child(if is_current {
                                "✓"
                            } else {
                                ""
                            }))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .truncate()
                                    .child(t.name.to_string()),
                            )
                            .when(matches!(t.source, Source::File(_)), |d| {
                                d.child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(ui().text_faint))
                                        .child("Ghostty"),
                                )
                            })
                            .child(swatch)
                    })
                    .collect()
            }),
        )
        .h(px(380.));

        let panel = div()
            .id("theme-menu")
            .absolute()
            .top(px(42.))
            .right(px(110.))
            .w(px(320.))
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
            .child(div().p_1().child(menu.search.clone()))
            .child(
                div()
                    .px_2()
                    .text_xs()
                    .text_color(rgb(ui().text_faint))
                    .child(format!(
                        "{count} theme{}",
                        if count == 1 { "" } else { "s" }
                    )),
            )
            .child(list)
            .child(div().h(px(1.)).mx_2().bg(rgb(ui().card_border)))
            .child(
                div()
                    .id("theme-import")
                    .flex()
                    .items_center()
                    .h(px(30.))
                    .px_2()
                    .rounded(px(7.))
                    .cursor_pointer()
                    .text_color(rgb(ui().text_dim))
                    .hover(|d| d.bg(rgb(ui().hover)))
                    .on_click(cx.listener(|this, _, _, cx| this.import_themes(cx)))
                    .child("Import Ghostty theme…"),
            )
            .when_some(menu.notice.clone(), |d, notice| {
                d.child(
                    div()
                        .px_2()
                        .pb_1()
                        .text_xs()
                        .text_color(rgb(ui().text_dim))
                        .child(notice),
                )
            });

        div()
            .id("theme-menu-backdrop")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.theme_menu = None;
                    cx.notify();
                }),
            )
            .child(panel)
    }
}
