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
//! total; quick-add/remove actions inside each submenu land in tray-5..7
//! (#9..#11), and the live week-total tooltip in tray-8 (#12).
//!
//! # Runtime
//!
//! `ksni` is used with its `async-io` feature only (never the default `tokio`
//! feature -- see the workspace manifest and issue #4). The binary is driven
//! with `async_io::block_on`, the same pattern as the service.

use ksni::TrayMethods as _;
use std::time::Duration;
use timetrack_proto::{Client, ClientError, Snapshot};

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

/// The tray item. State is replaced wholesale from the poll loop via
/// `Handle::update`; `menu()` renders whatever is current.
struct TimetrackTray {
    menu: std::sync::Mutex<MenuModel>,
}

impl TimetrackTray {
    fn new() -> Self {
        Self {
            menu: std::sync::Mutex::new(MenuModel::default()),
        }
    }

    fn set_menu(&mut self, menu: MenuModel) {
        *self.menu.lock().unwrap() = menu;
    }

    /// One submenu per project: a disabled week-total row for now; the
    /// quick-add/remove actions inside land in tray-5..7 (#9..#11).
    fn project_submenu(project: &ProjectItem) -> ksni::MenuItem<Self> {
        ksni::MenuItem::SubMenu(ksni::menu::SubMenu {
            label: project.name.clone(),
            submenu: vec![ksni::MenuItem::Standard(ksni::menu::StandardItem {
                label: format!(
                    "This week: {}",
                    timetrack_core::format_compact(project.week_ms)
                ),
                enabled: false,
                ..Default::default()
            })],
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
        ksni::Status::Active
    }

    fn icon_name(&self) -> String {
        "org.sequ.timetrack-symbolic".into()
    }

    /// Static placeholder: the icon theme name plus a fixed title. The live
    /// week total replaces `description` in tray-8 (#12).
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            icon_name: "org.sequ.timetrack-symbolic".into(),
            title: "TimeTrack".into(),
            description: "Service running".into(),
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        let menu = self.menu.lock().unwrap();
        let mut items: Vec<ksni::MenuItem<Self>> =
            menu.visible.iter().map(Self::project_submenu).collect();
        if !menu.overflow.is_empty() {
            items.push(ksni::MenuItem::SubMenu(ksni::menu::SubMenu {
                label: "More projects".into(),
                submenu: menu.overflow.iter().map(Self::project_submenu).collect(),
                ..Default::default()
            }));
        }
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
    let handle = TimetrackTray::new()
        .assume_sni_available(true)
        .spawn()
        .await
        .map_err(|e| anyhow::anyhow!("could not register tray item: {e}"))?;
    println!("timetrack-tray: registered org.sequ.timetrack.tray");

    let mut client: Option<Client> = None;
    let mut last_state: Option<ConnState> = None;
    let mut last_total_ms: Option<i64> = None;
    // The last menu pushed to the item: compared by value so the host only
    // re-renders on a real change, not every 5s tick.
    let mut last_menu = MenuModel::default();
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

        match result {
            Ok(snapshot) => {
                ever_ready = true;
                if last_total_ms != Some(snapshot.week.total_ms) {
                    last_total_ms = Some(snapshot.week.total_ms);
                    let minutes = snapshot.week.total_ms / 60_000;
                    println!("timetrack-tray: week total {minutes} min");
                }
                let menu = build_menu_model(&snapshot);
                if menu != last_menu {
                    last_menu = menu.clone();
                    handle.update(|tray| tray.set_menu(menu)).await;
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

        async_io::Timer::after(POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::{ConnState, MenuModel, TimetrackTray, build_menu_model, conn_state};
    use ksni::Tray as _;
    use timetrack_proto::{ClientError, ProjectView, ServiceError, Snapshot};

    fn tray() -> TimetrackTray {
        TimetrackTray::new()
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
        let mut snapshot = Snapshot {
            projects,
            ..Default::default()
        };
        for (id, ms) in week_ms {
            snapshot.week.per_project.insert((*id).into(), *ms);
        }
        snapshot.week.total_ms = week_ms.iter().map(|(_, ms)| *ms).sum();
        snapshot
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
        assert_eq!(tip.description, "Service running");
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
        });
        assert_ne!(a, grown);
        let renamed = build_menu_model(&snapshot_with(
            vec![project("p1", "Play", false)],
            &[("p1", 60 * 60_000)],
        ));
        assert_ne!(a, renamed);
        let retotalled = build_menu_model(&snapshot_with(projects, &[("p1", 61 * 60_000)]));
        assert_ne!(a, retotalled);
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
        assert_eq!(items.len(), 1);
        let ksni::MenuItem::SubMenu(sub) = &items[0] else {
            panic!("project must render as a submenu");
        };
        assert_eq!(sub.label, "Work");
        assert_eq!(sub.submenu.len(), 1);
        let ksni::MenuItem::Standard(row) = &sub.submenu[0] else {
            panic!("submenu must hold the week-total row");
        };
        assert_eq!(row.label, "This week: 1h 15m");
        assert!(!row.enabled);
    }
}
