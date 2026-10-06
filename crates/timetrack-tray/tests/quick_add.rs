//! tray-5..7 (#9..#11): each project submenu offers +5/+15/+30/+60 rows,
//! an undo row, and a delete-confirm submenu -- activating them quick-adds
//! a bucket to the project in the service store, removes it again, and
//! deletes with confirmation.
//!
//! A private bus hosts a minimal watcher (as in `watcher.rs`) plus a fake
//! timetrack service backed by a real `JsonStore` through the real core
//! rules -- the same `Snapshot`/`QuickAdd`/`UndoQuickAdd`/`DeleteEntry` wire
//! the real service speaks. (`real_service.rs` runs the same round-trips
//! against the real service binary.) The real tray binary runs against that
//! bus; the test clicks `+15m` over D-Bus (`com.canonical.dbusmenu` `Event`,
//! the same call a desktop host makes), asserts a 15-minute `QuickAdd` entry
//! lands for the seeded project, undoes it, quick-adds `+30m`, cancels its
//! delete (the entry stays), then confirms and asserts it is gone.

mod common;

use common::ChildGuard;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use timetrack_proto::{BUS_NAME, Client, EntrySource, OBJECT_PATH};
use zbus::interface;

/// The fake service's state: a real store file through the real core rules,
/// so a quick-add that lands here went through the same validation and
/// persistence as against the real service.
struct FakeInner {
    store: timetrack_core::JsonStore,
    data: timetrack_core::Store,
    ids: timetrack_core::Counter,
}

struct FakeService {
    inner: Mutex<FakeInner>,
}

impl FakeService {
    /// A fresh store seeding `General` (p1), like the real service: with no
    /// project there is nothing the tray menu could offer.
    fn new(path: std::path::PathBuf) -> Self {
        let store = timetrack_core::JsonStore::new(path);
        let mut data = store.load().unwrap_or_default();
        if data.projects.is_empty() {
            data.projects.push(timetrack_core::Project {
                id: "p1".into(),
                name: "General".into(),
                colour: None,
                archived: false,
            });
        }
        let ids = timetrack_core::Counter::continuing(&data);
        let svc = Self {
            inner: Mutex::new(FakeInner { store, data, ids }),
        };
        {
            let g = svc.inner.lock().unwrap();
            g.store.save(&g.data).expect("the seed must persist");
        }
        svc
    }

    /// Everything the tray's poll loop reads: projects plus this week's
    /// per-project totals from the service aggregates, never re-summed.
    fn snapshot_json(&self) -> String {
        let g = self.inner.lock().unwrap();
        let now = now_ms();
        let tz = timetrack_core::Tz::utc();
        let today = tz.day_of(now);
        let week = timetrack_core::week_totals(&g.data, today, &tz);
        let month = timetrack_core::month_totals(&g.data, today, &tz);
        let all = timetrack_core::all_totals(&g.data);
        let snap = timetrack_proto::Snapshot {
            projects: g
                .data
                .projects
                .iter()
                .map(timetrack_proto::project_to_view)
                .collect(),
            entries: g
                .data
                .recent()
                .into_iter()
                .map(timetrack_proto::entry_to_view)
                .collect(),
            week: to_totals(&week),
            month: to_totals(&month),
            all: to_totals(&all),
            total_ms: all.total_ms,
            over_24h_days: Vec::new(),
            local_offset_ms: 0,
            tz: "UTC".into(),
        };
        serde_json::to_string(&snap).expect("the snapshot is serializable")
    }

    /// Method 4 through the real rule, persisted before acknowledging -- the
    /// same ordering the real service guarantees. Refusals travel typed, as
    /// on the real wire.
    fn quick_add_json(&self, project_id: &str, duration_ms: i64) -> zbus::fdo::Result<String> {
        let mut g = self.inner.lock().unwrap();
        let now = now_ms();
        let FakeInner { data, ids, store } = &mut *g;
        match timetrack_core::quick_add(data, ids, project_id, duration_ms, now) {
            Ok(entry) => {
                if let Err(e) = store.save(data) {
                    return Err(zbus::fdo::Error::Failed(
                        timetrack_proto::ServiceError::Storage(e.to_string()).to_failed_body(),
                    ));
                }
                serde_json::to_string(&timetrack_proto::entry_to_view(&entry))
                    .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
            }
            Err(rule) => Err(zbus::fdo::Error::Failed(
                timetrack_proto::ServiceError::from(rule).to_failed_body(),
            )),
        }
    }

