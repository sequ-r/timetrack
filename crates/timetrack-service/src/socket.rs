/* service/socket.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The local-socket server (REQUIREMENTS §16, ADR 0013).
//!
//! Dual-served beside D-Bus through the transition: the same
//! [`EntryService`] answers both transports, each request dispatching
//! onto the same methods the bus wrappers call, and a mutation made
//! over either one announces `Changed` on both (the funnel notifies
//! socket subscribers; the wrappers emit the bus signal).
//!
//! Socket ownership is the single-instance mechanism the bus name used
//! to be alone: serving starts by probing the socket — a live answer
//! means another service already owns it and this process is a second
//! copy, which has exited harmlessly since the AppImage era. A socket
//! file nobody answers on is stale and is replaced.

use std::path::PathBuf;
use std::sync::Arc;

use timetrack_proto::ServiceError;
use timetrack_proto::socket::{
    ConnStream, Frame, Listener, Request, Response, SOCKET_API_VERSION, SocketClient, read_frame,
    set_read_timeout, socket_path, write_frame,
};

use crate::interface::EntryService;

/// How binding the service socket went.
pub enum ServeOutcome {
    /// Listening; connections are served on background threads.
    Listening(PathBuf),
    /// A live service already owns the socket: this process is a
    /// second copy and should exit quietly.
    AlreadyRunning,
}

/// Dispatch one request onto the service. `SubscribeChanged` never
/// reaches here (the connection handler keeps that connection open).
fn dispatch(service: &EntryService, request: Request) -> Result<Response, ServiceError> {
    Ok(match request {
        Request::ApiVersion => Response::ApiVersion(SOCKET_API_VERSION),
        Request::Snapshot => Response::Snapshot(Box::new(service.snapshot())),
        Request::SubscribeChanged => Response::Subscribed,
        Request::Add {
            project_id,
            description,
            started_at,
            ended_at,
        } => Response::Entry(Box::new(service.add(
            &project_id,
            &description,
            started_at,
            ended_at,
        )?)),
        Request::AddDuration {
            project_id,
            description,
            duration_ms,
        } => Response::Entry(Box::new(service.add_duration(
            &project_id,
            &description,
            duration_ms,
        )?)),
        Request::AddDurationEnding {
            project_id,
            description,
            duration_ms,
            ended_at,
        } => Response::Entry(Box::new(service.add_duration_ending(
            &project_id,
            &description,
            duration_ms,
            ended_at,
        )?)),
        Request::QuickAdd {
            project_id,
            duration_ms,
        } => Response::Entry(Box::new(service.quick_add(&project_id, duration_ms)?)),
        Request::SetTimes {
            id,
            started_at,
            ended_at,
        } => Response::Entry(Box::new(service.set_times(&id, started_at, ended_at)?)),
        Request::SetText { id, description } => {
            Response::Entry(Box::new(service.set_text(&id, &description)?))
        }
        Request::SetProject { id, project_id } => {
            Response::Entry(Box::new(service.set_project(&id, &project_id)?))
        }
        Request::UndoQuickAdd { id } => {
            service.undo_quick_add(&id)?;
            Response::Unit
        }
        Request::DeleteEntry { id } => {
            service.delete_entry(&id)?;
            Response::Unit
        }
        Request::Split { id, at_ms } => {
            let (a, b) = service.split(&id, at_ms)?;
            Response::EntryPair(Box::new(a), Box::new(b))
        }
        Request::Merge { ids } => Response::Entry(Box::new(service.merge(&ids)?)),
        Request::AddProject { name } => Response::Project(Box::new(service.add_project(&name)?)),
        Request::UpdateProject { id, name, colour } => {
            Response::Project(Box::new(service.update_project(&id, &name, colour)?))
        }
        Request::SetArchived { id, archived } => {
            Response::Project(Box::new(service.set_archived(&id, archived)?))
        }
        Request::DeleteProject { id } => {
            service.delete_project(&id)?;
            Response::Unit
        }
        Request::ExportCsv { scope } => Response::Csv(service.export_csv(&scope)?),
        Request::ExportCsvAt { scope, anchor_ms } => {
            Response::Csv(service.export_csv_at(&scope, anchor_ms)?)
        }
        Request::ExportCsvRange { from_ms, to_ms } => {
            Response::Csv(service.export_csv_range(from_ms, to_ms)?)
        }
        Request::AddSource {
            kind,
            locator,
            project_id,
            label,
        } => Response::Source(Box::new(service.add_source(
            &kind,
            &locator,
            project_id.as_deref(),
            &label,
        )?)),
        Request::ListSources => Response::Sources(service.sources()?),
        Request::RefreshSource { id } => Response::Refresh(service.refresh_source(id.as_deref())?),
        Request::ListSuggestions => Response::Suggestions(service.suggestions()?),
        Request::ConfirmSuggestion { id, project_id } => Response::Entry(Box::new(
            service.confirm_suggestion(&id, project_id.as_deref())?,
        )),
        Request::DismissSuggestion { id } => {
            service.dismiss_suggestion(&id)?;
            Response::Unit
        }
        Request::SetNote { id, note } => {
            Response::Entry(Box::new(service.set_note(&id, note.as_deref())?))
        }
    })
}

fn handle_conn(mut stream: ConnStream, service: Arc<EntryService>) {
    set_read_timeout(&stream, Some(timetrack_proto::CALL_TIMEOUT));
    let first = match read_frame(&mut stream) {
        Ok(Some(Frame::Request(request))) => request,
        _ => return,
    };
    if first == Request::SubscribeChanged {
        let rx = service.subscribe_changed();
        if write_frame(&mut stream, &Frame::Reply(Ok(Response::Subscribed))).is_err() {
            return;
        }
        set_read_timeout(&stream, None);
        // One Changed frame per notification until the write fails
        // (the subscriber left) or the service drops the sender side.
        while rx.recv().is_ok() {
            if write_frame(&mut stream, &Frame::Changed).is_err() {
                return;
            }
        }
        return;
    }
    let reply = Frame::Reply(dispatch(&service, first));
    let _ = write_frame(&mut stream, &reply);
}

/// Bind the default service socket and serve it on background threads.
/// Dual-serves beside D-Bus; see the module docs for ownership rules.
pub fn serve(service: Arc<EntryService>) -> anyhow::Result<ServeOutcome> {
    serve_at(service, socket_path())
}

/// [`serve`] at an explicit path (test rigs name their own, beside
/// their private bus, so no test can reach a real service).
pub fn serve_at(service: Arc<EntryService>, path: PathBuf) -> anyhow::Result<ServeOutcome> {
    if SocketClient::new(path.clone()).probe() {
        return Ok(ServeOutcome::AlreadyRunning);
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    // Nobody answered: a leftover socket file is stale by definition
    // here (on Windows the path is only a pipe *name*, so this is a
    // no-op there and the bind itself refuses a live owner).
    #[cfg(unix)]
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    let listener = Listener::bind(&path)?;
    std::thread::spawn(move || {
        loop {
            match listener.accept() {
                Ok(stream) => {
                    let service = Arc::clone(&service);
                    std::thread::spawn(move || handle_conn(stream, service));
                }
                Err(e) => eprintln!("timetrack-service: socket accept failed ({e})"),
            }
        }
    });
    Ok(ServeOutcome::Listening(path))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use timetrack_proto::socket::SocketClient;
    use timetrack_store::SqliteStore;

    fn service_at(name: &str) -> (Arc<EntryService>, PathBuf) {
        let dir = std::env::temp_dir().join(format!("tt-sock-svc-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let svc =
            EntryService::new(SqliteStore::open_at(&dir.join("store.json")).unwrap()).unwrap();
        (Arc::new(svc), dir)
    }

    #[test]
    fn socket_round_trip_and_changed_subscription() {
        let (svc, dir) = service_at("rt");
        let path = dir.join("s.sock");
        match serve_at(Arc::clone(&svc), path.clone()).unwrap() {
            ServeOutcome::Listening(p) => assert_eq!(p, path),
            ServeOutcome::AlreadyRunning => panic!("a fresh socket must not read as owned"),
        }
        let client = SocketClient::new(path.clone());
        let snap = client.call(Request::Snapshot).unwrap().snapshot().unwrap();
        assert_eq!(snap.projects.len(), 1, "the seed is served over socket");
        let project = snap.projects[0].id.clone();

        // Subscribe first, then mutate over a separate call connection:
        // the subscription must see the change.
        let changed = svc.subscribe_changed();
        let entry = client
            .call(Request::QuickAdd {
                project_id: project,
                duration_ms: 900_000,
            })
            .unwrap()
            .entry()
            .unwrap();
        assert_eq!(entry.duration_ms(), 900_000);
        assert_eq!(
            changed.try_recv(),
            Ok(()),
            "a socket mutation notifies subscribers"
        );

        let snap = client.call(Request::Snapshot).unwrap().snapshot().unwrap();
        assert_eq!(snap.entries.len(), 1);
        assert_eq!(snap.week.total_ms, 900_000);

        // A second service over the same socket is a second copy.
        match serve_at(Arc::clone(&svc), path).unwrap() {
            ServeOutcome::AlreadyRunning => {}
            ServeOutcome::Listening(_) => panic!("the owned socket must refuse a second server"),
        }
    }

    #[test]
    fn ics_source_refresh_confirm_round_trip() {
        // §19 end to end at the service seam: register a file source,
        // refresh (fetch+parse+store, creating nothing), confirm one
        // suggestion into an External entry, dismiss the other.
        let (svc, dir) = service_at("ics");
        let cal = dir.join("cal.ics");
        // Refresh only suggests near-term items (§19), so the fixture's
        // events sit an hour either side of *now*, not a fixed date
        // that ages out of the window.
        let basic = |ms: i64| {
            timetrack_core::format_local_iso8601(ms, &timetrack_core::Tz::utc())
                .replace(['-', ':'], "")
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let (s1, e1) = (basic(now - 1_800_000), basic(now + 1_800_000));
        let (s2, e2) = (basic(now + 3_600_000), basic(now + 5_400_000));
        std::fs::write(
            &cal,
            format!(
                "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:ev1\nSUMMARY:Design review\nDTSTART:{s1}Z\nDTEND:{e1}Z\nEND:VEVENT\nBEGIN:VEVENT\nUID:ev2\nSUMMARY:Skip me\nDTSTART:{s2}Z\nDTEND:{e2}Z\nEND:VEVENT\nEND:VCALENDAR\n"
            ),
        )
        .unwrap();
        let path = dir.join("s.sock");
        let _ = serve_at(Arc::clone(&svc), path.clone()).unwrap();
        let client = SocketClient::new(path);
        let src = client
            .call(Request::AddSource {
                kind: "ics_file".into(),
                locator: cal.display().to_string(),
                project_id: None,
                label: String::new(),
            })
            .unwrap()
            .source()
            .unwrap();
        assert_eq!(src.label, "cal.ics");
        let tally = client
            .call(Request::RefreshSource { id: None })
            .unwrap()
            .refresh()
            .unwrap();
        assert_eq!((tally.fetched, tally.inserted, tally.skipped), (2, 2, 0));
        // A second refresh inserts nothing new.
        let tally = client
            .call(Request::RefreshSource { id: None })
            .unwrap()
            .refresh()
            .unwrap();
        assert_eq!(tally.inserted, 0);
        // Fetching created no entries.
        assert!(svc.snapshot().entries.is_empty());

        let pending = client
            .call(Request::ListSuggestions)
            .unwrap()
            .suggestions()
            .unwrap();
        assert_eq!(pending.len(), 2);
        let entry = client
            .call(Request::ConfirmSuggestion {
                id: pending[0].id.clone(),
                project_id: None,
            })
            .unwrap()
            .entry()
            .unwrap();
        assert_eq!(entry.description, "Design review");
        assert_eq!(entry.source, timetrack_proto::EntrySource::External);
        assert_eq!(svc.snapshot().entries.len(), 1);
        // Confirming again refuses instead of double-creating.
        assert!(
            client
                .call(Request::ConfirmSuggestion {
                    id: pending[0].id.clone(),
                    project_id: None,
                })
                .is_err()
        );
        client
            .call(Request::DismissSuggestion {
                id: pending[1].id.clone(),
            })
            .unwrap()
            .unit()
            .unwrap();
        assert!(
            client
                .call(Request::ListSuggestions)
                .unwrap()
                .suggestions()
                .unwrap()
                .is_empty()
        );
        // Notes ride the same socket surface (import restoration).
        let noted = client
            .call(Request::SetNote {
                id: entry.id.clone(),
                note: Some("from calendar".into()),
            })
            .unwrap()
            .entry()
            .unwrap();
        assert_eq!(noted.note.as_deref(), Some("from calendar"));
    }

    #[test]
    fn socket_refusals_come_back_typed() {
        let (svc, dir) = service_at("typed");
        let path = dir.join("s.sock");
        let _ = serve_at(Arc::clone(&svc), path.clone()).unwrap();
        let client = SocketClient::new(path);
        let err = client
            .call(Request::ExportCsv {
                scope: "bogus".into(),
            })
            .unwrap_err();
        assert!(
            matches!(
                err,
                timetrack_proto::ClientError::Service(
                    timetrack_proto::ServiceError::UnknownExportScope(_)
                )
            ),
            "refusals keep their type over the socket: {err:?}"
        );
    }

    #[test]
    fn a_stale_socket_file_is_replaced() {
        let (svc, dir) = service_at("stale");
        let path = dir.join("s.sock");
        std::fs::write(&path, "not a socket").unwrap();
        match serve_at(svc, path.clone()).unwrap() {
            ServeOutcome::Listening(_) => {}
            ServeOutcome::AlreadyRunning => panic!("an unanswered file is stale, not owned"),
        }
        assert!(SocketClient::new(path).probe());
    }
}
