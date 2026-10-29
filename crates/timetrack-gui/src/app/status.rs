/* gui/app/status.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The footer status line: what to show, and what kind of problem it is.
//!
//! The kind travels with the text rather than being re-parsed out of it, so
//! the view never has to recognise a failure by sniffing its wording.

use timetrack_proto::{ClientError, no_service_hint, version_mismatch_hint};

/// What kind of problem a status line reports.
///
/// The kind drives behaviour -- a missing service offers to start it, and
/// only connection notices clear on recovery -- so it travels with the text
/// rather than being re-parsed out of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StatusKind {
    /// Nothing owns the service name.
    NoService,
    /// The service answered unknown-interface or unknown-method.
    VersionMismatch,
    /// The service refused an action; the text is its own wording.
    Service,
    /// Local UI notes and bus-level failures, not service refusals.
    Local,
}

/// Whether a status is a connection notice: one that a snapshot proves over.
///
/// The "service is not running" and "different versions" notices describe the
/// connection rather than one action, so the first snapshot that arrives
/// clears them -- the service is back, or it is speaking this client's
/// interface now. An ordinary error is about one action and survives until
/// the next one, because snapshots arrive every second and would otherwise
/// make it unreadable.
pub(super) fn is_connection_notice(kind: StatusKind) -> bool {
    matches!(kind, StatusKind::NoService | StatusKind::VersionMismatch)
}

/// The footer status line: what to show, and what kind of problem it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StatusMsg {
    pub(super) text: String,
    pub(super) kind: StatusKind,
}

impl StatusMsg {
    /// A local UI note or bus-level failure: shown as-is, never offered the
    /// start-service action, and dismissed by the next action's success.
    pub(super) fn local(text: impl Into<String>) -> Self {
        StatusMsg {
            text: text.into(),
            kind: StatusKind::Local,
        }
    }
}

impl From<&ClientError> for StatusMsg {
    /// Map a call failure to footer text without matching on message text:
    /// the variant decides both the wording and the kind.
    fn from(error: &ClientError) -> Self {
        match error {
            ClientError::NoService => StatusMsg {
                text: no_service_hint(),
                kind: StatusKind::NoService,
            },
            ClientError::VersionMismatch { .. } => StatusMsg {
                text: version_mismatch_hint("GUI"),
                kind: StatusKind::VersionMismatch,
            },
            // Service- and bus-origin text is sanitized (#101): a refusal
            // can embed stored text, and the footer is a single line. The
            // shared hints above are ours (the version hint's newlines are
            // deliberate) and pass through untouched.
            ClientError::Service(service) => StatusMsg {
                text: timetrack_core::sanitize_display(&service.to_string()),
                kind: StatusKind::Service,
            },
            ClientError::Transport(detail) => StatusMsg {
                text: timetrack_core::sanitize_display(detail),
                kind: StatusKind::Local,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::testsupport::status_for;
    use timetrack_proto::{ClientError, ServiceError};

    #[test]
    fn no_service_offers_to_start_it() {
        let note = status_for(&ClientError::NoService);
        assert_eq!(note.kind, StatusKind::NoService);
        assert!(
            note.text.contains("not running"),
            "must keep the start-service wording: {}",
            note.text
        );
    }

    #[test]
    fn version_mismatch_names_the_fix() {
        let note = status_for(&ClientError::VersionMismatch {
            name: "org.freedesktop.DBus.Error.UnknownInterface".into(),
        });
        assert_eq!(note.kind, StatusKind::VersionMismatch);
        assert!(
            note.text.contains("different versions"),
            "must keep the reinstall wording: {}",
            note.text
        );
    }

    #[test]
    fn refusals_show_the_services_own_wording() {
        // Backward-compatible strings, dispatched by variant.
        let note = status_for(&ClientError::Service(ServiceError::Rule(
            timetrack_core::RuleError::NotQuickAdd("e1".into()),
        )));
        assert_eq!(note.kind, StatusKind::Service);
        assert_eq!(
            note.text,
            "entry 'e1' was not created by a quick add, so it cannot be undone"
        );
    }

    #[test]
    fn hostile_names_never_steer_dispatch() {
        // A project literally named after an error state must still land in
        // the Service bucket: dispatch keys on the variant, never on the
        // text it happens to contain.
        for name in [
            "org.freedesktop.DBus.Error.UnknownInterface",
            "the service is not running, honestly",
            "ServiceUnknown",
        ] {
            let note = status_for(&ClientError::Service(ServiceError::Rule(
                timetrack_core::RuleError::DuplicateProject(name.into()),
            )));
            assert_eq!(
                note.kind,
                StatusKind::Service,
                "project name {name:?} must not steer dispatch"
            );
            assert!(
                note.text.contains(name),
                "wording must survive: {}",
                note.text
            );
        }
    }

    #[test]
    fn service_text_is_sanitized_for_the_footer() {
        // #101: a refusal embedding a hostile stored name must not carry raw
        // controls into the status line; the wording otherwise survives.
        let note = status_for(&ClientError::Service(ServiceError::Rule(
            timetrack_core::RuleError::DuplicateProject("Evil\n\u{1b}[2J".into()),
        )));
        assert_eq!(note.kind, StatusKind::Service);
        assert_eq!(note.text, "a project named 'Evil  [2J' already exists");
        // Our own multi-line hint keeps its line breaks.
        let note = status_for(&ClientError::VersionMismatch {
            name: "org.freedesktop.DBus.Error.UnknownInterface".into(),
        });
        assert!(
            note.text.contains('\n'),
            "hint lines survive: {}",
            note.text
        );
    }

    #[test]
    fn transport_failures_are_local_notes() {
        let note = status_for(&ClientError::Transport("d-bus wobble".into()));
        assert_eq!(note.kind, StatusKind::Local);
        assert_eq!(note.text, "d-bus wobble");
    }
}
