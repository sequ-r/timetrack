# Embedded SQLite store, auto-migrating the JSON history

Status: accepted; supersedes the storage half of [0010](0010-journal-and-changed-signal.md) (its `Changed` push stays, over the socket transport of [0013](0013-multiplatform-local-socket-ipc.md)).

The service's store moves from the JSON checkpoint-plus-journal to embedded SQLite (REQUIREMENTS §17): bundled so no system library can version-skew, WAL mode, numbered migrations on open, a too-new database refused loudly. Only the service ever opens the database — clients keep using the service API — and storage moves out of `timetrack-core` into a service-owned store crate so the core stays free of I/O. On first v2 start an existing `store.json` + journal is imported once (journal replayed exactly as today's loader does, ids preserved so the result is verifiable entry-for-entry, sync metadata stamped), and the JSON files are left untouched as backup and rollback. Reminting ids as UUIDs is a separate later migration (§20), not part of the storage swap.

0010 rejected exactly this, to keep a human-readable, hand-repairable file and avoid a C build and a one-way migration of the user's only history. The trade has flipped: v2 needs partial queries instead of whole-history snapshots, entry search (#131), custom-range totals (#133), a cache for fetched external events (§19), tombstones for sync-readiness (§20), and one locking story across three filesystems — none of which a JSON document gives. The migration risk 0010 feared is bounded by keeping the old files byte-identical beside the new database.

## Considered Options

- **Keep JSON + journal, index in memory.** Rejected: every v2 query feature re-derives indexes at startup and re-ships whole snapshots; the journal removed the write cost, not the read shape.
- **Start clean on SQLite.** Rejected: v1's start-clean (§2) was for throwaway pre-project files; real histories exist now and the author chose automatic migration.
- **Let clients query the database directly.** Rejected: multi-writer file access is the world the service exists to prevent, and it would fork aggregation and timezone rules per client (ADR 0009).
