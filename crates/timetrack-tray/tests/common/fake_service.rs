//! The fake service both tray suites share: a real `JsonStore` through
//! the real core rules over the local socket (§16), speaking the same
//! `Snapshot`/`QuickAdd`/`UndoQuickAdd`/`DeleteEntry` contract as the real
//! service. `quick_add.rs` drives it through the tray binary's dbusmenu
//! (Linux only); `dispatch.rs` drives it through the neutral dispatch
//! directly (every target, no dbusmenu, no private bus).

use std::sync::{Arc, Mutex};
use timetrack_proto::ServiceError;
use timetrack_proto::socket::{
    ConnStream, Frame, Listener, Request, Response, SOCKET_API_VERSION, read_frame, socket_path,
    write_frame,
};

/// The fake's seed project id: UUID-shaped like the real service's (#148),
/// fixed so the assertions below can name it.
pub const SEED_ID: &str = "aaaaaaaa-0000-4000-8000-000000000001";

/// The fake service's state: a real store file through the real core rules,
/// so a quick-add that lands here went through the same validation and
/// persistence as against the real service.
struct FakeInner {
    store: timetrack_core::JsonStore,
    data: timetrack_core::Store,
    ids: timetrack_core::Counter,
}

pub struct FakeService {
    inner: Mutex<FakeInner>,
    /// Method names in call order, so the test can tell the tray's
    /// clicks really reached this fake over the socket.
    calls: Mutex<Vec<String>>,
    /// `Changed` subscription senders, notified after each mutation
    /// like the real service's funnel.
    changed: Mutex<Vec<std::sync::mpsc::Sender<()>>>,
}

impl FakeService {
    /// A fresh store seeding `General`, like the real service: with no
    /// project there is nothing the tray menu could offer.
    pub fn new(path: std::path::PathBuf) -> Self {
        let store = timetrack_core::JsonStore::new(path);
        let mut data = store.load().unwrap_or_default();
        if data.projects.is_empty() {
            data.projects.push(timetrack_core::Project {
                id: SEED_ID.into(),
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

    pub fn calls(&self, method: &str) -> usize {
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
/// names): the same frames the real service answers, over per-call
/// connections plus subscriptions.
pub fn serve_fake(fake: Arc<FakeService>) {
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
