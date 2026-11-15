/* gui/app/render/home.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The Home tab: the week total first, then quick add, then projects, then
//! recent entries. Everything below the total is deliberately quieter.

use std::time::Duration;

use super::super::TimetrackView;
use super::super::{display_tz, service::Action, theme};
use gpui::prelude::*;
use gpui::{Animation, AnimationExt as _, ElementId, IntoElement, div, ease_out_quint, px, rgba};
use timetrack_core::QUICK_ADD_MS;
use timetrack_proto::Snapshot;

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

/// Snapshot indices of the entries a filter query keeps (#131).
///
/// The same rule as the CLI's `--search`: description, project name or
/// note contains the needle, case-insensitively, over sanitized display
/// text. Client-side over the snapshot the clients already hold, so
/// totals stay unfiltered — only the entries list narrows, and the week
/// hero keeps reading `snapshot.week` whole. An empty query keeps every
/// entry, in snapshot order.
pub(crate) fn filtered_entry_indices(snapshot: &Snapshot, query: &str) -> Vec<usize> {
    let needle = query.to_lowercase();
    if needle.is_empty() {
        return (0..snapshot.entries.len()).collect();
    }
    let project_name = |id: &str| -> &str {
        snapshot
            .projects
            .iter()
            .find(|p| p.id == id)
            .map_or("", |p| p.name.as_str())
    };
    snapshot
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            timetrack_core::sanitize_display(&e.description)
                .to_lowercase()
                .contains(&needle)
                || timetrack_core::sanitize_display(project_name(&e.project_id))
                    .to_lowercase()
                    .contains(&needle)
                || e.note.as_deref().is_some_and(|n| {
                    timetrack_core::sanitize_display(n)
                        .to_lowercase()
                        .contains(&needle)
                })
        })
        .map(|(i, _)| i)
        .collect()
}

impl TimetrackView {
    /// Snapshot indices of the entries the active filter keeps, in
    /// snapshot order. The cursor (`selected`) is a position in this
    /// list, and every entry action translates it first (#131).
    pub(crate) fn visible_entry_indices(&self) -> Vec<usize> {
        filtered_entry_indices(&self.snapshot, &self.entry_filter.query)
    }

    /// Home: the week total first, then quick add, then projects, then recent
    /// entries. Everything below the total is deliberately quieter.
    pub(crate) fn render_home(&self) -> impl IntoElement {
        if self.connecting() {
            return self.render_connecting().into_any_element();
        }
        let week = &self.snapshot.week;
        let motion = theme::motion_enabled();

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
                // When the total changes (a quick-add landed), it pulses
                // once -- opacity only, never size -- keyed on the total
                // itself so an unchanged total never replays it.
                div()
                    .text_size(px(72.))
                    .text_color(rgba(theme::SELECTED_TEXT))
                    .child(timetrack_core::format_duration(week.total_ms))
                    .with_animation(
                        ElementId::from(("hero-total", week.total_ms as u64)),
                        Animation::new(Duration::from_millis(theme::HERO_PULSE_MS))
                            .with_easing(ease_out_quint()),
                        move |el, delta| {
                            el.opacity(if motion {
                                theme::hero_opacity(delta)
                            } else {
                                1.0
                            })
                        },
                    ),
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
            .child(self.render_suggestions())
            .child(self.render_sources())
            .child(self.render_week_by_project())
            .child(self.render_filter())
            .child(self.render_entries())
            .into_any_element()
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
                    .text_color(rgba(theme::SELECTED_TEXT))
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

    /// Pending external suggestions (§19): fetched calendar items the
    /// user has not decided yet. Hidden entirely when there are none,
    /// so a service without sources leaves Home exactly as it was.
    /// Confirm lands on the source's mapped project (service-side),
    /// dismiss is permanent across refreshes.
    fn render_suggestions(&self) -> impl IntoElement {
        if self.suggestions.is_empty() {
            return div().into_any_element();
        }
        let tz = display_tz(&self.snapshot);
        let mut list = div().flex().flex_col();
        for (i, s) in self.suggestions.iter().enumerate() {
            let add_id = s.id.clone();
            let skip_id = s.id.clone();
            // gpui: `self` here is the `Render` receiver (the view),
            // and `self.entity` is view state (see app.rs::render).
            let entity_add = self.entity.clone();
            let entity_skip = self.entity.clone();
            list = list.child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .py_1()
                    .child(
                        div()
                            .flex_shrink_0()
                            .w(px(150.))
                            .text_sm()
                            .text_color(rgba(theme::ROW_TEXT))
                            .child(timetrack_core::format_local_iso8601(s.started_at, &tz)),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .w(px(90.))
                            .text_color(rgba(theme::ACCENT))
                            .child(timetrack_core::format_duration(s.ended_at - s.started_at)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .truncate()
                            .text_color(rgba(theme::BODY_TEXT))
                            .child(timetrack_core::sanitize_display(&s.title)),
                    )
                    .child(
                        div()
                            .id(ElementId::from(("suggest-add", i as u64)))
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .bg(rgba(theme::ACTION))
                            .text_color(rgba(theme::SELECTED_TEXT))
                            .cursor_pointer()
                            .child("add")
                            .on_click(move |_ev, _window, cx| {
                                if let Some(entity) = entity_add.clone() {
                                    let id = add_id.clone();
                                    entity.update(cx, |view, cx| {
                                        view.send(Action::ConfirmSuggestion(id));
                                        cx.notify();
                                    });
                                }
                            }),
                    )
                    .child(
                        div()
                            .id(ElementId::from(("suggest-skip", i as u64)))
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .bg(rgba(theme::NEUTRAL_BUTTON_BG))
                            .text_color(rgba(theme::BODY_TEXT))
                            .cursor_pointer()
                            .child("skip")
                            .on_click(move |_ev, _window, cx| {
                                if let Some(entity) = entity_skip.clone() {
                                    let id = skip_id.clone();
                                    entity.update(cx, |view, cx| {
                                        view.send(Action::DismissSuggestion(id));
                                        cx.notify();
                                    });
                                }
                            }),
                    ),
            );
        }
        let entity = self.entity.clone();
        div()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_6()
                    .pt_2()
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgba(theme::MUTED_TEXT))
                            .child("FROM YOUR CALENDARS"),
                    )
                    .child(
                        div()
                            .id("suggestions-refresh")
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .text_sm()
                            .text_color(rgba(theme::ACCENT))
                            .cursor_pointer()
                            .child("refresh")
                            .on_click(move |_ev, _window, cx| {
                                if let Some(entity) = entity.clone() {
                                    entity.update(cx, |view, cx| {
                                        view.send(Action::RefreshSources { id: None });
                                        cx.notify();
                                    });
                                }
                            }),
                    ),
            )
            .child(div().px_6().child(list))
            .into_any_element()
    }

    /// The element id of one of a source's row buttons (#153): built
    /// from the source id itself, like the project rows' buttons, so no
    /// two sources can share one.
    fn source_button_id(kind: &'static str, source_id: &str) -> ElementId {
        ElementId::from((ElementId::from(kind), source_id.to_string()))
    }

    /// Registered external sources (#153): each row names its kind,
    /// locator and project mapping and offers edit, refresh and
    /// remove. Always shown — unlike the suggestions above, an empty
    /// list still offers the way out of it. A CalDAV row also says
    /// whether the OS store holds its secret, since a missing one is
    /// the common reason a refresh fails.
    fn render_sources(&self) -> impl IntoElement {
        let mut list = div().flex().flex_col();
        if self.sources.is_empty() {
            list = list.child(
                div()
                    .py_2()
                    .text_color(rgba(theme::HINT_TEXT))
                    .child("No sources yet — add one to suggest calendar events."),
            );
        }
        for src in &self.sources {
            let edit_id = src.id.clone();
            let refresh_id = src.id.clone();
            let remove_id = src.id.clone();
            let entity_edit = self.entity.clone();
            let entity_refresh = self.entity.clone();
            let entity_remove = self.entity.clone();
            let mapping = match &src.project_id {
                None => "unmapped".to_string(),
                Some(pid) => self
                    .snapshot
                    .projects
                    .iter()
                    .find(|p| &p.id == pid)
                    .map(|p| format!("→ {}", timetrack_core::sanitize_display(&p.name)))
                    .unwrap_or_else(|| format!("→ {pid}")),
            };
            let credential = (src.kind == "caldav").then(|| {
                div()
                    .flex_shrink_0()
                    .text_sm()
                    .text_color(rgba(if src.has_credential {
                        theme::MUTED_TEXT
                    } else {
                        theme::WARNING
                    }))
                    .child(if src.has_credential {
                        "credential set".to_string()
                    } else {
                        "no credential".to_string()
                    })
            });
            list = list.child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .py_1()
                    .child(
                        div()
                            .flex_shrink_0()
                            .w(px(130.))
                            .truncate()
                            .text_color(rgba(theme::STRONG_TEXT))
                            .child(timetrack_core::sanitize_display(&src.label)),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .w(px(70.))
                            .text_sm()
                            .text_color(rgba(theme::MUTED_TEXT))
                            .child(src.kind.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .truncate()
                            .text_sm()
                            .text_color(rgba(theme::ROW_TEXT))
                            .child(timetrack_core::sanitize_display(&src.locator)),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .w(px(130.))
                            .truncate()
                            .text_sm()
                            .text_color(rgba(theme::MUTED_TEXT))
                            .child(mapping),
                    )
                    .when_some(credential, |el, badge| el.child(badge))
                    .child(
                        div()
                            .id(Self::source_button_id("edit-source", &src.id))
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .bg(rgba(theme::NEUTRAL_BUTTON_BG))
                            .text_color(rgba(theme::BODY_TEXT))
                            .cursor_pointer()
                            .child("edit")
                            .on_click(move |_ev, _window, cx| {
                                if let Some(entity) = entity_edit.clone() {
                                    let id = edit_id.clone();
                                    entity.update(cx, |view, cx| {
                                        view.open_source_edit(&id);
                                        cx.notify();
                                    });
                                }
                            }),
                    )
                    .child(
                        div()
                            .id(Self::source_button_id("refresh-source", &src.id))
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .bg(rgba(theme::NEUTRAL_BUTTON_BG))
                            .text_color(rgba(theme::BODY_TEXT))
                            .cursor_pointer()
                            .child("refresh")
                            .on_click(move |_ev, _window, cx| {
                                if let Some(entity) = entity_refresh.clone() {
                                    let id = refresh_id.clone();
                                    entity.update(cx, |view, cx| {
                                        view.send(Action::RefreshSources { id: Some(id) });
                                        cx.notify();
                                    });
                                }
                            }),
                    )
                    .child(
                        div()
                            .id(Self::source_button_id("remove-source", &src.id))
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .bg(rgba(theme::NEUTRAL_BUTTON_BG))
                            .text_color(rgba(theme::DANGER))
                            .cursor_pointer()
                            .child("remove")
                            .on_click(move |_ev, _window, cx| {
                                if let Some(entity) = entity_remove.clone() {
                                    let id = remove_id.clone();
                                    entity.update(cx, |view, cx| {
                                        view.open_source_remove_confirm(&id);
                                        cx.notify();
                                    });
                                }
                            }),
                    ),
            );
        }
        let entity = self.entity.clone();
        div()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_6()
                    .pt_2()
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgba(theme::MUTED_TEXT))
                            .child("SOURCES"),
                    )
                    .child(
                        div()
                            .id("add-source")
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .text_sm()
                            .text_color(rgba(theme::ACCENT))
                            .cursor_pointer()
                            .child("add source")
                            .on_click(move |_ev, _window, cx| {
                                if let Some(entity) = entity.clone() {
                                    entity.update(cx, |view, cx| {
                                        view.open_source_dialog();
                                        cx.notify();
                                    });
                                }
                            }),
                    ),
            )
            .child(div().px_6().child(list))
            .into_any_element()
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
                            .truncate()
                            .text_color(rgba(theme::STRONG_TEXT))
                            .child(name.to_string()),
                    )
                    // A bar proportional to the longest row (§11), so the
                    // distribution reads without reading every number.
                    .child(
                        div()
                            .flex_shrink_0()
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
                            .flex_shrink_0()
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

    /// The filter line: visible only while a query narrows the list or is
    /// being typed (#131). Totals are pinned to the unfiltered snapshot
    /// week, so the line also says how much of the list is showing.
    fn render_filter(&self) -> impl IntoElement {
        if !self.entry_filter.editing && !self.entry_filter.is_active() {
            return div().into_any_element();
        }
        let visible = self.visible_entry_indices().len();
        let total = self.snapshot.entries.len();
        let text = if self.entry_filter.editing {
            format!(
                "/{}_ · enter keep · esc clear",
                timetrack_core::sanitize_display(&self.entry_filter.query),
            )
        } else {
            format!(
                "filter {:?} · {visible}/{total} shown · / to edit · esc to clear",
                timetrack_core::sanitize_display(&self.entry_filter.query),
            )
        };
        div()
            .px_6()
            .py_1()
            .text_sm()
            .text_color(rgba(theme::MUTED_TEXT))
            .child(text)
            .into_any_element()
    }

    /// Recent entries. Everything is closed, so there is no running marker.
    /// The list shows the filtered view (#131); the cursor is a position in
    /// it, and the week hero above still reads the unfiltered snapshot.
    fn render_entries(&self) -> impl IntoElement {
        let visible = self.visible_entry_indices();
        if visible.is_empty() {
            return div()
                .px_6()
                .py_3()
                .text_color(rgba(theme::HINT_TEXT))
                .child(if self.entry_filter.is_active() {
                    "No entries match the filter — esc clears it."
                } else {
                    "Nothing tracked yet — press 1 to add 5 minutes."
                })
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

        let rows = visible.into_iter().enumerate().map(|(pos, i)| {
            let e = &self.snapshot.entries[i];
            let selected = pos == self.selected;
            div()
                .flex()
                .items_center()
                .gap_3()
                .px_6()
                .py_1()
                .bg(if selected {
                    rgba(theme::RAISED_BG)
                } else {
                    rgba(theme::TRANSPARENT)
                })
                // A quick-add is marked so it is visibly distinct from
                // hand-entered time, and so `u` reads as "undo that".
                .child(
                    div()
                        .flex_shrink_0()
                        .w_4()
                        .text_color(rgba(theme::ACTION))
                        .child(if e.source == timetrack_proto::EntrySource::QuickAdd {
                            "+"
                        } else {
                            " "
                        }),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .w(px(110.))
                        .text_color(rgba(theme::ACCENT))
                        .child(timetrack_core::format_duration(e.duration_ms())),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .w(px(160.))
                        .truncate()
                        .text_color(rgba(theme::ROW_TEXT))
                        .child(project_name(&e.project_id)),
                )
                .child(
                    div()
                        .flex_1()
                        .truncate()
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
    use timetrack_proto::{EntrySource, EntryView, ProjectView};

    fn entry(id: &str, project: &str, description: &str, note: Option<&str>) -> EntryView {
        EntryView {
            id: id.into(),
            project_id: project.into(),
            description: description.into(),
            started_at: 0,
            ended_at: 60_000,
            source: EntrySource::Manual,
            note: note.map(str::to_string),
        }
    }

    fn project(id: &str, name: &str) -> ProjectView {
        ProjectView {
            id: id.into(),
            name: name.into(),
            colour: None,
            archived: false,
        }
    }

    fn search_snapshot() -> Snapshot {
        Snapshot {
            entries: vec![
                entry("e0", "p1", "Review PR", None),
                entry("e1", "p1", "groceries", Some("weekly")),
                entry("e2", "p2", "misc", None),
            ],
            projects: vec![project("p1", "Deep Work"), project("p2", "Home")],
            ..Default::default()
        }
    }

    #[test]
    fn bars_fill_the_track_proportionally() {
        assert_eq!(bar_width(3_600_000, 3_600_000), PROJECT_BAR_TRACK_PX);
        assert_eq!(bar_width(1_800_000, 3_600_000), PROJECT_BAR_TRACK_PX / 2.0);
        assert_eq!(bar_width(0, 3_600_000), 0.0);
        // No entries (or no time) anywhere: zero width, never NaN.
        assert_eq!(bar_width(0, 0), 0.0);
    }

    #[test]
    fn an_empty_query_keeps_every_entry_in_order() {
        let snap = search_snapshot();
        assert_eq!(filtered_entry_indices(&snap, ""), vec![0, 1, 2]);
    }

    #[test]
    fn the_filter_matches_description_project_and_note_case_insensitively() {
        // #131: the same rule as the CLI's `--search`, client-side over
        // the snapshot.
        let snap = search_snapshot();
        assert_eq!(filtered_entry_indices(&snap, "review"), vec![0]);
        assert_eq!(
            filtered_entry_indices(&snap, "DEEP"),
            vec![0, 1],
            "project name matches its entries"
        );
        assert_eq!(filtered_entry_indices(&snap, "Weekly"), vec![1]);
        assert!(filtered_entry_indices(&snap, "absent").is_empty());
    }

    #[test]
    fn the_filter_never_narrows_the_week_totals() {
        // #131: search is a view, not a scope — the hero keeps reading
        // the unfiltered snapshot week, so filtering only returns
        // indices and leaves the totals struct untouched.
        let mut snap = search_snapshot();
        snap.week.total_ms = 180_000;
        snap.week.entry_count = 3;
        let before = snap.week.clone();
        let kept = filtered_entry_indices(&snap, "review");
        assert_eq!(kept, vec![0]);
        assert_eq!(snap.week, before);
        assert_eq!(snap.week.total_ms, 180_000);
        assert_eq!(snap.week.entry_count, 3);
    }
}
