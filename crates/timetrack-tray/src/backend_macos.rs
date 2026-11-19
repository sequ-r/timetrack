/* tray/backend_macos.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The macOS menu-bar backend (#156): an `NSStatusItem` extra showing the
//! week total as its title, a status-coloured icon, a tooltip, and a native
//! menu that mirrors the Linux structure, routed through the neutral
//! [`dispatch`].
//!
//! The same `tray-icon` family as the Windows backend (#155): `tray-icon`
//! covers macOS cleanly (status item plus title via `objc2`, menu via
//! `muda`), while this module drives the `NSApplication` event loop
//! directly via `objc2` / `objc2-app-kit` / `objc2-foundation`. The
//! menu-id wire format, the menu structure, and the generated icon bytes
//! are shared with `backend_windows` — one stack, one set of rows — while
//! this module adds only what macOS owns: the title text, the `flock`
//! single-instance claim (identical for raw binaries and future bundles,
//! since the lock path never depends on the executable's location), and
//! the runner.
//!
//! Like the Windows backend, the pure mapping compiles on every target so
//! the headless unit tests below pin it without a desktop. Only the runner
//! — the status item, the AppKit event loop, the poll worker, and the lock
//! claim — is `cfg(target_os = "macos")`, with its `tray-icon`/`objc2`/
//! `libc` dependencies gated the same way in the manifest.
//!
//! [`dispatch`]: crate::actions::dispatch

#[cfg(target_os = "macos")]
use crate::backend_windows::{ICON_PX, icon_rgba};
#[cfg(any(target_os = "macos", test))]
use crate::backend_windows::{WinMenuItem, build_window_menu, decode_action, parse_keep_id};
#[cfg(test)]
use crate::backend_windows::{encode_action, keep_id};
#[cfg(target_os = "macos")]
use crate::menu::MenuModel;
#[cfg(target_os = "macos")]
use crate::state::{NeutralStatus, presentation};
use crate::state::{TooltipState, Unwell};
#[cfg(any(target_os = "macos", test))]
use std::collections::HashMap;
#[cfg(target_os = "macos")]
use std::sync::{Arc, Mutex};

/// The fallback snapshot cadence, in seconds: the same 60 s tick as the
/// Linux poll loop and the Windows worker. Freshness normally arrives as a
/// `Changed` announcement the moment a mutation persists; this only covers
/// a missed signal and noticing that the service has gone away.
pub const POLL_SECS: u64 = 60;

/// Maximum wait before the UI event loop re-checks sticky action errors
/// even when neither AppKit input nor a worker publish arrived, in
/// milliseconds.
pub const UI_TICK_MS: u32 = crate::backend_windows::UI_TICK_MS;

/// The menu-bar title: this ISO week's total in the core's compact form
/// (`1h 30m`), the same spelling as the menu rows beside it.
/// A missing service flattens the title to a dash — the summary would
/// otherwise read as a tracked total of zero — while a skew or a refused
/// snapshot keeps the last good total, with the tooltip carrying the
/// explanation, exactly like the attention-via-status rule in
/// [`presentation`](crate::state::presentation).
pub fn status_title(summary: &TooltipState, unwell: Option<&Unwell>) -> String {
    match unwell {
        Some(Unwell::Down) => "–".to_string(),
        Some(Unwell::Skewed) | Some(Unwell::Refused(_)) | None => {
            timetrack_core::format_compact(summary.total_ms)
        }
    }
}

/// The single-instance lock file name, derived from [`crate::TRAY_NAME`]
/// so the backends cannot drift apart.
pub fn single_instance_lock_file_name() -> String {
    format!("{}.lock", crate::TRAY_NAME)
}

/// Where the single-instance lock lives: the per-user temporary directory
/// plus [`single_instance_lock_file_name`]. Raw binaries and future app
/// bundles share one `$TMPDIR` per user, so both claim the same lock and
/// behave as one single instance either way.
pub fn single_instance_lock_path() -> std::path::PathBuf {
    std::env::temp_dir().join(single_instance_lock_file_name())
}

/// Whether the Open row's target exists. A standalone tray install may
/// not ship the sibling GUI binary, so the runner logs its presence once
/// at startup and clicks degrade gracefully through neutral dispatch's
/// spawn-failure log.
pub fn gui_program_present(program: &str) -> bool {
    !program.is_empty() && std::path::Path::new(program).is_file()
}

/// The single-instance lock, held for the process's whole run (macOS).
/// Dropping it (on Quit, or on death) releases the `flock`, exactly like
/// the Linux bus-name claim — and unlike a create-only lock file, a crash
/// can never leave a stale claim behind.
#[cfg(target_os = "macos")]
pub struct SingleInstance {
    _file: std::fs::File,
}

#[cfg(target_os = "macos")]
impl SingleInstance {
    /// Claim the tray's lock. `None` means a tray is already running —
    /// the designed outcome of a repeated launch, not an error.
    pub fn try_claim() -> anyhow::Result<Option<Self>> {
        use std::os::unix::io::AsRawFd as _;
        let path = single_instance_lock_path();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| anyhow::anyhow!("could not open the single-instance lock: {e}"))?;
        // `flock` is per-open-file-description: the held `File` keeps the
        // claim until it drops, and the kernel releases it on death.
        // `EWOULDBLOCK` (a held lock) sorts by kind, never by text.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            anyhow::bail!("could not claim the single-instance lock: {err}");
        }
        Ok(Some(Self { _file: file }))
    }
}

/// What the poll worker publishes to the UI thread: the same four inputs
/// the Linux loop gates on (menu, tooltip summary, connection health, and
/// the undo map for the gate), sent by value only when one changes.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone)]
struct Published {
    menu: MenuModel,
    summary: TooltipState,
    unwell: Option<Unwell>,
}

/// Sentinel subtype identifying synthetic wake events posted to AppKit.
#[cfg(target_os = "macos")]
const WAKE_EVENT_SUBTYPE: std::ffi::c_short = 0x5454;

/// Sentinel data1 payload distinguishing our wake events from unrelated
/// `ApplicationDefined` events.
#[cfg(target_os = "macos")]
const WAKE_EVENT_DATA1: objc2_foundation::NSInteger = 0x5454_5741;

/// Build a synthetic `NSEvent` of type `ApplicationDefined` carrying our wake
/// sentinel so `nextEventMatchingMask_untilDate_inMode_dequeue` unblocks.
#[cfg(target_os = "macos")]
fn build_wake_event() -> Option<objc2::rc::Retained<objc2_app_kit::NSEvent>> {
    use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSEventType};
    use objc2_foundation::NSPoint;
    NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
        NSEventType::ApplicationDefined,
        NSPoint::ZERO,
        NSEventModifierFlags::empty(),
        0.0,
        0,
        None,
        WAKE_EVENT_SUBTYPE,
        WAKE_EVENT_DATA1,
        0,
    )
}

/// Return whether `event` is our synthetic wake sentinel.
#[cfg(target_os = "macos")]
fn is_wake_event(event: &objc2_app_kit::NSEvent) -> bool {
    // IMPORTANT: short-circuit on `r#type() == ApplicationDefined` first;
    // AppKit throws NSInternalInconsistencyException if `subtype` or `data1`
    // is read on non-custom event types.
    event.r#type() == objc2_app_kit::NSEventType::ApplicationDefined
        && event.subtype().0 == WAKE_EVENT_SUBTYPE
        && event.data1() == WAKE_EVENT_DATA1
}

/// Thread-safe handle for waking the AppKit event loop from the poll worker.
#[cfg(target_os = "macos")]
struct MacWake {
    app: std::ptr::NonNull<objc2_app_kit::NSApplication>,
}

// SAFETY: `NSApplication::sharedApplication` is a process-lifetime singleton
// created before `MacWake`, and Apple documents `-[NSApplication postEvent:atStart:]`
// as thread-safe from background threads.
#[cfg(target_os = "macos")]
unsafe impl Send for MacWake {}
// SAFETY: See `Send` impl above.
#[cfg(target_os = "macos")]
unsafe impl Sync for MacWake {}

#[cfg(target_os = "macos")]
impl MacWake {
    fn new(app: &objc2_app_kit::NSApplication) -> Self {
        Self {
            app: std::ptr::NonNull::from(app),
        }
    }

    fn signal(&self) {
        objc2::rc::autoreleasepool(|_| {
            if let Some(event) = build_wake_event() {
                // SAFETY: `self.app` points to the process-wide `NSApplication`
                // singleton and `postEvent_atStart` is safe from any thread.
                let app = unsafe { self.app.as_ref() };
                app.postEvent_atStart(&event, false);
            }
        });
    }
}

/// Worker-to-UI channel plus its AppKit wake handle so published updates
/// interrupt `nextEventMatchingMask_untilDate_inMode_dequeue` immediately.
#[cfg(target_os = "macos")]
struct MacNotifier {
    tx: Option<std::sync::mpsc::Sender<Published>>,
    wake: Arc<MacWake>,
}

#[cfg(target_os = "macos")]
impl MacNotifier {
    fn new(tx: std::sync::mpsc::Sender<Published>, wake: Arc<MacWake>) -> Self {
        Self { tx: Some(tx), wake }
    }

    fn send(&self, published: Published) -> bool {
        let Some(tx) = self.tx.as_ref() else {
            return false;
        };
        if tx.send(published).is_err() {
            return false;
        }
        self.wake.signal();
        true
    }
}

#[cfg(target_os = "macos")]
impl Drop for MacNotifier {
    fn drop(&mut self) {
        // Drop the sender before waking so the main thread observes
        // `Disconnected` immediately when unblocked.
        self.tx.take();
        self.wake.signal();
    }
}

/// Drain every pending menu click produced by `muda` on the main thread.
#[cfg(target_os = "macos")]
fn drain_menu_events(ctx: &crate::actions::DispatchContext) {
    use crate::actions::dispatch;
    use tray_icon::menu::MenuEvent;
    while let Ok(event) = MenuEvent::receiver().try_recv() {
        let id = event.id.as_ref();
        if let Some(action) = decode_action(id) {
            dispatch(action, ctx);
        } else if let Some(entry_id) = parse_keep_id(id) {
            println!("timetrack-tray: delete of {entry_id} cancelled");
        } else {
            eprintln!("timetrack-tray: ignoring unknown menu id");
        }
    }
}

/// Apply the latest worker snapshot if one arrived, and report whether the
/// worker channel disconnected.
#[cfg(target_os = "macos")]
fn drain_updates(
    rx: &std::sync::mpsc::Receiver<Published>,
    tray: &tray_icon::TrayIcon,
    last_quick_add: &Arc<Mutex<HashMap<String, String>>>,
    last: &mut Published,
) -> bool {
    let mut latest = None;
    let mut disconnected = false;
    loop {
        match rx.try_recv() {
            Ok(published) => latest = Some(published),
            Err(std::sync::mpsc::TryRecvError::Empty) => break,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                disconnected = true;
                break;
            }
        }
    }
    if let Some(published) = latest {
        match build_native_menu(&build_window_menu(
            &published.menu,
            &crate::lock(last_quick_add),
        )) {
            Ok(menu) => tray.set_menu(Some(Box::new(menu))),
            Err(e) => eprintln!("timetrack-tray: could not rebuild the menu: {e}"),
        }
        *last = published;
    }
    disconnected
}

/// Run the macOS tray: claim single instance, initialize AppKit, show the
/// status item, and pump AppKit events, menu clicks, and worker publishes
/// until Quit. Blocks the calling thread, which must be the main thread —
/// `tray-icon` builds the `NSStatusItem` there and refuses (`NotMainThread`)
/// anywhere else, so `main` calls this directly, never under `block_on` or
/// on a worker. The service polling lives on a worker thread, like the Linux
/// poll loop, and clicks dispatch the neutral [`TrayAction`]s.
///
/// [`TrayAction`]: crate::actions::TrayAction
#[cfg(target_os = "macos")]
pub fn run() -> anyhow::Result<()> {
    use crate::actions::DispatchContext;
    use crate::ipc::set_shared_client;
    use objc2::MainThreadMarker;
    use objc2::rc::autoreleasepool;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSEventMask};
    use objc2_foundation::{NSDate, NSDefaultRunLoopMode};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };
    use tray_icon::{Icon, TrayIconBuilder};

    let Some(_instance) = SingleInstance::try_claim()? else {
        println!("timetrack-tray: already running; exiting");
        return Ok(());
    };

    // Say once at startup whether the Open row's target is present beside
    // the tray; clicks degrade gracefully to neutral dispatch's
    // spawn-failure log when the GUI binary is absent.
    let argv = crate::platform::open_gui_command();
    let program = argv.first().cloned().unwrap_or_default();
    if gui_program_present(&program) {
        println!("timetrack-tray: GUI at {program}");
    } else {
        eprintln!("timetrack-tray: no GUI at {program}; the Open row will report it");
    }

    let mtm = MainThreadMarker::new()
        .ok_or_else(|| anyhow::anyhow!("timetrack-tray must run on the main thread"))?;
    let app = NSApplication::sharedApplication(mtm);
    let _ = app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    app.finishLaunching();

    let last_quick_add = Arc::new(Mutex::new(HashMap::new()));
    let shutdown = Arc::new(AtomicBool::new(false));
    let client_slot = Arc::new(Mutex::new(None));
    let action_error = Arc::new(Mutex::new(None));
    let ctx = DispatchContext::new(
        last_quick_add.clone(),
        shutdown.clone(),
        client_slot.clone(),
        action_error.clone(),
    );

    // The starting item mirrors a fresh Linux item: idle-active over the
    // zero summary, replaced by the first publish within a tick. The icon
    // stays a colour icon (not a template), so the three status colours
    // show; the title carries the week total beside it.
    let summary = TooltipState::default();
    let (status, description) = presentation(&summary, None, None);
    let title = status_title(&summary, None);
    let icon = Icon::from_rgba(icon_rgba(status), ICON_PX, ICON_PX)
        .map_err(|e| anyhow::anyhow!("could not build the tray icon: {e}"))?;
    let menu = build_native_menu(&build_window_menu(&MenuModel::default(), &HashMap::new()))?;
    let tray = TrayIconBuilder::new()
        .with_id(crate::TRAY_NAME)
        .with_tooltip(description)
        .with_title(title)
        .with_icon(icon)
        .with_menu(Box::new(menu))
        .build()
        .map_err(|e| anyhow::anyhow!("could not register tray icon: {e}"))?;
    println!("timetrack-tray: registered {}", crate::TRAY_NAME);

    let wake = Arc::new(MacWake::new(&app));
    let (tx, rx) = std::sync::mpsc::channel::<Published>();
    let notifier = MacNotifier::new(tx, wake);
    // The poll worker: the Linux loop's twin, publishing state instead of
    // pushing it into a ksni handle. Detached like every dispatch worker:
    // Quit drops the process, and the flag stops the loop first anyway.
    let worker_undo = last_quick_add.clone();
    let worker_shutdown = shutdown.clone();
    let worker_slot = client_slot.clone();
    std::thread::spawn(move || {
        async_io::block_on(poll_worker(
            notifier,
            worker_undo,
            worker_shutdown,
            worker_slot,
        ));
    });

    // The last publish applied, plus the last item state pushed: the icon,
    // tooltip, and title only move on a real change, while `UI_TICK_MS`
    // re-reads the sticky action error so a failed click surfaces within
    // a beat even when neither AppKit input nor a snapshot changed (ksni
    // re-renders on demand; here the UI thread must push).
    let mut last = Published {
        menu: MenuModel::default(),
        summary: TooltipState::default(),
        unwell: None,
    };
    let mut last_pushed: Option<(NeutralStatus, String, String)> = None;
    let tick_secs = f64::from(UI_TICK_MS) / 1000.0;
    loop {
        let mut disconnected = false;
        autoreleasepool(|_| {
            drain_menu_events(&ctx);
            disconnected = drain_updates(&rx, &tray, &last_quick_add, &mut last);
            push_status(&tray, &last, &action_error, &mut last_pushed);
        });
        if disconnected {
            break;
        }
        if shutdown.load(Ordering::Relaxed) {
            // Quit stops the tray only: clear the shared slot without
            // calling the service, exactly like the Linux return path.
            set_shared_client(&client_slot, None);
            println!("timetrack-tray: quit requested; exiting");
            break;
        }
        autoreleasepool(|_| {
            // SAFETY: `NSDefaultRunLoopMode` is an immutable AppKit/Foundation
            // constant string valid for the lifetime of the process.
            let mode = unsafe { NSDefaultRunLoopMode };
            let deadline = NSDate::dateWithTimeIntervalSinceNow(tick_secs);
            if let Some(event) = app.nextEventMatchingMask_untilDate_inMode_dequeue(
                NSEventMask::Any,
                Some(&deadline),
                mode,
                true,
            ) {
                if !is_wake_event(&event) {
                    app.sendEvent(&event);
                }
                let past = NSDate::distantPast();
                while let Some(event) = app.nextEventMatchingMask_untilDate_inMode_dequeue(
                    NSEventMask::Any,
                    Some(&past),
                    mode,
                    true,
                ) {
                    if !is_wake_event(&event) {
                        app.sendEvent(&event);
                    }
                }
                app.updateWindows();
            }
        });
    }
    Ok(())
}

/// Push the icon state plus the tooltip body and the menu-bar title when
/// any of them changed. The tooltip passes through unclamped: the 127-char
/// cap is a Win32 `NOTIFYICONDATA` limit, not a status-item one.
#[cfg(target_os = "macos")]
fn push_status(
    tray: &tray_icon::TrayIcon,
    last: &Published,
    action_error: &Arc<Mutex<Option<String>>>,
    last_pushed: &mut Option<(NeutralStatus, String, String)>,
) {
    let failed = crate::lock(action_error).clone();
    let (status, description) =
        presentation(&last.summary, last.unwell.as_ref(), failed.as_deref());
    let title = status_title(&last.summary, last.unwell.as_ref());
    if last_pushed.as_ref() == Some(&(status, description.clone(), title.clone())) {
        return;
    }
    *last_pushed = Some((status, description.clone(), title.clone()));
    match tray_icon::Icon::from_rgba(icon_rgba(status), ICON_PX, ICON_PX) {
        Ok(icon) => {
            if let Err(e) = tray.set_icon(Some(icon)) {
                eprintln!("timetrack-tray: could not set the tray icon: {e}");
            }
        }
        Err(e) => eprintln!("timetrack-tray: could not build the tray icon: {e}"),
    }
    if let Err(e) = tray.set_tooltip(Some(description)) {
        eprintln!("timetrack-tray: could not set the tooltip: {e}");
    }
    tray.set_title(Some(title));
}

/// Turn a [`build_window_menu`] model into a live native menu. Every action
/// row carries its shared menu id; submenus only group.
#[cfg(target_os = "macos")]
fn build_native_menu(items: &[WinMenuItem]) -> anyhow::Result<tray_icon::menu::Menu> {
    use tray_icon::menu::{Menu, MenuItem, Submenu};
    let menu = Menu::new();
    append_items(&menu, items).map_err(|e| anyhow::anyhow!("could not build the menu: {e}"))?;
    return Ok(menu);

    fn append_items(parent: &Menu, items: &[WinMenuItem]) -> Result<(), tray_icon::menu::Error> {
        for item in items {
            match item {
                WinMenuItem::Action { id, label, enabled } => {
                    parent.append(&MenuItem::with_id(id.clone(), label, *enabled, None))?;
                }
                WinMenuItem::Submenu { label, items } => {
                    let submenu = Submenu::new(label, true);
                    append_submenu(&submenu, items)?;
                    parent.append(&submenu)?;
                }
            }
        }
        return Ok(());

        fn append_submenu(
            parent: &Submenu,
            items: &[WinMenuItem],
        ) -> Result<(), tray_icon::menu::Error> {
            for item in items {
                match item {
                    WinMenuItem::Action { id, label, enabled } => {
                        parent.append(&MenuItem::with_id(id.clone(), label, *enabled, None))?;
                    }
                    WinMenuItem::Submenu { label, items } => {
                        let submenu = Submenu::new(label, true);
                        append_submenu(&submenu, items)?;
                        parent.append(&submenu)?;
                    }
                }
            }
            Ok(())
        }
    }
}

/// The poll worker: the Linux `run` loop's twin. It (re)connects
/// anonymously, publishes the live client in the shared slot for the menu
/// workers, snapshots, and sends a [`Published`] only when the menu, the
/// summary, the undo map, or the connection health actually changed.
/// Failures sort by `ClientError` variant, never by message text; a dead
/// connection is dropped so the next tick reconnects from scratch.
#[cfg(target_os = "macos")]
async fn poll_worker(
    notifier: MacNotifier,
    last_quick_add: Arc<Mutex<HashMap<String, String>>>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    client_slot: crate::ipc::SharedClient,
) {
    use crate::ipc::{set_shared_client, shared_client};
    use crate::menu::build_menu_model;
    use crate::state::{ConnState, TooltipState, conn_state, unwell_of};
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use timetrack_proto::{ChangedStream, ChangedWait, Client, ClientError};

    const POLL: Duration = Duration::from_secs(POLL_SECS);

    let mut last_state: Option<ConnState> = None;
    let mut last_total_ms: Option<i64> = None;
    let mut last_menu = MenuModel::default();
    let mut last_undo: HashMap<String, String> = HashMap::new();
    let mut last_summary = TooltipState::default();
    let mut last_unwell: Option<Unwell> = None;
    let mut ever_ready = false;
    let mut changed: Option<ChangedStream> = None;

    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        // (Re)connect anonymously: `Client::connect` never requests a
        // well-known name, so a healthy service is never disturbed and an
        // absent one just fails here, before any method call.
        let client = match shared_client(&client_slot) {
            Some(c) => c,
            None => match Client::connect().await {
                Ok(c) => {
                    set_shared_client(&client_slot, Some(c.clone()));
                    c
                }
                Err(e) => {
                    if last_state != Some(ConnState::NoService) {
                        last_state = Some(ConnState::NoService);
                        eprintln!("timetrack-tray: service not running ({e}); retrying");
                    }
                    if last_unwell != Some(Unwell::Down) {
                        last_unwell = Some(Unwell::Down);
                        let published = Published {
                            menu: last_menu.clone(),
                            summary: last_summary.clone(),
                            unwell: last_unwell.clone(),
                        };
                        if !notifier.send(published) {
                            break;
                        }
                    }
                    async_io::Timer::after(POLL).await;
                    continue;
                }
            },
        };

        if changed.is_none() {
            changed = client.changed_stream().await.ok();
        }

        let result = client.snapshot().await;
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

        let unwell = unwell_of(&result);
        let unwell_changed = unwell != last_unwell;
        if unwell_changed {
            last_unwell = unwell.clone();
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
                let undo = crate::lock(&last_quick_add).clone();
                let summary = TooltipState::from_snapshot(&snapshot);
                if unwell_changed
                    || menu != last_menu
                    || undo != last_undo
                    || summary != last_summary
                {
                    last_menu = menu.clone();
                    last_undo = undo;
                    last_summary = summary.clone();
                    let published = Published {
                        menu,
                        summary,
                        unwell: last_unwell.clone(),
                    };
                    if !notifier.send(published) {
                        break;
                    }
                }
            }
            Err(ClientError::NoService) | Err(ClientError::Transport(_)) => {
                // The service went away: drop the connection so the next
                // tick reconnects from scratch.
                set_shared_client(&client_slot, None);
                changed = None;
                if unwell_changed {
                    let published = Published {
                        menu: last_menu.clone(),
                        summary: last_summary.clone(),
                        unwell: last_unwell.clone(),
                    };
                    if !notifier.send(published) {
                        break;
                    }
                }
            }
            Err(ClientError::VersionMismatch { .. }) | Err(ClientError::Service(_)) => {
                // Keep the connection: the transport is fine. A mismatch
                // means a stale install (needs a reinstall, not a
                // reconnect); a refusal on Snapshot should not happen, and
                // retrying is harmless either way.
                if unwell_changed {
                    let published = Published {
                        menu: last_menu.clone(),
                        summary: last_summary.clone(),
                        unwell: last_unwell.clone(),
                    };
                    if !notifier.send(published) {
                        break;
                    }
                }
            }
        }

        // Wait out the fallback tick in short slices: a `Changed`
        // announcement ends the wait at once, and the slices keep Quit
        // answering within a beat instead of a full tick.
        for _ in 0..POLL.as_millis() / 100 {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            let slice = Duration::from_millis(100);
            match changed.as_mut() {
                Some(stream) => match stream.wait_timeout(slice).await {
                    ChangedWait::Changed => break,
                    ChangedWait::Ended => {
                        changed = None;
                        break;
                    }
                    ChangedWait::TimedOut => {}
                },
                None => {
                    async_io::Timer::after(slice).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::TrayAction;
    use crate::menu::{MAX_PROJECTS, build_menu_model};
    use timetrack_core::QUICK_ADD_MS;
    use timetrack_proto::{EntryView, ProjectView, Snapshot};

    fn summary(total_ms: i64, projects: usize, attention: bool) -> TooltipState {
        TooltipState {
            total_ms,
            projects,
            attention,
        }
    }

    fn project(id: &str, name: &str, archived: bool) -> ProjectView {
        ProjectView {
            id: id.into(),
            name: name.into(),
            colour: None,
            archived,
        }
    }

    fn entry(id: &str, project_id: &str, duration_ms: i64) -> EntryView {
        EntryView {
            id: id.into(),
            project_id: project_id.into(),
            description: String::new(),
            started_at: 0,
            ended_at: duration_ms,
            source: timetrack_proto::EntrySource::QuickAdd,
            note: None,
        }
    }

    fn snapshot_with(projects: Vec<ProjectView>, week_ms: &[(&str, i64)]) -> Snapshot {
        let mut snapshot = Snapshot {
            projects,
            entries: Vec::new(),
            ..Default::default()
        };
        for (id, ms) in week_ms {
            snapshot.week.per_project.insert((*id).into(), *ms);
        }
        snapshot.week.total_ms = week_ms.iter().map(|(_, ms)| *ms).sum();
        snapshot
    }

    fn submenu_labels(items: &[WinMenuItem]) -> Vec<String> {
        items
            .iter()
            .map(|item| match item {
                WinMenuItem::Action { label, .. } => label.clone(),
                WinMenuItem::Submenu { label, .. } => label.clone(),
            })
            .collect()
    }

    #[test]
    fn menu_bar_title_shows_the_week_total() {
        assert_eq!(
            status_title(&summary(90 * 60_000, 2, false), None),
            "1h 30m"
        );
        assert_eq!(status_title(&summary(0, 0, false), None), "0m");
        // Attention travels via the icon, not the title: the total stays.
        assert_eq!(status_title(&summary(25 * 3_600_000, 1, true), None), "25h");
    }

    #[test]
    fn menu_bar_title_goes_flat_while_the_service_is_down() {
        // A zero-looking total while down would read as tracked time.
        assert_eq!(
            status_title(&summary(90 * 60_000, 2, false), Some(&Unwell::Down)),
            "–"
        );
        assert_eq!(status_title(&summary(0, 0, true), Some(&Unwell::Down)), "–");
    }

    #[test]
    fn menu_bar_title_keeps_the_total_on_skew_and_refusal() {
        // The transport is fine; the tooltip carries the explanation.
        assert_eq!(
            status_title(&summary(90 * 60_000, 2, false), Some(&Unwell::Skewed)),
            "1h 30m"
        );
        assert_eq!(
            status_title(
                &summary(90 * 60_000, 2, false),
                Some(&Unwell::Refused("older service".into())),
            ),
            "1h 30m"
        );
    }

    #[test]
    fn single_instance_lock_derives_from_the_tray_name() {
        // Raw binaries and future bundles claim the same per-user path,
        // which never depends on the executable's location.
        assert_eq!(
            single_instance_lock_file_name(),
            "org.sequ.timetrack.tray.lock"
        );
        assert!(single_instance_lock_file_name().starts_with(crate::TRAY_NAME));
        let path = single_instance_lock_path();
        assert_eq!(path.parent(), Some(std::env::temp_dir().as_path()));
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some(single_instance_lock_file_name().as_str())
        );
    }

    #[test]
    fn poll_cadence_matches_the_linux_loop() {
        // The worker is the Linux loop's twin: the same 60 s fallback,
        // pinned against drift from the Windows twin too.
        assert_eq!(POLL_SECS, 60);
        assert_eq!(POLL_SECS, crate::backend_windows::POLL_SECS);
    }

    #[test]
    fn every_action_round_trips_through_its_menu_id() {
        // The macOS runner mints and decodes through the shared
        // tray-icon/muda wire format: every row must survive the trip.
        let mut actions = vec![
            TrayAction::Undo {
                project_id: "p1".into(),
            },
            TrayAction::DeleteEntry {
                entry_id: "e9".into(),
                confirm_label: "Delete 15m from Work".into(),
            },
            TrayAction::OpenGui,
            TrayAction::Quit,
        ];
        for ms in QUICK_ADD_MS {
            actions.push(TrayAction::QuickAdd {
                project_id: "p1".into(),
                duration_ms: ms,
            });
        }
        for action in &actions {
            assert_eq!(decode_action(&encode_action(action)), Some(action.clone()));
        }
    }

    #[test]
    fn keep_ids_round_trip_outside_dispatch() {
        assert_eq!(parse_keep_id(&keep_id("e9")), Some("e9"));
        // Neither direction leaks into the other: keep ids never decode
        // as actions, and actions never parse as keep.
        assert_eq!(decode_action(&keep_id("e9")), None);
        assert_eq!(parse_keep_id(&encode_action(&TrayAction::Quit)), None);
    }

    #[test]
    fn project_submenu_mirrors_the_linux_rows() {
        let mut snapshot = snapshot_with(vec![project("p1", "Work", false)], &[("p1", 3_600_000)]);
        snapshot.entries = vec![entry("e1", "p1", 900_000)];
        let model = build_menu_model(&snapshot);
        let items = build_window_menu(&model, &HashMap::new());
        // The project submenu, then Open and Quit.
        assert_eq!(items.len(), 3);
        let WinMenuItem::Submenu { label, items } = &items[0] else {
            panic!("first row is the project submenu: {:?}", items[0]);
        };
        assert_eq!(label, "Work");
        assert_eq!(
            submenu_labels(items),
            vec![
                "This week: 1h",
                "+5m",
                "+15m",
                "+30m",
                "+60m",
                "Undo quick-add",
                "Delete last entry (15m)",
            ]
        );
        // No quick-add this session: undo stays disabled.
        assert!(matches!(
            &items[5],
            WinMenuItem::Action { enabled: false, .. }
        ));
        // The confirm pair states exactly what would go, and both ids
        // route: confirm through dispatch, cancel through the keep log.
        let WinMenuItem::Submenu { items: confirm, .. } = &items[6] else {
            panic!("delete row is a submenu: {:?}", items[6]);
        };
        assert_eq!(
            submenu_labels(confirm),
            vec!["Delete 15m from Work", "Keep it"]
        );
        let WinMenuItem::Action { id, .. } = &confirm[0] else {
            panic!("confirm row is an action: {:?}", confirm[0]);
        };
        assert!(matches!(
            decode_action(id),
            Some(TrayAction::DeleteEntry { .. })
        ));
        let WinMenuItem::Action { id, .. } = &confirm[1] else {
            panic!("cancel row is an action: {:?}", confirm[1]);
        };
        assert_eq!(parse_keep_id(id), Some("e1"));
    }

    #[test]
    fn overflow_projects_group_under_more_projects() {
        let projects: Vec<ProjectView> = (0..MAX_PROJECTS + 2)
            .map(|i| project(&format!("p{i}"), &format!("P{i}"), false))
            .collect();
        let model = build_menu_model(&snapshot_with(projects, &[]));
        assert_eq!(model.overflow.len(), 2);
        let items = build_window_menu(&model, &HashMap::new());
        // Visible projects, then the overflow submenu, then Open and Quit.
        assert_eq!(items.len(), MAX_PROJECTS + 3);
        let WinMenuItem::Submenu { label, items } = &items[MAX_PROJECTS] else {
            panic!("overflow groups under one submenu");
        };
        assert_eq!(label, "More projects");
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn gui_presence_follows_the_filesystem() {
        // The Open row's target exists only when a macOS GUI binary
        // sits beside the tray; otherwise the runner logs its absence.
        assert!(!gui_program_present(""));
        assert!(!gui_program_present("/definitely/not/here/timetrack-gui"));
        let dir =
            std::env::temp_dir().join(format!("timetrack-tray-test-macos-{}", std::process::id()));
        let exe = dir.join("timetrack-gui");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&exe, b"fake").unwrap();
        assert!(gui_program_present(&exe.to_string_lossy()));
        assert!(!gui_program_present(&dir.to_string_lossy()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ui_tick_matches_windows() {
        assert_eq!(UI_TICK_MS, crate::backend_windows::UI_TICK_MS);
        const {
            assert!(UI_TICK_MS > 0);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn wake_event_matches_only_our_sentinel() {
        use objc2::rc::autoreleasepool;
        use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSEventType};
        use objc2_foundation::NSPoint;

        autoreleasepool(|_| {
            let wake = build_wake_event().expect("synthetic wake event");
            assert!(is_wake_event(&wake));

            let other =
                NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
                    NSEventType::ApplicationDefined,
                    NSPoint::ZERO,
                    NSEventModifierFlags::empty(),
                    0.0,
                    0,
                    None,
                    0,
                    0,
                    0,
                )
                .expect("other ApplicationDefined event");
            assert!(!is_wake_event(&other));
        });
    }
}
