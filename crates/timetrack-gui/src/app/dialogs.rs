/* gui/app/dialogs.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The modal prompts: entry dialog, project prompt, split, merge, delete.
//!
//! Each prompt is plain state over strings, so the whole interaction is
//! unit-testable without a window. At most one is open at a time; the
//! parent's `which_prompt` is the one question the key handler and the
//! renderer both ask, so a prompt that takes the keyboard is always one
//! that is on screen.

pub mod delete;
pub mod delete_project;
pub mod entry;
pub mod merge;
pub mod project;
pub mod recolour;
pub mod split;
