# UUID identities with unique-prefix resolution (#148)

Status: accepted; lands the §20 identity half of [0014](0014-sqlite-store.md)'s deferred remint (sync itself is still not designed, #161).

Entries and projects are identified by UUIDv4, lowercase hyphenated (`xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx`). The core stays clock-free and randomness-free behind its `Ids` trait: production ids mint only at the service/store boundary (the service's `UuidIds`, the store migration's injected generator), while `Counter` keeps minting sequential ids as the deterministic test fake. Generation never lives in a rule, so a rule test pins every id it asserts.

The remint is one way. Opening a v2 database runs a v2→v3 migration in a single transaction: every non-UUID project and entry id gains a fresh UUID, and entry→project links, source→project links, suggestion→entry links, and tombstones (by kind, through the same mapping — or fresh, when their record is already gone) follow. A generator failure aborts the step and the database stays v2, untouched. Importing a legacy JSON store converges to the same UUID-only state before its first write; the JSON files stay byte-identical beside the database, as since 0014. Already-UUID ids are left alone either way, so neither path rewrites an identity some peer may already hold. There is no downgrade: a v3 database on an older binary is refused as too new, the §2 rule unchanged.

UUIDs stay typeable through unique-prefix resolution, shared as one pure helper: an exact id always wins, an empty input never resolves, a prefix of exactly one id resolves to it, and a prefix of several refuses as ambiguous without mutating anything. Resolution happens wherever an entry/project id is looked up — core rules, the service's `add_source`/`confirm` paths (which refuse before any claim or notification), and the CLI's `find_entry`/`find_project`/`pick_project` (exact, then name, then prefix; resolved ids travel onward). Ambiguity has its own typed errors (`AmbiguousEntry`/`AmbiguousProject`) on the wire, so scripts match one string either side.

CSV import/export semantics do not move: exported ids are UUIDs, and import stays match-by-content, never trusting the file's id column — re-importing an export is still a no-op.

## Considered Options

- **Keep sequential ids per device with a device prefix.** Rejected: prefixes leak the writer into every id, reintroduce a coordination story (who assigns device prefixes?), and still need a migration of the same shape.
- **Content-hash ids.** Rejected: an edit changes the identity, so every rename or time fix would rewrite references — the opposite of a stable identity.
- **Resolve prefixes only in the CLI.** Rejected: the socket API and GUI-driven actions accept the same ids, and three resolvers would drift; one helper, resolved at every lookup, keeps them identical.
