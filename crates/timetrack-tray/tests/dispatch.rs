//! The neutral dispatch round-trips against the fake service directly:
//! no tray binary, no private bus, no dbusmenu. Every backend (ksni on
//! Linux, native menus on Windows/macOS in #155/#156) translates its row
//! clicks into [`TrayAction`](timetrack_tray::actions::TrayAction)s and
//! calls [`dispatch`](timetrack_tray::actions::dispatch); this pins that
//! contract on every target, reusing the fake `quick_add.rs` drives
//! through the menu.
//!
//! Cross-platform by design: no zbus, no dbus-daemon, only the socket
//! fake plus the library. Runs on win/mac CI as well as Linux.
//!
//! Single `#[test]` like the other tray suites: the `TIMETRACK_SOCKET`
//! switch below is safe only under that shape.

#[path = "common/fake_service.rs"]
mod fake_service;

use fake_service::{FakeService, SEED_ID, serve_fake};
use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};
use timetrack_proto::{Client, EntrySource};
use timetrack_tray::actions::{DispatchContext, TrayAction, dispatch};
use timetrack_tray::ipc::{set_shared_client, shared_client};

/// Wait until the fake holds an entry of `duration_ms` on `SEED_ID`,
/// returning its id.
async fn wait_for_entry(client: &Client, duration_ms: i64) -> String {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let snap = client.snapshot().await.expect("snapshot must succeed");
        if let Some(entry) = snap
            .entries
            .iter()
            .find(|e| e.project_id == SEED_ID && e.duration_ms() == duration_ms)
        {
            return entry.id.clone();
        }
        if Instant::now() > deadline {
            panic!("no {duration_ms}ms entry landed: {snap:?}");
        }
        async_io::Timer::after(Duration::from_millis(100)).await;
    }
}

/// Wait until no entry of `duration_ms` remains on `SEED_ID`.
async fn wait_for_gone(client: &Client, duration_ms: i64) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let snap = client.snapshot().await.expect("snapshot must succeed");
        if snap
            .entries
            .iter()
            .all(|e| !(e.project_id == SEED_ID && e.duration_ms() == duration_ms))
        {
            return;
        }
        if Instant::now() > deadline {
            panic!("the {duration_ms}ms entry never left: {snap:?}");
        }
        async_io::Timer::after(Duration::from_millis(100)).await;
    }
}

