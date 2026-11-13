# Calendar and tasks are fetched read-only, never built in

TimeTrack connects to external calendar and task services instead of growing its own (REQUIREMENTS §19). Connectors run in the service, fetch read-only — `.ics` file/URL first, then CalDAV (`VEVENT` calendars, `VTODO` tasks) — cache what they fetch in SQLite, and offer each event or task as a suggestion: a proposed entry the user confirms, edits, or dismisses. Fetching never creates an entry and never writes back, confirmed entries record their external UID so nothing is suggested twice, each external calendar maps to at most one project, and secrets live in the OS credential store, referenced by name from the database. GUI and CLI see the same suggestions because the service owns them.

## Considered Options

- **Built-in calendar + task manager.** Rejected: a second calendar to keep in step with the user's first one, a large recurring-event and reminder surface, and no path for the calendars users already have.
- **Both built-in and external.** Rejected: doubles the surface and invents two-way conflicts for a read-only need.
- **Per-vendor APIs (Google/Microsoft) first.** Rejected as the first step: OAuth per vendor before any value ships; CalDAV covers the standard case once, and vendor APIs remain possible later behind the same suggestion model.
- **Auto-create entries from events.** Rejected: tracked time is the user's statement about where time went (§1); a fetched meeting is evidence for that statement, not the statement itself.
