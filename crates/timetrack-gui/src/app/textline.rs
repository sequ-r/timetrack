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
//! Composition (dead keys, CJK candidates) lives here too (#126): the
//! caret carries the marked (preedit) range the platform is composing,
//! and [`replace_text_in_range`] / [`replace_and_mark_text_in_range`]
//! are the pure halves of gpui's text-input protocol — the view's input
//! handler (`super::ime`) only converts units and routes to the focused
//! field. Offsets here are bytes; the protocol speaks UTF-16, converted
//! by [`offset_to_utf16`] / [`offset_from_utf16`].

use std::ops::Range;

/// A caret: a byte offset into the field's text, always on a char
/// boundary, plus the selection anchor when a selection is being held.
/// The selection is the range between `anchor` and `pos` when they
/// differ.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Caret {
    pub(crate) pos: usize,
    pub(crate) anchor: Option<usize>,
    /// The marked (composing) range, while the platform holds preedit
    /// text here. Byte offsets on char boundaries with `start < end`;
    /// any direct key-driven edit abandons the composition (the text
    /// stays as plain text) rather than tracking it.
    pub(crate) marked: Option<Range<usize>>,
}

impl Caret {
    /// A caret at the end of `text` — where a prefilled field starts.
    pub(crate) fn at_end(text: &str) -> Self {
        Caret {
            pos: text.len(),
            anchor: None,
            marked: None,
        }
    }