    /// Undo through the real rule, like the service: only a quick-add may
    /// go; anything else is a typed refusal, never a silent delete.
    fn undo_quick_add_unit(&self, entry_id: &str) -> zbus::fdo::Result<()> {
        let mut g = self.inner.lock().unwrap();
        let FakeInner { data, store, .. } = &mut *g;
        let refused = |rule: timetrack_core::RuleError| {
            zbus::fdo::Error::Failed(timetrack_proto::ServiceError::from(rule).to_failed_body())
        };
        let (id, source) = data
            .get(entry_id)
            .map(|e| (e.id.clone(), e.source))
            .ok_or_else(|| {
                refused(timetrack_core::RuleError::UnknownEntry(
                    entry_id.to_string(),
                ))
            })?;
        if source != timetrack_core::EntrySource::QuickAdd {
            return Err(refused(timetrack_core::RuleError::NotQuickAdd(id)));
        }
        timetrack_core::delete_entry(data, &id).map_err(refused)?;
        if let Err(e) = store.save(data) {
            return Err(zbus::fdo::Error::Failed(
                timetrack_proto::ServiceError::Storage(e.to_string()).to_failed_body(),
            ));
        }
        Ok(())
    }

    /// Explicit deletion, like the service: no source check, just remove.
    /// Never reached via shorten -- the tray has no shorten row at all.
    fn delete_entry_unit(&self, entry_id: &str) -> zbus::fdo::Result<()> {
        let mut g = self.inner.lock().unwrap();
        let FakeInner { data, store, .. } = &mut *g;
        let id = data.get(entry_id).map(|e| e.id.clone()).ok_or_else(|| {
            zbus::fdo::Error::Failed(
                timetrack_proto::ServiceError::from(timetrack_core::RuleError::UnknownEntry(
                    entry_id.to_string(),
                ))
                .to_failed_body(),
            )
        })?;
        timetrack_core::delete_entry(data, &id).map_err(|rule| {
            zbus::fdo::Error::Failed(timetrack_proto::ServiceError::from(rule).to_failed_body())
        })?;
        if let Err(e) = store.save(data) {
            return Err(zbus::fdo::Error::Failed(
                timetrack_proto::ServiceError::Storage(e.to_string()).to_failed_body(),
            ));
        }
        Ok(())
    }
}

