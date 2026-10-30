/* gui/app/render/prompts.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The prompt panels, drawn above the tab content.
//!
//! They live here, in the shared wrapper, rather than inside `render_home`:
//! a prompt can be opened from any tab (the shortcuts are global), so
//! drawing it only on Home produced a modal that was never painted and that
//! took the keyboard anyway.
//!
//! Which panel is drawn is the same question the key handler asks
//! (`open_prompt`), so the two cannot drift apart.

use super::super::TimetrackView;
use super::super::dialogs::delete::DeleteConfirm;
use super::super::dialogs::delete_project::DeleteProjectConfirm;
use super::super::dialogs::entry::{DialogField, EntryDialog};
use super::super::dialogs::merge::MergeConfirm;
use super::super::dialogs::project::ProjectPrompt;
use super::super::dialogs::recolour::RecolourPrompt;
use super::super::dialogs::split::SplitPrompt;
use super::super::{display_tz, theme};
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    Animation, AnimationExt as _, Div, ElementId, Entity, IntoElement, Rgba, div, ease_out_quint,
    px, rgba,
};

/// The shared prompt panel shell: one bordered panel above the tab content,
/// where the eye already is.
fn prompt_panel() -> Div {
    div()
        .mx_6()
        .mt_3()
        .mb_1()
        .px_6()
        .py_3()
        .rounded_md()
        .border_1()
        .border_color(rgba(theme::BORDER_SUBTLE))
        .flex()
        .flex_col()
        .bg(rgba(theme::PANEL_BG))
}

/// One confirmation banner: the summary line, then Confirm and Cancel
/// buttons under it.
///
/// The buttons are the mouse route to the same answers the second
/// keystroke and Escape give: `on_confirm`/`on_cancel` are the view
/// methods the key handlers themselves call, so a click can never send
/// a different action than the key. Without them a mouse user could
/// open a banner (a row button, say) and never answer it.
#[allow(clippy::too_many_arguments)]
fn confirm_banner(
    summary: String,
    confirm_id: &'static str,
    confirm_label: &'static str,
    confirm_bg: Rgba,
    cancel_id: &'static str,
    entity: Option<Entity<TimetrackView>>,
    on_confirm: fn(&mut TimetrackView),
    on_cancel: fn(&mut TimetrackView),
) -> Div {
    let confirm_entity = entity.clone();
    prompt_panel()
        .child(div().text_color(rgba(theme::ACCENT)).child(summary))
        .child(
            div()
                .flex()
                .gap_2()
                .pt_2()
                .child(
                    div()
                        .id(confirm_id)
                        .flex_shrink_0()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .cursor_pointer()
                        .bg(confirm_bg)
                        .text_color(rgba(theme::SELECTED_TEXT))
                        .child(confirm_label)
                        .when_some(confirm_entity, move |this, entity| {
                            this.on_click(move |_ev, _window, cx| {
                                entity.update(cx, |view, cx| {
                                    on_confirm(view);
                                    cx.notify();
                                });
                            })
                        }),
                )
                .child(
                    div()
                        .id(cancel_id)
                        .flex_shrink_0()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .cursor_pointer()
                        .bg(rgba(theme::NEUTRAL_BUTTON_BG))
                        .text_color(rgba(theme::STRONG_TEXT))
                        .child("Cancel")
                        .when_some(entity, move |this, entity| {
                            this.on_click(move |_ev, _window, cx| {
                                entity.update(cx, |view, cx| {
                                    on_cancel(view);
                                    cx.notify();
                                });
                            })
                        }),
                ),
        )
}

impl TimetrackView {
    /// The prompt panels, drawn above the tab content.
    ///
    /// They live here, in the shared wrapper, rather than inside
    /// `render_home`: a prompt can be opened from any tab (the shortcuts are
    /// global), so drawing it only on Home produced a modal that was never
    /// painted and that took the keyboard anyway.
    ///
    /// Which panel is drawn is the same question the key handler asks
    /// (`open_prompt`), so the two cannot drift apart.
    pub(crate) fn render_prompts(&self) -> impl IntoElement {
        // A prompt fades in on mount (opacity only, one-way: closing
        // renders nothing rather than holding a dying panel for an exit
        // fade). The animation is keyed on which prompt is open, so
        // swapping prompts replays it and an unchanged prompt does not.
        let motion = theme::motion_enabled();
        let key = format!("{:?}", self.open_prompt());
        div()
            // Absolutely positioned over the relative stage the root
            // wraps the tab content in: the panel covers the content's
            // top rows while it is open, and the rows underneath never
            // move. With no prompt open this div is empty and zero
            // height, so it intercepts nothing.
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .flex()
            .flex_col()
            .when_some(self.dialog.as_ref(), |el, dlg| {
                el.child(self.render_entry_dialog(dlg))
            })
            .when_some(self.project_prompt.as_ref(), |el, prompt| {
                el.child(self.render_project_prompt(prompt))
            })
            .when_some(self.split_prompt.as_ref(), |el, prompt| {
                el.child(self.render_split_prompt(prompt))
            })
            .when_some(self.merge_confirm.as_ref(), |el, confirm| {
                el.child(self.render_merge_confirm(confirm))
            })
            .when_some(self.delete_confirm.as_ref(), |el, confirm| {
                el.child(self.render_delete_confirm(confirm))
            })
            .when_some(self.recolour_prompt.as_ref(), |el, prompt| {
                el.child(self.render_recolour_prompt(prompt))
            })
            .when_some(self.delete_project_confirm.as_ref(), |el, confirm| {
                el.child(self.render_delete_project_confirm(confirm))
            })
            .with_animation(
                ElementId::from((ElementId::from("prompt-fade"), key)),
                Animation::new(Duration::from_millis(theme::PROMPT_FADE_MS))
                    .with_easing(ease_out_quint()),
                move |el, delta| el.opacity(if motion { delta } else { 1.0 }),
            )
    }

    /// The manual-entry dialog (REQUIREMENTS §5 methods 1 and 2, one form).
    ///
    /// A bordered panel at the top of Home, above the week total: while it is
    /// open it owns the keyboard (see `on_dialog_key`), so it sits where the
    /// eye already is rather than competing from below. Typing is
    /// keyboard-driven with no toolkit text field, but a click on a field
    /// row focuses that field, the mouse route to where Tab goes.
    fn render_entry_dialog(&self, dlg: &EntryDialog) -> impl IntoElement {
        // Drawing only (the keep-hints): saving re-resolves strictly.
        let tz = display_tz(&self.snapshot);
        let title = match &dlg.edit_id {
            None => format!("NEW ENTRY → {}", dlg.project_name),
            Some(id) => format!("EDIT ENTRY {id} · {}", dlg.project_name),
        };
        let current_field = dlg.field;
        let entity_for_fields = self.entity.clone();
        let field_row = |id: &'static str,
                         field: DialogField,
                         label: &str,
                         value: &str,
                         hint: &str,
                         caret: &super::super::textline::Caret| {
            let focused = current_field == field;
            let entity = entity_for_fields.clone();
            div()
                .id(id)
                .flex()
                .items_center()
                .gap_2()
                .py_1()
                .cursor_pointer()
                .child(
                    div()
                        .w(px(110.))
                        .text_color(if focused {
                            rgba(theme::SELECTED_TEXT)
                        } else {
                            rgba(theme::MUTED_TEXT)
                        })
                        .child(format!("{} {label}", if focused { ">" } else { " " })),
                )
                .child(
                    div()
                        .flex_1()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .border_1()
                        .border_color(if focused {
                            rgba(theme::FOCUS_RING)
                        } else {
                            rgba(theme::TRANSPARENT)
                        })
                        .bg(if focused {
                            rgba(theme::RAISED_BG)
                        } else {
                            rgba(theme::TRANSPARENT)
                        })
                        .text_color(rgba(theme::BODY_TEXT))
                        .child(if focused {
                            super::super::textline::render_with_caret(value, caret)
                        } else {
                            timetrack_core::sanitize_display(value)
                        }),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(rgba(theme::HINT_TEXT))
                        .child(hint.to_string()),
                )
                .when_some(entity, move |this, entity| {
                    this.on_click(move |_ev, _window, cx| {
                        entity.update(cx, |view, cx| {
                            view.focus_dialog_field(field);
                            cx.notify();
                        });
                    })
                })
        };

        let mut panel = prompt_panel()
            .child(div().text_color(rgba(theme::ACCENT)).child(title))
            .child(field_row(
                "dialog-field-description",
                DialogField::Description,
                "what",
                &dlg.description,
                "optional note",
                &dlg.caret,
            ));
        // The project field exists only in edit mode (#104): it cycles
        // (h/l, or a click) instead of taking text, so it is built apart
        // from the shared text-row shape above.
        if dlg.edit_id.is_some() {
            let focused = dlg.field == DialogField::Project;
            let (_, chosen) = dlg.chosen_project();
            let entity = self.entity.clone();
            panel = panel.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .py_1()
                    .child(
                        div()
                            .w(px(110.))
                            .text_color(if focused {
                                rgba(theme::SELECTED_TEXT)
                            } else {
                                rgba(theme::MUTED_TEXT)
                            })
                            .child(format!("{} project", if focused { ">" } else { " " })),
                    )
                    .child(
                        div()
                            .id("dialog-project-cycle")
                            .flex_1()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .cursor_pointer()
                            .border_1()
                            .border_color(if focused {
                                rgba(theme::FOCUS_RING)
                            } else {
                                rgba(theme::TRANSPARENT)
                            })
                            .bg(if focused {
                                rgba(theme::RAISED_BG)
                            } else {
                                rgba(theme::TRANSPARENT)
                            })
                            .text_color(rgba(theme::BODY_TEXT))
                            .child(format!("‹ {chosen} ›"))
                            .when_some(entity, |this, entity| {
                                this.on_click(move |_ev, _window, cx| {
                                    entity.update(cx, |view, cx| {
                                        view.focus_dialog_field(DialogField::Project);
                                        view.cycle_dialog_project(1);
                                        cx.notify();
                                    });
                                })
                            }),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgba(theme::HINT_TEXT))
                            .child("h/l or click to move"),
                    ),
            );
        }
        panel = panel
            .child(field_row(
                "dialog-field-start",
                DialogField::Start,
                "start",
                &dlg.start,
                &dlg.start_hint(&tz),
                &dlg.caret,
            ))
            .child(field_row(
                "dialog-field-end",
                DialogField::End,
                "end",
                &dlg.end,
                &dlg.end_hint(&tz),
                &dlg.caret,
            ))
            .child(
                div()
                    .pt_2()
                    .text_sm()
                    .text_color(rgba(theme::HINT_TEXT))
                    .child("type to edit · tab next field · enter save · esc cancel"),
            );
        if let Some(err) = &dlg.error {
            panel = panel.child(
                div()
                    .pt_1()
                    .text_color(rgba(theme::WARNING))
                    .child(err.clone()),
            );
        }
        panel
    }

    /// The split prompt: one moment field, same panel styling as the entry
    /// dialog, above the week total where the eye already is.
    fn render_split_prompt(&self, prompt: &SplitPrompt) -> impl IntoElement {
        let mut panel = prompt_panel()
            .child(
                div()
                    .text_color(rgba(theme::ACCENT))
                    .child(format!("SPLIT ENTRY {}", prompt.entry_id)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .py_1()
                    .child(
                        div()
                            .w(px(110.))
                            .text_color(rgba(theme::SELECTED_TEXT))
                            .child("> at"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(rgba(theme::FOCUS_RING))
                            .bg(rgba(theme::RAISED_BG))
                            .text_color(rgba(theme::BODY_TEXT))
                            .child(super::super::textline::render_with_caret(
                                &prompt.at,
                                &prompt.caret,
                            )),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgba(theme::HINT_TEXT))
                            .child("(-90, 90, now, HH:MM)"),
                    ),
            )
            .child(
                div()
                    .pt_2()
                    .text_sm()
                    .text_color(rgba(theme::HINT_TEXT))
                    .child("type a moment · enter split · esc cancel"),
            );
        if let Some(err) = &prompt.error {
            panel = panel.child(
                div()
                    .pt_1()
                    .text_color(rgba(theme::WARNING))
                    .child(err.clone()),
            );
        }
        panel
    }

    /// The merge confirmation banner: what would merge and how the total
    /// changes, computed when the confirmation opened. The amount is the
    /// point (§8): merging overlapping entries shrinks the total, and the
    /// user states the overlap was a mistake by confirming.
    fn render_merge_confirm(&self, confirm: &MergeConfirm) -> impl IntoElement {
        confirm_banner(
            confirm.summary.clone(),
            "confirm-merge",
            "Merge",
            rgba(theme::ACTION),
            "cancel-merge",
            self.entity.clone(),
            TimetrackView::confirm_merge,
            TimetrackView::cancel_merge,
        )
    }

    /// The delete confirmation banner: exactly which entry would go, so a
    /// destructive action is never one keystroke with no reading step.
    fn render_delete_confirm(&self, confirm: &DeleteConfirm) -> impl IntoElement {
        confirm_banner(
            confirm.summary.clone(),
            "confirm-delete-entry",
            "Delete",
            rgba(theme::DANGER),
            "cancel-delete-entry",
            self.entity.clone(),
            TimetrackView::confirm_delete,
            TimetrackView::cancel_delete,
        )
    }

    /// The project prompt: one name field, same panel styling as the other
    /// prompts, above the week total where the eye already is.
    fn render_project_prompt(&self, prompt: &ProjectPrompt) -> impl IntoElement {
        let title = match &prompt.edit_id {
            None => "NEW PROJECT".to_string(),
            Some(_) => "RENAME PROJECT".to_string(),
        };
        let mut panel = prompt_panel()
            .child(div().text_color(rgba(theme::ACCENT)).child(title))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .py_1()
                    .child(
                        div()
                            .w(px(110.))
                            .text_color(rgba(theme::SELECTED_TEXT))
                            .child("> name"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(rgba(theme::FOCUS_RING))
                            .bg(rgba(theme::RAISED_BG))
                            .text_color(rgba(theme::BODY_TEXT))
                            .child(super::super::textline::render_with_caret(
                                &prompt.name,
                                &prompt.caret,
                            )),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgba(theme::HINT_TEXT))
                            .child("a short name, e.g. Work"),
                    ),
            )
            .child(
                div()
                    .pt_2()
                    .text_sm()
                    .text_color(rgba(theme::HINT_TEXT))
                    .child("type a name · enter save · esc cancel"),
            );
        if let Some(err) = &prompt.error {
            panel = panel.child(
                div()
                    .pt_1()
                    .text_color(rgba(theme::WARNING))
                    .child(err.clone()),
            );
        }
        panel
    }

    /// The recolour prompt (#104): one hex field with a live preview dot,
    /// same panel styling as the project prompt. The dot shows the typed
    /// colour while it parses (and the neutral grey when the field is
    /// empty, which is what saving then means: no colour).
    fn render_recolour_prompt(&self, prompt: &RecolourPrompt) -> impl IntoElement {
        let preview = if prompt.text.trim().is_empty() {
            theme::project_colour(None)
        } else {
            theme::project_colour(timetrack_core::parse_colour(&prompt.text).ok())
        };
        let mut panel = prompt_panel()
            .child(
                div()
                    .text_color(rgba(theme::ACCENT))
                    .child(format!("RECOLOUR {}", prompt.project_name)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .py_1()
                    .child(
                        div()
                            .w(px(110.))
                            .text_color(rgba(theme::SELECTED_TEXT))
                            .child("> colour"),
                    )
                    .child(div().w(px(12.)).h(px(12.)).rounded_md().bg(preview))
                    .child(
                        div()
                            .flex_1()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(rgba(theme::FOCUS_RING))
                            .bg(rgba(theme::RAISED_BG))
                            .text_color(rgba(theme::BODY_TEXT))
                            .child(format!("{}_", prompt.text)),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgba(theme::HINT_TEXT))
                            .child("RRGGBB hex · empty clears"),
                    ),
            )
            .child(
                div()
                    .pt_2()
                    .text_sm()
                    .text_color(rgba(theme::HINT_TEXT))
                    .child("type a colour · enter save · esc cancel"),
            );
        if let Some(err) = &prompt.error {
            panel = panel.child(
                div()
                    .pt_1()
                    .text_color(rgba(theme::WARNING))
                    .child(err.clone()),
            );
        }
        panel
    }

    /// The delete-project confirmation banner (#104): which project would
    /// go, behind the same second-keystroke guard as entry deletion.
    fn render_delete_project_confirm(&self, confirm: &DeleteProjectConfirm) -> impl IntoElement {
        confirm_banner(
            confirm.summary.clone(),
            "confirm-delete-project",
            "Delete project",
            rgba(theme::DANGER),
            "cancel-delete-project",
            self.entity.clone(),
            TimetrackView::confirm_delete_project,
            TimetrackView::cancel_delete_project,
        )
    }
}
