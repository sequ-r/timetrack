/* gui/app/dialogs/project.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The project prompt: one name field, for creating or renaming.
//!
//! Keystroke editing with a caret and selection like the other prompts, and likewise pure state
//! so resolving is unit-testable without a window.

/// The project prompt: one name field, for creating (`edit_id` none) or
/// renaming (`edit_id` set, prefilled with the current name).
///
/// Keystroke editing with a caret and selection like the other prompts, and likewise pure state
/// so resolving is unit-testable without a window.
#[derive(Debug, Clone)]
pub(crate) struct ProjectPrompt {
    pub(crate) edit_id: Option<String>,
    /// The typed name. Prefilled when renaming, so saving untouched would
    /// be a no-op the service round-trips harmlessly.
    pub(crate) name: String,
    /// The caret (and any selection) in the name field (#109).
    pub(crate) caret: crate::app::textline::Caret,
    pub(crate) error: Option<String>,
}

impl ProjectPrompt {
    /// Apply a platform text-input edit (IME preedit or commit, #126)
    /// to the name field, clearing a stale error like any edit.
    pub(crate) fn ime_edit(&mut self, edit: crate::app::textline::ImeEdit) {
        crate::app::textline::apply_ime_edit(&mut self.name, &mut self.caret, edit);
        self.error = None;
    }

    /// Press the mouse at a text offset in the name field (#127):
    /// places the caret, or extends to it with shift held. A press
    /// never clears the error: it edits no text.
    pub(crate) fn mouse_down(&mut self, at: usize, shift: bool) {
        if shift {
            crate::app::textline::extend_caret_to(&self.name, &mut self.caret, at);
        } else {
            crate::app::textline::place_caret(&self.name, &mut self.caret, at);
        }
    }

    /// Drag the mouse to a text offset in the name field (#127).
    pub(crate) fn mouse_drag(&mut self, at: usize) {
        crate::app::textline::extend_caret_to(&self.name, &mut self.caret, at);
    }
}

/// What saving a project prompt means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProjectSave {
    Create { name: String },
    Update { id: String, name: String },
}

/// Resolve a project prompt. Empty stays open with the core's typed rule
/// error rather than storing a label no picker could show; duplicates travel
/// to the service, which owns name uniqueness and reports them typed.
pub(crate) fn resolve_project_prompt(
    prompt: &ProjectPrompt,
) -> Result<ProjectSave, timetrack_core::RuleError> {
    if prompt.name.trim().is_empty() {
        return Err(timetrack_core::RuleError::BlankProjectName);
    }
    let name = prompt.name.trim().to_string();
    match &prompt.edit_id {
        None => Ok(ProjectSave::Create { name }),
        Some(id) => Ok(ProjectSave::Update {
            id: id.clone(),
            name,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::testsupport::project_prompt;

    #[test]
    fn project_prompt_creates_or_renames() {
        assert_eq!(
            resolve_project_prompt(&project_prompt(None, "Work")),
            Ok(ProjectSave::Create {
                name: "Work".into()
            })
        );
        assert_eq!(
            resolve_project_prompt(&project_prompt(Some("p1"), "Deep Work")),
            Ok(ProjectSave::Update {
                id: "p1".into(),
                name: "Deep Work".into()
            })
        );
    }

    #[test]
    fn project_prompt_trims_and_refuses_empty() {
        assert_eq!(
            resolve_project_prompt(&project_prompt(None, "  Work  ")),
            Ok(ProjectSave::Create {
                name: "Work".into()
            })
        );
        assert_eq!(
            resolve_project_prompt(&project_prompt(None, "  \t ")),
            Err(timetrack_core::RuleError::BlankProjectName)
        );
        assert_eq!(
            resolve_project_prompt(&project_prompt(Some("p1"), "   ")),
            Err(timetrack_core::RuleError::BlankProjectName)
        );
    }

    #[test]
    fn ime_edits_compose_the_name_and_clear_errors() {
        use crate::app::textline::ImeEdit;
        let mut prompt = project_prompt(None, "");
        prompt.error = Some("stale".into());
        prompt.ime_edit(ImeEdit::Mark {
            range: None,
            text: "gong",
            selected: None,
        });
        assert_eq!(prompt.name, "gong");
        assert_eq!(prompt.error, None);
        prompt.ime_edit(ImeEdit::Commit {
            range: None,
            text: "工作",
        });
        assert_eq!(prompt.name, "工作");
        assert_eq!(prompt.caret.marked, None);
    }

    #[test]
    fn a_press_places_and_a_drag_selects() {
        // #127: place, drag from the press point, shift-extend — over
        // multi-byte text, without touching the error.
        use crate::app::textline::selected_text;
        let mut prompt = project_prompt(None, "caffè");
        prompt.error = Some("stale".into());
        prompt.mouse_down(2, false);
        assert_eq!(prompt.caret.pos, 2);
        prompt.mouse_drag(6);
        assert_eq!(selected_text(&prompt.name, &prompt.caret), Some("ffè"));
        prompt.mouse_down(0, true);
        assert_eq!(selected_text(&prompt.name, &prompt.caret), Some("ca"));
        assert_eq!(prompt.error.as_deref(), Some("stale"));
    }
}
