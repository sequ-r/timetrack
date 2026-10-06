//! The tray registers with a `StatusNotifierWatcher` and exposes its icon
//! and tooltip over D-Bus (issue #7).
//!
//! A minimal watcher is served on a private bus, the real tray binary is
//! spawned against it, and the item's SNI properties are read back. No
//! timetrack service is needed: the tray must register even while the
//! service is absent (issues #6/#9).

use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A private session bus, mirroring `scripts/e2e.sh`: hermetic, no host
/// activation, so a stale binary can never answer instead of the test.
const BUS_CONF: &str = r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-BUS Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:tmpdir=/tmp</listen>
  <policy context="default"><allow own="*"/><allow send_destination="*" eavesdrop="true"/><allow receive_sender="*"/></policy>
</busconfig>
"#;

/// Just enough of `org.kde.StatusNotifierWatcher` for ksni to register:
/// the register/unregister methods plus the properties ksni reads.
struct Watcher {
    items: Arc<Mutex<Vec<String>>>,
}

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl Watcher {
    fn register_status_notifier_item(&self, service: &str) {
        self.items.lock().unwrap().push(service.to_string());
    }

    fn unregister_status_notifier_item(&self, service: &str) {
        self.items.lock().unwrap().retain(|s| s != service);
    }

    fn register_status_notifier_host(&self, _service: &str) {}

    fn unregister_status_notifier_host(&self, _service: &str) {}

    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        self.items.lock().unwrap().clone()
    }

    #[zbus(property)]
    fn is_status_notifier_host_registered(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn protocol_version(&self) -> i32 {
        0
    }
}

/// Kill the tray on the way out, including on assertion failure, so a red
/// test never leaves an orphan parking on the bus.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn tray_registers_with_watcher_and_exposes_icon_tooltip() {
    async_io::block_on(async {
        let dir = std::env::temp_dir().join(format!("timetrack-tray-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("bus.conf"), BUS_CONF).unwrap();

        let out = Command::new("dbus-daemon")
            .arg(format!("--config-file={}", dir.join("bus.conf").display()))
            .arg("--print-address")
            .arg("--fork")
            .output()
            .expect("dbus-daemon must be installed (see scripts/e2e.sh)");
        assert!(out.status.success(), "could not start a private bus");
        let address = String::from_utf8(out.stdout).unwrap();
        let address = address.trim().to_string();
        assert!(!address.is_empty(), "empty bus address");

        let items: Arc<Mutex<Vec<String>>> = Arc::default();
        let conn = zbus::connection::Builder::address(address.as_str())
            .expect("private bus address must parse")
            .name("org.kde.StatusNotifierWatcher")
            .expect("watcher name must be valid")
            .serve_at(
                "/StatusNotifierWatcher",
                Watcher {
                    items: items.clone(),
                },
            )
            .expect("watcher path must be valid")
            .build()
            .await
            .expect("could not serve the test watcher");

        let _tray = ChildGuard(
            Command::new(env!("CARGO_BIN_EXE_timetrack-tray"))
                .env("DBUS_SESSION_BUS_ADDRESS", &address)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("could not spawn timetrack-tray"),
        );

        // The item requests `org.kde.StatusNotifierItem-<pid>-<n>` and then
        // registers it; wait for the register call to land.
        let deadline = Instant::now() + Duration::from_secs(10);
        let service = loop {
            if let Some(first) = items.lock().unwrap().first().cloned() {
                break first;
            }
            if Instant::now() > deadline {
                panic!("tray never registered with the test watcher");
            }
            async_io::Timer::after(Duration::from_millis(100)).await;
        };
        assert!(
            service.starts_with("org.kde.StatusNotifierItem-"),
            "unexpected item name: {service}"
        );

        // Read the item back over D-Bus: this is the icon the user sees.
        let proxy = zbus::Proxy::new(
            &conn,
            service.as_str(),
            "/StatusNotifierItem",
            "org.kde.StatusNotifierItem",
        )
        .await
        .expect("item proxy must build");
        assert_eq!(
            proxy.get_property::<String>("Id").await.unwrap(),
            "org.sequ.timetrack.tray"
        );
        assert_eq!(
            proxy.get_property::<String>("Category").await.unwrap(),
            "ApplicationStatus"
        );
        assert_eq!(
            proxy.get_property::<String>("Title").await.unwrap(),
            "TimeTrack"
        );
        assert_eq!(
            proxy.get_property::<String>("Status").await.unwrap(),
            "Active"
        );
        assert_eq!(
            proxy.get_property::<String>("IconName").await.unwrap(),
            "org.sequ.timetrack-symbolic"
        );

        // `ToolTip` has no Deserialize impl, so decode the struct shape:
        // (icon-name, pixmap, title, description).
        let tip: zbus::zvariant::OwnedValue = proxy.get_property("ToolTip").await.unwrap();
        let tip: zbus::zvariant::Value = tip.into();
        let zbus::zvariant::Value::Structure(tip) = tip else {
            panic!("ToolTip is not a struct: {tip:?}");
        };
        let fields = tip.fields();
        assert_eq!(fields.len(), 4, "unexpected ToolTip shape");
        assert_eq!(
            fields[0],
            zbus::zvariant::Value::new("org.sequ.timetrack-symbolic")
        );
        assert_eq!(fields[2], zbus::zvariant::Value::new("TimeTrack"));
        assert_eq!(fields[3], zbus::zvariant::Value::new("Service running"));
    });
}
