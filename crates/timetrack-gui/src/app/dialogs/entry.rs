/* gui/app/dialogs/entry.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The manual-entry dialog (REQUIREMENTS §5 methods 1 and 2, one form).
//!
//! Keystroke editing with a movable caret and selection rather than
//! a toolkit text field: the window already routes every key through one
//! focused root, and three small fields do not need IME or cursor movement.
//! Everything here is pure state over strings, so the whole interaction is
//! unit-testable without a window.

/// Which field of the entry dialog receives keystrokes.
///
/// `Project` exists only in edit mode (#104): a new entry's project is
/// fixed when the dialog opens, while an edit may move the entry. Tab
/// cycling goes through `EntryDialog::next_field`, which skips it for new
/// entries; `next`/`prev` here are the raw four-field order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DialogField {
    Description,
    Project,
    Start,
    End,
}

impl DialogField {
    pub(crate) fn next(self) -> Self {
        match self {
            DialogField::Description => DialogField::Project,
            DialogField::Project => DialogField::Start,
            DialogField::Start => DialogField::End,
            DialogField::End => DialogField::Description,
        }
    }

    pub(crate) fn prev(self) -> Self {
        match self {
            DialogField::Description => DialogField::End,
            DialogField::Project => DialogField::Description,
            DialogField::Start => DialogField::Project,
            DialogField::End => DialogField::Start,
        }
    }
}

/// The manual-entry dialog (REQUIREMENTS §5 methods 1 and 2, one form).
///
/// Method 2 is method 1 with "end = now" pre-filled, so a new dialog starts
/// with `start = "-60"`, `end = "now"` and the user edits from there. In edit
/// mode the times start empty, meaning "keep the current endpoint" — the
/// `keep_*` labels say what that is — which is the dialog form of `shorten`:
/// unspecified endpoints are left alone and the entry is always kept.
///
/// Keystroke editing with a movable caret and selection rather than
/// a toolkit text field: the window already routes every key through one
/// focused root, and three small fields do not need IME or cursor movement.
/// Everything here is pure state over strings, so the whole interaction is
/// unit-testable without a window.
#[derive(Debug, Clone)]
pub(crate) struct EntryDialog {
    /// `None` for a new entry; `Some(id)` when editing that entry.
    pub(crate) edit_id: Option<String>,
    /// The project a new entry goes to, fixed when the dialog opens.
    pub(crate) project_id: String,
    /// Display copy of the project name for the dialog title, sanitized at
    /// construction (#101). The id above is the identity; this never is.
    pub(crate) project_name: String,
    /// Edit mode only: the projects the entry may move to, as (id, display
    /// name) pairs, with the entry's current project always among them.
    /// New mode carries just the fixed project. Names are sanitized
    /// display copies; ids are the identity.
    pub(crate) project_choices: Vec<(String, String)>,
    /// Index into `project_choices` of the chosen target project.
    pub(crate) project_choice: usize,
    /// Edit mode only: the project the entry is on now, so saving can tell
    /// a move from a no-op. New mode leaves this equal to `project_id`.
    pub(crate) orig_project_id: String,
    pub(crate) description: String,
    pub(crate) start: String,
    pub(crate) end: String,
    /// Edit mode only: the current endpoints, for the "keep" fallback and
    /// the labels. New mode leaves these at zero and never reads them.
    pub(crate) orig_start: i64,
    pub(crate) orig_end: i64,
    pub(crate) orig_description: String,
    pub(crate) field: DialogField,
    /// The caret (and any selection) in the focused text field (#109).
    /// Reset to the field's end whenever focus moves.
    pub(crate) caret: crate::app::textline::Caret,
    pub(crate) error: Option<String>,
}

impl EntryDialog {
    /// A new-entry dialog for `project`, with "end = now" pre-filled.
    pub(crate) fn new(project_id: String, project_name: String) -> Self {
        let project_name = timetrack_core::sanitize_display(&project_name);
        EntryDialog {
            edit_id: None,
            orig_project_id: project_id.clone(),
            project_choices: vec![(project_id.clone(), project_name.clone())],
            project_choice: 0,
            project_id,
            project_name,
            description: String::new(),
            start: "-60".to_string(),
            end: "now".to_string(),
            orig_start: 0,
            orig_end: 0,
            orig_description: String::new(),
            field: DialogField::Description,
            caret: crate::app::textline::Caret::default(),
            error: None,
        }
    }

    /// An edit dialog for the entry with these current values. Times start
    /// empty, meaning "keep the current endpoint".
    ///
    /// `project_id` is the entry's current project and `project_choices`
    /// the projects it may move to (usually the active ones). The current
    /// project is always represented — prepended when the caller did not
    /// include it (an archived project, say) — so "unchanged" is always a
    /// choice the field can show.
    pub(crate) fn edit(
        id: String,
        project_id: String,
        project_choices: Vec<(String, String)>,
        description: String,
        started_at: i64,
        ended_at: i64,
    ) -> Self {
        let mut choices: Vec<(String, String)> = project_choices
            .into_iter()
            .map(|(id, name)| (id, timetrack_core::sanitize_display(&name)))
            .collect();
        let choice = match choices.iter().position(|(cid, _)| *cid == project_id) {
            Some(i) => i,
            None => {
                choices.insert(0, (project_id.clone(), project_id.clone()));
                0
            }
        };
        EntryDialog {
            edit_id: Some(id),
            project_name: choices[choice].1.clone(),
            orig_project_id: project_id.clone(),
            project_choices: choices,
            project_choice: choice,
            project_id,
            description: description.clone(),
            start: String::new(),
            end: String::new(),
            caret: crate::app::textline::Caret::at_end(&description),
            orig_start: started_at,
            orig_end: ended_at,
            orig_description: description,
            field: DialogField::Description,
            error: None,
        }
    }

    /// The project the dialog currently targets, as (id, display name).
    pub(crate) fn chosen_project(&self) -> (&str, &str) {
        let (id, name) = &self.project_choices[self.project_choice];
        (id.as_str(), name.as_str())
    }

    /// Move the project choice by `delta`, wrapping at both ends. Clears a
    /// stale error like any other edit.
    pub(crate) fn cycle_project(&mut self, delta: i32) {
        let n = self.project_choices.len() as i32;
        if n > 1 {
            self.project_choice = (self.project_choice as i32 + delta).rem_euclid(n) as usize;
            self.error = None;
        }
    }

    /// Tab to the next field. The project field exists only in edit mode,
    /// so new-entry dialogs step over it.
    pub(crate) fn next_field(&mut self) {
        self.field = self.field.next();
        if self.field == DialogField::Project && self.edit_id.is_none() {
            self.field = self.field.next();
        }
        self.reset_caret();
    }

    /// Shift-Tab to the previous field, skipping like `next_field`.
    pub(crate) fn prev_field(&mut self) {
        self.field = self.field.prev();
        if self.field == DialogField::Project && self.edit_id.is_none() {
            self.field = self.field.prev();
        }
        self.reset_caret();
    }

    /// The focused text field and its caret, or `None` on the project
    /// field, which cycles instead of taking text.
    pub(crate) fn focused_text_mut(
        &mut self,
    ) -> Option<(&mut String, &mut crate::app::textline::Caret)> {
        let field = self.field;
        let text = match field {
            DialogField::Description => &mut self.description,
            DialogField::Project => return None,
            DialogField::Start => &mut self.start,
            DialogField::End => &mut self.end,
        };
        Some((text, &mut self.caret))
    }

    /// Put the caret at the end of the newly focused field: where
    /// append-style typing used to put it, and the least surprising
    /// place to arrive.
    fn reset_caret(&mut self) {
        let len = match self.field {
            DialogField::Description => self.description.len(),
            DialogField::Project => 0,
            DialogField::Start => self.start.len(),
            DialogField::End => self.end.len(),
        };
        self.caret = crate::app::textline::Caret {
            pos: len,
            anchor: None,
        };
    }

    /// Apply an editing key (caret movement, selection, deletes) to the
    /// focused field. Returns false when the key was not an editing key
    /// or the field takes no text.
    pub(crate) fn edit_key(&mut self, key: &str, shift: bool, ctrl: bool) -> bool {
        let Some((text, caret)) = self.focused_text_mut() else {
            return false;
        };
        let handled = crate::app::textline::handle_edit_key(text, caret, key, shift, ctrl);
        if handled {
            self.error = None;
        }
        handled
    }

    /// Label for the start row: the hint in edit mode says what empty keeps.
    pub(crate) fn start_hint(&self, tz: &timetrack_core::Tz) -> String {
        match &self.edit_id {
            None => "(-90, 90, now, HH:MM)".to_string(),
            Some(_) => format!(
                "(empty keeps {})",
                timetrack_core::format_local_hm(self.orig_start, tz)
            ),
        }
    }

    /// Label for the end row, as above.
    pub(crate) fn end_hint(&self, tz: &timetrack_core::Tz) -> String {
        match &self.edit_id {
            None => "(-90, 90, now, HH:MM)".to_string(),
            Some(_) => format!(
                "(empty keeps {})",
                timetrack_core::format_local_hm(self.orig_end, tz)
            ),
        }
    }

    /// Insert typed text at the caret in the focused field, replacing
    /// any selection. Any edit clears a stale error:
    /// the message described the text as it was, not as it is now.
    ///
    /// The project field takes no text: it is cycled (`cycle_project`), so
    /// keystrokes there simply do not land anywhere.
    pub(crate) fn type_text(&mut self, ch: &str) {
        let Some((text, caret)) = self.focused_text_mut() else {
            return;
        };
        crate::app::textline::insert_text(text, caret, ch);
        self.error = None;
    }

    /// Delete backwards in the focused field: the selection if one is
    /// held, else the character before the caret.
    pub(crate) fn backspace(&mut self) {
        let Some((text, caret)) = self.focused_text_mut() else {
            return;
        };
        crate::app::textline::backspace(text, caret);
        self.error = None;
    }
}

/// What saving a dialog means, resolved purely from its strings.
///
/// Pure over the dialog plus an explicit clock and offset, so the whole
/// save path — parsing, the end-before-start refusal, the keep-fallback — is
/// testable without a view or a service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SaveOutcome {
    /// Create `[started_at, ended_at]` on `project_id` with `description`.
    Create {
        project_id: String,
        description: String,
        started_at: i64,
        ended_at: i64,
    },
    /// Move `id`'s endpoints, rewrite the description when present, and
    /// move it to `project_id` when present (#104).
    Update {
        id: String,
        started_at: i64,
        ended_at: i64,
        description: Option<String>,
        project_id: Option<String>,
    },
    /// Nothing changed; close without calling the service.
    NoChange,
    /// Keep the dialog open and show this instead.
    Invalid(String),
}

pub(crate) fn resolve_save(dlg: &EntryDialog, now: i64, tz: &timetrack_core::Tz) -> SaveOutcome {
    match &dlg.edit_id {
        None => {
            if dlg.start.trim().is_empty() {
                return SaveOutcome::Invalid("start cannot be empty".to_string());
            }
            if dlg.end.trim().is_empty() {
                return SaveOutcome::Invalid("end cannot be empty".to_string());
            }
            let started_at = match timetrack_core::parse_moment(&dlg.start, "start", now, tz) {
                Ok(t) => t,
                Err(e) => return SaveOutcome::Invalid(e.to_string()),
            };
            let ended_at = match timetrack_core::parse_moment(&dlg.end, "end", now, tz) {
                Ok(t) => t,
                Err(e) => return SaveOutcome::Invalid(e.to_string()),
            };
            if ended_at < started_at {
                return SaveOutcome::Invalid(
                    timetrack_core::RuleError::NegativeDuration.to_string(),
                );
            }
            SaveOutcome::Create {
                project_id: dlg.project_id.clone(),
                description: dlg.description.clone(),
                started_at,
                ended_at,
            }
        }
        Some(id) => {
            let started_at = if dlg.start.trim().is_empty() {
                dlg.orig_start
            } else {
                match timetrack_core::parse_moment(&dlg.start, "start", now, tz) {
                    Ok(t) => t,
                    Err(e) => return SaveOutcome::Invalid(e.to_string()),
                }
            };
            let ended_at = if dlg.end.trim().is_empty() {
                dlg.orig_end
            } else {
                match timetrack_core::parse_moment(&dlg.end, "end", now, tz) {
                    Ok(t) => t,
                    Err(e) => return SaveOutcome::Invalid(e.to_string()),
                }
            };
            if ended_at < started_at {
                return SaveOutcome::Invalid(
                    timetrack_core::RuleError::NegativeDuration.to_string(),
                );
            }
            let description =
                (dlg.description != dlg.orig_description).then(|| dlg.description.clone());
            let (chosen_id, _) = dlg.chosen_project();
            let project_id = (chosen_id != dlg.orig_project_id).then(|| chosen_id.to_string());
            if started_at == dlg.orig_start
                && ended_at == dlg.orig_end
                && description.is_none()
                && project_id.is_none()
            {
                return SaveOutcome::NoChange;
            }
            SaveOutcome::Update {
                id: id.clone(),
                started_at,
                ended_at,
                description,
                project_id,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::testsupport::new_dialog;

    /// An edit dialog for e1 on p1 ("Work"), with p2 ("Home") to move to.
    fn edit_dialog(description: &str, started_at: i64, ended_at: i64) -> EntryDialog {
        EntryDialog::edit(
            "e1".into(),
            "p1".into(),
            vec![("p1".into(), "Work".into()), ("p2".into(), "Home".into())],
            description.into(),
            started_at,
            ended_at,
        )
    }

    #[test]
    fn a_new_dialog_prefills_end_at_now() {
        // Method 2 is method 1 with "end = now" pre-filled (§5): the common
        // case needs no typing at all.
        let dlg = new_dialog();
        assert_eq!(dlg.start, "-60");
        assert_eq!(dlg.end, "now");
        assert_eq!(dlg.field, DialogField::Description);
    }

    #[test]
    fn a_typo_mid_word_is_fixed_without_deleting_back_to_it() {
        // The scenario that opened #109: "caffé" typed for the
        // description, then fixed by moving the caret, not by
        // backspacing through the word.
        let mut dlg = new_dialog();
        dlg.type_text("caffé");
        assert!(dlg.edit_key("left", false, false));
        assert!(dlg.edit_key("left", false, false));
        dlg.type_text("e");
        assert_eq!(dlg.description, "cafefé");
        assert!(dlg.edit_key("a", false, true), "ctrl-a selects all");
        dlg.type_text("caffè");
        assert_eq!(dlg.description, "caffè");
        // Tabbing re-seats the caret at the next field's end.
        dlg.next_field();
        assert_eq!(dlg.field, DialogField::Start);
        assert_eq!(dlg.caret.pos, dlg.start.len());
    }

    #[test]
    fn typing_appends_and_backspace_deletes() {
        let mut dlg = new_dialog();
        dlg.type_text("smoke");
        assert_eq!(dlg.description, "smoke");
        dlg.backspace();
        assert_eq!(dlg.description, "smok");
    }

    #[test]
    fn tab_cycles_through_the_fields() {
        // New mode has no project field to land on: its project is fixed.
        let mut dlg = new_dialog();
        assert_eq!(dlg.field, DialogField::Description);
        dlg.next_field();
        assert_eq!(dlg.field, DialogField::Start);
        dlg.next_field();
        assert_eq!(dlg.field, DialogField::End);
        dlg.next_field();
        assert_eq!(dlg.field, DialogField::Description);
        dlg.prev_field();
        assert_eq!(dlg.field, DialogField::End);
        // Edit mode cycles through the project field between them.
        let mut dlg = edit_dialog("kept", 0, 3_600_000);
        dlg.next_field();
        assert_eq!(dlg.field, DialogField::Project);
        dlg.next_field();
        assert_eq!(dlg.field, DialogField::Start);
        dlg.prev_field();
        assert_eq!(dlg.field, DialogField::Project);
        dlg.prev_field();
        assert_eq!(dlg.field, DialogField::Description);
    }

    #[test]
    fn saving_a_new_dialog_creates_the_interval() {
        let mut dlg = new_dialog();
        dlg.description = "review".into();
        // now = 12:00 UTC; -60 → 11:00, now → 12:00.
        let now = 12 * 3_600_000;
        let utc = timetrack_core::Tz::utc();
        match resolve_save(&dlg, now, &utc) {
            SaveOutcome::Create {
                project_id,
                description,
                started_at,
                ended_at,
            } => {
                assert_eq!(project_id, "p1");
                assert_eq!(description, "review");
                assert_eq!(started_at, 11 * 3_600_000);
                assert_eq!(ended_at, now);
            }
            other => panic!("expected Create, got {other:?}"),
        }
    }

    #[test]
    fn saving_on_a_clock_change_day_keeps_the_typed_wall_clocks() {
        // #102: the dialog parses through the same core parser and the
        // snapshot's zone as the CLI, so on Rome's spring-forward day the
        // created interval must carry the wall clocks the user typed,
        // not the hour-late pair midnight anchoring produced.
        let rome = timetrack_core::Tz::parse("Europe/Rome").unwrap();
        let now = timetrack_core::parse_anchor_day("2026-03-29", "anchor", &rome).unwrap();
        let mut dlg = new_dialog();
        dlg.start = "00:30".into();
        dlg.end = "09:30".into();
        match resolve_save(&dlg, now, &rome) {
            SaveOutcome::Create {
                started_at,
                ended_at,
                ..
            } => {
                assert_eq!(timetrack_core::format_local_hm(started_at, &rome), "00:30");
                assert_eq!(timetrack_core::format_local_hm(ended_at, &rome), "09:30");
                assert_eq!(rome.day_of(started_at), rome.day_of(now));
                assert_eq!(rome.day_of(ended_at), rome.day_of(now));
            }
            other => panic!("expected Create, got {other:?}"),
        }
    }

    #[test]
    fn a_backwards_interval_stays_open_with_an_error() {
        let mut dlg = new_dialog();
        dlg.start = "now".into();
        dlg.end = "-60".into();
        let utc = timetrack_core::Tz::utc();
        match resolve_save(&dlg, 12 * 3_600_000, &utc) {
            SaveOutcome::Invalid(msg) => assert!(msg.contains("must not end before")),
            other => panic!("expected Invalid, got {other:?}"),
        }
        // And the dialog keeps the error for the panel to show.
        dlg.error = Some("x".into());
        dlg.field = dlg.field.next();
        assert_eq!(dlg.error, Some("x".into()));
    }

    #[test]
    fn typing_clears_a_stale_error() {
        let mut dlg = new_dialog();
        dlg.error = Some("bad".into());
        dlg.type_text("x");
        assert_eq!(dlg.error, None);
    }

    #[test]
    fn an_untouched_edit_dialog_is_a_no_op() {
        let dlg = edit_dialog("kept", 0, 3_600_000);
        let utc = timetrack_core::Tz::utc();
        assert_eq!(
            resolve_save(&dlg, 12 * 3_600_000, &utc),
            SaveOutcome::NoChange
        );
    }

    #[test]
    fn empty_times_keep_the_current_endpoints() {
        // The dialog form of `shorten`: only the typed endpoint moves, and
        // the entry is never at risk.
        let mut dlg = edit_dialog("kept", 0, 3_600_000);
        dlg.end = "-60".into();
        let now = 12 * 3_600_000;
        let utc = timetrack_core::Tz::utc();
        match resolve_save(&dlg, now, &utc) {
            SaveOutcome::Update {
                id,
                started_at,
                ended_at,
                description,
                project_id,
            } => {
                assert_eq!(id, "e1");
                assert_eq!(started_at, 0, "untouched start is kept");
                assert_eq!(ended_at, now - 3_600_000);
                assert_eq!(description, None, "untouched text sends no SetText");
                assert_eq!(project_id, None, "untouched project sends no SetProject");
            }
            other => panic!("expected Update, got {other:?}"),
        }
    }

    #[test]
    fn rewritten_text_sends_a_description_update() {
        let mut dlg = edit_dialog("typo", 0, 3_600_000);
        dlg.description = "review".into();
        let utc = timetrack_core::Tz::utc();
        match resolve_save(&dlg, 12 * 3_600_000, &utc) {
            SaveOutcome::Update { description, .. } => {
                assert_eq!(description.as_deref(), Some("review"))
            }
            other => panic!("expected Update, got {other:?}"),
        }
    }

    #[test]
    fn the_keep_hints_name_the_current_times() {
        // 01:00 and 02:00 UTC.
        let dlg = edit_dialog("", 3_600_000, 7_200_000);
        let utc = timetrack_core::Tz::utc();
        assert_eq!(dlg.start_hint(&utc), "(empty keeps 01:00)");
        assert_eq!(dlg.end_hint(&utc), "(empty keeps 02:00)");
    }

    #[test]
    fn cycling_the_project_sends_a_move_and_nothing_else() {
        // #104: moving an entry is its own change — untouched times and
        // text send no SetTimes/SetText alongside it.
        let mut dlg = edit_dialog("kept", 0, 3_600_000);
        assert_eq!(dlg.chosen_project(), ("p1", "Work"));
        dlg.cycle_project(1);
        assert_eq!(dlg.chosen_project(), ("p2", "Home"));
        let utc = timetrack_core::Tz::utc();
        match resolve_save(&dlg, 12 * 3_600_000, &utc) {
            SaveOutcome::Update {
                description,
                project_id,
                ..
            } => {
                assert_eq!(project_id.as_deref(), Some("p2"));
                assert_eq!(description, None);
            }
            other => panic!("expected Update, got {other:?}"),
        }
        // Cycling wraps, and landing back on the original is a no-op again.
        dlg.cycle_project(1);
        assert_eq!(dlg.chosen_project().0, "p1");
        assert_eq!(
            resolve_save(&dlg, 12 * 3_600_000, &utc),
            SaveOutcome::NoChange
        );
        dlg.cycle_project(-1);
        assert_eq!(dlg.chosen_project().0, "p2");
    }

    #[test]
    fn an_archived_current_project_stays_representable() {
        // The entry lives on a project the caller did not offer (archived):
        // it is prepended, so "unchanged" is still what the field shows.
        let dlg = EntryDialog::edit(
            "e1".into(),
            "p9".into(),
            vec![("p1".into(), "Work".into())],
            "kept".into(),
            0,
            3_600_000,
        );
        assert_eq!(dlg.chosen_project().0, "p9");
        assert_eq!(dlg.project_choices.len(), 2);
        let utc = timetrack_core::Tz::utc();
        assert_eq!(resolve_save(&dlg, 0, &utc), SaveOutcome::NoChange);
    }

    #[test]
    fn the_project_field_takes_no_text() {
        let mut dlg = edit_dialog("kept", 0, 3_600_000);
        dlg.field = DialogField::Project;
        dlg.type_text("x");
        dlg.backspace();
        assert_eq!(dlg.description, "kept");
        assert_eq!(dlg.chosen_project().0, "p1", "typing never moves the entry");
    }

    #[test]
    fn the_keep_hints_use_the_named_zone() {
        // 22:30 UTC is 00:30 the next day in Rome (summer, +2).
        let rome = timetrack_core::Tz::parse("Europe/Rome").unwrap();
        let dlg = edit_dialog("", 1_783_290_600_000, 1_783_290_600_000 + 3_600_000);
        assert_eq!(dlg.start_hint(&rome), "(empty keeps 00:30)");
    }
}
