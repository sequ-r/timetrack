/* cli/main.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The TimeTrack terminal client.
//!
//! Two modes in one binary: a one-shot command surface for scripts and shell
//! aliases (`timetrack quick -f 30`), and a full ratatui TUI (`timetrack`, or
//! `timetrack tui`). Both are pure clients -- the store lives in the service,
//! so closing the terminal never loses anything.
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
//! moves an entry between projects, `unarchive` reverses `archive`.

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::io::IsTerminal;
use std::time::Duration;
use timetrack_cli::tui;
use timetrack_proto::{Client, ProjectView, Snapshot};

/// How long to wait for the service to appear before giving up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Parser, Debug)]
#[command(
    name = "timetrack",
    version,
    about = "TimeTrack terminal client",
    long_about = "Control the TimeTrack service from the terminal.\n\n\
                  The service owns the store and does all the aggregation; this \
                  is a client, so quitting the TUI never loses tracked time."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Open the full terminal UI (the default when no subcommand is given).
    Tui,

    // --- the four add methods ---
    /// Method 1: record an explicit start and end.
    Add {
        /// Project to attribute the time to. Defaults to the first project.
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
        #[arg(short, long)]
        project: Option<String>,
        /// One of 5, 15, 30 or 60.
        #[arg(short, long)]
        minutes: i64,
    },

    // --- the three ways time comes back off ---
    /// Move an entry's endpoints. Shortens without deleting.
    Shorten {
        /// The entry to change.
        id: String,
        /// New start, in the same format as add --start.
        #[arg(short = 'S', long, allow_hyphen_values = true)]
        start: Option<String>,
        /// New end.
        #[arg(short = 'E', long, allow_hyphen_values = true)]
        end: Option<String>,
    },

    /// Remove the entry a quick add created. Refuses anything else.
    Undo { id: String },

    /// Delete an entry outright.
    Delete { id: String },

    /// Rewrite an entry's description.
    SetText {
        /// The entry to change.
        id: String,
        /// The new description.
        #[arg(allow_hyphen_values = true)]
        text: String,
    },

    /// Move an entry to another project. Totals follow the entry.
    SetProject {
        /// The entry to move.
        id: String,
        /// The project id or name.
        project: String,
    },

    // --- split and merge (REQUIREMENTS §8) ---
    /// Split one entry into two at a given moment. The total is unchanged.
    Split {
        /// The entry to split.
        id: String,
        /// Where to split: `-30` for 30 minutes ago, or `09:30` today.
        #[arg(short = 'A', long, allow_hyphen_values = true)]
        at: String,
    },

    /// Merge entries into one spanning earliest start to latest end.
    /// Merging overlapping entries shrinks the total by the overlap.
    Merge {
        /// The entries to merge (two or more; the service refuses fewer).
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
        /// The project id (or its current name).
        id: String,
        /// The new name.
        name: String,
    },

    /// Recolour a project. Colour is RRGGBB hex, e.g. `2ea043`.
    Recolour {
        /// The project id (or its current name).
        id: String,
        /// Six hex digits, with or without a leading `#`.
        colour: String,
    },

    /// Archive a project, keeping its history in totals.
    Archive { id: String },

    /// Unarchive a project, making it selectable again.
    Unarchive { id: String },

    /// Export entries as CSV (REQUIREMENTS §10).
    Export {
        /// What to export: `week` (current ISO week), `month` or `all`.
        #[arg(long, default_value = "all")]
        scope: String,
        /// Write to this file instead of stdout.
        #[arg(short, long)]
        out: Option<std::path::PathBuf>,
    },

    // --- reading ---
    /// Print this week, this month and the all-time total.
    Status,

    /// List tracked entries, most recent first.
    List {
        /// Show at most this many entries.
        #[arg(short, long, default_value_t = 20)]
        limit: usize,
    },
}

/// zbus uses the async-io backend (see the workspace Cargo.toml), so the
/// runtime is `async_io`, not tokio.
fn main() -> Result<()> {
    async_io::block_on(run())
}

async fn run() -> Result<()> {
    let cli = Cli::parse();

    // The TUI is interactive and wants the terminal to itself.
    if matches!(cli.command, None | Some(Command::Tui)) {
        return tui::run().await;
    }

    let client = Client::wait_for_service(CONNECT_TIMEOUT)
        .await
        .map_err(|e| {
            // A bare "ServiceUnknown" is accurate but unhelpful; say what to do.
            if e.to_string().contains("ServiceUnknown") {
                e.context(
                    "the TimeTrack service is not running.\n\
                 Start it with:  timetrack-service\n\
                 or enable the systemd user unit for your session.",
                )
            } else {
                e
            }
        })?;
    dispatch(client, cli.command.expect("non-TUI command above")).await
}

async fn dispatch(client: Client, command: Command) -> Result<()> {
    match command {
        Command::Tui => unreachable!("handled above"),

        Command::Add {
            project,
            description,
            start,
            end,
        } => {
            let snap = client.snapshot().await?;
            let project = pick_project(&snap, project.as_deref())?;
            let tz = snapshot_tz(&snap);
            let start_ms = parse_when(&start, "start", &tz)?;
            let end_ms = parse_when(&end, "end", &tz)?;
            let e = client.add(&project, &description, start_ms, end_ms).await?;
            println!(
                "added {} ({})",
                timetrack_core::format_duration(e.duration_ms()),
                e.id
            );
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
            println!(
                "added {} ending now ({})",
                timetrack_core::format_duration(e.duration_ms()),
                e.id
            );
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
            let tz = snapshot_tz(&snap);
            let end_ms = parse_when(&ended, "ended", &tz)?;
            let e = client
                .add_duration_ending(&project, &description, ms, end_ms)
                .await?;
            println!(
                "added {} ending {}ms before now ({})",
                timetrack_core::format_duration(e.duration_ms()),
                now_ms() - end_ms,
                e.id
            );
        }

        Command::Quick { project, minutes } => {
            let snap = client.snapshot().await?;
            let project = pick_project(&snap, project.as_deref())?;
            let ms = minutes * 60_000;
            anyhow::ensure!(
                timetrack_core::QUICK_ADD_MS.contains(&ms),
                "quick add takes one of {:?} minutes",
                timetrack_core::QUICK_ADD_MS
                    .iter()
                    .map(|m| m / 60_000)
                    .collect::<Vec<_>>()
            );
            let e = client.quick_add(&project, ms).await?;
            println!(
                "added {} ({}) -- undo with: timetrack undo {}",
                timetrack_core::format_duration(e.duration_ms()),
                e.id,
                e.id
            );
        }

        Command::Shorten { id, start, end } => {
            // Unspecified endpoints are left alone, so `shorten e2 -E 30m`
            // moves only the end and the entry is kept.
            let snap = client.snapshot().await?;
            let existing = snap
                .entries
                .iter()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("no entry with id '{id}'"))?;
            let tz = snapshot_tz(&snap);
            let start_ms = match &start {
                Some(s) => parse_when(s, "start", &tz)?,
                None => existing.started_at,
            };
            let end_ms = match &end {
                Some(s) => parse_when(s, "end", &tz)?,
                None => existing.ended_at,
            };
            let e = client.set_times(&id, start_ms, end_ms).await?;
            println!(
                "shortened to {} (the entry is still there: {})",
                timetrack_core::format_duration(e.duration_ms()),
                e.id
            );
        }

        Command::Undo { id } => {
            client.undo_quick_add(&id).await?;
            println!("undid quick add: {id}");
        }

        Command::Delete { id } => {
            client.delete_entry(&id).await?;
            println!("deleted: {id}");
        }

        Command::SetText { id, text } => {
            let snap = client.snapshot().await?;
            snap.entries
                .iter()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("no entry with id '{id}'"))?;
            let e = client.set_text(&id, &text).await?;
            println!("set text on {} to \"{}\"", e.id, e.description);
        }

        Command::SetProject { id, project } => {
            let snap = client.snapshot().await?;
            snap.entries
                .iter()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("no entry with id '{id}'"))?;
            let target = find_project(&snap, &project)?;
            let e = client.set_project(&id, &target.id).await?;
            println!("moved {} to {} ({})", e.id, target.name, target.id);
        }

        Command::Split { id, at } => {
            let snap = client.snapshot().await?;
            let existing = snap
                .entries
                .iter()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("no entry with id '{id}'"))?;
            let tz = snapshot_tz(&snap);
            let at_ms = parse_when(&at, "at", &tz)?;
            if at_ms <= existing.started_at || at_ms >= existing.ended_at {
                anyhow::bail!("split point is outside entry '{id}'");
            }
            let (a, b) = client.split(&id, at_ms).await?;
            println!(
                "split into ({}) {} and ({}) {}",
                a.id,
                timetrack_core::format_duration(a.duration_ms()),
                b.id,
                timetrack_core::format_duration(b.duration_ms()),
            );
        }

        Command::Merge { ids, yes } => {
            let snap = client.snapshot().await?;
            let mut found = Vec::with_capacity(ids.len());
            for id in &ids {
                found.push(
                    snap.entries
                        .iter()
                        .find(|e| e.id == *id)
                        .ok_or_else(|| anyhow::anyhow!("no entry with id '{id}'"))?,
                );
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
                .map(|p| p.name.clone())
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
                        println!("merge cancelled");
                        return Ok(());
                    }
                } else {
                    anyhow::bail!("merge needs confirmation ({delta}); re-run with --yes");
                }
            }
            let merged = client.merge(&ids).await?;
            println!(
                "merged {} entries into ({}) {} ({delta})",
                ids.len(),
                merged.id,
                timetrack_core::format_duration(merged.duration_ms()),
            );
        }

        Command::Project { name } => {
            let p = client.add_project(&name).await?;
            println!("created project {} ({})", p.name, p.id);
        }

        Command::Rename { id, name } => {
            // Refused here, not just by the service: at the D-Bus level an
            // empty name means "keep", so passing blank through would
            // silently succeed as a no-op. Same wording as the core guard.
            if name.trim().is_empty() {
                anyhow::bail!("project name cannot be blank");
            }
            let snap = client.snapshot().await?;
            let existing = find_project(&snap, &id)?;
            let old = existing.name.clone();
            let p = client.update_project(&existing.id, &name, -1).await?;
            println!("renamed {old} to {} ({})", p.name, p.id);
        }

        Command::Recolour { id, colour } => {
            let snap = client.snapshot().await?;
            let existing = find_project(&snap, &id)?;
            let value = parse_colour(&colour)?;
            let p = client
                .update_project(&existing.id, "", value as i64)
                .await?;
            println!("recoloured {} ({}) to #{value:06x}", p.name, p.id);
        }

        Command::Archive { id } => {
            let p = client.set_archived(&id, true).await?;
            println!(
                "archived {} ({}) -- its entries stay in historical totals",
                p.name, p.id
            );
        }

        Command::Unarchive { id } => {
            let p = client.set_archived(&id, false).await?;
            println!("unarchived {} ({})", p.name, p.id);
        }

        Command::Status => print_status(&client.snapshot().await?),

        Command::List { limit } => print_list(&client.snapshot().await?, limit),

        Command::Export { scope, out } => {
            let csv = client.export_csv(&scope).await?;
            match out {
                Some(path) => {
                    std::fs::write(&path, &csv)?;
                    println!("exported to {}", path.display());
                }
                None => print!("{csv}"),
            }
        }
    }
    Ok(())
}

fn print_status(s: &Snapshot) {
    println!(
        "this week:  {}  ({} entries)",
        timetrack_core::format_duration(s.week.total_ms),
        s.week.entry_count
    );
    println!(
        "this month: {}",
        timetrack_core::format_duration(s.month.total_ms)
    );
    println!(
        "all time:   {}",
        timetrack_core::format_duration(s.total_ms)
    );
    if !s.over_24h_days.is_empty() {
        // Legal, but worth surfacing: it usually means a mistyped interval
        // rather than a genuinely enormous day (REQUIREMENTS §4).
        println!(
            "warning:    {} day(s) total more than 24h",
            s.over_24h_days.len()
        );
    }
}

fn print_list(s: &Snapshot, limit: usize) {
    if s.entries.is_empty() {
        println!("no entries yet");
        return;
    }
    for e in s.entries.iter().take(limit) {
        let marker = if e.source == timetrack_proto::EntrySource::QuickAdd {
            "+"
        } else {
            " "
        };
        println!(
            "{marker} {:>10}  {:<6}  {}",
            timetrack_core::format_duration(e.duration_ms()),
            e.id,
            e.label()
        );
    }
}

/// Resolve a project argument to a snapshot entry, for rename/recolour.
///
/// Unlike `pick_project` this matches archived projects too: renaming one
/// is harmless, and refusing would strand the numbered placeholders this
/// replaced. Matches the id first, then the name case-insensitively.
fn find_project<'a>(snap: &'a Snapshot, wanted: &str) -> Result<&'a ProjectView> {
    snap.projects
        .iter()
        .find(|p| p.id == wanted)
        .or_else(|| {
            snap.projects
                .iter()
                .find(|p| p.name.eq_ignore_ascii_case(wanted))
        })
        .ok_or_else(|| anyhow::anyhow!("no project with id '{wanted}'"))
}

/// Parse an `RRGGBB` colour like `2ea043` (leading `#` allowed).
fn parse_colour(text: &str) -> Result<u32> {
    let hex = text.trim().strip_prefix('#').unwrap_or(text.trim());
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        anyhow::bail!("'{text}' is not RRGGBB hex (e.g. 2ea043)");
    }
    Ok(u32::from_str_radix(hex, 16).expect("six hex digits always fit"))
}

/// Resolve a project argument to an id, defaulting to the first active one.
///
/// Matches either the id or (case-insensitively) the name: `timetrack quick
/// -p p1` should work the same as `-p Work`, since ids are what every other
/// command prints back.
fn pick_project(snap: &Snapshot, requested: Option<&str>) -> Result<String> {
    let active: Vec<&ProjectView> = snap.projects.iter().filter(|p| !p.archived).collect();
    match requested {
        Some(want) => active
            .iter()
            .find(|p| p.id == want)
            .or_else(|| active.iter().find(|p| p.name.eq_ignore_ascii_case(want)))
            .map(|p| p.id.clone())
            .ok_or_else(|| {
                let names: Vec<&str> = active.iter().map(|p| p.name.as_str()).collect();
                anyhow::anyhow!("no active project named '{want}'. Active: {names:?}")
            }),
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

/// The snapshot's zone: prefer the name, fall back to the fixed offset for
/// old services that send no name.
fn snapshot_tz(snap: &Snapshot) -> timetrack_core::Tz {
    timetrack_core::Tz::from_snapshot(&snap.tz, snap.local_offset_ms)
}

/// Whether a typed merge confirmation counts as "yes".
///
/// Split out so the actual prompt stays a thin reader: `y`/`yes` in any
/// case confirms, everything else (including empty) cancels.
fn confirmed(line: &str) -> bool {
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
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
    fn the_command_line_parses() {
        // The subcommand surface is the CLI's real API; a rename that breaks
        // someone's alias should fail here, not at runtime.
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
