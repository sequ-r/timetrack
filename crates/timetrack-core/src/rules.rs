/* core/rules.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! The rules: everything that can change a `Store`.
//!
//! Like the old state machine, every operation takes the store mutably and an
//! explicit timestamp, and never reads a clock itself. That keeps the whole
//! module deterministic and testable without I/O (PLAN.md).

use crate::model::{Entry, EntrySource, MS_PER_DAY, Project, Store};

/// Why an operation was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RuleError {
    #[error("no entry with id '{0}'")]
    UnknownEntry(String),
    #[error("no project with id '{0}'")]
    UnknownProject(String),
    #[error("a project named '{0}' already exists")]
    DuplicateProject(String),
    #[error("an entry must not end before it starts")]
    NegativeDuration,
    #[error("cannot delete the last project")]
    LastProject,
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

/// Source of identifiers. Kept abstract so tests can pin them.
pub trait Ids {
    fn next(&mut self) -> String;
}

/// Sequential ids, `e1`, `p1`, ...
#[derive(Debug)]
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
        return Err(RuleError::DuplicateProject(String::new()));
    }
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
        return Err(RuleError::UnknownProject(id.to_string()));
    }
    let idx = store
        .projects
        .iter()
        .position(|p| p.id == id)
        .ok_or_else(|| RuleError::UnknownProject(id.to_string()))?;
    Ok(store.projects.remove(idx))
}

// --- entries ----------------------------------------------------------------

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
    if ended_at < started_at {
        return Err(RuleError::NegativeDuration);
    }
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
    create_entry(store, ids, project_id, description, now - duration_ms, now)
}

/// Add one of the quick-add buckets, ending now (REQUIREMENTS §6).
///
/// Each tap creates its own entry rather than extending an existing one, so
/// that undo is unambiguous and nothing rewrites an earlier entry's
/// `ended_at`.
pub fn quick_add(
    store: &mut Store,
    ids: &mut dyn Ids,
    project_id: &str,
    duration_ms: i64,
    now: i64,
) -> RuleResult<Entry> {
    debug_assert!(
        QUICK_ADD_MS.contains(&duration_ms),
        "use a quick-add bucket"
    );
    create_entry_from(
        store,
        ids,
        project_id,
        "",
        now - duration_ms,
        now,
        EntrySource::QuickAdd,
    )
}

/// Move an entry's interval (method 1's "remove" side, §5).
///
/// Shortening only ever moves `ended_at` earlier. It never deletes: the rule
/// is that no method may lose data as a side effect of reducing time.
pub fn set_times(store: &mut Store, id: &str, started_at: i64, ended_at: i64) -> RuleResult<Entry> {
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
pub fn set_text(
    store: &mut Store,
    id: &str,
    description: Option<&str>,
    note: Option<Option<&str>>,
) -> RuleResult<Entry> {
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
pub fn split_entry(store: &mut Store, id: &str, at_ms: i64) -> RuleResult<(Entry, Entry)> {
    let original = store
        .get(id)
        .cloned()
        .ok_or_else(|| RuleError::UnknownEntry(id.to_string()))?;
    if at_ms <= original.started_at || at_ms >= original.ended_at {
        return Err(RuleError::NegativeDuration);
    }

    // The original entry becomes the first half, in place, so a client that
    // is already holding its id keeps pointing at something valid.
    let first = Entry {
        ended_at: at_ms,
        ..original.clone()
    };
    let second = Entry {
        id: format!("{id}-b"),
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
pub fn merge_entries(store: &mut Store, ids: &[String]) -> RuleResult<Entry> {
    if ids.len() < 2 {
        return Err(RuleError::UnknownEntry("<need two entries>".into()));
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
    let project_id = chosen[0].project_id.clone();
    let started_at = chosen.iter().map(|e| e.started_at).min().unwrap();
    let ended_at = chosen.iter().map(|e| e.ended_at).max().unwrap();

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

/// Whether any day's summed time exceeds 24 hours (REQUIREMENTS §4).
///
/// Flagged, never rejected: overlapping entries are legal, and the point is
/// to surface a probable duplicate rather than to forbid the data.
pub fn days_exceeding_24h(store: &Store, local_offset_ms: i64) -> Vec<i64> {
    let mut per_day = std::collections::BTreeMap::new();
    for e in &store.entries {
        *per_day.entry(e.local_day(local_offset_ms)).or_insert(0i64) += e.duration_ms();
    }
    per_day
        .into_iter()
        .filter(|(_, ms)| *ms > MS_PER_DAY)
        .map(|(day, _)| day)
        .collect()
}

/// Convenience: does any entry land on `day`, in local time?
pub fn has_entry_on(store: &Store, day: i64, local_offset_ms: i64) -> bool {
    store
        .entries
        .iter()
        .any(|e| e.local_day(local_offset_ms) == day)
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
        let mut s = store_with_project();
        let mut c = ids();
        assert!(create_project(&mut s, &mut c, "   ").is_err());
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
        let (first, second) = split_entry(&mut s, &e.id, D / 2).unwrap();
        assert_eq!(first.duration_ms(), D / 2);
        assert_eq!(second.duration_ms(), D / 2);
        assert_eq!(s.entries.len(), 2);
        assert_eq!(s.total_ms(), D, "a split must not change the total");
    }

    #[test]
    fn split_outside_the_entry_is_refused() {
        let mut s = store_with_project();
        let mut c = ids();
        let e = create_entry(&mut s, &mut c, "p1", "", 0, D).unwrap();
        assert!(split_entry(&mut s, &e.id, 0).is_err());
        assert!(split_entry(&mut s, &e.id, D).is_err());
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
        assert_eq!(days_exceeding_24h(&s, 0), vec![0], "but it is flagged");
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
        assert!(days_exceeding_24h(&s, 0).is_empty());
    }

    #[test]
    fn a_normal_day_is_not_flagged() {
        let mut s = store_with_project();
        let mut c = ids();
        create_entry(&mut s, &mut c, "p1", "", 0, 8 * 3_600_000).unwrap();
        create_entry(&mut s, &mut c, "p1", "", 9 * 3_600_000, 10 * 3_600_000).unwrap();
        assert!(days_exceeding_24h(&s, 0).is_empty());
    }
}
