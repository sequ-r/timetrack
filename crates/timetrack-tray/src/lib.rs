/* tray/lib.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The TimeTrack tray's neutral core plus its per-OS backends.
//!
//! The neutral modules (`menu`, `state`, `ipc`, `actions`, `platform`)
//! compile on every target: pure presentation, the menu model, the shared
//! service connection, and the action dispatch the backends invoke. The
//! Linux StatusNotifier backend lives behind `backend_linux`
//! (`cfg(target_os = "linux")`); the Windows notification-area backend
//! lives behind `backend_windows` (#155), whose pure mapping compiles
//! everywhere for headless tests while only its runner is `cfg(windows)`.
//! The macOS menu-bar backend lives behind `backend_macos` (#156) on the
//! same pattern: shared tray-icon mapping plus its own `cfg(macos)`
//! runner. The binary in `main.rs` is thin: single-instance claim plus
//! the poll loop on Linux, the native runner on Windows and macOS, a
//! stub elsewhere.

use std::sync::{Mutex, MutexGuard};

pub mod actions;
pub mod ipc;
pub mod menu;
pub mod platform;
pub mod state;

#[cfg(target_os = "linux")]
pub mod backend_linux;
pub mod backend_macos;
pub mod backend_windows;

/// The well-known name the tray owns for its whole lifetime, making it
/// a single instance the same way the service is one: AppRun starts a
/// tray next to the service on every app launch, so a name the second
/// copy cannot also own is what keeps repeated launches from stacking
/// tray icons. It is deliberately *not* the service's `BUS_NAME` --
/// the tray still connects to the service anonymously and can neither
/// steal its name nor keep a stale service alive. It matches the SNI
/// `id()` in `backend_linux`, pinned together by the identity test.
pub const TRAY_NAME: &str = "org.sequ.timetrack.tray";

/// Take a lock without unwrapping it (#51).
///
/// A poisoned lock still holds its data: the panic that poisoned it
/// happened elsewhere, and the data behind it is intact. Unwrapping the
/// poison turned one failure into a panic loop -- every later render
/// panicked on the same lock -- so the poison is recovered here and the
/// caller goes on.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}
