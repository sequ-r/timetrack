/* cli/main.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The TimeTrack terminal client.
//!
//! One-shot commands only (§18): each invocation does one thing, prints
//! one result, and exits -- `timetrack quick -m 15` between two commands
//! on some other project. Bare `timetrack` prints `status`, the
//! between-commands glance. There is no TUI; interactive editing is the
//! GUI's job. The store lives in the service, so closing the terminal
//! never loses anything.
//!
//! # The four entry methods
//!
//! Each of REQUIREMENTS §5's methods is a subcommand, named for what it does
//! rather than which UI gesture it came from:
//!
//! - `add`      explicit start and end (method 1)
//! - `duration` a duration ending now (method 2)
//! - `past`     a duration ending earlier (method 3)
//! - `quick`    one of the fixed buckets (method 4)
//!
//! And the three ways time comes back off, which are deliberately separate
//! commands because they are separate operations (REQUIREMENTS §5):
//!
//! - `shorten` moves an endpoint, keeping the entry
//! - `undo`    removes the entry a quick add created
//! - `delete`  removes any entry, on purpose
//!
//! Plus the two §8 restructuring operations:
//!
//! - `split` divides one entry in two at a moment (total unchanged)
//! - `merge` fuses entries into their union (overlaps collapse, so the total
//!   can shrink -- the confirmation says by how much)
//!
//! And the small edits: `set-text` rewrites a description, `set-project`
//! moves an entry between projects, `unarchive` reverses `archive`,
//! `delete-project` removes a project that has no entries left.

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::io::IsTerminal;
use std::time::Duration;
use timetrack_proto::{Client, ClientError, EntryView, ProjectView, Snapshot, no_service_hint};

/// How long to wait for the service to appear before giving up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Parser, Debug)]
#[command(
    name = "timetrack",
    version,
    about = "TimeTrack terminal client",
    long_about = "Control the TimeTrack service from the terminal.\n\n\
                  The service owns the store and does all the aggregation; this \
                  is a client, so closing the terminal never loses tracked time. \
                  With no subcommand, prints the status (this week's total first)."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

impl Cli {
    /// The command to run: bare `timetrack` is `status` (§18), the
    /// between-commands glance -- week total first, like the GUI hero.
    fn command_or_status(self) -> Command {
        self.command.unwrap_or(Command::Status { json: false })
    }
}

#[derive(Subcommand, Debug)]
enum Command {
    // --- the four add methods ---
    /// Method 1: record an explicit start and end.
    Add {
        /// Project to attribute the time to (id, name, or id prefix).
        /// Defaults to the first project.
        #[arg(short, long)]
        project: Option<String>,
        /// What you are working on.
        #[arg(short, long, default_value = "")]
        description: String,
        /// Start of the interval: `-90` for 90 minutes ago, or `09:30` today.
        #[arg(short = 'S', long, allow_hyphen_values = true)]
        start: String,
        /// End of the interval, in the same format as --start.
        #[arg(short = 'E', long, allow_hyphen_values = true)]
        end: String,
    },

    /// Method 2: record a duration ending now.
    Duration {
        /// Project to attribute the time to (id, name, or id prefix).
        /// Defaults to the first project.
        #[arg(short, long)]
        project: Option<String>,
        #[arg(short, long, default_value = "")]
        description: String,
        /// The duration, e.g. 90m, 1h30m or 45s.
        #[arg(short, long, value_name = "DURATION")]
        for_: String,
    },

    /// Method 3: record a duration ending at a given moment.
    Past {
        /// Project to attribute the time to (id, name, or id prefix).
        /// Defaults to the first project.
        #[arg(short, long)]
        project: Option<String>,
        #[arg(short, long, default_value = "")]
        description: String,
        /// The duration, e.g. 90m or 1h30m.
        #[arg(short, long, value_name = "DURATION")]
        for_: String,
        /// When it ended: `-120` for two hours ago, or `14:00` today.
        #[arg(short = 'E', long, allow_hyphen_values = true, default_value = "0")]
        ended: String,
    },

    /// Method 4: add one of the fixed buckets, ending now.
    Quick {
        /// Project to attribute the time to (id, name, or id prefix).
        /// Defaults to the first project.
        #[arg(short, long)]
        project: Option<String>,
        /// One of 5, 15, 30 or 60.
        #[arg(short, long)]
        minutes: i64,
        /// Confirm with a desktop notification as well as stdout. For
        /// launcher actions and keyboard shortcuts (#105), where stdout
        /// goes nowhere a person can see.
        #[arg(long)]
        notify: bool,
    },

    // --- the three ways time comes back off ---
    /// Move an entry's endpoints. Shortens without deleting.
    Shorten {
        /// The entry to change: its id or an unambiguous id prefix.
        id: String,
        /// New start, in the same format as add --start.
        #[arg(short = 'S', long, allow_hyphen_values = true)]
        start: Option<String>,
        /// New end.
        #[arg(short = 'E', long, allow_hyphen_values = true)]
        end: Option<String>,
    },

    /// Remove the entry a quick add created. Refuses anything else.
    Undo {
        /// The entry to remove: its id or an unambiguous id prefix.
        id: String,
    },

    /// Delete an entry outright.
    Delete {
        /// The entry to delete: its id or an unambiguous id prefix.
        id: String,
    },

    /// Rewrite an entry's description.
    SetText {
        /// The entry to change: its id or an unambiguous id prefix.
        id: String,
        /// The new description.
        #[arg(allow_hyphen_values = true)]
        text: String,
    },

    /// Move an entry to another project. Totals follow the entry.
    SetProject {
        /// The entry to move: its id or an unambiguous id prefix.
        id: String,
        /// The project id, name, or unambiguous id prefix.
        project: String,
    },

    // --- split and merge (REQUIREMENTS §8) ---
    /// Split one entry into two at a given moment. The total is unchanged.
    Split {
        /// The entry to split: its id or an unambiguous id prefix.
        id: String,
        /// Where to split: `-30` for 30 minutes ago, or `09:30` today.
        #[arg(short = 'A', long, allow_hyphen_values = true)]
        at: String,
    },

    /// Merge entries into one spanning earliest start to latest end.
    /// Merging overlapping entries shrinks the total by the overlap.
    Merge {
        /// The entries to merge: two or more ids or unambiguous id prefixes.
        #[arg(required = true)]
        ids: Vec<String>,
        /// Confirm without prompting. The prompt states how the total changes.
        #[arg(short, long)]
        yes: bool,
    },

    // --- projects ---
    /// Create a project.
    Project {
        /// The project name.
        name: String,
    },

    /// Rename a project.
    Rename {
        /// The project id, name, or unambiguous id prefix.
        id: String,
        /// The new name.
        name: String,
    },

    /// Recolour a project. Colour is RRGGBB hex, e.g. `2ea043`.
    Recolour {
        /// The project id, name, or unambiguous id prefix.
        id: String,
        /// Six hex digits, with or without a leading `#`.
        colour: String,
    },

    /// Archive a project, keeping its history in totals.
    Archive {
        /// The project to archive: its id, name, or unambiguous id prefix.
        id: String,
    },

    /// Unarchive a project, making it selectable again.
    Unarchive {
        /// The project to unarchive: its id, name, or unambiguous id prefix.
        id: String,
    },

    /// Delete a project. Refused when it still has entries, and when it is
    /// the last one.
    DeleteProject {
        /// The project to delete: its id, name, or unambiguous id prefix.
        id: String,
    },

    /// Export entries as CSV (REQUIREMENTS §10).
    Export {
        /// What to export: `week` (the ISO week), `month` or `all`.
        /// Old spellings (`this-week`, `current-week`, `all-time`, ...) keep
        /// working via the shared scope table.
        #[arg(long, default_value = "all")]
        scope: String,
        /// Day inside the week or month to export (`YYYY-MM-DD`, alongside
        /// the existing moment spellings). Omitted means now; ignored by
        /// `all` without warning.
        #[arg(long, allow_hyphen_values = true)]
        at: Option<String>,
        /// First local day of a custom range (`YYYY-MM-DD`), with `--to`
        /// (#133). A range replaces `--scope`/`--at`.
        #[arg(long, allow_hyphen_values = true)]
        from: Option<String>,
        /// Last local day of a custom range, inclusive.
        #[arg(long, allow_hyphen_values = true)]
        to: Option<String>,
        /// Write to this file instead of stdout.
        #[arg(short, long)]
        out: Option<std::path::PathBuf>,
    },

    // --- reading ---
    /// Print the service snapshot as JSON (projects, entries, totals,
    /// timezone): the scriptable whole picture.
    Snapshot,

    /// Print this week, this month and the all-time total.
    Status {
        /// Print JSON for scripts instead of the human totals.
        #[arg(long)]
        json: bool,
    },

    /// List tracked entries, most recent first.
    List {
        /// Show at most this many entries.
        #[arg(short, long, default_value_t = 20)]
        limit: usize,
        /// Print a JSON array for scripts instead of the human list.
        #[arg(long)]
        json: bool,
        /// Only entries whose description, project name, or note
        /// contains this text (case-insensitive). Totals are unaffected:
        /// this narrows the list, never the sums (#131).
        #[arg(long)]
        search: Option<String>,
    },

    /// Import a TimeTrack CSV export (#132): projects match by name
    /// (missing ones are created), rows already present are skipped, so
    /// importing the same file twice changes nothing the second time.
    Import {
        /// The export file to read.
        file: std::path::PathBuf,
        /// Report what would be imported, changing nothing.
        #[arg(long)]
        dry_run: bool,
    },

    // --- external sources (§19, socket-only) ---
    /// Register an external calendar: a local `.ics` file, an
    /// `http(s)://` URL serving one, or a CalDAV collection URL
    /// (`--kind caldav`; its secret goes to the OS credential store
    /// via `source-set-credential`, with TIMETRACK_CALDAV_PASSWORD as
    /// the headless fallback until one is stored). Read-only either way.
    SourceAdd {
        /// File path, or URL when it starts with `http://`/`https://`.
        locator: String,
        /// Source kind: `ics_file`, `ics_url`, or `caldav`. Inferred
        /// from the locator when omitted (`.ics` URL or file path).
        #[arg(long)]
        kind: Option<String>,
        /// Project its suggestions confirm into (id, name, or id prefix).
        #[arg(short, long)]
        project: Option<String>,
        /// Display label; defaults to the file or URL's last part.
        #[arg(long, default_value = "")]
        label: String,
    },

    /// Edit a registered external source (#153): kind, locator, label
    /// and project mapping. Omitted flags keep the current value; the
    /// service re-validates the result exactly as on registration (bad
    /// kind, empty locator, a CalDAV password in the URL, or another
    /// source's identity all refuse).
    SourceUpdate {
        /// Source id (from `sources`).
        id: String,
        /// New kind: `ics_file`, `ics_url`, or `caldav`.
        #[arg(long)]
        kind: Option<String>,
        /// New file path or URL.
        #[arg(long)]
        locator: Option<String>,
        /// New project mapping (id, name, or id prefix).
        #[arg(short, long, conflicts_with = "clear_project")]
        project: Option<String>,
        /// Drop the project mapping instead of setting one.
        #[arg(long)]
        clear_project: bool,
        /// New display label; empty re-derives it from the locator.
        #[arg(long)]
        label: Option<String>,
    },

    /// List registered external sources.
    Sources,

    /// Discover a CalDAV account's calendar/task collections from a
    /// server, account, principal, or home URL (#150), so collections
    /// can be registered without hand-constructing URLs. Read-only:
    /// registers nothing — each found URL becomes a source through
    /// `source-add --kind caldav`.
    SourceDiscover {
        /// Server, account, principal, or home URL to discover from.
        endpoint: String,
        /// Lend a registered source's stored credential to the
        /// discovery (source id from `sources`); without one the
        /// endpoint's userinfo and TIMETRACK_CALDAV_PASSWORD apply.
        #[arg(long)]
        source: Option<String>,
        /// Print a JSON array for scripts instead of the human list.
        #[arg(long)]
        json: bool,
    },

    /// Fetch one source (or all) now and store new suggestions.
    /// Fetching creates no entries — `confirm` does that.
    Refresh {
        /// Source id (from `sources`); omitted refreshes all.
        id: Option<String>,
    },

    /// List pending suggestions from external sources.
    Suggestions {
        /// Print a JSON array for scripts instead of the human list.
        #[arg(long)]
        json: bool,
    },

    /// Turn a suggestion into a real entry.
    Confirm {
        /// Suggestion id (from `suggestions`).
        id: String,
        /// Project override (id, name, or id prefix); default is the source's map.
        #[arg(short, long)]
        project: Option<String>,
    },

    /// Dismiss a suggestion; it will not be offered again.
    Dismiss {
        /// Suggestion id (from `suggestions`).
        id: String,
    },

    /// Store a source's secret in the OS credential store (#149). The
    /// secret is never an argument: it is prompted for (un-echoed) on
    /// a terminal, or read as the first stdin line when piped.
    SourceSetCredential {
        /// Source id (from `sources`).
        id: String,
        /// Login name; defaults to the `user@` in the source URL.
        #[arg(short, long)]
        username: Option<String>,
    },

    /// Forget a source's stored secret.
    SourceRemoveCredential {
        /// Source id (from `sources`).
        id: String,
    },

    /// Delete a source, its still-pending suggestions, and its
    /// stored secret (#153). Decided suggestions stay as history, and
    /// confirmed entries stay.
    SourceRemove {
        /// Source id (from `sources`).
        id: String,
    },
}

/// The client is driven by `async_io` — this workspace has no tokio
/// anywhere (see the workspace Cargo.toml).
fn main() -> Result<()> {
    async_io::block_on(run())
}

async fn run() -> Result<()> {
    let cli = Cli::parse();

    // `wait_for_service` waits for the service to answer on its socket
    // (spawning it once if nothing does), and gives up with the shared
    // guidance -- so a missing service says what to do about it.
    let client = Client::wait_for_service(CONNECT_TIMEOUT).await?;
    dispatch(client, cli.command_or_status())
        .await
        .map_err(explain)
}

/// A failure the user can act on, in the words the other clients use.
///
/// The three clients say the same thing about a missing service, so the
/// wording comes from the protocol crate rather than from here. A refusal
/// from the service keeps its own wording.
fn explain(err: anyhow::Error) -> anyhow::Error {
    if matches!(
        err.downcast_ref::<ClientError>(),
        Some(ClientError::NoService)
    ) {
        return anyhow::anyhow!(no_service_hint());
    }
    // A refusal arrives three layers deep (`ClientError::Service` →
    // `ServiceError::Rule` → the rule), every layer displaying the same
    // wording, so the anyhow chain prints it three times (`Error` plus
    // `Caused by` 0 and 1). Flatten refusals to the single wording the
    // service sent.
    if let Some(client) = err.downcast_ref::<ClientError>() {
        return anyhow::anyhow!("{}", disp(&client.to_string()));
    }
    // A refusal can embed stored text (a duplicate project name, say), and
    // `main`'s `Error: …` line would print it raw to the terminal (#101).
    // Clean messages keep their typed error; only a message that actually
    // carries control characters is rebuilt from its sanitized wording.
    let text = err.to_string();
    let clean = disp(&text);
    if clean == text {
        err
    } else {
        anyhow::anyhow!(clean)
    }
}

/// Write one stdout line, exiting quietly when the reader is gone.
///
/// `println!` panics on a closed pipe (`timetrack list | head -n 2`
/// exits 101), so stdout output goes through here instead: a
/// `BrokenPipe` means the reader left early and is exit 0, anything
/// else is still a panic.
fn emit(line: &str) {
    if let Err(e) = emit_to(&mut std::io::stdout().lock(), line) {
        if e.kind() == std::io::ErrorKind::BrokenPipe {
            std::process::exit(0);
        }
        panic!("failed writing to stdout: {e}");
    }
}

/// The writable half of [`emit`], split out so the error mapping is
/// testable without a real pipe.
fn emit_to(out: &mut dyn std::io::Write, line: &str) -> std::io::Result<()> {
    writeln!(out, "{line}")
}

/// [`emit`] without the trailing newline, for payloads (like CSV) where
/// the bytes must reach the reader verbatim.
fn emit_raw(text: &str) {
    if let Err(e) = emit_raw_to(&mut std::io::stdout().lock(), text) {
        if e.kind() == std::io::ErrorKind::BrokenPipe {
            std::process::exit(0);
        }
        panic!("failed writing to stdout: {e}");
    }
}

/// The writable half of [`emit_raw`], split out for the same reason as
/// [`emit_to`].
fn emit_raw_to(out: &mut dyn std::io::Write, text: &str) -> std::io::Result<()> {
    write!(out, "{text}")
}

/// A display copy of stored text for the terminal (#101): control
/// characters from a legacy or hand-edited store become spaces, so a stored
/// name can neither break the columns nor drive the terminal. Every line
/// that prints stored text goes through here.
fn disp(text: &str) -> String {
    timetrack_core::sanitize_display(text)
}

/// Refuse a `shorten` that moves nothing: with neither endpoint the
/// command used to exit 0 having changed nothing. Split out so the
/// refusal is testable without a service.
fn check_shorten_args(start: &Option<String>, end: &Option<String>) -> Result<()> {
    if start.is_none() && end.is_none() {
        anyhow::bail!("shorten needs a new start (--start/-S) or end (--end/-E)");
    }
    Ok(())
}

/// Refuse a `merge` that cannot merge: the service's arity refusal names
/// a `<need two entries>` sentinel id, so check up front in the CLI's own
/// words. Split out so the refusal is testable without a service.
fn check_merge_ids(ids: &[String]) -> Result<()> {
    if ids.len() < 2 {
        anyhow::bail!("merge: need two entries at least (got {})", ids.len());
    }
    Ok(())
}

/// Refuse names the service would reject, preserving the core rule for both
/// project creation and rename before sending the mutation over D-Bus.
fn validate_project_name(name: &str) -> Result<()> {
    if name.trim().is_empty() {
        return Err(anyhow::Error::new(
            timetrack_core::RuleError::BlankProjectName,
        ));
    }
    Ok(())
}

/// What `--notify` announces after a quick add (#105): the confirmation
/// a launcher action or keyboard shortcut has no stdout for. The project
/// is named because the default (first active project) is exactly what a
/// user firing a blind shortcut cannot otherwise see was chosen.
fn quick_add_notification(project_name: &str, duration_ms: i64, id: &str) -> (String, String) {
    (
        format!(
            "Added {} to {}",
            timetrack_core::format_compact(duration_ms),
            project_name
        ),
        format!("Undo with: timetrack undo {id}"),
    )
}

/// The quick-add bucket for `minutes`, in milliseconds.
///
/// Checked (#98): the multiplication used to run before the bucket
/// whitelist, so a huge typed value overflowed — a panic in debug, a wrap
/// that could land on a real bucket in release. Split out so the refusal is
/// testable without a service.
fn quick_add_ms(minutes: i64) -> Option<i64> {
    minutes
        .checked_mul(60_000)
        .filter(|ms| timetrack_core::QUICK_ADD_MS.contains(ms))
}

/// Whether a locator carries `user@` in its authority (#149): the
/// same rule the service's `split_userinfo` applies, minus the
/// password refusal (a registered locator cannot hold one). Only the
/// CLI's early "--username needed" check; the service re-resolves
/// authoritatively, including percent-decoding.
fn locator_has_userinfo(locator: &str) -> bool {
    let Some((_, rest)) = locator.split_once("://") else {
        return false;
    };
    let Some(at) = rest.rfind('@') else {
        return false;
    };
    if rest.find('/').is_some_and(|slash| slash < at) {
        return false;
    }
    !rest[..at].contains(':')
}

/// Read a source credential's secret (#149): an un-echoed prompt on a
/// terminal, the first stdin line when piped (`printf %s "$PASS" |
/// timetrack source-set-credential …`). Never an argument, so the
/// secret never appears in argv, shell history, or process listings.
fn read_credential_secret() -> Result<String> {
    if std::io::stdin().is_terminal() {
        rpassword::prompt_password("password: ")
            .map_err(|e| anyhow::anyhow!("could not read the password: {e}"))
            .and_then(|secret| {
                if secret.is_empty() {
                    Err(anyhow::anyhow!("the credential is empty"))
                } else {
                    Ok(secret)
                }
            })
    } else {
        read_piped_secret(&mut std::io::stdin().lock())
    }
}

/// The piped half of [`read_credential_secret`], split out so the
/// line handling is testable without a terminal: the first line,
/// newlines trimmed, nothing else touched (a password's own spaces
/// are significant).
fn read_piped_secret(reader: &mut dyn std::io::BufRead) -> Result<String> {
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| anyhow::anyhow!("could not read the secret from stdin: {e}"))?;
    let secret = line.trim_end_matches(['\r', '\n']).to_string();
    if secret.is_empty() {
        return Err(anyhow::anyhow!(
            "no secret on stdin; pipe one or run on a terminal for the prompt"
        ));
    }
    Ok(secret)
}

async fn dispatch(client: Client, command: Command) -> Result<()> {
    match command {
        Command::Add {
            project,
            description,
            start,
            end,
        } => {
            let snap = client.snapshot().await?;
            let project = pick_project(&snap, project.as_deref())?;
            let tz = snapshot_tz(&snap)?;
            let start_ms = parse_when(&start, "start", &tz)?;
            let end_ms = parse_when(&end, "end", &tz)?;
            let e = client.add(&project, &description, start_ms, end_ms).await?;
            emit(&format!(
                "added {} ({})",
                timetrack_core::format_duration(e.duration_ms()),
                e.id
            ));
        }

        Command::Duration {
            project,
            description,
            for_,
        } => {
            let snap = client.snapshot().await?;
            let project = pick_project(&snap, project.as_deref())?;
            let ms = parse_duration(&for_)?;
            let e = client.add_duration(&project, &description, ms).await?;
            emit(&format!(
                "added {} ending now ({})",
                timetrack_core::format_duration(e.duration_ms()),
                e.id
            ));
        }

        Command::Past {
            project,
            description,
            for_,
            ended,
        } => {
            let snap = client.snapshot().await?;
            let project = pick_project(&snap, project.as_deref())?;
            let ms = parse_duration(&for_)?;
            let tz = snapshot_tz(&snap)?;
            let end_ms = parse_when(&ended, "ended", &tz)?;
            let e = client
                .add_duration_ending(&project, &description, ms, end_ms)
                .await?;
            emit(&format!(
                "added {} ending {}ms before now ({})",
                timetrack_core::format_duration(e.duration_ms()),
                // Display only; saturating so an extreme typed end cannot
                // panic the confirmation line (#98).
                now_ms().saturating_sub(end_ms),
                e.id
            ));
        }

        Command::Quick {
            project,
            minutes,
            notify,
        } => {
            let snap = client.snapshot().await?;
            let project = pick_project(&snap, project.as_deref())?;
            let ms = quick_add_ms(minutes).ok_or_else(|| {
                anyhow::anyhow!(
                    "quick add takes one of {:?} minutes",
                    timetrack_core::QUICK_ADD_MS
                        .iter()
                        .map(|m| m / 60_000)
                        .collect::<Vec<_>>()
                )
            })?;
            let e = client.quick_add(&project, ms).await?;
            emit(&format!(
                "added {} ({}) -- undo with: timetrack undo {}",
                timetrack_core::format_duration(e.duration_ms()),
                e.id,
                e.id
            ));
            if notify {
                // Best effort (#105): the time is recorded either way, so
                // a missing notification daemon is a note on stderr, not
                // a failed quick-add.
                let project_name = snap
                    .projects
                    .iter()
                    .find(|p| p.id == project)
                    .map(|p| disp(&p.name))
                    .unwrap_or_else(|| project.clone());
                let (summary, body) = quick_add_notification(&project_name, e.duration_ms(), &e.id);
                if let Err(err) = timetrack_proto::desktop_notify(&summary, &body).await {
                    eprintln!("could not show a notification: {err}");
                }
            }
        }

        Command::Shorten { id, start, end } => {
            // Unspecified endpoints are left alone, so `shorten e2 -E 30m`
            // moves only the end and the entry is kept.
            check_shorten_args(&start, &end)?;
            let snap = client.snapshot().await?;
            let existing = find_entry(&snap, &id)?;
            let tz = snapshot_tz(&snap)?;
            let start_ms = match &start {
                Some(s) => parse_when(s, "start", &tz)?,
                None => existing.started_at,
            };
            let end_ms = match &end {
                Some(s) => parse_when(s, "end", &tz)?,
                None => existing.ended_at,
            };
            let e = client.set_times(&existing.id, start_ms, end_ms).await?;
            emit(&format!(
                "shortened to {} (the entry is still there: {})",
                timetrack_core::format_duration(e.duration_ms()),
                e.id
            ));
        }

        Command::Undo { id } => {
            let snap = client.snapshot().await?;
            let existing = find_entry(&snap, &id)?;
            client.undo_quick_add(&existing.id).await?;
            emit(&format!("undid quick add: {}", existing.id));
        }

        Command::Delete { id } => {
            let snap = client.snapshot().await?;
            let existing = find_entry(&snap, &id)?;
            client.delete_entry(&existing.id).await?;
            emit(&format!("deleted: {}", existing.id));
        }

        Command::SetText { id, text } => {
            let snap = client.snapshot().await?;
            let existing = find_entry(&snap, &id)?;
            let e = client.set_text(&existing.id, &text).await?;
            emit(&format!(
                "set text on {} to \"{}\"",
                e.id,
                disp(&e.description)
            ));
        }

        Command::SetProject { id, project } => {
            let snap = client.snapshot().await?;
            let existing = find_entry(&snap, &id)?;
            let target = find_project(&snap, &project)?;
            let e = client.set_project(&existing.id, &target.id).await?;
            emit(&format!(
                "moved {} to {} ({})",
                e.id,
                disp(&target.name),
                target.id
            ));
        }

        Command::Split { id, at } => {
            let snap = client.snapshot().await?;
            let existing = find_entry(&snap, &id)?;
            let tz = snapshot_tz(&snap)?;
            let at_ms = parse_when(&at, "at", &tz)?;
            if at_ms <= existing.started_at || at_ms >= existing.ended_at {
                anyhow::bail!("split point is outside entry '{id}'");
            }
            let (a, b) = client.split(&existing.id, at_ms).await?;
            emit(&format!(
                "split into ({}) {} and ({}) {}",
                a.id,
                timetrack_core::format_duration(a.duration_ms()),
                b.id,
                timetrack_core::format_duration(b.duration_ms()),
            ));
        }

        Command::Merge { ids, yes } => {
            check_merge_ids(&ids)?;
            let snap = client.snapshot().await?;
            let mut found = Vec::with_capacity(ids.len());
            for id in &ids {
                found.push(find_entry(&snap, id)?);
            }
            // Preview from the snapshot so the confirmation states how the
            // total changes; the service re-validates on execute.
            let intervals: Vec<(i64, i64)> =
                found.iter().map(|e| (e.started_at, e.ended_at)).collect();
            let delta =
                timetrack_core::format_merge_delta(timetrack_core::merge_shrink_ms(&intervals));
            let project = snap
                .projects
                .iter()
                .find(|p| p.id == found[0].project_id)
                .map(|p| disp(&p.name))
                .unwrap_or_else(|| found[0].project_id.clone());
            if !yes {
                if std::io::stdin().is_terminal() {
                    eprint!(
                        "Merge {} entries on {project}? {delta}. Confirm? [y/N] ",
                        ids.len()
                    );
                    use std::io::BufRead;
                    let stdin = std::io::stdin();
                    let mut line = String::new();
                    stdin.lock().read_line(&mut line)?;
                    if !confirmed(&line) {
                        emit("merge cancelled");
                        return Ok(());
                    }
                } else if !read_piped_confirmation(&mut std::io::stdin().lock())? {
                    anyhow::bail!("merge needs confirmation ({delta}); re-run with --yes");
                }
            }
            let resolved: Vec<String> = found.iter().map(|e| e.id.clone()).collect();
            let merged = client.merge(&resolved).await?;
            emit(&format!(
                "merged {} entries into ({}) {} ({delta})",
                ids.len(),
                merged.id,
                timetrack_core::format_duration(merged.duration_ms()),
            ));
        }

        Command::Project { name } => {
            validate_project_name(&name)?;
            let p = client.add_project(&name).await?;
            emit(&format!("created project {} ({})", disp(&p.name), p.id));
        }

        Command::Rename { id, name } => {
            // At the D-Bus level an empty name means "keep", so passing blank
            // through would silently succeed as a no-op.
            validate_project_name(&name)?;
            let snap = client.snapshot().await?;
            let existing = find_project(&snap, &id)?;
            let old = existing.name.clone();
            let p = client.update_project(&existing.id, &name, -1).await?;
            emit(&format!(
                "renamed {} to {} ({})",
                disp(&old),
                disp(&p.name),
                p.id
            ));
        }

        Command::Recolour { id, colour } => {
            let snap = client.snapshot().await?;
            let existing = find_project(&snap, &id)?;
            let value = parse_colour(&colour)?;
            let p = client
                .update_project(&existing.id, "", value as i64)
                .await?;
            emit(&format!(
                "recoloured {} ({}) to #{value:06x}",
                disp(&p.name),
                p.id
            ));
        }

        Command::Archive { id } => {
            let snap = client.snapshot().await?;
            let existing = find_project(&snap, &id)?;
            let p = client.set_archived(&existing.id, true).await?;
            emit(&format!(
                "archived {} ({}) -- its entries stay in historical totals",
                disp(&p.name),
                p.id
            ));
        }

        Command::Unarchive { id } => {
            let snap = client.snapshot().await?;
            let existing = find_project(&snap, &id)?;
            let p = client.set_archived(&existing.id, false).await?;
            emit(&format!("unarchived {} ({})", disp(&p.name), p.id));
        }

        Command::DeleteProject { id } => {
            // Resolved by id, by name, or by id prefix, like rename and
            // recolour, so `timetrack delete-project Work` works as well
            // as the id.
            let snap = client.snapshot().await?;
            let existing = find_project(&snap, &id)?;
            // The service owns the rules: a project that still has entries
            // cannot be deleted, and neither can the last project. Its
            // refusals reach the CLI in the core's own words.
            client.delete_project(&existing.id).await?;
            emit(&format!(
                "deleted project {} ({})",
                disp(&existing.name),
                existing.id
            ));
        }

        Command::Snapshot => {
            let snap = client.snapshot().await?;
            emit(&serde_json::to_string(&snap).expect("snapshots are always serializable"));
        }

        Command::Status { json } => {
            let snap = client.snapshot().await?;
            if json {
                emit(&status_json(&snap));
            } else {
                print_status(&snap);
            }
        }

        Command::List {
            limit,
            json,
            search,
        } => {
            let snap = client.snapshot().await?;
            if json {
                emit(&list_json(&snap, limit, search.as_deref()));
            } else {
                print_list(&snap, limit, search.as_deref());
            }
        }

        Command::Import { file, dry_run } => {
            let text = std::fs::read_to_string(&file)
                .map_err(|e| anyhow::anyhow!("could not read {}: {e}", file.display()))?;
            let snap = client.snapshot().await?;
            let tz = snapshot_tz(&snap)?;
            let rows = timetrack_core::parse_export_csv(&text, &tz).map_err(anyhow::Error::new)?;
            let plan = plan_import(&snap, &rows);
            let mut notes_dropped = 0usize;
            if dry_run {
                emit(&format!(
                    "would import {} entries ({} skipped as already present, {} new project(s))",
                    plan.to_add.len(),
                    plan.skipped,
                    plan.new_projects.len()
                ));
            } else {
                // Project name (lowercased) -> id, seeded from the
                // snapshot and extended as missing projects are created.
                let mut by_name: std::collections::HashMap<String, String> = snap
                    .projects
                    .iter()
                    .map(|p| (p.name.to_lowercase(), p.id.clone()))
                    .collect();
                for name in &plan.new_projects {
                    let created = client.add_project(name).await?;
                    by_name.insert(name.to_lowercase(), created.id);
                }
                for (project, row) in &plan.to_add {
                    let project_id = &by_name[&project.to_lowercase()];
                    let created = client
                        .add(project_id, &row.description, row.started_at, row.ended_at)
                        .await?;
                    if let Some(note) = &row.note
                        && client.set_note(&created.id, Some(note)).await.is_err()
                    {
                        notes_dropped += 1;
                    }
                }
                emit(&format!(
                    "imported {} entries ({} skipped as already present, {} project(s) created)",
                    plan.to_add.len(),
                    plan.skipped,
                    plan.new_projects.len()
                ));
            }
            if notes_dropped > 0 {
                emit(
                    "note: {notes_dropped} note(s) could not be restored (notes need a socket-speaking service)",
                );
            }
        }

        Command::SourceAdd {
            locator,
            kind,
            project,
            label,
        } => {
            let inferred = if locator.starts_with("http://") || locator.starts_with("https://") {
                "ics_url"
            } else {
                "ics_file"
            };
            let kind = kind.as_deref().unwrap_or(inferred);
            let project_id = match &project {
                Some(wanted) => {
                    let snap = client.snapshot().await?;
                    Some(find_project(&snap, wanted)?.id.clone())
                }
                None => None,
            };
            let src = client
                .add_source(kind, &locator, project_id.as_deref(), &label)
                .await?;
            emit(&format!(
                "added source {} ({}) {}",
                disp(&src.label),
                src.kind,
                src.id
            ));
        }

        Command::SourceUpdate {
            id,
            kind,
            locator,
            project,
            clear_project,
            label,
        } => {
            // Full-replace on the wire: read the current state, apply
            // the flags, send it all back. Omitted flags keep their
            // values, so a label-only edit is one flag.
            let sources = client.sources().await?;
            let current = sources.iter().find(|s| s.id == id).ok_or_else(|| {
                anyhow::anyhow!("no external source with id '{id}' (see: timetrack sources)")
            })?;
            let project_id = if clear_project {
                None
            } else {
                match &project {
                    Some(wanted) => {
                        let snap = client.snapshot().await?;
                        Some(find_project(&snap, wanted)?.id.clone())
                    }
                    None => current.project_id.clone(),
                }
            };
            let updated = client
                .update_source(
                    &id,
                    kind.as_deref().unwrap_or(&current.kind),
                    locator.as_deref().unwrap_or(&current.locator),
                    project_id.as_deref(),
                    label.as_deref().unwrap_or(&current.label),
                )
                .await?;
            emit(&format!(
                "updated source {} ({}) {}",
                disp(&updated.label),
                updated.kind,
                updated.id
            ));
        }

        Command::Sources => {
            let sources = client.sources().await?;
            if sources.is_empty() {
                emit("no external sources yet (add one with: timetrack source-add <file.ics>)");
            }
            for s in sources {
                // The credential state, never the secret: who the stored
                // login authenticates as, if one is stored.
                let credential = match (s.has_credential, &s.username) {
                    (true, Some(user)) => format!("  [credential: {}]", disp(user)),
                    (true, None) => "  [credential]".to_string(),
                    (false, _) => String::new(),
                };
                emit(&format!(
                    "{}  {:<8}  {}  {}{}",
                    s.id,
                    s.kind,
                    disp(&s.label),
                    disp(&s.locator),
                    credential
                ));
            }
        }

        Command::SourceDiscover {
            endpoint,
            source,
            json,
        } => {
            let found = client.discover_caldav(&endpoint, source.as_deref()).await?;
            if json {
                emit(
                    &serde_json::to_string(&found)
                        .expect("collection views are always serializable"),
                );
            } else if found.is_empty() {
                emit(&format!("no calendar collections found at {endpoint}"));
            } else {
                for c in &found {
                    emit(&format!(
                        "{}  [{}]  {}",
                        disp(&c.display_name),
                        c.components.join(", "),
                        disp(&c.url)
                    ));
                }
                emit("register one with: timetrack source-add --kind caldav <url>");
            }
        }

        Command::Refresh { id } => {
            let tally = client.refresh_source(id.as_deref()).await?;
            // CalDAV refreshes (#151) also update and withdraw; the
            // suffix stays silent at zero so `.ics` output is unchanged.
            let mut extra = String::new();
            if tally.updated > 0 {
                extra.push_str(&format!(", {} updated", tally.updated));
            }
            if tally.withdrawn > 0 {
                extra.push_str(&format!(", {} withdrawn", tally.withdrawn));
            }
            emit(&format!(
                "fetched {} item(s): {} new suggestion(s), {} skipped{}",
                tally.fetched, tally.inserted, tally.skipped, extra
            ));
        }

        Command::Suggestions { json } => {
            let suggestions = client.suggestions().await?;
            if json {
                emit(
                    &serde_json::to_string(&suggestions)
                        .expect("suggestion views are always serializable"),
                );
            } else if suggestions.is_empty() {
                emit("no pending suggestions (try: timetrack refresh)");
            } else {
                let snap = client.snapshot().await?;
                let tz = snapshot_tz(&snap)?;
                for s in suggestions {
                    emit(&format!(
                        "{}  {}  {:>10}  {}",
                        s.id,
                        timetrack_core::format_local_iso8601(s.started_at, &tz),
                        timetrack_core::format_duration(s.ended_at - s.started_at),
                        disp(&s.title)
                    ));
                }
            }
        }

        Command::Confirm { id, project } => {
            let project_id = match &project {
                Some(wanted) => {
                    let snap = client.snapshot().await?;
                    Some(find_project(&snap, wanted)?.id.clone())
                }
                None => None,
            };
            let e = client
                .confirm_suggestion(&id, project_id.as_deref())
                .await?;
            emit(&format!(
                "confirmed {} ({})",
                timetrack_core::format_duration(e.duration_ms()),
                e.id
            ));
        }

        Command::Dismiss { id } => {
            client.dismiss_suggestion(&id).await?;
            emit(&format!("dismissed {id}"));
        }

        Command::SourceSetCredential { id, username } => {
            let sources = client.sources().await?;
            let src = sources.iter().find(|s| s.id == id).ok_or_else(|| {
                anyhow::anyhow!("no external source with id '{id}' (see: timetrack sources)")
            })?;
            let username = username.unwrap_or_default();
            // Fail before prompting: no point reading a secret the
            // service would refuse for want of a username. The service
            // re-resolves authoritatively (including percent-decoding).
            if username.trim().is_empty() && !locator_has_userinfo(&src.locator) {
                anyhow::bail!("source '{id}' names no user; pass --username <name>");
            }
            let secret = read_credential_secret()?;
            let updated = client
                .set_source_credential(&id, &username, &secret)
                .await?;
            emit(&format!(
                "stored a credential for {} on source {} ({})",
                disp(updated.username.as_deref().unwrap_or_default()),
                disp(&updated.label),
                updated.id
            ));
        }

        Command::SourceRemoveCredential { id } => {
            let updated = client.remove_source_credential(&id).await?;
            emit(&format!(
                "removed the credential from source {} ({})",
                disp(&updated.label),
                updated.id
            ));
        }

        Command::SourceRemove { id } => {
            let sources = client.sources().await?;
            let label = sources
                .iter()
                .find(|s| s.id == id)
                .map(|s| disp(&s.label))
                .unwrap_or_else(|| id.clone());
            client.remove_source(&id).await?;
            emit(&format!("removed source {label} ({id})"));
        }

        Command::Export {
            scope,
            at,
            from,
            to,
            out,
        } if from.is_some() || to.is_some() => {
            // A custom range (#133) is its own addressing scheme, like
            // the anchor was (ADR 0001): both days or neither, and never
            // combined with a week/month scope that it would contradict.
            let (Some(from_text), Some(to_text)) = (&from, &to) else {
                return Err(anyhow::anyhow!("a custom range needs both --from and --to"));
            };
            if scope != "all" || at.is_some() {
                return Err(anyhow::anyhow!(
                    "--from/--to cannot combine with --scope or --at"
                ));
            }
            let snap = client.snapshot().await?;
            let tz = snapshot_tz(&snap)?;
            let from_ms = parse_export_anchor(from_text, &tz)?;
            let to_ms = parse_export_anchor(to_text, &tz)?;
            let csv = client.export_csv_range(from_ms, to_ms).await?;
            match out {
                Some(path) => {
                    std::fs::write(&path, &csv)?;
                    let count = csv.lines().count().saturating_sub(1);
                    let basename = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string());
                    emit(&format!(
                        "exported {count} entries ({}..{}) to {basename}",
                        timetrack_core::anchor_day_string(from_ms, &tz),
                        timetrack_core::anchor_day_string(to_ms, &tz)
                    ));
                }
                None => emit_raw(&csv),
            }
        }

        Command::Export { scope, at, out, .. } => {
            // Scope first, so a typo is refused before touching the bus.
            let parsed = parse_export_scope(&scope)?;
            // The file status needs the period id, which needs the snapshot
            // zone plus the anchor the CSV was fetched with. All time needs
            // no zone (its anchor is ignored); chosen scopes already fetch
            // the snapshot for the anchor; current scopes fetch it here only
            // when writing to a file.
            let (csv, period_id) = if parsed == timetrack_core::ExportScope::All {
                // All time ignores the anchor entirely: no parse, no warning,
                // so a stale `--at` never narrows a full export and old
                // services keep serving it over the old call.
                let csv = client.export_csv(&scope).await?;
                (csv, parsed.period_id(0, &timetrack_core::Tz::utc()))
            } else if let Some(at_text) = at {
                // Chosen week/month: parse the day to an instant in the
                // snapshot zone, then use the anchored call. Against an old
                // service this answers unknown-method/interface, which the
                // proto sorts as a version refusal -- a version message,
                // never a wrong file (reuses the ticket-23 wording as-is).
                let snap = client.snapshot().await?;
                let tz = snapshot_tz(&snap)?;
                let anchor_ms = parse_export_anchor(&at_text, &tz)?;
                let csv = client.export_csv_at(&scope, anchor_ms).await?;
                (csv, parsed.period_id(anchor_ms, &tz))
            } else {
                // Current week/month: the old call, anchored at now by the
                // service, so old clients and old services keep working.
                let csv = client.export_csv(&scope).await?;
                let period_id = match &out {
                    Some(_) => {
                        let snap = client.snapshot().await?;
                        parsed.period_id(now_ms(), &snapshot_tz(&snap)?)
                    }
                    None => String::new(),
                };
                (csv, period_id)
            };
            match out {
                Some(path) => {
                    std::fs::write(&path, &csv)?;
                    emit(&export_file_status(&csv, parsed, &period_id, &path));
                }
                None => emit_raw(&csv),
            }
        }
    }
    Ok(())
}

fn print_status(s: &Snapshot) {
    emit(&format!(
        "this week:  {}  ({} entries)",
        timetrack_core::format_duration(s.week.total_ms),
        s.week.entry_count
    ));
    emit(&format!(
        "this month: {}",
        timetrack_core::format_duration(s.month.total_ms)
    ));
    emit(&format!(
        "all time:   {}",
        timetrack_core::format_duration(s.total_ms)
    ));
    if !s.over_24h_days.is_empty() {
        // Legal, but worth surfacing: it usually means a mistyped interval
        // rather than a genuinely enormous day (REQUIREMENTS §4).
        emit(&format!(
            "warning:    {} day(s) total more than 24h",
            s.over_24h_days.len()
        ));
    }
}

/// The entries a `--search` keeps (#131): description, project name, or
/// note containing the needle, case-insensitively. Client-side over the
/// snapshot, so totals and exports are never narrowed — only this list.
fn filtered_entries<'a>(
    s: &'a Snapshot,
    search: Option<&str>,
) -> Vec<&'a timetrack_proto::EntryView> {
    let Some(needle) = search.map(|q| q.to_lowercase()) else {
        return s.entries.iter().collect();
    };
    let project_name = |id: &str| {
        s.projects
            .iter()
            .find(|p| p.id == id)
            .map_or("", |p| p.name.as_str())
    };
    s.entries
        .iter()
        .filter(|e| {
            e.description.to_lowercase().contains(&needle)
                || project_name(&e.project_id).to_lowercase().contains(&needle)
                || e.note
                    .as_deref()
                    .is_some_and(|n| n.to_lowercase().contains(&needle))
        })
        .collect()
}

fn print_list(s: &Snapshot, limit: usize, search: Option<&str>) {
    let entries = filtered_entries(s, search);
    if entries.is_empty() {
        emit(if search.is_some() {
            "no entries match"
        } else {
            "no entries yet"
        });
        return;
    }
    for e in entries.into_iter().take(limit) {
        let marker = if e.source == timetrack_proto::EntrySource::QuickAdd {
            "+"
        } else {
            " "
        };
        emit(&format!(
            "{marker} {:>10}  {:<6}  {}",
            timetrack_core::format_duration(e.duration_ms()),
            e.id,
            disp(e.label())
        ));
    }
}

/// The `status --json` document (§18): the same totals `print_status`
/// shows, as one stable object a script can read -- week first, like the
/// human output and the GUI hero.
fn status_json(s: &Snapshot) -> String {
    serde_json::json!({
        "week": s.week,
        "month": s.month,
        "total_ms": s.total_ms,
        "over_24h_days": s.over_24h_days,
    })
    .to_string()
}

/// The `list --json` document (§18): at most `limit` entries, most
/// recent first, in the wire's own field spellings.
fn list_json(s: &Snapshot, limit: usize, search: Option<&str>) -> String {
    let entries: Vec<&timetrack_proto::EntryView> = filtered_entries(s, search)
        .into_iter()
        .take(limit)
        .collect();
    serde_json::to_string(&entries).expect("entry views are always serializable")
}

/// What an import would do (#132), decided purely over a snapshot so it
/// is testable without a service: which projects must be created
/// (matched by name, case-insensitively), which rows are new, and how
/// many were skipped as already present — where "present" is same
/// project, same interval, same description, and a row repeated inside
/// the file counts once.
struct ImportPlan {
    new_projects: Vec<String>,
    to_add: Vec<(String, timetrack_core::ImportedEntry)>,
    skipped: usize,
}

fn plan_import(snap: &Snapshot, rows: &[timetrack_core::ImportedEntry]) -> ImportPlan {
    use std::collections::HashSet;
    let name_of = |id: &str| -> String {
        snap.projects
            .iter()
            .find(|p| p.id == id)
            .map_or_else(|| id.to_lowercase(), |p| p.name.to_lowercase())
    };
    // The export's precision is whole seconds, so dedupe at whole
    // seconds on both sides: an entry re-imported from its own export
    // must be recognised even though its stored milliseconds were
    // never in the file.
    fn whole_seconds(ms: i64) -> i64 {
        ms.div_euclid(1_000) * 1_000
    }
    let mut present: HashSet<(String, i64, i64, String)> = snap
        .entries
        .iter()
        .map(|e| {
            (
                name_of(&e.project_id),
                whole_seconds(e.started_at),
                whole_seconds(e.ended_at),
                e.description.clone(),
            )
        })
        .collect();
    let known: HashSet<String> = snap
        .projects
        .iter()
        .map(|p| p.name.to_lowercase())
        .collect();
    let mut new_projects: Vec<String> = Vec::new();
    let mut to_add = Vec::new();
    let mut skipped = 0;
    for row in rows {
        let key = (
            row.project.to_lowercase(),
            whole_seconds(row.started_at),
            whole_seconds(row.ended_at),
            row.description.clone(),
        );
        if !present.insert(key) {
            skipped += 1;
            continue;
        }
        if !known.contains(&row.project.to_lowercase())
            && !new_projects
                .iter()
                .any(|n: &String| n.to_lowercase() == row.project.to_lowercase())
        {
            new_projects.push(row.project.clone());
        }
        to_add.push((row.project.clone(), row.clone()));
    }
    ImportPlan {
        new_projects,
        to_add,
        skipped,
    }
}

/// Resolve an entry argument to a snapshot entry.
///
/// An exact id wins, then a unique id prefix (ADR 0018); anything else
/// refuses in the service's own words, so scripts match one string
/// either side. The caller sends the resolved id onward.
fn find_entry<'a>(snap: &'a Snapshot, wanted: &str) -> Result<&'a EntryView> {
    match timetrack_core::resolve_id_prefix(wanted, snap.entries.iter().map(|e| e.id.as_str())) {
        timetrack_core::IdResolution::Resolved(id) => Ok(snap
            .entries
            .iter()
            .find(|e| e.id == id)
            .expect("resolved above")),
        timetrack_core::IdResolution::Unknown => Err(anyhow::Error::new(
            timetrack_core::RuleError::UnknownEntry(wanted.to_string()),
        )),
        timetrack_core::IdResolution::Ambiguous => Err(anyhow::Error::new(
            timetrack_core::RuleError::AmbiguousEntry(wanted.to_string()),
        )),
    }
}

/// Resolve a project argument to a snapshot entry, for rename/recolour.
///
/// Unlike `pick_project` this matches archived projects too: renaming one
/// is harmless, and refusing would strand the numbered placeholders this
/// replaced. Matches the id first, then the name case-insensitively,
/// then a unique id prefix (ADR 0018). The caller sends the resolved id
/// onward.
fn find_project<'a>(snap: &'a Snapshot, wanted: &str) -> Result<&'a ProjectView> {
    if let Some(found) = snap.projects.iter().find(|p| p.id == wanted).or_else(|| {
        snap.projects
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(wanted))
    }) {
        return Ok(found);
    }
    match timetrack_core::resolve_id_prefix(wanted, snap.projects.iter().map(|p| p.id.as_str())) {
        // Exact was checked above, so a resolution here is a unique prefix.
        timetrack_core::IdResolution::Resolved(id) => Ok(snap
            .projects
            .iter()
            .find(|p| p.id == id)
            .expect("resolved above")),
        timetrack_core::IdResolution::Unknown => Err(anyhow::Error::new(
            timetrack_core::RuleError::UnknownProject(wanted.to_string()),
        )),
        timetrack_core::IdResolution::Ambiguous => Err(anyhow::Error::new(
            timetrack_core::RuleError::AmbiguousProject(wanted.to_string()),
        )),
    }
}

/// Parse an `RRGGBB` colour like `2ea043` (leading `#` allowed).
///
/// The spelling lives in the core, shared with the GUI's recolour prompt
/// (#104); this wrapper only re-types the error for anyhow. The wording is
/// unchanged from when the parse lived here.
fn parse_colour(text: &str) -> Result<u32> {
    timetrack_core::parse_colour(text).map_err(anyhow::Error::new)
}

/// Resolve a project argument to an id, defaulting to the first active one.
///
/// Matches the id, then (case-insensitively) the name, then a unique id
/// prefix among active projects (ADR 0018): `timetrack quick -p Work`
/// works the same as `-p` with the id, since ids are what every other
/// command prints back. Only active projects are candidates throughout.
fn pick_project(snap: &Snapshot, requested: Option<&str>) -> Result<String> {
    let active: Vec<&ProjectView> = snap.projects.iter().filter(|p| !p.archived).collect();
    match requested {
        Some(want) => {
            if let Some(found) = active
                .iter()
                .find(|p| p.id == want)
                .or_else(|| active.iter().find(|p| p.name.eq_ignore_ascii_case(want)))
            {
                return Ok(found.id.clone());
            }
            match timetrack_core::resolve_id_prefix(want, active.iter().map(|p| p.id.as_str())) {
                // Exact was checked above, so this is a unique prefix.
                timetrack_core::IdResolution::Resolved(id) => Ok(id.to_string()),
                timetrack_core::IdResolution::Ambiguous => Err(anyhow::Error::new(
                    timetrack_core::RuleError::AmbiguousProject(want.to_string()),
                )),
                timetrack_core::IdResolution::Unknown => {
                    let names: Vec<&str> = active.iter().map(|p| p.name.as_str()).collect();
                    Err(anyhow::anyhow!(
                        "no active project named '{want}'. Active: {names:?}"
                    ))
                }
            }
        }
        None => active.first().map(|p| p.id.clone()).ok_or_else(|| {
            anyhow::anyhow!("no projects yet. Create one with:  timetrack project \"Work\"")
        }),
    }
}

/// The current time, in milliseconds since the epoch.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Parse a moment given on the command line, in milliseconds since the epoch.
///
/// Two forms, because they cover the two things a person actually means:
///
/// - `-90` -- minutes relative to now, so `-90` is 90 minutes ago
/// - `09:30` -- today at that wall-clock time, in local time
///
/// `tz` is the service's zone from the snapshot: `HH:MM` has to anchor on
/// local midnight, not UTC midnight, or entries land hours out for anyone
/// east or west of Greenwich. The zone resolves per instant, so days either
/// side of a DST transition still anchor correctly.
///
/// A leading `-` is deliberately allowed through clap (`allow_hyphen_values`)
/// because `-90` is a perfectly ordinary thing to type for "90 minutes ago".
///
/// The spellings live in `timetrack_core::parse_moment` so the CLI and the
/// GUI cannot disagree; this is a thin wrapper that supplies the clock.
fn parse_when(text: &str, what: &str, tz: &timetrack_core::Tz) -> Result<i64> {
    timetrack_core::parse_moment(text, what, now_ms(), tz).map_err(|e| anyhow::anyhow!("{e}"))
}

/// Parse an export scope, refusing unknown spellings with the service's own
/// sentence so scripts match one string either side of the bus.
fn parse_export_scope(scope: &str) -> Result<timetrack_core::ExportScope> {
    timetrack_core::ExportScope::parse(scope)
        .ok_or_else(|| anyhow::anyhow!("unknown export scope '{scope}' (use week, month or all)"))
}

/// Parse an export anchor (`--at`) to an instant, in the snapshot zone.
///
/// The calendar-day spelling (`YYYY-MM-DD`, resolving to local midday so
/// midnight/DST edges cannot shift it) goes through the export-only
/// [`timetrack_core::parse_anchor_day`]; the existing moment spellings go
/// through [`timetrack_core::parse_moment`]. Days never reach the moment
/// parser, so `add -S` keeps refusing them while `--at` accepts both.
fn parse_export_anchor(text: &str, tz: &timetrack_core::Tz) -> Result<i64> {
    if let Ok(anchor) = timetrack_core::parse_anchor_day(text, "at", tz) {
        return Ok(anchor);
    }
    match timetrack_core::parse_moment(text, "at", now_ms(), tz) {
        Ok(at) => Ok(at),
        Err(moment_err) => {
            // Both spellings failed: a day-shaped value must report the day
            // error (naming the value as a day), anything else the moment
            // error. Relative moments like `-90` never reach here — they
            // succeed via `parse_moment` above.
            let trimmed = text.trim();
            if trimmed.contains('-') || trimmed.contains('/') {
                timetrack_core::parse_anchor_day(text, "at", tz).map_err(|e| anyhow::anyhow!("{e}"))
            } else {
                Err(anyhow::anyhow!("{moment_err}"))
            }
        }
    }
}

/// Status line after a `--out` save: entry count, scope, period and file.
///
/// Counts the CSV rows minus the header and reuses the shared core wording,
/// so the CLI and the GUI report the same sentence for the same file.
fn export_file_status(
    csv: &str,
    scope: timetrack_core::ExportScope,
    period_id: &str,
    path: &std::path::Path,
) -> String {
    let count = csv.lines().count().saturating_sub(1);
    let basename = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    timetrack_core::format_export_status(count, scope, period_id, &basename)
}

/// The service's zone, from the snapshot's name, strictly (#102).
///
/// The CLI never reads its own `TZ`: the service is the zone authority,
/// and a snapshot without a usable name (an older service) is an error
/// the user sees, never a fixed-offset guess that could bucket a typed
/// time differently from the service's totals.
fn snapshot_tz(snap: &Snapshot) -> Result<timetrack_core::Tz> {
    timetrack_core::Tz::from_snapshot(&snap.tz).map_err(anyhow::Error::new)
}

/// Whether a typed merge confirmation counts as "yes".
///
/// Split out so the actual prompt stays a thin reader: `y`/`yes` in any
/// case confirms, everything else (including empty) cancels.
fn confirmed(line: &str) -> bool {
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// The non-terminal half of the merge confirmation: the first stdin line
/// when piped (`echo y | timetrack merge …`), so scripts can confirm
/// without `--yes`. An empty pipe (EOF) refuses like any other non-yes.
fn read_piped_confirmation(reader: &mut dyn std::io::BufRead) -> Result<bool> {
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| anyhow::anyhow!("could not read confirmation: {e}"))?;
    Ok(confirmed(&line))
}

/// Parse a duration like `90m`, `1h30m` or `45s`, in milliseconds.
///
/// Shared with the GUI via `timetrack_core::parse_duration`; see above.
fn parse_duration(text: &str) -> Result<i64> {
    timetrack_core::parse_duration(text).map_err(|e| anyhow::anyhow!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_service_is_explained_with_the_shared_hint() {
        // The guidance used to name a host `timetrack-service` binary and a
        // systemd unit -- neither exists any more: the service ships inside
        // the AppImage. The hint must name the image instead.
        let err = anyhow::Error::new(ClientError::NoService);
        let text = explain(err).to_string();
        assert_eq!(text, no_service_hint());
        assert!(
            text.contains("AppImage"),
            "the hint says how the service is started: {text}"
        );
    }

    #[test]
    fn a_service_refusal_keeps_its_own_wording() {
        let err = anyhow::Error::new(ClientError::Service(
            timetrack_proto::ServiceError::UnknownExportScope("bogus".into()),
        ));
        assert_eq!(
            explain(err).to_string(),
            "unknown export scope 'bogus' (use week, month or all)"
        );
    }

    #[test]
    fn stored_text_reaches_the_terminal_sanitized() {
        // #101: a legacy store can hold names the current rules refuse.
        // What the CLI prints — list labels via `disp`, and refusals via
        // `explain` — must carry no raw newline or escape sequence.
        assert_eq!(disp("Wo\nrk\u{1b}[31m"), "Wo rk [31m");
        let err = anyhow::Error::new(ClientError::Service(timetrack_proto::ServiceError::Rule(
            timetrack_core::RuleError::DuplicateProject("Evil\n\u{1b}[2J".into()),
        )));
        let text = explain(err).to_string();
        assert_eq!(text, "a project named 'Evil  [2J' already exists");
        assert!(!text.chars().any(|c| c.is_control()));
    }

    #[test]
    fn the_quick_add_notification_names_project_duration_and_undo() {
        // #105: a launcher action has no stdout, so the notification is
        // the only proof of what landed where.
        let (summary, body) = quick_add_notification("Work", 15 * 60_000, "e7");
        assert_eq!(summary, "Added 15m to Work");
        assert_eq!(body, "Undo with: timetrack undo e7");
    }

    #[test]
    fn a_snapshot_without_a_usable_zone_is_an_error_not_a_guess() {
        // #102: an old service sends no zone name. The CLI must refuse to
        // parse typed times against it rather than fall back to the
        // snapshot's fixed offset (or UTC) and bucket differently from
        // the service's totals.
        let mut snap = Snapshot::default();
        let err = snapshot_tz(&snap).unwrap_err().to_string();
        assert!(err.contains("did not report a timezone"), "got: {err}");
        snap.tz = "Europe/Romeo".into();
        assert!(snapshot_tz(&snap).is_err());
        snap.tz = "Europe/Rome".into();
        assert_eq!(snapshot_tz(&snap).unwrap().name(), "Europe/Rome");
    }

    #[test]
    fn durations_parse_with_units() {
        assert_eq!(parse_duration("30m").unwrap(), 1_800_000);
        assert_eq!(parse_duration("1h").unwrap(), 3_600_000);
        assert_eq!(parse_duration("45s").unwrap(), 45_000);
        assert_eq!(parse_duration("1h30m").unwrap(), 5_400_000);
    }

    #[test]
    fn durations_are_case_insensitive() {
        assert_eq!(parse_duration("30M").unwrap(), 1_800_000);
        assert_eq!(parse_duration("1H").unwrap(), 3_600_000);
    }

    #[test]
    fn a_bare_number_reads_as_minutes() {
        // `timetrack duration -f 30` almost certainly means 30 minutes.
        assert_eq!(parse_duration("30").unwrap(), 1_800_000);
    }

    #[test]
    fn zero_and_negative_durations_are_refused() {
        assert!(parse_duration("0m").is_err());
        assert!(parse_duration("0").is_err());
    }

    #[test]
    fn a_unitless_string_is_refused() {
        assert!(parse_duration("soon").is_err());
        assert!(parse_duration("").is_err());
    }

    #[test]
    fn an_unknown_unit_is_named_in_the_error() {
        let e = parse_duration("30d").unwrap_err().to_string();
        assert!(e.contains('d'), "should name the bad unit: {e}");
    }

    #[test]
    fn confirmations_accept_y_variants_and_reject_the_rest() {
        for yes in ["y", "Y", "yes", "YES", "  y  "] {
            assert!(confirmed(yes), "{yes:?} should confirm");
        }
        for no in ["", "n", "no", "yess", "1", "cancel"] {
            assert!(!confirmed(no), "{no:?} should cancel");
        }
    }

    #[test]
    fn colours_parse_as_rrggbb_hex() {
        assert_eq!(parse_colour("2ea043").unwrap(), 0x2ea043);
        assert_eq!(parse_colour("#2EA043").unwrap(), 0x2ea043);
        assert_eq!(parse_colour("  ff0000  ").unwrap(), 0xff0000);
    }

    #[test]
    fn bad_colours_name_the_problem() {
        for bad in ["", "red", "12345", "1234567", "zzzzzz", "#12 34"] {
            let e = parse_colour(bad).unwrap_err().to_string();
            assert!(e.contains("RRGGBB"), "{bad:?} should say RRGGBB: {e}");
        }
    }

    #[test]
    fn minutes_ago_parses_relative_to_now() {
        let before = now_ms();
        let t = parse_when("-90", "start", &timetrack_core::Tz::utc()).unwrap();
        let after = now_ms();
        // 90 minutes before, give or take the time the call itself took.
        assert!(
            (before - 5_400_001..=after - 5_399_999).contains(&t),
            "got {t}"
        );
    }

    #[test]
    fn minutes_from_now_parses_forward() {
        let before = now_ms();
        let t = parse_when("30", "start", &timetrack_core::Tz::utc()).unwrap();
        assert!((before + 1_800_000..=now_ms() + 1_800_000).contains(&t));
    }

    #[test]
    fn a_wall_clock_time_lands_today() {
        let t = parse_when("09:30", "start", &timetrack_core::Tz::utc()).unwrap();
        let day = 86_400_000;
        let today = now_ms().div_euclid(day) * day;
        assert!(
            t >= today && t < today + day,
            "09:30 should fall inside today"
        );
        // And specifically at 9h30m past the anchor day.
        let offset_in_day = t - today;
        assert!(
            (6 * 3_600_000..=11 * 3_600_000).contains(&offset_in_day),
            "09:30 should be mid-morning, got {}ms into the day",
            offset_in_day
        );
    }

    #[test]
    fn a_wall_clock_time_is_local_not_utc() {
        // UTC+2: anchoring on UTC midnight would put 00:30 two hours out and
        // could bucket the entry on the wrong local day.
        let tz = timetrack_core::Tz::fixed_ms(2 * 3_600_000);
        let t = parse_when("00:30", "start", &tz).unwrap();
        assert_eq!(timetrack_core::format_local_hm(t, &tz), "00:30");
        assert_eq!(tz.day_of(t), tz.day_of(now_ms()));
    }

    #[test]
    fn impossible_times_are_refused() {
        let utc = timetrack_core::Tz::utc();
        assert!(parse_when("25:00", "start", &utc).is_err());
        assert!(parse_when("09:70", "start", &utc).is_err());
        assert!(parse_when("nonsense", "start", &utc).is_err());
        assert!(parse_when("", "start", &utc).is_err());
    }

    #[test]
    fn the_error_names_the_field() {
        // "start" vs "end" matters: a user who typed the wrong one should be
        // told which argument to look at.
        assert!(
            parse_when("99:99", "end", &timetrack_core::Tz::utc())
                .unwrap_err()
                .to_string()
                .contains("end")
        );
    }

    #[test]
    fn blank_project_names_are_refused_before_the_bus() {
        // `timetrack project "  "` used to reach the service and come back
        // as a duplicate-project refusal; rename already refused locally.
        for blank in ["", "   ", "\t\n"] {
            let err = validate_project_name(blank).unwrap_err();
            assert_eq!(
                err.downcast_ref::<timetrack_core::RuleError>(),
                Some(&timetrack_core::RuleError::BlankProjectName),
                "blank project name {blank:?} should preserve the core rule error"
            );
            assert_eq!(
                err.to_string(),
                timetrack_core::RuleError::BlankProjectName.to_string()
            );
        }
        assert!(validate_project_name("Work").is_ok());
        assert!(validate_project_name("  Work  ").is_ok());
    }

    #[test]
    fn the_command_line_parses() {
        // The subcommand surface is the CLI's real API; a rename that breaks
        // someone's alias should fail here, not at runtime.
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn delete_project_takes_one_positional_project() {
        // `timetrack delete-project p2`: one positional argument, the project
        // id or its name.
        use clap::CommandFactory;
        let cli = Cli::try_parse_from(["timetrack", "delete-project", "p2"])
            .expect("delete-project args must parse");
        match cli.command {
            Some(Command::DeleteProject { id }) => assert_eq!(id, "p2"),
            other => panic!("expected delete-project, got {other:?}"),
        }
        // Spelled with a dash, like every other multi-word subcommand.
        assert!(
            Cli::command()
                .get_subcommands()
                .any(|c| c.get_name() == "delete-project"),
            "the subcommand must be called delete-project"
        );
        // And it needs a project: clap refuses the missing argument, not the
        // service.
        assert!(Cli::try_parse_from(["timetrack", "delete-project"]).is_err());
    }

    #[test]
    fn the_credential_secret_is_never_an_argument() {
        // #149: `source-set-credential` takes an id and an optional
        // (non-secret) username; the secret arrives via prompt or stdin,
        // so it can never appear in argv, shell history, or `ps`.
        use clap::CommandFactory;
        let set = Cli::command()
            .get_subcommands()
            .find(|c| c.get_name() == "source-set-credential")
            .expect("source-set-credential must exist")
            .clone();
        for arg in set.get_arguments() {
            let id = arg.get_id();
            assert!(
                id != "password" && id != "secret",
                "the secret must not be an argument: {id}"
            );
        }
        let cli = Cli::try_parse_from(["timetrack", "source-set-credential", "src-1"])
            .expect("id-only must parse");
        assert!(matches!(
            cli.command,
            Some(Command::SourceSetCredential { .. })
        ));
        assert!(Cli::try_parse_from(["timetrack", "source-remove-credential", "src-1"]).is_ok());
        assert!(Cli::try_parse_from(["timetrack", "source-remove", "src-1"]).is_ok());
    }

    #[test]
    fn locator_userinfo_detection_matches_the_service_rule() {
        assert!(locator_has_userinfo("https://alice@cal.test/u/"));
        assert!(locator_has_userinfo("http://alice@127.0.0.1:8080/cal/"));
        assert!(!locator_has_userinfo("https://cal.test/u/"));
        // An @ in the path is not userinfo.
        assert!(!locator_has_userinfo("https://cal.test/a@b.ics"));
        // A password is not a username either (the service refuses it).
        assert!(!locator_has_userinfo("https://alice:s3cr3t@cal.test/u/"));
        assert!(!locator_has_userinfo("/tmp/cal.ics"));
    }

    #[test]
    fn piped_secrets_read_one_line_verbatim() {
        use std::io::BufReader;
        let mut piped = BufReader::new("s3cr3t\nignored-rest\n".as_bytes());
        assert_eq!(read_piped_secret(&mut piped).unwrap(), "s3cr3t");
        // CRLF trims; a password's own spaces stay significant.
        let mut piped = BufReader::new("  padded  \r\n".as_bytes());
        assert_eq!(read_piped_secret(&mut piped).unwrap(), "  padded  ");
        // Empty stdin refuses instead of storing an empty secret.
        let mut piped = BufReader::new("".as_bytes());
        assert!(read_piped_secret(&mut piped).is_err());
        let mut piped = BufReader::new("\n".as_bytes());
        assert!(read_piped_secret(&mut piped).is_err());
    }

    #[test]
    fn quick_minutes_are_checked_before_the_bucket_whitelist() {
        // #98: the buckets still map, everything else refuses — including
        // values whose `minutes * 60_000` overflows i64 outright.
        assert_eq!(quick_add_ms(5), Some(300_000));
        assert_eq!(quick_add_ms(60), Some(3_600_000));
        assert_eq!(quick_add_ms(7), None);
        assert_eq!(quick_add_ms(0), None);
        assert_eq!(quick_add_ms(-5), None);
        assert_eq!(quick_add_ms(i64::MAX), None);
        assert_eq!(quick_add_ms(i64::MIN), None);
    }

    #[test]
    fn project_command_rejects_blank_names_with_the_core_rule_error() {
        let cli = Cli::try_parse_from(["timetrack", "project", " \t "])
            .expect("project args must parse so the rule can reject the name");
        let Some(Command::Project { name }) = cli.command else {
            panic!("expected project command");
        };

        let err = validate_project_name(&name).unwrap_err();
        assert_eq!(
            err.downcast_ref::<timetrack_core::RuleError>(),
            Some(&timetrack_core::RuleError::BlankProjectName)
        );
    }

    #[test]
    fn export_accepts_scope_at_and_out() {
        // `timetrack export --scope week --at <day> --out <file>`: the chosen
        // export surface (spec #19, ticket #24).
        let cli = Cli::try_parse_from([
            "timetrack",
            "export",
            "--scope",
            "week",
            "--at",
            "2026-10-12",
        ])
        .expect("export args must parse");
        match cli.command {
            Some(Command::Export { scope, at, out, .. }) => {
                assert_eq!(scope, "week");
                assert_eq!(at.as_deref(), Some("2026-10-12"));
                assert!(out.is_none());
            }
            other => panic!("expected export, got {other:?}"),
        }
        let cli = Cli::try_parse_from([
            "timetrack",
            "export",
            "--scope",
            "month",
            "--at",
            "2026-09-15",
            "--out",
            "/tmp/x.csv",
        ])
        .expect("export --out must parse");
        match cli.command {
            Some(Command::Export { scope, at, out, .. }) => {
                assert_eq!(scope, "month");
                assert_eq!(at.as_deref(), Some("2026-09-15"));
                assert_eq!(
                    out,
                    Some(std::path::PathBuf::from("/tmp/x.csv")),
                    "explicit file lands where the caller says"
                );
            }
            other => panic!("expected export, got {other:?}"),
        }
    }

    #[test]
    fn export_old_scope_spellings_keep_working() {
        // `ExportScope::parse` is the single spelling table; the CLI must not
        // narrow it (compat per ADR-0001).
        for ok in [
            "week",
            "this-week",
            "current-week",
            "month",
            "this-month",
            "current-month",
            "all",
            "all-time",
        ] {
            assert!(parse_export_scope(ok).is_ok(), "{ok:?} must keep working");
        }
    }

    #[test]
    fn export_bad_scope_is_refused_with_the_service_wording() {
        // Same sentence the service uses, so scripts match one string.
        let e = parse_export_scope("bogus").unwrap_err().to_string();
        assert!(
            e.contains("unknown export scope 'bogus'"),
            "must name the scope, got: {e}"
        );
        assert!(
            e.contains("week, month or all"),
            "must name the choices: {e}"
        );
    }

    #[test]
    fn export_anchor_day_resolves_to_local_midday() {
        // 2026-10-12 at midday UTC, the independent literal from the core
        // anchor tests (ticket #20 pins the same instant).
        let anchor = parse_export_anchor("2026-10-12", &timetrack_core::Tz::utc()).unwrap();
        assert_eq!(anchor, 1_791_806_400_000);
        assert_eq!(
            timetrack_core::format_local_hm(anchor, &timetrack_core::Tz::utc()),
            "12:00"
        );
    }

    #[test]
    fn export_bad_anchor_names_the_value() {
        for bad in ["2026-13-01", "not-a-day"] {
            let e = parse_export_anchor(bad, &timetrack_core::Tz::utc())
                .unwrap_err()
                .to_string();
            assert!(e.contains(bad), "{bad:?} must name the value: {e}");
        }
    }

    #[test]
    fn entry_moments_refuse_calendar_days() {
        // Regression: days leaked into every moment field, so `add -S
        // 2026-10-12` parsed as midday instead of refusing. Days belong to
        // `--at` only; entry moments keep main-branch behaviour.
        let utc = timetrack_core::Tz::utc();
        for what in ["start", "end", "at"] {
            let e = parse_when("2026-10-12", what, &utc)
                .unwrap_err()
                .to_string();
            assert!(e.contains(what), "must name the field: {e}");
            assert!(e.contains("2026-10-12"), "must name the value: {e}");
        }
    }

    #[test]
    fn export_anchor_accepts_days_and_moments() {
        // `--at` is the one path that accepts both: days via the anchor
        // parser, moments via the shared moment parser.
        let utc = timetrack_core::Tz::utc();
        assert_eq!(
            parse_export_anchor("2026-10-12", &utc).unwrap(),
            1_791_806_400_000
        );
        assert!(parse_export_anchor("now", &utc).is_ok());
        assert!(parse_export_anchor("-90", &utc).is_ok());
        assert!(parse_export_anchor("09:30", &utc).is_ok());
    }

    #[test]
    fn file_status_names_count_scope_period_and_name() {
        // Spec: the `--out` status names entry count, scope, period id and
        // file base name in the shared core wording — never bare
        // `exported to {path}`.
        let csv = format!(
            "{}\na,p1,x,2026-10-06T12:00:00,2026-10-06T13:00:00,01:00:00,60,manual,\nb,p1,y,2026-10-07T12:00:00,2026-10-07T13:00:00,01:00:00,60,manual,\n",
            timetrack_core::CSV_HEADER
        );
        assert_eq!(
            export_file_status(
                &csv,
                timetrack_core::ExportScope::Week,
                "2026-W41",
                std::path::Path::new("/tmp/timetrack-export-week-2026-W41.csv"),
            ),
            "exported 2 entries (week 2026-W41) to timetrack-export-week-2026-W41.csv"
        );
        assert_eq!(
            export_file_status(
                &csv,
                timetrack_core::ExportScope::All,
                "all time",
                std::path::Path::new("/tmp/timetrack-export-all.csv"),
            ),
            "exported 2 entries (all time) to timetrack-export-all.csv"
        );
        // Header alone counts zero.
        assert_eq!(
            export_file_status(
                timetrack_core::CSV_HEADER,
                timetrack_core::ExportScope::Month,
                "2026-09",
                std::path::Path::new("timetrack-export-month-2026-09.csv"),
            ),
            "exported 0 entries (month 2026-09) to timetrack-export-month-2026-09.csv"
        );
    }

    #[test]
    fn bare_timetrack_defaults_to_status_and_tui_is_gone() {
        // §18: the terminal client is one-shot only. Bare `timetrack` is
        // the between-commands glance (status), and `tui` refuses.
        let cli = Cli::try_parse_from(["timetrack"]).expect("bare timetrack parses");
        assert!(matches!(
            cli.command_or_status(),
            Command::Status { json: false }
        ));
        assert!(Cli::try_parse_from(["timetrack", "tui"]).is_err());
        use clap::CommandFactory;
        assert!(
            Cli::command()
                .get_subcommands()
                .all(|c| c.get_name() != "tui"),
            "no tui subcommand may remain"
        );
    }

    #[test]
    fn status_and_list_accept_json_flags() {
        let cli = Cli::try_parse_from(["timetrack", "status", "--json"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Status { json: true })));
        let cli = Cli::try_parse_from(["timetrack", "list", "--json", "-l", "3"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::List {
                limit: 3,
                json: true,
                search: None
            })
        ));
    }

    #[test]
    fn status_json_carries_week_month_and_all_time_totals() {
        let mut snap = Snapshot::default();
        snap.week.total_ms = 3_600_000;
        snap.week.entry_count = 2;
        snap.month.total_ms = 7_200_000;
        snap.total_ms = 10_800_000;
        let v: serde_json::Value = serde_json::from_str(&status_json(&snap)).unwrap();
        assert_eq!(v["week"]["total_ms"], 3_600_000);
        assert_eq!(v["week"]["entry_count"], 2);
        assert_eq!(v["month"]["total_ms"], 7_200_000);
        assert_eq!(v["total_ms"], 10_800_000);
    }

    #[test]
    fn search_filters_by_description_project_and_note_case_insensitively() {
        // #131: client-side over the snapshot; totals stay unfiltered
        // because only the list rendering narrows.
        let mut snap = Snapshot::default();
        snap.projects.push(timetrack_proto::ProjectView {
            id: "p1".into(),
            name: "Deep Work".into(),
            colour: None,
            archived: false,
        });
        for (id, desc, note) in [
            ("e0", "Review PR", None),
            ("e1", "groceries", Some("weekly")),
            ("e2", "misc", None),
        ] {
            snap.entries.push(timetrack_proto::EntryView {
                id: id.into(),
                project_id: "p1".into(),
                description: desc.into(),
                started_at: 0,
                ended_at: 60_000,
                source: timetrack_proto::EntrySource::Manual,
                note: note.map(str::to_string),
            });
        }
        let ids = |q: &str| -> Vec<String> {
            filtered_entries(&snap, Some(q))
                .iter()
                .map(|e| e.id.clone())
                .collect()
        };
        assert_eq!(ids("review"), ["e0"]);
        assert_eq!(ids("DEEP"), ["e0", "e1", "e2"], "project name matches");
        assert_eq!(ids("Weekly"), ["e1"], "note matches");
        assert!(ids("absent").is_empty());
        assert_eq!(filtered_entries(&snap, None).len(), 3);
        let cli = Cli::try_parse_from(["timetrack", "list", "--search", "review"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::List {
                search: Some(ref s),
                ..
            }) if s == "review"
        ));
    }

    #[test]
    fn import_plans_only_missing_entries_and_projects() {
        // #132: match projects by name, skip rows already present (same
        // project, interval and description), including repeats inside
        // the file itself.
        let mut snap = Snapshot::default();
        snap.projects.push(timetrack_proto::ProjectView {
            id: "p1".into(),
            name: "Work".into(),
            colour: None,
            archived: false,
        });
        snap.entries.push(timetrack_proto::EntryView {
            id: "e1".into(),
            project_id: "p1".into(),
            description: "kept".into(),
            // Sub-second parts the export never carried: dedupe must
            // still recognise this entry in its own export.
            started_at: 1_500,
            ended_at: 61_500,
            source: timetrack_proto::EntrySource::Manual,
            note: None,
        });
        let row = |project: &str, desc: &str, a: i64, b: i64| timetrack_core::ImportedEntry {
            project: project.into(),
            description: desc.into(),
            started_at: a,
            ended_at: b,
            source: timetrack_core::EntrySource::Manual,
            note: None,
        };
        let rows = vec![
            row("work", "kept", 1_000, 61_000), // already there (case aside)
            row("Work", "new", 2_000, 62_000),
            row("Work", "new", 2_000, 62_000), // repeat inside the file
            row("Home", "x", 3_000, 63_000),
        ];
        let plan = plan_import(&snap, &rows);
        assert_eq!(plan.new_projects, ["Home"]);
        assert_eq!(plan.skipped, 2);
        assert_eq!(plan.to_add.len(), 2);
        assert_eq!(plan.to_add[0].1.description, "new");
        assert_eq!(plan.to_add[1].0, "Home");
        let cli = Cli::try_parse_from(["timetrack", "import", "old.csv", "--dry-run"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Import { dry_run: true, .. })
        ));
    }

    #[test]
    fn source_commands_parse() {
        let cli =
            Cli::try_parse_from(["timetrack", "source-add", "cal.ics", "-p", "Work"]).unwrap();
        assert!(matches!(cli.command, Some(Command::SourceAdd { .. })));
        let cli = Cli::try_parse_from(["timetrack", "confirm", "sg-1"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Confirm { ref id, .. }) if id == "sg-1"
        ));
        let cli = Cli::try_parse_from(["timetrack", "suggestions", "--json"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Suggestions { json: true })
        ));
        // #150: discovery takes an endpoint, an optional lending source,
        // and an optional JSON switch.
        let cli = Cli::try_parse_from([
            "timetrack",
            "source-discover",
            "https://cal.test/",
            "--source",
            "src-1",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::SourceDiscover { json: true, .. })
        ));
        let cli =
            Cli::try_parse_from(["timetrack", "source-discover", "https://cal.test/"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::SourceDiscover {
                source: None,
                json: false,
                ..
            })
        ));
        // #153: update takes an id plus optional new values; setting
        // and clearing the mapping together is refused by clap.
        let cli = Cli::try_parse_from([
            "timetrack",
            "source-update",
            "src-1",
            "--kind",
            "caldav",
            "--locator",
            "https://cal.test/u/",
            "-p",
            "Work",
            "--label",
            "Cal",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::SourceUpdate {
                clear_project: false,
                ..
            })
        ));
        let cli = Cli::try_parse_from(["timetrack", "source-update", "src-1", "--clear-project"])
            .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::SourceUpdate {
                project: None,
                clear_project: true,
                ..
            })
        ));
        assert!(
            Cli::try_parse_from([
                "timetrack",
                "source-update",
                "src-1",
                "-p",
                "Work",
                "--clear-project",
            ])
            .is_err(),
            "setting and clearing the mapping conflict"
        );
    }

    #[test]
    fn list_json_respects_the_limit_and_names_entries() {
        let mut snap = Snapshot::default();
        for i in 0..3 {
            snap.entries.push(timetrack_proto::EntryView {
                id: format!("e{i}"),
                project_id: "p1".into(),
                description: format!("task {i}"),
                started_at: i * 60_000,
                ended_at: i * 60_000 + 30_000,
                source: timetrack_proto::EntrySource::Manual,
                note: None,
            });
        }
        let v: serde_json::Value = serde_json::from_str(&list_json(&snap, 2, None)).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 2);
        assert_eq!(v[0]["id"], "e0");
        assert_eq!(v[0]["description"], "task 0");
        assert_eq!(v[1]["id"], "e1");
    }

    /// A snapshot with ambiguous and unique id prefixes (ADR 0018).
    fn prefixed_snapshot() -> Snapshot {
        let mut snap = Snapshot::default();
        for (id, name, archived) in [
            ("aaaaaaaa-0000-4000-8000-000000000001", "Work", false),
            ("aaaaaaaa-0000-4000-8000-000000000002", "Home", false),
            ("cccccccc-0000-4000-8000-000000000001", "Solo", false),
            ("dddddddd-0000-4000-8000-000000000001", "Old", true),
        ] {
            snap.projects.push(timetrack_proto::ProjectView {
                id: id.into(),
                name: name.into(),
                colour: None,
                archived,
            });
        }
        for (id, project_id) in [
            (
                "bbbbbbbb-0000-4000-8000-000000000001",
                "aaaaaaaa-0000-4000-8000-000000000001",
            ),
            (
                "bbbbbbbb-0000-4000-8000-000000000002",
                "aaaaaaaa-0000-4000-8000-000000000002",
            ),
            (
                "eeeeeeee-0000-4000-8000-000000000001",
                "cccccccc-0000-4000-8000-000000000001",
            ),
        ] {
            snap.entries.push(timetrack_proto::EntryView {
                id: id.into(),
                project_id: project_id.into(),
                description: String::new(),
                started_at: 0,
                ended_at: 1,
                source: timetrack_proto::EntrySource::Manual,
                note: None,
            });
        }
        snap
    }

    #[test]
    fn find_entry_resolves_exact_then_unique_prefix() {
        let snap = prefixed_snapshot();
        assert_eq!(
            find_entry(&snap, "eeeeeeee-0000-4000-8000-000000000001")
                .unwrap()
                .id,
            "eeeeeeee-0000-4000-8000-000000000001"
        );
        assert_eq!(
            find_entry(&snap, "eeeeeeee").unwrap().id,
            "eeeeeeee-0000-4000-8000-000000000001"
        );
        assert_eq!(
            find_entry(&snap, "bbbbbbbb").unwrap_err().to_string(),
            "ambiguous entry id prefix 'bbbbbbbb'"
        );
        assert_eq!(
            find_entry(&snap, "nope").unwrap_err().to_string(),
            "no entry with id 'nope'"
        );
        assert_eq!(
            find_entry(&snap, "").unwrap_err().to_string(),
            "no entry with id ''",
            "empty never resolves"
        );
    }

    #[test]
    fn find_project_prefers_exact_then_name_then_prefix() {
        let snap = prefixed_snapshot();
        // Exact id first.
        assert_eq!(
            find_project(&snap, "aaaaaaaa-0000-4000-8000-000000000002")
                .unwrap()
                .name,
            "Home"
        );
        // Name second, case-insensitively, archived included.
        assert_eq!(find_project(&snap, "work").unwrap().name, "Work");
        assert_eq!(find_project(&snap, "OLD").unwrap().name, "Old");
        // Unique prefix third.
        assert_eq!(find_project(&snap, "cccccccc").unwrap().name, "Solo");
        // Ambiguous and unknown refuse in the service's words.
        assert_eq!(
            find_project(&snap, "aaaaaaaa").unwrap_err().to_string(),
            "ambiguous project id prefix 'aaaaaaaa'"
        );
        assert_eq!(
            find_project(&snap, "nope").unwrap_err().to_string(),
            "no project with id 'nope'"
        );
    }

    #[test]
    fn pick_project_offers_active_ids_names_and_prefixes() {
        let snap = prefixed_snapshot();
        // Exact, name, and unique prefix all resolve to the id.
        assert_eq!(
            pick_project(&snap, Some("cccccccc-0000-4000-8000-000000000001")).unwrap(),
            "cccccccc-0000-4000-8000-000000000001"
        );
        assert_eq!(
            pick_project(&snap, Some("home")).unwrap(),
            "aaaaaaaa-0000-4000-8000-000000000002"
        );
        assert_eq!(
            pick_project(&snap, Some("cccccccc")).unwrap(),
            "cccccccc-0000-4000-8000-000000000001"
        );
        // Archived projects are not candidates, even by exact id.
        assert!(pick_project(&snap, Some("dddddddd-0000-4000-8000-000000000001")).is_err());
        assert!(pick_project(&snap, Some("Old")).is_err());
        // Ambiguity is named; the unknown wording is unchanged.
        assert_eq!(
            pick_project(&snap, Some("aaaaaaaa"))
                .unwrap_err()
                .to_string(),
            "ambiguous project id prefix 'aaaaaaaa'"
        );
        let err = pick_project(&snap, Some("nope")).unwrap_err().to_string();
        assert!(err.contains("no active project named 'nope'"), "got: {err}");
        // No request still defaults to the first active project.
        assert_eq!(
            pick_project(&snap, None).unwrap(),
            "aaaaaaaa-0000-4000-8000-000000000001"
        );
    }

    #[test]
    fn service_refusals_print_once_not_three_times() {
        // A refusal nests three deep (`ClientError::Service` →
        // `ServiceError::Rule` → the rule) with the same wording at every
        // layer, so `timetrack undo <manual-entry-id>` rendered `Error`
        // plus `Caused by` 0 and 1 identically. `explain` flattens that.
        let err = anyhow::Error::new(ClientError::Service(timetrack_proto::ServiceError::Rule(
            timetrack_core::RuleError::NotQuickAdd("e1".into()),
        )));
        let explained = explain(err);
        assert_eq!(
            explained.to_string(),
            "entry 'e1' was not created by a quick add, so it cannot be undone"
        );
        let debug = format!("{explained:?}");
        assert!(
            !debug.contains("Caused by"),
            "a refusal must print once, got: {debug}"
        );
    }

    #[test]
    fn shorten_with_no_endpoint_refuses() {
        let err = check_shorten_args(&None, &None).unwrap_err().to_string();
        assert!(
            err.contains("--start") && err.contains("--end"),
            "the refusal must name the flags: {err}"
        );
        assert!(check_shorten_args(&Some("09:00".into()), &None).is_ok());
        assert!(check_shorten_args(&None, &Some("10:00".into())).is_ok());
    }

    #[test]
    fn merge_with_fewer_than_two_entries_refuses_up_front() {
        // The service's arity refusal names a `<need two entries>`
        // sentinel id; the CLI refuses first, naming the count.
        let err = check_merge_ids(&["e1".to_string()])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("need two") && err.contains('1'),
            "got: {err}"
        );
        assert!(check_merge_ids(&["e1".into(), "e2".into()]).is_ok());
    }

    #[test]
    fn piped_merge_confirmations_follow_the_same_yes_rule() {
        use std::io::BufReader;
        for yes in ["y\n", "YES\n", "  yes  \n"] {
            assert!(
                read_piped_confirmation(&mut BufReader::new(yes.as_bytes())).unwrap(),
                "{yes:?} should confirm"
            );
        }
        for no in ["n\n", "no\n", "yess\n"] {
            assert!(
                !read_piped_confirmation(&mut BufReader::new(no.as_bytes())).unwrap(),
                "{no:?} should cancel"
            );
        }
        // EOF (an empty pipe) refuses instead of hanging or merging.
        assert!(!read_piped_confirmation(&mut BufReader::new("".as_bytes())).unwrap());
    }

    #[test]
    fn emit_to_writes_lines_and_reports_broken_pipe() {
        struct FailPipe(std::io::ErrorKind);
        impl std::io::Write for FailPipe {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(self.0, "gone"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        // The writer `emit` relies on: lines end with a newline …
        let mut ok = Vec::new();
        emit_to(&mut ok, "hi").unwrap();
        assert_eq!(ok, b"hi\n");
        // … and a closed pipe surfaces as `BrokenPipe` (mapped to exit 0
        // by `emit`), while other failures stay distinct (still a panic).
        let err = emit_to(&mut FailPipe(std::io::ErrorKind::BrokenPipe), "hi").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
        let err = emit_to(&mut FailPipe(std::io::ErrorKind::PermissionDenied), "hi").unwrap_err();
        assert_ne!(err.kind(), std::io::ErrorKind::BrokenPipe);
    }
}
