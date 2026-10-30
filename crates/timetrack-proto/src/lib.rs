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
//! # Why payloads are JSON strings
//!
//! Every method takes and returns a single `s` holding JSON. A hand-written
//! D-Bus signature per call would mean two definitions of every type -- one
//! in the interface, one in the client -- and they would drift. With JSON the
//! serde structs below are the single definition, so a field added on the
//! service side arrives on the client side with no signature change at all.
//!
//! # The running timer is gone
//!
//! There is deliberately no `Running` field, `Start` or `Stop` method: every
//! entry is closed (REQUIREMENTS §7). The old stopwatch surface was removed
//! rather than deprecated, so nothing here can resurrect it.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Well-known bus name. Also the app-id used by the desktop entry.
pub const BUS_NAME: &str = "org.sequ.timetrack";

/// Object path of the single exported interface.
pub const OBJECT_PATH: &str = "/org/sequ/timetrack";

/// Interface name.
pub const INTERFACE: &str = "org.sequ.timetrack.Entries";

/// How long a method call may take before the client gives up on it.
///
/// zbus has no default method timeout, so without this a service that
/// accepts the connection and then never answers hangs the caller forever:
/// the TUI freezes inside its refresh and even `q` stops working (issue #43).
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntrySource {
    Manual,
    QuickAdd,
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
/// This travels as JSON in the D-Bus `Failed` message body, so the D-Bus
/// signatures never change. `Display` reproduces exactly the strings clients
/// have always shown, so CLI output and scripts keep matching: match on the
/// variants, never on the text.
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

impl ServiceError {
    /// Decode a `org.freedesktop.DBus.Error.Failed` message body back into
    /// the typed refusal. Anything that is not our JSON is a refusal from an
    /// older service; keep its text rather than failing the decode.
    pub fn from_failed_body(body: &str) -> Self {
        serde_json::from_str(body).unwrap_or_else(|_| ServiceError::Other(body.to_string()))
    }

    /// Encode for the D-Bus `Failed` message body.
    pub fn to_failed_body(&self) -> String {
        serde_json::to_string(self).expect("service errors are always serializable")
    }
}

/// D-Bus error names that decide a call's fate. These come from the D-Bus
/// specification and never change; matching on them exactly is protocol
/// dispatch, not message-text sniffing.
const FAILED: &str = "org.freedesktop.DBus.Error.Failed";
const SERVICE_UNKNOWN: &str = "org.freedesktop.DBus.Error.ServiceUnknown";
const NAME_HAS_NO_OWNER: &str = "org.freedesktop.DBus.Error.NameHasNoOwner";
const UNKNOWN_INTERFACE: &str = "org.freedesktop.DBus.Error.UnknownInterface";
const UNKNOWN_METHOD: &str = "org.freedesktop.DBus.Error.UnknownMethod";

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
    /// Nothing owns the well-known name: the service is not running.
    #[error("the timetrack service is not running")]
    NoService,
    /// The service answered unknown-interface or unknown-method: the client
    /// and the service are different versions.
    #[error(
        "service spoke an unknown interface or method ({name}); client and service are different versions"
    )]
    VersionMismatch {
        /// The D-Bus error name the service answered with.
        name: String,
    },
    /// Any other bus-level failure, with the bus's own wording.
    #[error("{0}")]
    Transport(String),
}

impl ClientError {
    /// Sort a bus failure by its error NAME, never by message text: only a
    /// `Failed` body is decoded further, and only as typed JSON.
    pub fn from_bus(error: zbus::Error) -> Self {
        match error {
            zbus::Error::MethodError(name, detail, _) => {
                Self::from_error_name(name.as_str(), detail.as_deref())
            }
            // A call that ran out of time: the service took the connection
            // and then stopped answering. That is a transport failure, not a
            // refusal, and it must not look like a missing service.
            zbus::Error::InputOutput(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                ClientError::Transport(TIMEOUT_WORDING.to_string())
            }
            other => ClientError::Transport(other.to_string()),
        }
    }

