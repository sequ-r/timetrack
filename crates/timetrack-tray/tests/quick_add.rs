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
//!
//! Linux only: the menu clicks travel over dbusmenu. `dispatch.rs` covers
//! the same fake without a bus on every target.

#![cfg(target_os = "linux")]

mod common;

#[path = "common/fake_service.rs"]
mod fake_service;

use common::ChildGuard;
use fake_service::{FakeService, SEED_ID, serve_fake};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use timetrack_proto::{Client, EntrySource};

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
                .find(|e| e.project_id == SEED_ID && e.duration_ms() == fifteen)
            {
                assert_eq!(
                    entry.source,
                    EntrySource::QuickAdd,
                    "a menu quick-add is undoable, so it must be tagged: {entry:?}"
                );
                assert_eq!(
                    snap.week.per_project.get(SEED_ID),
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
                .all(|e| !(e.project_id == SEED_ID && e.duration_ms() == fifteen));
            if gone {
                assert_eq!(
                    snap.week.per_project.get(SEED_ID).copied().unwrap_or(0),
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
                .any(|e| e.project_id == SEED_ID && e.duration_ms() == thirty)
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
                .any(|e| e.project_id == SEED_ID && e.duration_ms() == thirty),
            "cancelling the delete must keep the entry: {snap:?}"
        );
        assert_eq!(
            snap.week.per_project.get(SEED_ID),
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
                .all(|e| !(e.project_id == SEED_ID && e.duration_ms() == thirty));
            if gone {
                assert_eq!(
                    snap.week.per_project.get(SEED_ID).copied().unwrap_or(0),
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
