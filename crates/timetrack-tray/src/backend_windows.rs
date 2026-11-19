/* tray/backend_windows.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The Windows notification-area backend (#155): a native tray icon with
//! Active/Passive/NeedsAttention states, a tooltip, and a native menu that
//! mirrors the Linux structure, routed through the neutral [`dispatch`].
//!
//! The pure mapping (menu ids, menu model, icon bytes, tooltip clamp, the
//! mutex name) compiles on every target so the headless unit tests below
//! pin it without a desktop. Only the runner — the icon, the menu event
//! loop, the poll worker, and the single-instance mutex — is
//! `cfg(windows)`, with its `tray-icon`/`windows-sys` dependencies gated
//! the same way in the manifest.
//!
//! [`dispatch`]: crate::actions::dispatch

use crate::actions::{TrayAction, quick_add_label};
use crate::menu::{MenuModel, ProjectItem};
use crate::state::NeutralStatus;
#[cfg(windows)]
use crate::state::{TooltipState, Unwell, presentation};
use std::collections::HashMap;
#[cfg(windows)]
use std::sync::{Arc, Mutex};
use timetrack_core::QUICK_ADD_MS;

/// The single-instance mutex: per-session (`Local\`, like the Linux
/// per-session bus name), derived from [`crate::TRAY_NAME`] so the two
/// backends cannot drift apart.
pub const SINGLE_INSTANCE_MUTEX: &str = "Local\\org.sequ.timetrack.tray";

/// The fallback snapshot cadence, in seconds: the same 60 s tick as the
/// Linux poll loop. Freshness normally arrives as a `Changed` announcement
/// the moment a mutation persists; this only covers a missed signal and
/// noticing that the service has gone away.
pub const POLL_SECS: u64 = 60;

/// Maximum wait before the UI event loop re-checks sticky action errors
/// even when neither OS input nor a worker publish arrived, in
/// milliseconds.
pub const UI_TICK_MS: u32 = 500;

/// The tray icon's edge, in pixels. The bytes are generated (see
/// [`icon_rgba`]), not shipped as an asset.
pub const ICON_PX: u32 = 32;

/// The Win32 tooltip cap (`NOTIFYICONDATA.szTip`): 128 wide chars
/// including the NUL, so 127 of payload.
pub const WINDOWS_TOOLTIP_MAX: usize = 127;

/// The generated icon's RGBA bytes for one [`NeutralStatus`]: a filled
/// circle in the status colour with a darker ring on a transparent
/// background. Green sits idle-active, grey flat while the service is
/// down, red when something needs the user — the same three states the
/// Linux backend renders through `ksni::Status`.
pub fn icon_rgba(status: NeutralStatus) -> Vec<u8> {
    let (r, g, b) = status_color(status);
    let darken = |c: u8| (u16::from(c) * 3 / 5) as u8;
    let (dr, dg, db) = (darken(r), darken(g), darken(b));
    let px = ICON_PX as i32;
    let center = px as f32 / 2.0;
    let mut rgba = vec![0u8; (px * px * 4) as usize];
    for y in 0..px {
        for x in 0..px {
            let dx = x as f32 + 0.5 - center;
            let dy = y as f32 + 0.5 - center;
            let dist = dx.hypot(dy);
            let i = ((y * px + x) * 4) as usize;
            if dist <= center - 3.0 {
                rgba[i..i + 4].copy_from_slice(&[r, g, b, 0xFF]);
            } else if dist <= center - 1.0 {
                rgba[i..i + 4].copy_from_slice(&[dr, dg, db, 0xFF]);
            }
        }
    }
    rgba
}

/// The status colour [`icon_rgba`] fills with.
fn status_color(status: NeutralStatus) -> (u8, u8, u8) {
    match status {
        NeutralStatus::Active => (46, 204, 113),
        NeutralStatus::Passive => (128, 128, 128),
        NeutralStatus::NeedsAttention => (231, 76, 60),
    }
}

/// Clamp a [`presentation`] tooltip to what the notification area shows:
/// at most [`WINDOWS_TOOLTIP_MAX`] chars, cut on a char boundary so a
/// multibyte tail can never panic the conversion.
pub fn tooltip_for_windows(description: &str) -> String {
    if description.chars().count() <= WINDOWS_TOOLTIP_MAX {
        return description.to_string();
    }
    description
        .chars()
        .take(WINDOWS_TOOLTIP_MAX)
        .collect::<String>()
}

/// The menu-id separator: a control character the sanitizer turns into a
/// space, so no label text that survives [`build_window_menu`] can ever
/// contain it, while store ids (UUIDs) never do either.
const SEP: char = '\u{1f}';

/// Encode a [`TrayAction`] as a native menu id, so a row click round-trips
/// back through [`decode_action`] into the neutral [`dispatch`].
///
/// [`dispatch`]: crate::actions::dispatch
pub fn encode_action(action: &TrayAction) -> String {
    match action {
        TrayAction::QuickAdd {
            project_id,
            duration_ms,
        } => format!("qa{SEP}{project_id}{SEP}{duration_ms}"),
        TrayAction::Undo { project_id } => format!("undo{SEP}{project_id}"),
        TrayAction::DeleteEntry {
            entry_id,
            confirm_label,
        } => format!("del{SEP}{entry_id}{SEP}{confirm_label}"),
        TrayAction::OpenGui => "open".to_string(),
        TrayAction::Quit => "quit".to_string(),
    }
}

/// Decode a clicked menu id back into a [`TrayAction`]. `None` is an id
/// this backend never minted (a stale click, a foreign event): the runner
/// logs and ignores it. The quick-add bucket is validated, so a forged id
/// cannot quick-add an arbitrary duration.
pub fn decode_action(id: &str) -> Option<TrayAction> {
    let (tag, rest) = match id.split_once(SEP) {
        Some((tag, rest)) => (tag, Some(rest)),
        None => (id, None),
    };
    match (tag, rest) {
        ("open", None) => Some(TrayAction::OpenGui),
        ("quit", None) => Some(TrayAction::Quit),
        ("undo", Some(project_id)) if !project_id.contains(SEP) => Some(TrayAction::Undo {
            project_id: project_id.to_string(),
        }),
        ("qa", Some(rest)) => {
            let (project_id, ms) = rest.split_once(SEP)?;
            if project_id.contains(SEP) {
                return None;
            }
            let duration_ms: i64 = ms.parse().ok()?;
            if !QUICK_ADD_MS.contains(&duration_ms) {
                return None;
            }
            Some(TrayAction::QuickAdd {
                project_id: project_id.to_string(),
                duration_ms,
            })
        }
        ("del", Some(rest)) => {
            let (entry_id, confirm_label) = rest.split_once(SEP)?;
            if entry_id.contains(SEP) {
                return None;
            }
            Some(TrayAction::DeleteEntry {
                entry_id: entry_id.to_string(),
                confirm_label: confirm_label.to_string(),
            })
        }
        _ => None,
    }
}

/// The "Keep it" row's id: the cancel half of the delete confirm, which
/// is not a [`TrayAction`] (there is nothing to dispatch) but still needs
/// the entry id for its log line, like the Linux cancel closure.
pub fn keep_id(entry_id: &str) -> String {
    format!("keep{SEP}{entry_id}")
}

/// Parse a [`keep_id`]: the entry id whose deletion was cancelled, or
/// `None` for anything else (including every [`decode_action`] id).
pub fn parse_keep_id(id: &str) -> Option<&str> {
    let (tag, entry_id) = id.split_once(SEP)?;
    if tag == "keep" && !entry_id.contains(SEP) {
        Some(entry_id)
    } else {
        None
    }
}

/// One native menu row, backend-neutral like [`TrayAction`]: the runner
/// turns these into `muda` items, and the tests pin the structure without
/// a desktop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WinMenuItem {
    /// A clickable (or disabled) row: `id` round-trips through
    /// [`decode_action`] (or [`parse_keep_id`]); disabled rows never fire.
    Action {
        id: String,
        label: String,
        enabled: bool,
    },
    Submenu {
        label: String,
        items: Vec<WinMenuItem>,
    },
}

/// Build the native menu model from a snapshot's [`MenuModel`]: one
/// submenu per visible project, the overflow under "More projects", then
/// Open and Quit. Row labels mirror `backend_linux::project_submenu`
/// exactly — the disabled week-total row, the four quick-add buckets,
/// undo, and the delete-confirm pair — so the two trays read as one app.
pub fn build_window_menu(model: &MenuModel, undo: &HashMap<String, String>) -> Vec<WinMenuItem> {
    let mut items: Vec<WinMenuItem> = model
        .visible
        .iter()
        .map(|p| project_submenu(p, undo.contains_key(&p.id)))
        .collect();
    if !model.overflow.is_empty() {
        items.push(WinMenuItem::Submenu {
            label: "More projects".to_string(),
            items: model
                .overflow
                .iter()
                .map(|p| project_submenu(p, undo.contains_key(&p.id)))
                .collect(),
        });
    }
    items.push(WinMenuItem::Action {
        id: encode_action(&TrayAction::OpenGui),
        label: "Open TimeTrack".to_string(),
        enabled: true,
    });
    items.push(WinMenuItem::Action {
        id: encode_action(&TrayAction::Quit),
        label: "Quit".to_string(),
        enabled: true,
    });
    items
}

/// One project's submenu: the disabled week-total row, one row per
/// quick-add bucket (tray-5, #9), the undo row (tray-6, #10), and the
/// delete row with its confirm submenu (tray-7, #11).
fn project_submenu(project: &ProjectItem, can_undo: bool) -> WinMenuItem {
    let mut submenu = vec![WinMenuItem::Action {
        id: "noop".to_string(),
        label: format!(
            "This week: {}",
            timetrack_core::format_compact(project.week_ms)
        ),
        enabled: false,
    }];
    for ms in QUICK_ADD_MS {
        submenu.push(WinMenuItem::Action {
            id: encode_action(&TrayAction::QuickAdd {
                project_id: project.id.clone(),
                duration_ms: ms,
            }),
            label: quick_add_label(ms),
            enabled: true,
        });
    }
    submenu.push(WinMenuItem::Action {
        id: encode_action(&TrayAction::Undo {
            project_id: project.id.clone(),
        }),
        label: "Undo quick-add".to_string(),
        enabled: can_undo,
    });
    match &project.last_entry {
        Some(target) => {
            let when = timetrack_core::format_compact(target.duration_ms);
            let confirm_label = format!("Delete {when} from {}", project.name);
            submenu.push(WinMenuItem::Submenu {
                label: format!("Delete last entry ({when})"),
                items: vec![
                    WinMenuItem::Action {
                        id: encode_action(&TrayAction::DeleteEntry {
                            entry_id: target.id.clone(),
                            confirm_label: confirm_label.clone(),
                        }),
                        label: confirm_label,
                        enabled: true,
                    },
                    WinMenuItem::Action {
                        id: keep_id(&target.id),
                        label: "Keep it".to_string(),
                        enabled: true,
                    },
                ],
            });
        }
        None => submenu.push(WinMenuItem::Action {
            id: "noop".to_string(),
            label: "Nothing to delete".to_string(),
            enabled: false,
        }),
    }
    WinMenuItem::Submenu {
        label: project.name.clone(),
        items: submenu,
    }
}

/// Whether the Open row's target exists. A standalone tray install may
/// not ship the sibling GUI binary, so the runner logs its presence once
/// at startup and clicks degrade gracefully through neutral dispatch's
/// spawn-failure log.
pub fn gui_exe_present(program: &str) -> bool {
    !program.is_empty() && std::path::Path::new(program).is_file()
}

/// The single-instance mutex, held for the process's whole run (Windows).
/// Dropping it (on Quit, or on death) releases the name, exactly like the
/// Linux bus-name claim.
#[cfg(windows)]
pub struct SingleInstance {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl SingleInstance {
    /// Claim the tray's mutex. `None` means a tray is already running --
    /// the designed outcome of a repeated launch, not an error.
    pub fn try_claim() -> anyhow::Result<Option<Self>> {
        use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
        use windows_sys::Win32::System::Threading::CreateMutexW;
        let wide: Vec<u16> = SINGLE_INSTANCE_MUTEX
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, wide.as_ptr()) };
        if handle.is_null() {
            anyhow::bail!("could not create the single-instance mutex");
        }
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
            return Ok(None);
        }
        Ok(Some(Self { handle }))
    }
}

#[cfg(windows)]
impl Drop for SingleInstance {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.handle) };
    }
}

/// What the poll worker publishes to the UI thread: the same four inputs
/// the Linux loop gates on (menu, tooltip summary, connection health, and
/// the undo map for the gate), sent by value only when one changes.
#[cfg(windows)]
#[derive(Debug, Clone)]
struct Published {
    menu: MenuModel,
    summary: TooltipState,
    unwell: Option<Unwell>,
}

/// Auto-reset Win32 event waking the UI thread's `MsgWaitForMultipleObjectsEx`
/// when the poll worker publishes an update or exits.
#[cfg(windows)]
struct WakeEvent {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

// SAFETY: A Win32 event handle may be signaled via `SetEvent` from any thread
// while the UI thread waits in `MsgWaitForMultipleObjectsEx`; `Arc<WakeEvent>`
// ensures `CloseHandle` runs once on final drop.
#[cfg(windows)]
unsafe impl Send for WakeEvent {}
// SAFETY: See `Send` impl above.
#[cfg(windows)]
unsafe impl Sync for WakeEvent {}

#[cfg(windows)]
impl WakeEvent {
    fn new() -> anyhow::Result<Self> {
        use windows_sys::Win32::System::Threading::CreateEventW;
        let handle = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
        if handle.is_null() {
            anyhow::bail!("could not create the UI wake event");
        }
        Ok(Self { handle })
    }

    fn raw_handle(&self) -> windows_sys::Win32::Foundation::HANDLE {
        self.handle
    }

    fn signal(&self) {
        use windows_sys::Win32::System::Threading::SetEvent;
        let _ = unsafe { SetEvent(self.handle) };
    }
}

#[cfg(windows)]
impl Drop for WakeEvent {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.handle) };
    }
}

/// Worker-to-UI channel plus its Win32 wake handle so published updates
/// interrupt `MsgWaitForMultipleObjectsEx` immediately.
#[cfg(windows)]
struct WinNotifier {
    tx: Option<std::sync::mpsc::Sender<Published>>,
    wake: Arc<WakeEvent>,
}

#[cfg(windows)]
impl WinNotifier {
    fn new(tx: std::sync::mpsc::Sender<Published>, wake: Arc<WakeEvent>) -> Self {
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

#[cfg(windows)]
impl Drop for WinNotifier {
    fn drop(&mut self) {
        // Drop the sender before signaling so the UI thread observes
        // `Disconnected` immediately when woken.
        self.tx.take();
        self.wake.signal();
    }
}

/// Drain every pending menu click produced by `muda` on the UI thread.
#[cfg(windows)]
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
#[cfg(windows)]
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

/// Run the Windows tray: claim single instance, show the icon, and pump
/// Win32 messages, menu clicks, and worker publishes until Quit. Blocks the
/// calling (UI) thread; the service polling lives on a worker thread, like
/// the Linux poll loop, and clicks dispatch the neutral [`TrayAction`]s.
#[cfg(windows)]
pub fn run() -> anyhow::Result<()> {
    use crate::actions::DispatchContext;
    use crate::ipc::set_shared_client;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };
    use tray_icon::{Icon, TrayIconBuilder};
    use windows_sys::Win32::Foundation::WAIT_FAILED;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, MSG, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, PM_REMOVE,
        PeekMessageW, QS_ALLINPUT, TranslateMessage, WM_QUIT,
    };

    let Some(_instance) = SingleInstance::try_claim()? else {
        println!("timetrack-tray: already running; exiting");
        return Ok(());
    };

    // Say once at startup whether the Open row's target is present beside
    // the tray; clicks degrade gracefully to neutral dispatch's
    // spawn-failure log when the GUI binary is absent.
    let argv = crate::platform::open_gui_command();
    let program = argv.first().cloned().unwrap_or_default();
    if gui_exe_present(&program) {
        println!("timetrack-tray: GUI at {program}");
    } else {
        eprintln!("timetrack-tray: no GUI at {program}; the Open row will report it");
    }

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

    // The starting icon mirrors a fresh Linux item: idle-active over the
    // zero summary, replaced by the first publish within a tick.
    let (status, description) = presentation(&TooltipState::default(), None, None);
    let icon = Icon::from_rgba(icon_rgba(status), ICON_PX, ICON_PX)
        .map_err(|e| anyhow::anyhow!("could not build the tray icon: {e}"))?;
    let menu = build_native_menu(&build_window_menu(&MenuModel::default(), &HashMap::new()))?;
    let tray = TrayIconBuilder::new()
        .with_id(crate::TRAY_NAME)
        .with_tooltip(tooltip_for_windows(&description))
        .with_icon(icon)
        .with_menu(Box::new(menu))
        .build()
        .map_err(|e| anyhow::anyhow!("could not register tray icon: {e}"))?;
    println!("timetrack-tray: registered {}", crate::TRAY_NAME);

    let wake = Arc::new(WakeEvent::new()?);
    let (tx, rx) = std::sync::mpsc::channel::<Published>();
    let notifier = WinNotifier::new(tx, wake.clone());
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

    // The last publish applied, plus the last icon state pushed: the icon
    // and tooltip only move on a real change, while `UI_TICK_MS` re-reads
    // the sticky action error so a failed click surfaces within a beat
    // even when no Win32 message or snapshot change arrived (ksni
    // re-renders on demand; here the UI thread must push).
    let mut last = Published {
        menu: MenuModel::default(),
        summary: TooltipState::default(),
        unwell: None,
    };
    let mut last_pushed: Option<(NeutralStatus, String)> = None;
    loop {
        let mut quit = false;
        let mut msg = MSG::default();
        while unsafe { PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
            if msg.message == WM_QUIT {
                quit = true;
                break;
            }
            unsafe {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        drain_menu_events(&ctx);
        let disconnected = drain_updates(&rx, &tray, &last_quick_add, &mut last);
        push_status(&tray, &last, &action_error, &mut last_pushed);
        if quit {
            shutdown.store(true, Ordering::Relaxed);
            set_shared_client(&client_slot, None);
            println!("timetrack-tray: WM_QUIT received; exiting");
            break;
        }
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
        let handles = [wake.raw_handle()];
        let waited = unsafe {
            MsgWaitForMultipleObjectsEx(
                1,
                handles.as_ptr(),
                UI_TICK_MS,
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            )
        };
        if waited == WAIT_FAILED {
            let err = std::io::Error::last_os_error();
            eprintln!("timetrack-tray: UI message wait failed ({err}); exiting");
            break;
        }
    }
    Ok(())
}

/// Push the icon state plus the tooltip body when either changed.
#[cfg(windows)]
fn push_status(
    tray: &tray_icon::TrayIcon,
    last: &Published,
    action_error: &Arc<Mutex<Option<String>>>,
    last_pushed: &mut Option<(NeutralStatus, String)>,
) {
    let failed = crate::lock(action_error).clone();
    let (status, description) =
        presentation(&last.summary, last.unwell.as_ref(), failed.as_deref());
    if last_pushed.as_ref() == Some(&(status, description.clone())) {
        return;
    }
    *last_pushed = Some((status, description.clone()));
    match tray_icon::Icon::from_rgba(icon_rgba(status), ICON_PX, ICON_PX) {
        Ok(icon) => {
            if let Err(e) = tray.set_icon(Some(icon)) {
                eprintln!("timetrack-tray: could not set the tray icon: {e}");
            }
        }
        Err(e) => eprintln!("timetrack-tray: could not build the tray icon: {e}"),
    }
    if let Err(e) = tray.set_tooltip(Some(tooltip_for_windows(&description))) {
        eprintln!("timetrack-tray: could not set the tooltip: {e}");
    }
}

/// Turn a [`build_window_menu`] model into a live native menu. Every action
/// row carries its [`encode_action`] id; submenus only group.
#[cfg(windows)]
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
#[cfg(windows)]
async fn poll_worker(
    notifier: WinNotifier,
    last_quick_add: Arc<Mutex<HashMap<String, String>>>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    client_slot: crate::ipc::SharedClient,
) {
    use crate::ipc::{set_shared_client, shared_client};
    use crate::menu::build_menu_model;
    use crate::state::{ConnState, conn_state, unwell_of};
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
    use crate::menu::{LastEntry, MAX_PROJECTS, build_menu_model};
    use timetrack_proto::{EntryView, ProjectView, Snapshot};

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

    fn pixel(rgba: &[u8], x: u32, y: u32) -> [u8; 4] {
        let i = ((y * ICON_PX + x) * 4) as usize;
        [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
    }

    #[test]
    fn single_instance_names_the_tray_per_session() {
        // `Local\` is the per-session namespace, matching the Linux
        // per-session bus claim; the tail is the tray's well-known name.
        assert_eq!(SINGLE_INSTANCE_MUTEX, "Local\\org.sequ.timetrack.tray");
        assert!(SINGLE_INSTANCE_MUTEX.ends_with(crate::TRAY_NAME));
    }

    #[test]
    fn poll_cadence_matches_the_linux_loop() {
        // The worker is the Linux loop's twin: the same 60 s fallback.
        assert_eq!(POLL_SECS, 60);
        assert_eq!(
            std::time::Duration::from_secs(POLL_SECS).as_millis(),
            60_000
        );
    }

    #[test]
    fn icon_bytes_cover_the_full_square() {
        for status in [
            NeutralStatus::Active,
            NeutralStatus::Passive,
            NeutralStatus::NeedsAttention,
        ] {
            let rgba = icon_rgba(status);
            assert_eq!(rgba.len(), (ICON_PX * ICON_PX * 4) as usize);
        }
    }

    #[test]
    fn icon_states_are_distinct_circles_on_transparent_ground() {
        let active = icon_rgba(NeutralStatus::Active);
        let passive = icon_rgba(NeutralStatus::Passive);
        let attention = icon_rgba(NeutralStatus::NeedsAttention);
        assert_ne!(active, passive);
        assert_ne!(active, attention);
        assert_ne!(passive, attention);
        // The disc centre carries the status colour, opaque.
        assert_eq!(pixel(&active, 16, 16), [46, 204, 113, 0xFF]);
        assert_eq!(pixel(&passive, 16, 16), [128, 128, 128, 0xFF]);
        assert_eq!(pixel(&attention, 16, 16), [231, 76, 60, 0xFF]);
        // The corners stay transparent: a circle, not a square.
        for rgba in [&active, &passive, &attention] {
            assert_eq!(pixel(rgba, 0, 0)[3], 0);
            assert_eq!(pixel(rgba, ICON_PX - 1, ICON_PX - 1)[3], 0);
        }
        // The ring is the fill darkened, so the disc reads against light
        // and dark trays alike.
        let [r, g, b, a] = pixel(&active, 16, 1);
        assert_eq!(a, 0xFF);
        assert!(r < 46 && g < 204 && b < 113, "ring: {r} {g} {b}");
    }

    #[test]
    fn short_tooltips_pass_through() {
        let tip = "Week total 1h 30m · 2 projects";
        assert_eq!(tooltip_for_windows(tip), tip);
    }

    #[test]
    fn long_tooltips_clamp_on_a_char_boundary() {
        let tip = "é".repeat(200);
        let clamped = tooltip_for_windows(&tip);
        assert_eq!(clamped.chars().count(), WINDOWS_TOOLTIP_MAX);
        assert_eq!(clamped, "é".repeat(WINDOWS_TOOLTIP_MAX));
        let tip = "x".repeat(500);
        assert_eq!(tooltip_for_windows(&tip).len(), WINDOWS_TOOLTIP_MAX);
    }

    #[test]
    fn every_action_round_trips_through_its_menu_id() {
        let actions = vec![
            TrayAction::QuickAdd {
                project_id: "p1".into(),
                duration_ms: QUICK_ADD_MS[0],
            },
            TrayAction::QuickAdd {
                project_id: "p1".into(),
                duration_ms: QUICK_ADD_MS[1],
            },
            TrayAction::QuickAdd {
                project_id: "p1".into(),
                duration_ms: QUICK_ADD_MS[2],
            },
            TrayAction::QuickAdd {
                project_id: "p1".into(),
                duration_ms: QUICK_ADD_MS[3],
            },
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
        for action in &actions {
            assert_eq!(decode_action(&encode_action(action)), Some(action.clone()));
        }
    }

    #[test]
    fn hostile_ids_never_decode_into_actions() {
        // Stale clicks, foreign events, and forged ids must all land on
        // the runner's ignore path, never on a dispatch.
        for id in [
            "",
            "noop",
            "open ",
            "open\u{1f}x",
            "quit\u{1f}",
            "qa",
            "qa\u{1f}p1",
            "qa\u{1f}p1\u{1f}not-a-number",
            "qa\u{1f}p1\u{1f}999",
            "qa\u{1f}p1\u{1f}300000\u{1f}extra",
            "qa\u{1f}p1\u{1f}p2\u{1f}300000",
            "undo",
            "del",
            "del\u{1fe9}",
            "keep\u{1fe9}",
        ] {
            assert_eq!(decode_action(id), None, "id: {id:?}");
        }
        // A forged bucket outside the four quick-add rows is refused even
        // though it parses as a number.
        assert_eq!(decode_action("qa\u{1f}p1\u{1f}60000"), None);
    }

    #[test]
    fn awkward_store_ids_still_round_trip() {
        // Colons, spaces, and unicode ride the id untouched; only the
        // separator itself is reserved (store ids are UUIDs).
        for id in ["a:b c", "pré", "550e8400-e29b-41d4-a716-446655440000"] {
            let action = TrayAction::Undo {
                project_id: id.into(),
            };
            assert_eq!(decode_action(&encode_action(&action)), Some(action));
        }
    }

    #[test]
    fn keep_ids_round_trip_outside_dispatch() {
        assert_eq!(parse_keep_id(&keep_id("e9")), Some("e9"));
        assert_eq!(parse_keep_id("keep"), None);
        assert_eq!(parse_keep_id("keep\u{1f}a\u{1f}b"), None);
        // Neither direction leaks into the other: keep ids never decode
        // as actions, and actions never parse as keep.
        assert_eq!(decode_action(&keep_id("e9")), None);
        assert_eq!(parse_keep_id(&encode_action(&TrayAction::Quit)), None);
    }

    #[test]
    fn empty_menu_holds_only_open_and_quit() {
        let items = build_window_menu(&MenuModel::default(), &HashMap::new());
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0],
            WinMenuItem::Action {
                id: "open".to_string(),
                label: "Open TimeTrack".to_string(),
                enabled: true,
            }
        );
        assert_eq!(
            items[1],
            WinMenuItem::Action {
                id: "quit".to_string(),
                label: "Quit".to_string(),
                enabled: true,
            }
        );
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
    fn project_submenu_mirrors_the_linux_rows() {
        let mut snapshot = snapshot_with(vec![project("p1", "Work", false)], &[("p1", 3_600_000)]);
        snapshot.entries = vec![entry("e1", "p1", 900_000)];
        let model = build_menu_model(&snapshot);
        let items = build_window_menu(&model, &HashMap::new());
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
        assert_eq!(
            items[5],
            WinMenuItem::Action {
                id: encode_action(&TrayAction::Undo {
                    project_id: "p1".into()
                }),
                label: "Undo quick-add".to_string(),
                enabled: false,
            }
        );
        // The confirm pair states exactly what would go.
        let WinMenuItem::Submenu { items: confirm, .. } = &items[6] else {
            panic!("delete row is a submenu: {:?}", items[6]);
        };
        assert_eq!(
            submenu_labels(confirm),
            vec!["Delete 15m from Work", "Keep it"]
        );
        assert_eq!(
            confirm[0],
            WinMenuItem::Action {
                id: encode_action(&TrayAction::DeleteEntry {
                    entry_id: "e1".into(),
                    confirm_label: "Delete 15m from Work".into(),
                }),
                label: "Delete 15m from Work".to_string(),
                enabled: true,
            }
        );
    }

    #[test]
    fn undo_enables_once_this_session_quick_added() {
        let model = build_menu_model(&snapshot_with(
            vec![project("p1", "Work", false)],
            &[("p1", 0)],
        ));
        let undo = HashMap::from([("p1".to_string(), "e7".to_string())]);
        let items = build_window_menu(&model, &undo);
        let WinMenuItem::Submenu { items, .. } = &items[0] else {
            panic!("first row is the project submenu");
        };
        assert!(matches!(
            &items[5],
            WinMenuItem::Action { enabled: true, .. }
        ));
    }

    #[test]
    fn project_without_entries_shows_no_delete_row() {
        let model = build_menu_model(&snapshot_with(
            vec![project("p1", "Work", false)],
            &[("p1", 0)],
        ));
        let items = build_window_menu(&model, &HashMap::new());
        let WinMenuItem::Submenu { items, .. } = &items[0] else {
            panic!("first row is the project submenu");
        };
        assert_eq!(
            items.last(),
            Some(&WinMenuItem::Action {
                id: "noop".to_string(),
                label: "Nothing to delete".to_string(),
                enabled: false,
            })
        );
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
        assert!(matches!(items[0], WinMenuItem::Submenu { .. }));
    }

    #[test]
    fn week_totals_come_from_the_aggregate() {
        // Entries may carry only recent ones; the header reads the service
        // aggregate like the Linux submenu does.
        let model = MenuModel {
            visible: vec![ProjectItem {
                id: "p1".into(),
                name: "Work".into(),
                week_ms: 90 * 60_000,
                last_entry: Some(LastEntry {
                    id: "e1".into(),
                    duration_ms: 5 * 60_000,
                }),
            }],
            overflow: Vec::new(),
        };
        let items = build_window_menu(&model, &HashMap::new());
        let WinMenuItem::Submenu { items, .. } = &items[0] else {
            panic!("first row is the project submenu");
        };
        assert_eq!(
            items[0],
            WinMenuItem::Action {
                id: "noop".to_string(),
                label: "This week: 1h 30m".to_string(),
                enabled: false,
            }
        );
    }

    #[test]
    fn gui_presence_follows_the_filesystem() {
        // The Open row's target exists only when a Windows GUI binary
        // sits beside the tray; otherwise the runner logs its absence.
        assert!(!gui_exe_present(""));
        assert!(!gui_exe_present("/definitely/not/here/timetrack-gui.exe"));
        let dir = std::env::temp_dir().join(format!("timetrack-tray-test-{}", std::process::id()));
        let exe = dir.join("timetrack-gui.exe");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&exe, b"fake").unwrap();
        assert!(gui_exe_present(&exe.to_string_lossy()));
        assert!(!gui_exe_present(&dir.to_string_lossy()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ui_tick_is_responsive() {
        const {
            assert!(UI_TICK_MS > 0 && UI_TICK_MS <= 1_000);
        }
    }

    #[cfg(windows)]
    #[test]
    fn wake_event_unblocks_msg_wait_from_worker_thread() {
        use windows_sys::Win32::Foundation::{WAIT_FAILED, WAIT_OBJECT_0};
        use windows_sys::Win32::UI::WindowsAndMessaging::MsgWaitForMultipleObjectsEx;

        let wake = Arc::new(WakeEvent::new().unwrap());
        let worker_wake = wake.clone();
        std::thread::spawn(move || {
            worker_wake.signal();
        })
        .join()
        .unwrap();

        let handles = [wake.raw_handle()];
        let first = unsafe { MsgWaitForMultipleObjectsEx(1, handles.as_ptr(), 5_000, 0, 0) };
        assert_eq!(first, WAIT_OBJECT_0);

        // Auto-reset clears the signal once consumed by the wait above.
        let second = unsafe { MsgWaitForMultipleObjectsEx(1, handles.as_ptr(), 0, 0, 0) };
        assert_ne!(second, WAIT_OBJECT_0);
        assert_ne!(second, WAIT_FAILED);
    }
}
