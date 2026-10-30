/* gui/app/render/projects.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The Projects tab: every project with its week and all-time totals, plus
//! an archive action. Archived projects stay listed, because their history
//! still counts (REQUIREMENTS §14).

use super::super::TimetrackView;
use super::super::{service::Action, theme};
use gpui::prelude::*;
use gpui::{ElementId, IntoElement, div, px, rgba};

/// One small row action button (Recolour/Delete), in the archive pair's
/// shape. `on_click` receives the view, already entity-updated.
fn row_button(
    id: ElementId,
    label: &'static str,
    bg: gpui::Rgba,
    fg: gpui::Rgba,
    entity: Option<gpui::Entity<TimetrackView>>,
    act: impl Fn(&mut TimetrackView) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .flex_shrink_0()
        .px_2()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .bg(bg)
        .text_color(fg)
        .child(label)
        .when_some(entity, move |this, entity| {
            this.on_click(move |_ev, _window, cx| {
                entity.update(cx, |view, cx| {
                    act(view);
                    cx.notify();
                });
            })
        })
}

/// The element id of one of a project's row buttons.
///
/// Built from the project id itself rather than from a number parsed out of
/// it. The core mints ids as `p1`, `p2`, ..., so parsing the numeric suffix
/// was unique in practice -- but any id that was not `p<digits>` fell back to
/// `0`, which would have given two such projects the same element id, and
/// gpui would have treated the second button as the first.
fn project_button_id(kind: &'static str, project_id: &str) -> ElementId {
    ElementId::from((ElementId::from(kind), project_id.to_string()))
}

impl TimetrackView {
    /// Projects: every project with its week and all-time totals, plus an
    /// archive action. Archived projects stay listed, because their history
    /// still counts (REQUIREMENTS §14).
    pub(crate) fn render_projects(&self) -> impl IntoElement {
        if self.connecting() {
            return self.render_connecting().into_any_element();
        }
        // A "New project" row rather than a dead action: it opens the naming
        // prompt, so every project name is typed -- never invented.
        let new_project = {
            let entity = self.entity.clone();
            div()
                .id("new-project")
                .mx_6()
                .my_3()
                .px_3()
                .py_2()
                .rounded_md()
                .cursor_pointer()
                .bg(rgba(theme::ACTION))
                .text_color(rgba(theme::SELECTED_TEXT))
                .child("New project")
                .when_some(entity, |this, entity| {
                    this.on_click(move |_ev, _window, cx| {
                        entity.update(cx, |view, cx| {
                            view.open_project_prompt();
                            cx.notify();
                        });
                    })
                })
        };

        if self.snapshot.projects.is_empty() {
            return div()
                .flex_1()
                .flex_col()
                .px_6()
                .py_4()
                .child(
                    div()
                        .text_color(rgba(theme::HINT_TEXT))
                        .child("No projects yet — create one to start tracking time."),
                )
                .child(new_project)
                .into_any_element();
        }

        let rows = self.snapshot.projects.iter().map(|p| {
            // Both columns come from the service's aggregates, never re-summed
            // here: the snapshot may carry only recent entries while the
            // aggregates cover the whole store, and re-summing under-reports
            // a large store's all-time column next to a correct week column.
            let (week, all) = self.snapshot.project_totals(&p.id);
            let id = p.id.clone();
            let name = timetrack_core::sanitize_display(&p.name);
            // The project's colour, if it has one, else the neutral row grey.
            let dot = theme::project_colour(p.colour);
            div()
                .flex()
                .items_center()
                .gap_3()
                .px_6()
                .py_2()
                .child(
                    div()
                        .flex_shrink_0()
                        .w(px(12.))
                        .h(px(12.))
                        .rounded_md()
                        .bg(dot),
                )
                .child(
                    div()
                        .flex_1()
                        .truncate()
                        .text_color(if p.archived {
                            rgba(theme::HINT_TEXT)
                        } else {
                            rgba(theme::BODY_TEXT)
                        })
                        .child(if p.archived {
                            format!("{name}  (archived)")
                        } else {
                            name.clone()
                        }),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .w(px(110.))
                        .text_color(rgba(theme::ACCENT))
                        .child(timetrack_core::format_duration(week)),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .w(px(110.))
                        .text_color(rgba(theme::MUTED_TEXT))
                        .child(timetrack_core::format_duration(all)),
                )
                // Housekeeping (#104): the mouse routes to the same
                // prompts the `c`/`D` keys open for the current project —
                // Delete only opens the confirmation, like the first `D`.
                .child(row_button(
                    project_button_id("recolour-project", &p.id),
                    "Recolour",
                    rgba(theme::NEUTRAL_BUTTON_BG),
                    rgba(theme::SELECTED_TEXT),
                    self.entity.clone(),
                    {
                        let id = id.clone();
                        move |view: &mut TimetrackView| view.open_recolour_prompt(&id)
                    },
                ))
                .child(row_button(
                    project_button_id("delete-project", &p.id),
                    "Delete",
                    rgba(theme::NEUTRAL_BUTTON_BG),
                    rgba(theme::DANGER),
                    self.entity.clone(),
                    {
                        let id = id.clone();
                        move |view: &mut TimetrackView| view.open_delete_project_confirm(&id)
                    },
                ))
                // Archiving is offered only for live projects; an archived one
                // offers the reverse instead, so archiving is reversible
                // without the CLI.
                .when(!p.archived, {
                    let entity = self.entity.clone();
                    let project_id = id.clone();
                    move |el| {
                        el.child(
                            div()
                                .id(project_button_id("archive", &p.id))
                                .flex_shrink_0()
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .cursor_pointer()
                                .bg(rgba(theme::NEUTRAL_BUTTON_BG))
                                .text_color(rgba(theme::STRONG_TEXT))
                                .child("Archive")
                                .on_click(move |_ev, _window, cx| {
                                    if let Some(entity) = entity.clone() {
                                        entity.update(cx, |view, cx| {
                                            view.send(Action::ArchiveProject(project_id.clone()));
                                            cx.notify();
                                        });
                                    }
                                }),
                        )
                    }
                })
                .when(p.archived, {
                    let entity = self.entity.clone();
                    let project_id = id.clone();
                    move |el| {
                        el.child(
                            div()
                                .id(project_button_id("unarchive", &p.id))
                                .flex_shrink_0()
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .cursor_pointer()
                                .bg(rgba(theme::ACTION_FADED))
                                .text_color(rgba(theme::STRONG_TEXT))
                                .child("Unarchive")
                                .on_click(move |_ev, _window, cx| {
                                    if let Some(entity) = entity.clone() {
                                        entity.update(cx, |view, cx| {
                                            view.send(Action::UnarchiveProject(project_id.clone()));
                                            cx.notify();
                                        });
                                    }
                                }),
                        )
                    }
                })
        });

        div()
            .id("projects-scroll")
            .flex_1()
            .flex_col()
            .overflow_y_scroll()
            .child(
                div()
                    .px_6()
                    .pt_2()
                    .pb_1()
                    .text_sm()
                    .text_color(rgba(theme::MUTED_TEXT))
                    .child("PROJECTS   (this week · all time)"),
            )
            .child(div().flex().flex_col().children(rows))
            .child(new_project)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_projects_never_share_a_button_element_id() {
        // The old id parsed the numeric suffix out of the project id and
        // fell back to 0 for anything else, so two ids that were not
        // `p<digits>` would have collided.
        assert_ne!(
            project_button_id("archive", "p1"),
            project_button_id("archive", "p2")
        );
        assert_ne!(
            project_button_id("archive", "imported"),
            project_button_id("archive", "other")
        );
        assert_ne!(
            project_button_id("archive", "imported"),
            project_button_id("unarchive", "imported"),
            "the two buttons of one project must differ too"
        );
        // The housekeeping buttons (#104) share each row with those two.
        for kind in ["recolour-project", "delete-project"] {
            assert_ne!(
                project_button_id(kind, "p1"),
                project_button_id("archive", "p1")
            );
            assert_ne!(
                project_button_id(kind, "p1"),
                project_button_id("archive", "p2")
            );
        }
        assert_ne!(
            project_button_id("recolour-project", "p1"),
            project_button_id("delete-project", "p1")
        );
    }

    #[test]
    fn one_project_always_gets_the_same_button_element_id() {
        assert_eq!(
            project_button_id("archive", "p7"),
            project_button_id("archive", "p7")
        );
    }
}
