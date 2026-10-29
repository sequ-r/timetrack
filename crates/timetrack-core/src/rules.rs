/* core/rules.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The rules: everything that can change a `Store`.
//!
//! Every operation takes the store mutably and an
//! explicit timestamp, and never reads a clock itself. That keeps the whole
//! module deterministic and testable without I/O.

use crate::model::{Entry, EntrySource, MS_PER_DAY, Project, Store, instant_in_range};
use serde::{Deserialize, Serialize};

/// Why an operation was refused.
///
/// Serializable because the typed service error embeds it verbatim on the
/// wire (`timetrack-proto::ServiceError`); the human-readable strings below
/// are the contract CLI output and scripts match on, so they never change.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuleError {
    #[error("no entry with id '{0}'")]
    UnknownEntry(String),
    #[error("no project with id '{0}'")]
    UnknownProject(String),
    #[error("a project named '{0}' already exists")]
    DuplicateProject(String),
    /// A project name trimmed to nothing. Creating or renaming states it
    /// plainly rather than storing an empty label no picker could show.
    #[error("project name cannot be blank")]
    BlankProjectName,
    /// Free text past its length cap (#101). The payloads name the field in
    /// the user's words ("project name", "description", "note") and the cap,
    /// so one variant serves every text field.
    #[error("{0} is too long (max {1} characters)")]
    TextTooLong(String, usize),
    /// Free text containing control characters (#101): newlines, escape
    /// sequences, and friends have no place in a label that terminals, menus,
    /// and spreadsheets will render.
    #[error("{0} must not contain control characters")]
    TextHasControlCharacters(String),
    #[error("an entry must not end before it starts")]
    NegativeDuration,
    /// An instant outside the supported range (years 0001–9999, #97).
    ///
    /// Its own variant rather than `NegativeDuration`: the interval may be
    /// perfectly ordered, it is simply at a point in time the calendar math
    /// does not cover. The payload is the refused endpoint (saturated when
    /// the true endpoint overflowed `i64` altogether, #98).
    #[error("instant {0} is outside the supported range")]
    InstantOutOfRange(i64),
    #[error("cannot delete the last project")]
    LastProject,
    /// A project with entries cannot be deleted outright.
    ///
    /// Its own variant rather than reusing `UnknownProject`, because the
    /// project does exist -- deleting it would silently orphan its history,
    /// so the client has to be able to say what actually happened.
    #[error("project '{0}' still has entries; move or delete them first")]
    ProjectHasEntries(String),
    /// A quick-add duration outside the fixed buckets.
    ///
    /// `quick_add` used to accept any duration in release builds (the bucket
    /// check was a `debug_assert`), which let arbitrary entries in through
    /// tagged as quick-adds and therefore undoable as a group that never was
    /// one (REQUIREMENTS §6).
    #[error("quick add takes one of 5, 15, 30 or 60 minutes, not {0}ms")]
    InvalidQuickAdd(i64),
    /// A merge that cannot be performed without losing or misattributing data.
    ///
    /// Kept distinct from `UnknownEntry`: the entries exist, but merging them
    /// as asked would silently discard information -- see `merge_entries`.
    #[error("cannot merge: {0}")]
    InvalidMerge(String),
    /// Quick-add undo was asked to remove something it did not create.
    ///
    /// Its own variant rather than reusing `UnknownEntry`, because the entry
    /// does exist -- it just was not made by a quick add, and the client needs
    /// to be able to say so.
    #[error("entry '{0}' was not created by a quick add, so it cannot be undone")]
    NotQuickAdd(String),
}

pub type RuleResult<T> = Result<T, RuleError>;

/// Milliseconds in the quick-add buckets (REQUIREMENTS §6).
pub const QUICK_ADD_MS: [i64; 4] = [5 * 60_000, 15 * 60_000, 30 * 60_000, 60 * 60_000];

/// Length caps for stored free text, in characters (#101).
///
/// Enforced here, at the trust boundary every mutation crosses, so a direct
/// D-Bus caller is bounded exactly like the GUI and CLI. The values are
/// generous for their jobs — a menu label, a one-line summary, a short
/// note — while keeping any one field from becoming an unbounded payload
/// that every snapshot then carries to every client.
pub const MAX_PROJECT_NAME_CHARS: usize = 120;
/// Cap for an entry's description. See [`MAX_PROJECT_NAME_CHARS`].
pub const MAX_DESCRIPTION_CHARS: usize = 500;
/// Cap for an entry's note. See [`MAX_PROJECT_NAME_CHARS`].
pub const MAX_NOTE_CHARS: usize = 2_000;

/// Refuse free text with control characters or past its cap (#101).
///
/// `what` names the field in the user's words; it is what the error says.
/// Length counts characters, not bytes, so the cap means the same thing in
/// every language.
fn check_text(what: &str, text: &str, max_chars: usize) -> RuleResult<()> {
    if text.chars().any(|c| c.is_control()) {
        return Err(RuleError::TextHasControlCharacters(what.to_string()));
    }
    if text.chars().count() > max_chars {
        return Err(RuleError::TextTooLong(what.to_string(), max_chars));
    }
    Ok(())
}

/// Source of identifiers. Kept abstract so tests can pin them.
pub trait Ids {
    fn next(&mut self) -> String;
}

/// Sequential ids, `e1`, `p1`, ...
// Clone lets the service snapshot the counter before a mutation and
// restore it if the save fails (#100).
#[derive(Debug, Clone)]
pub struct Counter {
    next_entry: u64,
    next_project: u64,
}

impl Default for Counter {
    fn default() -> Self {
        Counter {
            next_entry: 1,
            next_project: 1,
        }
    }
}

impl Counter {
    /// Continue past anything already on disk, so a restart cannot mint an id
    /// that collides with a stored one.
    pub fn continuing(store: &Store) -> Self {
        let entry_max = store
            .entries
            .iter()
            .filter_map(|e| e.id.strip_prefix('e'))
            .filter_map(|n| n.parse::<u64>().ok())
            .max();
        let project_max = store
            .projects
            .iter()
            .filter_map(|p| p.id.strip_prefix('p'))
            .filter_map(|n| n.parse::<u64>().ok())
            .max();
        Counter {
            next_entry: entry_max.map_or(1, |m| m + 1),
            next_project: project_max.map_or(1, |m| m + 1),
        }
    }
}

impl Ids for Counter {
    fn next(&mut self) -> String {
        let id = format!("e{}", self.next_entry);
        self.next_entry += 1;
        id
    }
}

/// Mints project ids from the same counter.
pub fn next_project_id(counter: &mut Counter) -> String {
    let id = format!("p{}", counter.next_project);
    counter.next_project += 1;
    id
}

// --- projects ---------------------------------------------------------------

/// Create a project. Names need not be unique in general, but an exact
/// duplicate is almost always a double-click rather than intent.
pub fn create_project(store: &mut Store, counter: &mut Counter, name: &str) -> RuleResult<Project> {
    let name = name.trim();
    if name.is_empty() {
        return Err(RuleError::BlankProjectName);
    }
    check_text("project name", name, MAX_PROJECT_NAME_CHARS)?;
    if store.projects.iter().any(|p| p.name == name) {
        return Err(RuleError::DuplicateProject(name.to_string()));
    }
    let project = Project {
        id: next_project_id(counter),
        name: name.to_string(),
        colour: None,
        archived: false,
    };
    store.projects.push(project.clone());
    Ok(project)
}

/// Rename a project, or set its colour.
pub fn update_project(
    store: &mut Store,
    id: &str,
    name: Option<&str>,
    colour: Option<Option<u32>>,
) -> RuleResult<Project> {
    if !store.projects.iter().any(|p| p.id == id) {
        return Err(RuleError::UnknownProject(id.to_string()));
    }
    // Validate the name before taking the mutable borrow, otherwise the
    // uniqueness scan cannot also borrow `store.projects`.
    let trimmed = name.map(str::trim);
    if let Some(new_name) = trimmed {
        if new_name.is_empty() {
            return Err(RuleError::BlankProjectName);
        }
        check_text("project name", new_name, MAX_PROJECT_NAME_CHARS)?;
        if store
            .projects
            .iter()
            .any(|p| p.id != id && p.name == new_name)
        {
            return Err(RuleError::DuplicateProject(new_name.to_string()));
        }
    }
    let project = store
        .projects
        .iter_mut()
        .find(|p| p.id == id)
        .expect("checked above");
    if let Some(new_name) = trimmed {
        project.name = new_name.to_string();
    }
    if let Some(c) = colour {
        project.colour = c;
    }
    Ok(project.clone())
}

/// Archive or unarchive a project. Archiving never deletes entries and never
/// removes it from historical totals (REQUIREMENTS §14).
pub fn set_archived(store: &mut Store, id: &str, archived: bool) -> RuleResult<Project> {
    let project = store
        .projects
        .iter_mut()
        .find(|p| p.id == id)
        .ok_or_else(|| RuleError::UnknownProject(id.to_string()))?;
    project.archived = archived;
    Ok(project.clone())
}

/// Delete a project. Refused when it is the last one, or when it still has
/// entries — deleting a project with history would silently orphan them.
pub fn delete_project(store: &mut Store, id: &str) -> RuleResult<Project> {
    if store.projects.len() <= 1 {
        return Err(RuleError::LastProject);
    }
    if store.entries.iter().any(|e| e.project_id == id) {
        return Err(RuleError::ProjectHasEntries(id.to_string()));
    }
    let idx = store
        .projects
        .iter()
        .position(|p| p.id == id)
        .ok_or_else(|| RuleError::UnknownProject(id.to_string()))?;
    Ok(store.projects.remove(idx))
}

// --- entries ----------------------------------------------------------------

/// Refuse an instant outside the supported range (#97).
fn check_instant(ms: i64) -> RuleResult<()> {
    if instant_in_range(ms) {
        Ok(())
    } else {
        Err(RuleError::InstantOutOfRange(ms))
    }
}

/// Refuse an interval with either endpoint outside the supported range.
fn check_interval(started_at: i64, ended_at: i64) -> RuleResult<()> {
    check_instant(started_at)?;
    check_instant(ended_at)
}

/// The start of an entry of `duration_ms` ending at `ended_at`, checked
/// (#98): on overflow the refused endpoint is reported saturated, so the
/// error still names the end of time the caller asked for.
fn checked_start(ended_at: i64, duration_ms: i64) -> RuleResult<i64> {
    ended_at
        .checked_sub(duration_ms)
        .ok_or_else(|| RuleError::InstantOutOfRange(ended_at.saturating_sub(duration_ms)))
}

/// Validate an interval and build the entry. Overlap is explicitly *not*
/// checked (REQUIREMENTS §4).
fn build_entry(
    store: &Store,
    ids: &mut dyn Ids,
    project_id: &str,
    description: &str,
    started_at: i64,
    ended_at: i64,
    source: EntrySource,
) -> RuleResult<Entry> {
    if store.project(project_id).is_none() {
        return Err(RuleError::UnknownProject(project_id.to_string()));
    }
    check_interval(started_at, ended_at)?;
    if ended_at < started_at {
        return Err(RuleError::NegativeDuration);
    }
    // Descriptions are stored trimmed and bounded (#101): every client used
    // to store them verbatim, padding and all. All validation runs before
    // `ids.next()`, so a refused entry consumes no id.
    let description = description.trim();
    check_text("description", description, MAX_DESCRIPTION_CHARS)?;
    Ok(Entry {
        id: ids.next(),
        project_id: project_id.to_string(),
        description: description.to_string(),
        started_at,
        ended_at,
        source,
        note: None,
    })
}

/// Create an entry over an arbitrary interval (method 1, REQUIREMENTS §5).
pub fn create_entry(
    store: &mut Store,
    ids: &mut dyn Ids,
    project_id: &str,
    description: &str,
    started_at: i64,
    ended_at: i64,
) -> RuleResult<Entry> {
    create_entry_from(
        store,
        ids,
        project_id,
        description,
        started_at,
        ended_at,
        EntrySource::Manual,
    )
}

/// As `create_entry`, but tagged with how the entry was recorded.
pub fn create_entry_from(
    store: &mut Store,
    ids: &mut dyn Ids,
    project_id: &str,
    description: &str,
    started_at: i64,
    ended_at: i64,
    source: EntrySource,
) -> RuleResult<Entry> {
    let entry = build_entry(
        store,
        ids,
        project_id,
        description,
        started_at,
        ended_at,
        source,
    )?;
    store.entries.push(entry.clone());
    Ok(entry)
}

/// Record `duration_ms` ending at `now` (methods 2 and 3).
///
/// `now` is supplied by the caller: the core never reads a clock. Method 2
/// passes the current instant; method 3 passes an instant in the past.
pub fn create_duration(
    store: &mut Store,
    ids: &mut dyn Ids,
    project_id: &str,
    description: &str,
    duration_ms: i64,
    now: i64,
) -> RuleResult<Entry> {
    let started_at = checked_start(now, duration_ms)?;
    create_entry(store, ids, project_id, description, started_at, now)
}

/// Record `duration_ms` ending at an explicit instant (method 3 with the end
/// named). The subtraction lives here, checked (#98), rather than in the
/// service: the service holds no rules, and its unchecked copy of this
/// arithmetic was one of the overflow sites.
pub fn create_duration_ending(
    store: &mut Store,
    ids: &mut dyn Ids,
    project_id: &str,
    description: &str,
    duration_ms: i64,
    ended_at: i64,
) -> RuleResult<Entry> {
    let started_at = checked_start(ended_at, duration_ms)?;
    create_entry(store, ids, project_id, description, started_at, ended_at)
}

/// Add one of the quick-add buckets, ending now (REQUIREMENTS §6).
///
/// Each tap creates its own entry rather than extending an existing one, so
/// that undo is unambiguous and nothing rewrites an earlier entry's
/// `ended_at`. Durations outside the buckets are refused: a non-bucket entry
/// tagged `QuickAdd` would be undoable under false pretences.
pub fn quick_add(
    store: &mut Store,
    ids: &mut dyn Ids,
    project_id: &str,
    duration_ms: i64,
    now: i64,
) -> RuleResult<Entry> {
    if !QUICK_ADD_MS.contains(&duration_ms) {
        return Err(RuleError::InvalidQuickAdd(duration_ms));
    }
    let started_at = checked_start(now, duration_ms)?;
    create_entry_from(
        store,
        ids,
        project_id,
        "",
        started_at,
        now,
        EntrySource::QuickAdd,
    )
}

/// Move an entry's interval (method 1's "remove" side, §5).
///
/// Shortening only ever moves `ended_at` earlier. It never deletes: the rule
/// is that no method may lose data as a side effect of reducing time.
pub fn set_times(store: &mut Store, id: &str, started_at: i64, ended_at: i64) -> RuleResult<Entry> {
    check_interval(started_at, ended_at)?;
    if ended_at < started_at {
        return Err(RuleError::NegativeDuration);
    }
    let entry = store
        .get_mut(id)
        .ok_or_else(|| RuleError::UnknownEntry(id.to_string()))?;
    entry.started_at = started_at;
    entry.ended_at = ended_at;
    Ok(entry.clone())
}

/// Reassign an entry to another project.
pub fn set_project(store: &mut Store, id: &str, project_id: &str) -> RuleResult<Entry> {
    if store.project(project_id).is_none() {
        return Err(RuleError::UnknownProject(project_id.to_string()));
    }
    let entry = store
        .get_mut(id)
        .ok_or_else(|| RuleError::UnknownEntry(id.to_string()))?;
    entry.project_id = project_id.to_string();
    Ok(entry.clone())
}

/// Edit an entry's description and note.
///
/// Both fields are trimmed and bounded exactly as at creation (#101), and
/// both are validated before either is stored, so a refused edit changes
/// nothing.
pub fn set_text(
    store: &mut Store,
    id: &str,
    description: Option<&str>,
    note: Option<Option<&str>>,
) -> RuleResult<Entry> {
    let description = description.map(str::trim);
    if let Some(d) = description {
        check_text("description", d, MAX_DESCRIPTION_CHARS)?;
    }
    let note = note.map(|n| n.map(str::trim));
    if let Some(Some(n)) = note {
        check_text("note", n, MAX_NOTE_CHARS)?;
    }
    let entry = store
        .get_mut(id)
        .ok_or_else(|| RuleError::UnknownEntry(id.to_string()))?;
    if let Some(d) = description {
        entry.description = d.to_string();
    }
    if let Some(n) = note {
        entry.note = n.map(str::to_string);
    }
    Ok(entry.clone())
}

/// Delete an entry outright. Distinct from shortening, which never deletes.
pub fn delete_entry(store: &mut Store, id: &str) -> RuleResult<Entry> {
    let idx = store
        .entries
        .iter()
        .position(|e| e.id == id)
        .ok_or_else(|| RuleError::UnknownEntry(id.to_string()))?;
    Ok(store.entries.remove(idx))
}

/// Split an entry at `at_ms`, producing two entries that together cover
/// exactly the original interval (REQUIREMENTS §8).
///
/// The split point must fall strictly inside the entry, otherwise it is not a
/// split and the caller should edit times instead.
///
/// The second half mints its id from `ids`, like every other new entry: the
/// old `{id}-b` scheme collided when one entry was split twice, silently
/// leaving two entries with the same id in the store.
pub fn split_entry(
    store: &mut Store,
    ids: &mut dyn Ids,
    id: &str,
    at_ms: i64,
) -> RuleResult<(Entry, Entry)> {
    let original = store
        .get(id)
        .cloned()
        .ok_or_else(|| RuleError::UnknownEntry(id.to_string()))?;
    // The split point becomes an endpoint of both halves, so it is held to
    // the same range as any other endpoint (#97).
    check_instant(at_ms)?;
    if at_ms <= original.started_at || at_ms >= original.ended_at {
        return Err(RuleError::NegativeDuration);
    }

    // Mint until the id is actually fresh, so even a store whose ids predate
    // the counter scheme cannot gain a duplicate.
    let second_id = loop {
        let candidate = ids.next();
        if store.get(&candidate).is_none() {
            break candidate;
        }
    };

    // The original entry becomes the first half, in place, so a client that
    // is already holding its id keeps pointing at something valid.
    let first = Entry {
        ended_at: at_ms,
        ..original.clone()
    };
    let second = Entry {
        id: second_id,
        started_at: at_ms,
        ..original
    };
    *store.get_mut(id).expect("checked above") = first.clone();
    store.entries.push(second.clone());
    Ok((first, second))
}

/// Merge several entries into one spanning earliest start to latest end
/// (REQUIREMENTS §8).
///
/// The result is the **union** of the intervals, so merging overlapping
/// entries shrinks the total. That is deliberate: merging is the user stating
/// the overlap was a mistake. Preserving the summed total instead would
/// require an entry claiming time it did not occupy.
///
/// Two requests are refused rather than misapplied: merging an entry with
/// itself (which used to remove the survivor and then panic looking it up),
/// and merging entries from different projects (whose time would otherwise be
/// silently reattributed to the first entry's project).
pub fn merge_entries(store: &mut Store, ids: &[String]) -> RuleResult<Entry> {
    if ids.len() < 2 {
        return Err(RuleError::UnknownEntry("<need two entries>".into()));
    }
    let mut seen = std::collections::HashSet::with_capacity(ids.len());
    for id in ids {
        if !seen.insert(id) {
            return Err(RuleError::InvalidMerge(format!(
                "entry '{id}' is listed twice"
            )));
        }
    }
    let mut chosen = Vec::with_capacity(ids.len());
    for id in ids {
        chosen.push(
            store
                .get(id)
                .cloned()
                .ok_or_else(|| RuleError::UnknownEntry(id.clone()))?,
        );
    }
    if chosen.iter().any(|e| e.project_id != chosen[0].project_id) {
        return Err(RuleError::InvalidMerge(
            "entries belong to different projects; move them onto one project first".into(),
        ));
    }
    let project_id = chosen[0].project_id.clone();
    let started_at = chosen.iter().map(|e| e.started_at).min().unwrap();
    let ended_at = chosen.iter().map(|e| e.ended_at).max().unwrap();
    // Entries the rules accepted are always in range; this refuses to mint a
    // fresh out-of-range entry out of a hand-edited store's extremes (#97).
    check_interval(started_at, ended_at)?;

    // Keep the first id as the merged entry's, so a client holding that
    // handle keeps pointing at something valid.
    let survivor = chosen.remove(0);
    let merged = Entry {
        id: survivor.id.clone(),
        project_id,
        description: survivor.description.clone(),
        started_at,
        ended_at,
        source: EntrySource::Manual,
        note: survivor.note.clone(),
    };
    let removed: Vec<String> = chosen.iter().map(|e| e.id.clone()).collect();
    store.entries.retain(|e| !removed.contains(&e.id));
    *store.get_mut(&survivor.id).expect("survivor still present") = merged.clone();
    Ok(merged)
}

/// How much summed time a merge of `intervals` would drop, in milliseconds
/// (REQUIREMENTS §8).
///
/// Positive when overlapping entries collapse (the overlap was a mistake, so
/// the total shrinks), zero when touching entries join losslessly, negative
/// when disjoint entries span a gap (the merged entry covers time none of
/// its parts occupied, so the total grows). Both UIs state this number in
/// the merge confirmation, computed here so they cannot disagree.
pub fn merge_shrink_ms(intervals: &[(i64, i64)]) -> i64 {
    if intervals.is_empty() {
        return 0;
    }
    // Saturating (#98): this only ever feeds a confirmation prompt, so on
    // absurd stored intervals it saturates instead of wrapping. Intervals
    // the rules accepted (in range, ordered) never reach saturation.
    let sum: i64 = intervals.iter().fold(0i64, |acc, &(start, end)| {
        acc.saturating_add(end.saturating_sub(start).max(0))
    });
    let start = intervals.iter().map(|&(start, _)| start).min().unwrap_or(0);
    let end = intervals.iter().map(|&(_, end)| end).max().unwrap_or(0);
    sum.saturating_sub(end.saturating_sub(start).max(0))
}

/// State what a merge does to the summed total, for confirmations.
///
/// The overlap case is the point of §8 (merging states the overlap was a
/// mistake); the other two keep the confirmation honest when entries merely
/// touch or span a gap.
pub fn format_merge_delta(shrink_ms: i64) -> String {
    if shrink_ms > 0 {
        format!("total shrinks by {}", crate::format_duration(shrink_ms))
    } else if shrink_ms < 0 {
        // `saturating_neg`: `i64::MIN` has no positive counterpart (#98).
        format!(
            "total grows by {}",
            crate::format_duration(shrink_ms.saturating_neg())
        )
    } else {
        "total unchanged".to_string()
    }
}

/// Whether any day's summed time exceeds 24 hours (REQUIREMENTS §4).
///
/// Flagged, never rejected: overlapping entries are legal, and the point is
/// to surface a probable duplicate rather than to forbid the data.
///
/// Each entry buckets by the offset in force at its own start, so days either
/// side of a DST transition are still whole local days.
pub fn days_exceeding_24h(store: &Store, tz: &crate::tz::Tz) -> Vec<i64> {
    let mut per_day = std::collections::BTreeMap::new();
    for e in &store.entries {
        let slot = per_day.entry(e.local_day(tz)).or_insert(0i64);
        *slot = slot.saturating_add(e.duration_ms());
    }
    per_day
        .into_iter()
        .filter(|(_, ms)| *ms > MS_PER_DAY)
        .map(|(day, _)| day)
        .collect()
}

/// Convenience: does any entry land on `day`, in local time?
pub fn has_entry_on(store: &Store, day: i64, tz: &crate::tz::Tz) -> bool {
    store.entries.iter().any(|e| e.local_day(tz) == day)
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: i64 = MS_PER_DAY;

    fn store_with_project() -> Store {
        let mut s = Store::default();
        s.projects.push(Project {
            id: "p1".into(),
            name: "Work".into(),
            colour: None,
            archived: false,
        });
        s
    }

    fn ids() -> Counter {
        Counter::default()
    }

    // --- projects ---

    #[test]
    fn create_project_mints_and_appends() {
        let mut s = store_with_project();
        let mut c = Counter::continuing(&s);
        let p = create_project(&mut s, &mut c, "Personal").unwrap();
        assert_eq!(p.id, "p2");
        assert_eq!(s.projects.len(), 2);
    }

    #[test]
    fn duplicate_project_name_is_refused() {
        let mut s = store_with_project();
        let mut c = ids();
        assert_eq!(
            create_project(&mut s, &mut c, "Work").unwrap_err(),
            RuleError::DuplicateProject("Work".into())
        );
    }

    #[test]
    fn blank_project_name_is_refused() {
        // The exact variant matters: blank used to report DuplicateProject(""),
        // which tells the user a project named '' already exists.
        for name in ["", "   ", "\t"] {
            let mut s = store_with_project();
            let mut c = ids();
            assert_eq!(
                create_project(&mut s, &mut c, name).unwrap_err(),
                RuleError::BlankProjectName,
                "blank project name {name:?} should have its own rule error"
            );
            assert_eq!(s.projects.len(), 1, "a refused project must not be stored");
            assert_eq!(c.next_project, 1, "a rejected project does not use an id");
        }
    }

    #[test]
    fn project_names_with_controls_or_past_the_cap_are_refused() {
        // #101: the cap and the control check live in the core, so no client
        // (or direct D-Bus caller) can store a label that breaks a renderer.
        let mut s = store_with_project();
        let mut c = ids();
        for bad in ["Wo\nrk", "Wo\trk", "\u{1b}[31mWork", "Work\u{7}"] {
            assert_eq!(
                create_project(&mut s, &mut c, bad).unwrap_err(),
                RuleError::TextHasControlCharacters("project name".into()),
                "{bad:?}"
            );
        }
        let too_long = "x".repeat(MAX_PROJECT_NAME_CHARS + 1);
        assert_eq!(
            create_project(&mut s, &mut c, &too_long).unwrap_err(),
            RuleError::TextTooLong("project name".into(), MAX_PROJECT_NAME_CHARS)
        );
        assert_eq!(s.projects.len(), 1, "nothing refused may be stored");
        // Exactly at the cap is fine, and rename holds the same line.
        let at_cap = "y".repeat(MAX_PROJECT_NAME_CHARS);
        let mut c = Counter::continuing(&s);
        let p = create_project(&mut s, &mut c, &at_cap).unwrap();
        assert_eq!(
            update_project(&mut s, &p.id, Some("a\nb"), None).unwrap_err(),
            RuleError::TextHasControlCharacters("project name".into())
        );
        assert_eq!(s.project(&p.id).unwrap().name, at_cap);
    }

    #[test]
    fn descriptions_and_notes_are_trimmed_and_bounded() {
        // #101: stored trimmed (the GUI used to store padding verbatim),
        // control characters and over-length refused, ids unconsumed.
        let mut s = store_with_project();
        let mut c = ids();
        let e = create_entry(&mut s, &mut c, "p1", "  review  ", 0, 1_000).unwrap();
        assert_eq!(e.description, "review");
        assert_eq!(
            create_entry(&mut s, &mut c, "p1", "a\nb", 0, 1_000).unwrap_err(),
            RuleError::TextHasControlCharacters("description".into())
        );
        let too_long = "d".repeat(MAX_DESCRIPTION_CHARS + 1);
        assert_eq!(
            create_entry(&mut s, &mut c, "p1", &too_long, 0, 1_000).unwrap_err(),
            RuleError::TextTooLong("description".into(), MAX_DESCRIPTION_CHARS)
        );
        assert_eq!(s.entries.len(), 1);
        assert_eq!(c.next_entry, 2, "refused entries consume no id");
        // set_text trims, bounds, and is all-or-nothing.
        let e = set_text(&mut s, &e.id, Some("  notes  "), Some(Some("  n  "))).unwrap();
        assert_eq!(e.description, "notes");
        assert_eq!(e.note.as_deref(), Some("n"));
        let note_too_long = "n".repeat(MAX_NOTE_CHARS + 1);
        assert_eq!(
            set_text(&mut s, &e.id, Some("changed"), Some(Some(&note_too_long))).unwrap_err(),
            RuleError::TextTooLong("note".into(), MAX_NOTE_CHARS)
        );
        assert_eq!(
            s.get(&e.id).unwrap().description,
            "notes",
            "a refused note must not half-apply the description"
        );
        assert_eq!(
            set_text(&mut s, &e.id, None, Some(Some("a\u{1b}b"))).unwrap_err(),
            RuleError::TextHasControlCharacters("note".into())
        );
    }

    #[test]
    fn ids_do_not_collide_after_reload() {
        // Restarting the service must not reissue an id already on disk.
        let mut s = store_with_project();
        let mut c = ids();
        create_entry(&mut s, &mut c, "p1", "", 0, 1000).unwrap();
        create_entry(&mut s, &mut c, "p1", "", 0, 1000).unwrap();

        // A reload resumes from the highest id on disk, not from 1.
        let mut reloaded = Counter::continuing(&s);
        let e = create_entry(&mut s, &mut reloaded, "p1", "", 0, 1000).unwrap();
        // The new entry is in the store by definition, so the thing to assert
        // is that its id is *unique*, not that it is absent.
        let occurrences = s.entries.iter().filter(|x| x.id == e.id).count();
        assert_eq!(occurrences, 1, "id {} was issued more than once", e.id);
        assert_eq!(e.id, "e3", "resumed past e1 and e2");
        assert_eq!(s.entries.len(), 3);
    }

    #[test]
    fn counter_continuing_resumes_after_the_highest_id() {
        let mut s = store_with_project();
        s.entries.push(Entry {
            id: "e7".into(),
            project_id: "p1".into(),
            description: String::new(),
            started_at: 0,
            ended_at: 1,
            source: EntrySource::Manual,
            note: None,
        });
        let mut c = Counter::continuing(&s);
        let e = create_entry(&mut s, &mut c, "p1", "", 0, 1).unwrap();
        assert_eq!(e.id, "e8");
    }

    #[test]
    fn archiving_keeps_entries_and_history() {
        let mut s = store_with_project();
        let mut c = ids();
        create_entry(&mut s, &mut c, "p1", "", 0, 1000).unwrap();
        set_archived(&mut s, "p1", true).unwrap();
        assert_eq!(s.entries.len(), 1);
        assert_eq!(s.total_for_project("p1"), 1000, "history must survive");
        assert!(s.project("p1").unwrap().archived);
    }

    #[test]
    fn last_project_cannot_be_deleted() {
        let mut s = store_with_project();
        assert_eq!(
            delete_project(&mut s, "p1").unwrap_err(),
            RuleError::LastProject
        );
    }

    #[test]
    fn rename_changes_only_the_name() {
        let mut s = store_with_project();
        let p = update_project(&mut s, "p1", Some("Deep Work"), None).unwrap();
        assert_eq!(p.name, "Deep Work");
        assert_eq!(s.project("p1").unwrap().name, "Deep Work");
    }

    #[test]
    fn recolour_sets_and_clears() {
        let mut s = store_with_project();
        let p = update_project(&mut s, "p1", None, Some(Some(0x2ea043))).unwrap();
        assert_eq!(p.colour, Some(0x2ea043));
        let p = update_project(&mut s, "p1", None, Some(None)).unwrap();
        assert_eq!(p.colour, None);
    }

    #[test]
    fn rename_rejects_unknown_blank_and_duplicate_names() {
        let mut s = store_with_project();
        let mut c = Counter::continuing(&s);
        create_project(&mut s, &mut c, "Personal").unwrap();
        assert_eq!(
            update_project(&mut s, "nope", Some("X"), None).unwrap_err(),
            RuleError::UnknownProject("nope".into())
        );
        assert_eq!(
            update_project(&mut s, "p1", Some("   "), None).unwrap_err(),
            RuleError::BlankProjectName
        );
        assert_eq!(
            update_project(&mut s, "p1", Some("Personal"), None).unwrap_err(),
            RuleError::DuplicateProject("Personal".into())
        );
        // Keeping the same name is not a duplicate of anything.
        assert!(update_project(&mut s, "p1", Some("Work"), None).is_ok());
        assert_eq!(s.project("p1").unwrap().name, "Work");
    }

    // --- creating entries ---

    #[test]
    fn create_entry_records_the_interval() {
        let mut s = store_with_project();
        let mut c = ids();
        let e = create_entry(&mut s, &mut c, "p1", "deep work", 100, 500).unwrap();
        assert_eq!(e.duration_ms(), 400);
        assert_eq!(e.source, EntrySource::Manual);
        assert_eq!(e.description, "deep work");
    }

    #[test]
    fn entries_on_an_unknown_project_are_refused() {
        let mut s = store_with_project();
        let mut c = ids();
        assert_eq!(
            create_entry(&mut s, &mut c, "nope", "", 0, 1).unwrap_err(),
            RuleError::UnknownProject("nope".into())
        );
    }

    #[test]
    fn an_entry_may_not_end_before_it_starts() {
        let mut s = store_with_project();
        let mut c = ids();
        assert_eq!(
            create_entry(&mut s, &mut c, "p1", "", 500, 100).unwrap_err(),
            RuleError::NegativeDuration
        );
        assert!(s.entries.is_empty(), "a refused entry must not be stored");
    }

    #[test]
    fn entry_endpoints_outside_the_supported_range_are_refused() {
        use crate::model::{MAX_INSTANT_MS, MIN_INSTANT_MS};
        // #97: every path that can mint or move an endpoint refuses cleanly
        // and stores nothing. The error names the refused endpoint.
        for (start, end, bad) in [
            (i64::MIN, 0, i64::MIN),
            (0, i64::MAX, i64::MAX),
            (MIN_INSTANT_MS - 1, MIN_INSTANT_MS - 1, MIN_INSTANT_MS - 1),
            (MAX_INSTANT_MS + 1, MAX_INSTANT_MS + 1, MAX_INSTANT_MS + 1),
        ] {
            let mut s = store_with_project();
            let mut c = ids();
            assert_eq!(
                create_entry(&mut s, &mut c, "p1", "", start, end).unwrap_err(),
                RuleError::InstantOutOfRange(bad),
                "({start}, {end})"
            );
            assert!(s.entries.is_empty(), "a refused entry must not be stored");
        }
        // The boundary instants themselves are supported.
        let mut s = store_with_project();
        let mut c = ids();
        create_entry(&mut s, &mut c, "p1", "", MIN_INSTANT_MS, MIN_INSTANT_MS).unwrap();
        let e = create_entry(&mut s, &mut c, "p1", "", MIN_INSTANT_MS, MAX_INSTANT_MS).unwrap();
        // set_times holds the same line and leaves the entry untouched.
        assert_eq!(
            set_times(&mut s, &e.id, 0, i64::MAX).unwrap_err(),
            RuleError::InstantOutOfRange(i64::MAX)
        );
        assert_eq!(s.get(&e.id).unwrap().ended_at, MAX_INSTANT_MS);
    }

    #[test]
    fn overflowing_duration_arithmetic_is_refused_not_wrapped() {
        // #98: `ended_at - duration_ms` near the i64 edges used to panic in
        // debug and wrap in release. Every flavour now refuses typed.
        let mut s = store_with_project();
        let mut c = ids();
        assert_eq!(
            create_duration(&mut s, &mut c, "p1", "", i64::MIN, 0).unwrap_err(),
            RuleError::InstantOutOfRange(i64::MAX),
            "0 - i64::MIN saturates to the far future it asked for"
        );
        assert_eq!(
            create_duration_ending(&mut s, &mut c, "p1", "", i64::MAX, i64::MIN).unwrap_err(),
            RuleError::InstantOutOfRange(i64::MIN)
        );
        // A merely out-of-range (but representable) start reports itself.
        assert_eq!(
            create_duration_ending(&mut s, &mut c, "p1", "", 1_000, i64::MAX).unwrap_err(),
            RuleError::InstantOutOfRange(i64::MAX - 1_000)
        );
        // A negative duration is still just a backwards interval.
        assert_eq!(
            create_duration(&mut s, &mut c, "p1", "", -1_000, 0).unwrap_err(),
            RuleError::NegativeDuration
        );
        assert!(s.entries.is_empty(), "nothing refused may be stored");
        // In-range durations ending at an explicit instant still work.
        let e = create_duration_ending(&mut s, &mut c, "p1", "", 3_600_000, 10 * D).unwrap();
        assert_eq!((e.started_at, e.ended_at), (10 * D - 3_600_000, 10 * D));
    }

    #[test]
    fn overlapping_entries_are_both_stored_and_both_counted() {
        // The central decision of REQUIREMENTS §4.
        let mut s = store_with_project();
        let mut c = ids();
        create_entry(&mut s, &mut c, "p1", "a", 0, D).unwrap();
        create_entry(&mut s, &mut c, "p1", "b", D / 2, D + D / 2).unwrap();
        assert_eq!(s.entries.len(), 2, "overlap is legal");
        assert_eq!(s.total_ms(), 2 * D, "and sums, not unions");
    }

    #[test]
    fn create_duration_backdates_from_now() {
        let mut s = store_with_project();
        let mut c = ids();
        let e = create_duration(&mut s, &mut c, "p1", "", 3_600_000, 10 * D).unwrap();
        assert_eq!(e.ended_at, 10 * D);
        assert_eq!(e.started_at, 10 * D - 3_600_000);
    }

    #[test]
    fn quick_add_makes_its_own_entry_ending_now() {
        let mut s = store_with_project();
        let mut c = ids();
        let e = quick_add(&mut s, &mut c, "p1", 15 * 60_000, D).unwrap();
        assert_eq!(e.source, EntrySource::QuickAdd);
        assert_eq!(e.ended_at, D);
        assert_eq!(e.duration_ms(), 15 * 60_000);
    }

    #[test]
    fn successive_quick_adds_are_separate_entries() {
        // Not increments: each tap is its own entry, so undo is unambiguous
        // and no earlier entry's ended_at is rewritten (REQUIREMENTS §6).
        let mut s = store_with_project();
        let mut c = ids();
        let a = quick_add(&mut s, &mut c, "p1", 5 * 60_000, D).unwrap();
        let b = quick_add(&mut s, &mut c, "p1", 5 * 60_000, D + 60_000).unwrap();
        assert_ne!(a.id, b.id);
        assert_eq!(a.ended_at, D, "the first entry must be untouched");
        assert_eq!(s.entries.len(), 2);
        assert_eq!(s.total_ms(), 10 * 60_000);
    }

    // --- editing ---

    #[test]
    fn set_times_shortens_without_deleting() {
        let mut s = store_with_project();
        let mut c = ids();
        let e = create_entry(&mut s, &mut c, "p1", "", 0, D).unwrap();
        set_times(&mut s, &e.id, 0, D / 2).unwrap();
        assert_eq!(s.entries.len(), 1, "shortening must never delete");
        assert_eq!(s.total_ms(), D / 2);
    }

    #[test]
    fn set_times_refuses_a_reversed_interval() {
        let mut s = store_with_project();
        let mut c = ids();
        let e = create_entry(&mut s, &mut c, "p1", "", 0, D).unwrap();
        assert_eq!(
            set_times(&mut s, &e.id, D, 0).unwrap_err(),
            RuleError::NegativeDuration
        );
        assert_eq!(s.total_ms(), D, "a refused edit must not apply");
    }

    #[test]
    fn set_project_moves_an_entry() {
        let mut s = store_with_project();
        let mut c = Counter::continuing(&s);
        create_project(&mut s, &mut c, "Personal").unwrap();
        let e = create_entry(&mut s, &mut c, "p1", "", 0, 100).unwrap();
        set_project(&mut s, &e.id, "p2").unwrap();
        assert_eq!(s.total_for_project("p1"), 0);
        assert_eq!(s.total_for_project("p2"), 100);
    }

    #[test]
    fn delete_entry_removes_only_that_entry() {
        let mut s = store_with_project();
        let mut c = ids();
        let a = create_entry(&mut s, &mut c, "p1", "", 0, 100).unwrap();
        create_entry(&mut s, &mut c, "p1", "", 0, 200).unwrap();
        delete_entry(&mut s, &a.id).unwrap();
        assert_eq!(s.entries.len(), 1);
        assert_eq!(s.total_ms(), 200);
    }

    // --- split and merge ---

    #[test]
    fn split_produces_two_entries_covering_the_original() {
        let mut s = store_with_project();
        let mut c = ids();
        let e = create_entry(&mut s, &mut c, "p1", "long", 0, D).unwrap();
        let (first, second) = split_entry(&mut s, &mut c, &e.id, D / 2).unwrap();
        assert_eq!(first.duration_ms(), D / 2);
        assert_eq!(second.duration_ms(), D / 2);
        assert_ne!(first.id, second.id, "halves must be distinct entries");
        assert_eq!(s.entries.len(), 2);
        assert_eq!(s.total_ms(), D, "a split must not change the total");
    }

    #[test]
    fn splitting_twice_never_reuses_an_id() {
        // The old `{id}-b` scheme minted the same second-half id on every
        // split of one entry, leaving two entries sharing an id.
        let mut s = store_with_project();
        let mut c = ids();
        let e = create_entry(&mut s, &mut c, "p1", "", 0, D).unwrap();
        split_entry(&mut s, &mut c, &e.id, D / 2).unwrap();
        split_entry(&mut s, &mut c, &e.id, D / 4).unwrap();
        let mut seen = std::collections::HashSet::new();
        for entry in &s.entries {
            assert!(seen.insert(entry.id.clone()), "duplicate id {}", entry.id);
        }
        assert_eq!(s.entries.len(), 3);
        assert_eq!(s.total_ms(), D, "splits must not change the total");
    }

    #[test]
    fn split_outside_the_entry_is_refused() {
        let mut s = store_with_project();
        let mut c = ids();
        let e = create_entry(&mut s, &mut c, "p1", "", 0, D).unwrap();
        assert!(split_entry(&mut s, &mut c, &e.id, 0).is_err());
        assert!(split_entry(&mut s, &mut c, &e.id, D).is_err());
        assert_eq!(s.entries.len(), 1);
    }

    #[test]
    fn merge_of_disjoint_entries_spans_the_union() {
        // The merged interval is the union (REQUIREMENTS §8), so it spans the
        // gap between two disjoint entries and its duration is larger than the
        // sum of the parts. This is the documented consequence of "merge takes
        // the union", and it is why merge is offered for *touching or
        // overlapping* entries rather than arbitrary ones.
        let mut s = store_with_project();
        let mut c = ids();
        let a = create_entry(&mut s, &mut c, "p1", "", 0, D / 4).unwrap();
        let b = create_entry(&mut s, &mut c, "p1", "", D / 2, 3 * D / 4).unwrap();
        let merged = merge_entries(&mut s, &[a.id.clone(), b.id.clone()]).unwrap();
        assert_eq!(s.entries.len(), 1);
        assert_eq!(merged.started_at, 0);
        assert_eq!(merged.ended_at, 3 * D / 4);
        assert_eq!(merged.duration_ms(), 3 * D / 4);
    }

    #[test]
    fn merge_of_touching_entries_is_lossless() {
        let mut s = store_with_project();
        let mut c = ids();
        let a = create_entry(&mut s, &mut c, "p1", "", 0, D / 2).unwrap();
        let b = create_entry(&mut s, &mut c, "p1", "", D / 2, D).unwrap();
        let before = s.total_ms();
        merge_entries(&mut s, &[a.id.clone(), b.id.clone()]).unwrap();
        assert_eq!(
            s.total_ms(),
            before,
            "touching entries must merge losslessly"
        );
    }

    #[test]
    fn merge_of_overlapping_entries_shrinks_the_total() {
        // The deliberate exception to sum-of-durations (REQUIREMENTS §8):
        // merging states the overlap was a mistake, so the total drops.
        let mut s = store_with_project();
        let mut c = ids();
        let a = create_entry(&mut s, &mut c, "p1", "", 0, D).unwrap();
        let b = create_entry(&mut s, &mut c, "p1", "", D / 2, D + D / 2).unwrap();
        let merged = merge_entries(&mut s, &[a.id.clone(), b.id.clone()]).unwrap();
        assert_eq!(s.entries.len(), 1);
        assert_eq!(merged.duration_ms(), D + D / 2);
        assert!(s.total_ms() < 2 * D, "the overlap must collapse");
    }

    #[test]
    fn merge_keeps_the_first_id_valid() {
        let mut s = store_with_project();
        let mut c = ids();
        let a = create_entry(&mut s, &mut c, "p1", "", 0, 100).unwrap();
        let b = create_entry(&mut s, &mut c, "p1", "", 200, 300).unwrap();
        let merged = merge_entries(&mut s, &[a.id.clone(), b.id.clone()]).unwrap();
        assert_eq!(merged.id, a.id, "a held handle must stay valid");
        assert!(s.get(&a.id).is_some());
    }

    #[test]
    fn merge_needs_two_entries() {
        let mut s = store_with_project();
        let mut c = ids();
        let a = create_entry(&mut s, &mut c, "p1", "", 0, 100).unwrap();
        assert!(merge_entries(&mut s, &[a.id]).is_err());
    }

    #[test]
    fn merge_preview_matches_the_merge_it_describes() {
        // The confirmation number must equal what the merge will actually
        // drop: check the preview against real merges, not just arithmetic.
        let mut s = store_with_project();
        let mut c = ids();
        let a = create_entry(&mut s, &mut c, "p1", "", 0, D).unwrap();
        let b = create_entry(&mut s, &mut c, "p1", "", D / 2, D + D / 2).unwrap();
        let before = s.total_ms();
        let preview = merge_shrink_ms(&[(0, D), (D / 2, D + D / 2)]);
        merge_entries(&mut s, &[a.id, b.id]).unwrap();
        assert_eq!(preview, before - s.total_ms());
    }

    #[test]
    fn merge_shrink_is_positive_for_overlap_zero_for_touching() {
        assert_eq!(merge_shrink_ms(&[(0, 100), (50, 150)]), 50);
        assert_eq!(merge_shrink_ms(&[(0, 100), (100, 200)]), 0);
        assert_eq!(merge_shrink_ms(&[]), 0);
    }

    #[test]
    fn merge_shrink_is_negative_across_a_gap() {
        // Disjoint entries span the gap between them, so the merged entry
        // is longer than the sum of its parts.
        assert_eq!(merge_shrink_ms(&[(0, 100), (200, 300)]), -100);
    }

    #[test]
    fn merge_delta_says_shrink_grow_or_unchanged() {
        assert_eq!(format_merge_delta(3_600_000), "total shrinks by 01:00:00");
        assert_eq!(format_merge_delta(0), "total unchanged");
        assert_eq!(format_merge_delta(-900_000), "total grows by 00:15:00");
    }

    #[test]
    fn merge_with_itself_is_refused_not_a_panic() {
        // Merging [e1, e1] used to remove the survivor and then panic on the
        // lookup that assumed it was still there.
        let mut s = store_with_project();
        let mut c = ids();
        let a = create_entry(&mut s, &mut c, "p1", "", 0, 100).unwrap();
        let ids = vec![a.id.clone(), a.id.clone()];
        assert!(matches!(
            merge_entries(&mut s, &ids).unwrap_err(),
            RuleError::InvalidMerge(_)
        ));
        assert_eq!(s.entries.len(), 1, "a refused merge must not apply");
    }

    #[test]
    fn merge_across_projects_is_refused() {
        // The merged entry keeps one project id, so merging across projects
        // would silently reattribute time. Refuse with a message that says so.
        let mut s = store_with_project();
        let mut c = Counter::continuing(&s);
        create_project(&mut s, &mut c, "Personal").unwrap();
        let a = create_entry(&mut s, &mut c, "p1", "", 0, 100).unwrap();
        let b = create_entry(&mut s, &mut c, "p2", "", 200, 300).unwrap();
        assert!(matches!(
            merge_entries(&mut s, &[a.id.clone(), b.id.clone()]).unwrap_err(),
            RuleError::InvalidMerge(_)
        ));
        assert_eq!(s.entries.len(), 2, "a refused merge must not apply");
    }

    #[test]
    fn quick_add_outside_the_buckets_is_refused() {
        // The bucket check was a debug_assert, so release builds accepted any
        // duration as an undoable quick-add.
        let mut s = store_with_project();
        let mut c = ids();
        assert_eq!(
            quick_add(&mut s, &mut c, "p1", 7 * 60_000, D).unwrap_err(),
            RuleError::InvalidQuickAdd(7 * 60_000)
        );
        assert!(s.entries.is_empty(), "a refused entry must not be stored");
    }

    #[test]
    fn deleting_a_project_with_entries_names_the_problem() {
        // It used to report UnknownProject for a project that plainly exists.
        let mut s = store_with_project();
        let mut c = Counter::continuing(&s);
        create_project(&mut s, &mut c, "Personal").unwrap();
        create_entry(&mut s, &mut c, "p1", "", 0, 100).unwrap();
        assert_eq!(
            delete_project(&mut s, "p1").unwrap_err(),
            RuleError::ProjectHasEntries("p1".into())
        );
        assert!(s.project("p1").is_some(), "a refused delete must not apply");
    }

    // --- the 24h warning ---

    #[test]
    fn a_day_over_24h_is_flagged_not_rejected() {
        let mut s = store_with_project();
        let mut c = ids();
        // Three overlapping 12h entries, all *starting* on local day 0. They
        // sum to 36h, which is legal data but improbable, so it is flagged.
        create_entry(&mut s, &mut c, "p1", "", 0, 12 * 3_600_000).unwrap();
        create_entry(&mut s, &mut c, "p1", "", 6 * 3_600_000, 18 * 3_600_000).unwrap();
        create_entry(&mut s, &mut c, "p1", "", 12 * 3_600_000, 24 * 3_600_000).unwrap();
        assert_eq!(s.entries.len(), 3, "the data is still stored");
        assert_eq!(
            days_exceeding_24h(&s, &crate::tz::Tz::utc()),
            vec![0],
            "but it is flagged"
        );
    }

    #[test]
    fn a_day_of_exactly_24h_is_not_flagged() {
        // The warning is strictly greater-than, so a full but plausible day
        // does not cry wolf.
        let mut s = store_with_project();
        let mut c = ids();
        create_entry(&mut s, &mut c, "p1", "", 0, 12 * 3_600_000).unwrap();
        create_entry(&mut s, &mut c, "p1", "", 12 * 3_600_000, 24 * 3_600_000).unwrap();
        assert_eq!(s.total_ms(), D);
        assert!(days_exceeding_24h(&s, &crate::tz::Tz::utc()).is_empty());
    }

    #[test]
    fn a_normal_day_is_not_flagged() {
        let mut s = store_with_project();
        let mut c = ids();
        create_entry(&mut s, &mut c, "p1", "", 0, 8 * 3_600_000).unwrap();
        create_entry(&mut s, &mut c, "p1", "", 9 * 3_600_000, 10 * 3_600_000).unwrap();
        assert!(days_exceeding_24h(&s, &crate::tz::Tz::utc()).is_empty());
    }

    #[test]
    fn over_24h_buckets_by_named_zone_per_instant() {
        // Entries straddling local midnight in Rome must flag the Rome day,
        // not the UTC day.
        let rome = crate::tz::Tz::parse("Europe/Rome").unwrap();
        let mut s = store_with_project();
        let mut c = ids();
        // 2026-07-05 22:00 UTC = 2026-07-06 00:00 Rome. Three overlapping 12h
        // entries starting there sum to 36h on one Rome day.
        let base = 1_783_288_800_000;
        create_entry(&mut s, &mut c, "p1", "", base, base + 12 * 3_600_000).unwrap();
        create_entry(
            &mut s,
            &mut c,
            "p1",
            "",
            base + 6 * 3_600_000,
            base + 18 * 3_600_000,
        )
        .unwrap();
        create_entry(
            &mut s,
            &mut c,
            "p1",
            "",
            base + 12 * 3_600_000,
            base + 24 * 3_600_000,
        )
        .unwrap();
        let flagged = days_exceeding_24h(&s, &rome);
        assert_eq!(flagged, vec![rome.day_of(base)]);
    }
}
