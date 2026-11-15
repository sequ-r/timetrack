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
/// A fourth choice, the custom range (#133), keeps two typed days instead
/// of the anchor: GUI-local state, since core has no `ExportScope::Range`
/// and the range travels as two day instants end to end.
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
    /// Whether the range choice is showing instead of the scope pills'
    /// scopes: the anchor controls go inert and the two range days drive
    /// the preview and the range export.
    pub(crate) range_selected: bool,
    /// The custom from/to days (#133). Always present so switching back
    /// and forth never loses a typed range.
    pub(crate) range: ExportRange,
}

/// Which end of the custom range a keystroke goes to (#133).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RangeEnd {
    From,
    To,
}

/// The custom from/to day range (#133): two typed days, inclusive at both
/// ends, parsed with the snapshot zone exactly like the CLI's `--from` and
/// `--to`. The service buckets them with its own zone, so no new timezone
/// logic lives here — only the typed text and its carets.
#[derive(Debug, Clone)]
pub(crate) struct ExportRange {
    /// The instants selecting the first and last local days; each is an
    /// instant anywhere inside its day.
    pub(crate) from_ms: i64,
    pub(crate) to_ms: i64,
    /// The typed days as shown in the fields; each mirrors its instant's
    /// local day.
    pub(crate) from_text: String,
    pub(crate) to_text: String,
    /// The carets (and any selections) in the two day fields (#109),
    /// re-seated at the end whenever a field is (re)mirrored.
    pub(crate) from_caret: crate::app::textline::Caret,
    pub(crate) to_caret: crate::app::textline::Caret,
    /// Which field keystrokes go to, if either.
    pub(crate) editing: Option<RangeEnd>,
    /// The last bad day (or backwards range), shown inline; the instants
    /// are unchanged.
    pub(crate) error: Option<String>,
}

impl ExportRange {
    /// Both ends start on the anchor's day: a one-day range until typed.
    pub(crate) fn new(now_ms: i64) -> Self {
        let utc = timetrack_core::Tz::utc();
        let day = format_anchor_day(now_ms, &utc);
        ExportRange {
            from_ms: now_ms,
            to_ms: now_ms,
            from_caret: crate::app::textline::Caret::at_end(&day),
            to_caret: crate::app::textline::Caret::at_end(&day),
            from_text: day.clone(),
            to_text: day,
            editing: None,
            error: None,
        }
    }

    /// Stable identity of the range: the two local days joined by `..`
    /// (`2026-10-01..2026-10-15`), the same pair the CLI prints in its
    /// range status line, so one range reads the same everywhere.
    pub(crate) fn period_id(&self, tz: &timetrack_core::Tz) -> String {
        format!(
            "{}..{}",
            format_anchor_day(self.from_ms, tz),
            format_anchor_day(self.to_ms, tz)
        )
    }

    /// Whether the range runs forward: the service refuses a backwards
    /// range typed, and the preview would be empty, so the export click
    /// is refused locally with the same wording instead.
    pub(crate) fn is_ordered(&self, tz: &timetrack_core::Tz) -> bool {
        tz.day_of(self.from_ms) <= tz.day_of(self.to_ms)
    }

    /// Open one of the two day fields. The other field keeps its text but
    /// takes no keystrokes.
    pub(crate) fn start_editing(&mut self, end: RangeEnd) {
        self.editing = Some(end);
        self.error = None;
    }

    /// Close the open field without committing, mirroring both instants again.
    pub(crate) fn cancel_editing(&mut self, tz: &timetrack_core::Tz) {
        self.editing = None;
        self.error = None;
        self.from_text = format_anchor_day(self.from_ms, tz);
        self.from_caret = crate::app::textline::Caret::at_end(&self.from_text);
        self.to_text = format_anchor_day(self.to_ms, tz);
        self.to_caret = crate::app::textline::Caret::at_end(&self.to_text);
    }

    /// Insert typed text at the caret in the open field, clearing a stale error.
    pub(crate) fn type_text(&mut self, ch: &str) {
        let Some(end) = self.editing else { return };
        let (text, caret) = match end {
            RangeEnd::From => (&mut self.from_text, &mut self.from_caret),
            RangeEnd::To => (&mut self.to_text, &mut self.to_caret),
        };
        crate::app::textline::insert_text(text, caret, ch);
        self.error = None;
    }

