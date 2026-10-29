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
use super::super::{snapshot_tz, theme};
use gpui::prelude::*;
use gpui::{Div, IntoElement, div, px, rgb, rgba};

/// The shared prompt panel shell: one bordered panel above the tab content,
/// where the eye already is.
fn prompt_panel() -> Div {
    div()
        .mx_6()
        .mt_3()
        .mb_1()
        .px_4()
        .py_3()
        .rounded_md()
        .flex()
        .flex_col()
        .bg(rgba(theme::PANEL_BG))
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
        div()
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
    }

    /// The manual-entry dialog (REQUIREMENTS §5 methods 1 and 2, one form).
    ///
    /// A bordered panel at the top of Home, above the week total: while it is
    /// open it owns the keyboard (see `on_dialog_key`), so it sits where the
    /// eye already is rather than competing from below. Keyboard-driven like
    /// everything else here — no mouse targets, no toolkit text field.
    fn render_entry_dialog(&self, dlg: &EntryDialog) -> impl IntoElement {
        let tz = snapshot_tz(&self.snapshot);
        let title = match &dlg.edit_id {
            None => format!("NEW ENTRY → {}", dlg.project_name),
            Some(id) => format!("EDIT ENTRY {id} · {}", dlg.project_name),
        };
        let field_row = |label: &str, value: &str, hint: &str, focused: bool| {
            div()
                .flex()
                .items_center()
                .gap_2()
                .py_1()
                .child(
                    div()
                        .w(px(110.))
                        .text_color(if focused {
                            rgb(0xffffff)
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
                        .bg(if focused {
                            rgba(theme::RAISED_BG)
                        } else {
                            rgba(0x00000000)
                        })
                        .text_color(rgba(theme::BODY_TEXT))
                        .child(format!("{value}{}", if focused { "_" } else { "" })),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(rgba(theme::HINT_TEXT))
                        .child(hint.to_string()),
                )
        };

        let mut panel = prompt_panel()
            .child(div().text_color(rgba(theme::ACCENT)).child(title))
            .child(field_row(
                "what",
                &dlg.description,
                "optional note",
                dlg.field == DialogField::Description,
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
                                rgb(0xffffff)
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
                            .bg(if focused {
                                rgba(theme::RAISED_BG)
                            } else {
                                rgba(0x00000000)
                            })
                            .text_color(rgba(theme::BODY_TEXT))
                            .child(format!("‹ {chosen} ›"))
                            .when_some(entity, |this, entity| {
                                this.on_click(move |_ev, _window, cx| {
                                    entity.update(cx, |view, cx| {
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
                "start",
                &dlg.start,
                &dlg.start_hint(&tz),
                dlg.field == DialogField::Start,
            ))
            .child(field_row(
                "end",
                &dlg.end,
                &dlg.end_hint(&tz),
                dlg.field == DialogField::End,
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
                    .child(div().w(px(110.)).text_color(rgb(0xffffff)).child("> at"))
                    .child(
                        div()
                            .flex_1()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .bg(rgba(theme::RAISED_BG))
                            .text_color(rgba(theme::BODY_TEXT))
                            .child(format!("{}_", prompt.at)),
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
        prompt_panel().child(
            div()
                .text_color(rgba(theme::ACCENT))
                .child(confirm.summary.clone()),
        )
    }

    /// The delete confirmation banner: exactly which entry would go, so a
    /// destructive action is never one keystroke with no reading step.
    fn render_delete_confirm(&self, confirm: &DeleteConfirm) -> impl IntoElement {
        prompt_panel().child(
            div()
                .text_color(rgba(theme::ACCENT))
                .child(confirm.summary.clone()),
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
                    .child(div().w(px(110.)).text_color(rgb(0xffffff)).child("> name"))
                    .child(
                        div()
                            .flex_1()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .bg(rgba(theme::RAISED_BG))
                            .text_color(rgba(theme::BODY_TEXT))
                            .child(format!("{}_", prompt.name)),
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
                            .text_color(rgb(0xffffff))
                            .child("> colour"),
                    )
                    .child(div().w(px(12.)).h(px(12.)).rounded_md().bg(preview))
                    .child(
                        div()
                            .flex_1()
                            .px_2()
                            .py_1()
                            .rounded_md()
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
        prompt_panel().child(
            div()
                .text_color(rgba(theme::ACCENT))
                .child(confirm.summary.clone()),
        )
    }
}
