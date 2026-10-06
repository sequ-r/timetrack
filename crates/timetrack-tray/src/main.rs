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

use ksni::TrayMethods as _;
use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use timetrack_core::{QUICK_ADD_MS, RuleError};
use timetrack_proto::{Client, ClientError, ServiceError, Snapshot};

/// The desktop entry whose `Exec` line the Open row runs
/// (`data/org.sequ.timetrack.desktop:5`). Baked at compile time so the tray
/// and the installed launcher always agree on how the app starts.
const DESKTOP_ENTRY: &str = include_str!("../../../data/org.sequ.timetrack.desktop");

/// Command the Open row runs: the desktop entry's `Exec` line, first word
/// as argv[0]. Field codes (`%U`, `%F`, ...) are desktop-file placeholders
/// for the launcher, not arguments, and are dropped. Pure over the file
/// text so tests pin it without spawning anything.
fn gui_argv(desktop: &str) -> Vec<String> {
    desktop
        .lines()
        .find_map(|line| line.strip_prefix("Exec="))
        .map(|exec| {
            exec.split_whitespace()
                .filter(|arg| !arg.starts_with('%'))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// How often the service is polled for a new snapshot. Slower than the
/// GUI's 1s tick: the tray only needs fresh menu data, not animation.
const POLL: Duration = Duration::from_secs(5);

/// How many projects sit at the menu's top level before the rest move under
/// "More projects". Tray menus must stay short; the aggregates behind them
/// still cover every project (cf. TODO task 5 on truncated entry lists --
/// here it is the menu, not the data, that is capped).
const MAX_PROJECTS: usize = 8;

/// One unarchived project as the menu sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProjectItem {
    id: String,
    name: String,
    /// This ISO week's total, from the service aggregate -- never re-summed
    /// from entries, which may carry only recent ones.
    week_ms: i64,
    /// The entry the delete row would remove: the most recent one for this
    /// project in the snapshot (tray-7, #11).
    last_entry: Option<LastEntry>,
}

/// The most recent entry for a project: its id plus its duration for the
/// confirm row. The snapshot arrives newest-first, so the first match per
/// project is the target.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LastEntry {
    id: String,
    duration_ms: i64,
}

/// What the menu shows. Compared by value each tick so the item only
/// rebuilds (and the host only re-renders) when something actually changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct MenuModel {
    visible: Vec<ProjectItem>,
    overflow: Vec<ProjectItem>,
}

/// Build the menu model from a snapshot. Archived projects are filtered out
/// of the menu but stay in the totals (REQUIREMENTS §14); beyond
/// `MAX_PROJECTS` the remainder moves under an overflow submenu.
fn build_menu_model(snapshot: &Snapshot) -> MenuModel {
    let mut active: Vec<ProjectItem> = snapshot
        .projects
        .iter()
        .filter(|p| !p.archived)
        .map(|p| ProjectItem {
            id: p.id.clone(),
            name: p.name.clone(),
            week_ms: snapshot.week.per_project.get(&p.id).copied().unwrap_or(0),
            last_entry: snapshot
                .entries
                .iter()
                .find(|e| e.project_id == p.id)
                .map(|e| LastEntry {
                    id: e.id.clone(),
                    duration_ms: e.duration_ms(),
                }),
        })
        .collect();
    let overflow = if active.len() > MAX_PROJECTS {
        active.split_off(MAX_PROJECTS)
    } else {
        Vec::new()
    };
    MenuModel {
        visible: active,
        overflow,
    }
}

/// What the tooltip and icon state show. Replaced wholesale from the poll
/// loop via `Handle::update`, like the menu -- and part of the same rebuild
/// gate, so the host re-renders on any real change, not every 5s tick.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct TooltipState {
    /// This ISO week's total, from the service aggregate.
    total_ms: i64,
    /// How many projects share that total (nonzero week total each).
    projects: usize,
    /// A day's summed time exceeds 24h (REQUIREMENTS §4): the icon asks for
    /// attention instead of sitting idle.
    attention: bool,
}

