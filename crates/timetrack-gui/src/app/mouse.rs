/* gui/app/mouse.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! Mouse caret placement and click-drag selection in text fields (#127).
//!
//! The model half is pure `textline` ops (`place_caret`,
//! `extend_caret_to`, the drawn-string mapping); this module is the
//! thin gpui adapter between a click position and those ops. It
//! answers #127's first design question with *measure at click time*:
//! each field's text origin is recorded during paint by [`FieldHit`],
//! and a press shapes the exact string on screen with the default text
//! style the field divs render with, taking the nearest character
//! boundary. No shaped run is kept between paints, and the render call
//! sites keep drawing plain strings.
//!
//! Click-to-focus (#127's second design question) reuses the keyboard
//! field model unchanged: pressing a field focuses it exactly like Tab
//! and then places the caret at the click, so a mouse user lands where
//! a keyboard user would have tabbed to, plus the click offset.

use gpui::{
    AnyElement, App, Bounds, Element, ElementId, Entity, GlobalElementId, InspectorElementId,
    IntoElement, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ParentElement, Pixels, Point, Style, Window, div, px, relative,
};

use super::TimetrackView;
use super::dialogs::entry::DialogField;
use super::dialogs::source::SourceField;
use super::export::picker::RangeEnd;
use super::textline::{self, Caret};

/// A text field the mouse can place the caret in (#127): the entry
/// dialog's three text rows, the two single-field prompts, the source
/// dialog's locator/label/user-name rows (#153), the export anchor day
/// and the two range days. The entry project row, the source
/// kind/project rows, the entries filter query (#131) and the
/// recolour hex prompt have no caret, so they are not fields here;
/// the source password row is masked, so it only takes focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum MouseField {
    Entry(DialogField),
    Project,
    Split,
    Source(SourceField),
    Anchor,
    Range(RangeEnd),
}

/// The field-text wrapper that records where the text painted (#127):
/// a pass-through like [`super::ime::ImeRoot`] whose bounds' left edge
/// is the text origin the click-time shaping measures from.
pub(crate) struct FieldHit {
    field: MouseField,
    view: Option<Entity<TimetrackView>>,
    child: AnyElement,
}

impl FieldHit {
    pub(crate) fn wrap(
        field: MouseField,
        view: Option<Entity<TimetrackView>>,
        text: String,
    ) -> Self {
        FieldHit {
            field,
            view,
            child: div().child(text).into_any_element(),
        }
    }
}

impl IntoElement for FieldHit {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for FieldHit {
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
        // Recorded without a notify, like `ImeRoot`'s flag: the bounds
        // are read by the next mouse event, never by the next paint.
        if let Some(view) = self.view.clone() {
            let field = self.field;
            view.update(cx, |view, _| view.record_field_bounds(field, bounds));
        }
        self.child.paint(window, cx);
    }
}

/// The drawn-string byte offset under a click (#127): shapes the exact
/// string on screen — the same default text style the field divs render
/// with, so the metrics match — and takes the nearest boundary.
/// Single-line only: anything past a newline is unreachable in a field.
pub(crate) fn hit_displayed_offset(
    window: &mut Window,
    text_left: Pixels,
    click_x: Pixels,
    displayed: &str,
) -> usize {
    let line = displayed.lines().next().unwrap_or("");
    if line.is_empty() {
        return 0;
    }
    let style = window.text_style();
    let font_size = style.font_size.to_pixels(window.rem_size());
    let run = style.to_run(line.len());
    let shaped = window
        .text_system()
        .shape_line(line.to_string().into(), font_size, &[run], None);
    shaped.closest_index_for_x(click_x - text_left)
}

/// The x of a drawn-string byte offset, for the platform's caret rect.
pub(crate) fn x_for_displayed_offset(
    window: &mut Window,
    displayed: &str,
    at: usize,
) -> Option<Pixels> {
    let line = displayed.lines().next().unwrap_or("");
    if line.is_empty() {
        return Some(px(0.));
    }
    let style = window.text_style();
    let font_size = style.font_size.to_pixels(window.rem_size());
    let run = style.to_run(line.len());
    let shaped = window
        .text_system()
        .shape_line(line.to_string().into(), font_size, &[run], None);
    Some(shaped.x_for_index(at.min(line.len())))
}

/// The label drawn ahead of a field's text in its own box (#127): the
/// export day fields read `day 2026-10-08`, and a hit index into the
/// shaped string counts from the `d`.
pub(crate) fn field_prefix(field: MouseField) -> &'static str {
    match field {
        MouseField::Anchor | MouseField::Range(_) => "day ",
        MouseField::Entry(_) | MouseField::Project | MouseField::Split | MouseField::Source(_) => {
            ""
        }
    }
}

impl TimetrackView {
    /// Remember where a field's text painted, for the next click.
    pub(crate) fn record_field_bounds(&mut self, field: MouseField, bounds: Bounds<Pixels>) {
        self.field_bounds.insert(field, bounds);
    }

    /// Whether a drag started in `field` is still held.
    fn is_dragging(&self, field: MouseField) -> bool {
        self.mouse_drag == Some(field)
    }

    /// Whether a mouse field shows its caret: unfocused rows draw
    /// plain text, so mapping shapes them that way too (see
    /// [`TimetrackView::mouse_hit_state`]).
    pub(crate) fn mouse_field_focused(&self, field: MouseField) -> bool {
        match field {
            MouseField::Entry(want) => self.dialog.as_ref().is_some_and(|dlg| dlg.field == want),
            MouseField::Project => self.project_prompt.is_some(),
            MouseField::Split => self.split_prompt.is_some(),
            MouseField::Source(want) => self
                .source_dialog
                .as_ref()
                .is_some_and(|dlg| dlg.field == want),
            MouseField::Anchor => self.export_picker.editing,
            MouseField::Range(end) => self.export_picker.range.editing == Some(end),
        }
    }

    /// The live text, caret and painted left edge behind a mouse field:
    /// what a click maps through. `None` where the field is not on
    /// screen (a dialog closed, the anchor inert, the range hidden) or
    /// has not painted yet — stale bounds never place a caret.
    ///
    /// The caret is the mapping caret, not always the stored one: an
    /// unfocused row draws no bar, so it maps with the caret parked at
    /// the end, where a bar shifts nothing. Mapping an unfocused row
    /// with the focused field's caret would draw a phantom bar
    /// mid-string and misplace every click past it by a character.
    pub(crate) fn mouse_hit_state(&self, field: MouseField) -> Option<(String, Caret, Pixels)> {
        let (text, caret) = self.mouse_live_text(field)?;
        let bounds = self.field_bounds.get(&field)?;
        let caret = if self.mouse_field_focused(field) {
            caret.clone()
        } else {
            Caret::at_end(text)
        };
        Some((text.to_string(), caret, bounds.origin.x))
    }

    /// Map a click x to field text through the last paint (#127), if
    /// the field is live: shape the exact string on screen and take
    /// the nearest boundary back to text. Shared by the mouse halves
    /// below and the platform's `character_index_for_point`.
    pub(crate) fn hit_text_offset(
        &self,
        window: &mut Window,
        field: MouseField,
        click_x: Pixels,
    ) -> Option<usize> {
        let (text, caret, left) = self.mouse_hit_state(field)?;
        let displayed = textline::render_with_caret(&text, &caret);
        let prefix = field_prefix(field);
        let full = format!("{prefix}{displayed}");
        let shown = hit_displayed_offset(window, left, click_x, &full).saturating_sub(prefix.len());
        Some(textline::displayed_to_text_offset(&text, &caret, shown))
    }

    /// The live text and caret behind a mouse field, if it is on screen.
    /// Mirrors the render gates: entry rows while the dialog is open,
    /// the prompts while open, source rows while the source dialog is
    /// open (#153), the anchor while active and showing, a range day
    /// while the range choice is showing (#133).
    pub(crate) fn mouse_live_text(&self, field: MouseField) -> Option<(&str, &Caret)> {
        match field {
            MouseField::Entry(want) => {
                let dlg = self.dialog.as_ref()?;
                match want {
                    DialogField::Description => Some((&dlg.description, &dlg.caret)),
                    DialogField::Project => None,
                    DialogField::Start => Some((&dlg.start, &dlg.caret)),
                    DialogField::End => Some((&dlg.end, &dlg.caret)),
                }
            }
            MouseField::Source(want) => {
                let dlg = self.source_dialog.as_ref()?;
                if !dlg.field_applies(want) {
                    return None;
                }
                match want {
                    SourceField::Kind
                    | SourceField::Project
                    | SourceField::Secret
                    | SourceField::RemoveCredential => None,
                    SourceField::Locator => Some((&dlg.locator, &dlg.caret)),
                    SourceField::Label => Some((&dlg.label, &dlg.caret)),
                    SourceField::Username => Some((&dlg.username, &dlg.caret)),
                }
            }
            MouseField::Project => {
                let prompt = self.project_prompt.as_ref()?;
                Some((&prompt.name, &prompt.caret))
            }
            MouseField::Split => {
                let prompt = self.split_prompt.as_ref()?;
                Some((&prompt.at, &prompt.caret))
            }
            MouseField::Anchor => {
                if !self.export_picker.is_active() || self.export_picker.is_range() {
                    return None;
                }
                Some((&self.export_picker.anchor_text, &self.export_picker.caret))
            }
            MouseField::Range(end) => {
                if !self.export_picker.is_range() {
                    return None;
                }
                match end {
                    RangeEnd::From => Some((
                        &self.export_picker.range.from_text,
                        &self.export_picker.range.from_caret,
                    )),
                    RangeEnd::To => Some((
                        &self.export_picker.range.to_text,
                        &self.export_picker.range.to_caret,
                    )),
                }
            }
        }
    }

    /// The live mouse field containing a window point, if any — in the
    /// same routing priority the keyboard uses, so overlapping paints
    /// resolve like focus does.
    pub(crate) fn mouse_field_at(&self, point: Point<Pixels>) -> Option<MouseField> {
        let mut fields = Vec::new();
        if self.dialog.is_some() {
            fields.extend(
                [
                    DialogField::Description,
                    DialogField::Start,
                    DialogField::End,
                ]
                .map(MouseField::Entry),
            );
        }
        if self.project_prompt.is_some() {
            fields.push(MouseField::Project);
        }
        if self.split_prompt.is_some() {
            fields.push(MouseField::Split);
        }
        if self.source_dialog.is_some() {
            fields.extend(
                [
                    SourceField::Locator,
                    SourceField::Label,
                    SourceField::Username,
                ]
                .map(MouseField::Source),
            );
        }
        if self.export_picker.is_active() && !self.export_picker.is_range() {
            fields.push(MouseField::Anchor);
        }
        if self.export_picker.is_range() {
            fields.push(MouseField::Range(RangeEnd::From));
            fields.push(MouseField::Range(RangeEnd::To));
        }
        fields.into_iter().find(|field| {
            self.field_bounds
                .get(field)
                .is_some_and(|bounds| bounds.contains(&point))
        })
    }

    /// The focused field as a mouse field, if it is one: the entry
    /// dialog's focused text row, the open prompt, the source dialog's
    /// focused text row (#153), the anchor while editing, the open
    /// range day. The mirror of `ime_target` for the platform's caret
    /// rect.
    pub(crate) fn mouse_focused_field(&self) -> Option<MouseField> {
        if let Some(dlg) = self.dialog.as_ref() {
            return match dlg.field {
                DialogField::Project => None,
                field => Some(MouseField::Entry(field)),
            };
        }
        if self.project_prompt.is_some() {
            return Some(MouseField::Project);
        }
        if self.split_prompt.is_some() {
            return Some(MouseField::Split);
        }
        if let Some(dlg) = self.source_dialog.as_ref() {
            return match dlg.field {
                SourceField::Kind
                | SourceField::Project
                | SourceField::Secret
                | SourceField::RemoveCredential => None,
                field => Some(MouseField::Source(field)),
            };
        }
        if self.export_picker.editing {
            return Some(MouseField::Anchor);
        }
        self.export_picker.range.editing.map(MouseField::Range)
    }

    /// Press the mouse at a text offset in a field (#127): focuses the
    /// field like Tab, then places the caret — or extends to it with
    /// shift held in the already-focused field. Starts the drag the
    /// matching release ends.
    pub(crate) fn mouse_down(&mut self, field: MouseField, at: usize, shift: bool) {
        match field {
            MouseField::Entry(want) => {
                let Some(dlg) = self.dialog.as_mut() else {
                    return;
                };
                dlg.mouse_down(want, at, shift);
            }
            MouseField::Project => {
                let Some(prompt) = self.project_prompt.as_mut() else {
                    return;
                };
                prompt.mouse_down(at, shift);
            }
            MouseField::Split => {
                let Some(prompt) = self.split_prompt.as_mut() else {
                    return;
                };
                prompt.mouse_down(at, shift);
            }
            MouseField::Source(want) => {
                let Some(dlg) = self.source_dialog.as_mut() else {
                    return;
                };
                dlg.mouse_down(want, at, shift);
            }
            MouseField::Anchor => {
                self.export_picker.mouse_down_anchor(at, shift);
            }
            MouseField::Range(end) => {
                if !self.export_picker.is_range() {
                    return;
                }
                self.export_picker.range.mouse_down(end, at, shift);
            }
        }
        self.mouse_drag = Some(field);
    }

    /// Drag the mouse to a text offset (#127): extends the press
    /// field's selection. Moves over any other row are ignored, so a
    /// drag that strays keeps selecting in the field it started in.
    pub(crate) fn mouse_drag_to(&mut self, field: MouseField, at: usize) {
        if !self.is_dragging(field) {
            return;
        }
        match field {
            MouseField::Entry(_) => {
                if let Some(dlg) = self.dialog.as_mut() {
                    dlg.mouse_drag(at);
                }
            }
            MouseField::Project => {
                if let Some(prompt) = self.project_prompt.as_mut() {
                    prompt.mouse_drag(at);
                }
            }
            MouseField::Split => {
                if let Some(prompt) = self.split_prompt.as_mut() {
                    prompt.mouse_drag(at);
                }
            }
            MouseField::Source(_) => {
                if let Some(dlg) = self.source_dialog.as_mut() {
                    dlg.mouse_drag(at);
                }
            }
            MouseField::Anchor => {
                self.export_picker.mouse_drag_anchor(at);
            }
            MouseField::Range(_) => {
                self.export_picker.range.mouse_drag(at);
            }
        }
    }

    /// Release the mouse button: ends any drag. Reports whether one was
    /// held, so stray releases paint no frame.
    pub(crate) fn mouse_up(&mut self) -> bool {
        self.mouse_drag.take().is_some()
    }
}

/// Map a press x to field text through the last paint, if the field is
/// live. `None` leaves the event unhandled, like a click off-field.
fn press_offset(
    field: MouseField,
    click_x: Pixels,
    window: &mut Window,
    view: &Entity<TimetrackView>,
    cx: &mut App,
) -> Option<usize> {
    view.read(cx).hit_text_offset(window, field, click_x)
}

/// The press half of a field's mouse handling (#127): map the click
/// and route it to the view. One function for every render call site,
/// so the adapter stays three lines per field.
pub(crate) fn on_press(
    field: MouseField,
    ev: &MouseDownEvent,
    window: &mut Window,
    view: &Entity<TimetrackView>,
    cx: &mut App,
) {
    let Some(at) = press_offset(field, ev.position.x, window, view, cx) else {
        return;
    };
    let shift = ev.modifiers.shift;
    view.update(cx, |view, cx| {
        view.mouse_down(field, at, shift);
        cx.notify();
    });
}

/// The drag half: extend the press field's selection while held.
pub(crate) fn on_drag(
    field: MouseField,
    ev: &MouseMoveEvent,
    window: &mut Window,
    view: &Entity<TimetrackView>,
    cx: &mut App,
) {
    if !view.read(cx).is_dragging(field) {
        return;
    }
    let Some(at) = press_offset(field, ev.position.x, window, view, cx) else {
        return;
    };
    view.update(cx, |view, cx| {
        view.mouse_drag_to(field, at);
        cx.notify();
    });
}

/// The release half: end any drag.
pub(crate) fn on_release(view: &Entity<TimetrackView>, cx: &mut App) {
    view.update(cx, |view, cx| {
        if view.mouse_up() {
            cx.notify();
        }
    });
}

/// Attach the three mouse halves to a field's row: press places,
/// motion while held drags, release (in or out) ends the drag.
/// Generic over the element so rows with an id (`Stateful<Div>`) and
/// plain boxes (`Div`) share the one adapter.
pub(crate) fn field_mouse<D: gpui::InteractiveElement>(
    el: D,
    field: MouseField,
    view: Entity<TimetrackView>,
) -> D {
    let press_view = view.clone();
    let drag_view = view.clone();
    el.on_mouse_down(MouseButton::Left, move |ev, window, cx| {
        on_press(field, ev, window, &press_view, cx);
    })
    .on_mouse_move(move |ev, window, cx| {
        on_drag(field, ev, window, &drag_view, cx);
    })
    .on_mouse_up(MouseButton::Left, {
        let view = view.clone();
        move |_ev: &MouseUpEvent, _window, cx| on_release(&view, cx)
    })
    .on_mouse_up_out(MouseButton::Left, move |_ev, _window, cx| {
        on_release(&view, cx)
    })
}