    /// Apply a platform text-input edit (IME preedit or commit, #126)
    /// to the open field, clearing a stale error. False when no field
    /// is open.
    pub(crate) fn ime_edit(&mut self, edit: crate::app::textline::ImeEdit) -> bool {
        let Some(end) = self.editing else {
            return false;
        };
        let (text, caret) = match end {
            RangeEnd::From => (&mut self.from_text, &mut self.from_caret),
            RangeEnd::To => (&mut self.to_text, &mut self.to_caret),
        };
        crate::app::textline::apply_ime_edit(text, caret, edit);
        self.error = None;
        true
    }

    /// Delete backwards in the open field.
    pub(crate) fn backspace(&mut self) {
        let Some(end) = self.editing else { return };
        let (text, caret) = match end {
            RangeEnd::From => (&mut self.from_text, &mut self.from_caret),
            RangeEnd::To => (&mut self.to_text, &mut self.to_caret),
        };
        crate::app::textline::backspace(text, caret);
        self.error = None;
    }

    /// Press the mouse at a text offset in one of the day fields
    /// (#127): opens the field like a click, then places the caret —
    /// or extends to it with shift held in the already-open field. A
    /// press never clears the error: it edits no text.
    pub(crate) fn mouse_down(&mut self, end: RangeEnd, at: usize, shift: bool) {
        if self.editing != Some(end) {
            self.start_editing(end);
            // A newly opened field has no selection to extend.
            let (text, caret) = match end {
                RangeEnd::From => (&mut self.from_text, &mut self.from_caret),
                RangeEnd::To => (&mut self.to_text, &mut self.to_caret),
            };
            crate::app::textline::place_caret(text, caret, at);
            return;
        }
        let (text, caret) = match end {
            RangeEnd::From => (&mut self.from_text, &mut self.from_caret),
            RangeEnd::To => (&mut self.to_text, &mut self.to_caret),
        };
        if shift {
            crate::app::textline::extend_caret_to(text, caret, at);
        } else {
            crate::app::textline::place_caret(text, caret, at);
        }
    }

    /// Drag the mouse to a text offset in the open day field (#127).
    pub(crate) fn mouse_drag(&mut self, at: usize) {
        let Some(end) = self.editing else { return };
        let (text, caret) = match end {
            RangeEnd::From => (&mut self.from_text, &mut self.from_caret),
            RangeEnd::To => (&mut self.to_text, &mut self.to_caret),
        };
        crate::app::textline::extend_caret_to(text, caret, at);
    }

    /// Apply an editing key to the open day field; false when no field is
    /// open or the key is not an editing key.
    pub(crate) fn edit_key(&mut self, key: &str, shift: bool, ctrl: bool) -> bool {
        let Some(end) = self.editing else {
            return false;
        };
        let (text, caret) = match end {
            RangeEnd::From => (&mut self.from_text, &mut self.from_caret),
            RangeEnd::To => (&mut self.to_text, &mut self.to_caret),
        };
        let handled = crate::app::textline::handle_edit_key(text, caret, key, shift, ctrl);
        if handled {
            self.error = None;
        }
        handled
    }

    /// Commit the open field: the terminal spelling parsed with the
    /// snapshot zone. A bad day stays open with an inline error naming the
    /// field, and the instant is unchanged.
    pub(crate) fn commit(&mut self, tz: &timetrack_core::Tz) -> bool {
        let Some(end) = self.editing else {
            return false;
        };
        let (what, text) = match end {
            RangeEnd::From => ("from", self.from_text.clone()),
            RangeEnd::To => ("to", self.to_text.clone()),
        };
        match timetrack_core::parse_anchor_day(&text, what, tz) {
            Ok(instant) => {
                match end {
                    RangeEnd::From => {
                        self.from_ms = instant;
                        self.from_text = format_anchor_day(instant, tz);
                        self.from_caret = crate::app::textline::Caret::at_end(&self.from_text);
                    }
                    RangeEnd::To => {
                        self.to_ms = instant;
                        self.to_text = format_anchor_day(instant, tz);
                        self.to_caret = crate::app::textline::Caret::at_end(&self.to_text);
                    }
                }
                self.error = None;
                self.editing = None;
                true
            }
            Err(e) => {
                self.error = Some(e.to_string());
                false
            }
        }
    }

