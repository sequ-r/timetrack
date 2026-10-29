/* gui/app/render/home.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The Home tab: the week total first, then quick add, then projects, then
//! recent entries. Everything below the total is deliberately quieter.

use super::super::TimetrackView;
use super::super::{service::Action, theme};
use gpui::prelude::*;
use gpui::{ElementId, IntoElement, div, px, rgb, rgba};
use timetrack_core::QUICK_ADD_MS;

/// Width of the per-project bar track on Home.
const PROJECT_BAR_TRACK_PX: f32 = 120.0;

/// Bar fill width for one per-project row: longest fills the track.
///
/// Pure arithmetic so the proportion is unit-testable; the renderer only
/// turns the number into pixels.
fn bar_width(ms: i64, max_ms: i64) -> f32 {
    if max_ms <= 0 {
        return 0.0;
    }
    (ms.max(0) as f32 / max_ms as f32) * PROJECT_BAR_TRACK_PX
}

impl TimetrackView {
    /// Home: the week total first, then quick add, then projects, then recent
    /// entries. Everything below the total is deliberately quieter.
    pub(crate) fn render_home(&self) -> impl IntoElement {
        let week = &self.snapshot.week;

        // --- the number, at the top, largest on the screen ---
        let hero = div()
            .flex()
            .flex_col()
            .items_center()
            .gap_1()
            .px_6()
            .pt_6()
            .pb_4()
            .child(
                // An explicit size rather than a step from the scale: the
                // largest step (text_3xl) was still smaller than this number
                // is supposed to be. It has to be the first thing you see.
                div()
                    .text_size(px(72.))
                    .text_color(rgb(0xffffff))
                    .child(timetrack_core::format_duration(week.total_ms)),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(rgba(theme::MUTED_TEXT))
                    .child(format!(
                        "this week · {} {}",
                        week.entry_count,
                        if week.entry_count == 1 {
                            "entry"
                        } else {
                            "entries"
                        }
                    )),
            );

        // A day over 24h is legal data, but it usually means a mistyped
        // interval, so it is worth saying (REQUIREMENTS §4). Built as a div
        // rather than an Option because `child` takes an `IntoElement`.
        let warning = div()
            .px_6()
            .pb_1()
            .text_sm()
            .text_color(rgba(theme::WARNING))
            .child(if self.snapshot.over_24h_days.is_empty() {
                String::new()
            } else {
                format!(
                    "⚠ {} day(s) total more than 24h",
                    self.snapshot.over_24h_days.len()
                )
            });

        div()
            .id("home-scroll")
            .flex_1()
            .flex_col()
            .overflow_y_scroll()
            .child(hero)
            .child(warning)
            .child(self.render_quick_add())
            .child(self.render_week_by_project())
            .child(self.render_entries())
    }

    /// The 5/15/30/60 row, naming the project it will go to.
    fn render_quick_add(&self) -> impl IntoElement {
        let project_id = self.current_project().map(|p| p.id.clone());
        let project_name = self
            .current_project()
            .map(|p| timetrack_core::sanitize_display(&p.name))
            .unwrap_or_else(|| "no project".into());

        let mut row = div().flex().items_center().gap_2().px_6().py_3();
        row = row.child(
            div()
                .text_sm()
                .text_color(rgba(theme::MUTED_TEXT))
                .child("add time"),
        );
        for ms in QUICK_ADD_MS {
            let target = project_id.clone();
            row = row.child(
                div()
                    .id(ElementId::from(("quick", ms as u64)))
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .bg(rgba(theme::ACTION))
                    .text_color(rgb(0xffffff))
                    // With no project there is nothing to attribute the time
                    // to, so the button is present but inert rather than
                    // silently dropping the tap.
                    .when(target.is_none(), |this| this.cursor_default())
                    .when(target.is_some(), |this| this.cursor_pointer())
                    .child(format!("{}m", ms / 60_000))
                    .when(target.is_some(), move |el| {
                        let entity = self.entity.clone();
                        el.on_click(move |_ev, _window, cx| {
                            let project = target.clone().expect("checked above");
                            if let Some(entity) = entity.clone() {
                                entity.update(cx, |view, cx| {
                                    view.send(Action::QuickAdd { project, ms });
                                    cx.notify();
                                });
                            }
                        })
                    }),
            );
        }
        row.child(
            div()
                .text_sm()
                .text_color(rgba(theme::ROW_TEXT))
                .child(format!("→ {project_name}")),
        )
    }

    /// This week's time per project, longest first.
    fn render_week_by_project(&self) -> impl IntoElement {
        let mut rows: Vec<(String, i64)> = self
            .active_projects()
            .iter()
            .map(|p| {
                (
                    timetrack_core::sanitize_display(&p.name),
                    self.snapshot
                        .week
                        .per_project
                        .get(&p.id)
                        .copied()
                        .unwrap_or(0),
                )
            })
            .collect();
        // Longest first, ties by name, so the order never flickers between
        // refreshes.
        rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let max_ms = rows.iter().map(|(_, ms)| *ms).max().unwrap_or(0);

        let mut list = div().flex().flex_col();
        if rows.is_empty() {
            list = list.child(
                div()
                    .py_2()
                    .text_color(rgba(theme::HINT_TEXT))
                    .child("No projects yet — add one on the Projects tab."),
            );
        }
        for (name, ms) in rows {
            list = list.child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .py_1()
                    .child(
                        div()
                            .flex_1()
                            .text_color(rgba(theme::STRONG_TEXT))
                            .child(name.to_string()),
                    )
                    // A bar proportional to the longest row (§11), so the
                    // distribution reads without reading every number.
                    .child(
                        div()
                            .w(px(PROJECT_BAR_TRACK_PX))
                            .h(px(8.))
                            .rounded_md()
                            .bg(rgba(theme::RAISED_BG))
                            .child(
                                div()
                                    .w(px(bar_width(ms, max_ms)))
                                    .h_full()
                                    .rounded_md()
                                    .bg(rgba(theme::ACCENT)),
                            ),
                    )
                    .child(
                        div()
                            .w(px(110.))
                            .text_color(rgba(theme::ACCENT))
                            .child(timetrack_core::format_duration(ms)),
                    ),
            );
        }

        div()
            .child(
                div()
                    .px_6()
                    .pt_2()
                    .text_sm()
                    .text_color(rgba(theme::MUTED_TEXT))
                    .child("THIS WEEK BY PROJECT"),
            )
            .child(div().px_6().child(list))
    }

    /// Recent entries. Everything is closed, so there is no running marker.
    fn render_entries(&self) -> impl IntoElement {
        if self.snapshot.entries.is_empty() {
            return div()
                .px_6()
                .py_3()
                .text_color(rgba(theme::HINT_TEXT))
                .child("Nothing tracked yet — press 1 to add 5 minutes.")
                .into_any_element();
        }

        // Display copies only (#101): the ids stay the identity.
        let project_name = |id: &str| -> String {
            self.snapshot
                .projects
                .iter()
                .find(|p| p.id == id)
                .map(|p| timetrack_core::sanitize_display(&p.name))
                .unwrap_or_else(|| id.to_string())
        };

        let rows = self.snapshot.entries.iter().enumerate().map(|(i, e)| {
            let selected = i == self.selected;
            div()
                .flex()
                .items_center()
                .gap_3()
                .px_6()
                .py_1()
                .bg(if selected {
                    rgba(theme::RAISED_BG)
                } else {
                    rgba(0x00000000)
                })
                // A quick-add is marked so it is visibly distinct from
                // hand-entered time, and so `u` reads as "undo that".
                .child(div().w_4().text_color(rgba(theme::ACTION)).child(
                    if e.source == timetrack_proto::EntrySource::QuickAdd {
                        "+"
                    } else {
                        " "
                    },
                ))
                .child(
                    div()
                        .w(px(110.))
                        .text_color(rgba(theme::ACCENT))
                        .child(timetrack_core::format_duration(e.duration_ms())),
                )
                .child(
                    div()
                        .w(px(160.))
                        .text_color(rgba(theme::ROW_TEXT))
                        .child(project_name(&e.project_id)),
                )
                .child(
                    div()
                        .flex_1()
                        .text_color(rgba(theme::BODY_TEXT))
                        .child(timetrack_core::sanitize_display(e.label())),
                )
        });

        div()
            .px_6()
            .pt_2()
            .pb_1()
            .text_sm()
            .text_color(rgba(theme::MUTED_TEXT))
            .child("RECENT ENTRIES  (+ = QUICK ADD)")
            .child(div().flex().flex_col().children(rows))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bars_fill_the_track_proportionally() {
        assert_eq!(bar_width(3_600_000, 3_600_000), PROJECT_BAR_TRACK_PX);
        assert_eq!(bar_width(1_800_000, 3_600_000), PROJECT_BAR_TRACK_PX / 2.0);
        assert_eq!(bar_width(0, 3_600_000), 0.0);
        // No entries (or no time) anywhere: zero width, never NaN.
        assert_eq!(bar_width(0, 0), 0.0);
    }
}
