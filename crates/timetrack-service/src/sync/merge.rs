/* service/sync/merge.rs
 *
 * Copyright 2026 sequ
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! Idempotent last-writer-wins snapshot merge and conflict journaling (ADR 0020, #173).
//!
//! Pure function over local rows, local config/watermarks, known peer records,
//! and decrypted peer envelopes. Emits a single transactional [`SyncMergeWrite`]
//! plan so the caller can commit all winning state, tombstones, conflicts, peer
//! state, and watermarks atomically.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};
use timetrack_core::{Entry, EntrySource, Project};
use timetrack_store::{
    EntryRow, ExternalSource, ExternalSourceRow, ProjectRow, RestoredRecord, SyncConfig,
    SyncConflictRow, SyncMergeWrite, SyncPeerRow, SyncRows, Tombstone,
};

use super::envelope::{
    ENVELOPE_FORMAT_VERSION, Envelope, SyncedEntry, SyncedProject, SyncedSource, SyncedTombstone,
};

/// Stale peer warning threshold shown in status views (7 days).
pub const STALE_PEER_WARNING_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// Serialized losing-record payload preserved in the local `sync_conflicts` journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum ConflictPayload {
    #[serde(rename = "project")]
    Project {
        id: String,
        name: String,
        colour: Option<u32>,
        archived: bool,
    },
    #[serde(rename = "entry")]
    Entry {
        id: String,
        project_id: String,
        description: String,
        started_at: i64,
        ended_at: i64,
        source: EntrySource,
        note: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        external_source_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        external_uid: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        external_started_at: Option<i64>,
    },
    #[serde(rename = "source")]
    Source {
        id: String,
        source_kind: String,
        locator: String,
        project_id: Option<String>,
        label: String,
    },
}

impl ConflictPayload {
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    pub fn from_json(raw: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(raw)
    }

    pub fn to_restored_record(&self) -> RestoredRecord {
        match self {
            Self::Project {
                id,
                name,
                colour,
                archived,
            } => RestoredRecord::Project(Project {
                id: id.clone(),
                name: name.clone(),
                colour: *colour,
                archived: *archived,
            }),
            Self::Entry {
                id,
                project_id,
                description,
                started_at,
                ended_at,
                source,
                note,
                external_source_id,
                external_uid,
                external_started_at,
            } => RestoredRecord::Entry {
                entry: Entry {
                    id: id.clone(),
                    project_id: project_id.clone(),
                    description: description.clone(),
                    started_at: *started_at,
                    ended_at: *ended_at,
                    source: *source,
                    note: note.clone(),
                },
                external_source_id: external_source_id.clone(),
                external_uid: external_uid.clone(),
                external_started_at: *external_started_at,
            },
            Self::Source {
                id,
                source_kind,
                locator,
                project_id,
                label,
            } => RestoredRecord::Source {
                id: id.clone(),
                kind: source_kind.clone(),
                locator: locator.clone(),
                project_id: project_id.clone(),
                label: label.clone(),
            },
        }
    }

    pub fn summary(&self) -> String {
        match self {
            Self::Project { name, archived, .. } => {
                let name = timetrack_core::sanitize_display(name);
                if *archived {
                    format!("project \"{name}\" (archived)")
                } else {
                    format!("project \"{name}\"")
                }
            }
            Self::Entry {
                description,
                started_at,
                ended_at,
                ..
            } => {
                let dur = timetrack_core::format_compact((*ended_at - *started_at).max(0));
                let desc = timetrack_core::sanitize_display(description);
                let trimmed = desc.trim();
                if trimmed.is_empty() {
                    format!("entry ({dur})")
                } else {
                    format!("entry \"{trimmed}\" ({dur})")
                }
            }
            Self::Source {
                source_kind,
                locator,
                label,
                ..
            } => {
                let kind = timetrack_core::sanitize_display(source_kind);
                let locator = timetrack_core::sanitize_display(locator);
                let label = timetrack_core::sanitize_display(label);
                if label.trim().is_empty() {
                    format!("external source {kind}: {locator}")
                } else {
                    format!("external source {kind}: {label} ({locator})")
                }
            }
        }
    }
}

pub fn summarize_conflict_payload(raw: &str) -> String {
    ConflictPayload::from_json(raw)
        .map(|p| p.summary())
        .unwrap_or_else(|_| "unknown conflict record".to_string())
}

/// Output of one deterministic snapshot merge pass.
#[derive(Debug, Default)]
pub struct MergeOutcome {
    pub write: SyncMergeWrite,
    pub peers_read: usize,
    pub applied_changes: usize,
    pub new_conflicts: usize,
    pub compacted_tombstones: usize,
    pub warnings: Vec<String>,
}

#[derive(Clone, Copy)]
struct VersionKey<'a> {
    ts: i64,
    dev: &'a str,
}

impl<'a> VersionKey<'a> {
    fn cmp_key(&self, other: &Self) -> Ordering {
        self.ts.cmp(&other.ts).then_with(|| self.dev.cmp(other.dev))
    }
}

#[derive(Clone)]
struct LiveCandidate<T> {
    value: T,
    updated_at: i64,
    device_id: String,
    /// `None` when originating from local SQLite; `Some(env_dev, env_merged_through)`
    /// when read from a peer envelope.
    from_peer: Option<(String, i64)>,
}

#[derive(Clone)]
struct TombCandidate {
    tombstone: Tombstone,
    from_peer_merged_through: Option<i64>,
}

fn pick_winning_live<T>(candidates: &[LiveCandidate<T>]) -> Option<usize> {
    let mut best: Option<usize> = None;
    for (idx, cand) in candidates.iter().enumerate() {
        let replace = match best {
            None => true,
            Some(b) => {
                let cur = VersionKey {
                    ts: cand.updated_at,
                    dev: &cand.device_id,
                };
                let prev = VersionKey {
                    ts: candidates[b].updated_at,
                    dev: &candidates[b].device_id,
                };
                cur.cmp_key(&prev) == Ordering::Greater
            }
        };
        if replace {
            best = Some(idx);
        }
    }
    best
}

fn pick_winning_tomb(candidates: &[TombCandidate]) -> Option<usize> {
    let mut best: Option<usize> = None;
    for (idx, cand) in candidates.iter().enumerate() {
        let replace = match best {
            None => true,
            Some(b) => {
                let cur = VersionKey {
                    ts: cand.tombstone.deleted_at,
                    dev: &cand.tombstone.device_id,
                };
                let prev = VersionKey {
                    ts: candidates[b].tombstone.deleted_at,
                    dev: &candidates[b].tombstone.device_id,
                };
                cur.cmp_key(&prev) == Ordering::Greater
            }
        };
        if replace {
            best = Some(idx);
        }
    }
    best
}

fn peer_prev_seen(prev_peers: &[SyncPeerRow], dev_id: &str) -> i64 {
    prev_peers
        .iter()
        .find(|p| p.device_id == dev_id)
        .map(|p| p.last_published_at)
        .unwrap_or(0)
}

#[allow(clippy::too_many_arguments)]
fn make_conflict_row(
    kind: &str,
    record_id: &str,
    recorded_at: i64,
    loser_updated_at: i64,
    loser_device_id: &str,
    winner_updated_at: i64,
    winner_device_id: &str,
    payload: &ConflictPayload,
) -> Option<SyncConflictRow> {
    let loser_payload = payload.to_json().ok()?;
    Some(SyncConflictRow {
        id: uuid::Uuid::new_v4().to_string(),
        kind: kind.to_string(),
        record_id: record_id.to_string(),
        recorded_at,
        loser_updated_at,
        loser_device_id: loser_device_id.to_string(),
        winner_updated_at,
        winner_device_id: winner_device_id.to_string(),
        loser_payload,
        restored_at: None,
    })
}

trait SyncRecord: Clone {
    fn version(&self) -> (i64, &str);
    fn content_eq(&self, other: &Self) -> bool;
    fn conflict_payload(&self) -> ConflictPayload;
}

impl SyncRecord for ProjectRow {
    fn version(&self) -> (i64, &str) {
        (self.updated_at, self.device_id.as_str())
    }
    fn content_eq(&self, other: &Self) -> bool {
        self.project.name == other.project.name
            && self.project.colour == other.project.colour
            && self.project.archived == other.project.archived
    }
    fn conflict_payload(&self) -> ConflictPayload {
        ConflictPayload::Project {
            id: self.project.id.clone(),
            name: self.project.name.clone(),
            colour: self.project.colour,
            archived: self.project.archived,
        }
    }
}

impl SyncRecord for EntryRow {
    fn version(&self) -> (i64, &str) {
        (self.updated_at, self.device_id.as_str())
    }
    fn content_eq(&self, other: &Self) -> bool {
        self.entry.project_id == other.entry.project_id
            && self.entry.description == other.entry.description
            && self.entry.started_at == other.entry.started_at
            && self.entry.ended_at == other.entry.ended_at
            && self.entry.source == other.entry.source
            && self.entry.note == other.entry.note
            && self.external_source_id == other.external_source_id
            && self.external_uid == other.external_uid
            && self.external_started_at == other.external_started_at
    }
    fn conflict_payload(&self) -> ConflictPayload {
        ConflictPayload::Entry {
            id: self.entry.id.clone(),
            project_id: self.entry.project_id.clone(),
            description: self.entry.description.clone(),
            started_at: self.entry.started_at,
            ended_at: self.entry.ended_at,
            source: self.entry.source,
            note: self.entry.note.clone(),
            external_source_id: self.external_source_id.clone(),
            external_uid: self.external_uid.clone(),
            external_started_at: self.external_started_at,
        }
    }
}

impl SyncRecord for ExternalSourceRow {
    fn version(&self) -> (i64, &str) {
        (self.updated_at, self.device_id.as_str())
    }
    fn content_eq(&self, other: &Self) -> bool {
        self.source.kind == other.source.kind
            && self.source.locator == other.source.locator
            && self.source.project_id == other.source.project_id
            && self.source.label == other.source.label
    }
    fn conflict_payload(&self) -> ConflictPayload {
        ConflictPayload::Source {
            id: self.source.id.clone(),
            source_kind: self.source.kind.clone(),
            locator: self.source.locator.clone(),
            project_id: self.source.project_id.clone(),
            label: self.source.label.clone(),
        }
    }
}

struct FamilyContext<'a> {
    kind: &'a str,
    prev_peers: &'a [SyncPeerRow],
    now_ms: i64,
}

struct FamilyResolution<T> {
    winners: BTreeMap<String, T>,
    deletes: Vec<String>,
    upsert_tombs: Vec<Tombstone>,
    delete_tombs: Vec<(String, String)>,
    conflicts: Vec<SyncConflictRow>,
}

fn resolve_family<T: SyncRecord>(
    ctx: &FamilyContext<'_>,
    local_map: &HashMap<String, T>,
    all_candidates: BTreeMap<String, Vec<LiveCandidate<T>>>,
    all_tombstones: BTreeMap<String, Vec<TombCandidate>>,
    local_tombstones: &HashMap<String, Tombstone>,
) -> FamilyResolution<T> {
    let kind = ctx.kind;
    let prev_peers = ctx.prev_peers;
    let now_ms = ctx.now_ms;
    let mut winners: BTreeMap<String, T> = BTreeMap::new();
    let mut deletes: Vec<String> = Vec::new();
    let mut upsert_tombs: Vec<Tombstone> = Vec::new();
    let mut delete_tombs: Vec<(String, String)> = Vec::new();
    let mut conflicts: Vec<SyncConflictRow> = Vec::new();

    let mut all_ids: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    all_ids.extend(all_candidates.keys().cloned());
    all_ids.extend(all_tombstones.keys().cloned());

    for id in all_ids {
        let cands = all_candidates.get(&id).map(Vec::as_slice).unwrap_or(&[]);
        let tombs = all_tombstones.get(&id).map(Vec::as_slice).unwrap_or(&[]);
        let best_live_idx = pick_winning_live(cands);
        let best_tomb_idx = pick_winning_tomb(tombs);

        let tomb_wins = match (best_tomb_idx, best_live_idx) {
            (Some(_), None) => true,
            (Some(ti), Some(li)) => {
                let t = &tombs[ti].tombstone;
                let l = &cands[li];
                VersionKey {
                    ts: t.deleted_at,
                    dev: &t.device_id,
                }
                .cmp_key(&VersionKey {
                    ts: l.updated_at,
                    dev: &l.device_id,
                }) != Ordering::Less
            }
            (None, _) => false,
        };

        if tomb_wins {
            let winning_tomb = &tombs[best_tomb_idx.unwrap()];
            let t = &winning_tomb.tombstone;
            let same_local_tomb = local_tombstones
                .get(&id)
                .is_some_and(|lt| lt.deleted_at == t.deleted_at && lt.device_id == t.device_id);
            if !same_local_tomb {
                upsert_tombs.push(t.clone());
            }
            if let Some(local_rec) = local_map.get(&id) {
                deletes.push(id.clone());
                let (l_ts, l_dev) = local_rec.version();
                if let Some(peer_mt) = winning_tomb.from_peer_merged_through
                    && l_dev != t.device_id
                    && (peer_mt < l_ts || t.deleted_at == l_ts)
                    && let Some(row) = make_conflict_row(
                        kind,
                        &id,
                        now_ms,
                        l_ts,
                        l_dev,
                        t.deleted_at,
                        &t.device_id,
                        &local_rec.conflict_payload(),
                    )
                {
                    conflicts.push(row);
                }
            }
            // Case B: incoming peer live record beaten by a winning tombstone.
            for cand in cands {
                let Some((ref env_dev, env_mt)) = cand.from_peer else {
                    continue;
                };
                if cand.device_id == t.device_id {
                    continue;
                }
                let seen_from_author = prev_peers
                    .iter()
                    .find(|p| p.device_id == cand.device_id)
                    .map(|p| p.last_published_at)
                    .unwrap_or_else(|| peer_prev_seen(prev_peers, env_dev));
                if cand.updated_at > seen_from_author
                    && (env_mt < t.deleted_at || cand.updated_at == t.deleted_at)
                    && let Some(row) = make_conflict_row(
                        kind,
                        &id,
                        now_ms,
                        cand.updated_at,
                        &cand.device_id,
                        t.deleted_at,
                        &t.device_id,
                        &cand.value.conflict_payload(),
                    )
                {
                    conflicts.push(row);
                }
            }
        } else if let Some(li) = best_live_idx {
            let winner = &cands[li];
            if local_tombstones.contains_key(&id) {
                delete_tombs.push((id.clone(), kind.to_string()));
            }
            winners.insert(id.clone(), winner.value.clone());

            // Conflict checks against every distinct losing live candidate.
            if let Some(local_rec) = local_map.get(&id) {
                let (l_ts, l_dev) = local_rec.version();
                if let Some((_, peer_mt)) = winner.from_peer
                    && (l_ts, l_dev) != (winner.updated_at, winner.device_id.as_str())
                    && l_dev != winner.device_id
                    && !local_rec.content_eq(&winner.value)
                    && (peer_mt < l_ts || winner.updated_at == l_ts)
                    && let Some(row) = make_conflict_row(
                        kind,
                        &id,
                        now_ms,
                        l_ts,
                        l_dev,
                        winner.updated_at,
                        &winner.device_id,
                        &local_rec.conflict_payload(),
                    )
                {
                    conflicts.push(row);
                }
            }
            for cand in cands {
                let Some((ref env_dev, env_mt)) = cand.from_peer else {
                    continue;
                };
                if (cand.updated_at, cand.device_id.as_str())
                    == (winner.updated_at, winner.device_id.as_str())
                {
                    continue;
                }
                if cand.device_id == winner.device_id || cand.value.content_eq(&winner.value) {
                    continue;
                }
                let seen_from_author = prev_peers
                    .iter()
                    .find(|p| p.device_id == cand.device_id)
                    .map(|p| p.last_published_at)
                    .unwrap_or_else(|| peer_prev_seen(prev_peers, env_dev));
                if cand.updated_at > seen_from_author
                    && (env_mt < winner.updated_at || cand.updated_at == winner.updated_at)
                    && let Some(row) = make_conflict_row(
                        kind,
                        &id,
                        now_ms,
                        cand.updated_at,
                        &cand.device_id,
                        winner.updated_at,
                        &winner.device_id,
                        &cand.value.conflict_payload(),
                    )
                {
                    conflicts.push(row);
                }
            }
        }
    }

    FamilyResolution {
        winners,
        deletes,
        upsert_tombs,
        delete_tombs,
        conflicts,
    }
}

/// Merge `local` with decrypted `peer_envelopes` into a single transactional write plan.
pub fn merge_snapshots(
    local_device_id: &str,
    local: &SyncRows,
    local_config: &SyncConfig,
    prev_peers: &[SyncPeerRow],
    peer_envelopes: &[Envelope],
    peer_warnings: &[String],
    now_ms: i64,
) -> MergeOutcome {
    let mut outcome = MergeOutcome {
        peers_read: peer_envelopes.len(),
        warnings: peer_warnings.to_vec(),
        ..Default::default()
    };

    // Index local records and local tombstones by kind.
    let local_projects: HashMap<String, ProjectRow> = local
        .projects
        .iter()
        .cloned()
        .map(|p| (p.project.id.clone(), p))
        .collect();
    let local_entries: HashMap<String, EntryRow> = local
        .entries
        .iter()
        .cloned()
        .map(|e| (e.entry.id.clone(), e))
        .collect();
    let local_sources: HashMap<String, ExternalSourceRow> = local
        .sources
        .iter()
        .cloned()
        .map(|s| (s.source.id.clone(), s))
        .collect();

    let mut local_proj_tombs: HashMap<String, Tombstone> = HashMap::new();
    let mut local_entry_tombs: HashMap<String, Tombstone> = HashMap::new();
    let mut local_source_tombs: HashMap<String, Tombstone> = HashMap::new();
    for t in &local.tombstones {
        match t.kind.as_str() {
            "project" => {
                local_proj_tombs.insert(t.id.clone(), t.clone());
            }
            "entry" => {
                local_entry_tombs.insert(t.id.clone(), t.clone());
            }
            "source" => {
                local_source_tombs.insert(t.id.clone(), t.clone());
            }
            _ => {}
        }
    }

    // Gather all live and tombstone candidates from local + peer envelopes.
    let mut proj_cands: BTreeMap<String, Vec<LiveCandidate<ProjectRow>>> = BTreeMap::new();
    let mut entry_cands: BTreeMap<String, Vec<LiveCandidate<EntryRow>>> = BTreeMap::new();
    let mut source_cands: BTreeMap<String, Vec<LiveCandidate<ExternalSourceRow>>> = BTreeMap::new();
    let mut proj_tombs: BTreeMap<String, Vec<TombCandidate>> = BTreeMap::new();
    let mut entry_tombs: BTreeMap<String, Vec<TombCandidate>> = BTreeMap::new();
    let mut source_tombs: BTreeMap<String, Vec<TombCandidate>> = BTreeMap::new();

    for p in &local.projects {
        proj_cands
            .entry(p.project.id.clone())
            .or_default()
            .push(LiveCandidate {
                value: p.clone(),
                updated_at: p.updated_at,
                device_id: p.device_id.clone(),
                from_peer: None,
            });
    }
    for e in &local.entries {
        entry_cands
            .entry(e.entry.id.clone())
            .or_default()
            .push(LiveCandidate {
                value: e.clone(),
                updated_at: e.updated_at,
                device_id: e.device_id.clone(),
                from_peer: None,
            });
    }
    for s in &local.sources {
        source_cands
            .entry(s.source.id.clone())
            .or_default()
            .push(LiveCandidate {
                value: s.clone(),
                updated_at: s.updated_at,
                device_id: s.device_id.clone(),
                from_peer: None,
            });
    }
    for t in &local.tombstones {
        let cand = TombCandidate {
            tombstone: t.clone(),
            from_peer_merged_through: None,
        };
        match t.kind.as_str() {
            "project" => proj_tombs.entry(t.id.clone()).or_default().push(cand),
            "entry" => entry_tombs.entry(t.id.clone()).or_default().push(cand),
            "source" => source_tombs.entry(t.id.clone()).or_default().push(cand),
            _ => {}
        }
    }

    for env in peer_envelopes {
        for p in &env.projects {
            proj_cands
                .entry(p.id.clone())
                .or_default()
                .push(LiveCandidate {
                    value: ProjectRow {
                        project: Project {
                            id: p.id.clone(),
                            name: p.name.clone(),
                            colour: p.colour,
                            archived: p.archived,
                        },
                        created_at: p.created_at,
                        updated_at: p.updated_at,
                        device_id: p.device_id.clone(),
                    },
                    updated_at: p.updated_at,
                    device_id: p.device_id.clone(),
                    from_peer: Some((env.device_id.clone(), env.merged_through)),
                });
        }
        for e in &env.entries {
            entry_cands
                .entry(e.id.clone())
                .or_default()
                .push(LiveCandidate {
                    value: EntryRow {
                        entry: Entry {
                            id: e.id.clone(),
                            project_id: e.project_id.clone(),
                            description: e.description.clone(),
                            started_at: e.started_at,
                            ended_at: e.ended_at,
                            source: e.source,
                            note: e.note.clone(),
                        },
                        external_source_id: e.external_source_id.clone(),
                        external_uid: e.external_uid.clone(),
                        external_started_at: e.external_started_at,
                        created_at: e.created_at,
                        updated_at: e.updated_at,
                        device_id: e.device_id.clone(),
                    },
                    updated_at: e.updated_at,
                    device_id: e.device_id.clone(),
                    from_peer: Some((env.device_id.clone(), env.merged_through)),
                });
        }
        for s in &env.sources {
            source_cands
                .entry(s.id.clone())
                .or_default()
                .push(LiveCandidate {
                    value: ExternalSourceRow {
                        source: ExternalSource {
                            id: s.id.clone(),
                            kind: s.kind.clone(),
                            locator: s.locator.clone(),
                            project_id: s.project_id.clone(),
                            label: s.label.clone(),
                            cred_ref: None,
                            cred_username: None,
                        },
                        created_at: s.created_at,
                        updated_at: s.updated_at,
                        device_id: s.device_id.clone(),
                    },
                    updated_at: s.updated_at,
                    device_id: s.device_id.clone(),
                    from_peer: Some((env.device_id.clone(), env.merged_through)),
                });
        }
        for t in &env.tombstones {
            let cand = TombCandidate {
                tombstone: Tombstone {
                    id: t.id.clone(),
                    kind: t.kind.clone(),
                    deleted_at: t.deleted_at,
                    device_id: t.device_id.clone(),
                },
                from_peer_merged_through: Some(env.merged_through),
            };
            match t.kind.as_str() {
                "project" => proj_tombs.entry(t.id.clone()).or_default().push(cand),
                "entry" => entry_tombs.entry(t.id.clone()).or_default().push(cand),
                "source" => source_tombs.entry(t.id.clone()).or_default().push(cand),
                _ => {}
            }
        }
    }

    let FamilyResolution {
        winners: proj_winners,
        deletes: proj_del,
        upsert_tombs: proj_utomb,
        delete_tombs: proj_dtomb,
        conflicts: proj_conf,
    } = resolve_family(
        &FamilyContext {
            kind: "project",
            prev_peers,
            now_ms,
        },
        &local_projects,
        proj_cands,
        proj_tombs,
        &local_proj_tombs,
    );
    let FamilyResolution {
        winners: entry_winners,
        deletes: entry_del,
        upsert_tombs: entry_utomb,
        delete_tombs: entry_dtomb,
        conflicts: entry_conf,
    } = resolve_family(
        &FamilyContext {
            kind: "entry",
            prev_peers,
            now_ms,
        },
        &local_entries,
        entry_cands,
        entry_tombs,
        &local_entry_tombs,
    );
    let FamilyResolution {
        winners: mut source_winners,
        deletes: mut source_del,
        upsert_tombs: mut source_utomb,
        delete_tombs: source_dtomb,
        conflicts: mut source_conf,
    } = resolve_family(
        &FamilyContext {
            kind: "source",
            prev_peers,
            now_ms,
        },
        &local_sources,
        source_cands,
        source_tombs,
        &local_source_tombs,
    );

    // Resolve any offline duplicate-source `(kind, locator)` collisions
    // deterministically so SQLite's `UNIQUE(kind, locator)` constraint never fails.
    let mut by_locator: BTreeMap<(String, String), Vec<ExternalSourceRow>> = BTreeMap::new();
    for src in source_winners.values() {
        by_locator
            .entry((src.source.kind.clone(), src.source.locator.clone()))
            .or_default()
            .push(src.clone());
    }
    for (_, mut group) in by_locator {
        if group.len() <= 1 {
            continue;
        }
        group.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| b.device_id.cmp(&a.device_id))
                .then_with(|| b.source.id.cmp(&a.source.id))
        });
        let winner = group.remove(0);
        for loser in group {
            source_winners.remove(&loser.source.id);
            source_utomb.push(Tombstone {
                id: loser.source.id.clone(),
                kind: "source".to_string(),
                deleted_at: winner.updated_at,
                device_id: winner.device_id.clone(),
            });
            if local_sources.contains_key(&loser.source.id) {
                source_del.push(loser.source.id.clone());
                if loser.device_id != winner.device_id
                    && !loser.content_eq(&winner)
                    && let Some(row) = make_conflict_row(
                        "source",
                        &loser.source.id,
                        now_ms,
                        loser.updated_at,
                        &loser.device_id,
                        winner.updated_at,
                        &winner.device_id,
                        &loser.conflict_payload(),
                    )
                {
                    source_conf.push(row);
                }
            }
        }
    }

    // Only emit SQLite upserts for records that differ from what local already stores.
    for (id, w) in proj_winners {
        let same = local_projects.get(&id).is_some_and(|l| {
            l.updated_at == w.updated_at && l.device_id == w.device_id && l.content_eq(&w)
        });
        if !same {
            outcome.write.upsert_projects.push(w);
        }
    }
    for (id, w) in entry_winners {
        let same = local_entries.get(&id).is_some_and(|l| {
            l.updated_at == w.updated_at && l.device_id == w.device_id && l.content_eq(&w)
        });
        if !same {
            outcome.write.upsert_entries.push(w);
        }
    }
    for (id, w) in source_winners {
        let same = local_sources.get(&id).is_some_and(|l| {
            l.updated_at == w.updated_at && l.device_id == w.device_id && l.content_eq(&w)
        });
        if !same {
            outcome.write.upsert_sources.push(w);
        }
    }

    outcome.write.delete_projects = proj_del;
    outcome.write.delete_entries = entry_del;
    outcome.write.delete_sources = source_del;
    outcome.write.upsert_tombstones.extend(proj_utomb);
    outcome.write.upsert_tombstones.extend(entry_utomb);
    outcome.write.upsert_tombstones.extend(source_utomb);
    outcome.write.delete_tombstones.extend(proj_dtomb);
    outcome.write.delete_tombstones.extend(entry_dtomb);
    outcome.write.delete_tombstones.extend(source_dtomb);
    outcome.write.new_conflicts.extend(proj_conf);
    outcome.write.new_conflicts.extend(entry_conf);
    outcome.write.new_conflicts.extend(source_conf);

    // Deduplicate new_conflicts on (kind, record_id, loser_updated_at, loser_device_id).
    let mut seen_conf = std::collections::HashSet::new();
    outcome.write.new_conflicts.retain(|c| {
        seen_conf.insert((
            c.kind.clone(),
            c.record_id.clone(),
            c.loser_updated_at,
            c.loser_device_id.clone(),
        ))
    });

    outcome.applied_changes = outcome.write.upsert_projects.len()
        + outcome.write.delete_projects.len()
        + outcome.write.upsert_entries.len()
        + outcome.write.delete_entries.len()
        + outcome.write.upsert_sources.len()
        + outcome.write.delete_sources.len();
    outcome.new_conflicts = outcome.write.new_conflicts.len();

    // Update peer tracking and `merged_through` watermark (only when every peer file decoded cleanly).
    let mut seen_peer_ids = std::collections::HashSet::new();
    let mut max_peer_pub = 0i64;
    for env in peer_envelopes {
        if env.device_id == local_device_id {
            continue;
        }
        seen_peer_ids.insert(env.device_id.as_str());
        max_peer_pub = max_peer_pub.max(env.published_at);
        outcome.write.upsert_peers.push(SyncPeerRow {
            device_id: env.device_id.clone(),
            last_published_at: env.published_at,
            last_merged_through: env.merged_through,
            last_seen_at: now_ms,
            quarantined: false,
            last_warning: None,
        });
    }
    if peer_warnings.is_empty() {
        for prev in prev_peers {
            if !seen_peer_ids.contains(prev.device_id.as_str()) {
                outcome.write.delete_peers.push(prev.device_id.clone());
            }
        }
        let next_mt = local_config.merged_through.max(max_peer_pub);
        if next_mt != local_config.merged_through {
            outcome.write.merged_through = Some(next_mt);
        }
        outcome.write.last_error = Some(None);
    } else {
        outcome.write.last_error = Some(Some(peer_warnings.join("; ")));
    }
    outcome.write.last_sync_at = Some(now_ms);

    outcome
}

/// Build this device's full-state encrypted snapshot envelope from SQLite rows.
///
/// Local OS credential references (`cred_ref`, `cred_username`) are omitted by
/// construction so credentials never leave this device.
pub fn build_envelope(
    device_id: &str,
    rows: &SyncRows,
    config: &SyncConfig,
    published_at: i64,
) -> Envelope {
    Envelope {
        format_version: ENVELOPE_FORMAT_VERSION,
        device_id: device_id.to_string(),
        published_at,
        merged_through: config.merged_through,
        compacted_through: config.compacted_through,
        projects: rows
            .projects
            .iter()
            .map(|p| SyncedProject {
                id: p.project.id.clone(),
                name: p.project.name.clone(),
                colour: p.project.colour,
                archived: p.project.archived,
                created_at: p.created_at,
                updated_at: p.updated_at,
                device_id: p.device_id.clone(),
            })
            .collect(),
        entries: rows
            .entries
            .iter()
            .map(|e| SyncedEntry {
                id: e.entry.id.clone(),
                project_id: e.entry.project_id.clone(),
                description: e.entry.description.clone(),
                started_at: e.entry.started_at,
                ended_at: e.entry.ended_at,
                source: e.entry.source,
                note: e.entry.note.clone(),
                external_source_id: e.external_source_id.clone(),
                external_uid: e.external_uid.clone(),
                external_started_at: e.external_started_at,
                created_at: e.created_at,
                updated_at: e.updated_at,
                device_id: e.device_id.clone(),
            })
            .collect(),
        sources: rows
            .sources
            .iter()
            .map(|s| SyncedSource {
                id: s.source.id.clone(),
                kind: s.source.kind.clone(),
                locator: s.source.locator.clone(),
                project_id: s.source.project_id.clone(),
                label: s.source.label.clone(),
                created_at: s.created_at,
                updated_at: s.updated_at,
                device_id: s.device_id.clone(),
            })
            .collect(),
        tombstones: rows
            .tombstones
            .iter()
            .map(|t| SyncedTombstone {
                id: t.id.clone(),
                kind: t.kind.clone(),
                deleted_at: t.deleted_at,
                device_id: t.device_id.clone(),
            })
            .collect(),
    }
}
