/* gui/app/render/export.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The Export tab. The service owns aggregation, so what is shown here is
//! exactly what a CSV would contain, rather than a second aggregation that
//! could disagree with the totals on Home.

use super::super::TimetrackView;
use super::super::export::save::{export_location_text, export_preview_entries};
use super::super::service::{Action, export_request, now_ms};
use super::super::status::StatusMsg;
use super::super::{display_tz, snapshot_tz, theme};
use gpui::prelude::*;
use gpui::{ElementId, IntoElement, div, rgba};

impl TimetrackView {
    /// Export. The service owns aggregation, so what is shown here is exactly
    /// what a CSV would contain, rather than a second aggregation that could
    /// disagree with the totals on Home.
    pub(crate) fn render_export(&self) -> impl IntoElement {
        if self.connecting() {
            return self.render_connecting().into_any_element();
        }
        // Drawing only (labels and preview): the export click below
        // re-resolves strictly before anything is computed or sent.
        let tz = display_tz(&self.snapshot);
        div()
            .id("export-scroll")
            .flex_1()
            .flex_col()
            .px_6()
            .py_2()
            .overflow_y_scroll()
            .child(
                div()
                    .text_sm()
                    .text_color(rgba(theme::MUTED_TEXT))
                    .child("EXPORT"),
            )
            .child(
                div()
                    .pt_2()
                    .text_color(rgba(theme::STRONG_TEXT))
                    .child(format!(
                        "{} entries · {} this week · {} all time",
                        self.snapshot.entries.len(),
                        timetrack_core::format_duration(self.snapshot.week.total_ms),
                        timetrack_core::format_duration(self.snapshot.total_ms),
                    )),
            )
            .child(self.render_export_scope())
            .child(self.render_export_anchor(&tz))
            .child(self.render_export_actions())
            .child(self.render_export_preview(&tz))
            .into_any_element()
    }

    /// The SCOPE section: the three pills following the picker's scope.
    fn render_export_scope(&self) -> impl IntoElement {
        // Scope plus anchor: the stepper and the typed day edit one shared
        // anchor, and all time visibly ignores it. The preview and the
        // export buttons below follow this scope plus anchor.
        let mut section = div().child(
            div()
                .pt_4()
                .text_sm()
                .text_color(rgba(theme::MUTED_TEXT))
                .child("SCOPE"),
        );
        let mut row = div().flex().gap_2().pt_1();
        for (scope, id, label) in [
            (timetrack_core::ExportScope::Week, "scope-week", "Week"),
            (timetrack_core::ExportScope::Month, "scope-month", "Month"),
            (timetrack_core::ExportScope::All, "scope-all", "All"),
        ] {
            row = row.child(self.render_scope_pill(scope, id, label));
        }
        section = section.child(row);
        section
    }

    /// The ANCHOR section: the stepper, the typed day and its hint, plus any
    /// error from a bad day.
    fn render_export_anchor(&self, tz: &timetrack_core::Tz) -> impl IntoElement {
        let picker_active = self.export_picker.is_active();
        let mut section = div().child(
            div()
                .pt_3()
                .text_sm()
                .text_color(rgba(theme::MUTED_TEXT))
                .child("ANCHOR"),
        );
        {
            let mut row = div().flex().items_center().gap_2().pt_1();
            row = row.child(self.render_step_button("anchor-prev", "< prev", -1));
            row = row.child(
                div()
                    .px_3()
                    .py_1()
                    .text_color(if picker_active {
                        rgba(theme::SELECTED_TEXT)
                    } else {
                        rgba(theme::HINT_TEXT)
                    })
                    .child(self.export_picker.period_label(tz)),
            );
            row = row.child(self.render_step_button("anchor-next", "next >", 1));
            section = section.child(row);
        }
        {
            let field_text = if self.export_picker.editing {
                super::super::textline::render_with_caret(
                    &self.export_picker.anchor_text,
                    &self.export_picker.caret,
                )
            } else {
                self.export_picker.anchor_text.clone()
            };
            let entity = self.entity.clone();
            let mut field = div()
                .id(ElementId::from("anchor-field"))
                .flex_1()
                .px_2()
                .py_1()
                .rounded_md()
                .bg(if self.export_picker.editing {
                    rgba(theme::RAISED_BG)
                } else {
                    rgba(theme::TRANSPARENT)
                })
                .text_color(if picker_active {
                    rgba(theme::BODY_TEXT)
                } else {
                    rgba(theme::HINT_TEXT)
                })
                .child(format!("day {field_text}"));
            if picker_active {
                field = field.cursor_pointer().when_some(entity, |el, entity| {
                    el.on_click(move |_ev, _window, cx| {
                        entity.update(cx, |view, cx| {
                            view.export_picker.start_editing();
                            cx.notify();
                        });
                    })
                });
            } else {
                field = field.cursor_default();
            }
            section = section.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .pt_1()
                    .child(field)
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgba(theme::HINT_TEXT))
                            .child("(YYYY-MM-DD)"),
                    ),
            );
        }
        if picker_active {
            section = section.child(
                div()
                    .pt_1()
                    .text_sm()
                    .text_color(rgba(theme::HINT_TEXT))
                    .child("t type a day · [ previous period · ] next period"),
            );
        } else {
            section = section.child(
                div()
                    .pt_1()
                    .text_sm()
                    .text_color(rgba(theme::HINT_TEXT))
                    .child("anchor ignored for all time"),
            );
        }
        if let Some(err) = &self.export_picker.error {
            section = section.child(
                div()
                    .pt_1()
                    .text_color(rgba(theme::WARNING))
                    .child(err.clone()),
            );
        }
        section
    }

    /// The export actions: the CSV shape plus one button per scope and the
    /// location line naming where the last save went.
    fn render_export_actions(&self) -> impl IntoElement {
        let mut section = div();
        // The columns a CSV export will carry, shown as a header so the shape
        // is visible without opening a file.
        section = section.child(
            div()
                .pt_4()
                .text_sm()
                .text_color(rgba(theme::MUTED_TEXT))
                .child("CSV COLUMNS"),
        );
        section = section.child(
            div()
                .pt_1()
                .text_color(rgba(theme::ROW_TEXT))
                .child(timetrack_core::CSV_HEADER),
        );

        // Export actions: fetch the CSV over D-Bus and write it to a
        // period-stamped name the status line reports. Scopes match the
        // CLI's `timetrack export --scope week|month|all`; each button saves
        // its own scope with the shared anchor (all time ignores it), so the
        // buttons keep working while the preview follows the picker.
        let mut row = div().flex().gap_2().pt_3();
        for (i, button_scope) in [
            timetrack_core::ExportScope::Week,
            timetrack_core::ExportScope::Month,
            timetrack_core::ExportScope::All,
        ]
        .iter()
        .enumerate()
        {
            let entity = self.entity.clone();
            let button_scope = *button_scope;
            let label = format!("Export {}", button_scope.as_str());
            row = row.child(
                div()
                    .id(ElementId::from(("export", i as u64)))
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .bg(rgba(theme::ACTION))
                    .text_color(rgba(theme::SELECTED_TEXT))
                    .child(label)
                    .when_some(entity, move |el, entity| {
                        el.on_click(move |_ev, _window, cx| {
                            entity.update(cx, |view, cx| {
                                // Everything about the request is computed
                                // here, from one snapshot and one zone:
                                // the destination used to be computed at
                                // render time and captured, so a zone
                                // that changed between the last paint and
                                // the click would have put the file in a
                                // directory named for a different week.
                                match snapshot_tz(&view.snapshot) {
                                    Ok(tz) => {
                                        let request = export_request(
                                            button_scope,
                                            view.export_picker.anchor_ms,
                                            now_ms(),
                                            &tz,
                                        );
                                        view.send(Action::ExportCsv(request));
                                    }
                                    Err(e) => {
                                        view.status = Some(StatusMsg::local(e.to_string()));
                                    }
                                }
                                cx.notify();
                            });
                        })
                    }),
            );
        }
        section = section.child(row);
        section = section.child(
            div()
                .pt_1()
                .text_sm()
                .text_color(rgba(theme::HINT_TEXT))
                .child(export_location_text(
                    self.export_last_dest
                        .as_deref()
                        .map(|dest| (dest, self.export_last_via_portal)),
                )),
        );
        section
    }

    /// A preview of the first rows of the scoped export, so what is seen
    /// is what the stakeholder gets: the picker's scope plus anchor,
    /// oldest first (all time ignores the anchor).
    fn render_export_preview(&self, tz: &timetrack_core::Tz) -> impl IntoElement {
        let mut section = div().child(
            div()
                .pt_4()
                .text_sm()
                .text_color(rgba(theme::MUTED_TEXT))
                .child("PREVIEW"),
        );
        let preview = export_preview_entries(
            &self.snapshot,
            self.export_picker.scope,
            self.export_picker.anchor_ms,
            tz,
        );
        for e in preview.iter().take(20) {
            let project = self
                .snapshot
                .projects
                .iter()
                .find(|p| p.id == e.project_id)
                .map(|p| timetrack_core::sanitize_display(&p.name))
                .unwrap_or_else(|| e.project_id.clone());
            section = section.child(
                div()
                    .flex()
                    .gap_3()
                    .py_1()
                    .text_color(rgba(theme::ROW_TEXT))
                    .child(e.id.clone())
                    .child(project)
                    .child(timetrack_core::format_duration(e.duration_ms()))
                    .child(timetrack_core::sanitize_display(e.label())),
            );
        }
        if preview.len() > 20 {
            section = section.child(
                div()
                    .pt_1()
                    .text_sm()
                    .text_color(rgba(theme::HINT_TEXT))
                    .child(format!("… and {} more", preview.len() - 20)),
            );
        }
        section
    }

    /// One scope pill on the Export tab. Neutral rather than green: green is
    /// the export-action colour the smoke check counts.
    fn render_scope_pill(
        &self,
        scope: timetrack_core::ExportScope,
        id: &'static str,
        label: &'static str,
    ) -> impl IntoElement {
        let selected = self.export_picker.scope == scope;
        let entity = self.entity.clone();
        div()
            .id(ElementId::from(id))
            .px_3()
            .py_1()
            .rounded_md()
            .cursor_pointer()
            .bg(if selected {
                rgba(theme::RAISED_BG)
            } else {
                rgba(theme::TRANSPARENT)
            })
            .text_color(if selected {
                rgba(theme::SELECTED_TEXT)
            } else {
                rgba(theme::MUTED_TEXT)
            })
            .child(label)
            .when_some(entity, move |el, entity| {
                el.on_click(move |_ev, _window, cx| {
                    entity.update(cx, |view, cx| {
                        match snapshot_tz(&view.snapshot) {
                            Ok(tz) => view.export_picker.set_scope(scope, &tz),
                            Err(e) => {
                                view.status = Some(StatusMsg::local(e.to_string()));
                            }
                        }
                        cx.notify();
                    });
                })
            })
    }

    /// One anchor stepper button. Inert (present but clickless) for all time,
    /// which ignores the anchor. Neutral rather than green, as above.
    fn render_step_button(
        &self,
        id: &'static str,
        label: &'static str,
        delta: i32,
    ) -> impl IntoElement {
        let active = self.export_picker.is_active();
        let entity = self.entity.clone();
        let mut button = div()
            .id(ElementId::from(id))
            .px_3()
            .py_1()
            .rounded_md()
            .bg(rgba(theme::NEUTRAL_BUTTON_BG))
            .text_color(if active {
                rgba(theme::STRONG_TEXT)
            } else {
                rgba(theme::HINT_TEXT)
            })
            .child(label);
        if active {
            button = button
                .cursor_pointer()
                .when_some(entity, move |el, entity| {
                    el.on_click(move |_ev, _window, cx| {
                        entity.update(cx, |view, cx| {
                            view.step_export_anchor(delta);
                            cx.notify();
                        });
                    })
                });
        } else {
            button = button.cursor_default();
        }
        button
    }
}
