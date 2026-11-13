//! tray-5..7 (#9..#11): each project submenu offers +5/+15/+30/+60 rows,
//! an undo row, and a delete-confirm submenu -- activating them quick-adds
//! a bucket to the project in the service store, removes it again, and
//! deletes with confirmation.
//!
//! A private bus hosts a minimal watcher (as in `watcher.rs`) for the
//! tray *menu*; the service itself is a fake on the local socket (§16),
//! backed by a real `JsonStore` through the real core rules -- the same
//! `Snapshot`/`QuickAdd`/`UndoQuickAdd`/`DeleteEntry` contract the real
//! service speaks. (`real_service.rs` runs the same round-trips against
//! the real service binary.) The real tray binary runs against that
//! pair; the test clicks `+15m` over D-Bus (`com.canonical.dbusmenu`
//! `Event`, the same call a desktop host makes), asserts a 15-minute
//! `QuickAdd` entry lands for the seeded project, undoes it, quick-adds
//! `+30m`, cancels its delete (the entry stays), then confirms and
//! asserts it is gone.

mod common;

use common::ChildGuard;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use timetrack_proto::socket::{
    ConnStream, Frame, Listener, Request, Response, SOCKET_API_VERSION, read_frame, socket_path,
    write_frame,
};
use timetrack_proto::{Client, EntrySource, ServiceError};

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
    /// Method names in call order, so the test can tell the tray's
    /// clicks really reached this fake over the socket.
    calls: Mutex<Vec<String>>,
    /// `Changed` subscription senders, notified after each mutation
    /// like the real service's funnel.
    changed: Mutex<Vec<std::sync::mpsc::Sender<()>>>,
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
            calls: Mutex::new(Vec::new()),
            changed: Mutex::new(Vec::new()),
        };
        {
            let g = svc.inner.lock().unwrap();
            g.store.save(&g.data).expect("the seed must persist");
        }
        svc
    }

    fn record(&self, method: &str) {
        self.calls.lock().unwrap().push(method.to_string());
    }

    fn calls(&self, method: &str) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|m| *m == method)
            .count()
    }

    fn notify_changed(&self) {
        self.changed
            .lock()
            .unwrap()
            .retain(|tx| tx.send(()).is_ok());
    }

    fn subscribe(&self) -> std::sync::mpsc::Receiver<()> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.changed.lock().unwrap().push(tx);
        rx
    }

    /// Everything the tray's poll loop reads: projects plus this week's
    /// per-project totals from the service aggregates, never re-summed.
    fn snapshot(&self) -> timetrack_proto::Snapshot {
        let g = self.inner.lock().unwrap();
        let now = now_ms();
        let tz = timetrack_core::Tz::utc();
        let today = tz.day_of(now);
        let week = timetrack_core::week_totals(&g.data, today, &tz);
        let month = timetrack_core::month_totals(&g.data, today, &tz);
        let all = timetrack_core::all_totals(&g.data);
        timetrack_proto::Snapshot {
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
        }
    }

    fn quick_add(
        &self,
        project_id: &str,
        duration_ms: i64,
    ) -> Result<timetrack_proto::EntryView, ServiceError> {
        let mut g = self.inner.lock().unwrap();
        let now = now_ms();
        let FakeInner { data, ids, store } = &mut *g;
        let entry = timetrack_core::quick_add(data, ids, project_id, duration_ms, now)?;
        store
            .save(data)
            .map_err(|e| ServiceError::Storage(e.to_string()))?;
        Ok(timetrack_proto::entry_to_view(&entry))
    }

    fn undo_quick_add(&self, entry_id: &str) -> Result<(), ServiceError> {
        let mut g = self.inner.lock().unwrap();
        let FakeInner { data, store, .. } = &mut *g;
        let (id, source) = data
            .get(entry_id)
            .map(|e| (e.id.clone(), e.source))
            .ok_or_else(|| {
                ServiceError::from(timetrack_core::RuleError::UnknownEntry(
                    entry_id.to_string(),
                ))
            })?;
        if source != timetrack_core::EntrySource::QuickAdd {
            return Err(ServiceError::from(timetrack_core::RuleError::NotQuickAdd(
                id,
            )));
        }
        timetrack_core::delete_entry(data, &id)?;
        store
            .save(data)
            .map_err(|e| ServiceError::Storage(e.to_string()))?;
        Ok(())
    }

    fn delete_entry(&self, entry_id: &str) -> Result<(), ServiceError> {
        let mut g = self.inner.lock().unwrap();
        let FakeInner { data, store, .. } = &mut *g;
        timetrack_core::delete_entry(data, entry_id)?;
        store
            .save(data)
            .map_err(|e| ServiceError::Storage(e.to_string()))?;
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

/// Serve the fake on the rig socket (the path `TIMETRACK_SOCKET`
/// names, set by `common::point_at_private_bus`): the same frames the
/// real service answers, over per-call connections plus subscriptions.
fn serve_fake(fake: Arc<FakeService>) {
    let path = socket_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::remove_file(&path);
    let listener = Listener::bind(&path).expect("fake service must bind its socket");
    std::thread::spawn(move || {
        loop {
            let Ok(stream) = listener.accept() else {
                continue;
            };
            let fake = Arc::clone(&fake);
            std::thread::spawn(move || handle_fake_conn(stream, fake));
        }
    });
}

fn handle_fake_conn(mut stream: ConnStream, fake: Arc<FakeService>) {
    let Ok(Some(Frame::Request(request))) = read_frame(&mut stream) else {
        return;
    };
    if request == Request::SubscribeChanged {
        let rx = fake.subscribe();
        if write_frame(&mut stream, &Frame::Reply(Ok(Response::Subscribed))).is_err() {
            return;
        }
        while rx.recv().is_ok() {
            if write_frame(&mut stream, &Frame::Changed).is_err() {
                return;
            }
        }
        return;
    }
    let (name, reply): (&str, Result<Response, ServiceError>) = match &request {
        Request::ApiVersion => ("ApiVersion", Ok(Response::ApiVersion(SOCKET_API_VERSION))),
        Request::Snapshot => (
            "Snapshot",
            Ok(Response::Snapshot(Box::new(fake.snapshot()))),
        ),
        Request::QuickAdd {
            project_id,
            duration_ms,
        } => (
            "QuickAdd",
            fake.quick_add(project_id, *duration_ms)
                .map(|e| Response::Entry(Box::new(e))),
        ),
        Request::UndoQuickAdd { id } => (
            "UndoQuickAdd",
            fake.undo_quick_add(id).map(|()| Response::Unit),
        ),
        Request::DeleteEntry { id } => (
            "DeleteEntry",
            fake.delete_entry(id).map(|()| Response::Unit),
        ),
        other => (
            "other",
            Err(ServiceError::Other(format!(
                "the fake service does not speak {other:?}"
            ))),
        ),
    };
    fake.record(name);
    let mutated = reply.is_ok() && !matches!(name, "ApiVersion" | "Snapshot");
    let _ = write_frame(&mut stream, &Frame::Reply(reply));
    if mutated {
        fake.notify_changed();
    }
}

#[test]
fn tray_menu_quick_add_undo_and_delete_round_trip() {
    async_io::block_on(async {
        let dir =
            std::env::temp_dir().join(format!("timetrack-tray-quickadd-{}", std::process::id()));
        let address = common::start_private_bus(&dir);
        common::point_at_private_bus(&address, &dir);

        let (conn, items) = common::serve_watcher(&address).await;

        let store_path = dir.join("store.json");
        let fake = Arc::new(FakeService::new(store_path.clone()));
        serve_fake(Arc::clone(&fake));

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

        // Wait for the tray's menu to offer the seeded project with a
        // +15m row, then click it.
        let menu = common::menu_proxy(&conn, &service).await;
        common::click_when_ready(&menu, "+15m", false, Duration::from_secs(20)).await;

        // The tray's worker thread quick-adds over the connection the tray
        // already holds; the entry must land in the store with the
        // 15-minute bucket, tagged quick-add.
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

        // The click reached this fake exactly once as a QuickAdd, over
        // the socket (one connection per call is the transport's shape
        // now, so connection identity -- the old bus-era #50 assertion
        // -- is replaced by the call count itself).
        assert_eq!(
            fake.calls("QuickAdd"),
            1,
            "one click must make exactly one quick-add call"
        );

        // Tray-6 (#10): the submenu now offers an enabled undo row (the
        // refresh loop rebuilds once the worker records the created id); click
        // it, and the entry just created must disappear again.
        common::click_when_ready(&menu, "Undo quick-add", true, Duration::from_secs(20)).await;

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
        common::click_when_ready(&menu, "+30m", false, Duration::from_secs(20)).await;
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
        common::click_when_ready(&menu, "Keep it", true, Duration::from_secs(20)).await;
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
        common::click_when_ready(
            &menu,
            "Delete 30m from General",
            true,
            Duration::from_secs(20),
        )
        .await;
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
