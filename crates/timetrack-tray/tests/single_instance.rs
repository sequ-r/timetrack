//! Starting the app now starts the tray alongside the service (AppRun),
//! so a second launch must not stack a second tray icon: the extra
//! tray process exits at once and the first one's item stays the only
//! registration.

// The shared harness is wider than this test needs (no menu clicks).
#[allow(dead_code)]
mod common;

use common::ChildGuard;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn spawn_tray(address: &str, dir: &std::path::Path) -> ChildGuard {
    ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_timetrack-tray"))
            .env("DBUS_SESSION_BUS_ADDRESS", address)
            // §16: never the developer's real service socket (see common).
            .env("TIMETRACK_SOCKET", dir.join("tt.sock"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("could not spawn timetrack-tray"),
    )
}

#[test]
fn second_tray_exits_and_only_one_item_registers() {
    async_io::block_on(async {
        let dir =
            std::env::temp_dir().join(format!("timetrack-tray-single-{}", std::process::id()));
        let address = common::start_private_bus(&dir);
        let (_conn, items) = common::serve_watcher(&address).await;

        let mut first = spawn_tray(&address, &dir);
        common::wait_for_registration(&items).await;

        // The second copy must leave on its own, successfully: the name
        // is taken, and that is the designed outcome, not an error.
        let mut second = spawn_tray(&address, &dir);
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = second
                .0
                .try_wait()
                .expect("must be able to poll the second tray")
            {
                break status;
            }
            if Instant::now() > deadline {
                panic!("second tray never exited: it would stack a second tray icon");
            }
            async_io::Timer::after(Duration::from_millis(100)).await;
        };
        assert!(status.success(), "second tray exited uncleanly: {status}");

        // Only the first item ever registered, and it is still running.
        let registered = items.lock().unwrap().clone();
        assert_eq!(
            registered.len(),
            1,
            "expected exactly one tray item, saw {registered:?}"
        );
        assert!(
            first
                .0
                .try_wait()
                .expect("must be able to poll the first tray")
                .is_none(),
            "the first tray must still be running"
        );
    });
}
