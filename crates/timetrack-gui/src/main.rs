/* gui/main.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The TimeTrack desktop client, built on gpui-ce.
//!
//! The entry point is `gpui_platform::application()`, NOT
//! `Application::new()` — the latter no longer exists in gpui-ce. The platform
//! crate supplies the real window backend; the gpui crate alone ships only a
//! test platform, so a crates.io-only dependency cannot open a window at all.
//!
//! The bus connection deliberately lives on a dedicated thread with its own
//! tokio runtime (see `app.rs`); nothing bus-related is driven from a gpui
//! future, because gpui's executor threads have no tokio context and zbus'
//! `spawn_blocking` panics there.

mod app;

use app::TimetrackView;
use gpui::{App, AppContext, Bounds, WindowBounds, WindowOptions, px, size};

fn main() {
    gpui_platform::application().run(|cx: &mut App| {
        // Tall enough for the 72px week total plus the quick-add row, the
        // per-project list and a few recent entries, without scrolling to
        // reach the buttons.
        let bounds = Bounds::centered(None, size(px(760.), px(720.)), cx);
        let _ = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(TimetrackView::new),
        );
    });
}
