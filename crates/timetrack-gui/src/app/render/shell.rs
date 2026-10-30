/* gui/app/render/shell.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The window shell around the tab content: the integrated titlebar (drag
//! handle, tab strip and window buttons in one) and the footer (status line,
//! or the start-service offer when the service is missing).

use super::super::TimetrackView;
use super::super::chrome::{maximize_glyph, titlebar_drag, window_button};
use super::super::tabs::Tab;
use super::super::theme;
use gpui::prelude::*;
use gpui::{ElementId, IntoElement, Window, WindowControlArea, div, rgba};

impl TimetrackView {
    /// The pre-snapshot placeholder every tab shows instead of data:
    /// until the first snapshot (or the first connection error) the
    /// view holds an empty default, and drawing it would claim the
    /// store is empty when nothing has been heard at all.
    pub(crate) fn render_connecting(&self) -> impl IntoElement {
        div()
            .flex_1()
            .px_6()
            .pt_6()
            .text_color(rgba(theme::HINT_TEXT))
            .child("Connecting to service…")
    }

    /// The integrated titlebar: the window has no system title bar (see
    /// `main.rs`), so this row is the drag handle, the tab strip and the
    /// minimize/maximize/close buttons in one.
    ///
    /// Only the title and the spacer drag. The tabs and buttons are siblings
    /// of those regions, never children, so pressing one can never start a
    /// move as a side effect of the event bubbling up to a draggable parent.
    pub(crate) fn render_titlebar(&self, window: &Window) -> impl IntoElement {
        let mut row = div()
            .flex()
            .items_center()
            .gap_1()
            .px_3()
            .py_2()
            .bg(rgba(theme::PANEL_BG))
            // The panel clip does not round children, so the titlebar
            // rounds its own top to meet the panel's corners. Square when
            // maximized, like the panel itself.
            .when(!window.is_maximized(), |el| el.rounded_t_xl());
        row = row.child(titlebar_drag("titlebar-drag", false, "TimeTrack"));
        for tab in Tab::all() {
            let is_selected = tab == self.tab;
            row = row.child(
                div()
                    .id(ElementId::from(tab.label()))
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .bg(if is_selected {
                        rgba(theme::RAISED_BG)
                    } else {
                        rgba(theme::TRANSPARENT)
                    })
                    .text_color(if is_selected {
                        rgba(theme::SELECTED_TEXT)
                    } else {
                        rgba(theme::MUTED_TEXT)
                    })
                    .child(tab.label())
                    .when(!is_selected, move |el| {
                        let entity = self.entity.clone();
                        el.on_click(move |_ev, _window, cx| {
                            if let Some(entity) = entity.clone() {
                                entity.update(cx, |view, cx| {
                                    view.tab = tab;
                                    cx.notify();
                                });
                            }
                        })
                    }),
            );
        }
        row = row.child(titlebar_drag("titlebar-spacer", true, ""));
        row = row
            .child(window_button(
                "window-min",
                "−",
                WindowControlArea::Min,
                false,
                |window| window.minimize_window(),
            ))
            .child(window_button(
                "window-max",
                maximize_glyph(window.is_maximized()),
                WindowControlArea::Max,
                false,
                |window| window.zoom_window(),
            ))
            .child(window_button(
                "window-close",
                "×",
                WindowControlArea::Close,
                true,
                |window| window.remove_window(),
            ));
        row
    }

    pub(crate) fn render_footer(&self) -> impl IntoElement {
        if self.service_missing() {
            let Some(this) = self.entity.clone() else {
                return div().into_any_element();
            };
            return div()
                .flex()
                .flex_col()
                .gap_2()
                .px_6()
                .py_3()
                .child(
                    div().text_sm().text_color(rgba(theme::STRONG_TEXT)).child(
                        self.status
                            .as_ref()
                            .map(|s| s.text.clone())
                            .unwrap_or_default(),
                    ),
                )
                .child(
                    div()
                        .id("start-service")
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .cursor_pointer()
                        .bg(rgba(theme::ACTION))
                        .text_color(rgba(theme::SELECTED_TEXT))
                        .on_click(move |_ev, _window, cx| {
                            this.update(cx, |view, cx| {
                                view.try_start_service();
                                cx.notify();
                            });
                        })
                        .child("Start the service"),
                )
                .into_any_element();
        }
        div()
            .px_6()
            .py_2()
            .text_sm()
            .text_color(rgba(theme::HINT_TEXT))
            .child(self.footer())
            .into_any_element()
    }
}
