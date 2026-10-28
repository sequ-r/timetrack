/* gui/app/dialogs/project.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The project prompt: one name field, for creating or renaming.
//!
//! Plain keystroke editing like the other prompts, and likewise pure state
//! so resolving is unit-testable without a window.

/// The project prompt: one name field, for creating (`edit_id` none) or
/// renaming (`edit_id` set, prefilled with the current name).
///
/// Plain keystroke editing like the other prompts, and likewise pure state
/// so resolving is unit-testable without a window.
#[derive(Debug, Clone)]
pub(crate) struct ProjectPrompt {
    pub(crate) edit_id: Option<String>,
    /// The typed name. Prefilled when renaming, so saving untouched would
    /// be a no-op the service round-trips harmlessly.
    pub(crate) name: String,
    pub(crate) error: Option<String>,
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
}