/// Wait until `action_error` becomes `Some`, returning its text.
fn wait_for_action_error(action_error: &Arc<Mutex<Option<String>>>) -> String {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(text) = action_error.lock().unwrap().clone() {
            return text;
        }
        if Instant::now() > deadline {
            panic!("the action error never landed");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn neutral_dispatch_quick_add_undo_delete_and_quit_round_trip() {
    async_io::block_on(async {
        // No private bus here: the socket alone, pointed into a fresh dir
        // so the rig never reaches a developer's real service.
        let dir =
            std::env::temp_dir().join(format!("timetrack-tray-dispatch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join(format!("tt-{}.sock", std::process::id()));
        unsafe {
            std::env::set_var("TIMETRACK_SOCKET", &sock);
        }

        let fake = Arc::new(FakeService::new(dir.join("store.json")));
        serve_fake(Arc::clone(&fake));

        let undo: Arc<Mutex<HashMap<String, String>>> = Arc::default();
        let shutdown = Arc::new(AtomicBool::new(false));
        let slot = Arc::new(Mutex::new(None));
        let action_error: Arc<Mutex<Option<String>>> = Arc::default();
        let ctx = DispatchContext::new(
            undo.clone(),
            shutdown.clone(),
            slot.clone(),
            action_error.clone(),
        );

        // The poll loop's half of #50: publish the live connection once,
        // and every dispatch below reuses it.
        let client = Client::connect()
            .await
            .expect("must connect to the fake service");
        set_shared_client(&slot, Some(client.clone()));
        assert!(shared_client(&slot).is_some());

        // QuickAdd lands tagged quick-add with the week aggregate, exactly
        // once, and records the undo target for this session.
        let fifteen = 15 * 60_000;
        dispatch(
            TrayAction::QuickAdd {
                project_id: SEED_ID.into(),
                duration_ms: fifteen,
            },
            &ctx,
        );
        let created = wait_for_entry(&client, fifteen).await;
        let snap = client.snapshot().await.expect("snapshot must succeed");
        let entry = snap
            .entries
            .iter()
            .find(|e| e.id == created)
            .expect("the entry just waited for must still be there");
        assert_eq!(
            entry.source,
            EntrySource::QuickAdd,
            "a dispatched quick-add is undoable, so it must be tagged: {entry:?}"
        );
        assert_eq!(snap.week.per_project.get(SEED_ID), Some(&fifteen));
        assert_eq!(fake.calls("QuickAdd"), 1);
        assert_eq!(
            undo.lock().unwrap().get(SEED_ID).cloned(),
            Some(created.clone()),
            "dispatch must record the created id for undo"
        );
        assert!(
            action_error.lock().unwrap().is_none(),
            "a successful action clears the failure slot"
        );

        // Undo removes exactly the created entry and drops the record.
        dispatch(
            TrayAction::Undo {
                project_id: SEED_ID.into(),
            },
            &ctx,
        );
        wait_for_gone(&client, fifteen).await;
        let snap = client.snapshot().await.expect("snapshot must succeed");
        assert_eq!(snap.week.per_project.get(SEED_ID).copied().unwrap_or(0), 0);
        assert_eq!(fake.calls("UndoQuickAdd"), 1);
        assert!(
            undo.lock().unwrap().get(SEED_ID).is_none(),
            "a finished undo drops the record so the row disables"
        );

        // Undo with no record is a no-op, not a failure: nothing to undo
        // for this project, and the failure slot stays clear.
        dispatch(
            TrayAction::Undo {
                project_id: "no-such-project".into(),
            },
            &ctx,
        );
        async_io::Timer::after(Duration::from_millis(500)).await;
        assert!(
            action_error.lock().unwrap().is_none(),
            "undo with no record must not fail the slot"
        );

        // DeleteEntry removes the named entry behind its confirm label.
        let thirty = 30 * 60_000;
        dispatch(
            TrayAction::QuickAdd {
                project_id: SEED_ID.into(),
                duration_ms: thirty,
            },
            &ctx,
        );
        let doomed = wait_for_entry(&client, thirty).await;
        dispatch(
            TrayAction::DeleteEntry {
                entry_id: doomed.clone(),
                confirm_label: "Delete 30m from General".to_string(),
            },
            &ctx,
        );
        wait_for_gone(&client, thirty).await;
        assert_eq!(fake.calls("DeleteEntry"), 1);

        // No connection: the worker refuses instead of opening one of its
        // own (#50), and the failure lands in the sticky slot.
        let lonely_undo: Arc<Mutex<HashMap<String, String>>> = Arc::default();
        let lonely_shutdown = Arc::new(AtomicBool::new(false));
        let lonely_slot = Arc::new(Mutex::new(None));
        let lonely_error: Arc<Mutex<Option<String>>> = Arc::default();
        let lonely = DispatchContext::new(
            lonely_undo.clone(),
            lonely_shutdown.clone(),
            lonely_slot.clone(),
            lonely_error.clone(),
        );
        dispatch(
            TrayAction::QuickAdd {
                project_id: SEED_ID.into(),
                duration_ms: fifteen,
            },
            &lonely,
        );
        let text = wait_for_action_error(&lonely_error);
        assert!(
            text.contains("not running"),
            "a dispatch with no connection reports NoService: {text}"
        );
        assert!(
            lonely_undo.lock().unwrap().is_empty(),
            "a failed quick-add records no undo target"
        );

        // OpenGui touches neither the quit flag nor the failure slot. The
        // PATH is emptied so the test never launches a real GUI even on a
        // machine with one installed: the spawn fails and the dispatcher
        // logs it, like the Linux backend always did.
        let saved_path = std::env::var_os("PATH");
        unsafe {
            std::env::set_var("PATH", &dir);
        }
        dispatch(TrayAction::OpenGui, &ctx);
        async_io::Timer::after(Duration::from_millis(500)).await;
        unsafe {
            match saved_path {
                Some(path) => std::env::set_var("PATH", path),
                None => std::env::remove_var("PATH"),
            }
        }
        assert!(
            !shutdown.load(Ordering::Relaxed),
            "opening the GUI must not quit the tray"
        );
        assert!(
            action_error.lock().unwrap().is_none(),
            "opening the GUI must not fail the slot"
        );

        // Quit flips the flag and nothing else: no service call, no failure.
        let calls_before =
            fake.calls("QuickAdd") + fake.calls("UndoQuickAdd") + fake.calls("DeleteEntry");
        assert!(!shutdown.load(Ordering::Relaxed));
        dispatch(TrayAction::Quit, &ctx);
        assert!(
            shutdown.load(Ordering::Relaxed),
            "dispatching Quit must request shutdown"
        );
        assert!(
            action_error.lock().unwrap().is_none(),
            "quitting never fails"
        );
        let calls_after =
            fake.calls("QuickAdd") + fake.calls("UndoQuickAdd") + fake.calls("DeleteEntry");
        assert_eq!(
            calls_before, calls_after,
            "quitting stops the tray only, never the service"
        );
    });
}
