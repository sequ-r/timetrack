/* cli/lib.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! Library half of the TimeTrack CLI.
//!
//! The TUI lives here rather than in `main.rs` so its unit tests are actually
//! compiled and run: a binary-only crate's `#[cfg(test)]` modules inside
//! sibling files are never built by `cargo test`.

pub mod tui;
