/* proto/lib.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The wire contract and a client for the TimeTrack service.
//!
//! Both front ends depend on this rather than on each other, so the GUI
//! (an AppImage) and the CLI (bundled in that same image) agree on one
//! definition. It carries no GUI and no storage code.
//!
//! # One contract, on the local socket
//!
//! The service speaks exactly one wire now: the local socket (see
//! [`socket`], §16/ADR 0013), carrying these serde structs and the
//! typed [`ServiceError`] refusals. It grew from two D-Bus interfaces
//! (the JSON-in-string original and the typed `Entries2`, ADR 0012),
//! dual-served through the socket transition and retired with the
//! bus (#129); the shapes below are what all three generations shared,
//! so the contract survived both moves unchanged. The one bus use
//! left in this crate is `desktop_notify` — freedesktop notifications
//! are a D-Bus protocol, and they are the tray's surface, not the
//! service's.
//!
//! # The running timer is gone
//!
//! There is deliberately no `Running` field, `Start` or `Stop` method: every
//! entry is closed (REQUIREMENTS §7). The old stopwatch surface was removed
//! rather than deprecated, so nothing here can resurrect it.

use serde::{Deserialize, Serialize};
use std::time::Duration;

pub mod socket;

use socket::{Request as SocketRequest, Response as SocketResponse, SocketClient};

/// The app-id (desktop entry, notification icon). The service's wire
/// identity is its socket now (§16); this name lives on for the
/// desktop integration that still uses it.
pub const APP_ID: &str = "org.sequ.timetrack";

/// How long a method call may take before the client gives up on it.
///
/// zbus has no default method timeout, so without this a service that
/// accepts the connection and then never answers hangs the caller forever:
/// a client freezes inside its refresh and even quitting stops working
/// (issue #43, from the removed TUI).
///
/// Ten seconds is the trade-off. A healthy service answers a snapshot in
/// milliseconds, so ten seconds is three orders of magnitude more than it
/// needs -- a slow machine, a store with years of entries or a busy service
/// still answers well inside it -- while a wedged one surfaces as an error
/// the user can see within ten seconds instead of never.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// What a call that ran out of time says.
///
/// The service did not refuse: it stopped answering. The wording says so,
/// because "the service is not running" would send the user looking in the
/// wrong place.
const TIMEOUT_WORDING: &str = "the service did not answer in time";

/// How an entry was recorded, mirroring `timetrack_core::EntrySource`.
///
/// On the typed interface (#110) this travels as its string spelling
/// (`Manual`/`QuickAdd` — the same bytes the JSON form used), so the
/// signature is pinned to `s` rather than a variant box.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntrySource {
    Manual,
    QuickAdd,
    External,
}

/// One entry as seen by a client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryView {
    pub id: String,
    pub project_id: String,
    pub description: String,
    pub started_at: i64,
    /// Always present: entries are closed on creation (REQUIREMENTS §7).
    pub ended_at: i64,
    pub source: EntrySource,
    #[serde(default)]
    pub note: Option<String>,
}

impl EntryView {
    /// How long this entry ran. No `now` parameter, because nothing is
    /// running.
    pub fn duration_ms(&self) -> i64 {
        // Saturating, mirroring the core (#98): a view of a hand-edited or
        // hostile store must not panic a client that merely displays it.
        self.ended_at.saturating_sub(self.started_at).max(0)
    }

    /// Description, or a placeholder when the user left it empty.
    pub fn label(&self) -> &str {
        if self.description.is_empty() {
            "(no description)"
        } else {
            &self.description
        }
    }
}

/// One project as seen by a client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectView {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub colour: Option<u32>,
    #[serde(default)]
    pub archived: bool,
}

/// A registered external calendar/task source (§19). Socket-only for
/// now: sources are a new feature, so no D-Bus-era service could serve
/// them anyway.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceView {
    pub id: String,
    /// `ics_file` or `ics_url`.
    pub kind: String,
    pub locator: String,
    pub project_id: Option<String>,
    pub label: String,
}

/// A fetched event/task awaiting (or past) decision (§19).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuggestionView {
    pub id: String,
    pub source_id: String,
    pub external_uid: String,
    /// `event` or `task`.
    pub kind: String,
    pub title: String,
    pub started_at: i64,
    pub ended_at: i64,
    /// `pending`, `confirmed`, or `dismissed`.
    pub status: String,
    pub entry_id: Option<String>,
}

/// What one refresh fetched: items seen, newly stored, and skipped by
/// the parser (no interval, no UID, unparseable).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RefreshView {
    pub fetched: usize,
    pub inserted: usize,
    pub skipped: usize,
}

/// Totals for one period, mirroring `timetrack_core::aggregate::Totals`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TotalsView {
    pub total_ms: i64,
    pub per_project: std::collections::BTreeMap<String, i64>,
    pub entry_count: usize,
}

/// A consistent snapshot of the service's state.
///
/// One call, one point in time: a client renders this without the risk of
/// mixing a stale project list with fresh entries.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// All projects, including archived ones -- archived projects still
    /// contribute to historical totals (REQUIREMENTS §14), so a client
    /// filtering them out for display would lose the ability to show them.
    pub projects: Vec<ProjectView>,
    /// Entries, most recent first.
    pub entries: Vec<EntryView>,
    /// Totals for the ISO week containing `now`.
    pub week: TotalsView,
    /// Totals for the calendar month containing `now`.
    pub month: TotalsView,
    /// Totals across all time, per project as well as overall. The Projects
    /// tab reads its all-time column from here rather than re-summing
    /// `entries`, which may carry only recent entries.
    #[serde(default)]
    pub all: TotalsView,
    /// Total across all time.
    pub total_ms: i64,
    /// Local days whose summed time exceeds 24h, for the 24h warning
    /// (REQUIREMENTS §4). Values are local day indices.
    #[serde(default)]
    pub over_24h_days: Vec<i64>,
    /// The zone's UTC offset in milliseconds *at snapshot time*, so an old
    /// client without tz-database support can still bucket today the same
    /// way the service did.
    #[serde(default)]
    pub local_offset_ms: i64,
    /// The service's timezone name (`UTC`, `UTC+2`, or an IANA name such as
    /// `Europe/Rome`). Clients resolve per-instant offsets from this and
    /// parse typed local times only with it (#102). A service too old to
    /// send the field leaves it empty — detectable, so a new client can
    /// refuse instead of silently assuming UTC (the old serde default
    /// here) or the fixed `local_offset_ms`.
    #[serde(default)]
    pub tz: String,
}

impl Snapshot {
    /// This-week and all-time totals for one project, both from the
    /// service's aggregates — never re-summed from [`Snapshot::entries`],
    /// which may carry only recent entries while the aggregates cover the
    /// whole store.
    ///
    /// Services older than the all-time breakdown send no `all` (it arrives
    /// as the default); for those, fall back to summing the snapshot's
    /// entries, which is exact only when the snapshot carries every entry.
    /// Unknown projects total zero either way.
    pub fn project_totals(&self, project_id: &str) -> (i64, i64) {
        let week = self.week.per_project.get(project_id).copied().unwrap_or(0);
        let all = match self.all.per_project.get(project_id).copied() {
            Some(ms) => ms,
            None if self.all.entry_count == 0 && !self.entries.is_empty() => self
                .entries
                .iter()
                .filter(|e| e.project_id == project_id)
                .map(|e| e.duration_ms())
                .sum(),
            None => 0,
        };
        (week, all)
    }
}

/// Convert a core entry into its wire representation.
pub fn entry_to_view(e: &timetrack_core::Entry) -> EntryView {
    EntryView {
        id: e.id.clone(),
        project_id: e.project_id.clone(),
        description: e.description.clone(),
        started_at: e.started_at,
        ended_at: e.ended_at,
        source: match e.source {
            timetrack_core::EntrySource::Manual => EntrySource::Manual,
            timetrack_core::EntrySource::QuickAdd => EntrySource::QuickAdd,
            timetrack_core::EntrySource::External => EntrySource::External,
        },
        note: e.note.clone(),
    }
}

/// Convert a core project into its wire representation.
pub fn project_to_view(p: &timetrack_core::Project) -> ProjectView {
    ProjectView {
        id: p.id.clone(),
        name: p.name.clone(),
        colour: p.colour,
        archived: p.archived,
    }
}

/// Why the service refused a call, in typed form.
///
/// This travels in the socket reply frame. `Display` reproduces exactly
/// the strings clients have always shown, so CLI output and scripts
/// keep matching: match on the variants, never on the text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum ServiceError {
    /// A rule refused the operation; the wording is the rule's own.
    #[error("{0}")]
    Rule(#[from] timetrack_core::RuleError),
    /// An export scope other than `week`, `month` or `all`.
    #[error("unknown export scope '{0}' (use week, month or all)")]
    UnknownExportScope(String),
    /// Persisting failed; the message is the storage layer's own wording.
    #[error("{0}")]
    Storage(String),
    /// A refusal from a service older than the typed errors: its plain-text
    /// message, kept verbatim for display.
    #[error("{0}")]
    Other(String),
}

/// Why a client call failed, without anyone matching on message text.
///
/// The `?` operator still converts this into `anyhow::Error` wherever the
/// caller only needs the wording, so call sites that just propagate errors
/// keep compiling unchanged.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClientError {
    /// The service refused the call: validation, bad scope, persist failure.
    /// `Display` is the service's own wording, unchanged from before.
    #[error("{0}")]
    Service(#[from] ServiceError),
    /// Nothing listens on the service socket: the service is not running.
    #[error("the timetrack service is not running")]
    NoService,
    /// The client and the service are different versions. Kept variant
    /// for the GUI/tray skew surfaces built around it; the socket
    /// transport reports skew as transport failures today.
    #[error(
        "service spoke an unknown interface or method ({name}); client and service are different versions"
    )]
    VersionMismatch {
        /// What the peer answered with, named for the log line.
        name: String,
    },
    /// Any other transport-level failure, with its own wording.
    #[error("{0}")]
    Transport(String),
}

/// The "service is not running" guidance, shared by the GUI footer and
/// the tray tooltip so both clients say the same thing. Shown for
/// [`ClientError::NoService`].
///
/// The service ships inside the AppImage and starts with it: there is no
/// host install to point at, so the hint names the image, not a binary.
pub fn no_service_hint() -> String {
    "the timetrack service is not running. Launch the TimeTrack AppImage (it starts its bundled service), then retry".to_string()
}

/// Post a desktop notification (`org.freedesktop.Notifications`), best
/// effort by contract: callers use it to confirm work that has already
/// happened (a launcher quick-add, a tray that cannot appear), so a
/// missing notification daemon must surface as an error the caller can
/// log and drop — never as a reason to undo or fail the work itself.
/// Shared here, with the other client-facing bus helpers, so the CLI and
/// the tray speak to the notification server one way (#105).
pub async fn desktop_notify(summary: &str, body: &str) -> Result<(), ClientError> {
    // Not `ClientError::from_bus`: that mapping reads `ServiceUnknown`
    // as "the timetrack service is not running", which here would blame
    // the wrong service — it is the notification daemon that is absent.
    let unavailable =
        |e: zbus::Error| ClientError::Transport(format!("notification server unavailable: {e}"));
    let conn = zbus::connection::Builder::session()
        .build()
        .await
        .map_err(unavailable)?;
    let proxy = zbus::Proxy::new(
        &conn,
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
    )
    .await
    .map_err(unavailable)?;
    let hints: std::collections::HashMap<String, zbus::Variant<'_>> =
        std::collections::HashMap::new();
    proxy
        .call_method(
            "Notify",
            &(
                "TimeTrack",
                0u32,
                "org.sequ.timetrack",
                summary,
                body,
                Vec::<String>::new(),
                hints,
                -1i32,
            ),
        )
        .await
        .map_err(unavailable)?;
    Ok(())
}

/// The version-skew guidance, shared by the GUI footer and the tray
/// tooltip. Names the client (`GUI`, `tray`, ...) so each client states who
/// disagrees with the service. Shown for [`ClientError::VersionMismatch`].
pub fn version_mismatch_hint(client: &str) -> String {
    format!(
        "The {client} and the service are different versions.\nReinstall whichever one is older so both speak\nthe same socket protocol."
    )
}

/// A connection to the running service over its local socket (§16).
/// Cheap to clone — it is only a socket path; each call opens its own
/// connection (see `socket` module docs).
#[derive(Clone)]
pub struct Client {
    socket: SocketClient,
}

/// How a bounded wait for `Changed` ended (#103).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangedWait {
    /// The service announced a persisted mutation: refresh now.
    Changed,
    /// Nothing arrived inside the bound: the caller's turn to act.
    TimedOut,
    /// The stream ended (the connection went away): resubscribe or fall
    /// back to polling rather than spinning on a dead stream.
    Ended,
}

/// A subscription to the service's `Changed` announcements (#103).
///
/// Coalescing is inexact by design — two mutations may arrive as one
/// refresh — because the snapshot fetched after any signal is the
/// service's current state either way.
pub struct ChangedStream {
    inner: ChangedInner,
}

enum ChangedInner {
    #[cfg(unix)]
    Socket(async_io::Async<std::os::unix::net::UnixStream>),
    #[cfg(not(unix))]
    SocketChannel(std::sync::mpsc::Receiver<()>),
}

impl ChangedStream {
    /// The next `Changed`, or `None` when the stream has ended.
    pub async fn next(&mut self) -> Option<()> {
        match &mut self.inner {
            #[cfg(unix)]
            ChangedInner::Socket(stream) => loop {
                match socket::read_frame_async(stream).await {
                    Ok(Some(socket::Frame::Changed)) => return Some(()),
                    Ok(Some(_)) => continue,
                    _ => return None,
                }
            },
            #[cfg(not(unix))]
            ChangedInner::SocketChannel(rx) => loop {
                match rx.try_recv() {
                    Ok(()) => return Some(()),
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => return None,
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        async_io::Timer::after(Duration::from_millis(25)).await;
                    }
                }
            },
        }
    }

    /// Wait at most `bound` for the next `Changed`.
    pub async fn wait_timeout(&mut self, bound: Duration) -> ChangedWait {
        futures_lite::future::race(
            async {
                match self.next().await {
                    Some(()) => ChangedWait::Changed,
                    None => ChangedWait::Ended,
                }
            },
            async {
                async_io::Timer::after(bound).await;
                ChangedWait::TimedOut
            },
        )
        .await
    }
}

impl Client {
    /// Connect to the service.
    ///
    /// This proves nothing yet — a socket client is just a path, so a
    /// missing service surfaces at the first method call as
    /// `NoService`, the same contract the bus client had. Every call
    /// carries [`CALL_TIMEOUT`], so a wedged service becomes an error,
    /// never a hang (issue #43).
    pub async fn connect() -> Result<Self, ClientError> {
        Ok(Client {
            socket: SocketClient::at_default_path(),
        })
    }

    fn call(&self, request: SocketRequest) -> Result<SocketResponse, ClientError> {
        self.socket.call(request)
    }

    pub async fn snapshot(&self) -> Result<Snapshot, ClientError> {
        self.call(SocketRequest::Snapshot)?.snapshot()
    }

    /// Subscribe to the service's `Changed` announcements (#103).
    pub async fn changed_stream(&self) -> Result<ChangedStream, ClientError> {
        #[cfg(unix)]
        {
            let stream = self.socket.subscribe_changed().await?;
            Ok(ChangedStream {
                inner: ChangedInner::Socket(stream),
            })
        }
        #[cfg(not(unix))]
        {
            let rx = self.socket.subscribe_changed_channel()?;
            Ok(ChangedStream {
                inner: ChangedInner::SocketChannel(rx),
            })
        }
    }

    // --- the four v1 entry methods (REQUIREMENTS §5) ---

    pub async fn add(
        &self,
        project_id: &str,
        description: &str,
        started_at: i64,
        ended_at: i64,
    ) -> Result<EntryView, ClientError> {
        self.call(SocketRequest::Add {
            project_id: project_id.into(),
            description: description.into(),
            started_at,
            ended_at,
        })?
        .entry()
    }

    pub async fn add_duration(
        &self,
        project_id: &str,
        description: &str,
        duration_ms: i64,
    ) -> Result<EntryView, ClientError> {
        self.call(SocketRequest::AddDuration {
            project_id: project_id.into(),
            description: description.into(),
            duration_ms,
        })?
        .entry()
    }

    pub async fn add_duration_ending(
        &self,
        project_id: &str,
        description: &str,
        duration_ms: i64,
        ended_at: i64,
    ) -> Result<EntryView, ClientError> {
        self.call(SocketRequest::AddDurationEnding {
            project_id: project_id.into(),
            description: description.into(),
            duration_ms,
            ended_at,
        })?
        .entry()
    }

    pub async fn quick_add(
        &self,
        project_id: &str,
        duration_ms: i64,
    ) -> Result<EntryView, ClientError> {
        self.call(SocketRequest::QuickAdd {
            project_id: project_id.into(),
            duration_ms,
        })?
        .entry()
    }

    // --- editing ---

    pub async fn set_times(
        &self,
        id: &str,
        started_at: i64,
        ended_at: i64,
    ) -> Result<EntryView, ClientError> {
        self.call(SocketRequest::SetTimes {
            id: id.into(),
            started_at,
            ended_at,
        })?
        .entry()
    }

    pub async fn set_text(&self, id: &str, description: &str) -> Result<EntryView, ClientError> {
        self.call(SocketRequest::SetText {
            id: id.into(),
            description: description.into(),
        })?
        .entry()
    }

    pub async fn set_project(&self, id: &str, project_id: &str) -> Result<EntryView, ClientError> {
        self.call(SocketRequest::SetProject {
            id: id.into(),
            project_id: project_id.into(),
        })?
        .entry()
    }

    pub async fn undo_quick_add(&self, id: &str) -> Result<(), ClientError> {
        self.call(SocketRequest::UndoQuickAdd { id: id.into() })?
            .unit()
    }

    pub async fn delete_entry(&self, id: &str) -> Result<(), ClientError> {
        self.call(SocketRequest::DeleteEntry { id: id.into() })?
            .unit()
    }

    pub async fn split(&self, id: &str, at_ms: i64) -> Result<(EntryView, EntryView), ClientError> {
        self.call(SocketRequest::Split {
            id: id.into(),
            at_ms,
        })?
        .entry_pair()
    }

    pub async fn merge(&self, ids: &[String]) -> Result<EntryView, ClientError> {
        self.call(SocketRequest::Merge { ids: ids.to_vec() })?
            .entry()
    }

    // --- projects ---

    pub async fn add_project(&self, name: &str) -> Result<ProjectView, ClientError> {
        self.call(SocketRequest::AddProject { name: name.into() })?
            .project()
    }

    /// Rename and/or recolour a project. Empty `name` keeps the current
    /// name; `colour` -1 keeps, -2 clears (the service's sentinels).
    pub async fn update_project(
        &self,
        id: &str,
        name: &str,
        colour: i64,
    ) -> Result<ProjectView, ClientError> {
        self.call(SocketRequest::UpdateProject {
            id: id.into(),
            name: name.into(),
            colour,
        })?
        .project()
    }

    pub async fn set_archived(&self, id: &str, archived: bool) -> Result<ProjectView, ClientError> {
        self.call(SocketRequest::SetArchived {
            id: id.into(),
            archived,
        })?
        .project()
    }

    pub async fn delete_project(&self, id: &str) -> Result<(), ClientError> {
        self.call(SocketRequest::DeleteProject { id: id.into() })?
            .unit()
    }

    // --- export ---

    pub async fn export_csv(&self, scope: &str) -> Result<String, ClientError> {
        self.call(SocketRequest::ExportCsv {
            scope: scope.into(),
        })?
        .csv()
    }

    pub async fn export_csv_at(&self, scope: &str, anchor_ms: i64) -> Result<String, ClientError> {
        self.call(SocketRequest::ExportCsvAt {
            scope: scope.into(),
            anchor_ms,
        })?
        .csv()
    }

    pub async fn export_csv_range(&self, from_ms: i64, to_ms: i64) -> Result<String, ClientError> {
        self.call(SocketRequest::ExportCsvRange { from_ms, to_ms })?
            .csv()
    }

    // --- external sources (§19) ---

    pub async fn add_source(
        &self,
        kind: &str,
        locator: &str,
        project_id: Option<&str>,
        label: &str,
    ) -> Result<SourceView, ClientError> {
        self.call(SocketRequest::AddSource {
            kind: kind.into(),
            locator: locator.into(),
            project_id: project_id.map(str::to_string),
            label: label.into(),
        })?
        .source()
    }

    pub async fn sources(&self) -> Result<Vec<SourceView>, ClientError> {
        self.call(SocketRequest::ListSources)?.sources()
    }

    pub async fn refresh_source(&self, id: Option<&str>) -> Result<RefreshView, ClientError> {
        self.call(SocketRequest::RefreshSource {
            id: id.map(str::to_string),
        })?
        .refresh()
    }

    pub async fn suggestions(&self) -> Result<Vec<SuggestionView>, ClientError> {
        self.call(SocketRequest::ListSuggestions)?.suggestions()
    }

    pub async fn confirm_suggestion(
        &self,
        id: &str,
        project_id: Option<&str>,
    ) -> Result<EntryView, ClientError> {
        self.call(SocketRequest::ConfirmSuggestion {
            id: id.into(),
            project_id: project_id.map(str::to_string),
        })?
        .entry()
    }

    pub async fn dismiss_suggestion(&self, id: &str) -> Result<(), ClientError> {
        self.call(SocketRequest::DismissSuggestion { id: id.into() })?
            .unit()
    }

    pub async fn set_note(&self, id: &str, note: Option<&str>) -> Result<EntryView, ClientError> {
        self.call(SocketRequest::SetNote {
            id: id.into(),
            note: note.map(str::to_string),
        })?
        .entry()
    }

    /// Poll until the service answers, or give up.
    ///
    /// The first method call is what proves the service, so that is
    /// what this waits for. One auto-spawn attempt (§16) is made along
    /// the way: the AppRun script used to be the only thing that ever
    /// started the service, so a bare CLI launch outside it met a dead
    /// store. Gives up with the shared "not running" guidance.
    pub async fn wait_for_service(timeout: Duration) -> anyhow::Result<Self> {
        let deadline = std::time::Instant::now() + timeout;
        let mut spawned = false;
        loop {
            let client = Client::connect().await?;
            match client.snapshot().await {
                Ok(_) => return Ok(client),
                Err(ClientError::NoService) => {
                    if !spawned {
                        socket::try_spawn_service();
                        spawned = true;
                    }
                }
                Err(e) => return Err(anyhow::Error::new(e)),
            }
            if std::time::Instant::now() >= deadline {
                anyhow::bail!("{}", no_service_hint());
            }
            // `async_io::Timer`, not a tokio sleep: this function runs
            // inside a program driven by `async_io::block_on` (the CLI),
            // which has no tokio reactor.
            async_io::Timer::after(Duration::from_millis(200)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> EntryView {
        EntryView {
            id: "e1".into(),
            project_id: "p1".into(),
            description: "work".into(),
            started_at: 1_000,
            ended_at: 4_500,
            source: EntrySource::Manual,
            note: None,
        }
    }

    #[test]
    fn snapshot_round_trips_through_json() {
        // Clients deserialise exactly this shape, so a struct change that
        // breaks the round trip would break them at runtime.
        let mut snap = Snapshot {
            entries: vec![entry()],
            local_offset_ms: 3_600_000,
            ..Default::default()
        };
        snap.projects.push(ProjectView {
            id: "p1".into(),
            name: "Work".into(),
            colour: Some(0x2ea043),
            archived: false,
        });
        snap.week.total_ms = 3_500;
        snap.all.total_ms = 3_500;
        snap.all.per_project.insert("p1".into(), 3_500);
        snap.all.entry_count = 1;
        let text = serde_json::to_string(&snap).unwrap();
        assert_eq!(serde_json::from_str::<Snapshot>(&text).unwrap(), snap);
    }

    #[test]
    fn default_snapshot_is_empty_but_valid() {
        let text = serde_json::to_string(&Snapshot::default()).unwrap();
        let back: Snapshot = serde_json::from_str(&text).unwrap();
        assert!(back.entries.is_empty());
        assert!(back.projects.is_empty());
        assert_eq!(back.total_ms, 0);
    }

    #[test]
    fn a_snapshot_without_a_tz_field_is_detectably_nameless() {
        // #102: an old service's JSON has no `tz` field. It must arrive
        // empty — never defaulted to "UTC" — so a new client can refuse
        // to parse typed local times against a guessed zone.
        let back: Snapshot = serde_json::from_str(
            r#"{"projects":[],"entries":[],"week":{"total_ms":0,"per_project":{},"entry_count":0},"month":{"total_ms":0,"per_project":{},"entry_count":0},"total_ms":0}"#,
        )
        .unwrap();
        assert_eq!(back.tz, "");
    }

    #[test]
    fn duration_needs_no_clock() {
        assert_eq!(entry().duration_ms(), 3_500);
    }

    #[test]
    fn negative_duration_clamps_to_zero() {
        let mut e = entry();
        e.started_at = 5_000;
        e.ended_at = 1_000;
        assert_eq!(e.duration_ms(), 0);
    }

    #[test]
    fn empty_description_gets_a_placeholder() {
        let mut e = entry();
        e.description = String::new();
        assert_eq!(e.label(), "(no description)");
    }

    #[test]
    fn there_is_no_running_entry_on_the_wire() {
        // A regression guard: the stopwatch's `running` field must not creep
        // back into the contract, because it would imply a state that no
        // longer exists in the model.
        let text = serde_json::to_string(&Snapshot::default()).unwrap();
        assert!(
            !text.contains("running"),
            "unexpected running field: {text}"
        );
    }

    #[test]
    fn entry_conversion_preserves_the_source() {
        let core = timetrack_core::Entry {
            id: "e9".into(),
            project_id: "p2".into(),
            description: "note".into(),
            started_at: 5,
            ended_at: 10,
            source: timetrack_core::EntrySource::QuickAdd,
            note: Some("n".into()),
        };
        let view = entry_to_view(&core);
        assert_eq!(view.source, EntrySource::QuickAdd);
        assert_eq!(view.duration_ms(), 5);
        assert_eq!(view.note.as_deref(), Some("n"));
    }

    #[test]
    fn totals_view_round_trips() {
        let mut t = TotalsView {
            total_ms: 100,
            entry_count: 1,
            ..Default::default()
        };
        t.per_project.insert("p1".into(), 100);
        let text = serde_json::to_string(&t).unwrap();
        assert_eq!(serde_json::from_str::<TotalsView>(&text).unwrap(), t);
    }

    fn snapshot_with_truncated_entries() -> Snapshot {
        // A large store: the snapshot carries one recent entry, but the
        // service aggregates cover all three hours on p1.
        let mut snap = Snapshot {
            entries: vec![EntryView {
                project_id: "p1".into(),
                ended_at: 3_600_000,
                ..entry()
            }],
            ..Default::default()
        };
        snap.week.per_project.insert("p1".into(), 3_600_000);
        snap.week.total_ms = 3_600_000;
        snap.week.entry_count = 1;
        snap.all.per_project.insert("p1".into(), 3 * 3_600_000);
        snap.all.total_ms = 3 * 3_600_000;
        snap.all.entry_count = 3;
        snap.total_ms = 3 * 3_600_000;
        snap
    }

    #[test]
    fn project_totals_come_from_service_aggregates() {
        // The all-time column must not be re-summed from `entries`: with a
        // truncated entry list that would under-report.
        let snap = snapshot_with_truncated_entries();
        assert_eq!(snap.project_totals("p1"), (3_600_000, 3 * 3_600_000));
    }

    #[test]
    fn project_totals_fall_back_for_old_services() {
        // A service predating the all-time breakdown sends no `all`: the
        // entries are then the only source, so sum them as before.
        let mut snap = Snapshot {
            entries: vec![entry(), entry()],
            ..Default::default()
        };
        snap.week.per_project.insert("p1".into(), 7_000);
        assert_eq!(snap.project_totals("p1"), (7_000, 7_000));
    }

    #[test]
    fn project_totals_are_zero_when_there_is_nothing() {
        assert_eq!(Snapshot::default().project_totals("p1"), (0, 0));
        // A project the service never saw totals zero, not the fallback:
        // the aggregates are populated, so `entries` stay out of it.
        let snap = snapshot_with_truncated_entries();
        assert_eq!(snap.project_totals("nope"), (0, 0));
    }

    // --- shared client guidance ---

    #[test]
    fn service_hints_are_shared_client_wording() {
        // The GUI footer and the tray tooltip render these verbatim, so
        // both clients say the same thing: pin the exact strings here.
        assert_eq!(
            no_service_hint(),
            "the timetrack service is not running. Launch the TimeTrack AppImage (it starts its bundled service), then retry"
        );
        assert_eq!(
            version_mismatch_hint("GUI"),
            "The GUI and the service are different versions.\nReinstall whichever one is older so both speak\nthe same socket protocol."
        );
        assert!(
            version_mismatch_hint("tray").starts_with("The tray and the service"),
            "each client names itself"
        );
    }

    // --- typed errors ---

    #[test]
    fn service_errors_keep_their_display_strings() {
        // CLI output and scripts match on these wordings, so the typed enum
        // must reproduce them exactly.
        assert_eq!(
            ServiceError::Rule(timetrack_core::RuleError::DuplicateProject("Work".into()))
                .to_string(),
            "a project named 'Work' already exists"
        );
        assert_eq!(
            ServiceError::Rule(timetrack_core::RuleError::NegativeDuration).to_string(),
            "an entry must not end before it starts"
        );
        assert_eq!(
            ServiceError::Rule(timetrack_core::RuleError::NotQuickAdd("e1".into())).to_string(),
            "entry 'e1' was not created by a quick add, so it cannot be undone"
        );
        assert_eq!(
            ServiceError::UnknownExportScope("everything".into()).to_string(),
            "unknown export scope 'everything' (use week, month or all)"
        );
        assert_eq!(
            ServiceError::Storage("could not write /s: denied".into()).to_string(),
            "could not write /s: denied"
        );
    }

    #[test]
    fn client_errors_convert_for_propagating_callers() {
        // Call sites that only use `?` keep working: the wording survives.
        fn propagated() -> anyhow::Result<()> {
            Err(ClientError::NoService)?;
            Ok(())
        }
        assert!(propagated().is_err());
        assert_eq!(
            ClientError::Service(ServiceError::Rule(
                timetrack_core::RuleError::NegativeDuration
            ))
            .to_string(),
            "an entry must not end before it starts"
        );
    }

    // --- the call timeout ---

    #[test]
    fn the_call_timeout_is_ten_seconds() {
        // Pinned: the value is a deliberate trade-off, not an accident. It
        // has to be long enough that a slow but healthy service never trips
        // it (a snapshot takes milliseconds), and short enough that a wedged
        // service is reported rather than waited on forever.
        assert_eq!(CALL_TIMEOUT, Duration::from_secs(10));
        assert!(CALL_TIMEOUT >= Duration::from_secs(5));
        assert!(CALL_TIMEOUT <= Duration::from_secs(30));
    }
}
