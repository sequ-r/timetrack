/* gui/app/export/picker.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The Export tab's scope-plus-anchor picker (spec #19).
//!
//! One shared anchor selects which week or month the stepper and the typed
//! day point at; all time ignores it. Plain state over values like the entry
//! dialog, so every transition is unit-testable without a window.

/// The Export tab's scope-plus-anchor picker (spec #19).
///
/// One shared anchor selects which week or month the stepper and the typed
/// day point at; all time ignores it. The typed day accepts the terminal day
/// spelling (`YYYY-MM-DD`) parsed with the snapshot zone, exactly like the
/// CLI's `--at`. The preview shows the first rows of the scoped export
/// oldest first following this scope plus anchor, and the export buttons
/// below save that scope with this anchor (all time ignores it on both).
///
/// Plain state over values like the entry dialog, so every transition is
/// unit-testable without a window.
#[derive(Debug, Clone)]
pub(crate) struct ExportPicker {
    pub(crate) scope: timetrack_core::ExportScope,
    /// The instant selecting which week or month; ignored by all time.
    pub(crate) anchor_ms: i64,
    /// The typed day as shown in the field; mirrors the anchor's local day.
    pub(crate) anchor_text: String,
    /// The caret (and any selection) in the typed day (#109), re-seated
    /// at the end whenever the field is (re)mirrored from the anchor.
    pub(crate) caret: crate::app::textline::Caret,
    /// Whether keystrokes go to the typed day field.
    pub(crate) editing: bool,
    /// The last bad day, shown inline; the anchor is unchanged.
    pub(crate) error: Option<String>,
}

impl ExportPicker {
    /// This week anchored at now: the common retrospective needs no typing.
    /// The typed day starts in UTC (the default snapshot's zone) and is
    /// re-mirrored from the snapshot zone on arrival via
    /// [`ExportPicker::refresh_from_snapshot`].
    pub(crate) fn new(now_ms: i64) -> Self {
        let utc = timetrack_core::Tz::utc();
        let anchor_text = format_anchor_day(now_ms, &utc);
        let caret = crate::app::textline::Caret::at_end(&anchor_text);
        ExportPicker {
            scope: timetrack_core::ExportScope::Week,
            anchor_ms: now_ms,
            caret,
            anchor_text,
            editing: false,
            error: None,
        }
    }

    /// All time ignores the anchor, so its controls render inert.
    pub(crate) fn is_active(&self) -> bool {
        self.scope != timetrack_core::ExportScope::All
    }

    pub(crate) fn period_label(&self, tz: &timetrack_core::Tz) -> String {
        export_period_label(self.scope, self.anchor_ms, tz)
    }

    /// Switch week/month/all, keeping the shared anchor; the typed day
    /// re-mirrors it.
    pub(crate) fn set_scope(
        &mut self,
        scope: timetrack_core::ExportScope,
        tz: &timetrack_core::Tz,
    ) {
        self.scope = scope;
        self.editing = false;
        self.error = None;
        self.anchor_text = format_anchor_day(self.anchor_ms, tz);
        self.caret = crate::app::textline::Caret::at_end(&self.anchor_text);
    }

    /// One stepper press. Stepping leaves edit mode: the field mirrors the
    /// anchor again.
    pub(crate) fn step(&mut self, delta: i32, tz: &timetrack_core::Tz) {
        if !self.is_active() {
            return;
        }
        self.anchor_ms = shift_export_anchor(self.scope, self.anchor_ms, delta, tz);
        self.anchor_text = format_anchor_day(self.anchor_ms, tz);
        self.caret = crate::app::textline::Caret::at_end(&self.anchor_text);
        self.editing = false;
        self.error = None;
    }

    /// Open the typed day field. Never for all time.
    pub(crate) fn start_editing(&mut self) {
        if !self.is_active() {
            return;
        }
        self.editing = true;
        self.error = None;
    }

    /// Close the field without committing, mirroring the anchor again.
    pub(crate) fn cancel_editing(&mut self, tz: &timetrack_core::Tz) {
        self.editing = false;
        self.error = None;
        self.anchor_text = format_anchor_day(self.anchor_ms, tz);
        self.caret = crate::app::textline::Caret::at_end(&self.anchor_text);
    }

    /// Insert typed text at the caret in the day field, clearing a
    /// stale error.
    pub(crate) fn type_text(&mut self, ch: &str) {
        if !self.editing || !self.is_active() {
            return;
        }
        crate::app::textline::insert_text(&mut self.anchor_text, &mut self.caret, ch);
        self.error = None;
    }

    /// Delete backwards in the day field.
    pub(crate) fn backspace(&mut self) {
        if !self.editing || !self.is_active() {
            return;
        }
        crate::app::textline::backspace(&mut self.anchor_text, &mut self.caret);
        self.error = None;
    }

    /// Apply an editing key (caret movement, selection, deletes) to the
    /// day field; false when not editable or not an editing key.
    pub(crate) fn edit_key(&mut self, key: &str, shift: bool, ctrl: bool) -> bool {
        if !self.editing || !self.is_active() {
            return false;
        }
        let handled = crate::app::textline::handle_edit_key(
            &mut self.anchor_text,
            &mut self.caret,
            key,
            shift,
            ctrl,
        );
        if handled {
            self.error = None;
        }
        handled
    }

    /// Commit the typed day: the terminal spelling parsed with the snapshot
    /// zone. A bad day stays open with an inline error and the anchor is
    /// unchanged.
    pub(crate) fn commit(&mut self, tz: &timetrack_core::Tz) -> bool {
        if !self.editing || !self.is_active() {
            return false;
        }
        match timetrack_core::parse_anchor_day(&self.anchor_text, "anchor", tz) {
            Ok(anchor) => {
                self.anchor_ms = anchor;
                self.anchor_text = format_anchor_day(anchor, tz);
                self.caret = crate::app::textline::Caret::at_end(&self.anchor_text);
                self.error = None;
                self.editing = false;
                true
            }
            Err(e) => {
                self.error = Some(e.to_string());
                false
            }
        }
    }

    /// Re-mirror the typed day from the snapshot zone after a snapshot
    /// arrives. Only the just-opened value is fixed: while the field is being
    /// typed (`editing`) the text is the user's, never the mirror.
    pub(crate) fn refresh_from_snapshot(&mut self, tz: &timetrack_core::Tz) {
        if !self.editing {
            self.anchor_text = format_anchor_day(self.anchor_ms, tz);
            self.caret = crate::app::textline::Caret::at_end(&self.anchor_text);
        }
    }
}

/// The anchor's local day as `YYYY-MM-DD`, the spelling the typed field and
/// the CLI share. Thin wrapper over the shared core helper so the GUI mirror
/// and the shifters cannot disagree about the day boundary.
pub(crate) fn format_anchor_day(anchor_ms: i64, tz: &timetrack_core::Tz) -> String {
    timetrack_core::anchor_day_string(anchor_ms, tz)
}

/// What the stepper sits on: the ISO week id, the month id, or all time
/// (which has no id because its anchor is ignored). Thin wrapper over the
/// shared scope table so both UIs agree.
pub(crate) fn export_period_label(
    scope: timetrack_core::ExportScope,
    anchor_ms: i64,
    tz: &timetrack_core::Tz,
) -> String {
    scope.period_id(anchor_ms, tz)
}

/// Move the anchor one scope step per `delta`. Thin wrapper over the shared
/// scope table: both week and month resolve through the day spelling, so a
/// DST edge cannot move the anchor off midday.
pub(crate) fn shift_export_anchor(
    scope: timetrack_core::ExportScope,
    anchor_ms: i64,
    delta: i32,
    tz: &timetrack_core::Tz,
) -> i64 {
    scope.shift(anchor_ms, delta, tz)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::testsupport::{day_anchor, utc_tz};

    #[test]
    fn anchor_picker_defaults_to_this_week_at_now() {
        // The anchor is now at view creation; week is the default scope, and
        // the typed day mirrors the anchor's local day.
        let now = 1_791_460_800_000; // Thu 8 Oct 2026 12:00 UTC
        let picker = ExportPicker::new(now);
        assert_eq!(picker.scope, timetrack_core::ExportScope::Week);
        assert_eq!(picker.anchor_ms, now);
        assert_eq!(picker.anchor_text, "2026-10-08");
        assert!(picker.is_active());
        assert_eq!(picker.period_label(&utc_tz()), "2026-W41");
    }

    #[test]
    fn week_stepper_moves_one_week_per_press_across_year_boundary() {
        // 2026-01-01 was a Thursday opening ISO week 2026-W01; one press back
        // lands in 2025-W52, and the label follows the core week id.
        let mut picker = ExportPicker::new(day_anchor("2026-01-01"));
        assert_eq!(picker.period_label(&utc_tz()), "2026-W01");
        picker.step(-1, &utc_tz());
        assert_eq!(picker.anchor_ms, day_anchor("2025-12-25"));
        assert_eq!(picker.anchor_text, "2025-12-25");
        assert_eq!(picker.period_label(&utc_tz()), "2025-W52");
        picker.step(1, &utc_tz());
        assert_eq!(picker.anchor_ms, day_anchor("2026-01-01"));
        assert_eq!(picker.period_label(&utc_tz()), "2026-W01");
    }

    #[test]
    fn month_stepper_moves_one_month_per_press_across_year_boundary() {
        let mut picker = ExportPicker::new(day_anchor("2026-01-15"));
        assert_eq!(picker.period_label(&utc_tz()), "2026-W03");
        picker.set_scope(timetrack_core::ExportScope::Month, &utc_tz());
        assert_eq!(picker.period_label(&utc_tz()), "2026-01");
        picker.step(-1, &utc_tz());
        assert_eq!(picker.anchor_ms, day_anchor("2025-12-15"));
        assert_eq!(picker.anchor_text, "2025-12-15");
        assert_eq!(picker.period_label(&utc_tz()), "2025-12");
        picker.step(1, &utc_tz());
        assert_eq!(picker.anchor_ms, day_anchor("2026-01-15"));
        assert_eq!(picker.period_label(&utc_tz()), "2026-01");
    }

    #[test]
    fn month_stepper_clamps_to_short_months() {
        // 2026 is not a leap year, so stepping back from 31 March lands on
        // 28 February rather than spilling into March.
        let mut picker = ExportPicker::new(day_anchor("2026-03-31"));
        picker.set_scope(timetrack_core::ExportScope::Month, &utc_tz());
        picker.step(-1, &utc_tz());
        assert_eq!(picker.anchor_ms, day_anchor("2026-02-28"));
        assert_eq!(picker.anchor_text, "2026-02-28");
    }

    #[test]
    fn typed_day_and_stepper_share_one_anchor() {
        // Typing a day moves the same anchor the stepper moves: after the
        // commit the label follows, and the next press steps from there.
        let mut picker = ExportPicker::new(day_anchor("2026-10-08"));
        picker.start_editing();
        picker.anchor_text = "2026-10-12".to_string();
        assert!(picker.commit(&utc_tz()));
        assert_eq!(picker.anchor_ms, day_anchor("2026-10-12"));
        assert_eq!(picker.anchor_text, "2026-10-12");
        assert_eq!(picker.error, None);
        assert_eq!(picker.period_label(&utc_tz()), "2026-W42");
        picker.step(1, &utc_tz());
        assert_eq!(picker.anchor_ms, day_anchor("2026-10-19"));
        assert_eq!(picker.anchor_text, "2026-10-19");
        assert_eq!(picker.period_label(&utc_tz()), "2026-W43");
    }

    #[test]
    fn bad_day_leaves_the_anchor_alone() {
        let before = day_anchor("2026-10-08");
        let mut picker = ExportPicker::new(before);
        for bad in ["2026-13-01", "2026-02-30", "not-a-day", "2026-10"] {
            picker.start_editing();
            picker.anchor_text = bad.to_string();
            assert!(!picker.commit(&utc_tz()), "{bad:?} must not commit");
            assert_eq!(
                picker.anchor_ms, before,
                "{bad:?} must leave the anchor alone"
            );
            let err = picker.error.clone().expect("an inline error is shown");
            assert!(err.contains("anchor"), "must name the field: {err}");
            assert!(err.contains(bad), "must name the bad day: {err}");
        }
    }

    #[test]
    fn all_time_leaves_the_anchor_controls_inert() {
        // All time ignores the anchor, so the stepper is a no-op and the
        // field never opens there; the anchor itself is kept, not cleared.
        let before = day_anchor("2026-10-08");
        let mut picker = ExportPicker::new(before);
        picker.set_scope(timetrack_core::ExportScope::All, &utc_tz());
        assert!(!picker.is_active());
        assert_eq!(picker.period_label(&utc_tz()), "all time");
        picker.step(-1, &utc_tz());
        picker.step(1, &utc_tz());
        assert_eq!(picker.anchor_ms, before, "stepping all time moves nothing");
        picker.start_editing();
        assert!(!picker.editing, "the day field never opens for all time");
        assert_eq!(picker.anchor_ms, before);
    }

    #[test]
    fn switching_scope_keeps_the_anchor() {
        let anchor = day_anchor("2026-09-15");
        let mut picker = ExportPicker::new(anchor);
        assert_eq!(picker.period_label(&utc_tz()), "2026-W38");
        picker.set_scope(timetrack_core::ExportScope::Month, &utc_tz());
        assert_eq!(picker.anchor_ms, anchor);
        assert_eq!(picker.anchor_text, "2026-09-15");
        assert_eq!(picker.period_label(&utc_tz()), "2026-09");
    }

    #[test]
    fn period_labels_use_the_core_week_and_month_ids() {
        assert_eq!(
            export_period_label(
                timetrack_core::ExportScope::Week,
                day_anchor("2026-10-08"),
                &utc_tz()
            ),
            "2026-W41"
        );
        assert_eq!(
            export_period_label(
                timetrack_core::ExportScope::Month,
                day_anchor("2026-09-15"),
                &utc_tz()
            ),
            "2026-09"
        );
        assert_eq!(
            export_period_label(
                timetrack_core::ExportScope::All,
                day_anchor("2026-09-15"),
                &utc_tz()
            ),
            "all time"
        );
        assert_eq!(
            shift_export_anchor(
                timetrack_core::ExportScope::All,
                day_anchor("2026-09-15"),
                1,
                &utc_tz()
            ),
            day_anchor("2026-09-15"),
            "all time ignores the anchor entirely"
        );
    }

    #[test]
    fn week_stepper_stays_at_midday_across_dst() {
        // GUI pin of the core DST fix: stepping a week across Rome's spring
        // forward must land at midday, not 11:00 drifting off the day.
        let rome = timetrack_core::Tz::parse("Europe/Rome").unwrap();
        let before = timetrack_core::parse_anchor_day("2026-03-22", "anchor", &rome).unwrap();
        let after = shift_export_anchor(timetrack_core::ExportScope::Week, before, 1, &rome);
        assert_eq!(format_anchor_day(after, &rome), "2026-03-29");
        assert_eq!(timetrack_core::format_local_hm(after, &rome), "12:00");
        let back = shift_export_anchor(timetrack_core::ExportScope::Week, after, -1, &rome);
        assert_eq!(format_anchor_day(back, &rome), "2026-03-22");
        assert_eq!(timetrack_core::format_local_hm(back, &rome), "12:00");
    }

    #[test]
    fn typed_day_refreshes_from_the_snapshot_zone() {
        // The picker opens on UTC (the default snapshot's zone); the first
        // real snapshot re-mirrors the just-opened value, but never a day
        // being typed.
        let rome = timetrack_core::Tz::parse("Europe/Rome").unwrap();
        // 2026-07-05 23:30 UTC is 2026-07-06 in Rome (+2): the UTC mirror
        // shows the 5th, the Rome mirror the 6th.
        let anchor = 1_783_294_200_000;
        assert_eq!(format_anchor_day(anchor, &utc_tz()), "2026-07-05");
        let mut picker = ExportPicker::new(anchor);
        assert_eq!(picker.anchor_text, "2026-07-05");
        picker.refresh_from_snapshot(&rome);
        assert_eq!(picker.anchor_text, "2026-07-06");
        // While typing, the user's text wins.
        picker.start_editing();
        picker.anchor_text = "2026-10-".to_string();
        picker.refresh_from_snapshot(&utc_tz());
        assert_eq!(picker.anchor_text, "2026-10-");
    }
}
