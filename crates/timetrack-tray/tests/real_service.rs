//! tray-14 (#18): the menu round-trips against the real service binary,
//! not the in-process fake in `quick_add.rs`.
//!
//! A private bus hosts the mock watcher, the real `timetrack-service` on a
//! fresh store (so it seeds `General` itself), and the real tray binary.
//! The test clicks `+15m` over D-Bus and asserts the entry lands with the
//! week aggregate; then it undoes it and deletes a `+30m` one, asserting the
//! tray menu rebuilds behind each step (the week-total header follows the
//! store). The fake covers the menu mechanics hermetically; this covers the
//! real wire, the real seeding, and real persistence.
//!
//! Needs the workspace binaries built (`cargo test --workspace` always does:
//! it compiles every bin to run their unit tests). A bare
//! `cargo test -p timetrack-tray` without a prior workspace build fails fast
//! with directions, rather than probing a stale install.

mod common;

use common::ChildGuard;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use timetrack_proto::{Client, EntrySource};

/// A workspace binary built for the same profile as this test: the test
/// executable lives in `<profile>/deps/`, so its grandparent is `<profile>/`,
/// next to every built binary.
fn workspace_bin(name: &str) -> std::path::PathBuf {
    let exe = std::env::current_exe().expect("test executable path must be known");
    let profile = exe
        .parent()
        .and_then(|deps| deps.parent())
        .expect("test executable must live in <profile>/deps");
    profile.join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
}

/// Wait until the real service answers snapshots on the private bus.
/// `Client::connect` is anonymous and succeeds with no service around; the
/// first method call is what proves the name is owned.
async fn wait_for_service(client: &Client, timeout: Duration) -> timetrack_proto::Snapshot {
    let deadline = Instant::now() + timeout;
    loop {
        match client.snapshot().await {
            Ok(snap) => return snap,
            Err(e) => {
                if Instant::now() > deadline {
                    panic!("real service never answered snapshots: {e}");
                }
                async_io::Timer::after(Duration::from_millis(200)).await;
            }
        }
    }
}

/// Wait until some entry with the wanted duration lands, returning the live
/// snapshot that carries it.
async fn wait_for_entry(client: &Client, duration_ms: i64, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let snap = client.snapshot().await.expect("snapshot must succeed");
        if snap
            .entries
            .iter()
            .any(|e| e.project_id == "p1" && e.duration_ms() == duration_ms)
        {
            return;
        }
        if Instant::now() > deadline {
            panic!(
                "no {duration_ms}ms entry landed ({} entries: {:?})",
                snap.entries.len(),
                snap.entries,
            );
        }
        async_io::Timer::after(Duration::from_millis(200)).await;
    }
}

#[test]
fn tray_round_trips_against_the_real_service() {
    async_io::block_on(async {
        let dir = std::env::temp_dir().join(format!("timetrack-tray-real-{}", std::process::id()));
        let address = common::start_private_bus(&dir);
        common::point_at_private_bus(&address);
        let (conn, items) = common::serve_watcher(&address).await;

        // A fresh store: the service seeds `General` itself, which is what
        // the tray menu must then offer.
        let store_path = dir.join("store.json");
        assert!(!store_path.exists(), "the store must start absent");
        let service_bin = workspace_bin("timetrack-service");
        assert!(
            service_bin.exists(),
            "no service binary at {}: run cargo build --workspace first",
            service_bin.display()
        );
        let svc_log = std::fs::File::create(dir.join("svc.log")).unwrap();
        let _svc = ChildGuard(
            Command::new(service_bin)
                .arg(&store_path)
                .env("DBUS_SESSION_BUS_ADDRESS", &address)
                .env("TZ", "UTC")
                .stdout(Stdio::null())
                .stderr(svc_log)
                .spawn()
                .expect("could not spawn timetrack-service"),
        );

        let client = Client::connect()
            .await
            .expect("must connect to the private bus");
        // The seed, through the real wire: one unarchived General, no time.
        let snap = wait_for_service(&client, Duration::from_secs(15)).await;
        assert_eq!(snap.projects.len(), 1);
        assert_eq!(snap.projects[0].name, "General");
        assert!(!snap.projects[0].archived);
        assert!(snap.entries.is_empty());
        assert!(store_path.exists(), "the service must persist its seed");

        let tray_log = std::fs::File::create(dir.join("tray.log")).unwrap();
        let _tray = ChildGuard(
            Command::new(env!("CARGO_BIN_EXE_timetrack-tray"))
                .env("DBUS_SESSION_BUS_ADDRESS", &address)
                .stdout(Stdio::null())
                .stderr(tray_log)
                .spawn()
                .expect("could not spawn timetrack-tray"),
        );
        let service = common::wait_for_registration(&items).await;
        let menu = common::menu_proxy(&conn, &service).await;

        // The seeded project reaches the menu: the tray read a real snapshot.
        common::wait_for_row(&menu, "General", false, Duration::from_secs(20)).await;

        // Quick-add through the real rules; the entry lands tagged quick-add
        // with the week aggregate carrying it.
        let fifteen = 15 * 60_000;
        let plus_15m = common::wait_for_row(&menu, "+15m", false, Duration::from_secs(20)).await;
        common::click_row(&menu, plus_15m).await;
        wait_for_entry(&client, fifteen, Duration::from_secs(15)).await;
        let snap = client.snapshot().await.expect("snapshot must succeed");
        let entry = snap
            .entries
            .iter()
            .find(|e| e.project_id == "p1" && e.duration_ms() == fifteen)
            .expect("the entry just waited for must still be there");
        assert_eq!(
            entry.source,
            EntrySource::QuickAdd,
            "a menu quick-add is undoable, so it must be tagged: {entry:?}"
        );
        assert_eq!(snap.week.per_project.get("p1"), Some(&fifteen));
        let undone_id = entry.id.clone();

        // The menu rebuilds behind the store: the week-total header follows.
        common::wait_for_row(&menu, "This week: 15m", false, Duration::from_secs(20)).await;

        // Undo removes exactly the created entry; the header drops back.
        let undo =
            common::wait_for_row(&menu, "Undo quick-add", true, Duration::from_secs(20)).await;
        common::click_row(&menu, undo).await;
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let snap = client.snapshot().await.expect("snapshot must succeed");
            if snap.entries.iter().all(|e| e.id != undone_id) {
                assert_eq!(snap.week.per_project.get("p1").copied().unwrap_or(0), 0);
                break;
            }
            if Instant::now() > deadline {
                panic!("the quick-add was never undone: {:?}", snap.entries);
            }
            async_io::Timer::after(Duration::from_millis(200)).await;
        }
        common::wait_for_row(&menu, "This week: 0m", false, Duration::from_secs(20)).await;

        // Delete with confirmation through the real store: +30m, then the
        // confirm row stating duration and project.
        let thirty = 30 * 60_000;
        let plus_30m = common::wait_for_row(&menu, "+30m", false, Duration::from_secs(20)).await;
        common::click_row(&menu, plus_30m).await;
        wait_for_entry(&client, thirty, Duration::from_secs(15)).await;
        common::wait_for_row(&menu, "This week: 30m", false, Duration::from_secs(20)).await;
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
            if snap.entries.iter().all(|e| e.duration_ms() != thirty) {
                assert_eq!(snap.week.per_project.get("p1").copied().unwrap_or(0), 0);
                break;
            }
            if Instant::now() > deadline {
                panic!("the entry was never deleted: {:?}", snap.entries);
            }
            async_io::Timer::after(Duration::from_millis(200)).await;
        }
    });
}