impl TooltipState {
    /// Derive the tooltip state from a snapshot: the week total, the
    /// contributor count through `Snapshot::project_totals`, and the
    /// over-24h flag. Entries are never re-summed -- they may carry only
    /// recent ones while the aggregates cover the whole store.
    fn from_snapshot(snapshot: &Snapshot) -> Self {
        Self {
            total_ms: snapshot.week.total_ms,
            projects: snapshot
                .projects
                .iter()
                .filter(|p| snapshot.project_totals(&p.id).0 > 0)
                .count(),
            attention: !snapshot.over_24h_days.is_empty(),
        }
    }
}

/// Format milliseconds as `H:MM`. Hours are unpadded and unwrapped: a week
/// may legally exceed 168h (§4) and must read as such, not wrap.
fn format_hmm(ms: i64) -> String {
    let mins = (ms / 60_000).max(0);
    format!("{}:{:02}", mins / 60, mins % 60)
}

/// The tooltip body: the week total plus how many projects share it.
fn tooltip_text(state: &TooltipState) -> String {
    let projects = if state.projects == 1 {
        "project"
    } else {
        "projects"
    };
    format!(
        "Week total {} · {} {projects}",
        format_hmm(state.total_ms),
        state.projects
    )
}

/// Why the icon is not showing live totals: the last poll outcome when it
/// was not a good snapshot. `None` means the tooltip shows the week summary
/// above. Sorted by `ClientError` variant (see `unwell_of`), never by
/// message text.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Unwell {
    /// The service is unreachable (absent, or the bus broke mid-run): the
    /// icon goes flat and the tooltip carries the start-service hint.
    Down,
    /// Client and service speak different interfaces: a distinct message
    /// naming the reinstall fix.
    Skewed,
    /// The service refused the snapshot: its own wording, like the GUI's
    /// `StatusKind::Service`.
    Refused(String),
}

/// Map a poll outcome to the icon's unwell state, if any. Pure over the
/// result so both down states are drivable without a bus (tray-9, #13).
fn unwell_of(result: &Result<Snapshot, ClientError>) -> Option<Unwell> {
    match result {
        Ok(_) => None,
        Err(ClientError::NoService) | Err(ClientError::Transport(_)) => Some(Unwell::Down),
        Err(ClientError::VersionMismatch { .. }) => Some(Unwell::Skewed),
        Err(ClientError::Service(e)) => Some(Unwell::Refused(e.to_string())),
    }
}

/// The tray item. State is replaced wholesale from the poll loop via
/// `Handle::update`; `menu()` renders whatever is current.
struct TimetrackTray {
    menu: Mutex<MenuModel>,
    /// The live tooltip and icon state (tray-8, #12). Written by the poll
    /// loop alongside the menu; `status()` and `tool_tip()` render it.
    summary: Mutex<TooltipState>,
    /// Why the icon is not showing live totals, if anything is wrong
    /// (tray-9, #13). Written by the poll loop on every tick; cleared on
    /// the next good snapshot.
    unwell: Mutex<Option<Unwell>>,
    /// The entry each project's last quick-add created, this session only.
    /// Written by the quick-add worker thread after the service answers;
    /// the undo row in tray-6 (#10) reads it. Shared with the workers via
    /// `Arc` so the sync `activate` handler stays non-blocking (cf. ksni's
    /// `StandardItem::activate` docs); the map itself is never replaced,
    /// only its entries, so a menu built earlier still records into this one.
    last_quick_add: Arc<Mutex<HashMap<String, String>>>,
    /// Set by the Quit row; the poll loop exits on the next beat (tray-10,
    /// #14). Shared so the sync `activate` handler only flips the flag --
    /// quitting never blocks, never calls the service, and leaves through
    /// `main`, unregistering the item cleanly.
    shutdown: Arc<AtomicBool>,
}

/// Label for a quick-add bucket row, in minutes like the GUI and CLI buckets
/// (`+60m`, not the compact `1h`, so the four rows read uniformly).
fn quick_add_label(duration_ms: i64) -> String {
    format!("+{}m", duration_ms / 60_000)
}

