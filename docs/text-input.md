# GUI text input: spike findings (#109)

What the GUI's text fields can and cannot do, where each claim is pinned,
and what is deliberately left for follow-up. Maintained alongside
`crates/timetrack-gui/src/app/textline.rs`, which is the one caret and
selection editor every free-text field now shares.

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
placement (the whole GUI is keyboard-driven), and the recolour hex
prompt keeps append/backspace because its charset is six hex digits.

## Not shipped, and why — the follow-ups

- **IME and dead-key composition.** Typing `^` then `e`, or composing
  CJK candidates, needs gpui's text-input protocol
  (`EntityInputHandler`: marked ranges, candidate windows, surrounding
  text). Today such sequences only land when the platform composes them
  into a plain key event before gpui sees them, which varies by
  backend. This is real windowing work, scoped as its own ticket
  (#126) rather than bolted onto `textline.rs`.
- **Mouse caret placement and click-drag selection.** Needs
  pixel-to-byte mapping against gpui's shaped text runs. Filed as #127; keyboard selection covers the editing
  need meanwhile.

## What still needs a live desktop to confirm

Unit tests cannot observe the platform half. Each of these was reasoned
from gpui 0.2.2's API but wants one manual pass on a real GNOME Wayland
and X11 session after release: Ctrl+V pasting non-ASCII text, the
clipboard surviving our window losing focus, and dead-key behaviour
(the follow-up ticket's starting matrix). The failure mode if any of
them differs is small: clipboard chords do nothing visible, and typing
still works.