    /// Sort a D-Bus error reply. `name` is one of the spec-fixed error names;
    /// `detail` is the message body, JSON only for our own `Failed` errors.
    fn from_error_name(name: &str, detail: Option<&str>) -> Self {
        if name == SERVICE_UNKNOWN || name == NAME_HAS_NO_OWNER {
            ClientError::NoService
        } else if name == UNKNOWN_INTERFACE || name == UNKNOWN_METHOD {
            ClientError::VersionMismatch {
                name: name.to_string(),
            }
        } else if name == FAILED {
            ClientError::Service(ServiceError::from_failed_body(detail.unwrap_or("")))
        } else {
            ClientError::Transport(format!("{name}: {}", detail.unwrap_or("no details")))
        }
    }
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
        .map_err(unavailable)?
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
    let hints: std::collections::HashMap<String, zbus::zvariant::Value<'_>> =
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
        "The {client} and the service are different versions.\nReinstall whichever one is older so both speak\n{INTERFACE}."
    )
}

/// A connection to the running service.
#[derive(Clone)]
pub struct Client {
    proxy: zbus::Proxy<'static>,
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

/// A subscription to the service's `Changed` signal (#103).
///
/// Wraps zbus' signal stream so client crates need no futures traits of
/// their own: await [`ChangedStream::next`] for the next announcement,
/// or [`ChangedStream::wait_timeout`] to share a wait with a fallback
/// timer. Coalescing is inexact by design — two mutations may arrive as
/// one refresh — because the snapshot fetched after any signal is the
/// service's current state either way.
pub struct ChangedStream {
    inner: std::pin::Pin<Box<zbus::proxy::SignalStream<'static>>>,
}

impl ChangedStream {
    /// The next `Changed`, or `None` when the stream has ended.
    ///
    /// zbus also yields the proxy's internal `NameOwnerChanged`
    /// bookkeeping messages on this stream; only the service's own
    /// member counts, or merely subscribing would read as a change.
    pub async fn next(&mut self) -> Option<()> {
        loop {
            let msg = futures_lite::StreamExt::next(&mut self.inner).await?;
            if msg
                .header()
                .member()
                .is_some_and(|m| m.as_str() == "Changed")
            {
                return Some(());
            }
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
    /// The connection carries a default method timeout ([`CALL_TIMEOUT`]), so
    /// every call this client makes is abandoned if the service stops
    /// answering: a wedged service becomes an error, never a hang
    /// (issue #43).
    pub async fn connect() -> Result<Self, ClientError> {
        // NOTE: do *not* call `.name(BUS_NAME)` here. On a client connection
        // that asks the bus to hand us the well-known name, which only ever
        // succeeds if nothing else owns it -- so it would block forever
        // against a healthy service and report success when the service is
        // absent. A client must connect anonymously and address the owner by
        // name at call time, which is what `Proxy::new` does.
        //
        // This connects to the bus, it does not reach the service: a missing
        // service surfaces at the first method call, as `NoService`.
        let conn = zbus::connection::Builder::session()
            .map_err(ClientError::from_bus)?
            // The timeout lives on the connection, so every method inherits
            // it -- there is no per-call timeout to forget.
            .method_timeout(CALL_TIMEOUT)
            .build()
            .await
            .map_err(ClientError::from_bus)?;
        let proxy = zbus::Proxy::new(&conn, BUS_NAME, OBJECT_PATH, INTERFACE)
            .await
            .map_err(ClientError::from_bus)?;
        Ok(Client { proxy })
    }

    /// Call a no-argument method and decode its JSON reply.
    async fn call<T: serde::de::DeserializeOwned>(&self, method: &str) -> Result<T, ClientError> {
        let reply = self
            .proxy
            .call_method(method, &())
            .await
            .map_err(ClientError::from_bus)?;
        let wire: String = reply
            .body()
            .deserialize()
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        serde_json::from_str(&wire).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Call a one-argument method and decode its JSON reply.
    ///
    /// `A` needs zbus's own `DynamicType`, not just `Serialize`: zbus derives
    /// the D-Bus signature from the type, so a plain `Serialize` bound is not
    /// enough to call a method.
    async fn call1<
        T: serde::de::DeserializeOwned,
        A: serde::Serialize + zbus::zvariant::DynamicType,
    >(
        &self,
        method: &str,
        arg: &A,
    ) -> Result<T, ClientError> {
        let reply = self
            .proxy
            .call_method(method, arg)
            .await
            .map_err(ClientError::from_bus)?;
        let wire: String = reply
            .body()
            .deserialize()
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        serde_json::from_str(&wire).map_err(|e| ClientError::Transport(e.to_string()))
    }

    pub async fn snapshot(&self) -> Result<Snapshot, ClientError> {
        self.call("Snapshot").await
    }

    /// Subscribe to the service's `Changed` signal (#103).
    ///
    /// Subscribing only adds a bus match rule, so it succeeds even against
    /// a service (or a bus) that never emits: old services simply never
    /// fire, and callers keep a slow fallback poll for exactly that case.
    pub async fn changed_stream(&self) -> Result<ChangedStream, ClientError> {
        let inner = self
            .proxy
            .receive_signal("Changed")
            .await
            .map_err(ClientError::from_bus)?;
        Ok(ChangedStream {
            inner: Box::pin(inner),
        })
    }

    // --- the four v1 entry methods (REQUIREMENTS §5) ---

    /// Method 1: an explicit start and end.
    pub async fn add(
        &self,
        project_id: &str,
        description: &str,
        started_at: i64,
        ended_at: i64,
    ) -> Result<EntryView, ClientError> {
        self.call1("Add", &(project_id, description, started_at, ended_at))
            .await
    }

    /// Method 2: a duration ending now.
    pub async fn add_duration(
        &self,
        project_id: &str,
        description: &str,
        duration_ms: i64,
    ) -> Result<EntryView, ClientError> {
        self.call1("AddDuration", &(project_id, description, duration_ms))
            .await
    }

    /// Method 3: a duration ending at a given instant.
    pub async fn add_duration_ending(
        &self,
        project_id: &str,
        description: &str,
        duration_ms: i64,
        ended_at: i64,
    ) -> Result<EntryView, ClientError> {
        self.call1(
            "AddDurationEnding",
            &(project_id, description, duration_ms, ended_at),
        )
        .await
    }

    /// Method 4: one of the quick-add buckets, ending now.
    pub async fn quick_add(
        &self,
        project_id: &str,
        duration_ms: i64,
    ) -> Result<EntryView, ClientError> {
        self.call1("QuickAdd", &(project_id, duration_ms)).await
    }

    // --- editing ---

    /// Move an entry's interval. This is method 1's "remove" side: it
    /// shortens by moving an endpoint and never deletes (REQUIREMENTS §5).
    pub async fn set_times(
        &self,
        id: &str,
        started_at: i64,
        ended_at: i64,
    ) -> Result<EntryView, ClientError> {
        self.call1("SetTimes", &(id, started_at, ended_at)).await
    }

    /// Edit an entry's description.
    pub async fn set_text(&self, id: &str, description: &str) -> Result<EntryView, ClientError> {
        self.call1("SetText", &(id, description)).await
    }

    /// Reassign an entry to another project.
    pub async fn set_project(&self, id: &str, project_id: &str) -> Result<EntryView, ClientError> {
        self.call1("SetProject", &(id, project_id)).await
    }

    /// Undo exactly the entry a quick-add created.
    pub async fn undo_quick_add(&self, id: &str) -> Result<(), ClientError> {
        self.proxy
            .call_method("UndoQuickAdd", &(id,))
            .await
            .map(|_| ())
            .map_err(ClientError::from_bus)
    }

    /// Explicit deletion. The only way an entry is removed on purpose.
    pub async fn delete_entry(&self, id: &str) -> Result<(), ClientError> {
        self.proxy
            .call_method("DeleteEntry", &(id,))
            .await
            .map(|_| ())
            .map_err(ClientError::from_bus)
    }

    pub async fn split(&self, id: &str, at_ms: i64) -> Result<(EntryView, EntryView), ClientError> {
        self.call1("Split", &(id, at_ms)).await
    }

    /// Merge entries into one spanning the union of their intervals.
    ///
    /// The result's duration is the union, so merging *overlapping* entries
    /// reduces the total by the overlap. Clients must say so before calling.
    pub async fn merge(&self, ids: &[String]) -> Result<EntryView, ClientError> {
        self.call1("Merge", &ids.to_vec()).await
    }

    // --- projects ---

    pub async fn add_project(&self, name: &str) -> Result<ProjectView, ClientError> {
        self.call1("AddProject", &name).await
    }

    /// Rename and/or recolour a project. Empty `name` keeps the current
    /// name; `colour` -1 keeps the current colour, -2 clears it, otherwise
    /// the low 24 bits become the new `0xRRGGBB` (mirrors the service's
    /// sentinels; an old service reads -2 as "keep").
    pub async fn update_project(
        &self,
        id: &str,
        name: &str,
        colour: i64,
    ) -> Result<ProjectView, ClientError> {
        self.call1("UpdateProject", &(id, name, colour)).await
    }

    pub async fn set_archived(&self, id: &str, archived: bool) -> Result<ProjectView, ClientError> {
        self.call1("SetArchived", &(id, archived)).await
    }

    pub async fn delete_project(&self, id: &str) -> Result<(), ClientError> {
        self.proxy
            .call_method("DeleteProject", &(id,))
            .await
            .map(|_| ())
            .map_err(ClientError::from_bus)
    }

    /// CSV export (REQUIREMENTS §10). `scope` is `week`, `month` or `all`;
    /// returns the CSV text including the header row.
    ///
    /// This stays on the old `ExportCsv` call, anchored at now by the
    /// service, so it works against both old and new services: current-period
    /// behaviour never needs the new call.
    pub async fn export_csv(&self, scope: &str) -> Result<String, ClientError> {
        self.call1("ExportCsv", &scope).await
    }

    /// CSV export anchored at `anchor_ms` (REQUIREMENTS §10). `scope` is
    /// `week`, `month` or `all`; the anchor selects which week or month and
    /// is ignored by `all`.
    ///
    /// Wire compat (per ADR-0001): this speaks the new `ExportCsvAt` call.
    /// Against an old service the call answers unknown-method/interface,
    /// which sorts as [`ClientError::VersionMismatch`] — a version message,
    /// never a wrong file. Current-period callers stay on [`Client::export_csv`]
    /// (the old `ExportCsv` call), which works against both old and new
    /// services; only a chosen anchor needs the new call, and only a chosen
    /// anchor can meet an old service.
    pub async fn export_csv_at(&self, scope: &str, anchor_ms: i64) -> Result<String, ClientError> {
        self.call1("ExportCsvAt", &(scope, anchor_ms)).await
    }

    /// Poll until the service appears on the bus, or give up.
    ///
    /// Connecting proves a *bus* is there, not that the service owns the
    /// name: `Client::connect` is anonymous, so it succeeds with no service
    /// around. The first method call is what proves the service, so that is
    /// what this waits for -- otherwise the name and the timeout promised a
    /// wait the function never did, and a missing service surfaced as a bare
    /// error from whatever command was being run.
    ///
    /// Gives up with the shared "not running" guidance, so every client says
    /// the same thing about a missing service.
    pub async fn wait_for_service(timeout: Duration) -> anyhow::Result<Self> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Ok(client) = Client::connect().await {
                match client.snapshot().await {
                    Ok(_) => return Ok(client),
                    // The bus is there and the service is not: wait for it.
                    Err(ClientError::NoService) => {}
                    // Any other failure is about this call, not about the
                    // service being absent: report it as it is.
                    Err(e) => return Err(anyhow::Error::new(e)),
                }
            }
            if std::time::Instant::now() >= deadline {
                anyhow::bail!("{}", no_service_hint());
            }
            // `async_io::Timer`, not a tokio sleep: this function runs
            // inside a program driven by `async_io::block_on` (the CLI and the
            // TUI), which has no tokio reactor -- a tokio sleep here panicked
            // with "there is no reactor running" the moment the session bus
            // was absent, which is exactly when this loop runs.
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
    fn bus_name_matches_object_path_convention() {
        assert_eq!(OBJECT_PATH, format!("/{}", BUS_NAME.replace('.', "/")));
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
            format!(
                "The GUI and the service are different versions.\nReinstall whichever one is older so both speak\n{INTERFACE}."
            )
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
    fn service_errors_round_trip_through_the_failed_body() {
        for e in [
            ServiceError::Rule(timetrack_core::RuleError::UnknownEntry("e9".into())),
            ServiceError::UnknownExportScope("bogus".into()),
            ServiceError::Storage("disk full".into()),
            ServiceError::Other("some old wording".into()),
        ] {
            assert_eq!(ServiceError::from_failed_body(&e.to_failed_body()), e);
        }
    }

    #[test]
    fn an_old_services_plain_text_survives_decoding() {
        // A service predating the typed errors sends its sentence, not JSON.
        // Decoding keeps the text verbatim instead of failing.
        let body = "an entry must not end before it starts";
        assert_eq!(
            ServiceError::from_failed_body(body),
            ServiceError::Other(body.into())
        );
    }

    fn failed(name: &str, detail: Option<&str>) -> ClientError {
        // Mirror of the D-Bus error-name dispatch, without needing a bus:
        // `from_error_name` is the whole decision.
        ClientError::from_error_name(name, detail)
    }

    #[test]
    fn missing_services_sort_as_no_service() {
        assert_eq!(
            failed(
                "org.freedesktop.DBus.Error.ServiceUnknown",
                Some("whatever the daemon says")
            ),
            ClientError::NoService
        );
        assert_eq!(
            failed("org.freedesktop.DBus.Error.NameHasNoOwner", None),
            ClientError::NoService
        );
    }

    #[test]
    fn stale_interfaces_sort_as_version_mismatch() {
        for name in [
            "org.freedesktop.DBus.Error.UnknownInterface",
            "org.freedesktop.DBus.Error.UnknownMethod",
        ] {
            assert_eq!(
                failed(name, None),
                ClientError::VersionMismatch {
                    name: name.to_string()
                }
            );
        }
    }

    #[test]
    fn anchored_export_against_an_old_service_refuses_as_version_mismatch() {
        // `export_csv_at` speaks the new `ExportCsvAt` call, which an old
        // service answers with unknown-method/interface. That must sort as a
        // version refusal — a version message, never a wrong file — while
        // current-period callers stay on `export_csv` (the old `ExportCsv`
        // call), which works against both old and new services.
        for name in [
            "org.freedesktop.DBus.Error.UnknownInterface",
            "org.freedesktop.DBus.Error.UnknownMethod",
        ] {
            let err = failed(name, None);
            assert_eq!(
                err,
                ClientError::VersionMismatch {
                    name: name.to_string()
                },
                "a chosen anchor against an old service must refuse, not serve now"
            );
            assert!(
                err.to_string().contains("different versions"),
                "version wording, got: {err}"
            );
        }
    }

    #[test]
    fn refusals_decode_to_typed_service_errors() {
        let typed = ServiceError::Rule(timetrack_core::RuleError::LastProject);
        assert_eq!(
            failed(
                "org.freedesktop.DBus.Error.Failed",
                Some(&typed.to_failed_body())
            ),
            ClientError::Service(typed)
        );
        // And an old service's sentence arrives intact, as a refusal.
        assert_eq!(
            failed(
                "org.freedesktop.DBus.Error.Failed",
                Some("a project named 'Work' already exists")
            ),
            ClientError::Service(ServiceError::Other(
                "a project named 'Work' already exists".into()
            ))
        );
    }

    #[test]
    fn dispatch_keys_on_names_not_on_message_text() {
        // Adversarial contents: a project literally named after a D-Bus
        // error must still dispatch as a validation refusal, and a refusal
        // mentioning the bus must not read as a missing service.
        let hostile = ServiceError::Rule(timetrack_core::RuleError::DuplicateProject(
            "org.freedesktop.DBus.Error.UnknownInterface".into(),
        ));
        assert!(
            matches!(
                failed(
                    "org.freedesktop.DBus.Error.Failed",
                    Some(&hostile.to_failed_body())
                ),
                ClientError::Service(_)
            ),
            "error names inside the message must not steer dispatch"
        );
        let legacy = "org.freedesktop.DBus.Error.ServiceUnknown was seen";
        assert!(
            matches!(
                failed("org.freedesktop.DBus.Error.Failed", Some(legacy)),
                ClientError::Service(_)
            ),
            "legacy text mentioning the bus must not read as no-service"
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

    #[test]
    fn a_timed_out_call_is_a_transport_failure_not_a_hang() {
        // zbus reports a call that ran out of time as an I/O error with
        // `ErrorKind::TimedOut`. It must sort as a transport failure, so no
        // client mistakes it for a missing service or a refusal, and it must
        // say what happened (issue #43).
        let err = ClientError::from_bus(zbus::Error::InputOutput(
            std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out").into(),
        ));
        assert!(
            matches!(err, ClientError::Transport(_)),
            "a timeout is a transport failure, got {err:?}"
        );
        assert_eq!(err.to_string(), TIMEOUT_WORDING);
        assert!(
            !err.to_string().contains("not running"),
            "a wedged service is not a missing one: {err}"
        );
    }

    #[test]
    fn other_io_failures_keep_their_own_wording() {
        // Only a timeout gets the timeout wording: a broken socket is a
        // different problem and keeps the bus's own words.
        let err = ClientError::from_bus(zbus::Error::InputOutput(
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "socket closed").into(),
        ));
        assert_eq!(err.to_string(), "I/O error: socket closed");
    }
}
