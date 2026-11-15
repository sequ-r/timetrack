/* tray/backend_linux.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The Linux `StatusNotifierItem` backend (via `ksni`): the tray struct,
//! its rendering (status, tooltip, menus), and the per-project submenu
//! whose rows dispatch the neutral [`TrayAction`](crate::actions::TrayAction)s.
//! The poll loop in `main.rs` feeds it state; the pure pieces it renders
//! live in `menu`, `state`, `ipc`, and `actions`.

use crate::actions::{DispatchContext, TrayAction, dispatch, quick_add_label};
use crate::ipc::SharedClient;
use crate::lock;
use crate::menu::{MenuModel, ProjectItem};
use crate::state::{NeutralStatus, TooltipState, Unwell, presentation};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, atomic::AtomicBool};
use timetrack_core::QUICK_ADD_MS;

/// The tray item. State is replaced wholesale from the poll loop via
/// `Handle::update`; `menu()` renders whatever is current.
pub struct TimetrackTray {
    pub menu: Mutex<MenuModel>,
    /// The live tooltip and icon state (tray-8, #12). Written by the poll
    /// loop alongside the menu; `status()` and `tool_tip()` render it.
    pub summary: Mutex<TooltipState>,
    /// Why the icon is not showing live totals, if anything is wrong
    /// (tray-9, #13). Written by the poll loop on every tick; cleared on
    /// the next good snapshot.
    pub unwell: Mutex<Option<Unwell>>,
    /// The entry each project's last quick-add created, this session only.
    /// Written by the quick-add worker thread after the service answers;
    /// the undo row in tray-6 (#10) reads it. Shared with the workers via
    /// `Arc` so the sync `activate` handler stays non-blocking (cf. ksni's
    /// `StandardItem::activate` docs); the map itself is never replaced,
    /// only its entries, so a menu built earlier still records into this one.
    pub last_quick_add: Arc<Mutex<HashMap<String, String>>>,
    /// Set by the Quit row; the poll loop exits on the next beat (tray-10,
    /// #14). Shared so the sync `activate` handler only flips the flag --
    /// quitting never blocks, never calls the service, and leaves through
    /// `main`, unregistering the item cleanly.
    pub shutdown: Arc<AtomicBool>,
    /// The tray's one bus connection, shared with the poll loop (#50). The
    /// menu workers take the client out of it instead of opening a
    /// connection of their own.
    pub client: SharedClient,
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
    pub action_error: Arc<Mutex<Option<String>>>,
}

impl TimetrackTray {
    pub fn new(
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

    pub fn set_menu(&mut self, menu: MenuModel) {
        *lock(&self.menu) = menu;
    }

    pub fn set_summary(&mut self, summary: TooltipState) {
        *lock(&self.summary) = summary;
    }

    pub fn set_unwell(&mut self, unwell: Option<Unwell>) {
        *lock(&self.unwell) = unwell;
    }

    /// Whether quit was requested. The poll loop reads the shared flag
    /// directly, since the item itself lives inside ksni; the tests read
    /// it here.
    pub fn quit_requested(&self) -> bool {
        use std::sync::atomic::Ordering;
        self.shutdown.load(Ordering::Relaxed)
    }

    /// The entry undo should remove for `project_id`, if this session
    /// quick-added to it. `None` disables the undo row. Production clicks
    /// dispatch [`TrayAction::Undo`], which reads the record itself; the
    /// tests read it here.
    pub fn last_quick_add_for(&self, project_id: &str) -> Option<String> {
        lock(&self.last_quick_add).get(project_id).cloned()
    }

    /// This item's dispatch context: the shared handles [`dispatch`]
    /// needs, cloned per click.
    fn ctx(&self) -> DispatchContext {
        DispatchContext::new(
            self.last_quick_add.clone(),
            self.shutdown.clone(),
            self.client.clone(),
            self.action_error.clone(),
        )
    }

    /// Run one neutral action from a menu row: every `activate` below is
    /// this one call.
    fn dispatch(&self, action: TrayAction) {
        dispatch(action, &self.ctx());
    }

    /// One submenu per project: a disabled week-total row, then one row per
    /// quick-add bucket (tray-5, #9), the undo row (tray-6, #10), and the
    /// delete row with its confirm submenu (tray-7, #11).
    pub fn project_submenu(project: &ProjectItem, can_undo: bool) -> ksni::MenuItem<Self> {
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
                    tray.dispatch(TrayAction::QuickAdd {
                        project_id: project_id.clone(),
                        duration_ms: ms,
                    });
                }),
                ..Default::default()
            }));
        }
        // Undo (tray-6, #10): enabled only when this session quick-added to
        // this project.
        let undo_project = project.id.clone();
        submenu.push(ksni::MenuItem::Standard(ksni::menu::StandardItem {
            label: "Undo quick-add".into(),
            enabled: can_undo,
            activate: Box::new(move |tray: &mut Self| {
                tray.dispatch(TrayAction::Undo {
                    project_id: undo_project.clone(),
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
                                tray.dispatch(TrayAction::DeleteEntry {
                                    entry_id: entry_id.clone(),
                                    confirm_label: confirm_label.clone(),
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
        crate::TRAY_NAME.into()
    }

    fn title(&self) -> String {
        "TimeTrack".into()
    }

    fn category(&self) -> ksni::Category {
        ksni::Category::ApplicationStatus
    }

    fn status(&self) -> ksni::Status {
        let summary = lock(&self.summary).clone();
        let unwell = lock(&self.unwell).clone();
        let failed = lock(&self.action_error).clone();
        let (status, _) = presentation(&summary, unwell.as_ref(), failed.as_deref());
        match status {
            NeutralStatus::Passive => ksni::Status::Passive,
            NeutralStatus::Active => ksni::Status::Active,
            NeutralStatus::NeedsAttention => ksni::Status::NeedsAttention,
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
        let failed = lock(&self.action_error).clone();
        let (_, description) = presentation(&summary, unwell.as_ref(), failed.as_deref());
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
            activate: Box::new(move |tray: &mut Self| {
                tray.dispatch(TrayAction::OpenGui);
            }),
            ..Default::default()
        }));
        items.push(ksni::MenuItem::Standard(ksni::menu::StandardItem {
            label: "Quit".into(),
            activate: Box::new(move |tray: &mut Self| {
                tray.dispatch(TrayAction::Quit);
            }),
            ..Default::default()
        }));
        items
    }
}