/// Remember the entry a quick-add created, so undo (#10) targets exactly the
/// id the service handed back rather than re-deriving it from the list.
fn record_quick_add(undo: &Arc<Mutex<HashMap<String, String>>>, project_id: &str, entry_id: &str) {
    undo.lock()
        .unwrap()
        .insert(project_id.to_string(), entry_id.to_string());
}

/// Drop the undo record for `project_id`, but only if it still points at
/// `entry_id`: a newer quick-add may have moved the target while the undo
/// call was in flight, and finishing an undo must never clear a newer record.
fn forget_quick_add_if(
    undo: &Arc<Mutex<HashMap<String, String>>>,
    project_id: &str,
    entry_id: &str,
) {
    let mut guard = undo.lock().unwrap();
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
fn undo_record_stale(result: &Result<(), ClientError>) -> bool {
    matches!(
        result,
        Err(ClientError::Service(ServiceError::Rule(
            RuleError::NotQuickAdd(_) | RuleError::UnknownEntry(_)
        )))
    )
}

impl TimetrackTray {
    fn new(last_quick_add: Arc<Mutex<HashMap<String, String>>>, shutdown: Arc<AtomicBool>) -> Self {
        Self {
            menu: Mutex::new(MenuModel::default()),
            summary: Mutex::new(TooltipState::default()),
            unwell: Mutex::new(None),
            last_quick_add,
            shutdown,
        }
    }

    fn set_menu(&mut self, menu: MenuModel) {
        *self.menu.lock().unwrap() = menu;
    }

    fn set_summary(&mut self, summary: TooltipState) {
        *self.summary.lock().unwrap() = summary;
    }

    fn set_unwell(&mut self, unwell: Option<Unwell>) {
        *self.unwell.lock().unwrap() = unwell;
    }

    /// Ask the poll loop to exit. Touches nothing on the bus: quitting stops
    /// the tray only, never the service (tray-10, #14).
    fn request_quit(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    /// Whether quit was requested. Test-only: the poll loop reads the
    /// shared flag directly, since the item itself lives inside ksni.
    #[cfg(test)]
    fn quit_requested(&self) -> bool {
        self.shutdown.load(Ordering::Relaxed)
    }

    /// The entry undo should remove for `project_id`, if this session
    /// quick-added to it. `None` disables the undo row.
    fn last_quick_add_for(&self, project_id: &str) -> Option<String> {
        self.last_quick_add.lock().unwrap().get(project_id).cloned()
    }

    /// One submenu per project: a disabled week-total row, then one row per
    /// quick-add bucket (tray-5, #9), the undo row (tray-6, #10), and the
    /// delete row with its confirm submenu (tray-7, #11).
    fn project_submenu(project: &ProjectItem, can_undo: bool) -> ksni::MenuItem<Self> {
        let mut submenu = vec![ksni::MenuItem::Standard(ksni::menu::StandardItem {
            label: format!(
                "This week: {}",
                timetrack_core::format_compact(project.week_ms)
            ),
            enabled: false,
            ..Default::default()
        })];
        for ms in QUICK_ADD_MS {
            let project_id = project.id.clone();
            submenu.push(ksni::MenuItem::Standard(ksni::menu::StandardItem {
                label: quick_add_label(ms),
                activate: Box::new(move |tray: &mut Self| {
                    // `activate` runs on the D-Bus thread and must not block:
                    // hand the call to a worker thread and return. The worker
                    // records the created id for undo below when it lands.
                    let project_id = project_id.clone();
                    let undo = tray.last_quick_add.clone();
                    std::thread::spawn(move || {
                        let result: Result<timetrack_proto::EntryView, ClientError> =
                            async_io::block_on(async {
                                let client = Client::connect().await?;
                                client.quick_add(&project_id, ms).await
                            });
                        match result {
                            Ok(entry) => {
                                record_quick_add(&undo, &project_id, &entry.id);
                                println!(
                                    "timetrack-tray: quick-added {} to {project_id} ({})",
                                    entry.id,
                                    quick_add_label(ms)
                                );
                            }
                            Err(e) => {
                                eprintln!("timetrack-tray: quick add to {project_id} failed: {e}");
                            }
                        }
                    });
                }),
                ..Default::default()
            }));
        }
        // Undo (tray-6, #10): enabled only when this session quick-added to
        // this project. The click reads the current id at click time rather
        // than the one present at menu build, so a quick-add that lands
        // between build and click still undoes the latest entry.
        let undo_project = project.id.clone();
        submenu.push(ksni::MenuItem::Standard(ksni::menu::StandardItem {
            label: "Undo quick-add".into(),
            enabled: can_undo,
            activate: Box::new(move |tray: &mut Self| {
                let Some(entry_id) = tray.last_quick_add_for(&undo_project) else {
                    eprintln!("timetrack-tray: nothing to undo for {undo_project}");
                    return;
                };
                let project_id = undo_project.clone();
                let undo = tray.last_quick_add.clone();
                std::thread::spawn(move || {
                    let result: Result<(), ClientError> = async_io::block_on(async {
                        let client = Client::connect().await?;
                        client.undo_quick_add(&entry_id).await
                    });
                    match &result {
                        Ok(()) => {
                            forget_quick_add_if(&undo, &project_id, &entry_id);
                            println!("timetrack-tray: undid quick add {entry_id} in {project_id}");
                        }
                        Err(e) if undo_record_stale(&result) => {
                            // The entry is gone or was never a quick-add
                            // (removed or edited elsewhere): drop the record
                            // so the row disables instead of failing forever.
                            forget_quick_add_if(&undo, &project_id, &entry_id);
                            eprintln!(
                                "timetrack-tray: undo of {entry_id} refused ({e}); forgetting"
                            );
                        }
                        Err(e) => {
                            // Transient (no service, transport): keep the
                            // record so the next click retries.
                            eprintln!("timetrack-tray: undo of {entry_id} failed: {e}");
                        }
                    }
                });
            }),
            ..Default::default()
        }));
        // Delete (tray-7, #11): the most recent entry for this project,
        // behind a confirm submenu stating exactly what would go -- the
        // tray's version of the GUI's `d`-then-`d` delete confirmation (TODO
        // task 11). Explicit `delete_entry` only, never `set_times`: delete
        // is not a side effect of shortening.
        match &project.last_entry {
            Some(target) => {
                let when = timetrack_core::format_compact(target.duration_ms);
                let entry_id = target.id.clone();
                let cancel_id = entry_id.clone();
                let confirm_label = format!("Delete {} from {}", when, project.name);
                submenu.push(ksni::MenuItem::SubMenu(ksni::menu::SubMenu {
                    label: format!("Delete last entry ({when})"),
                    submenu: vec![
                        ksni::MenuItem::Standard(ksni::menu::StandardItem {
                            label: confirm_label.clone(),
                            activate: Box::new(move |_: &mut Self| {
                                // Clone for the worker: `activate` must stay
                                // callable, so nothing moves out of it.
                                let entry_id = entry_id.clone();
                                let confirm_label = confirm_label.clone();
                                std::thread::spawn(move || {
                                    let result: Result<(), ClientError> =
                                        async_io::block_on(async {
                                            let client = Client::connect().await?;
                                            client.delete_entry(&entry_id).await
                                        });
                                    match result {
                                        Ok(()) => println!(
                                            "timetrack-tray: deleted {entry_id} ({confirm_label})"
                                        ),
                                        Err(e) => eprintln!(
                                            "timetrack-tray: delete of {entry_id} failed: {e}"
                                        ),
                                    }
                                });
                            }),
                            ..Default::default()
                        }),
                        ksni::MenuItem::Standard(ksni::menu::StandardItem {
                            label: "Keep it".into(),
                            activate: Box::new(move |_: &mut Self| {
                                let cancel_id = cancel_id.clone();
                                println!("timetrack-tray: delete of {cancel_id} cancelled");
                            }),
                            ..Default::default()
                        }),
                    ],
                    ..Default::default()
                }));
            }
            None => {
                submenu.push(ksni::MenuItem::Standard(ksni::menu::StandardItem {
                    label: "Nothing to delete".into(),
                    enabled: false,
                    ..Default::default()
                }));
            }
        }
        ksni::MenuItem::SubMenu(ksni::menu::SubMenu {
            label: project.name.clone(),
            submenu,
            ..Default::default()
        })
    }
}

impl ksni::Tray for TimetrackTray {
    fn id(&self) -> String {
        "org.sequ.timetrack.tray".into()
    }

    fn title(&self) -> String {
        "TimeTrack".into()
    }

    fn category(&self) -> ksni::Category {
        ksni::Category::ApplicationStatus
    }

    fn status(&self) -> ksni::Status {
        // Attention travels via status, not a second icon: only one icon
        // asset is installed, so a second icon name would render nothing.
        // A missing service flattens the icon; a skew or an over-24h day
        // asks for attention; otherwise the icon sits idle-active.
        let unwell = self.unwell.lock().unwrap().clone();
        let attention = self.summary.lock().unwrap().attention;
        match unwell {
            Some(Unwell::Down) => ksni::Status::Passive,
            Some(Unwell::Skewed) => ksni::Status::NeedsAttention,
            Some(Unwell::Refused(_)) | None => {
                if attention {
                    ksni::Status::NeedsAttention
                } else {
                    ksni::Status::Active
                }
            }
        }
    }

    fn icon_name(&self) -> String {
        "org.sequ.timetrack-symbolic".into()
    }

    /// The tooltip: live week totals when the service answers, otherwise
    /// the shared client guidance for the failure (tray-9, #13) -- the same
    /// wording as the GUI footer (`timetrack-proto::{no_service_hint,
    /// version_mismatch_hint}`). A refusal shows the service's own wording,
    /// like the GUI's `StatusKind::Service`.
    fn tool_tip(&self) -> ksni::ToolTip {
        let summary = self.summary.lock().unwrap().clone();
        let unwell = self.unwell.lock().unwrap().clone();
        let description = match unwell {
            None => tooltip_text(&summary),
            Some(Unwell::Down) => timetrack_proto::no_service_hint(),
            Some(Unwell::Skewed) => timetrack_proto::version_mismatch_hint("tray"),
            Some(Unwell::Refused(text)) => text,
        };
        ksni::ToolTip {
            icon_name: "org.sequ.timetrack-symbolic".into(),
            title: "TimeTrack".into(),
            description,
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        let menu = self.menu.lock().unwrap();
        let undo = self.last_quick_add.lock().unwrap();
        let mut items: Vec<ksni::MenuItem<Self>> = menu
            .visible
            .iter()
            .map(|p| Self::project_submenu(p, undo.contains_key(&p.id)))
            .collect();
        if !menu.overflow.is_empty() {
            items.push(ksni::MenuItem::SubMenu(ksni::menu::SubMenu {
                label: "More projects".into(),
                submenu: menu
                    .overflow
                    .iter()
                    .map(|p| Self::project_submenu(p, undo.contains_key(&p.id)))
                    .collect(),
                ..Default::default()
            }));
        }
        // App actions (tray-10, #14): opening the GUI runs the desktop
        // entry's `Exec`, so the tray and the launcher agree; quitting only
        // flips the shutdown flag -- it never touches the service.
        items.push(ksni::MenuItem::Standard(ksni::menu::StandardItem {
            label: "Open TimeTrack".into(),
            activate: Box::new(move |_: &mut Self| {
                let argv = gui_argv(DESKTOP_ENTRY);
                let Some(program) = argv.first() else {
                    eprintln!("timetrack-tray: desktop entry has no Exec line");
                    return;
                };
                match std::process::Command::new(program).args(&argv[1..]).spawn() {
                    Ok(_) => println!("timetrack-tray: opened {program}"),
                    Err(e) => eprintln!("timetrack-tray: could not open {program}: {e}"),
                }
            }),
            ..Default::default()
        }));
        items.push(ksni::MenuItem::Standard(ksni::menu::StandardItem {
            label: "Quit".into(),
            activate: Box::new(move |tray: &mut Self| {
                tray.request_quit();
            }),
            ..Default::default()
        }));
        items
    }
}

/// Where the poll loop stands. Sorts by `ClientError` variant, reusing the
/// triage in `timetrack-proto` -- never by message text, so a refusal that
/// merely mentions the bus can never read as a missing service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnState {
    Ready,
    NoService,
    VersionMismatch,
    Refused,
    Transport,
}

fn conn_state(result: &Result<Snapshot, ClientError>) -> ConnState {
    match result {
        Ok(_) => ConnState::Ready,
        Err(ClientError::NoService) => ConnState::NoService,
        Err(ClientError::VersionMismatch { .. }) => ConnState::VersionMismatch,
        Err(ClientError::Service(_)) => ConnState::Refused,
        Err(ClientError::Transport(_)) => ConnState::Transport,
    }
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
    let handle = TimetrackTray::new(last_quick_add.clone(), shutdown.clone())
        .assume_sni_available(true)
        .spawn()
        .await
        .map_err(|e| anyhow::anyhow!("could not register tray item: {e}"))?;
    println!("timetrack-tray: registered org.sequ.timetrack.tray");

    let mut client: Option<Client> = None;
    let mut last_state: Option<ConnState> = None;
    let mut last_total_ms: Option<i64> = None;
    // The last menu pushed to the item, plus the last undo map and tooltip
    // state seen: compared by value so the host only re-renders on a real
    // change, not every 5s tick. The undo map joins the gate because it
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

    loop {
        // (Re)connect anonymously: `Client::connect` never requests BUS_NAME,
        // so a healthy service is never disturbed and an absent one just
        // fails here, before any method call.
        if client.is_none() {
            match Client::connect().await {
                Ok(c) => client = Some(c),
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
            }
        }

        let result = client
            .as_ref()
            .expect("client is connected above")
            .snapshot()
            .await;
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
                let undo = last_quick_add.lock().unwrap().clone();
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
                client = None;
            }
            Err(ClientError::VersionMismatch { .. }) | Err(ClientError::Service(_)) => {
                // Keep the connection: the bus is fine. A mismatch means a
                // stale interface (needs a reinstall, not a reconnect); a
                // refusal on Snapshot should not happen, and retrying is
                // harmless either way.
            }
        }

        // Quit answers within a beat instead of a full tick: the flag is
        // cheap to poll, and exiting here returns through `main`, which
        // drops the SNI item cleanly. This stops the tray only -- nothing
        // above calls the service on this path.
        for _ in 0..POLL.as_millis() / 100 {
            if shutdown.load(Ordering::Relaxed) {
                println!("timetrack-tray: quit requested; exiting");
                return Ok(());
            }
            async_io::Timer::after(Duration::from_millis(100)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ConnState, MenuModel, TimetrackTray, TooltipState, build_menu_model, conn_state,
        format_hmm, quick_add_label, tooltip_text,
    };
    use ksni::Tray as _;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, atomic::AtomicBool};
    use timetrack_proto::{ClientError, ProjectView, ServiceError, Snapshot};

    fn tray() -> TimetrackTray {
        TimetrackTray::new(
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(AtomicBool::new(false)),
        )
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
        assert_eq!(tip.description, "Week total 0:00 · 0 projects");
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
    fn week_total_formats_as_h_mm() {
        // Unpadded, unwrapped hours: a week may legally exceed 168h (§4).
        assert_eq!(format_hmm(0), "0:00");
        assert_eq!(format_hmm(5 * 60_000), "0:05");
        assert_eq!(format_hmm(60 * 60_000), "1:00");
        assert_eq!(format_hmm(75 * 60_000), "1:15");
        assert_eq!(format_hmm(25 * 3_600_000), "25:00");
        assert_eq!(format_hmm(100 * 3_600_000), "100:00");
        assert_eq!(format_hmm(-1), "0:00");
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
        assert_eq!(text(90 * 60_000, 2), "Week total 1:30 · 2 projects");
        assert_eq!(text(60 * 60_000, 1), "Week total 1:00 · 1 project");
        assert_eq!(text(0, 0), "Week total 0:00 · 0 projects");
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
        assert_eq!(tray.tool_tip().description, "Week total 1:30 · 2 projects");
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
        assert_eq!(tray.tool_tip().description, "Week total 25:00 · 1 project");
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
        assert_eq!(tray.tool_tip().description, "Week total 1:30 · 2 projects");
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
}