    /// Re-mirror both typed days from the snapshot zone after a snapshot
    /// arrives. Only the just-opened values are fixed: while a field is
    /// being typed (`editing`) its text is the user's, never the mirror.
    pub(crate) fn refresh_from_snapshot(&mut self, tz: &timetrack_core::Tz) {
        if self.editing.is_some() {
            return;
        }
        self.from_text = format_anchor_day(self.from_ms, tz);
        self.from_caret = crate::app::textline::Caret::at_end(&self.from_text);
        self.to_text = format_anchor_day(self.to_ms, tz);
        self.to_caret = crate::app::textline::Caret::at_end(&self.to_text);
    }
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
            range_selected: false,
            range: ExportRange::new(now_ms),
        }
    }

    /// Whether the anchor controls do anything: all time ignores the
    /// anchor, and the range choice drives the two range days instead.
    pub(crate) fn is_active(&self) -> bool {
        !self.range_selected && self.scope != timetrack_core::ExportScope::All
    }

    /// Whether the custom range is showing instead of a scope.
    pub(crate) fn is_range(&self) -> bool {
        self.range_selected
    }

    /// Whether any day field is being typed, anchor or range: the footer
    /// hint and the keyboard routing ask this one question.
    pub(crate) fn is_typing(&self) -> bool {
        self.editing || self.range.editing.is_some()
    }

    pub(crate) fn period_label(&self, tz: &timetrack_core::Tz) -> String {
        if self.range_selected {
            return self.range.period_id(tz);
        }
        export_period_label(self.scope, self.anchor_ms, tz)
    }

    /// Switch week/month/all, keeping the shared anchor; the typed day
    /// re-mirrors it. Leaves the range choice and keeps the typed range,
    /// so switching back loses nothing.
    pub(crate) fn set_scope(
        &mut self,
        scope: timetrack_core::ExportScope,
        tz: &timetrack_core::Tz,
    ) {
        self.scope = scope;
        self.range_selected = false;
        self.editing = false;
        self.error = None;
        self.anchor_text = format_anchor_day(self.anchor_ms, tz);
        self.caret = crate::app::textline::Caret::at_end(&self.anchor_text);
    }

    /// Show the custom range instead of a scope. The anchor field closes
    /// and the range fields keep whatever was typed.
    pub(crate) fn select_range(&mut self) {
        self.range_selected = true;
        self.editing = false;
        self.error = None;
    }

    /// One stepper press. Stepping leaves edit mode: the field mirrors the
    /// anchor again. A no-op for the range choice, which has no anchor.
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

    /// Open the typed day field. Never for all time or the range choice,
    /// which has its own two fields.
    pub(crate) fn start_editing(&mut self) {
        if !self.is_active() {
            return;
        }
        self.editing = true;
        self.error = None;
    }

    /// Open one of the range's day fields. Only for the range choice.
    pub(crate) fn start_range_editing(&mut self, end: RangeEnd) {
        if !self.range_selected {
            return;
        }
        self.range.start_editing(end);
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

    /// Apply a platform text-input edit (IME preedit or commit, #126)
    /// to the day field, clearing a stale error. False when the field
    /// is closed or inert.
    pub(crate) fn ime_edit(&mut self, edit: crate::app::textline::ImeEdit) -> bool {
        if !self.editing || !self.is_active() {
            return false;
        }
        crate::app::textline::apply_ime_edit(&mut self.anchor_text, &mut self.caret, edit);
        self.error = None;
        true
    }

    /// Delete backwards in the day field.
    pub(crate) fn backspace(&mut self) {
        if !self.editing || !self.is_active() {
            return;
        }
        crate::app::textline::backspace(&mut self.anchor_text, &mut self.caret);
        self.error = None;
    }

    /// Press the mouse at a text offset in the anchor day (#127):
    /// opens the field like `t`, then places the caret — or extends
    /// to it with shift held in the already-open field. A press never
    /// clears the error: it edits no text.
    pub(crate) fn mouse_down_anchor(&mut self, at: usize, shift: bool) {
        if !self.is_active() {
            return;
        }
        if !self.editing {
            self.start_editing();
            // A newly opened field has no selection to extend.
            crate::app::textline::place_caret(&self.anchor_text, &mut self.caret, at);
            return;
        }
        if shift {
            crate::app::textline::extend_caret_to(&self.anchor_text, &mut self.caret, at);
        } else {
            crate::app::textline::place_caret(&self.anchor_text, &mut self.caret, at);
        }
    }

    /// Drag the mouse to a text offset in the anchor day (#127).
    pub(crate) fn mouse_drag_anchor(&mut self, at: usize) {
        if !self.editing || !self.is_active() {
            return;
        }
        crate::app::textline::extend_caret_to(&self.anchor_text, &mut self.caret, at);
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
    /// typed (`editing`) the text is the user's, never the mirror. The
    /// range days re-mirror the same way, through their own guard.
    pub(crate) fn refresh_from_snapshot(&mut self, tz: &timetrack_core::Tz) {
        if !self.editing {
            self.anchor_text = format_anchor_day(self.anchor_ms, tz);
            self.caret = crate::app::textline::Caret::at_end(&self.anchor_text);
        }
        self.range.refresh_from_snapshot(tz);
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

    #[test]
    fn range_choice_names_both_days_and_parks_the_anchor() {
        // The fourth choice shows the two range days joined by `..` and
        // parks the anchor controls: the stepper and the anchor field go
        // inert while the range is showing.
        let mut picker = ExportPicker::new(day_anchor("2026-10-08"));
        assert!(!picker.is_range());
        picker.select_range();
        assert!(picker.is_range());
        assert!(!picker.is_active());
        assert_eq!(picker.period_label(&utc_tz()), "2026-10-08..2026-10-08");
        // The anchor is kept, not cleared, and stepping moves nothing.
        picker.step(1, &utc_tz());
        picker.step(-1, &utc_tz());
        assert_eq!(picker.anchor_ms, day_anchor("2026-10-08"));
        picker.start_editing();
        assert!(!picker.editing, "the anchor field never opens for a range");
        // Leaving the range restores the scope and keeps the typed range.
        picker.set_scope(timetrack_core::ExportScope::Month, &utc_tz());
        assert!(!picker.is_range());
        assert!(picker.is_active());
        assert_eq!(picker.period_label(&utc_tz()), "2026-10");
        picker.select_range();
        assert_eq!(picker.period_label(&utc_tz()), "2026-10-08..2026-10-08");
    }

    #[test]
    fn range_days_commit_independently_and_order_matters() {
        // Each end commits on its own; the pair orders the range and the
        // period id follows both ends.
        let mut picker = ExportPicker::new(day_anchor("2026-10-08"));
        picker.select_range();
        picker.start_range_editing(RangeEnd::From);
        picker.range.from_text = "2026-10-01".to_string();
        assert!(picker.range.commit(&utc_tz()));
        assert_eq!(picker.range.from_ms, day_anchor("2026-10-01"));
        assert_eq!(picker.range.to_ms, day_anchor("2026-10-08"));
        assert!(picker.range.is_ordered(&utc_tz()));
        assert_eq!(picker.period_label(&utc_tz()), "2026-10-01..2026-10-08");
        picker.start_range_editing(RangeEnd::To);
        picker.range.to_text = "2026-09-30".to_string();
        assert!(picker.range.commit(&utc_tz()));
        assert!(
            !picker.range.is_ordered(&utc_tz()),
            "a backwards range is representable so the click can refuse it"
        );
        assert_eq!(picker.period_label(&utc_tz()), "2026-10-01..2026-09-30");
    }

    #[test]
    fn range_bad_day_names_its_field_and_keeps_the_instant() {
        // Like the anchor field, a bad day stays open with an inline
        // error — but it names the range end, not the anchor.
        let before = day_anchor("2026-10-08");
        let mut picker = ExportPicker::new(before);
        picker.select_range();
        picker.start_range_editing(RangeEnd::To);
        picker.range.to_text = "2026-02-30".to_string();
        assert!(!picker.range.commit(&utc_tz()));
        assert_eq!(picker.range.to_ms, before);
        let err = picker
            .range
            .error
            .clone()
            .expect("an inline error is shown");
        assert!(err.contains("to"), "must name the field: {err}");
        assert!(err.contains("2026-02-30"), "must name the bad day: {err}");
        picker.start_range_editing(RangeEnd::From);
        picker.range.from_text = "not-a-day".to_string();
        assert!(!picker.range.commit(&utc_tz()));
        let err = picker
            .range
            .error
            .clone()
            .expect("an inline error is shown");
        assert!(err.contains("from"), "must name the field: {err}");
    }

    #[test]
    fn range_fields_open_only_for_the_range_choice() {
        // The range fields never open under a scope, and the anchor field
        // never opens under the range: one choice owns the keyboard.
        let mut picker = ExportPicker::new(day_anchor("2026-10-08"));
        picker.start_range_editing(RangeEnd::From);
        assert_eq!(
            picker.range.editing, None,
            "no range field opens under a scope"
        );
        assert!(!picker.is_typing());
        assert!(!picker.editing);
        picker.select_range();
        picker.start_range_editing(RangeEnd::To);
        assert_eq!(picker.range.editing, Some(RangeEnd::To));
        assert!(picker.is_typing());
        // A snapshot re-mirrors just-opened days but never a day being typed.
        picker.range.from_text = "2026-10-0".to_string();
        picker.range.editing = Some(RangeEnd::From);
        picker.refresh_from_snapshot(&utc_tz());
        assert_eq!(picker.range.from_text, "2026-10-0");
        picker.range.editing = None;
        picker.refresh_from_snapshot(&utc_tz());
        assert_eq!(picker.range.from_text, "2026-10-08");
    }

    #[test]
    fn ime_edits_compose_the_anchor_only_while_open() {
        use crate::app::textline::ImeEdit;
        let mut picker = ExportPicker::new(day_anchor("2026-10-08"));
        assert!(!picker.ime_edit(ImeEdit::Commit {
            range: None,
            text: "x",
        }));
        picker.start_editing();
        picker.error = Some("stale".into());
        assert!(picker.ime_edit(ImeEdit::Mark {
            range: None,
            text: "2026-10-",
            selected: None,
        }));
        assert_eq!(picker.error, None);
        assert!(picker.ime_edit(ImeEdit::Commit {
            range: None,
            text: "2026-10-15",
        }));
        assert_eq!(picker.anchor_text, "2026-10-08".to_string() + "2026-10-15");
        assert_eq!(picker.caret.marked, None);
    }

    #[test]
    fn a_press_opens_the_anchor_and_places_the_caret() {
        // #127: a closed field opens like `t`, then places; shift in
        // the open field extends. Inert scopes ignore the press.
        use crate::app::textline::selected_text;
        let mut picker = ExportPicker::new(day_anchor("2026-10-08"));
        picker.mouse_down_anchor(2, false);
        assert!(picker.editing);
        assert_eq!(picker.caret.pos, 2);
        picker.mouse_drag_anchor(6);
        assert_eq!(
            selected_text(&picker.anchor_text, &picker.caret),
            Some("26-1")
        );
        picker.mouse_down_anchor(0, true);
        assert_eq!(
            selected_text(&picker.anchor_text, &picker.caret),
            Some("20")
        );
        picker.set_scope(timetrack_core::ExportScope::All, &utc_tz());
        picker.mouse_down_anchor(1, false);
        assert!(!picker.editing);
    }

    #[test]
    fn a_press_opens_a_range_day_and_places_the_caret() {
        // #127, on the #133 days: pressing a day opens it; pressing
        // the other moves over; shift extends only within the open one.
        use crate::app::textline::selected_text;
        let mut picker = ExportPicker::new(day_anchor("2026-10-08"));
        picker.select_range();
        picker.range.mouse_down(RangeEnd::From, 2, false);
        assert_eq!(picker.range.editing, Some(RangeEnd::From));
        assert_eq!(picker.range.from_caret.pos, 2);
        picker.range.mouse_drag(6);
        assert_eq!(
            selected_text(&picker.range.from_text, &picker.range.from_caret),
            Some("26-1")
        );
        picker.range.mouse_down(RangeEnd::To, 1, true);
        assert_eq!(picker.range.editing, Some(RangeEnd::To));
        assert_eq!(picker.range.to_caret.pos, 1);
        assert_eq!(
            selected_text(&picker.range.to_text, &picker.range.to_caret),
            None
        );
    }

    #[test]
    fn ime_edits_compose_the_open_range_day_only() {
        use crate::app::textline::ImeEdit;
        let mut picker = ExportPicker::new(day_anchor("2026-10-08"));
        picker.select_range();
        picker.start_range_editing(RangeEnd::To);
        picker.range.error = Some("stale".into());
        assert!(picker.range.ime_edit(ImeEdit::Commit {
            range: None,
            text: "5",
        }));
        assert!(picker.range.to_text.ends_with('5'));
        assert_eq!(picker.range.error, None);
        assert!(!picker.ime_edit(ImeEdit::Commit {
            range: None,
            text: "x",
        }));
        picker.range.editing = None;
        assert!(!picker.range.ime_edit(ImeEdit::Commit {
            range: None,
            text: "x",
        }));
    }
}
