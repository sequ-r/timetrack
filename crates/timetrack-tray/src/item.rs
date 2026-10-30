/* tray/item.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The `StatusNotifierItem` itself: the tray struct, its ksni rendering
//! (status, tooltip, menus), and the per-project submenu whose rows spawn
//! the quick-add/undo/delete workers. The poll loop in `main.rs` feeds it
//! state; the pure pieces it renders live in `menu`, `state`, `ipc`, and
//! `actions`.

use crate::actions::{
    forget_quick_add_if, quick_add_label, record_quick_add, report_action, undo_record_stale,
};
use crate::ipc::{SharedClient, shared_client};
use crate::lock;
use crate::menu::{MenuModel, ProjectItem};
use crate::state::{TooltipState, Unwell, tooltip_text};
use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use timetrack_core::QUICK_ADD_MS;
use timetrack_proto::ClientError;

/// The desktop entry whose `Exec` line the Open row runs
/// (`data/org.sequ.timetrack.desktop:5`). Baked at compile time so the tray
/// and the installed launcher always agree on how the app starts.
pub(crate) const DESKTOP_ENTRY: &str = include_str!("../../../data/org.sequ.timetrack.desktop");

/// Command the Open row runs: the desktop entry's `Exec` line, first word
/// as argv[0]. Field codes (`%U`, `%F`, ...) are desktop-file placeholders
/// for the launcher, not arguments, and are dropped. Pure over the file
/// text so tests pin it without spawning anything.
pub(crate) fn gui_argv(desktop: &str) -> Vec<String> {
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

/// The tray item. State is replaced wholesale from the poll loop via
/// `Handle::update`; `menu()` renders whatever is current.
pub(crate) struct TimetrackTray {
    pub(crate) menu: Mutex<MenuModel>,
    /// The live tooltip and icon state (tray-8, #12). Written by the poll
    /// loop alongside the menu; `status()` and `tool_tip()` render it.
    pub(crate) summary: Mutex<TooltipState>,
    /// Why the icon is not showing live totals, if anything is wrong
    /// (tray-9, #13). Written by the poll loop on every tick; cleared on
    /// the next good snapshot.
    pub(crate) unwell: Mutex<Option<Unwell>>,
    /// The entry each project's last quick-add created, this session only.
    /// Written by the quick-add worker thread after the service answers;
    /// the undo row in tray-6 (#10) reads it. Shared with the workers via
    /// `Arc` so the sync `activate` handler stays non-blocking (cf. ksni's
    /// `StandardItem::activate` docs); the map itself is never replaced,
    /// only its entries, so a menu built earlier still records into this one.
    pub(crate) last_quick_add: Arc<Mutex<HashMap<String, String>>>,
    /// Set by the Quit row; the poll loop exits on the next beat (tray-10,
    /// #14). Shared so the sync `activate` handler only flips the flag --
    /// quitting never blocks, never calls the service, and leaves through
    /// `main`, unregistering the item cleanly.
    pub(crate) shutdown: Arc<AtomicBool>,
    /// The tray's one bus connection, shared with the poll loop (#50). The
    /// menu workers take the client out of it instead of opening a
    /// connection of their own.
    pub(crate) client: SharedClient,
    /// The last action that failed, and why.
    ///
    /// The menu workers write it; the icon and the tooltip read it. Without
    /// it a failed quick-add looked exactly like a successful one: the menu
    /// did not change and the icon stayed healthy, so the only trace was a
    /// line in a terminal that may not be open.
    ///
    /// Sticky until the next action succeeds -- unlike the connection
    /// state, which the poll loop clears on the next good snapshot, and
    /// rightly so: a failure is about one action, and a snapshot says
    /// nothing about whether that action worked.
    pub(crate) action_error: Arc<Mutex<Option<String>>>,
}

impl TimetrackTray {
    pub(crate) fn new(
        last_quick_add: Arc<Mutex<HashMap<String, String>>>,
        shutdown: Arc<AtomicBool>,
        client: SharedClient,
    ) -> Self {
        Self {
            menu: Mutex::new(MenuModel::default()),
            summary: Mutex::new(TooltipState::default()),
            unwell: Mutex::new(None),
            last_quick_add,
            shutdown,
            client,
            action_error: Arc::new(Mutex::new(None)),
        }
    }

    pub(crate) fn set_menu(&mut self, menu: MenuModel) {
        *lock(&self.menu) = menu;
    }

    pub(crate) fn set_summary(&mut self, summary: TooltipState) {
        *lock(&self.summary) = summary;
    }

    pub(crate) fn set_unwell(&mut self, unwell: Option<Unwell>) {
        *lock(&self.unwell) = unwell;
    }

    /// Ask the poll loop to exit. Touches nothing on the bus: quitting stops
    /// the tray only, never the service (tray-10, #14).
    pub(crate) fn request_quit(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    /// Whether quit was requested. Test-only: the poll loop reads the
    /// shared flag directly, since the item itself lives inside ksni.
    #[cfg(test)]
    pub(crate) fn quit_requested(&self) -> bool {
        self.shutdown.load(Ordering::Relaxed)
    }

    /// The entry undo should remove for `project_id`, if this session
    /// quick-added to it. `None` disables the undo row.
    pub(crate) fn last_quick_add_for(&self, project_id: &str) -> Option<String> {
        lock(&self.last_quick_add).get(project_id).cloned()
    }

    /// One submenu per project: a disabled week-total row, then one row per
    /// quick-add bucket (tray-5, #9), the undo row (tray-6, #10), and the
    /// delete row with its confirm submenu (tray-7, #11).
    pub(crate) fn project_submenu(project: &ProjectItem, can_undo: bool) -> ksni::MenuItem<Self> {
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
                    let slot = tray.client.clone();
                    let failed = tray.action_error.clone();
                    std::thread::spawn(move || {
                        let result: Result<timetrack_proto::EntryView, ClientError> =
                            async_io::block_on(async {
                                // The tray's own connection (#50): the click
                                // reuses it instead of opening a new one.
                                let client = shared_client(&slot).ok_or(ClientError::NoService)?;
                                client.quick_add(&project_id, ms).await
                            });
                        match result {
                            Ok(entry) => {
                                record_quick_add(&undo, &project_id, &entry.id);
                                report_action(&failed, None);
                                println!(
                                    "timetrack-tray: quick-added {} to {project_id} ({})",
                                    entry.id,
                                    quick_add_label(ms)
                                );
                            }
                            Err(e) => {
                                report_action(&failed, Some(&e));
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
                let slot = tray.client.clone();
                let failed = tray.action_error.clone();
                std::thread::spawn(move || {
                    let result: Result<(), ClientError> = async_io::block_on(async {
                        // The tray's own connection (#50): the click reuses
                        // it instead of opening a new one.
                        let client = shared_client(&slot).ok_or(ClientError::NoService)?;
                        client.undo_quick_add(&entry_id).await
                    });
                    match &result {
                        Ok(()) => {
                            forget_quick_add_if(&undo, &project_id, &entry_id);
                            report_action(&failed, None);
                            println!("timetrack-tray: undid quick add {entry_id} in {project_id}");
                        }
                        Err(e) if undo_record_stale(&result) => {
                            // The entry is gone or was never a quick-add
                            // (removed or edited elsewhere): drop the record
                            // so the row disables instead of failing forever.
                            forget_quick_add_if(&undo, &project_id, &entry_id);
                            report_action(&failed, Some(e));
                            eprintln!(
                                "timetrack-tray: undo of {entry_id} refused ({e}); forgetting"
                            );
                        }
                        Err(e) => {
                            // Transient (no service, transport): keep the
                            // record so the next click retries.
                            report_action(&failed, Some(e));
                            eprintln!("timetrack-tray: undo of {entry_id} failed: {e}");
                        }
                    }
                });
            }),
            ..Default::default()
        }));
        // Delete (tray-7, #11): the most recent entry for this project,
        // behind a confirm submenu stating exactly what would go -- the
        // tray's version of the GUI's `d`-then-`d` delete confirmation.
        // Explicit `delete_entry` only, never `set_times`: delete
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
                            activate: Box::new(move |tray: &mut Self| {
                                // Clone for the worker: `activate` must stay
                                // callable, so nothing moves out of it.
                                let entry_id = entry_id.clone();
                                let confirm_label = confirm_label.clone();
                                let slot = tray.client.clone();
                                let failed = tray.action_error.clone();
                                std::thread::spawn(move || {
                                    let result: Result<(), ClientError> =
                                        async_io::block_on(async {
                                            // The tray's own connection
                                            // (#50): the click reuses it
                                            // instead of opening a new one.
                                            let client = shared_client(&slot)
                                                .ok_or(ClientError::NoService)?;
                                            client.delete_entry(&entry_id).await
                                        });
                                    match result {
                                        Ok(()) => {
                                            report_action(&failed, None);
                                            println!(
                                                "timetrack-tray: deleted {entry_id} ({confirm_label})"
                                            );
                                        }
                                        Err(e) => {
                                            report_action(&failed, Some(&e));
                                            eprintln!(
                                                "timetrack-tray: delete of {entry_id} failed: {e}"
                                            );
                                        }
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
    /// No watcher, no icon (#105) — but not silently: log it and post the
    /// one notification that names the quick-add paths this desktop
    /// *does* have. Returns `true` (stay alive): a watcher can appear
    /// later — the user enables an extension, the shell restarts — and
    /// ksni re-registers then, calling `watcher_online`. The callback is
    /// synchronous, so the notification goes out on a worker thread like
    /// every other bus call from this item.
    fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
        let (summary, body) = crate::state::no_watcher_notice();
        eprintln!("timetrack-tray: {summary} ({reason:?}). {body}");
        std::thread::spawn(move || {
            async_io::block_on(async {
                // Best effort: a desktop with no watcher may also have
                // no notification daemon, and the log line above is then
                // the record. Nothing depends on this landing.
                let _ = timetrack_proto::desktop_notify(summary, body).await;
            });
        });
        true
    }

    fn watcher_online(&self) {
        println!("timetrack-tray: a status-notifier watcher appeared; the tray can show");
    }

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
        // A missing service flattens the icon; a skew, a refused snapshot,
        // a failed action or an over-24h day asks for attention; otherwise
        // the icon sits idle-active.
        let unwell = lock(&self.unwell).clone();
        let attention = lock(&self.summary).attention;
        let failed = lock(&self.action_error).is_some();
        match unwell {
            Some(Unwell::Down) => ksni::Status::Passive,
            Some(Unwell::Skewed) => ksni::Status::NeedsAttention,
            Some(Unwell::Refused(_)) | None => {
                if attention || failed {
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
        let summary = lock(&self.summary).clone();
        let unwell = lock(&self.unwell).clone();
        // A connection problem outranks a failed action: "the service is
        // not running" explains more than the refusal that failure caused.
        // An action failure outranks the week summary, which is the point:
        // the action is what the user just did.
        let failed = lock(&self.action_error).clone();
        // Service-origin text is sanitized (#101): a refusal can embed
        // stored text. The shared hints are ours — the version hint's
        // newlines are deliberate tooltip lines — so they pass through.
        let description = match unwell {
            Some(Unwell::Down) => timetrack_proto::no_service_hint(),
            Some(Unwell::Skewed) => timetrack_proto::version_mismatch_hint("tray"),
            Some(Unwell::Refused(text)) => timetrack_core::sanitize_display(&text),
            None => match failed {
                Some(text) => timetrack_core::sanitize_display(&text),
                None => tooltip_text(&summary),
            },
        };
        ksni::ToolTip {
            icon_name: "org.sequ.timetrack-symbolic".into(),
            title: "TimeTrack".into(),
            description,
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        // Copy the state out and drop the guards before building anything
        // (#51): the closures built below must not be built with a lock
        // held, and a panic while they are built must not poison the locks
        // the next render needs.
        let menu = lock(&self.menu).clone();
        let undo = lock(&self.last_quick_add).clone();
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