    /// Keep the caret inside `text` after the text was replaced behind
    /// its back (a programmatic prefill, an external set). A marked
    /// range that no longer spans text is dropped.
    pub(crate) fn clamp(&mut self, text: &str) {
        self.pos = floor_boundary(text, self.pos.min(text.len()));
        if let Some(a) = self.anchor {
            self.anchor = Some(floor_boundary(text, a.min(text.len())));
        }
        if let Some(m) = self.marked.clone() {
            let start = floor_boundary(text, m.start.min(text.len()));
            let end = floor_boundary(text, m.end.min(text.len()));
            self.marked = (start < end).then_some(start..end);
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
    // A direct edit abandons any composition in flight (#126): the
    // marked text stays as plain text, untracked.
    caret.marked = None;
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
    // `delete_selection` abandons any composition in flight (#126),
    // with or without a selection held.
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
    // `delete_selection` abandons any composition in flight (#126),
    // with or without a selection held.
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
    // Moving the caret abandons any composition in flight (#126).
    caret.marked = None;
    if !extend && let Some(range) = selection_range(caret) {
        caret.pos = range.start;
        caret.anchor = None;
        return;
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
    // Moving the caret abandons any composition in flight (#126).
    caret.marked = None;
    if !extend && let Some(range) = selection_range(caret) {
        caret.pos = range.end;
        caret.anchor = None;
        return;
    }
    begin_selection(caret, extend);
    caret.pos = next_boundary(text, caret.pos);
    if caret.anchor == Some(caret.pos) {
        caret.anchor = None;
    }
}

pub(crate) fn move_home(text: &str, caret: &mut Caret, extend: bool) {
    caret.clamp(text);
    // Moving the caret abandons any composition in flight (#126).
    caret.marked = None;
    begin_selection(caret, extend);
    caret.pos = 0;
    if caret.anchor == Some(0) {
        caret.anchor = None;
    }
}

pub(crate) fn move_end(text: &str, caret: &mut Caret, extend: bool) {
    caret.clamp(text);
    // Moving the caret abandons any composition in flight (#126).
    caret.marked = None;
    begin_selection(caret, extend);
    caret.pos = text.len();
    if caret.anchor == Some(caret.pos) {
        caret.anchor = None;
    }
}

pub(crate) fn select_all(text: &str, caret: &mut Caret) {
    // Selecting abandons any composition in flight (#126).
    caret.marked = None;
    caret.anchor = Some(0);
    caret.pos = text.len();
    if text.is_empty() {
        caret.anchor = None;
    }
}

/// Place the caret at a byte offset from a mouse click (#127):
/// collapses any selection and abandons any composition in flight
/// (#126) — the text stays as plain text. Out-of-range offsets floor
/// to a char boundary; past-the-end clamps to it.
pub(crate) fn place_caret(text: &str, caret: &mut Caret, at: usize) {
    caret.clamp(text);
    // A click moves the caret like an arrow key: the composition is
    // abandoned, its text kept.
    caret.marked = None;
    caret.anchor = None;
    caret.pos = floor_boundary(text, at.min(text.len()));
}

/// Extend the caret to a byte offset during a click-drag or shift-click
/// (#127): the press point becomes the anchor, the current point the
/// head, so the dragged range is the selection the clipboard chords
/// act on. Like [`place_caret`], abandons any composition in flight.
/// Collapses when the head meets the anchor, so dragging back to the
/// press point leaves a plain caret there.
pub(crate) fn extend_caret_to(text: &str, caret: &mut Caret, at: usize) {
    caret.clamp(text);
    caret.marked = None;
    caret.anchor.get_or_insert(caret.pos);
    caret.pos = floor_boundary(text, at.min(text.len()));
    if caret.anchor == Some(caret.pos) {
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

/// A byte offset as a UTF-16 offset, the unit gpui's text-input
/// protocol speaks. Floors to a char boundary and clamps into `text`.
pub(crate) fn offset_to_utf16(text: &str, at: usize) -> usize {
    let at = floor_boundary(text, at);
    text[..at].chars().map(|c| c.len_utf16()).sum()
}

/// A UTF-16 offset back to bytes. A unit landing inside a surrogate
/// pair floors to the character's start; past-the-end clamps to it.
pub(crate) fn offset_from_utf16(text: &str, mut units: usize) -> usize {
    for (i, c) in text.char_indices() {
        if units == 0 {
            return i;
        }
        let w = c.len_utf16();
        if units < w {
            return i;
        }
        units -= w;
    }
    text.len()
}

/// A byte range as a UTF-16 range, clamped and ordered.
pub(crate) fn range_to_utf16(text: &str, range: &Range<usize>) -> Range<usize> {
    let start = offset_to_utf16(text, range.start);
    let end = offset_to_utf16(text, range.end.max(range.start));
    start..end.max(start)
}

/// A UTF-16 range back to bytes, clamped and ordered.
pub(crate) fn range_from_utf16(text: &str, range: &Range<usize>) -> Range<usize> {
    let start = offset_from_utf16(text, range.start);
    let end = offset_from_utf16(text, range.end.max(range.start));
    start..end.max(start)
}

/// The selection (or the collapsed caret) in UTF-16 units, plus whether
/// the head sits at the range's start — the shape `UTF16Selection`
/// wants, computed purely so the input handler stays thin.
pub(crate) fn selection_utf16(text: &str, caret: &Caret) -> (Range<usize>, bool) {
    match caret.anchor {
        Some(a) if a != caret.pos => {
            let (lo, hi) = (a.min(caret.pos), a.max(caret.pos));
            (
                offset_to_utf16(text, lo)..offset_to_utf16(text, hi),
                a > caret.pos,
            )
        }
        _ => {
            let u = offset_to_utf16(text, caret.pos);
            (u..u, false)
        }
    }
}

/// One platform text-input edit, in byte offsets: the input handler
/// converts the protocol's UTF-16 ranges before calling.
#[derive(Debug, Clone)]
pub(crate) enum ImeEdit<'a> {
    /// Committed text (`insertText`): replaces `range`, or the marked
    /// range, the selection, or the caret when none is given.
    Commit {
        range: Option<Range<usize>>,
        text: &'a str,
    },
    /// Preedit text (`setMarkedText`): replaces like a commit, then
    /// marks the new text as composing. `selected` is the caret the
    /// platform wants inside it, in UTF-16 units *relative to the
    /// marked text's start* (Windows IMM cursor positions and macOS
    /// `selectedRange` both count this way); `None` collapses the
    /// caret to the marked text's end.
    Mark {
        range: Option<Range<usize>>,
        text: &'a str,
        selected: Option<Range<usize>>,
    },
    /// Drop the composing state (`unmarkText`); the text stays.
    Unmark,
}

/// Apply one platform text-input edit to a field.
pub(crate) fn apply_ime_edit(text: &mut String, caret: &mut Caret, edit: ImeEdit) {
    match edit {
        ImeEdit::Commit { range, text: new } => replace_text_in_range(text, caret, range, new),
        ImeEdit::Mark {
            range,
            text: new,
            selected,
        } => replace_and_mark_text_in_range(text, caret, range, new, selected),
        ImeEdit::Unmark => unmark_text(caret),
    }
}

/// The byte range a commit or mark replaces: the explicit range, else
/// the marked range, else the selection, else the caret. Clamped into
/// `text` on char boundaries.
fn ime_target_range(text: &str, caret: &Caret, range: Option<Range<usize>>) -> Range<usize> {
    let target = range
        .or_else(|| caret.marked.clone())
        .or_else(|| selection_range(caret))
        .unwrap_or(caret.pos..caret.pos);
    let start = floor_boundary(text, target.start.min(text.len()));
    let end = floor_boundary(text, target.end.min(text.len()).max(start));
    start..end
}

/// Replace a range with committed text: a commit over the marked range
/// (or the selection, or the caret). Committing clears the composing
/// state, and controls are stripped exactly like typed text.
pub(crate) fn replace_text_in_range(
    text: &mut String,
    caret: &mut Caret,
    range: Option<Range<usize>>,
    new_text: &str,
) {
    caret.clamp(text);
    let target = ime_target_range(text, caret, range);
    caret.anchor = Some(target.start);
    caret.pos = target.end;
    // `insert_text` deletes the held range, inserts sanitized, and
    // clears both the selection and the composing state.
    insert_text(text, caret, new_text);
}

/// Replace a range with composing (marked) text. An empty replacement
/// clears the composing state; otherwise the new text is marked and the
/// caret goes where `selected` says (see [`ImeEdit::Mark`]).
pub(crate) fn replace_and_mark_text_in_range(
    text: &mut String,
    caret: &mut Caret,
    range: Option<Range<usize>>,
    new_text: &str,
    selected: Option<Range<usize>>,
) {
    caret.clamp(text);
    let target = ime_target_range(text, caret, range);
    caret.anchor = Some(target.start);
    caret.pos = target.end;
    insert_text(text, caret, new_text);
    if caret.pos == target.start {
        caret.marked = None;
    } else {
        caret.marked = Some(target.start..caret.pos);
    }
    // The platform's caret counts UTF-16 units from the marked text's
    // start (gpui's own example maps it the same way, modulo an
    // upstream slip that anchors the range's end to the *old* text).
    if let Some(rel) = selected {
        let base = offset_to_utf16(text, target.start);
        let abs = base.saturating_add(rel.start)..base.saturating_add(rel.end.max(rel.start));
        let sel = range_from_utf16(text, &abs);
        caret.anchor = Some(sel.start);
        caret.pos = sel.end;
        if caret.anchor == Some(caret.pos) {
            caret.anchor = None;
        }
    }
}

/// Drop the composing state; the text stays as plain text.
pub(crate) fn unmark_text(caret: &mut Caret) {
    caret.marked = None;
}

/// The field as drawn: the text with a caret bar at the caret, or the
/// selection bracketed when one is held. A string like the trailing
/// `_` the append-only fields used to draw, so every prompt renders its
/// caret the same way without segmented text layout. The text is
/// display-sanitized first (#101), with the caret clamped into it.
///
/// Composing text (#126) draws bracketed in `⟨⟩`, distinct from the
/// selection's `⟦⟧`, with the caret bar at the caret even inside it; a
/// selection held mid-composition is not drawn separately.
pub(crate) fn render_with_caret(text: &str, caret: &Caret) -> String {
    let clean = timetrack_core::sanitize_display(text);
    let mut caret = caret.clone();
    caret.clamp(&clean);
    if let Some(marked) = caret.marked.clone() {
        return render_marked(&clean, &marked, caret.pos);
    }
    let (before, selected, after) = display_parts(&clean, &caret);
    if selected.is_empty() {
        format!("{before}▏{after}")
    } else {
        format!("{before}⟦{selected}⟧{after}")
    }
}

/// One span of the drawn field string (#127): a slice of the
/// (sanitized) field text, or a zero-width adornment — the caret bar,
/// the selection or composition brackets — sitting at a text offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RenderSpan {
    /// `clean[range]` drawn verbatim.
    Text(Range<usize>),
    /// An adornment glyph sitting at a text offset.
    Adorn { at: usize, glyph: &'static str },
}

/// The drawn field (#127): the sanitized text the spans index into,
/// the string as painted, and the spans in draw order — the same
/// segmentation [`render_with_caret`] draws, so a pixel-mapped offset
/// into the drawn string maps back to field text.
#[derive(Debug, Clone)]
pub(crate) struct RenderedField {
    pub(crate) clean: String,
    pub(crate) drawn: String,
    pub(crate) spans: Vec<RenderSpan>,
}

/// The drawn field as spans over its sanitized text. The segmentation
/// mirrors [`render_with_caret`] exactly (a test pins the two equal):
/// text spans partition the clean text, adornments carry no text.
pub(crate) fn render_spans(text: &str, caret: &Caret) -> RenderedField {
    let clean = timetrack_core::sanitize_display(text);
    let mut caret = caret.clone();
    caret.clamp(&clean);
    let mut spans = Vec::new();
    if let Some(marked) = caret.marked.clone() {
        let pos = caret.pos;
        let (ms, me) = (marked.start, marked.end);
        // The `render_marked` cases, as spans: before / composing /
        // after, subdivided where the caret bar sits.
        if pos < ms {
            spans.push(RenderSpan::Text(0..pos));
            spans.push(RenderSpan::Adorn {
                at: pos,
                glyph: "▏",
            });
            spans.push(RenderSpan::Text(pos..ms));
            spans.push(RenderSpan::Adorn {
                at: ms, glyph: "⟨"
            });
            spans.push(RenderSpan::Text(ms..me));
            spans.push(RenderSpan::Adorn {
                at: me, glyph: "⟩"
            });
            spans.push(RenderSpan::Text(me..clean.len()));
        } else if pos > me {
            spans.push(RenderSpan::Text(0..ms));
            spans.push(RenderSpan::Adorn {
                at: ms, glyph: "⟨"
            });
            spans.push(RenderSpan::Text(ms..me));
            spans.push(RenderSpan::Adorn {
                at: me, glyph: "⟩"
            });
            spans.push(RenderSpan::Text(me..pos));
            spans.push(RenderSpan::Adorn {
                at: pos,
                glyph: "▏",
            });
            spans.push(RenderSpan::Text(pos..clean.len()));
        } else {
            spans.push(RenderSpan::Text(0..ms));
            spans.push(RenderSpan::Adorn {
                at: ms, glyph: "⟨"
            });
            spans.push(RenderSpan::Text(ms..pos));
            spans.push(RenderSpan::Adorn {
                at: pos,
                glyph: "▏",
            });
            spans.push(RenderSpan::Text(pos..me));
            spans.push(RenderSpan::Adorn {
                at: me, glyph: "⟩"
            });
            spans.push(RenderSpan::Text(me..clean.len()));
        }
    } else if let Some(r) = selection_range(&caret) {
        spans.push(RenderSpan::Text(0..r.start));
        spans.push(RenderSpan::Adorn {
            at: r.start,
            glyph: "⟦",
        });
        spans.push(RenderSpan::Text(r.clone()));
        spans.push(RenderSpan::Adorn {
            at: r.end,
            glyph: "⟧",
        });
        spans.push(RenderSpan::Text(r.end..clean.len()));
    } else {
        let pos = floor_boundary(&clean, caret.pos);
        spans.push(RenderSpan::Text(0..pos));
        spans.push(RenderSpan::Adorn {
            at: pos,
            glyph: "▏",
        });
        spans.push(RenderSpan::Text(pos..clean.len()));
    }
    let mut drawn = String::new();
    for span in &spans {
        match span {
            RenderSpan::Text(range) => drawn.push_str(&clean[range.clone()]),
            RenderSpan::Adorn { glyph, .. } => drawn.push_str(glyph),
        }
    }
    RenderedField {
        clean,
        drawn,
        spans,
    }
}

/// A clean-text byte offset back to field-text bytes. Sanitizing maps
/// one char to one char, so the char index is preserved even where a
/// blanked control changed the byte width.
fn clean_offset_to_text(text: &str, clean: &str, at: usize) -> usize {
    if clean == text {
        return floor_boundary(text, at);
    }
    let at = floor_boundary(clean, at.min(clean.len()));
    let chars = clean[..at].chars().count();
    text.char_indices()
        .map(|(i, _)| i)
        .nth(chars)
        .unwrap_or(text.len())
}

/// A field-text byte offset forward to clean-text bytes, the mirror of
/// [`clean_offset_to_text`].
fn text_offset_to_clean(text: &str, clean: &str, at: usize) -> usize {
    if clean == text {
        return floor_boundary(text, at);
    }
    let at = floor_boundary(text, at.min(text.len()));
    let chars = text[..at].chars().count();
    clean
        .char_indices()
        .map(|(i, _)| i)
        .nth(chars)
        .unwrap_or(clean.len())
}

/// Map a byte offset in the drawn string — from pixel hit-testing —
/// back to the field text (#127). Adornments map to the offset they
/// sit at; past-the-end clamps to it.
pub(crate) fn displayed_to_text_offset(text: &str, caret: &Caret, at: usize) -> usize {
    let rendered = render_spans(text, caret);
    let at = floor_boundary(&rendered.drawn, at.min(rendered.drawn.len()));
    let mut drawn_at = 0;
    for span in &rendered.spans {
        let width = match span {
            RenderSpan::Text(range) => range.len(),
            RenderSpan::Adorn { glyph, .. } => glyph.len(),
        };
        if drawn_at <= at && at < drawn_at + width {
            let clean_at = match span {
                RenderSpan::Text(range) => range.start + (at - drawn_at),
                RenderSpan::Adorn { at: anchor, .. } => *anchor,
            };
            return clean_offset_to_text(text, &rendered.clean, clean_at);
        }
        drawn_at += width;
    }
    clean_offset_to_text(text, &rendered.clean, rendered.clean.len())
}

/// Map a field-text byte offset to the drawn string (#127): where the
/// platform's caret rect for a text offset starts. At a span edge the
/// earlier span wins, so a collapsed caret maps before its bar.
pub(crate) fn text_to_displayed_offset(text: &str, caret: &Caret, at: usize) -> usize {
    let rendered = render_spans(text, caret);
    let clean_at = text_offset_to_clean(text, &rendered.clean, at);
    let mut drawn_at = 0;
    for span in &rendered.spans {
        match span {
            RenderSpan::Text(range) => {
                if range.start <= clean_at && clean_at <= range.end {
                    return drawn_at + (clean_at - range.start);
                }
                drawn_at += range.len();
            }
            RenderSpan::Adorn { glyph, .. } => {
                drawn_at += glyph.len();
            }
        }
    }
    rendered.drawn.len()
}

/// The marked form of [`render_with_caret`]: `⟨⟩` around the composing
/// range, the caret bar at `pos`. All offsets are clamped into `text`
/// on char boundaries by the caller.
fn render_marked(text: &str, marked: &Range<usize>, pos: usize) -> String {
    let before = &text[..marked.start];
    let composing = &text[marked.clone()];
    let after = &text[marked.end..];
    // A caret exactly on a marked edge draws inside the brackets: it
    // belongs to the composition, which is also where an unpositioned
    // mark collapses it.
    if pos < marked.start {
        format!("{}▏{}⟨{composing}⟩{after}", &before[..pos], &before[pos..])
    } else if pos > marked.end {
        let rel = pos - marked.end;
        format!("{before}⟨{composing}⟩{}▏{}", &after[..rel], &after[rel..])
    } else {
        let rel = pos - marked.start;
        format!(
            "{before}⟨{}▏{}⟩{after}",
            &composing[..rel],
            &composing[rel..]
        )
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

    #[test]
    fn utf16_offsets_count_code_units_not_bytes() {
        // "aé日🎉b": 1 + 1 + 1 + 2 + 1 UTF-16 units over
        // 1 + 2 + 3 + 4 + 1 bytes.
        let text = "aé日🎉b";
        assert_eq!(offset_to_utf16(text, 0), 0);
        assert_eq!(offset_to_utf16(text, 1), 1);
        assert_eq!(offset_to_utf16(text, 3), 2);
        assert_eq!(offset_to_utf16(text, 6), 3);
        assert_eq!(offset_to_utf16(text, 10), 5);
        assert_eq!(offset_to_utf16(text, 11), 6);
        // Mid-character floors to the boundary.
        assert_eq!(offset_to_utf16(text, 2), 1);
        // And back, clamping past the end.
        assert_eq!(offset_from_utf16(text, 0), 0);
        assert_eq!(offset_from_utf16(text, 2), 3);
        assert_eq!(offset_from_utf16(text, 3), 6);
        assert_eq!(offset_from_utf16(text, 5), 10);
        assert_eq!(offset_from_utf16(text, 6), 11);
        assert_eq!(offset_from_utf16(text, 99), 11);
        // Inside a surrogate pair floors to the emoji's start.
        assert_eq!(offset_from_utf16(text, 4), 6);
        // Ranges round-trip both ways.
        assert_eq!(range_to_utf16(text, &(1..6)), 1..3);
        assert_eq!(range_from_utf16(text, &(1..3)), 1..6);
        assert_eq!(range_from_utf16(text, &(4..5)), 6..10);
    }

    #[test]
    fn marked_text_composes_then_commits() {
        // The dead-key shape: `^` arrives as preedit, then `ê` commits
        // over it — the Windows WM_DEADCHAR / commit sequence.
        let mut text = "caff".to_string();
        let mut caret = Caret::at_end(&text);
        replace_and_mark_text_in_range(&mut text, &mut caret, None, "^", None);
        assert_eq!(text, "caff^");
        assert_eq!(caret.marked, Some(4..5));
        assert_eq!(render_with_caret(&text, &caret), "caff⟨^▏⟩");
        replace_text_in_range(&mut text, &mut caret, None, "è");
        assert_eq!(text, "caffè");
        assert_eq!(caret.marked, None);
        assert_eq!(caret.pos, text.len());
        assert_eq!(render_with_caret(&text, &caret), "caffè▏");
    }

    #[test]
    fn marking_replaces_the_selection_and_tracks_the_caret() {
        // CJK shape: preedit replaces the selection, the platform puts
        // its caret inside the marked text (relative units), and the
        // commit replaces the whole marked range.
        let mut text = "ab".to_string();
        let mut caret = Caret::at_end(&text);
        select_all(&text, &mut caret);
        replace_and_mark_text_in_range(&mut text, &mut caret, None, "日本", Some(1..1));
        assert_eq!(text, "日本");
        assert_eq!(caret.marked, Some(0.."日本".len()));
        assert_eq!(caret.pos, "日".len());
        assert_eq!(render_with_caret(&text, &caret), "⟨日▏本⟩");
        // The caret survives a relative unit past the marked text: it
        // clamps into the field, never out of it.
        replace_and_mark_text_in_range(&mut text, &mut caret, None, "日本語", Some(9..9));
        assert_eq!(text, "日本語");
        assert_eq!(caret.pos, text.len());
        replace_text_in_range(&mut text, &mut caret, None, "日本語");
        assert_eq!(text, "日本語");
        assert_eq!(caret.marked, None);
    }

    #[test]
    fn unmarking_keeps_the_text_and_drops_the_state() {
        let mut text = String::new();
        let mut caret = Caret::default();
        replace_and_mark_text_in_range(&mut text, &mut caret, None, "ni", None);
        assert_eq!(caret.marked, Some(0..2));
        unmark_text(&mut caret);
        assert_eq!(text, "ni");
        assert_eq!(caret.marked, None);
        assert_eq!(render_with_caret(&text, &caret), "ni▏");
    }

    #[test]
    fn an_explicit_range_replaces_where_told() {
        // The platform may name the range (Linux DeleteText over the
        // marked range); it wins over caret and selection alike.
        let mut text = "hello".to_string();
        let mut caret = Caret::at_end(&text);
        replace_text_in_range(&mut text, &mut caret, Some(1..4), "i");
        assert_eq!(text, "hio");
        assert_eq!(caret.pos, 2);
        // Out-of-range endpoints clamp instead of panicking.
        replace_text_in_range(&mut text, &mut caret, Some(1..99), "");
        assert_eq!(text, "h");
    }

    #[test]
    fn committed_text_loses_its_controls_like_typed_text() {
        let mut text = String::new();
        let mut caret = Caret::default();
        // Enter forwarded by the platform ("enter" carries "\n") must
        // be a no-op, not a stored control.
        replace_text_in_range(&mut text, &mut caret, None, "\n");
        assert_eq!(text, "");
        assert_eq!(caret.marked, None);
        replace_text_in_range(&mut text, &mut caret, None, "a\tb");
        assert_eq!(text, "ab");
    }

    #[test]
    fn direct_edits_abandon_a_composition_without_losing_text() {
        let mut text = String::new();
        let mut caret = Caret::default();
        replace_and_mark_text_in_range(&mut text, &mut caret, None, "ka", None);
        move_left(&text, &mut caret, false);
        assert_eq!(caret.marked, None);
        assert_eq!(text, "ka");
        replace_and_mark_text_in_range(&mut text, &mut caret, None, "き", None);
        insert_text(&mut text, &mut caret, "x");
        assert_eq!(caret.marked, None);
        assert!(text.contains('x'));
    }

    #[test]
    fn selections_report_utf16_with_their_direction() {
        let text = "aé日b".to_string();
        let mut caret = Caret::at_end(&text);
        let (range, reversed) = selection_utf16(&text, &caret);
        assert_eq!((range, reversed), (4..4, false));
        move_home(&text, &mut caret, true);
        let (range, reversed) = selection_utf16(&text, &caret);
        assert_eq!((range, reversed), (0..4, true));
        move_end(&text, &mut caret, false);
        move_left(&text, &mut caret, true);
        move_left(&text, &mut caret, true);
        let (range, reversed) = selection_utf16(&text, &caret);
        assert_eq!(text[range_from_utf16(&text, &range)], *"日b");
        assert!(reversed, "the head (caret) sits at the range's start");
        // Collapse to the selection's start, then home to 0.
        move_home(&text, &mut caret, false);
        move_home(&text, &mut caret, false);
        move_right(&text, &mut caret, true);
        move_right(&text, &mut caret, true);
        let (range, reversed) = selection_utf16(&text, &caret);
        assert_eq!(text[range_from_utf16(&text, &range)], *"aé");
        assert!(!reversed, "the head (caret) sits at the range's end");
    }

    #[test]
    fn clamping_drops_a_marked_range_that_no_longer_spans() {
        let mut caret = Caret::at_end("hello");
        caret.marked = Some(1..4);
        caret.clamp("hi");
        assert_eq!(caret.marked, Some(1..2));
        caret.clamp("");
        assert_eq!(caret.marked, None);
    }

    #[test]
    fn clicking_places_the_caret_and_clears_the_selection() {
        // #127: a click collapses, clamps past the end, and floors
        // mid-character offsets to the boundary.
        let text = "aé日b";
        let mut caret = Caret::at_end(text);
        select_all(text, &mut caret);
        assert!(selection_range(&caret).is_some());
        place_caret(text, &mut caret, 1);
        assert_eq!(caret.pos, 1);
        assert_eq!(caret.anchor, None);
        // Past the end clamps; mid-日 (bytes 3..6) floors to its start.
        place_caret(text, &mut caret, 99);
        assert_eq!(caret.pos, text.len());
        place_caret(text, &mut caret, 4);
        assert_eq!(caret.pos, 3);
        // A click on empty text stays a caret at zero.
        let mut empty = Caret::default();
        place_caret("", &mut empty, 5);
        assert_eq!(empty.pos, 0);
    }

    #[test]
    fn dragging_extends_the_selection_from_the_press_point() {
        // #127: press, drag forward, drag back past the press, drag
        // home again — the press point stays the anchor throughout.
        let text = "Deep Work";
        let mut caret = Caret::at_end(text);
        place_caret(text, &mut caret, 2);
        extend_caret_to(text, &mut caret, 6);
        assert_eq!(selected_text(text, &caret), Some("ep W"));
        // Back past the press: the selection flips sides, anchored at 2.
        extend_caret_to(text, &mut caret, 0);
        assert_eq!(selected_text(text, &caret), Some("De"));
        // Home to the press point collapses to a plain caret there.
        extend_caret_to(text, &mut caret, 2);
        assert_eq!(caret.pos, 2);
        assert_eq!(selected_text(text, &caret), None);
        // ...from which the next drag re-anchors correctly.
        extend_caret_to(text, &mut caret, 5);
        assert_eq!(selected_text(text, &caret), Some("ep "));
    }

    #[test]
    fn shift_click_extends_the_held_selection() {
        // #127: extending with an anchor held keeps it — the mouse
        // shape of shift+arrows — over multi-byte text.
        let text = "aé日b";
        let mut caret = Caret::at_end(text);
        place_caret(text, &mut caret, 1);
        extend_caret_to(text, &mut caret, 6);
        assert_eq!(selected_text(text, &caret), Some("é日"));
        extend_caret_to(text, &mut caret, text.len());
        assert_eq!(selected_text(text, &caret), Some("é日b"));
    }

    #[test]
    fn mouse_moves_abandon_a_composition_without_losing_text() {
        // #127 composes with #126 like the arrow keys: the marked text
        // stays as plain text, untracked.
        let mut text = String::new();
        let mut caret = Caret::default();
        replace_and_mark_text_in_range(&mut text, &mut caret, None, "ka", None);
        place_caret(&text, &mut caret, 1);
        assert_eq!(caret.marked, None);
        assert_eq!(text, "ka");
        assert_eq!(caret.pos, 1);
        replace_and_mark_text_in_range(&mut text, &mut caret, None, "き", None);
        extend_caret_to(&text, &mut caret, 0);
        assert_eq!(caret.marked, None);
        assert!(text.contains("き"));
        assert!(selection_range(&caret).is_some());
    }

    #[test]
    fn spans_draw_exactly_what_the_caret_renderer_draws() {
        // #127's hit-testing maps through the drawn string, so the
        // spans must segment exactly what `render_with_caret` draws —
        // across collapsed, selected and composing states.
        let mut cases: Vec<(String, Caret)> = vec![
            ("".into(), Caret::default()),
            ("ab".into(), Caret::at_end("ab")),
            (
                "ab".into(),
                Caret {
                    pos: 1,
                    anchor: None,
                    marked: None,
                },
            ),
            ("aé日🎉b".into(), Caret::at_end("aé日🎉b")),
            (
                "aé日🎉b".into(),
                Caret {
                    pos: 3,
                    anchor: None,
                    marked: None,
                },
            ),
        ];
        let mut selected = Caret::at_end("Deep Work");
        selected.anchor = Some(2);
        selected.pos = 6;
        let mut reversed = Caret::at_end("Deep Work");
        reversed.anchor = Some(6);
        reversed.pos = 2;
        let mut marked_mid = Caret::at_end("日本語");
        marked_mid.marked = Some(0.."日本".len());
        marked_mid.pos = "日".len();
        let mut marked_edge = Caret::at_end(" affirm");
        marked_edge.marked = Some(1..4);
        marked_edge.pos = 1;
        let marked_outside = Caret {
            pos: 0,
            anchor: None,
            marked: Some(1..2),
        };
        cases.push(("Deep Work".into(), selected));
        cases.push(("Deep Work".into(), reversed));
        cases.push(("日本語".into(), marked_mid));
        cases.push((" affirm".into(), marked_edge));
        cases.push(("ab".into(), marked_outside));
        for (text, caret) in &cases {
            assert_eq!(
                render_spans(text, caret).drawn,
                render_with_caret(text, caret),
                "spans must draw the same string for {text:?} {caret:?}"
            );
        }
    }

    #[test]
    fn displayed_offsets_map_back_to_the_text_they_sit_on() {
        // #127: every text boundary round-trips through the drawn
        // string, and every adornment maps to its own anchor — over
        // multi-byte text, where byte widths differ on both sides.
        let text = "aé日b";
        for (caret, name) in [
            (Caret::at_end(text), "collapsed at end"),
            (
                Caret {
                    pos: 3,
                    anchor: None,
                    marked: None,
                },
                "collapsed mid-word",
            ),
            (
                Caret {
                    pos: 6,
                    anchor: Some(1),
                    marked: None,
                },
                "selection",
            ),
            (
                Caret {
                    pos: 3,
                    anchor: None,
                    marked: Some(1..6),
                },
                "composition",
            ),
        ] {
            // Every boundary round-trips both ways.
            for at in [0, 1, 3, 6, text.len()] {
                let drawn = text_to_displayed_offset(text, &caret, at);
                assert_eq!(
                    displayed_to_text_offset(text, &caret, drawn),
                    at,
                    "{name}: boundary {at} must round-trip"
                );
            }
            // Every adornment maps to its own anchor.
            let rendered = render_spans(text, &caret);
            let mut drawn_at = 0;
            for span in &rendered.spans {
                match span {
                    RenderSpan::Text(range) => drawn_at += range.len(),
                    RenderSpan::Adorn { at: anchor, glyph } => {
                        assert_eq!(
                            displayed_to_text_offset(text, &caret, drawn_at),
                            *anchor,
                            "{name}: {glyph} must map to {anchor}"
                        );
                        drawn_at += glyph.len();
                    }
                }
            }
            // Past-the-end clamps to the text end.
            assert_eq!(
                displayed_to_text_offset(text, &caret, rendered.drawn.len() + 9),
                text.len(),
                "{name}: past-the-end must clamp"
            );
        }
        // A collapsed caret maps before its bar, so the platform's
        // caret rect starts where the bar is drawn.
        let caret = Caret {
            pos: 1,
            anchor: None,
            marked: None,
        };
        let drawn = text_to_displayed_offset(text, &caret, 1);
        assert_eq!(&render_spans(text, &caret).drawn[drawn..], "▏é日b");
        // Sanitized controls keep their char index: the drawn blank
        // maps back to the control it covers.
        let dirty = "a\u{7}b";
        let caret = Caret::at_end(dirty);
        let rendered = render_spans(dirty, &caret);
        assert_eq!(rendered.drawn, "a b▏");
        assert_eq!(displayed_to_text_offset(dirty, &caret, 2), "a\u{7}".len());
        assert_eq!(text_to_displayed_offset(dirty, &caret, "a\u{7}b".len()), 3);
    }
}
