/* core/lib.rs
 *
 * Copyright 2026 sequ
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! TimeTrack's domain core.
//!
//! This crate holds the data model, the timer state machine and JSON storage.
//! It deliberately depends on no GUI, IPC or async machinery, so the service,
//! the GTK-era GUI and the terminal UI can all share exactly the same rules.
//!
//! # Concurrency model
//!
//! The state machine takes `&mut Store` and an explicit timestamp. It never
//! reads the clock itself, which keeps it deterministic under test and means
//! the service is the only place that has to care about time.

pub mod model;
pub mod state;
pub mod storage;

pub use model::{Entry, Store};
pub use state::{
    CoreError, CoreResult, CounterIds, IdSource, cancel_entry, delete_entry, set_description,
    start_entry, stop_entry,
};
pub use storage::{JsonStore, StorageError};

/// Milliseconds since the Unix epoch, as used throughout the model.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Format a duration in milliseconds as `HH:MM:SS`.
pub fn format_duration(ms: i64) -> String {
    let total = (ms / 1000).max(0);
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    format!("{h:02}:{m:02}:{s:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_formats_with_padding() {
        assert_eq!(format_duration(0), "00:00:00");
        assert_eq!(format_duration(1_000), "00:00:01");
        assert_eq!(format_duration(61_000), "00:01:01");
        assert_eq!(format_duration(3_600_000), "01:00:00");
        assert_eq!(format_duration(36_000_000), "10:00:00");
    }

    #[test]
    fn negative_duration_clamps_to_zero() {
        assert_eq!(format_duration(-5_000), "00:00:00");
    }

    #[test]
    fn now_is_plausible_and_monotonic() {
        let a = now_ms();
        let b = now_ms();
        assert!(a > 0);
        assert!(b >= a);
    }
}