fn to_totals(t: &timetrack_core::Totals) -> timetrack_proto::TotalsView {
    timetrack_proto::TotalsView {
        total_ms: t.total_ms,
        per_project: t.per_project.clone(),
        entry_count: t.entry_count,
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

struct FakeIface(FakeService);

#[interface(name = "org.sequ.timetrack.Entries")]
impl FakeIface {
    #[zbus(name = "Snapshot")]
    fn snapshot(&self) -> String {
        self.0.snapshot_json()
    }

    #[zbus(name = "QuickAdd")]
    fn quick_add(&self, project_id: &str, duration_ms: i64) -> zbus::fdo::Result<String> {
        self.0.quick_add_json(project_id, duration_ms)
    }

    #[zbus(name = "UndoQuickAdd")]
    fn undo_quick_add(&self, entry_id: &str) -> zbus::fdo::Result<()> {
        self.0.undo_quick_add_unit(entry_id)
    }

    #[zbus(name = "DeleteEntry")]
    fn delete_entry(&self, entry_id: &str) -> zbus::fdo::Result<()> {
        self.0.delete_entry_unit(entry_id)
    }
}

#[test]
fn tray_menu_quick_add_undo_and_delete_round_trip() {
    async_io::block_on(async {
        let dir =
            std::env::temp_dir().join(format!("timetrack-tray-quickadd-{}", std::process::id()));
        let address = common::start_private_bus(&dir);
        common::point_at_private_bus(&address);

        let (conn, items) = common::serve_watcher(&address).await;

        let store_path = dir.join("store.json");
        let _service_conn = zbus::connection::Builder::address(address.as_str())
            .expect("private bus address must parse")
            .name(BUS_NAME)
            .expect("service name must be valid")
            .serve_at(OBJECT_PATH, FakeIface(FakeService::new(store_path.clone())))
            .expect("service path must be valid")
            .build()
            .await
            .expect("could not serve the fake service");

        let tray_log = std::fs::File::create(dir.join("tray.log")).unwrap();
        let _tray = ChildGuard(
            Command::new(env!("CARGO_BIN_EXE_timetrack-tray"))
                .env("DBUS_SESSION_BUS_ADDRESS", &address)
                .stdout(Stdio::null())
                .stderr(tray_log)
                .spawn()
                .expect("could not spawn timetrack-tray"),
        );

        // The item requests `org.kde.StatusNotifierItem-<pid>-<n>` and then
        // registers it; wait for the register call to land.
        let service = common::wait_for_registration(&items).await;

        // The tray polls every 5s; wait for its menu to offer the seeded
        // project with a +15m row, then click it.
        let menu = common::menu_proxy(&conn, &service).await;
        let plus_15m = common::wait_for_row(&menu, "+15m", false, Duration::from_secs(20)).await;
        common::click_row(&menu, plus_15m).await;

        // The tray's worker thread connects and quick-adds; the entry must
        // land in the store with the 15-minute bucket, tagged quick-add.
        let client = Client::connect()
            .await
            .expect("must connect to the fake service");
        let fifteen = 15 * 60_000;
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let snap = client.snapshot().await.expect("snapshot must succeed");
            if let Some(entry) = snap
                .entries
                .iter()
                .find(|e| e.project_id == "p1" && e.duration_ms() == fifteen)
            {
                assert_eq!(
                    entry.source,
                    EntrySource::QuickAdd,
                    "a menu quick-add is undoable, so it must be tagged: {entry:?}"
                );
                assert_eq!(
                    snap.week.per_project.get("p1"),
                    Some(&fifteen),
                    "the week aggregate must carry the new entry"
                );
                break;
            }
            if Instant::now() > deadline {
                panic!(
                    "no 15-minute quick-add landed ({} entries: {:?}; tray log at {})",
                    snap.entries.len(),
                    snap.entries,
                    dir.join("tray.log").display()
                );
            }
            async_io::Timer::after(Duration::from_millis(200)).await;
        }
        assert!(store_path.exists(), "the store file must have been written");

        // Tray-6 (#10): the submenu now offers an enabled undo row (the
        // poll loop rebuilds once the worker records the created id); click
        // it, and the entry just created must disappear again.
        let undo =
            common::wait_for_row(&menu, "Undo quick-add", true, Duration::from_secs(20)).await;
        common::click_row(&menu, undo).await;

        // The undo worker removes exactly the entry the quick-add created.
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let snap = client.snapshot().await.expect("snapshot must succeed");
            let gone = snap
                .entries
                .iter()
                .all(|e| !(e.project_id == "p1" && e.duration_ms() == fifteen));
            if gone {
                assert_eq!(
                    snap.week.per_project.get("p1").copied().unwrap_or(0),
                    0,
                    "the week aggregate must drop the undone entry"
                );
                break;
            }
            if Instant::now() > deadline {
                panic!(
                    "the quick-add was never undone ({} entries: {:?}; tray log at {})",
                    snap.entries.len(),
                    snap.entries,
                    dir.join("tray.log").display()
                );
            }
            async_io::Timer::after(Duration::from_millis(200)).await;
        }

        // Tray-7 (#11): quick-add again, then walk the delete confirmation.
        // The menu rebuilt since, so probe the +30m row afresh.
        let thirty = 30 * 60_000;
        let plus_30m = common::wait_for_row(&menu, "+30m", false, Duration::from_secs(20)).await;
        common::click_row(&menu, plus_30m).await;
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let snap = client.snapshot().await.expect("snapshot must succeed");
            if snap
                .entries
                .iter()
                .any(|e| e.project_id == "p1" && e.duration_ms() == thirty)
            {
                break;
            }
            if Instant::now() > deadline {
                panic!(
                    "no 30-minute quick-add landed ({} entries: {:?}; tray log at {})",
                    snap.entries.len(),
                    snap.entries,
                    dir.join("tray.log").display()
                );
            }
            async_io::Timer::after(Duration::from_millis(200)).await;
        }

        // Cancel path: the confirm submenu offers "Keep it"; clicking it
        // must delete nothing. The entry is still there afterwards, with
        // its week total intact.
        let keep_it = common::wait_for_row(&menu, "Keep it", true, Duration::from_secs(20)).await;
        common::click_row(&menu, keep_it).await;
        async_io::Timer::after(Duration::from_secs(2)).await;
        let snap = client.snapshot().await.expect("snapshot must succeed");
        assert!(
            snap.entries
                .iter()
                .any(|e| e.project_id == "p1" && e.duration_ms() == thirty),
            "cancelling the delete must keep the entry: {snap:?}"
        );
        assert_eq!(
            snap.week.per_project.get("p1"),
            Some(&thirty),
            "cancelling the delete must keep the week total"
        );

        // Delete round-trip: the confirm row states duration and project.
        // It was already there for the cancel probe; re-probe it since the
        // menu may have rebuilt under the click.
        let confirm = common::wait_for_row(
            &menu,
            "Delete 30m from General",
            true,
            Duration::from_secs(20),
        )
        .await;
        common::click_row(&menu, confirm).await;
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let snap = client.snapshot().await.expect("snapshot must succeed");
            let gone = snap
                .entries
                .iter()
                .all(|e| !(e.project_id == "p1" && e.duration_ms() == thirty));
            if gone {
                assert_eq!(
                    snap.week.per_project.get("p1").copied().unwrap_or(0),
                    0,
                    "the week aggregate must drop the deleted entry"
                );
                break;
            }
            if Instant::now() > deadline {
                panic!(
                    "the entry was never deleted ({} entries: {:?}; tray log at {})",
                    snap.entries.len(),
                    snap.entries,
                    dir.join("tray.log").display()
                );
            }
            async_io::Timer::after(Duration::from_millis(200)).await;
        }
    });
}
