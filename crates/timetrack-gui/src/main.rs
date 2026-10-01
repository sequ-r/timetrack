/* gui/main.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The TimeTrack desktop client, built on gpui.
//!
//! The entry point is `Application::new()`, which picks the platform for the
//! compositor it finds. That is only true of upstream gpui: the community fork
//! (gpui-ce) kept its backend in a separate `gpui_platform` crate and had no
//! `Application::new` at all. See the workspace manifest for the rest of the
//! migration.
//!
//! The bus connection deliberately lives on a dedicated thread with its own
//! async-io runtime (see `app.rs`); nothing bus-related is driven from a gpui
//! future, because gpui's executor threads have no tokio context and zbus'
//! `spawn_blocking` panics there.

mod app;

use app::TimetrackView;
use gpui::{App, AppContext, Application, Bounds, WindowBounds, WindowOptions, px, size};

fn main() {
    Application::new().run(|cx: &mut App| {
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
