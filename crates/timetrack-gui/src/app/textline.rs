/* gui/app/textline.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! Caret and selection editing for the GUI's single-line fields (#109).
//!
//! The dialogs keep their text as plain `String`s, because resolving a
//! form is pure string work the tests already pin. What they lacked was
//! everything between the characters: a caret that can move, insert
//! mid-word, delete forwards, and hold a selection. That state is one
//! small [`Caret`] per editing surface plus these functions over
//! `(&mut String, &mut Caret)` — so entry descriptions, project names,
//! split moments and the export anchor all edit the same way, and the
//! rules are testable without a window.
//!
//! What this is *not*: an IME. Composition (dead keys, CJK candidates)
//! needs gpui's text-input protocol plumbing, which the spike in
//! `docs/text-input.md` scopes as follow-up work. Composed characters
//! the platform does deliver (é as one key event) insert here as text.

/// A caret: a byte offset into the field's text, always on a char
/// boundary, plus the selection anchor when a selection is being held.
/// The selection is the range between `anchor` and `pos` when they
/// differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Caret {
    pub(crate) pos: usize,
    pub(crate) anchor: Option<usize>,
}

impl Caret {
    /// A caret at the end of `text` — where a prefilled field starts.
    pub(crate) fn at_end(text: &str) -> Self {
        Caret {
            pos: text.len(),
            anchor: None,
        }
    }

    /// Keep the caret inside `text` after the text was replaced behind
    /// its back (a programmatic prefill, an external set).
    pub(crate) fn clamp(&mut self, text: &str) {
        self.pos = floor_boundary(text, self.pos.min(text.len()));
        if let Some(a) = self.anchor {
            self.anchor = Some(floor_boundary(text, a.min(text.len())));
        }
    }
}

fn floor_boundary(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

fn prev_boundary(text: &str, at: usize) -> usize {
    text[..floor_boundary(text, at)]
        .char_indices()
        .next_back()
        .map(|(i, _)| i)
        .unwrap_or(0)
}

fn next_boundary(text: &str, at: usize) -> usize {
    let at = floor_boundary(text, at);
    text[at..]
        .chars()
        .next()
        .map(|c| at + c.len_utf8())
        .unwrap_or(at)
}

/// The selection as a byte range, if one is held.
pub(crate) fn selection_range(caret: &Caret) -> Option<std::ops::Range<usize>> {
    let anchor = caret.anchor?;
    if anchor == caret.pos {
        return None;
    }
    Some(anchor.min(caret.pos)..anchor.max(caret.pos))
}

/// The selected text, for copy and cut.
pub(crate) fn selected_text<'a>(text: &'a str, caret: &Caret) -> Option<&'a str> {
    selection_range(caret).map(|r| &text[r])
}

/// The three slices a renderer draws: before, selected, after — with
/// the caret sitting between before/selected when there is no
/// selection. Slices of the stored text: valid Unicode renders as
/// itself, never re-encoded (#101's sanitization happens at the render
/// call site, as everywhere else).
pub(crate) fn display_parts<'a>(text: &'a str, caret: &Caret) -> (&'a str, &'a str, &'a str) {
    match selection_range(caret) {
        Some(r) => (&text[..r.start], &text[r.clone()], &text[r.end..]),
        None => {
            let pos = floor_boundary(text, caret.pos);
            (&text[..pos], "", &text[pos..])
        }
    }
}

/// Delete the selection if one is held. Reports whether anything went.
pub(crate) fn delete_selection(text: &mut String, caret: &mut Caret) -> bool {
    caret.clamp(text);
    let Some(range) = selection_range(caret) else {
        return false;
    };
    text.replace_range(range.clone(), "");
    caret.pos = range.start;
    caret.anchor = None;
    true
}

/// Insert typed text at the caret, replacing any selection. Control
/// characters never land in a field (a paste carrying newlines keeps
/// its words, joined by nothing — single-line fields have nowhere for
/// a line break to go, and stored controls are refused by the rules).
pub(crate) fn insert_text(text: &mut String, caret: &mut Caret, inserted: &str) {
    caret.clamp(text);
    delete_selection(text, caret);
    let clean: String = inserted.chars().filter(|c| !c.is_control()).collect();
    text.insert_str(caret.pos, &clean);
    caret.pos += clean.len();
    caret.anchor = None;
}

/// Delete backwards: the selection, or the character before the caret.
pub(crate) fn backspace(text: &mut String, caret: &mut Caret) {
    caret.clamp(text);
    if delete_selection(text, caret) {
        return;
    }
    if caret.pos == 0 {
        return;
    }
    let prev = prev_boundary(text, caret.pos);
    text.replace_range(prev..caret.pos, "");
    caret.pos = prev;
}

/// Delete forwards: the selection, or the character after the caret.
pub(crate) fn delete_forward(text: &mut String, caret: &mut Caret) {
    caret.clamp(text);
    if delete_selection(text, caret) {
        return;
    }
    if caret.pos >= text.len() {
        return;
    }
    let next = next_boundary(text, caret.pos);
    text.replace_range(caret.pos..next, "");
}

fn begin_selection(caret: &mut Caret, extend: bool) {
    if extend {
        caret.anchor.get_or_insert(caret.pos);
    } else {
        caret.anchor = None;
    }
}

/// One character left, collapsing a selection to its left edge unless
/// extending it.
pub(crate) fn move_left(text: &str, caret: &mut Caret, extend: bool) {
    caret.clamp(text);
    if !extend {
        if let Some(range) = selection_range(caret) {
            caret.pos = range.start;
            caret.anchor = None;
            return;
        }
    }
    begin_selection(caret, extend);
    caret.pos = prev_boundary(text, caret.pos);
    if caret.anchor == Some(caret.pos) {
        caret.anchor = None;
    }
}

/// One character right, the mirror of [`move_left`].
pub(crate) fn move_right(text: &str, caret: &mut Caret, extend: bool) {
    caret.clamp(text);
    if !extend {
        if let Some(range) = selection_range(caret) {
            caret.pos = range.end;
            caret.anchor = None;
            return;
        }
    }
    begin_selection(caret, extend);
    caret.pos = next_boundary(text, caret.pos);
    if caret.anchor == Some(caret.pos) {
        caret.anchor = None;
    }
}

pub(crate) fn move_home(text: &str, caret: &mut Caret, extend: bool) {
    caret.clamp(text);
    begin_selection(caret, extend);
    caret.pos = 0;
    if caret.anchor == Some(0) {
        caret.anchor = None;
    }
}

pub(crate) fn move_end(text: &str, caret: &mut Caret, extend: bool) {
    caret.clamp(text);
    begin_selection(caret, extend);
    caret.pos = text.len();
    if caret.anchor == Some(caret.pos) {
        caret.anchor = None;
    }
}

pub(crate) fn select_all(text: &str, caret: &mut Caret) {
    caret.anchor = Some(0);
    caret.pos = text.len();
    if text.is_empty() {
        caret.anchor = None;
    }
}

/// Apply one editing key. Returns whether the key was an editing key at
/// all, so callers can let everything else fall through to their own
/// handling. `key` is gpui's key name ("left", "backspace", …);
/// clipboard chords are handled where a clipboard exists (the view),
/// not here.
pub(crate) fn handle_edit_key(
    text: &mut String,
    caret: &mut Caret,
    key: &str,
    shift: bool,
    ctrl: bool,
) -> bool {
    match key {
        "left" => move_left(text, caret, shift),
        "right" => move_right(text, caret, shift),
        "home" => move_home(text, caret, shift),
        "end" => move_end(text, caret, shift),
        "backspace" => backspace(text, caret),
        "delete" => delete_forward(text, caret),
        "a" if ctrl => select_all(text, caret),
        _ => return false,
    }
    true
}

/// The field as drawn: the text with a caret bar at the caret, or the
/// selection bracketed when one is held. A string like the trailing
/// `_` the append-only fields used to draw, so every prompt renders its
/// caret the same way without segmented text layout. The text is
/// display-sanitized first (#101), with the caret clamped into it.
pub(crate) fn render_with_caret(text: &str, caret: &Caret) -> String {
    let clean = timetrack_core::sanitize_display(text);
    let mut caret = *caret;
    caret.clamp(&clean);
    let (before, selected, after) = display_parts(&clean, &caret);
    if selected.is_empty() {
        format!("{before}▏{after}")
    } else {
        format!("{before}⟦{selected}⟧{after}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_inserts_at_the_caret_not_only_the_end() {
        // #109: the mate scenario — "caffé" typed for "Work", fixed by
        // editing mid-word instead of deleting back to it.
        let mut text = "caff".to_string();
        let mut caret = Caret::at_end(&text);
        insert_text(&mut text, &mut caret, "e");
        move_left(&text, &mut caret, false);
        insert_text(&mut text, &mut caret, "è");
        assert_eq!(text, "caffèe");
        backspace(&mut text, &mut caret); // remove the stray e's neighbour…
        assert_eq!(text, "caffe");
    }

    #[test]
    fn caret_moves_by_characters_not_bytes() {
        // Multi-byte text must never split a character.
        let text = "aé日b".to_string();
        let mut caret = Caret::at_end(&text);
        move_left(&text, &mut caret, false);
        assert_eq!(caret.pos, "aé日".len());
        move_left(&text, &mut caret, false);
        assert_eq!(caret.pos, "aé".len());
        move_right(&text, &mut caret, false);
        assert_eq!(caret.pos, "aé日".len());
        move_home(&text, &mut caret, false);
        assert_eq!(caret.pos, 0);
    }

    #[test]
    fn selection_replaces_deletes_and_collapses() {
        let mut text = "Deep Wrok".to_string();
        let mut caret = Caret::at_end(&text);
        // Select "Wrok" backwards and type over it.
        for _ in 0..4 {
            move_left(&text, &mut caret, true);
        }
        assert_eq!(selected_text(&text, &caret), Some("Wrok"));
        insert_text(&mut text, &mut caret, "Work");
        assert_eq!(text, "Deep Work");
        assert_eq!(selected_text(&text, &caret), None);
        // Select all, then a plain left collapses to the left edge.
        select_all(&text, &mut caret);
        assert_eq!(selected_text(&text, &caret), Some("Deep Work"));
        move_left(&text, &mut caret, false);
        assert_eq!(caret.pos, 0);
        assert_eq!(selected_text(&text, &caret), None);
        // Backspace deletes a held selection whole.
        select_all(&text, &mut caret);
        backspace(&mut text, &mut caret);
        assert_eq!(text, "");
    }

    #[test]
    fn pasted_text_loses_its_controls() {
        let mut text = String::new();
        let mut caret = Caret::default();
        insert_text(&mut text, &mut caret, "a\nb\tc\u{1b}[31m");
        assert_eq!(text, "abc[31m");
    }

    #[test]
    fn edit_keys_are_recognised_and_others_fall_through() {
        let mut text = "abc".to_string();
        let mut caret = Caret::at_end(&text);
        assert!(handle_edit_key(&mut text, &mut caret, "left", false, false));
        assert_eq!(caret.pos, 2);
        assert!(handle_edit_key(
            &mut text, &mut caret, "delete", false, false
        ));
        assert_eq!(text, "ab");
        assert!(handle_edit_key(&mut text, &mut caret, "a", false, true));
        assert_eq!(selected_text(&text, &caret), Some("ab"));
        assert!(!handle_edit_key(
            &mut text, &mut caret, "enter", false, false
        ));
        assert!(!handle_edit_key(&mut text, &mut caret, "x", false, false));
    }

    #[test]
    fn rendering_puts_the_caret_bar_and_brackets_the_selection() {
        let text = "caffè";
        let mut caret = Caret::at_end(text);
        assert_eq!(render_with_caret(text, &caret), "caffè▏");
        move_left(text, &mut caret, false);
        assert_eq!(render_with_caret(text, &caret), "caff▏è");
        move_left(text, &mut caret, true);
        move_left(text, &mut caret, true);
        assert_eq!(render_with_caret(text, &caret), "ca⟦ff⟧è");
        // Valid stored Unicode renders as itself (#109/#101).
        let text = "日本語 🎉";
        assert_eq!(render_with_caret(text, &Caret::at_end(text)), "日本語 🎉▏");
    }

    #[test]
    fn display_parts_split_at_the_caret_and_selection() {
        let text = "caffè";
        let mut caret = Caret::at_end(text);
        move_left(text, &mut caret, false);
        assert_eq!(display_parts(text, &caret), ("caff", "", "è"));
        move_left(text, &mut caret, true);
        move_left(text, &mut caret, true);
        assert_eq!(display_parts(text, &caret), ("ca", "ff", "è"));
    }
}
