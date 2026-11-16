/* gui/app/dialogs/source.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The source dialog (#153): register or edit an external calendar/task
//! source — kind, locator, label, and project mapping.
//!
//! A CalDAV source also takes its credential here (user name plus a
//! masked password), so setup finishes without the CLI (#149 flows stay
//! available there). The secret is held only as dialog state, travels
//! over the socket as a redacted [`SecretString`], and is stored by the
//! service in the OS credential store — never in SQLite, never logged.
//! A blank password means "leave the stored credential alone"; the
//! removal toggle is the explicit way to forget one. The secret field
//! is cleared as soon as a save is sent, and `Debug` never prints it.
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
    /// The CalDAV login name (CalDAV only).
    Username,
    /// The CalDAV secret, masked on screen (CalDAV only).
    Secret,
    /// Forget the stored credential on save (edit + stored credential).
    RemoveCredential,
}

impl SourceField {
    pub(crate) fn next(self) -> Self {
        match self {
            SourceField::Kind => SourceField::Locator,
            SourceField::Locator => SourceField::Label,
            SourceField::Label => SourceField::Project,
            SourceField::Project => SourceField::Username,
            SourceField::Username => SourceField::Secret,
            SourceField::Secret => SourceField::RemoveCredential,
            SourceField::RemoveCredential => SourceField::Kind,
        }
    }

    pub(crate) fn prev(self) -> Self {
        match self {
            SourceField::Kind => SourceField::RemoveCredential,
            SourceField::Locator => SourceField::Kind,
            SourceField::Label => SourceField::Locator,
            SourceField::Project => SourceField::Label,
            SourceField::Username => SourceField::Project,
            SourceField::Secret => SourceField::Username,
            SourceField::RemoveCredential => SourceField::Secret,
        }
    }
}

/// The source dialog: `edit_id` none to register, set to edit.
/// Everything is plain state over strings, like the other prompts.
/// Hand-written `Debug` redacts the secret, like `SecretString`.
#[derive(Clone)]
pub(crate) struct SourceDialog {
    /// `None` to register a source; `Some(id)` when editing that one.
    pub(crate) edit_id: Option<String>,
    pub(crate) kind: SourceKind,
    pub(crate) locator: String,
    pub(crate) label: String,
    /// The CalDAV login name, prefilled from the stored credential.
    pub(crate) username: String,
    /// The login name the dialog opened with: changing it without
    /// typing a password cannot take effect (the service stores a
    /// name only together with a secret), and resolving says so
    /// instead of silently dropping the edit.
    pub(crate) stored_username: String,
    /// The CalDAV secret as typed. Never prefilled — the service
    /// cannot hand a stored secret back — so empty means "keep".
    pub(crate) secret: String,
    /// Whether the OS store already holds this source's secret.
    pub(crate) has_credential: bool,
    /// Forget the stored credential on save.
    pub(crate) remove_credential: bool,
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

impl std::fmt::Debug for SourceDialog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceDialog")
            .field("edit_id", &self.edit_id)
            .field("kind", &self.kind)
            .field("locator", &self.locator)
            .field("label", &self.label)
            .field("username", &self.username)
            .field("stored_username", &self.stored_username)
            .field("secret", &"redacted")
            .field("has_credential", &self.has_credential)
            .field("remove_credential", &self.remove_credential)
            .field("project_choices", &self.project_choices)
            .field("project_choice", &self.project_choice)
            .field("field", &self.field)
            .field("caret", &self.caret)
            .field("error", &self.error)
            .finish()
    }
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
            username: String::new(),
            stored_username: String::new(),
            secret: String::new(),
            has_credential: false,
            remove_credential: false,
            project_choices: choices,
            project_choice: at,
            field: SourceField::Locator,
            caret: crate::app::textline::Caret::default(),
            error: None,
        }
    }

    /// An edit dialog for the source with these current values. The
    /// stored credential's login name prefills the user-name field;
    /// the secret itself is never available to prefill.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn edit(
        id: String,
        kind: SourceKind,
        locator: String,
        label: String,
        project_id: Option<String>,
        username: Option<String>,
        has_credential: bool,
        project_choices: Vec<(String, String)>,
    ) -> Self {
        let (choices, at) = with_current_mapping(project_choices, project_id.as_deref());
        let caret = crate::app::textline::Caret::at_end(&locator);
        let username = username.unwrap_or_default();
        SourceDialog {
            edit_id: Some(id),
            kind,
            locator,
            label,
            stored_username: username.clone(),
            username,
            secret: String::new(),
            has_credential,
            remove_credential: false,
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

    /// Whether `field` exists for this dialog's current kind/state:
    /// credential fields are CalDAV-only, and the removal toggle
    /// needs a stored credential to remove. Tab skips the rest.
    pub(crate) fn field_applies(&self, field: SourceField) -> bool {
        match field {
            SourceField::Kind
            | SourceField::Locator
            | SourceField::Label
            | SourceField::Project => true,
            SourceField::Username | SourceField::Secret => self.kind == SourceKind::Caldav,
            SourceField::RemoveCredential => {
                self.kind == SourceKind::Caldav && self.edit_id.is_some() && self.has_credential
            }
        }
    }

    /// Forget the typed secret: called as soon as a save is sent (and
    /// on cancel), so the field state never outlives its purpose even
    /// if the dialog value itself lingers.
    pub(crate) fn clear_secret(&mut self) {
        self.secret.clear();
        if self.field == SourceField::Secret {
            self.reset_caret();
        }
    }

    /// Toggle forgetting the stored credential on save. Turning it on
    /// drops any typed secret: set and remove never travel together.
    pub(crate) fn toggle_remove_credential(&mut self) {
        self.remove_credential = !self.remove_credential;
        if self.remove_credential {
            self.clear_secret();
        }
        self.error = None;
    }

    /// Move the kind by `delta`, wrapping at both ends. Clears a stale
    /// error like any other edit. Leaving CalDAV drops the typed
    /// secret and any removal request — they have no meaning for an
    /// `.ics` source and must never travel with one.
    pub(crate) fn cycle_kind(&mut self, delta: i32) {
        self.kind = self.kind.cycle(delta);
        if self.kind != SourceKind::Caldav {
            self.clear_secret();
            self.remove_credential = false;
            if !self.field_applies(self.field) {
                self.field = SourceField::Locator;
                self.reset_caret();
            }
        }
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

    /// Tab to the next field, skipping ones that do not apply.
    pub(crate) fn next_field(&mut self) {
        let start = self.field;
        let mut field = start;
        loop {
            field = field.next();
            if field == start || self.field_applies(field) {
                break;
            }
        }
        self.field = field;
        self.reset_caret();
    }

    /// Focus `field` directly: the mouse route to the same place Tab
    /// goes, seating the caret at that field's end like `next_field`.
    /// A field that does not apply is not focusable.
    pub(crate) fn focus_field(&mut self, field: SourceField) {
        if !self.field_applies(field) {
            return;
        }
        self.field = field;
        self.reset_caret();
    }

    /// Shift-Tab to the previous field, skipping inapplicable ones.
    pub(crate) fn prev_field(&mut self) {
        let start = self.field;
        let mut field = start;
        loop {
            field = field.prev();
            if field == start || self.field_applies(field) {
                break;
            }
        }
        self.field = field;
        self.reset_caret();
    }

    /// The focused text field and its caret, or `None` on the kind,
    /// project and remove-credential fields, which cycle instead.
    pub(crate) fn focused_text_mut(
        &mut self,
    ) -> Option<(&mut String, &mut crate::app::textline::Caret)> {
        let field = self.field;
        let text = match field {
            SourceField::Kind | SourceField::Project | SourceField::RemoveCredential => {
                return None;
            }
            SourceField::Locator => &mut self.locator,
            SourceField::Label => &mut self.label,
            SourceField::Username => &mut self.username,
            SourceField::Secret => &mut self.secret,
        };
        Some((text, &mut self.caret))
    }

    /// The focused text field and its caret, read-only: what the input
    /// handler reports to the platform. `None` on the cycling fields,
    /// which take no text — and so draw no input handler. The secret
    /// is `None` too, deliberately: it is never reported to the
    /// platform (no IME composition, no surrounding-text queries), so
    /// it types through plain key events like a terminal prompt.
    pub(crate) fn focused_text(&self) -> Option<(&str, &crate::app::textline::Caret)> {
        let text = match self.field {
            SourceField::Kind
            | SourceField::Project
            | SourceField::RemoveCredential
            | SourceField::Secret => return None,
            SourceField::Locator => &self.locator,
            SourceField::Label => &self.label,
            SourceField::Username => &self.username,
        };
        Some((text, &self.caret))
    }

    /// Put the caret at the end of the newly focused field: where
    /// append-style typing used to put it, and the least surprising
    /// place to arrive.
    fn reset_caret(&mut self) {
        let len = match self.field {
            SourceField::Kind | SourceField::Project | SourceField::RemoveCredential => 0,
            SourceField::Locator => self.locator.len(),
            SourceField::Label => self.label.len(),
            SourceField::Username => self.username.len(),
            SourceField::Secret => self.secret.len(),
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
        let in_secret = self.field == SourceField::Secret;
        let Some((text, caret)) = self.focused_text_mut() else {
            return;
        };
        crate::app::textline::insert_text(text, caret, ch);
        // Typing a password is a decision to keep (replace) the
        // credential: it cancels a pending removal.
        if in_secret && !self.secret.is_empty() {
            self.remove_credential = false;
        }
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
        if self.field == SourceField::Secret {
            return false;
        }
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
        // The secret row focuses (caret to its end) but never places
        // from the mouse: its painted glyphs are mask bullets, so a
        // pixel offset names no secret character.
        if field == SourceField::Secret {
            self.focus_field(field);
            return;
        }
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

/// The masked form of a secret for drawing: one bullet per
/// character, so the row shows length but never the secret.
pub(crate) fn masked_secret(secret: &str) -> String {
    "•".repeat(secret.chars().count())
}

/// The masked secret with its caret/selection adornments, for the
/// focused row: a masked copy and a caret remapped from secret byte
/// offsets to bullet offsets, drawn by the shared field renderer.
pub(crate) fn masked_secret_with_caret(
    secret: &str,
    caret: &crate::app::textline::Caret,
) -> String {
    let bullet = "•".len();
    let to_masked = |at: usize| secret.char_indices().filter(|(i, _)| *i < at).count() * bullet;
    let masked_caret = crate::app::textline::Caret {
        pos: to_masked(caret.pos),
        anchor: caret.anchor.map(to_masked),
        marked: None,
    };
    crate::app::textline::render_with_caret(&masked_secret(secret), &masked_caret)
}

/// What saving does to the source's stored credential (#149).
/// `SecretString` redacts in `Debug`, so a logged save cannot leak it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CredentialSave {
    /// Leave the credential as it is (none on create, stored on edit).
    Keep,
    /// Store (or replace) the secret in the OS credential store.
    Set {
        username: String,
        secret: timetrack_proto::SecretString,
    },
    /// Forget the stored secret.
    Remove,
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
        credential: CredentialSave,
    },
    Update {
        id: String,
        kind: String,
        locator: String,
        project_id: Option<String>,
        label: String,
        credential: CredentialSave,
    },
}

/// Resolve a source dialog. A blank locator stays open with the
/// service's own sentence; everything harder (unknown kind — which the
/// cycling field cannot produce — duplicates, a CalDAV password in the
/// URL) travels to the service, which owns source validity and reports
/// it typed. An empty label travels as-is: the service derives it from
/// the locator, as on registration.
/// Whether a locator carries `user@` in its authority: the CLI's
/// early "a user name is needed" rule (#149); the service re-resolves
/// authoritatively, including percent-decoding.
fn locator_has_userinfo(locator: &str) -> bool {
    let Some((_, rest)) = locator.split_once("://") else {
        return false;
    };
    let Some(at) = rest.rfind('@') else {
        return false;
    };
    if rest.find('/').is_some_and(|slash| slash < at) {
        return false;
    }
    !rest[..at].contains(':')
}

/// The credential half of a save. The secret itself is never trimmed
/// (its own spaces are significant, as in the CLI's piped read).
fn resolve_credential(dlg: &SourceDialog, locator: &str) -> Result<CredentialSave, String> {
    if dlg.kind != SourceKind::Caldav {
        if !dlg.secret.is_empty() || dlg.remove_credential {
            return Err("credentials are only for CalDAV sources".to_string());
        }
        return Ok(CredentialSave::Keep);
    }
    if dlg.remove_credential {
        if !dlg.secret.is_empty() {
            return Err(
                "the password and removing the credential cannot both be sent; clear one"
                    .to_string(),
            );
        }
        if dlg.edit_id.is_none() || !dlg.has_credential {
            return Err("there is no stored credential to remove".to_string());
        }
        return Ok(CredentialSave::Remove);
    }
    if dlg.secret.is_empty() {
        if dlg.username.trim() != dlg.stored_username.trim() {
            return Err(
                "enter the password too — a user name is stored only with its secret".to_string(),
            );
        }
        return Ok(CredentialSave::Keep);
    }
    let username = dlg.username.trim().to_string();
    if username.is_empty() && !locator_has_userinfo(locator) {
        return Err(
            "a CalDAV credential needs a user name — in the field or as user@ in the URL"
                .to_string(),
        );
    }
    Ok(CredentialSave::Set {
        username,
        secret: timetrack_proto::SecretString::new(dlg.secret.clone()),
    })
}

pub(crate) fn resolve_source_dialog(dlg: &SourceDialog) -> Result<SourceSave, String> {
    if dlg.locator.trim().is_empty() {
        return Err("a source needs a file path or URL".to_string());
    }
    let kind = dlg.kind.as_str().to_string();
    let locator = dlg.locator.trim().to_string();
    let label = dlg.label.trim().to_string();
    let credential = resolve_credential(dlg, &locator)?;
    let (project_id, _) = dlg.chosen_project();
    let project_id = project_id.map(str::to_string);
    match &dlg.edit_id {
        None => Ok(SourceSave::Create {
            kind,
            locator,
            project_id,
            label,
            credential,
        }),
        Some(id) => Ok(SourceSave::Update {
            id: id.clone(),
            kind,
            locator,
            project_id,
            label,
            credential,
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
    fn fields_tab_through_all_seven() {
        assert_eq!(SourceField::Kind.next(), SourceField::Locator);
        assert_eq!(SourceField::Locator.next(), SourceField::Label);
        assert_eq!(SourceField::Label.next(), SourceField::Project);
        assert_eq!(SourceField::Project.next(), SourceField::Username);
        assert_eq!(SourceField::Username.next(), SourceField::Secret);
        assert_eq!(SourceField::Secret.next(), SourceField::RemoveCredential);
        assert_eq!(SourceField::RemoveCredential.next(), SourceField::Kind);
        assert_eq!(SourceField::Kind.prev(), SourceField::RemoveCredential);
        assert_eq!(SourceField::Project.prev(), SourceField::Label);
        assert_eq!(SourceField::Username.prev(), SourceField::Project);
    }

    #[test]
    fn tab_skips_credential_fields_unless_they_apply() {
        let mut dlg = SourceDialog::new(choices());
        // An .ics dialog tabs Project -> Kind, never onto a credential.
        dlg.focus_field(SourceField::Project);
        dlg.next_field();
        assert_eq!(dlg.field, SourceField::Kind);
        dlg.prev_field();
        assert_eq!(dlg.field, SourceField::Project);
        // CalDAV registration reaches user name and password, but not
        // the removal toggle (nothing stored yet).
        dlg.cycle_kind(1);
        dlg.cycle_kind(1);
        assert_eq!(dlg.kind, SourceKind::Caldav);
        dlg.focus_field(SourceField::Project);
        dlg.next_field();
        assert_eq!(dlg.field, SourceField::Username);
        dlg.next_field();
        assert_eq!(dlg.field, SourceField::Secret);
        dlg.next_field();
        assert_eq!(dlg.field, SourceField::Kind);
        // An edit with a stored credential reaches the toggle too.
        let mut edit = SourceDialog::edit(
            "src-1".into(),
            SourceKind::Caldav,
            "https://cal.test/u/".into(),
            "Cal".into(),
            None,
            Some("alice".into()),
            true,
            choices(),
        );
        edit.focus_field(SourceField::Secret);
        edit.next_field();
        assert_eq!(edit.field, SourceField::RemoveCredential);
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
            Some("alice".into()),
            true,
            choices(),
        );
        assert_eq!(dlg.edit_id.as_deref(), Some("src-1"));
        assert_eq!(dlg.kind, SourceKind::Caldav);
        assert_eq!(dlg.username, "alice");
        assert_eq!(dlg.secret, "", "a stored secret is never prefilled");
        assert!(dlg.has_credential);
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
                credential: CredentialSave::Keep,
            })
        );
        let mut edit = SourceDialog::edit(
            "src-1".into(),
            SourceKind::IcsUrl,
            "https://cal.test/c.ics".into(),
            String::new(),
            None,
            None,
            false,
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
                credential: CredentialSave::Keep,
            }),
            "an empty label travels for the service to derive"
        );
        edit.locator = "   ".into();
        assert!(resolve_source_dialog(&edit).is_err());
    }

    fn caldav_dialog() -> SourceDialog {
        let mut dlg = SourceDialog::new(choices());
        dlg.kind = SourceKind::Caldav;
        dlg.locator = "https://cal.test/u/".into();
        dlg
    }

    #[test]
    fn a_typed_caldav_credential_resolves_to_a_set_and_clears_on_send() {
        let mut dlg = caldav_dialog();
        dlg.username = " alice ".into();
        dlg.focus_field(SourceField::Secret);
        for ch in "s3cr3t ".chars() {
            dlg.type_text(&ch.to_string());
        }
        let resolved = resolve_source_dialog(&dlg).unwrap();
        match resolved {
            SourceSave::Create { credential, .. } => match credential {
                CredentialSave::Set { username, secret } => {
                    assert_eq!(username, "alice", "the login name trims");
                    assert_eq!(secret.expose(), "s3cr3t ", "the secret never trims");
                }
                other => panic!("expected a credential set, got {other:?}"),
            },
            other => panic!("expected a create, got {other:?}"),
        }
        // As on save: once sent, the dialog holds no secret any more,
        // so nothing can resolve to a credential set from it again.
        dlg.clear_secret();
        assert_eq!(dlg.secret, "");
        assert_eq!(dlg.caret.pos, 0);
        assert!(
            resolve_source_dialog(&dlg)
                .unwrap_err()
                .contains("enter the password too"),
            "the wiped secret takes the Set with it"
        );
    }

    #[test]
    fn credential_resolution_refuses_the_ambiguous_and_the_meaningless() {
        // A password with no user name anywhere.
        let mut dlg = caldav_dialog();
        dlg.secret = "s3cr3t".into();
        assert!(
            resolve_source_dialog(&dlg)
                .unwrap_err()
                .contains("needs a user name")
        );
        // `user@` in the URL supplies it instead.
        dlg.locator = "https://alice@cal.test/u/".into();
        assert!(resolve_source_dialog(&dlg).is_ok());
        // A user name without its password is not silently dropped.
        let mut named = caldav_dialog();
        named.username = "alice".into();
        assert!(
            resolve_source_dialog(&named)
                .unwrap_err()
                .contains("enter the password too")
        );
        // Set and remove never travel together (typed behind the
        // toggle's back — the toggle itself clears the secret).
        let mut both = SourceDialog::edit(
            "src-1".into(),
            SourceKind::Caldav,
            "https://cal.test/u/".into(),
            "Cal".into(),
            None,
            Some("alice".into()),
            true,
            choices(),
        );
        both.secret = "s3cr3t".into();
        both.remove_credential = true;
        assert!(resolve_source_dialog(&both).is_err());
        // And an .ics source takes no credential at all.
        let mut ics = SourceDialog::new(choices());
        ics.locator = "/tmp/cal.ics".into();
        ics.secret = "s3cr3t".into();
        assert!(
            resolve_source_dialog(&ics)
                .unwrap_err()
                .contains("only for CalDAV")
        );
    }

    #[test]
    fn a_blank_password_keeps_and_the_toggle_removes() {
        let mut edit = SourceDialog::edit(
            "src-1".into(),
            SourceKind::Caldav,
            "https://cal.test/u/".into(),
            "Cal".into(),
            None,
            Some("alice".into()),
            true,
            choices(),
        );
        match resolve_source_dialog(&edit) {
            Ok(SourceSave::Update { credential, .. }) => {
                assert_eq!(credential, CredentialSave::Keep);
            }
            other => panic!("blank keeps, got {other:?}"),
        }
        edit.focus_field(SourceField::Secret);
        edit.type_text("x");
        edit.toggle_remove_credential();
        assert_eq!(edit.secret, "", "removing drops a typed secret");
        match resolve_source_dialog(&edit) {
            Ok(SourceSave::Update { credential, .. }) => {
                assert_eq!(credential, CredentialSave::Remove);
            }
            other => panic!("the toggle removes, got {other:?}"),
        }
        // Typing again cancels the removal.
        edit.focus_field(SourceField::Secret);
        edit.type_text("n");
        assert!(!edit.remove_credential);
    }

    #[test]
    fn the_secret_is_masked_redacted_and_never_platform_text() {
        use crate::app::textline::{Caret, ImeEdit};
        let mut dlg = caldav_dialog();
        dlg.secret = "s3cr3t".into();
        assert_eq!(masked_secret(&dlg.secret), "••••••");
        let caret = Caret::at_end(&dlg.secret);
        let drawn = masked_secret_with_caret(&dlg.secret, &caret);
        assert!(drawn.starts_with("••••••"), "{drawn}");
        assert!(!drawn.contains("s3cr3t"), "{drawn}");
        let debug = format!("{dlg:?}");
        assert!(!debug.contains("s3cr3t"), "{debug}");
        // Focused, the secret reports no platform text and takes no
        // IME composition; it still edits through key events.
        dlg.focus_field(SourceField::Secret);
        assert_eq!(dlg.focused_text(), None);
        assert!(!dlg.ime_edit(ImeEdit::Commit {
            range: None,
            text: "x",
        }));
        dlg.type_text("!");
        assert_eq!(dlg.secret, "s3cr3t!");
        dlg.backspace();
        assert_eq!(dlg.secret, "s3cr3t");
        // A mouse press only focuses (caret to the end).
        dlg.mouse_down(SourceField::Label, 0, false);
        dlg.mouse_down(SourceField::Secret, 0, false);
        assert_eq!(dlg.field, SourceField::Secret);
        assert_eq!(dlg.caret.pos, dlg.secret.len());
        // Leaving CalDAV forgets it entirely.
        dlg.cycle_kind(1);
        assert_eq!(dlg.secret, "");
        assert_eq!(dlg.field, SourceField::Locator);
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
