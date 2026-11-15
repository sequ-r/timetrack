/* gui/app/dialogs/source.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The source dialog (#153): register or edit an external calendar/task
//! source — kind, locator, label, and project mapping.
//!
//! There is deliberately no password field: CalDAV secrets live in the
//! OS credential store and are managed through the CLI flows (#149),
//! which the dialog says in a hint line rather than by offering a box
//! a secret must never be typed into.
//!
//! Keystroke editing with a caret and selection like the other prompts,
//! and likewise pure state so resolving is unit-testable without a
//! window. Kind and project cycle (`h`/`l`, or a click) instead of
//! taking text, like the entry dialog's project field.

/// The source kinds the service accepts, in cycle order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SourceKind {
    IcsFile,
    IcsUrl,
    Caldav,
}

impl SourceKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            SourceKind::IcsFile => "ics_file",
            SourceKind::IcsUrl => "ics_url",
            SourceKind::Caldav => "caldav",
        }
    }

    /// Parse a stored kind back. `None` for anything the service of
    /// this build cannot have written — the caller refuses to edit
    /// rather than silently re-saving an unknown kind as something else.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        match text {
            "ics_file" => Some(SourceKind::IcsFile),
            "ics_url" => Some(SourceKind::IcsUrl),
            "caldav" => Some(SourceKind::Caldav),
            _ => None,
        }
    }

    /// Move the kind by `delta`, wrapping at both ends.
    pub(crate) fn cycle(self, delta: i32) -> Self {
        const ORDER: [SourceKind; 3] =
            [SourceKind::IcsFile, SourceKind::IcsUrl, SourceKind::Caldav];
        let at = ORDER.iter().position(|k| *k == self).unwrap_or(0);
        ORDER[(at as i32 + delta).rem_euclid(ORDER.len() as i32) as usize]
    }
}

/// Which field of the source dialog receives keystrokes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SourceField {
    Kind,
    Locator,
    Label,
    Project,
}

impl SourceField {
    pub(crate) fn next(self) -> Self {
        match self {
            SourceField::Kind => SourceField::Locator,
            SourceField::Locator => SourceField::Label,
            SourceField::Label => SourceField::Project,
            SourceField::Project => SourceField::Kind,
        }
    }

    pub(crate) fn prev(self) -> Self {
        match self {
            SourceField::Kind => SourceField::Project,
            SourceField::Locator => SourceField::Kind,
            SourceField::Label => SourceField::Locator,
            SourceField::Project => SourceField::Label,
        }
    }
}

/// The source dialog: `edit_id` none to register, set to edit.
/// Everything is plain state over strings, like the other prompts.
#[derive(Debug, Clone)]
pub(crate) struct SourceDialog {
    /// `None` to register a source; `Some(id)` when editing that one.
    pub(crate) edit_id: Option<String>,
    pub(crate) kind: SourceKind,
    pub(crate) locator: String,
    pub(crate) label: String,
    /// The mapping choices as (id, display name) pairs, with unmapped
    /// first. The current mapping is always represented — prepended
    /// when the caller did not include it — so "unchanged" is always a
    /// choice the field can show. Names are sanitized display copies;
    /// ids are the identity.
    pub(crate) project_choices: Vec<(Option<String>, String)>,
    /// Index into `project_choices` of the chosen mapping.
    pub(crate) project_choice: usize,
    pub(crate) field: SourceField,
    /// The caret (and any selection) in the focused text field (#109).
    /// Reset to the field's end whenever focus moves.
    pub(crate) caret: crate::app::textline::Caret,
    pub(crate) error: Option<String>,
}

/// The mapping choices for a dialog: unmapped first, then the given
/// (id, sanitized name) pairs, with `current` ensured present.
fn with_current_mapping(
    mut choices: Vec<(String, String)>,
    current: Option<&str>,
) -> (Vec<(Option<String>, String)>, usize) {
    let mut out: Vec<(Option<String>, String)> = vec![(None, "unmapped".to_string())];
    out.extend(
        choices
            .drain(..)
            .map(|(id, name)| (Some(id), timetrack_core::sanitize_display(&name))),
    );
    let at = match current {
        None => 0,
        Some(want) => match out.iter().position(|(id, _)| id.as_deref() == Some(want)) {
            Some(i) => i,
            None => {
                out.insert(1, (Some(want.to_string()), want.to_string()));
                1
            }
        },
    };
    (out, at)
}

impl SourceDialog {
    /// A registration dialog: file kind, blank locator and label (the
    /// service derives the label from the locator when empty), no
    /// mapping. Focus starts on the locator — the one thing that must
    /// be typed.
    pub(crate) fn new(project_choices: Vec<(String, String)>) -> Self {
        let (choices, at) = with_current_mapping(project_choices, None);
        SourceDialog {
            edit_id: None,
            kind: SourceKind::IcsFile,
            locator: String::new(),
            label: String::new(),
            project_choices: choices,
            project_choice: at,
            field: SourceField::Locator,
            caret: crate::app::textline::Caret::default(),
            error: None,
        }
    }

    /// An edit dialog for the source with these current values.
    pub(crate) fn edit(
        id: String,
        kind: SourceKind,
        locator: String,
        label: String,
        project_id: Option<String>,
        project_choices: Vec<(String, String)>,
    ) -> Self {
        let (choices, at) = with_current_mapping(project_choices, project_id.as_deref());
        let caret = crate::app::textline::Caret::at_end(&locator);
        SourceDialog {
            edit_id: Some(id),
            kind,
            locator,
            label,
            project_choices: choices,
            project_choice: at,
            field: SourceField::Locator,
            caret,
            error: None,
        }
    }

    /// The mapping the dialog currently shows, as (id, display name).
    pub(crate) fn chosen_project(&self) -> (Option<&str>, &str) {
        let (id, name) = &self.project_choices[self.project_choice];
        (id.as_deref(), name.as_str())
    }

    /// Move the kind by `delta`, wrapping at both ends. Clears a stale
    /// error like any other edit.
    pub(crate) fn cycle_kind(&mut self, delta: i32) {
        self.kind = self.kind.cycle(delta);
        self.error = None;
    }

    /// Move the mapping choice by `delta`, wrapping at both ends.
    /// Clears a stale error like any other edit.
    pub(crate) fn cycle_project(&mut self, delta: i32) {
        let n = self.project_choices.len() as i32;
        if n > 1 {
            self.project_choice = (self.project_choice as i32 + delta).rem_euclid(n) as usize;
            self.error = None;
        }
    }

    /// Tab to the next field.
    pub(crate) fn next_field(&mut self) {
        self.field = self.field.next();
        self.reset_caret();
    }

    /// Focus `field` directly: the mouse route to the same place Tab
    /// goes, seating the caret at that field's end like `next_field`.
    pub(crate) fn focus_field(&mut self, field: SourceField) {
        self.field = field;
        self.reset_caret();
    }

    /// Shift-Tab to the previous field.
    pub(crate) fn prev_field(&mut self) {
        self.field = self.field.prev();
        self.reset_caret();
    }

    /// The focused text field and its caret, or `None` on the kind and
    /// project fields, which cycle instead of taking text.
    pub(crate) fn focused_text_mut(
        &mut self,
    ) -> Option<(&mut String, &mut crate::app::textline::Caret)> {
        let field = self.field;
        let text = match field {
            SourceField::Kind | SourceField::Project => return None,
            SourceField::Locator => &mut self.locator,
            SourceField::Label => &mut self.label,
        };
        Some((text, &mut self.caret))
    }

    /// The focused text field and its caret, read-only: what the input
    /// handler reports to the platform. `None` on the cycling fields,
    /// which take no text — and so draw no input handler.
    pub(crate) fn focused_text(&self) -> Option<(&str, &crate::app::textline::Caret)> {
        let text = match self.field {
            SourceField::Kind | SourceField::Project => return None,
            SourceField::Locator => &self.locator,
            SourceField::Label => &self.label,
        };
        Some((text, &self.caret))
    }

    /// Put the caret at the end of the newly focused field: where
    /// append-style typing used to put it, and the least surprising
    /// place to arrive.
    fn reset_caret(&mut self) {
        let len = match self.field {
            SourceField::Kind | SourceField::Project => 0,
            SourceField::Locator => self.locator.len(),
            SourceField::Label => self.label.len(),
        };
        self.caret = crate::app::textline::Caret {
            pos: len,
            anchor: None,
            marked: None,
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

    /// Insert typed text at the caret in the focused field, replacing
    /// any selection. Any edit clears a stale error: the message
    /// described the text as it was, not as it is now.
    ///
    /// The cycling fields take no text, so keystrokes there simply do
    /// not land anywhere.
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

    /// Apply a platform text-input edit (IME preedit or commit, #126)
    /// to the focused field, clearing a stale error like any edit.
    /// False on the cycling fields, which take no text.
    pub(crate) fn ime_edit(&mut self, edit: crate::app::textline::ImeEdit) -> bool {
        let Some((text, caret)) = self.focused_text_mut() else {
            return false;
        };
        crate::app::textline::apply_ime_edit(text, caret, edit);
        self.error = None;
        true
    }

    /// Press the mouse at a text offset in `field` (#127): focuses the
    /// field like Tab, then places the caret — or extends to it with
    /// shift held in the already-focused field. A press never clears
    /// the error: it edits no text.
    pub(crate) fn mouse_down(&mut self, field: SourceField, at: usize, shift: bool) {
        if field != self.field {
            self.focus_field(field);
            if self.field != field {
                return;
            }
            // A newly focused field has no selection to extend.
            if let Some((text, caret)) = self.focused_text_mut() {
                crate::app::textline::place_caret(text, caret, at);
            }
            return;
        }
        let Some((text, caret)) = self.focused_text_mut() else {
            return;
        };
        if shift {
            crate::app::textline::extend_caret_to(text, caret, at);
        } else {
            crate::app::textline::place_caret(text, caret, at);
        }
    }

    /// Drag the mouse to a text offset in the focused field (#127):
    /// extends the press point's selection.
    pub(crate) fn mouse_drag(&mut self, at: usize) {
        let Some((text, caret)) = self.focused_text_mut() else {
            return;
        };
        crate::app::textline::extend_caret_to(text, caret, at);
    }
}

/// What saving a source dialog means: the full new state, for
/// registration or for the service's full-replace edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SourceSave {
    Create {
        kind: String,
        locator: String,
        project_id: Option<String>,
        label: String,
    },
    Update {
        id: String,
        kind: String,
        locator: String,
        project_id: Option<String>,
        label: String,
    },
}

/// Resolve a source dialog. A blank locator stays open with the
/// service's own sentence; everything harder (unknown kind — which the
/// cycling field cannot produce — duplicates, a CalDAV password in the
/// URL) travels to the service, which owns source validity and reports
/// it typed. An empty label travels as-is: the service derives it from
/// the locator, as on registration.
pub(crate) fn resolve_source_dialog(dlg: &SourceDialog) -> Result<SourceSave, String> {
    if dlg.locator.trim().is_empty() {
        return Err("a source needs a file path or URL".to_string());
    }
    let kind = dlg.kind.as_str().to_string();
    let locator = dlg.locator.trim().to_string();
    let label = dlg.label.trim().to_string();
    let (project_id, _) = dlg.chosen_project();
    let project_id = project_id.map(str::to_string);
    match &dlg.edit_id {
        None => Ok(SourceSave::Create {
            kind,
            locator,
            project_id,
            label,
        }),
        Some(id) => Ok(SourceSave::Update {
            id: id.clone(),
            kind,
            locator,
            project_id,
            label,
        }),
    }
}

/// A pending source removal: the id to send, and the one-line summary
/// the banner shows.
#[derive(Debug, Clone)]
pub(crate) struct SourceRemoveConfirm {
    pub(crate) id: String,
    pub(crate) summary: String,
}

/// Build the removal confirmation for a source: exactly what goes with
/// it (its still-pending suggestions) and what stays (decided rows and
/// confirmed entries), so the destructive click is never unread.
pub(crate) fn source_remove_offer(label: &str, id: &str, pending: usize) -> SourceRemoveConfirm {
    let label = timetrack_core::sanitize_display(label);
    let pending_note = match pending {
        0 => "nothing pending goes with it".to_string(),
        1 => "1 pending suggestion goes with it".to_string(),
        n => format!("{n} pending suggestions go with it"),
    };
    SourceRemoveConfirm {
        summary: format!(
            "REMOVE SOURCE {label} ({id})? {pending_note}; decided suggestions and confirmed entries stay."
        ),
        id: id.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choices() -> Vec<(String, String)> {
        vec![("p1".into(), "Work".into()), ("p2".into(), "Home".into())]
    }

    #[test]
    fn kinds_cycle_and_parse_in_both_directions() {
        assert_eq!(SourceKind::IcsFile.as_str(), "ics_file");
        assert_eq!(SourceKind::IcsUrl.as_str(), "ics_url");
        assert_eq!(SourceKind::Caldav.as_str(), "caldav");
        for kind in [SourceKind::IcsFile, SourceKind::IcsUrl, SourceKind::Caldav] {
            assert_eq!(SourceKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(SourceKind::parse("bogus"), None);
        assert_eq!(SourceKind::IcsFile.cycle(1), SourceKind::IcsUrl);
        assert_eq!(SourceKind::Caldav.cycle(1), SourceKind::IcsFile);
        assert_eq!(SourceKind::IcsFile.cycle(-1), SourceKind::Caldav);
    }

    #[test]
    fn fields_tab_through_all_four() {
        assert_eq!(SourceField::Kind.next(), SourceField::Locator);
        assert_eq!(SourceField::Locator.next(), SourceField::Label);
        assert_eq!(SourceField::Label.next(), SourceField::Project);
        assert_eq!(SourceField::Project.next(), SourceField::Kind);
        assert_eq!(SourceField::Kind.prev(), SourceField::Project);
        assert_eq!(SourceField::Project.prev(), SourceField::Label);
    }

    #[test]
    fn a_new_dialog_starts_unmapped_on_the_locator() {
        let dlg = SourceDialog::new(choices());
        assert_eq!(dlg.edit_id, None);
        assert_eq!(dlg.kind, SourceKind::IcsFile);
        assert_eq!(dlg.field, SourceField::Locator);
        assert_eq!(dlg.chosen_project(), (None, "unmapped"));
    }

    #[test]
    fn an_edit_dialog_prefills_and_keeps_a_missing_mapping() {
        let dlg = SourceDialog::edit(
            "src-1".into(),
            SourceKind::Caldav,
            "https://cal.test/u/".into(),
            "Cal".into(),
            Some("p9".into()),
            choices(),
        );
        assert_eq!(dlg.edit_id.as_deref(), Some("src-1"));
        assert_eq!(dlg.kind, SourceKind::Caldav);
        // The mapping names a project the choices lack (archived or
        // deleted since): it is still shown, by id, as the choice.
        assert_eq!(dlg.chosen_project(), (Some("p9"), "p9"));
    }

    #[test]
    fn cycling_clears_a_stale_error() {
        let mut dlg = SourceDialog::new(choices());
        dlg.error = Some("stale".into());
        dlg.cycle_kind(1);
        assert_eq!(dlg.kind, SourceKind::IcsUrl);
        assert_eq!(dlg.error, None);
        dlg.error = Some("stale".into());
        dlg.cycle_project(1);
        assert_eq!(dlg.chosen_project(), (Some("p1"), "Work"));
        assert_eq!(dlg.error, None);
        dlg.cycle_project(1);
        dlg.cycle_project(1);
        assert_eq!(dlg.chosen_project(), (None, "unmapped"), "it wraps");
    }

    #[test]
    fn resolve_creates_updates_trims_and_refuses_a_blank_locator() {
        let mut dlg = SourceDialog::new(choices());
        assert_eq!(
            resolve_source_dialog(&dlg).unwrap_err(),
            "a source needs a file path or URL"
        );
        dlg.locator = "  /tmp/cal.ics  ".into();
        dlg.label = " Cal ".into();
        dlg.cycle_project(1);
        assert_eq!(
            resolve_source_dialog(&dlg),
            Ok(SourceSave::Create {
                kind: "ics_file".into(),
                locator: "/tmp/cal.ics".into(),
                project_id: Some("p1".into()),
                label: "Cal".into(),
            })
        );
        let mut edit = SourceDialog::edit(
            "src-1".into(),
            SourceKind::IcsUrl,
            "https://cal.test/c.ics".into(),
            String::new(),
            None,
            choices(),
        );
        assert_eq!(
            resolve_source_dialog(&edit),
            Ok(SourceSave::Update {
                id: "src-1".into(),
                kind: "ics_url".into(),
                locator: "https://cal.test/c.ics".into(),
                project_id: None,
                label: String::new(),
            }),
            "an empty label travels for the service to derive"
        );
        edit.locator = "   ".into();
        assert!(resolve_source_dialog(&edit).is_err());
    }

    #[test]
    fn only_text_fields_take_text_ime_and_edits() {
        use crate::app::textline::ImeEdit;
        let mut dlg = SourceDialog::new(choices());
        dlg.focus_field(SourceField::Kind);
        assert_eq!(dlg.focused_text(), None);
        assert!(!dlg.edit_key("left", false, false));
        assert!(!dlg.ime_edit(ImeEdit::Commit {
            range: None,
            text: "x",
        }));
        dlg.type_text("x");
        dlg.backspace();
        assert_eq!(dlg.locator, "");
        assert_eq!(dlg.label, "");
        dlg.focus_field(SourceField::Project);
        assert_eq!(dlg.focused_text(), None);
        dlg.type_text("x");
        assert_eq!(dlg.chosen_project(), (None, "unmapped"));
    }

    #[test]
    fn ime_edits_compose_the_locator_and_clear_errors() {
        use crate::app::textline::ImeEdit;
        let mut dlg = SourceDialog::new(choices());
        dlg.error = Some("stale".into());
        assert!(dlg.ime_edit(ImeEdit::Mark {
            range: None,
            text: "cal",
            selected: None,
        }));
        assert_eq!(dlg.locator, "cal");
        assert_eq!(dlg.error, None);
        assert!(dlg.ime_edit(ImeEdit::Commit {
            range: None,
            text: "cal.ics",
        }));
        assert_eq!(dlg.locator, "cal.ics");
    }

    #[test]
    fn a_press_focuses_places_and_a_drag_selects() {
        use crate::app::textline::selected_text;
        let mut dlg = SourceDialog::new(choices());
        dlg.locator = "/tmp/cal.ics".into();
        dlg.label = "Cal".into();
        dlg.error = Some("stale".into());
        // A press on the other text field focuses it first, then places.
        dlg.mouse_down(SourceField::Label, 1, false);
        assert_eq!(dlg.field, SourceField::Label);
        assert_eq!(dlg.caret.pos, 1);
        dlg.mouse_drag(3);
        assert_eq!(selected_text(&dlg.label, &dlg.caret), Some("al"));
        assert_eq!(dlg.error.as_deref(), Some("stale"), "a press edits no text");
        // A press on a cycling field focuses it; there is no text to place.
        dlg.mouse_down(SourceField::Kind, 0, false);
        assert_eq!(dlg.field, SourceField::Kind);
    }

    #[test]
    fn the_remove_offer_names_what_goes_and_what_stays() {
        let offer = source_remove_offer("Cal", "src-1", 2);
        assert_eq!(offer.id, "src-1");
        assert!(
            offer.summary.contains("REMOVE SOURCE Cal (src-1)?"),
            "{}",
            offer.summary
        );
        assert!(
            offer.summary.contains("2 pending suggestions go"),
            "{}",
            offer.summary
        );
        assert!(
            offer
                .summary
                .contains("decided suggestions and confirmed entries stay"),
            "{}",
            offer.summary
        );
        let one = source_remove_offer("Cal", "src-1", 1);
        assert!(
            one.summary.contains("1 pending suggestion goes"),
            "{}",
            one.summary
        );
        let none = source_remove_offer("Cal", "src-1", 0);
        assert!(
            none.summary.contains("nothing pending goes"),
            "{}",
            none.summary
        );
        let dirty = source_remove_offer("Ca\nl\u{1b}[31m", "src-1", 0);
        assert!(
            !dirty.summary.chars().any(|c| c.is_control()),
            "{}",
            dirty.summary
        );
    }
}
