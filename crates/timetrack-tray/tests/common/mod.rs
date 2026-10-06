//! Shared harness for the tray integration tests (`quick_add.rs` against a
//! fake service, `real_service.rs` against the real one): a private bus, a
//! minimal SNI watcher, and the dbusmenu click helpers. A desktop host would
//! do the same over `com.canonical.dbusmenu`.
//!
//! Each integration test is its own binary holding a single `#[test]`; the
//! `DBUS_SESSION_BUS_ADDRESS` switch below is safe only under that shape --
//! a second test in either binary would race it. Keep it that way.

use std::collections::HashMap;
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use zbus::zvariant::{OwnedValue, Value};

/// A private session bus, mirroring `scripts/e2e.sh`: hermetic, no host
/// activation, so a stale binary can never answer instead of the test.
const BUS_CONF: &str = r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-BUS Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:tmpdir=/tmp</listen>
  <policy context="default"><allow own="*"/><allow send_destination="*" eavesdrop="true"/><allow receive_sender
="*"/></policy>
</busconfig>
"#;

/// Just enough of `org.kde.StatusNotifierWatcher` for ksni to register:
/// the register/unregister methods plus the properties ksni reads.
pub struct Watcher {
    pub items: Arc<Mutex<Vec<String>>>,
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

/// Kill a child on the way out, including on assertion failure, so a red
/// test never leaves an orphan parking on the bus.
pub struct ChildGuard(pub Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Start a private bus for `dir`, returning its address.
pub fn start_private_bus(dir: &std::path::Path) -> String {
    std::fs::create_dir_all(dir).unwrap();
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
    address
}

/// Point this process (and, by inheritance, spawned children) at the private
/// bus. See the module docs for why this is safe here.
pub fn point_at_private_bus(address: &str) {
    // The proto `Client` connects to the session bus, so both the test
    // process and the tray/service children must see the private one.
    unsafe {
        std::env::set_var("DBUS_SESSION_BUS_ADDRESS", address);
    }
}

/// Serve the mock watcher on the private bus.
pub async fn serve_watcher(address: &str) -> (zbus::Connection, Arc<Mutex<Vec<String>>>) {
    let items: Arc<Mutex<Vec<String>>> = Arc::default();
    let conn = zbus::connection::Builder::address(address)
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
    (conn, items)
}

/// Wait for the tray item to register: it requests
/// `org.kde.StatusNotifierItem-<pid>-<n>` and registers it.
pub async fn wait_for_registration(items: &Arc<Mutex<Vec<String>>>) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(first) = items.lock().unwrap().first().cloned() {
            return first;
        }
        if Instant::now() > deadline {
            panic!("tray never registered with the test watcher");
        }
        async_io::Timer::after(Duration::from_millis(100)).await;
    }
}

/// A proxy for the item's dbusmenu interface.
pub async fn menu_proxy<'a>(conn: &'a zbus::Connection, service: &'a str) -> zbus::Proxy<'a> {
    zbus::Proxy::new(conn, service, "/MenuBar", "com.canonical.dbusmenu")
        .await
        .expect("menu proxy must build")
}

/// Whether a menu item's properties carry the wanted label.
pub fn label_is(props: &HashMap<String, OwnedValue>, want: &str) -> bool {
    props.get("label").is_some_and(|v| {
        let as_value: Value = v.clone().into();
        as_value == Value::new(want)
    })
}

/// Whether a menu item is clickable. ksni serves only non-default dbusmenu
/// properties, and the default is enabled -- so a missing `enabled` means
/// enabled, and only an explicit false disables.
pub fn enabled_is(props: &HashMap<String, OwnedValue>) -> bool {
    props.get("enabled").is_none_or(|v| {
        let as_value: Value = v.clone().into();
        as_value != Value::Bool(false)
    })
}

/// Wait up to `timeout` for a menu row with the wanted label, returning the
/// id a host would click. Ids are probed explicitly: unlike the empty id
/// list (which numbers items from zero), per-id properties carry ksni's
/// revision offset, so the id found is the one `Event` accepts -- clicking a
/// zero-based index misroutes once the menu has rebuilt.
pub async fn wait_for_row(
    menu: &zbus::Proxy<'_>,
    want: &str,
    enabled_only: bool,
    timeout: Duration,
) -> i32 {
    let deadline = Instant::now() + timeout;
    loop {
        let reply = menu
            .call_method(
                "GetGroupProperties",
                &(
                    (0..200).collect::<Vec<i32>>(),
                    vec!["label".to_string(), "enabled".to_string()],
                ),
            )
            .await
            .expect("the menu must answer GetGroupProperties");
        let props: Vec<(i32, HashMap<String, OwnedValue>)> = reply
            .body()
            .deserialize()
            .expect("menu properties must decode");
        if let Some((id, _)) = props
            .iter()
            .find(|(_, m)| label_is(m, want) && (!enabled_only || enabled_is(m)))
        {
            return *id;
        }
        if Instant::now() > deadline {
            panic!("tray menu never offered {want} (saw {} items)", props.len());
        }
        async_io::Timer::after(Duration::from_millis(200)).await;
    }
}

/// Click a menu row, as a desktop host would: `Event` with `clicked`.
pub async fn click_row(menu: &zbus::Proxy<'_>, id: i32) {
    let data: OwnedValue = Value::new("").try_into().expect("empty data must convert");
    menu.call_method("Event", &(id, "clicked".to_string(), data, 0u32))
        .await
        .expect("the menu must accept the click");
}
