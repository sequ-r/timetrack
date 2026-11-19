/* gui/app/ime.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The view's text-input handler: IME and dead-key composition (#126).
//!
//! gpui delivers composed text through [`gpui::EntityInputHandler`]
//! (marked ranges, commits, surrounding text), not through key events:
//! dead-key sequences and CJK candidates never arrive as printable key
//! presses on backends with a composing input method. One handler on
//! the view dispatches to whichever textline field is focused — the
//! same routing priority as `focused_text_mut` — because keyboard
//! routing here is a single root `on_key_down`, and per-dialog elements
//! would each need paint-phase registration for fields that move.
//!
//! The handler is registered only while a textline field is focused
//! (see [`ImeRoot`]), and only then does the platform route text
//! through it. That routing covers *plain* typing too: on Linux the
//! platform forwards every shift-only key press's `key_char` to the
//! handler after dispatch, on macOS it goes through the input context
//! when the key propagates, and on Windows `WM_CHAR` always follows the
//! key press. So while the handler is live the key handlers must not
//! insert text themselves — `ime_live` tells them when to stand down —
//! and they must *not* stop propagation, or Linux and macOS would stop
//! delivering. The two paths are mutually exclusive by construction:
//! the platform forwards exactly when a handler was registered, which
//! is exactly when `ime_live` is set, both decided in the same paint.
//!
//! What the handler changes about shortcuts: nothing. Control, alt and
//! platform-modified presses never reach it — Linux only forwards
//! shift-only presses, and the key handlers keep their `printable_char`
//! gate for the frames (and fields) without a handler. The filter query
//! (#131) and the recolour hex prompt keep their key-event typing: they
//! have no caret, so composition has nowhere to anchor.
//!
//! The protocol speaks UTF-16 offsets; the pure conversions and the
//! marked-text ops live in `super::textline`, unit-tested without a
//! window. This module only converts and routes.

use std::ops::Range;

use gpui::{
    AnyElement, App, Bounds, Context, Element, ElementId, ElementInputHandler, Entity,
    EntityInputHandler, FocusHandle, GlobalElementId, InspectorElementId, IntoElement, LayoutId,
    Pixels, Style, UTF16Selection, Window, point, relative,
};

use super::TimetrackView;
use super::dialogs::entry::EntryDialog;
use super::dialogs::project::ProjectPrompt;
use super::dialogs::source::SourceDialog;
use super::dialogs::split::SplitPrompt;
use super::dialogs::sync::SyncDialog;
use super::export::picker::{ExportPicker, ExportRange, RangeEnd};
use super::textline::{self, Caret, ImeEdit};

/// The dialog that owns the focused textline field, if one does.
enum ImeTarget<'a> {
    Entry(&'a mut EntryDialog),
    Project(&'a mut ProjectPrompt),
    Split(&'a mut SplitPrompt),
    Source(&'a mut SourceDialog),
    Sync(&'a mut SyncDialog),
    Anchor(&'a mut ExportPicker),
    Range(&'a mut ExportRange),
}

impl ImeTarget<'_> {
    /// The focused field's text and caret. `None` only on the fields
    /// that take no text: the entry dialog's project row and the
    /// source dialog's cycling rows.
    fn text_caret(&self) -> Option<(&str, &Caret)> {
        match self {
            ImeTarget::Entry(dlg) => dlg.focused_text(),
            ImeTarget::Project(prompt) => Some((&prompt.name, &prompt.caret)),
            ImeTarget::Split(prompt) => Some((&prompt.at, &prompt.caret)),
            ImeTarget::Source(dlg) => dlg.focused_text(),
            ImeTarget::Sync(dlg) => dlg.focused_text(),
            ImeTarget::Anchor(picker) => Some((&picker.anchor_text, &picker.caret)),
            ImeTarget::Range(range) => match range.editing {
                Some(RangeEnd::From) => Some((&range.from_text, &range.from_caret)),
                Some(RangeEnd::To) => Some((&range.to_text, &range.to_caret)),
                None => None,
            },
        }
    }

    /// Apply a platform edit to the focused field. False where
    /// [`text_caret`](ImeTarget::text_caret) is `None`.
    fn apply(self, edit: ImeEdit) -> bool {
        match self {
            ImeTarget::Entry(dlg) => dlg.ime_edit(edit),
            ImeTarget::Project(prompt) => {
                prompt.ime_edit(edit);
                true
            }
            ImeTarget::Split(prompt) => {
                prompt.ime_edit(edit);
                true
            }
            ImeTarget::Source(dlg) => dlg.ime_edit(edit),
            ImeTarget::Sync(dlg) => dlg.ime_edit(edit),
            ImeTarget::Anchor(picker) => picker.ime_edit(edit),
            ImeTarget::Range(range) => range.ime_edit(edit),
        }
    }
}

impl TimetrackView {
    /// The dialog owning the focused textline field. Mirrors the
    /// routing priority of `focused_text_mut`: the entry dialog, then
    /// the project, split and source prompts, then the sync dialog,
    /// then the export anchor, then the open range day.
    fn ime_target(&mut self) -> Option<ImeTarget<'_>> {
        if let Some(dlg) = self.dialog.as_mut() {
            return Some(ImeTarget::Entry(dlg));
        }
        if let Some(prompt) = self.project_prompt.as_mut() {
            return Some(ImeTarget::Project(prompt));
        }
        if let Some(prompt) = self.split_prompt.as_mut() {
            return Some(ImeTarget::Split(prompt));
        }
        if let Some(dlg) = self.source_dialog.as_mut() {
            return Some(ImeTarget::Source(dlg));
        }
        if let Some(dlg) = self.sync_dialog.as_mut() {
            return Some(ImeTarget::Sync(dlg));
        }
        if self.export_picker.editing {
            return Some(ImeTarget::Anchor(&mut self.export_picker));
        }
        // The range's open day field, if either (#133).
        if self.export_picker.range.editing.is_some() {
            return Some(ImeTarget::Range(&mut self.export_picker.range));
        }
        None
    }

    /// The focused field's text and caret, read-only. The `&self`
    /// mirror of [`ime_target`](TimetrackView::ime_target) for the one
    /// protocol method that takes `&self` — and the paint gate below.
    fn ime_focused_ref(&self) -> Option<(&str, &Caret)> {
        if let Some(dlg) = self.dialog.as_ref() {
            return dlg.focused_text();
        }
        if let Some(prompt) = self.project_prompt.as_ref() {
            return Some((&prompt.name, &prompt.caret));
        }
        if let Some(prompt) = self.split_prompt.as_ref() {
            return Some((&prompt.at, &prompt.caret));
        }
        if let Some(dlg) = self.source_dialog.as_ref() {
            return dlg.focused_text();
        }
        if let Some(dlg) = self.sync_dialog.as_ref() {
            return dlg.focused_text();
        }
        if self.export_picker.editing {
            return Some((&self.export_picker.anchor_text, &self.export_picker.caret));
        }
        match self.export_picker.range.editing {
            Some(RangeEnd::From) => Some((
                &self.export_picker.range.from_text,
                &self.export_picker.range.from_caret,
            )),
            Some(RangeEnd::To) => Some((
                &self.export_picker.range.to_text,
                &self.export_picker.range.to_caret,
            )),
            None => None,
        }
    }

    /// Whether a textline field is focused: the paint gate for
    /// registering the input handler.
    fn ime_active(&self) -> bool {
        self.ime_focused_ref().is_some()
    }

    /// The focused field's row clipped to a UTF-16 range (#127): the
    /// candidate window's anchor. `None` where the field's paint is
    /// unknown, leaving the whole-root fallback to the caller.
    fn caret_rect_for_range(
        &self,
        window: &mut Window,
        range_utf16: &Range<usize>,
    ) -> Option<Bounds<Pixels>> {
        let field = self.mouse_focused_field()?;
        let bounds = *self.field_bounds.get(&field)?;
        let (text, caret) = self.mouse_live_text(field)?;
        let displayed = textline::render_with_caret(text, caret);
        let bytes = textline::range_from_utf16(text, range_utf16);
        let prefix = super::mouse::field_prefix(field);
        let full = format!("{prefix}{displayed}");
        let start = prefix.len() + textline::text_to_displayed_offset(text, caret, bytes.start);
        let end = prefix.len() + textline::text_to_displayed_offset(text, caret, bytes.end);
        let x0 = super::mouse::x_for_displayed_offset(window, &full, start)?;
        let x1 = super::mouse::x_for_displayed_offset(window, &full, end)?;
        Some(Bounds::from_corners(
            point(bounds.left() + x0, bounds.top()),
            point(bounds.left() + x1, bounds.bottom()),
        ))
    }
}

impl EntityInputHandler for TimetrackView {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let target = self.ime_target()?;
        let (text, _) = target.text_caret()?;
        let bytes = textline::range_from_utf16(text, &range_utf16);
        *adjusted_range = Some(textline::range_to_utf16(text, &bytes));
        Some(text[bytes].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        // No field here is ever disabled, so the flag is ignored.
        let target = self.ime_target()?;
        let (text, caret) = target.text_caret()?;
        let (range, reversed) = textline::selection_utf16(text, caret);
        Some(UTF16Selection { range, reversed })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        let (text, caret) = self.ime_focused_ref()?;
        let marked = caret.marked.clone()?;
        Some(textline::range_to_utf16(text, &marked))
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.ime_target().is_some_and(|t| t.apply(ImeEdit::Unmark)) {
            cx.notify();
        }
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self.ime_target() else {
            return;
        };
        let Some((current, _)) = target.text_caret() else {
            return;
        };
        let range = range_utf16.map(|r| textline::range_from_utf16(current, &r));
        if target.apply(ImeEdit::Commit { range, text }) {
            cx.notify();
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self.ime_target() else {
            return;
        };
        let Some((current, _)) = target.text_caret() else {
            return;
        };
        let range = range_utf16.map(|r| textline::range_from_utf16(current, &r));
        if target.apply(ImeEdit::Mark {
            range,
            text: new_text,
            selected: new_selected_range,
        }) {
            cx.notify();
        }
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        // The caret-anchored rect when the focused field's paint is
        // known (#127): the candidate window sits on the text. Else
        // the whole root, as before — somewhere sane in the window.
        Some(
            self.caret_rect_for_range(window, &range_utf16)
                .unwrap_or(element_bounds),
        )
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<Pixels>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        // Platform-driven placement (#127): the live field under the
        // point, mapped like a press. None off-field, as before.
        let field = self.mouse_field_at(point)?;
        let at = self.hit_text_offset(window, field, point.x)?;
        let (text, _) = self.mouse_live_text(field)?;
        Some(textline::offset_to_utf16(text, at))
    }
}

/// The root element with the input handler attached: a pass-through
/// wrapper that registers [`ElementInputHandler`] during paint — the
/// only phase `handle_input` allows — exactly while a textline field
/// is focused, and records that in `ime_live` so the key handlers know
/// the platform is delivering text.
pub(crate) struct ImeRoot {
    child: AnyElement,
    view: Entity<TimetrackView>,
    focus: FocusHandle,
}

impl ImeRoot {
    pub(crate) fn wrap(child: AnyElement, view: Entity<TimetrackView>, focus: FocusHandle) -> Self {
        ImeRoot { child, view, focus }
    }
}

impl IntoElement for ImeRoot {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for ImeRoot {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let child_id = self.child.request_layout(window, cx);
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [child_id], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        // Decided and recorded in the same paint: the platform forwards
        // text exactly when a handler was registered, so the key
        // handlers' `ime_live` check can never double-insert or drop.
        let live = self.view.update(cx, |view, _| {
            let active = view.ime_active();
            view.ime_live = active;
            active
        });
        if live {
            window.handle_input(
                &self.focus,
                ElementInputHandler::new(bounds, self.view.clone()),
                cx,
            );
        }
        self.child.paint(window, cx);
    }
}
