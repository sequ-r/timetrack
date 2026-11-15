# GUI text input: spike findings (#109) and composition (#126)

What the GUI's text fields can and cannot do, where each claim is pinned,
and what is deliberately left for follow-up. Maintained alongside
`crates/timetrack-gui/src/app/textline.rs`, which is the one caret and
selection editor every free-text field now shares, and
`crates/timetrack-gui/src/app/ime.rs`, the view's text-input handler.

## Shipped in this spike

| Capability | Where | Evidence |
|---|---|---|
| Caret movement (←/→/Home/End), insert mid-word, forward delete | entry dialog, project prompt, split prompt, export anchor | `textline` unit tests; entry-dialog mid-word test |
| Selection (Shift+arrows/Home/End, replace on type, delete whole) | same fields | `textline` unit tests |
| Clipboard: Ctrl+C copy, Ctrl+X cut, Ctrl+V paste, Ctrl+A select all | same fields, via the view's root key handler | `textline` insert/selection tests; the chord wiring itself needs a live window to observe (see below) |
| Composed and multi-byte characters the platform delivers as text (é, CJK, emoji) | same fields | `textline` Unicode tests (caret moves by character, never by byte) |
| Rendering of valid stored Unicode | field rows via `render_with_caret` after display sanitization | rendering test ("日本語 🎉" round-trips); sanitization only ever blanks control characters (#101), so valid names render as themselves |

Deliberate limits of what shipped: pasted text loses control characters
(newlines included — single-line fields have nowhere for them, and the
rules refuse stored controls anyway), fields have no mouse caret
placement (landed later as #127), and the recolour hex
prompt keeps append/backspace because its charset is six hex digits.

## Shipped in #126: IME and dead-key composition

One input handler on the view dispatches to the focused field — the
first design question's answer. Keyboard routing here is a single root
`on_key_down`, so per-dialog elements would each need paint-phase
registration for fields that move between dialogs; instead `ime.rs`
registers one `ElementInputHandler` during paint, only while a
textline field is focused, and routes every protocol call through the
same priority as `focused_text_mut` (entry dialog, project prompt,
split prompt, export anchor, open range day).

Marked (preedit) ranges live on the caret (`Caret.marked`, byte
offsets) and render bracketed in `⟨⟩`, distinct from the selection's
`⟦⟧`, with the caret bar at the caret even inside the composition —
the second design question's answer. All composition state changes are
pure `textline` ops (`replace_text_in_range`,
`replace_and_mark_text_in_range`, `unmark_text`, UTF-16 conversions),
unit-tested without a window; the handler only converts units and
routes. The platform's caret-inside-preedit counts UTF-16 units
relative to the marked text's start (Windows IMM cursor positions and
macOS `selectedRange` both count this way; gpui's own example maps it
likewise, modulo an upstream slip anchoring the range's end to the old
text). Any direct key-driven edit abandons a composition in flight —
the text stays as plain text — rather than tracking it.

Text reaches fields through the handler while it is live — plain
typing included. On Linux the platform forwards every shift-only key
press's `key_char` to the handler after dispatch, on macOS through
the input context when the key propagates, and on Windows `WM_CHAR`
always follows the key press; stopping propagation in the key handlers
would silence Linux and macOS but not Windows, and inserting in both
places would double every character on Windows. So the key handlers
stand down from inserting while `ime_live` is set (decided and
recorded in the same paint that registers the handler, so the paths
are mutually exclusive by construction) and never stop propagation.
Ctrl/AltGr chords are unaffected: modified presses never reach the
handler, and the `printable_char` gate is unchanged for the frames and
fields without one.

Out of scope, deliberately: the entries filter query (#131) and the
recolour hex prompt have no caret, so composition has nowhere to
anchor — they keep key-event typing, and no handler is registered
while they own the keyboard. Candidate-window anchoring returned the
whole root bounds here; #127 anchors it to the focused field's row
(whole-root fallback when the paint is unknown).

## Shipped in #127: mouse caret placement and click-drag selection

Pressing a field focuses it exactly like Tab and places the caret at
the click; dragging extends the selection the clipboard chords act on;
shift-press extends in the already-focused field. That answers the
second design question: click-to-focus reuses the keyboard field model
unchanged, plus the click offset.

The first design question's answer is *measure at click time*. Each
field's text origin is recorded during paint by a pass-through wrapper
(`mouse::FieldHit`), and a press shapes the exact string on screen with
the default text style the field divs render with, taking the nearest
character boundary (`LineLayout::closest_index_for_x`). No shaped run
is kept between paints. The shaped string is the drawn one — caret bar,
selection and composition brackets included — so a pure mapping
(`textline::displayed_to_text_offset`, pinned against
`render_with_caret` by test) walks it back to field text; unfocused
rows draw no bar and map with the caret parked at the end, where a bar
shifts nothing. The same path serves the platform's
`character_index_for_point`, and `bounds_for_range` anchors the
candidate window to the focused field's row (whole-root fallback when
the paint is unknown).

A press or drag abandons a composition in flight like any caret move
(#126) — the marked text stays as plain text. Out of scope,
deliberately: the entries filter query (#131) and the recolour hex
prompt have no caret, so a press has nowhere to place; double-click
word selection and triple-click line selection are not implemented.

## Not shipped, and why — the follow-ups

- **Double-click word selection, triple-click line selection.** The
  press/drag model places by character only; word and line units need
  a word-boundary rule over the field text first.

## What still needs a live desktop to confirm

Unit tests cannot observe the platform half. Each of these was reasoned
from gpui 0.2.2's API but wants one manual pass on a real GNOME Wayland
and X11 session after release: Ctrl+V pasting non-ASCII text, the
clipboard surviving our window losing focus, and dead-key behaviour.
The failure mode if any of them differs is small: clipboard chords do
nothing visible, and typing still works.

For #126 specifically, the live matrix is: GNOME Wayland + X11, at
least one dead-key layout (e.g. `caffè` via `^` + `e`) and one CJK
IME, in every field the handler covers (entry description, project
name, split moment, export anchor, both range days). What the unit
tests pin instead: marked/composed/replacement paths over ASCII,
multi-byte and surrogate-pair text, the relative-caret mapping, and
the error-clearing each dialog keeps when composition lands.

For #127 specifically, the live matrix is: in every mouse field (the
three entry rows, project name, split moment, export anchor, both
range days), press places the caret at the clicked character, drag
selects to the pointer, shift-press extends, and the selection copies
and cuts; repeat over multi-byte text (`caffè`, CJK, emoji), where a
metrics mismatch shows as an off-by-one placement. Pressing mid-preedit
abandons the composition and keeps its text. What the unit tests pin
instead: place/drag/shift/collapse over ASCII and multi-byte text,
the drawn-string mapping both ways against the renderer, and the
dialog routing (focus-then-place, open-then-place, inert fields
ignoring the press).
