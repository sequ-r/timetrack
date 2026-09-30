/* cli/main.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The TimeTrack terminal client.
//!
//! Two modes in one binary: a one-shot command surface for scripts and shell
//! aliases (`timetrack start "writing"`), and a full ratatui TUI
//! (`timetrack`, or `timetrack tui`). Both are pure clients — the running
//! timer lives in the service, so closing the terminal never stops it.

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::time::Duration;
use timetrack_cli::tui;
use timetrack_proto::Client;

/// How long to wait for the service to appear before giving up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Parser, Debug)]
#[command(
    name = "timetrack",
    version,
    about = "TimeTrack terminal client",
    long_about = "Control the TimeTrack service from the terminal.\n\n\
                  The service owns the running timer; this is a client, so \
                  quitting the TUI does not stop what you are timing."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Open the full terminal UI (the default when no subcommand is given).
    Tui,

    /// Start timing with an optional description.
    Start {
        /// What you are working on.
        description: Option<String>,
    },

    /// Stop the running timer and record the entry.
    Stop,

    /// Stop the running timer and discard it.
    Cancel,

    /// Print the current state and exit.
    Status,

    /// List tracked entries, most recent first.
    List {
        /// Show at most this many entries.
        #[arg(short, long, default_value_t = 20)]
        limit: usize,
    },

    /// Delete a finished entry by id.
    Remove { id: String },
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

    let client = Client::wait_for_service(CONNECT_TIMEOUT).await.map_err(|e| {
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
    match cli.command.expect("non-TUI command above") {
        Command::Tui => unreachable!("handled above"),
        Command::Start { description } => {
            let desc = description.unwrap_or_default();
            let e = client.start(&desc).await?;
            println!(
                "started: {}",
                if e.description.is_empty() {
                    "(no description)"
                } else {
                    &e.description
                }
            );
        }
        Command::Stop => {
            let e = client.stop().await?;
            println!(
                "stopped: {} ({})",
                if e.description.is_empty() {
                    "(no description)"
                } else {
                    &e.description
                },
                timetrack_core::format_duration(e.duration_ms(timetrack_core::now_ms()))
            );
        }
        Command::Cancel => {
            let e = client.cancel().await?;
            println!("discarded: {}", e.label());
        }
        Command::Status => {
            let s = client.snapshot().await?;
            let now = timetrack_core::now_ms();
            match s.running() {
                Some(r) => println!(
                    "running: {} ({})",
                    r.label(),
                    timetrack_core::format_duration(r.duration_ms(now))
                ),
                None => println!("running: (nothing)"),
            }
            println!("total:   {}", timetrack_core::format_duration(s.total_ms));
            println!("entries: {}", s.entries.len());
        }
        Command::List { limit } => {
            let s = client.snapshot().await?;
            let now = timetrack_core::now_ms();
            if s.entries.is_empty() {
                println!("no entries yet");
            }
            for e in s.entries.iter().take(limit) {
                let state = if e.is_running() { "running" } else { "done" };
                println!(
                    "{state}  {:>10}  {}  {}",
                    timetrack_core::format_duration(e.duration_ms(now)),
                    e.id,
                    e.label()
                );
            }
        }
        Command::Remove { id } => {
            client.remove(&id).await?;
            println!("removed: {id}");
        }
    }
    Ok(())
}
