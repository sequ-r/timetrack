/* gui/app/dialogs/sync.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! Folder-sync dialogs (#176): pairing (create vs join), pairing code display
//! / key rotation, and unpair confirmation.
//!
//! All state is plain strings/booleans over `textline::Caret`, matching the
//! other modal prompts so field editing, validation, and key routing are
//! unit-testable without a window.

/// Pairing mode in the sync setup dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SyncMode {
    Create,
    Join,
}

impl SyncMode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            SyncMode::Create => "create (first device)",
            SyncMode::Join => "join (existing folder)",
        }
    }

    pub(crate) fn cycle(self, delta: i32) -> Self {
        const ORDER: [SyncMode; 2] = [SyncMode::Create, SyncMode::Join];
        let at = ORDER.iter().position(|m| *m == self).unwrap_or(0);
        ORDER[(at as i32 + delta).rem_euclid(ORDER.len() as i32) as usize]
    }
}

/// Which field of [`SyncDialog`] owns focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SyncField {
    Mode,
    Folder,
    PairingCode,
}

impl SyncField {
    pub(crate) fn next(self) -> Self {
        match self {
            SyncField::Mode => SyncField::Folder,
            SyncField::Folder => SyncField::PairingCode,
            SyncField::PairingCode => SyncField::Mode,
        }
    }

    pub(crate) fn prev(self) -> Self {
        match self {
            SyncField::Mode => SyncField::PairingCode,
            SyncField::Folder => SyncField::Mode,
            SyncField::PairingCode => SyncField::Folder,
        }
    }
}

/// Modal dialog to enable folder sync or join an existing sync folder.
#[derive(Clone)]
pub(crate) struct SyncDialog {
    pub(crate) mode: SyncMode,
    pub(crate) folder: String,
    pub(crate) pairing_code: String,
    pub(crate) field: SyncField,
    pub(crate) caret: crate::app::textline::Caret,
    pub(crate) error: Option<String>,
}

impl std::fmt::Debug for SyncDialog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncDialog")
            .field("mode", &self.mode)
            .field("folder", &self.folder)
            .field("pairing_code", &"redacted")
            .field("field", &self.field)
            .field("caret", &self.caret)
            .field("error", &self.error)
            .finish()
    }
}

impl SyncDialog {
    /// Open in Create or Join mode with focus on the folder field.
    pub(crate) fn new(mode: SyncMode) -> Self {
        SyncDialog {
            mode,
            folder: String::new(),
            pairing_code: String::new(),
            field: SyncField::Folder,
            caret: crate::app::textline::Caret::default(),
            error: None,
        }
    }

    /// Whether `field` applies to the current mode.
    pub(crate) fn field_applies(&self, field: SyncField) -> bool {
        match field {
            SyncField::Mode | SyncField::Folder => true,
            SyncField::PairingCode => self.mode == SyncMode::Join,
        }
    }

    /// Clear any typed pairing code so it never lingers in dialog state.
    pub(crate) fn clear_code(&mut self) {
        self.pairing_code.clear();
        if self.field == SyncField::PairingCode {
            self.reset_caret();
        }
    }

    /// Cycle Create/Join mode and clear any pairing code when leaving Join.
    pub(crate) fn cycle_mode(&mut self, delta: i32) {
        self.mode = self.mode.cycle(delta);
        if self.mode != SyncMode::Join {
            self.clear_code();
            if !self.field_applies(self.field) {
                self.field = SyncField::Folder;
                self.reset_caret();
            }
        }
        self.error = None;
    }

    /// Update the folder path (e.g. from the OS folder picker) and keep the
    /// caret valid.
    pub(crate) fn set_folder(&mut self, path: String) {
        self.folder = path;
        if self.field == SyncField::Folder {
            self.reset_caret();
        }
        self.error = None;
    }

    pub(crate) fn focus_field(&mut self, field: SyncField) {
        if !self.field_applies(field) {
            return;
        }
        self.field = field;
        self.reset_caret();
    }

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

    pub(crate) fn focused_text_mut(
        &mut self,
    ) -> Option<(&mut String, &mut crate::app::textline::Caret)> {
        let text = match self.field {
            SyncField::Mode => return None,
            SyncField::Folder => &mut self.folder,
            SyncField::PairingCode => &mut self.pairing_code,
        };
        Some((text, &mut self.caret))
    }

    pub(crate) fn focused_text(&self) -> Option<(&str, &crate::app::textline::Caret)> {
        let text = match self.field {
            SyncField::Mode => return None,
            SyncField::Folder => &self.folder,
            SyncField::PairingCode => &self.pairing_code,
        };
        Some((text, &self.caret))
    }

    fn reset_caret(&mut self) {
        let len = match self.field {
            SyncField::Mode => 0,
            SyncField::Folder => self.folder.len(),
            SyncField::PairingCode => self.pairing_code.len(),
        };
        self.caret = crate::app::textline::Caret {
            pos: len,
            anchor: None,
            marked: None,
        };
    }

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

    pub(crate) fn type_text(&mut self, ch: &str) {
        let Some((text, caret)) = self.focused_text_mut() else {
            return;
        };
        crate::app::textline::insert_text(text, caret, ch);
        self.error = None;
    }

    pub(crate) fn backspace(&mut self) {
        let Some((text, caret)) = self.focused_text_mut() else {
            return;
        };
        crate::app::textline::backspace(text, caret);
        self.error = None;
    }

    pub(crate) fn ime_edit(&mut self, edit: crate::app::textline::ImeEdit) -> bool {
        let Some((text, caret)) = self.focused_text_mut() else {
            return false;
        };
        crate::app::textline::apply_ime_edit(text, caret, edit);
        self.error = None;
        true
    }

    pub(crate) fn mouse_down(&mut self, field: SyncField, at: usize, shift: bool) {
        if field != self.field {
            self.focus_field(field);
            if self.field != field {
                return;
            }
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

    pub(crate) fn mouse_drag(&mut self, at: usize) {
        let Some((text, caret)) = self.focused_text_mut() else {
            return;
        };
        crate::app::textline::extend_caret_to(text, caret, at);
    }
}

/// Resolved payload to send to `Action::SyncEnable`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SyncSave {
    Enable {
        folder: String,
        pairing_code: Option<timetrack_proto::SecretString>,
    },
}

/// Validate [`SyncDialog`] inputs before sending `SyncEnable`.
pub(crate) fn resolve_sync_dialog(dlg: &SyncDialog) -> Result<SyncSave, String> {
    let folder = dlg.folder.trim();
    if folder.is_empty() {
        return Err("folder path cannot be empty".to_string());
    }
    let pairing_code = match dlg.mode {
        SyncMode::Create => None,
        SyncMode::Join => {
            let code = dlg.pairing_code.trim();
            if code.is_empty() {
                return Err("enter the pairing code from your first device".to_string());
            }
            Some(timetrack_proto::SecretString::new(code))
        }
    };
    Ok(SyncSave::Enable {
        folder: folder.to_string(),
        pairing_code,
    })
}

/// Modal dialog displaying the active pairing code (after enable, show-code,
/// or rotate-key).
#[derive(Clone)]
pub(crate) struct SyncCodeDialog {
    pub(crate) code: String,
    pub(crate) rotated: bool,
    pub(crate) copied: bool,
}

impl std::fmt::Debug for SyncCodeDialog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncCodeDialog")
            .field("code", &"redacted")
            .field("rotated", &self.rotated)
            .field("copied", &self.copied)
            .finish()
    }
}

/// Confirmation prompt before unpairing folder sync (offers keep-file vs
/// delete-device-file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SyncUnpairConfirm {
    pub(crate) summary: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_mode_skips_pairing_code_on_tab_and_clears_on_switch() {
        let mut dlg = SyncDialog::new(SyncMode::Create);
        assert_eq!(dlg.field, SyncField::Folder);
        assert!(!dlg.field_applies(SyncField::PairingCode));
        dlg.next_field();
        assert_eq!(dlg.field, SyncField::Mode);
        dlg.next_field();
        assert_eq!(dlg.field, SyncField::Folder);

        dlg.cycle_mode(1);
        assert_eq!(dlg.mode, SyncMode::Join);
        assert!(dlg.field_applies(SyncField::PairingCode));
        dlg.next_field();
        assert_eq!(dlg.field, SyncField::PairingCode);
        dlg.type_text("ABCD-EFGH");
        assert_eq!(dlg.pairing_code, "ABCD-EFGH");

        dlg.cycle_mode(1);
        assert_eq!(dlg.mode, SyncMode::Create);
        assert!(dlg.pairing_code.is_empty(), "leaving Join wipes code");
        assert_eq!(dlg.field, SyncField::Folder);
    }

    #[test]
    fn resolve_validates_folder_and_join_code() {
        let mut dlg = SyncDialog::new(SyncMode::Create);
        assert_eq!(
            resolve_sync_dialog(&dlg).unwrap_err(),
            "folder path cannot be empty"
        );
        dlg.set_folder("  /tmp/shared-sync  ".into());
        assert_eq!(
            resolve_sync_dialog(&dlg).unwrap(),
            SyncSave::Enable {
                folder: "/tmp/shared-sync".into(),
                pairing_code: None,
            }
        );

        let mut join = SyncDialog::new(SyncMode::Join);
        join.set_folder("/tmp/shared-sync".into());
        assert_eq!(
            resolve_sync_dialog(&join).unwrap_err(),
            "enter the pairing code from your first device"
        );
        join.focus_field(SyncField::PairingCode);
        join.type_text("  AAAA-BBBB-CCCC  ");
        let saved = resolve_sync_dialog(&join).unwrap();
        match saved {
            SyncSave::Enable {
                folder,
                pairing_code: Some(code),
            } => {
                assert_eq!(folder, "/tmp/shared-sync");
                assert_eq!(code.expose(), "AAAA-BBBB-CCCC");
            }
            other => panic!("expected Enable with pairing code, got {other:?}"),
        }
    }

    #[test]
    fn editing_caret_ime_and_mouse_drag_work_on_sync_fields() {
        let mut dlg = SyncDialog::new(SyncMode::Join);
        dlg.type_text("/tmp/sync");
        assert_eq!(dlg.folder, "/tmp/sync");
        dlg.backspace();
        assert_eq!(dlg.folder, "/tmp/syn");
        assert!(dlg.edit_key("left", false, false));
        assert!(dlg.ime_edit(crate::app::textline::ImeEdit::Commit {
            range: None,
            text: "X",
        }));
        assert_eq!(dlg.folder, "/tmp/syXn");

        dlg.mouse_down(SyncField::PairingCode, 0, false);
        assert_eq!(dlg.field, SyncField::PairingCode);
        dlg.type_text("123456");
        dlg.mouse_down(SyncField::PairingCode, 1, false);
        dlg.mouse_drag(4);
        dlg.backspace();
        assert_eq!(dlg.pairing_code, "156");

        dlg.clear_code();
        assert!(dlg.pairing_code.is_empty());
        assert!(!format!("{dlg:?}").contains("156"));
    }
}
