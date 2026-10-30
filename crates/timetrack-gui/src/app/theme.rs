/* gui/app/theme.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The GUI's colour palette.
//!
//! The render code used to repeat the same hex literals in a dozen places.
//! Naming them here keeps the palette in one file: a colour is changed in
//! one edit, and a colour that appears in two places is visibly the same
//! one.
//!
//! The colours are in `0xRRGGBBAA` form, which is what gpui's `rgba`
//! takes; the opaque ones end in `ff`.

use gpui::{Rgba, rgb, rgba};

/// The window's background.
pub(super) const WINDOW_BG: u32 = 0x11111bff;

/// The panel background: the titlebar and the prompt panels.
pub(super) const PANEL_BG: u32 = 0x1a1a26ff;

/// The raised background: the selected row, the selected tab, the focused
/// field, and the track the per-project bars fill.
pub(super) const RAISED_BG: u32 = 0x2a2a3aff;

/// The muted grey: the secondary labels, and a project that has no colour
/// of its own.
pub(super) const MUTED_TEXT: u32 = 0x7a7a90ff;

/// The accent: the durations and the highlighted numbers.
pub(super) const ACCENT: u32 = 0x7fd3ffff;

/// The action green: the quick-add, export and start-service buttons.
pub(super) const ACTION: u32 = 0x2ea043ff;

/// The unarchive button's green, translucent: the quiet half of the
/// archive pair.
pub(super) const ACTION_FADED: u32 = 0x2ea04366;

/// The dimmest grey: the hints under the fields.
pub(super) const HINT_TEXT: u32 = 0x6a6a80ff;

/// The danger red: the close button.
pub(super) const DANGER: u32 = 0xe06c75ff;

/// The grey of the secondary text in a row.
pub(super) const ROW_TEXT: u32 = 0x9a9ab0ff;

/// The bright grey of the emphasised text.
pub(super) const STRONG_TEXT: u32 = 0xc8c8d8ff;

/// The body text colour.
pub(super) const BODY_TEXT: u32 = 0xe6e6f0ff;

/// The warning amber: the over-24h notice and the inline errors.
pub(super) const WARNING: u32 = 0xd29922ff;

/// The plain white of the text that carries emphasis by brightness alone:
/// the week total, the focused field's label, the selected tab, and the
/// text on the green action buttons. It used to be spelled
/// `rgb(0xffffff)` at every call site.
pub(super) const SELECTED_TEXT: u32 = 0xffffffff;

/// Fully transparent: the resting background of a row or field that is
/// only painted while selected or focused.
pub(super) const TRANSPARENT: u32 = 0x00000000;

/// The neutral button background: Recolour/Delete/Archive and their
/// Export-tab equivalents, a step lighter than the selected row so a
/// button reads as raised even off a panel.
pub(super) const NEUTRAL_BUTTON_BG: u32 = 0x3a3a4aff;

/// The focus ring: the border drawn around the field that owns the
/// keyboard, so focus is a shape and not only a brighter fill. The
/// accent at 80% alpha, so it sits on a panel without shouting.
pub(super) const FOCUS_RING: u32 = 0x7fd3ffcc;

/// The border that separates a prompt panel from the tab content
/// behind it: the raised background at 80% alpha. gpui has no shadow
/// here, so a 1px edge is what says "this floats above the list".
pub(super) const BORDER_SUBTLE: u32 = 0x2a2a3acc;

/// Whether decorative motion (fades, pulses) may run, from the
/// `TIMETRACK_REDUCED_MOTION` environment variable.
///
/// Every animation checks this first and renders its final state
/// directly when it is false. Only the literal `1` disables motion, so
/// a typo never silently freezes the UI mid-fade.
pub(super) fn motion_enabled() -> bool {
    motion_enabled_from(std::env::var("TIMETRACK_REDUCED_MOTION").ok().as_deref())
}

/// `motion_enabled` over an already-read value, so the rule is
/// unit-testable without mutating the process environment.
pub(super) fn motion_enabled_from(value: Option<&str>) -> bool {
    value != Some("1")
}

/// The week total's opacity at `delta` (0..=1) of its change pulse:
/// starts at 0.35 and lands at exactly 1.0. Out-of-range deltas from a
/// misbehaving easing are clamped so the hero stays renderable.
pub(super) fn hero_opacity(delta: f32) -> f32 {
    0.35 + 0.65 * delta.clamp(0.0, 1.0)
}

/// How long the week total's change pulse runs.
pub(super) const HERO_PULSE_MS: u64 = 200;

/// How long a prompt panel takes to fade in when it mounts.
pub(super) const PROMPT_FADE_MS: u64 = 150;

/// A project's own colour, or the muted grey when it has none.
///
/// The colour comes from the service in `0xRRGGBB` form, so it is
/// converted here rather than at the call site.
pub(super) fn project_colour(colour: Option<u32>) -> Rgba {
    match colour {
        Some(c) => rgb(c),
        None => rgba(MUTED_TEXT),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reduced_motion_env_only_the_literal_1_disables_motion() {
        // The gate every animation checks first: unset, empty, and any
        // other value keep motion on; only an explicit `1` turns it off,
        // so a typo never silently freezes the UI mid-fade.
        assert!(motion_enabled_from(None));
        assert!(motion_enabled_from(Some("")));
        assert!(motion_enabled_from(Some("0")));
        assert!(motion_enabled_from(Some("yes")));
        assert!(!motion_enabled_from(Some("1")));
    }

    #[test]
    fn hero_pulse_starts_dim_and_lands_fully_opaque() {
        // The pulse is opacity-only feedback that a quick-add landed:
        // it must start visibly dimmer and end exactly at the resting
        // opacity, or the total would stay permanently washed out.
        assert_eq!(hero_opacity(0.0), 0.35);
        assert_eq!(hero_opacity(1.0), 1.0);
        assert!(hero_opacity(0.5) > hero_opacity(0.0));
        assert!(hero_opacity(0.5) < hero_opacity(1.0));
        // Out-of-range deltas (a misbehaving easing) stay renderable.
        assert_eq!(hero_opacity(-1.0), 0.35);
        assert_eq!(hero_opacity(2.0), 1.0);
    }
}
