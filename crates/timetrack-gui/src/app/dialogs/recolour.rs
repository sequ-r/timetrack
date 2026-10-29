/* gui/app/dialogs/recolour.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The recolour prompt: one `RRGGBB` hex field for a project's colour
//! (#104).
//!
//! Plain keystroke editing like the other prompts, and likewise pure state
//! so resolving is unit-testable without a window. The hex spelling is the
//! core's (`timetrack_core::parse_colour`), shared with the CLI's
//! `recolour`, so the two front ends accept exactly the same text.

/// The recolour prompt for one project, prefilled with its current colour
/// (empty when it has none — and empty is also how a colour is cleared).
#[derive(Debug, Clone)]
pub(crate) struct RecolourPrompt {
    pub(crate) project_id: String,
    /// Display copy of the project name for the panel title, sanitized at
    /// construction (#101). The id above is the identity; this never is.
    pub(crate) project_name: String,
    /// The typed hex text, without any `#` the parser would also accept.
    pub(crate) text: String,
    /// The colour the project had when the prompt opened, so saving it
    /// unchanged sends nothing.
    pub(crate) orig_colour: Option<u32>,
    pub(crate) error: Option<String>,
}

impl RecolourPrompt {
    pub(crate) fn new(project_id: String, project_name: String, colour: Option<u32>) -> Self {
        Self {
            project_id,
            project_name: timetrack_core::sanitize_display(&project_name),
            text: colour.map(|c| format!("{c:06x}")).unwrap_or_default(),
            orig_colour: colour,
            error: None,
        }
    }
}

/// What saving a recolour prompt means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecolourSave {
    /// Set the colour (`None` clears it).
    Apply(Option<u32>),
    /// The typed colour is the current one; close without calling the
    /// service.
    NoChange,
}

/// Resolve a recolour prompt. Empty text clears the colour; anything else
/// must parse as `RRGGBB` hex or the prompt stays open with the core's
/// wording.
pub(crate) fn resolve_recolour(
    prompt: &RecolourPrompt,
) -> Result<RecolourSave, timetrack_core::ParseError> {
    let colour = if prompt.text.trim().is_empty() {
        None
    } else {
        Some(timetrack_core::parse_colour(&prompt.text)?)
    };
    if colour == prompt.orig_colour {
        Ok(RecolourSave::NoChange)
    } else {
        Ok(RecolourSave::Apply(colour))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt(colour: Option<u32>, text: &str) -> RecolourPrompt {
        RecolourPrompt {
            text: text.into(),
            ..RecolourPrompt::new("p1".into(), "Work".into(), colour)
        }
    }

    #[test]
    fn a_typed_hex_colour_applies() {
        assert_eq!(
            resolve_recolour(&prompt(None, "2ea043")),
            Ok(RecolourSave::Apply(Some(0x2ea043)))
        );
        assert_eq!(
            resolve_recolour(&prompt(Some(0x111111), "#FF0000")),
            Ok(RecolourSave::Apply(Some(0xff0000)))
        );
    }

    #[test]
    fn empty_text_clears_and_only_when_there_is_a_colour() {
        assert_eq!(
            resolve_recolour(&prompt(Some(0x2ea043), "")),
            Ok(RecolourSave::Apply(None))
        );
        assert_eq!(
            resolve_recolour(&prompt(None, "   ")),
            Ok(RecolourSave::NoChange)
        );
    }

    #[test]
    fn saving_the_current_colour_is_a_no_op() {
        // Prefilled and untouched, in either spelling the parser accepts.
        let p = RecolourPrompt::new("p1".into(), "Work".into(), Some(0x2ea043));
        assert_eq!(p.text, "2ea043");
        assert_eq!(resolve_recolour(&p), Ok(RecolourSave::NoChange));
        assert_eq!(
            resolve_recolour(&prompt(Some(0x2ea043), "#2EA043")),
            Ok(RecolourSave::NoChange)
        );
    }

    #[test]
    fn bad_hex_is_refused_with_the_shared_wording() {
        let err = resolve_recolour(&prompt(None, "green")).unwrap_err();
        assert_eq!(err, timetrack_core::ParseError::BadColour("green".into()));
        assert!(err.to_string().contains("RRGGBB"), "{err}");
    }

    #[test]
    fn the_title_name_is_a_sanitized_display_copy() {
        let p = RecolourPrompt::new("p1".into(), "Wo\nrk".into(), None);
        assert_eq!(p.project_name, "Wo rk");
        assert_eq!(p.project_id, "p1", "the id is the identity, untouched");
    }
}
