/* tray/actions.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! Quick-add action bookkeeping: labels, outcome reporting, and the
//! per-session undo records the menu workers maintain.

use crate::lock;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use timetrack_core::RuleError;
use timetrack_proto::{ClientError, ServiceError};

/// Label for a quick-add bucket row, in minutes like the GUI and CLI buckets
/// (`+60m`, not the compact `1h`, so the four rows read uniformly).
pub(crate) fn quick_add_label(duration_ms: i64) -> String {
    format!("+{}m", duration_ms / 60_000)
}

/// Record the outcome of a menu action, so the icon and the tooltip can say
/// what happened.
///
/// A failure used to reach the user only as a line on stderr, in a terminal
/// that may not be open: the menu did not change and the icon stayed
/// healthy, so a quick-add that failed looked exactly like one that worked.
///
/// The message is sticky until the next action succeeds -- unlike the
/// connection state, which the poll loop clears on the next good snapshot.
pub(crate) fn report_action(error: &Arc<Mutex<Option<String>>>, outcome: Option<&ClientError>) {
    *lock(error) = outcome.map(|e| e.to_string());
}

/// Remember the entry a quick-add created, so undo (#10) targets exactly the
/// id the service handed back rather than re-deriving it from the list.
pub(crate) fn record_quick_add(
    undo: &Arc<Mutex<HashMap<String, String>>>,
    project_id: &str,
    entry_id: &str,
) {
    lock(undo).insert(project_id.to_string(), entry_id.to_string());
}

/// Drop the undo record for `project_id`, but only if it still points at
/// `entry_id`: a newer quick-add may have moved the target while the undo
/// call was in flight, and finishing an undo must never clear a newer record.
pub(crate) fn forget_quick_add_if(
    undo: &Arc<Mutex<HashMap<String, String>>>,
    project_id: &str,
    entry_id: &str,
) {
    let mut guard = lock(undo);
    if guard.get(project_id).map(String::as_str) == Some(entry_id) {
        guard.remove(project_id);
    }
}
/// Whether a failed undo means the record is stale and must be dropped so
/// the row disables instead of failing forever.
///
/// A refusal naming the entry (`NotQuickAdd`, `UnknownEntry`) says the entry
/// is gone or was never a quick-add -- it was removed or edited elsewhere,
/// so no retry can succeed. Anything else (no service, transport) is
/// transient: the record stays and the next click retries.
pub(crate) fn undo_record_stale(result: &Result<(), ClientError>) -> bool {
    matches!(
        result,
        Err(ClientError::Service(ServiceError::Rule(
            RuleError::NotQuickAdd(_) | RuleError::UnknownEntry(_)
        )))
    )
}
