/* tray/main.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The TimeTrack system tray.
//!
//! A `StatusNotifierItem` (via `ksni`) whose presence shows the service is
//! running. The menu lists one submenu per unarchived project with its week
//! total plus quick-add, undo, and delete-confirm rows (tray-5..7, #9..#11);
//! the tooltip carries the live week total and the icon asks for attention
//! on over-24h days (tray-8, #12).
//!
//! # Runtime
//!
//! `ksni` is used with its `async-io` feature only (never the default `tokio`
//! feature -- see the workspace manifest and issue #4). The binary is driven
//! with `async_io::block_on`, the same pattern as the service.
//!
//! # Layout (#108)
//!
//! This file keeps only the poll loop and `main`. The seams live beside
//! it: `item` (the ksni item and its rendering), `menu` (the snapshot →
//! menu model), `state` (tooltip/connection classification), `ipc` (the
//! shared service connection), and `actions` (quick-add bookkeeping).

use ksni::TrayMethods as _;
use std::collections::HashMap;
use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use timetrack_proto::{ChangedStream, ChangedWait, Client, ClientError};

mod actions;
mod ipc;
mod item;
mod menu;
mod state;

use ipc::{SharedClient, set_shared_client, shared_client};
use item::TimetrackTray;
use menu::{MenuModel, build_menu_model};
use state::{ConnState, TooltipState, Unwell, conn_state, unwell_of};
// Named only by the tests (production code reaches them through the item
// and the model), so the non-test build does not import them.
#[cfg(test)]
use actions::{
    forget_quick_add_if, quick_add_label, record_quick_add, report_action, undo_record_stale,
};
#[cfg(test)]
use item::{DESKTOP_ENTRY, gui_argv};
#[cfg(test)]
use menu::{LastEntry, MAX_PROJECTS, ProjectItem};
#[cfg(test)]
use state::tooltip_text;

/// The fallback snapshot cadence (#103). Freshness normally arrives as
/// a `Changed` signal the moment a mutation persists; this slow poll
/// only covers an old (signalless) service, a missed signal, and
/// noticing that the service has gone away.
const POLL: Duration = Duration::from_secs(60);

/// Take a lock without unwrapping it (#51).
///
/// A poisoned lock still holds its data: the panic that poisoned it
/// happened elsewhere, and the data behind it is intact. Unwrapping the
/// poison turned one failure into a panic loop -- every later render
/// panicked on the same lock -- so the poison is recovered here and the
/// caller goes on.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// zbus uses the async-io backend (see the workspace Cargo.toml), so the
/// runtime is `async_io`, not tokio -- same as the service.
fn main() -> anyhow::Result<()> {
    async_io::block_on(run())
}

async fn run() -> anyhow::Result<()> {
    // `assume_sni_available` keeps the item alive when no watcher or host
    // exists yet (headless test bus, desktop still starting, GNOME without
    // an extension): the failure is routed to `watcher_offline` instead of
    // failing spawn, and registration is retried when a watcher appears.
    // Session undo targets, shared with the tray item and its worker
    // threads: quick-add records the created id here, the undo row reads it.
    // Held outside the item so the poll loop can see it change (see below).
    let last_quick_add = Arc::new(Mutex::new(HashMap::new()));
    // Quit is a flag, not an exit call, so the row stays unit-testable and
    // the process leaves through `main` below, unregistering the SNI item
    // cleanly instead of dying mid-D-Bus-call.
    let shutdown = Arc::new(AtomicBool::new(false));
    // The tray's one bus connection (#50): the poll loop publishes the
    // live client here and the menu workers reuse it, so a click never
    // opens a second connection.
    let client_slot: SharedClient = Arc::new(Mutex::new(None));
    let handle = TimetrackTray::new(
        last_quick_add.clone(),
        shutdown.clone(),
        client_slot.clone(),
    )
    .assume_sni_available(true)
    .spawn()
    .await
    .map_err(|e| anyhow::anyhow!("could not register tray item: {e}"))?;
    println!("timetrack-tray: registered org.sequ.timetrack.tray");

    let mut last_state: Option<ConnState> = None;
    let mut last_total_ms: Option<i64> = None;
    // The last menu pushed to the item, plus the last undo map and tooltip
    // state seen: compared by value so the host only re-renders on a real
    // change, not every tick. The undo map joins the gate because it
    // changes without any snapshot change: the quick-add worker records the
    // id after the service answers, which can land after the tick that
    // already rebuilt for the new week total -- without this the undo row
    // would stay disabled until the next snapshot change.
    let mut last_menu = MenuModel::default();
    let mut last_undo: HashMap<String, String> = HashMap::new();
    let mut last_summary = TooltipState::default();
    // The last icon state pushed: `None` while snapshots land, `Some` while
    // they fail. Compared by value with the rest, so a failure (or a
    // recovery) re-renders the icon and tooltip on the tick it happens.
    let mut last_unwell: Option<Unwell> = None;
    // Whether a snapshot ever succeeded: tells "not running" (never seen)
    // apart from "went away" (was here, now gone).
    let mut ever_ready = false;
    // The `Changed` subscription on the live connection (#103), created
    // with it and dropped with it. `None` while disconnected, and after
    // a stream ends until the next connection recreates it.
    let mut changed: Option<ChangedStream> = None;

    loop {
        // (Re)connect anonymously: `Client::connect` never requests BUS_NAME,
        // so a healthy service is never disturbed and an absent one just
        // fails here, before any method call.
        //
        // The connection is published in the shared slot, so the menu
        // workers reuse it instead of opening one of their own (#50).
        let client = match shared_client(&client_slot) {
            Some(c) => c,
            None => match Client::connect().await {
                Ok(c) => {
                    set_shared_client(&client_slot, Some(c.clone()));
                    c
                }
                Err(e) => {
                    // Connecting reaches no method, so any failure here is
                    // the bus itself, presented as the service being
                    // unreachable -- same as the GUI's startup path.
                    if last_state != Some(ConnState::NoService) {
                        last_state = Some(ConnState::NoService);
                        eprintln!("timetrack-tray: service not running ({e}); retrying");
                    }
                    // No bus, no totals either way: flatten the icon with
                    // the start hint until a connection succeeds.
                    if last_unwell != Some(Unwell::Down) {
                        last_unwell = Some(Unwell::Down);
                        handle
                            .update(|tray| tray.set_unwell(Some(Unwell::Down)))
                            .await;
                    }
                    async_io::Timer::after(POLL).await;
                    continue;
                }
            },
        };

        if changed.is_none() {
            changed = client.changed_stream().await.ok();
        }

        let result = client.snapshot().await;
        let state = conn_state(&result);
        if last_state != Some(state) {
            last_state = Some(state);
            match &result {
                Ok(_) => println!("timetrack-tray: connected"),
                Err(ClientError::NoService) if ever_ready => {
                    eprintln!("timetrack-tray: service went away; retrying");
                }
                Err(ClientError::NoService) => {
                    eprintln!("timetrack-tray: service not running; retrying");
                }
                Err(ClientError::VersionMismatch { name }) => {
                    eprintln!(
                        "timetrack-tray: service spoke {name}; client and service are different versions"
                    );
                }
                Err(e) => eprintln!("timetrack-tray: snapshot failed: {e}"),
            }
        }

        // The icon state follows every outcome, good or bad: a failure (or
        // a recovery) re-renders the icon and tooltip on the tick it
        // happens, without waiting for a menu change.
        let unwell = unwell_of(&result);
        if unwell != last_unwell {
            last_unwell = unwell.clone();
            handle.update(|tray| tray.set_unwell(unwell.clone())).await;
        }

        match result {
            Ok(snapshot) => {
                ever_ready = true;
                if last_total_ms != Some(snapshot.week.total_ms) {
                    last_total_ms = Some(snapshot.week.total_ms);
                    let minutes = snapshot.week.total_ms / 60_000;
                    println!("timetrack-tray: week total {minutes} min");
                }
                let menu = build_menu_model(&snapshot);
                let undo = lock(&last_quick_add).clone();
                let summary = TooltipState::from_snapshot(&snapshot);
                // A good snapshot clears any earlier failure state (pushed
                // above); the menu, undo map and summary gate the rest.
                if menu != last_menu || undo != last_undo || summary != last_summary {
                    last_menu = menu.clone();
                    last_undo = undo;
                    last_summary = summary.clone();
                    handle
                        .update(|tray| {
                            tray.set_menu(menu);
                            tray.set_summary(summary);
                        })
                        .await;
                }
            }
            Err(ClientError::NoService) | Err(ClientError::Transport(_)) => {
                // The bus or the service went away: drop the connection so
                // the next tick reconnects from scratch.
                set_shared_client(&client_slot, None);
                changed = None;
            }
            Err(ClientError::VersionMismatch { .. }) | Err(ClientError::Service(_)) => {
                // Keep the connection: the bus is fine. A mismatch means a
                // stale interface (needs a reinstall, not a reconnect); a
                // refusal on Snapshot should not happen, and retrying is
                // harmless either way.
            }
        }

        // Wait out the fallback tick in short slices (#103): a `Changed`
        // announcement ends the wait at once, and the slices keep Quit
        // answering within a beat instead of a full tick. Exiting here
        // returns through `main`, which drops the SNI item cleanly. This
        // stops the tray only -- nothing above calls the service on it.
        for _ in 0..POLL.as_millis() / 100 {
            if shutdown.load(Ordering::Relaxed) {
                println!("timetrack-tray: quit requested; exiting");
                return Ok(());
            }
            let slice = Duration::from_millis(100);
            match changed.as_mut() {
                Some(stream) => match stream.wait_timeout(slice).await {
                    ChangedWait::Changed => break,
                    // The connection ended: refresh now, which sorts the
                    // failure out on the normal snapshot path.
                    ChangedWait::Ended => {
                        changed = None;
                        break;
                    }
                    ChangedWait::TimedOut => {}
                },
                None => {
                    async_io::Timer::after(slice).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ConnState, MenuModel, TimetrackTray, TooltipState, build_menu_model, conn_state,
        quick_add_label, report_action, tooltip_text,
    };
    use ksni::Tray as _;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, atomic::AtomicBool};
    use timetrack_proto::{ClientError, ProjectView, ServiceError, Snapshot};

    fn tray() -> TimetrackTray {
        TimetrackTray::new(
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(None)),
        )
    }

    #[test]
    fn menu_model_names_are_sanitized_for_menu_labels() {
        // #101: the model name becomes submenu/confirm labels verbatim, so
        // a legacy store's control characters are cleaned at build time.
        let model = build_menu_model(&snapshot_with(
            vec![project("p1", "Wo\nrk\u{1b}[31m", false)],
            &[("p1", 60_000)],
        ));
        assert_eq!(model.visible[0].name, "Wo rk [31m");
        assert!(!model.visible[0].name.chars().any(|c| c.is_control()));
    }

    #[test]
    fn a_failed_action_is_visible_in_the_tooltip_and_the_icon() {
        // A failed quick-add used to reach the user only as a line on
        // stderr, in a terminal that may not be open: the menu did not
        // change and the icon stayed healthy, so it looked exactly like a
        // quick-add that worked.
        let tray = tray();
        let error = tray.action_error.clone();
        report_action(&error, Some(&ClientError::NoService));
        assert_eq!(
            tray.tool_tip().description,
            "the timetrack service is not running",
            "the tooltip says what failed"
        );
        assert_eq!(
            tray.status(),
            ksni::Status::NeedsAttention,
            "the icon asks for attention"
        );
    }

    #[test]
    fn a_successful_action_clears_the_previous_failure() {
        let tray = tray();
        let error = tray.action_error.clone();
        report_action(&error, Some(&ClientError::NoService));
        report_action(&error, None);
        assert!(
            tray.tool_tip().description.starts_with("Week total"),
            "the tooltip is back to the totals: {}",
            tray.tool_tip().description
        );
        assert_eq!(tray.status(), ksni::Status::Active);
    }

    fn project(id: &str, name: &str, archived: bool) -> ProjectView {
        ProjectView {
            id: id.into(),
            name: name.into(),
            colour: None,
            archived,
        }
    }

    fn snapshot_with(projects: Vec<ProjectView>, week_ms: &[(&str, i64)]) -> Snapshot {
        snapshot_with_entries(projects, week_ms, Vec::new())
    }

    fn snapshot_with_entries(
        projects: Vec<ProjectView>,
        week_ms: &[(&str, i64)],
        entries: Vec<timetrack_proto::EntryView>,
    ) -> Snapshot {
        let mut snapshot = Snapshot {
            projects,
            entries,
            ..Default::default()
        };
        for (id, ms) in week_ms {
            snapshot.week.per_project.insert((*id).into(), *ms);
        }
        snapshot.week.total_ms = week_ms.iter().map(|(_, ms)| *ms).sum();
        snapshot
    }

    fn entry(id: &str, project_id: &str, duration_ms: i64) -> timetrack_proto::EntryView {
        timetrack_proto::EntryView {
            id: id.into(),
            project_id: project_id.into(),
            description: String::new(),
            started_at: 0,
            ended_at: duration_ms,
            source: timetrack_proto::EntrySource::QuickAdd,
            note: None,
        }
    }

    #[test]
    fn tray_identity_is_stable() {
        // The watcher dedupes by id; a drift here would double-register.
        let tray = tray();
        assert_eq!(tray.id(), "org.sequ.timetrack.tray");
        assert_eq!(tray.title(), "TimeTrack");
        assert_eq!(tray.category(), ksni::Category::ApplicationStatus);
        assert_eq!(tray.status(), ksni::Status::Active);
        assert_eq!(tray.icon_name(), "org.sequ.timetrack-symbolic");
        let tip = tray.tool_tip();
        assert_eq!(tip.title, "TimeTrack");
        // No snapshot yet: the default summary reads zero, and the icon
        // sits idle rather than asking for attention.
        assert_eq!(tip.description, "Week total 0m · 0 projects");
        assert_eq!(tray.status(), ksni::Status::Active);
    }

    #[test]
    fn poll_states_sort_by_variant_not_text() {
        // Reuse of the proto triage: the tray sorts the same failures the
        // same way, and hostile message contents never steer the dispatch.
        assert_eq!(conn_state(&Ok(Snapshot::default())), ConnState::Ready);
        assert_eq!(
            conn_state(&Err(ClientError::NoService)),
            ConnState::NoService
        );
        assert_eq!(
            conn_state(&Err(ClientError::VersionMismatch {
                name: "org.freedesktop.DBus.Error.UnknownMethod".into(),
            })),
            ConnState::VersionMismatch
        );
        // A refusal whose text mentions the bus is still a refusal, not a
        // missing service.
        assert_eq!(
            conn_state(&Err(ClientError::Service(ServiceError::Other(
                "org.freedesktop.DBus.Error.ServiceUnknown was seen".into(),
            )))),
            ConnState::Refused
        );
        assert_eq!(
            conn_state(&Err(ClientError::Transport("brutal".into()))),
            ConnState::Transport
        );
    }

    #[test]
    fn menu_model_lists_unarchived_projects_with_week_totals() {
        let snapshot = snapshot_with(
            vec![project("p1", "Work", false), project("p2", "Rest", false)],
            &[("p1", 90 * 60_000), ("p2", 15 * 60_000)],
        );
        let model = build_menu_model(&snapshot);
        assert_eq!(model.visible.len(), 2);
        assert_eq!(model.visible[0].name, "Work");
        assert_eq!(model.visible[0].week_ms, 90 * 60_000);
        assert_eq!(model.visible[1].name, "Rest");
        assert!(model.overflow.is_empty());
    }

    #[test]
    fn menu_model_hides_archived_but_totals_keep_them() {
        // REQUIREMENTS §14: archived projects stay in historical totals, so
        // the week sum still counts them -- they just get no submenu.
        let snapshot = snapshot_with(
            vec![project("p1", "Work", false), project("p9", "Old", true)],
            &[("p1", 60 * 60_000), ("p9", 30 * 60_000)],
        );
        assert_eq!(snapshot.week.total_ms, 90 * 60_000);
        let model = build_menu_model(&snapshot);
        assert_eq!(model.visible.len(), 1);
        assert_eq!(model.visible[0].id, "p1");
        assert!(model.overflow.is_empty());
    }

    #[test]
    fn menu_model_overflows_beyond_max_projects() {
        let projects: Vec<ProjectView> = (0..10)
            .map(|n| project(&format!("p{n}"), &format!("P{n}"), false))
            .collect();
        let week: Vec<(String, i64)> = (0..10).map(|n| (format!("p{n}"), 0)).collect();
        let refs: Vec<(&str, i64)> = week.iter().map(|(id, ms)| (id.as_str(), *ms)).collect();
        let model = build_menu_model(&snapshot_with(projects, &refs));
        assert_eq!(model.visible.len(), super::MAX_PROJECTS);
        assert_eq!(model.overflow.len(), 10 - super::MAX_PROJECTS);
        // Stable order: the first projects stay on top, the tail overflows.
        assert_eq!(model.visible[0].id, "p0");
        assert_eq!(model.overflow[0].id, format!("p{}", super::MAX_PROJECTS));
    }

    #[test]
    fn menu_model_compares_by_value_for_rebuild_gating() {
        // The poll loop pushes to the item only on inequality, so equal
        // snapshots must compare equal and any visible change must not.
        let projects = vec![project("p1", "Work", false)];
        let a = build_menu_model(&snapshot_with(projects.clone(), &[("p1", 60 * 60_000)]));
        let same = build_menu_model(&snapshot_with(projects.clone(), &[("p1", 60 * 60_000)]));
        assert_eq!(a, same);
        let mut grown = same.clone();
        grown.visible.push(super::ProjectItem {
            id: "p2".into(),
            name: "New".into(),
            week_ms: 0,
            last_entry: None,
        });
        assert_ne!(a, grown);
        let renamed = build_menu_model(&snapshot_with(
            vec![project("p1", "Play", false)],
            &[("p1", 60 * 60_000)],
        ));
        assert_ne!(a, renamed);
        let retotalled = build_menu_model(&snapshot_with(projects, &[("p1", 61 * 60_000)]));
        assert_ne!(a, retotalled);
        // An entry landing (or leaving) changes the delete target, so it
        // rebuilds too.
        let entered = build_menu_model(&snapshot_with_entries(
            vec![project("p1", "Work", false)],
            &[("p1", 60 * 60_000)],
            vec![entry("e1", "p1", 60 * 60_000)],
        ));
        assert_ne!(a, entered);
        // An empty store models empty: no submenus, no overflow.
        assert_eq!(build_menu_model(&Snapshot::default()), MenuModel::default());
    }

    #[test]
    fn menu_renders_one_submenu_per_visible_project() {
        let mut tray = tray();
        tray.set_menu(build_menu_model(&snapshot_with(
            vec![project("p1", "Work", false), project("p9", "Old", true)],
            &[("p1", 75 * 60_000), ("p9", 30 * 60_000)],
        )));
        let items = tray.menu();
        // One project submenu, then the app actions (tray-10, #14).
        assert_eq!(items.len(), 3);
        let ksni::MenuItem::SubMenu(sub) = &items[0] else {
            panic!("project must render as a submenu");
        };
        assert_eq!(sub.label, "Work");
        // The disabled week-total header, one row per quick-add bucket, the
        // undo row, then the delete row (disabled: no entries here).
        assert_eq!(sub.submenu.len(), 3 + timetrack_core::QUICK_ADD_MS.len());
        let ksni::MenuItem::Standard(row) = &sub.submenu[0] else {
            panic!("submenu must hold the week-total row");
        };
        assert_eq!(row.label, "This week: 1h 15m");
        assert!(!row.enabled);
        let labels: Vec<String> = sub.submenu[1..=timetrack_core::QUICK_ADD_MS.len()]
            .iter()
            .map(|item| {
                let ksni::MenuItem::Standard(row) = item else {
                    panic!("quick-add must render as a standard row");
                };
                assert!(row.enabled);
                row.label.clone()
            })
            .collect();
        assert_eq!(labels, ["+5m", "+15m", "+30m", "+60m"]);
        let undo = find_standard(sub, "Undo quick-add");
        assert!(!undo.enabled);
        let delete = find_standard(sub, "Nothing to delete");
        assert!(!delete.enabled);
    }

    #[test]
    fn the_tray_keeps_one_shared_connection() {
        // #50: the poll loop publishes the live client in a slot, and the
        // menu workers take it from there -- a click must never open a
        // connection of its own. The slot is the tray's connection, so the
        // tray keeps it.
        let slot: super::SharedClient = Arc::new(Mutex::new(None));
        let tray = TimetrackTray::new(
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(AtomicBool::new(false)),
            slot.clone(),
        );
        assert!(
            Arc::ptr_eq(&tray.client, &slot),
            "the tray must keep the very slot the poll loop publishes to"
        );
        // Nothing published yet: there is no connection to reuse, and a
        // worker must refuse the call rather than open one in its place.
        assert!(super::shared_client(&slot).is_none());
    }

    #[test]
    fn quick_add_buckets_are_5_15_30_60() {
        // The submenu rows are hardcoded to these, so a change to the core
        // constant must break this test rather than the menu.
        let mins: Vec<i64> = timetrack_core::QUICK_ADD_MS
            .iter()
            .map(|m| m / 60_000)
            .collect();
        assert_eq!(mins, [5, 15, 30, 60]);
        let labels: Vec<String> = timetrack_core::QUICK_ADD_MS
            .iter()
            .map(|ms| quick_add_label(*ms))
            .collect();
        assert_eq!(labels, ["+5m", "+15m", "+30m", "+60m"]);
    }

    #[test]
    fn quick_add_records_the_created_id_per_project() {
        // Undo (#10) targets exactly the entry a quick-add created: the id
        // the service handed back, kept per project for this session only.
        // This is the same helper the menu worker calls when its call lands.
        let tray = tray();
        let undo = tray.last_quick_add.clone();
        assert!(undo.lock().unwrap().is_empty());
        super::record_quick_add(&undo, "p1", "e1");
        super::record_quick_add(&undo, "p2", "e9");
        assert_eq!(
            undo.lock().unwrap().get("p1").map(String::as_str),
            Some("e1")
        );
        assert_eq!(
            undo.lock().unwrap().get("p2").map(String::as_str),
            Some("e9")
        );
        // A second quick-add to the same project moves the undo target.
        super::record_quick_add(&undo, "p1", "e2");
        assert_eq!(
            undo.lock().unwrap().get("p1").map(String::as_str),
            Some("e2")
        );
    }

    /// A project submenu by project name: what the host renders one of.
    fn project_submenu_named<'a>(
        items: &'a [ksni::MenuItem<TimetrackTray>],
        project: &str,
    ) -> &'a ksni::menu::SubMenu<TimetrackTray> {
        items
            .iter()
            .find_map(|item| {
                let ksni::MenuItem::SubMenu(sub) = item else {
                    return None;
                };
                (sub.label == project).then_some(sub)
            })
            .unwrap_or_else(|| panic!("menu must hold {project}"))
    }

    /// A standard row by label within a project submenu.
    fn find_standard<'a>(
        sub: &'a ksni::menu::SubMenu<TimetrackTray>,
        label: &str,
    ) -> &'a ksni::menu::StandardItem<TimetrackTray> {
        sub.submenu
            .iter()
            .find_map(|item| {
                let ksni::MenuItem::Standard(row) = item else {
                    return None;
                };
                (row.label == label).then_some(row)
            })
            .unwrap_or_else(|| panic!("submenu must hold {label}"))
    }

    /// The submenu row a test host would click: the last row of each project
    /// submenu, with its enabled flag.
    fn undo_row(items: &[ksni::MenuItem<TimetrackTray>], project: &str) -> (String, bool) {
        let sub = project_submenu_named(items, project);
        let row = find_standard(sub, "Undo quick-add");
        (row.label.clone(), row.enabled)
    }

    #[test]
    fn undo_enables_only_for_projects_quick_added_this_session() {
        // Tray-6 (#10): the row enables per project, and only while this
        // session's last action on that project was a quick-add. No
        // snapshot fallback: a quick-add from an earlier session (visible in
        // the entries list) must not enable the row.
        let mut tray = tray();
        tray.set_menu(build_menu_model(&snapshot_with(
            vec![project("p1", "Work", false), project("p2", "Rest", false)],
            &[("p1", 15 * 60_000)],
        )));
        let items = tray.menu();
        assert_eq!(undo_row(&items, "Work"), ("Undo quick-add".into(), false));
        assert_eq!(undo_row(&items, "Rest"), ("Undo quick-add".into(), false));
        assert_eq!(tray.last_quick_add_for("p1"), None);

        super::record_quick_add(&tray.last_quick_add, "p1", "e1");
        assert_eq!(tray.last_quick_add_for("p1"), Some("e1".into()));
        let items = tray.menu();
        assert_eq!(undo_row(&items, "Work"), ("Undo quick-add".into(), true));
        assert_eq!(undo_row(&items, "Rest"), ("Undo quick-add".into(), false));
    }

    #[test]
    fn forget_quick_add_clears_only_the_matching_record() {
        // Finishing an undo drops the record so the row disables -- but a
        // newer quick-add may have moved the target mid-flight, and that
        // newer record must survive.
        let undo = Arc::new(Mutex::new(HashMap::new()));
        super::record_quick_add(&undo, "p1", "e1");
        super::record_quick_add(&undo, "p1", "e2");
        // A stale completion (the first entry) leaves the newer target alone.
        super::forget_quick_add_if(&undo, "p1", "e1");
        assert_eq!(
            undo.lock().unwrap().get("p1").map(String::as_str),
            Some("e2")
        );
        // The matching completion disables the row.
        super::forget_quick_add_if(&undo, "p1", "e2");
        assert!(undo.lock().unwrap().get("p1").is_none());
        // Forgetting what was never recorded is a no-op, not a panic.
        super::forget_quick_add_if(&undo, "p9", "e9");
    }

    #[test]
    fn undo_record_stale_drops_only_gone_or_refused_entries() {
        // A refusal naming the entry says no retry can succeed (removed or
        // edited elsewhere); anything else is transient and keeps the row
        // enabled for the next click.
        use timetrack_core::RuleError;
        let stale = |e: ClientError| super::undo_record_stale(&Err(e));
        assert!(!super::undo_record_stale(&Ok(())));
        assert!(stale(ClientError::Service(ServiceError::Rule(
            RuleError::NotQuickAdd("e1".into())
        ))));
        assert!(stale(ClientError::Service(ServiceError::Rule(
            RuleError::UnknownEntry("e1".into())
        ))));
        // Other rule refusals name no entry fate: keep the record.
        assert!(!stale(ClientError::Service(ServiceError::Rule(
            RuleError::UnknownProject("p1".into())
        ))));
        assert!(!stale(ClientError::Service(ServiceError::Rule(
            RuleError::InvalidQuickAdd(1)
        ))));
        assert!(!stale(ClientError::Service(ServiceError::Storage(
            "disk full".into()
        ))));
        assert!(!stale(ClientError::Service(ServiceError::Other(
            "older service".into()
        ))));
        assert!(!stale(ClientError::NoService));
        assert!(!stale(ClientError::Transport("cut".into())));
        assert!(!stale(ClientError::VersionMismatch { name: "x".into() }));
    }

    #[test]
    fn menu_model_targets_the_most_recent_entry_per_project() {
        // Tray-7 (#11): delete offers the newest entry per project. Entries
        // arrive newest-first, so the first match wins; projects without
        // entries offer nothing.
        let snapshot = snapshot_with_entries(
            vec![project("p1", "Work", false), project("p2", "Rest", false)],
            &[("p1", 45 * 60_000)],
            vec![
                entry("e2", "p1", 30 * 60_000),
                entry("e1", "p1", 15 * 60_000),
            ],
        );
        let model = build_menu_model(&snapshot);
        assert_eq!(
            model.visible[0].last_entry,
            Some(super::LastEntry {
                id: "e2".into(),
                duration_ms: 30 * 60_000,
            })
        );
        assert_eq!(model.visible[1].last_entry, None);
    }

    #[test]
    fn menu_renders_delete_confirm_behind_a_named_submenu() {
        // The outer row states the duration; the confirm row states duration
        // and project (mirroring the GUI banner); cancel keeps the entry.
        let mut tray = tray();
        tray.set_menu(build_menu_model(&snapshot_with_entries(
            vec![project("p1", "Work", false)],
            &[("p1", 30 * 60_000)],
            vec![entry("e2", "p1", 30 * 60_000)],
        )));
        let items = tray.menu();
        let sub = project_submenu_named(&items, "Work");
        let ksni::MenuItem::SubMenu(delete) = sub.submenu.last().unwrap() else {
            panic!("submenu must end with the delete row");
        };
        assert_eq!(delete.label, "Delete last entry (30m)");
        assert_eq!(delete.submenu.len(), 2);
        let ksni::MenuItem::Standard(confirm) = &delete.submenu[0] else {
            panic!("the confirm row must be a standard row");
        };
        assert_eq!(confirm.label, "Delete 30m from Work");
        assert!(confirm.enabled);
        let ksni::MenuItem::Standard(cancel) = &delete.submenu[1] else {
            panic!("the cancel row must be a standard row");
        };
        assert_eq!(cancel.label, "Keep it");
        assert!(cancel.enabled);
    }

    #[test]
    fn menu_disables_delete_without_entries() {
        let mut tray = tray();
        tray.set_menu(build_menu_model(&snapshot_with(
            vec![project("p1", "Work", false)],
            &[("p1", 0)],
        )));
        let items = tray.menu();
        let sub = project_submenu_named(&items, "Work");
        let row = find_standard(sub, "Nothing to delete");
        assert!(!row.enabled);
    }

    #[test]
    fn fresh_store_offers_general_with_empty_week() {
        // Tray-13 (#17): a fresh install seeds one `General` project, so the
        // menu is never empty -- but every row is inert until time lands.
        // Models the snapshot a freshly seeded service serves: the project,
        // no entries, zero totals.
        let snapshot = snapshot_with(vec![project("p1", "General", false)], &[]);
        let model = build_menu_model(&snapshot);
        assert_eq!(model.visible.len(), 1);
        assert_eq!(model.visible[0].name, "General");
        assert_eq!(model.visible[0].week_ms, 0);
        assert_eq!(model.visible[0].last_entry, None);
        assert!(model.overflow.is_empty());

        let mut tray = tray();
        tray.set_menu(model);
        let items = tray.menu();
        // The project submenu plus the app actions.
        assert_eq!(items.len(), 3);
        let sub = project_submenu_named(&items, "General");
        assert!(!find_standard(sub, "This week: 0m").enabled);
        assert!(!find_standard(sub, "Undo quick-add").enabled);
        assert!(!find_standard(sub, "Nothing to delete").enabled);
        // And the icon idles on a zero total.
        assert_eq!(tray.status(), ksni::Status::Active);
        assert_eq!(tray.tool_tip().description, "Week total 0m · 0 projects");
    }

    #[test]
    fn week_total_formats_as_h_mm() {
        // Unpadded, unwrapped hours: a week may legally exceed 168h (§4).
    }

    #[test]
    fn the_tooltip_and_the_menu_spell_a_duration_the_same_way() {
        // The tray had a formatter of its own for the tooltip, so the same
        // duration read `1:15` in the tooltip and `1h 15m` in the menu --
        // and the one that was not in the core was the one that was not
        // shared.
        let total = 75 * 60_000;
        let text = tooltip_text(&super::TooltipState {
            total_ms: total,
            projects: 1,
            attention: false,
        });
        let menu = timetrack_core::format_compact(total);
        assert!(
            text.contains(&menu),
            "the tooltip and the menu agree: {text} vs {menu}"
        );
    }

    #[test]
    fn tooltip_names_total_and_project_count() {
        let text = |total_ms: i64, projects: usize| {
            tooltip_text(&super::TooltipState {
                total_ms,
                projects,
                attention: false,
            })
        };
        assert_eq!(text(90 * 60_000, 2), "Week total 1h 30m · 2 projects");
        assert_eq!(text(60 * 60_000, 1), "Week total 1h · 1 project");
        assert_eq!(text(0, 0), "Week total 0m · 0 projects");
    }

    #[test]
    fn tooltip_state_comes_from_aggregates_not_entries() {
        // The total is authoritative even when the entries disagree: they
        // may carry only recent ones. The count covers every project with a
        // nonzero week total, archived or not -- their time feeds the total.
        let mut snapshot = snapshot_with_entries(
            vec![
                project("p1", "Work", false),
                project("p2", "Idle", false),
                project("p9", "Old", true),
            ],
            &[("p1", 60 * 60_000), ("p9", 30 * 60_000)],
            vec![entry("e1", "p1", 15 * 60_000)],
        );
        snapshot.week.total_ms = 90 * 60_000;
        let state = TooltipState::from_snapshot(&snapshot);
        assert_eq!(state.total_ms, 90 * 60_000);
        assert_eq!(state.projects, 2);
        assert!(!state.attention);

        let mut tray = tray();
        tray.set_summary(state);
        assert_eq!(tray.status(), ksni::Status::Active);
        assert_eq!(
            tray.tool_tip().description,
            "Week total 1h 30m · 2 projects"
        );
    }

    #[test]
    fn attention_triggers_on_over_24h_days() {
        // REQUIREMENTS §4: an over-24h day is a warning, never a rejection --
        // the icon asks for attention while the tooltip stays the total.
        let mut snapshot = snapshot_with(
            vec![project("p1", "Work", false)],
            &[("p1", 25 * 3_600_000)],
        );
        snapshot.week.total_ms = 25 * 3_600_000;
        snapshot.over_24h_days = vec![20240101];
        let state = TooltipState::from_snapshot(&snapshot);
        assert!(state.attention);

        let mut tray = tray();
        tray.set_summary(state);
        assert_eq!(tray.status(), ksni::Status::NeedsAttention);
        assert_eq!(tray.tool_tip().description, "Week total 25h · 1 project");
    }

    #[test]
    fn unwell_sorts_poll_outcomes_without_a_bus() {
        // Tray-9 (#13): the mapping from poll result to icon state is pure,
        // so both down states are drivable with no bus at all.
        assert_eq!(super::unwell_of(&Ok(Snapshot::default())), None);
        assert_eq!(
            super::unwell_of(&Err(ClientError::NoService)),
            Some(super::Unwell::Down)
        );
        assert_eq!(
            super::unwell_of(&Err(ClientError::Transport("cut".into()))),
            Some(super::Unwell::Down)
        );
        assert_eq!(
            super::unwell_of(&Err(ClientError::VersionMismatch { name: "x".into() })),
            Some(super::Unwell::Skewed)
        );
        // A refusal carries the service's own wording through, like the
        // GUI's StatusKind::Service -- matched by variant, never by text.
        assert_eq!(
            super::unwell_of(&Err(ClientError::Service(ServiceError::Other(
                "older service".into()
            )))),
            Some(super::Unwell::Refused("older service".into()))
        );
    }

    #[test]
    fn no_service_flattens_the_icon_with_the_start_hint() {
        // The shared wording with the GUI footer, rendered verbatim:
        // asserted against the proto helper itself so drift breaks here.
        let mut tray = tray();
        tray.set_unwell(Some(super::Unwell::Down));
        assert_eq!(tray.status(), ksni::Status::Passive);
        assert_eq!(
            tray.tool_tip().description,
            timetrack_proto::no_service_hint()
        );
    }

    #[test]
    fn version_mismatch_has_its_own_message_and_attention() {
        let mut tray = tray();
        tray.set_unwell(Some(super::Unwell::Skewed));
        assert_eq!(tray.status(), ksni::Status::NeedsAttention);
        assert_eq!(
            tray.tool_tip().description,
            timetrack_proto::version_mismatch_hint("tray")
        );
    }

    #[test]
    fn refusal_shows_the_services_own_wording() {
        let mut tray = tray();
        tray.set_unwell(Some(super::Unwell::Refused("older service".into())));
        // A refusal is not attention by itself: status still follows the
        // over-24h flag.
        assert_eq!(tray.status(), ksni::Status::Active);
        assert_eq!(tray.tool_tip().description, "older service");
        tray.set_summary(super::TooltipState {
            total_ms: 0,
            projects: 0,
            attention: true,
        });
        assert_eq!(tray.status(), ksni::Status::NeedsAttention);
    }

    #[test]
    fn recovery_returns_the_icon_to_live_totals() {
        let mut tray = tray();
        tray.set_unwell(Some(super::Unwell::Down));
        assert_eq!(tray.status(), ksni::Status::Passive);
        // The next good snapshot clears the failure (what the poll loop
        // pushes); rendering needs no bus to prove it.
        tray.set_unwell(super::unwell_of(&Ok(Snapshot::default())));
        tray.set_summary(super::TooltipState {
            total_ms: 90 * 60_000,
            projects: 2,
            attention: false,
        });
        assert_eq!(tray.status(), ksni::Status::Active);
        assert_eq!(
            tray.tool_tip().description,
            "Week total 1h 30m · 2 projects"
        );
    }

    #[test]
    fn open_runs_the_desktop_exec() {
        // The Open row follows `data/org.sequ.timetrack.desktop:5`, so the
        // tray and the installed launcher agree on how the app starts.
        // Field codes are launcher placeholders, not arguments.
        assert_eq!(
            super::gui_argv("Exec=timetrack-gui %U\n"),
            ["timetrack-gui"]
        );
        assert_eq!(
            super::gui_argv("[Desktop Entry]\nName=X\nExec=timetrack-gui --foo %F\n"),
            ["timetrack-gui", "--foo"]
        );
        assert!(super::gui_argv("[Desktop Entry]\nName=X\n").is_empty());
        // The shipped entry launches the GUI, not the TUI.
        assert_eq!(super::gui_argv(super::DESKTOP_ENTRY), ["timetrack-gui"]);
    }

    #[test]
    fn quit_requests_shutdown_without_touching_the_service() {
        // Tray-10 (#14): quitting stops the tray only, never the service.
        // The row flips a flag the poll loop exits on; the handler itself
        // makes no service call. There is deliberately no bus here, and the
        // test process surviving the click proves the handler returns
        // instead of killing the process inline.
        let mut tray = tray();
        assert!(!tray.quit_requested());
        let items = tray.menu();
        let labels: Vec<&str> = items
            .iter()
            .map(|item| {
                let ksni::MenuItem::Standard(row) = item else {
                    panic!("a projectless menu holds only the app actions");
                };
                row.label.as_str()
            })
            .collect();
        assert_eq!(labels, ["Open TimeTrack", "Quit"]);
        let quit = items
            .iter()
            .find_map(|item| {
                let ksni::MenuItem::Standard(row) = item else {
                    return None;
                };
                (row.label == "Quit").then_some(row)
            })
            .expect("menu must end with Quit");
        (quit.activate)(&mut tray);
        assert!(tray.quit_requested());
    }

    /// Poison a mutex the way a panic inside a guarded window would. The
    /// panic message is silenced: the panic is the point, and it is caught
    /// here.
    fn poison<T>(mutex: &Mutex<T>) {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = mutex.lock().unwrap();
            panic!("a panic inside the guarded window");
        }));
        std::panic::set_hook(previous);
        assert!(poisoned.is_err(), "the lock must really be poisoned");
    }

    #[test]
    fn a_poisoned_lock_does_not_panic_the_render_path() {
        // #51: a panic while a lock is held poisons it, and the render path
        // used to unwrap that poison into a panic of its own -- so one
        // failure turned into a panic loop. The data behind a poisoned lock
        // is still there, so the render path must go on using it.
        let mut tray = tray();
        tray.set_menu(build_menu_model(&snapshot_with(
            vec![project("p1", "Work", false)],
            &[("p1", 30 * 60_000)],
        )));
        super::record_quick_add(&tray.last_quick_add, "p1", "e1");

        // Every lock the render path takes, poisoned.
        poison(&tray.menu);
        poison(&tray.summary);
        poison(&tray.unwell);
        poison(&tray.last_quick_add);

        // The menu still renders, with the same rows.
        let items = tray.menu();
        let sub = project_submenu_named(&items, "Work");
        assert_eq!(sub.label, "Work");
        assert_eq!(find_standard(sub, "This week: 30m").label, "This week: 30m");
        // The undo row is still enabled: the undo map is intact.
        assert_eq!(undo_row(&items, "Work"), ("Undo quick-add".into(), true));
        // The tooltip and the icon still answer.
        assert_eq!(tray.tool_tip().description, "Week total 0m · 0 projects");
        assert_eq!(tray.status(), ksni::Status::Active);
        // And the setters still write through a poisoned lock.
        tray.set_summary(TooltipState {
            total_ms: 90 * 60_000,
            projects: 2,
            attention: false,
        });
        assert_eq!(
            tray.tool_tip().description,
            "Week total 1h 30m · 2 projects"
        );
    }
}
