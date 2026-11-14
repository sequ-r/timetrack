# TimeTrack requirements

Status: **v1 agreed and largely built; v2 rework agreed, in progress.**
Sections marked **[DECIDED]** were settled with the author. The v2 rework
(§16–§20) settled three further decisions with the author: desktop
multiplatform with sync-ready data, read-only external calendar/task
sources, and an embedded SQLite store with automatic migration. Where a v1
decision and a v2 section disagree, the v2 section wins and says which
earlier decision it supersedes.

> **What exists right now** is tracked in [§15 Build status](#15-build-status).
> This document is the specification; §15 is the gap between it and the code.
> Where the two disagree, the code is what ships and §15 says so.

The two decisions that shaped everything else: totals are **sum of durations**
(§4) and the **running timer is cut** (§7). Together they simplify the model
substantially — every entry is closed, and the service stops being a state
machine owner.

The centre of the app is the **Home tab** (§11): this week's total, in big
bold characters, at the top with nothing above it; four quick-add buttons;
undo; then the week's time per project. Three tabs: Home, Projects, Export.
That hero is an invariant of the v2 rework (§16): it survives every platform
port unchanged, and the CLI's `status` prints the same total first (§18).

The philosophy the v2 rework keeps: a pure **core** (model, rules,
aggregation — no clock, I/O, async, or IPC), one local **service** owning all
state, and thin **GUI and CLI** clients of that service. v2 changes where
that stack runs (Linux, Windows, macOS — §16), how the service stores data
(SQLite — §17) and is reached (portable local IPC, not D-Bus — §16), and
what the terminal client is (one-shot commands only, no TUI — §18). It does
not change §4–§10: what an entry is, how totals are computed, how time is
entered and edited, and what an export looks like.

## 1. What this app is

TimeTrack records **how much time went into a project**, so you can see where a
week or a month went, and hand a stakeholder an export.

It is **not primarily a stopwatch.** The primary action is *entering* time:
manually, or in quick increments. A running timer is at most a convenience for
the case where you are starting work right now.

### The model in one line

> "I spent about three hours on *project X* this week."

### Who it is for

One person, tracking their own time, on their own machines. v1 shipped for
one Linux machine; v2 (§16) targets the same person's Linux, Windows, and
macOS desktops, with data shaped so those machines can be reconciled by a
future sync (§20) — still never multiple people sharing one store.

## 2. Decisions already made

**[DECIDED] Projects/tags from the start.** Entries are grouped by project.
This is a schema-level decision and retrofitting it means changing the `Entry`
struct, the store format, the D-Bus protocol and both UIs.

**[DECIDED] Full editing.** Entries can have their times adjusted after the
fact, entries can be deleted, and entries can be split and merged.

**[DECIDED] Personal retrospective first, export second.** The weekly and
monthly per-project totals are the product. Export is for showing someone else.

**[DECIDED] Manual entry is a first-class input method**, alongside quick-add
increments. A running timer was considered here and cut in §7; the "retained?"
is resolved as **no**.

**[DECIDED]** The service owns all state; the GUI and CLI are clients. This
already works and is not up for renegotiation.

**[DECIDED] Start clean on storage.** *(Superseded for the v2 store by §17:
real histories now exist, so the SQLite move auto-migrates them; the rule
below still describes how the pre-project timer-era files were handled.)*
The existing `store.json` files are
throwaway — they were created while developing the timer model, and they hold
entries whose shape is now wrong. No migration is written; an unreadable store
produces a clear "unsupported or corrupt store" message rather than a crash.
A version field is added now so a future migration has something to branch on.

**[DECIDED]** Edition 2024, gpui for the GUI, ratatui for the CLI. *(The
ratatui half is superseded by §18: the TUI is removed and the terminal
client is one-shot CLI commands only.)* (Originally
gpui-ce, the community fork, because the published gpui crate carried no
platform backend; upstream 0.2.2 ships one, so the GUI is on upstream gpui.)

### v2 decisions (settled with the author; detail in §16–§20)

- **[DECIDED] Desktop multiplatform, sync-ready.** Linux, Windows, and
  macOS first; local-first (no account, no server) but every record carries
  the identity and timestamps a future sync needs (§16, §20, ADR 0013).
- **[DECIDED] Calendar and tasks are fetched, not built.** External
  sources are read-only; their events and tasks arrive as suggestions the
  user confirms into entries (§19, ADR 0015). TimeTrack does not grow its
  own calendar or task manager.
- **[DECIDED] Embedded SQLite, auto-migrating.** The service's store moves
  from JSON checkpoint-plus-journal to SQLite, importing existing histories
  automatically and keeping the old files as backup (§17, ADR 0014).
- **[DECIDED] The terminal client is a CLI, not a TUI.** It is used between
  other work in a terminal, so it is one-shot commands with scriptable
  output; the full-screen TUI is removed (§18, ADR 0016).

## 3. Data model (proposed)

Today an `Entry` is `{id, description, started_at, ended_at}`. Proposed:

```rust
pub struct Entry {
    pub id: String,
    pub project_id: String,        // was: no grouping at all
    pub description: String,       // optional note, not the label
    pub started_at: i64,           // ms since epoch, UTC
    pub ended_at: i64,             // was Option<i64>; the timer is gone (§7)
    pub source: EntrySource,       // Manual | QuickAdd
    pub note: Option<String>,      // free-form, never parsed
}

pub enum EntrySource { Manual, QuickAdd }

pub struct Project {
    pub id: String,
    pub name: String,
    pub colour: Option<u32>,       // for the UI
    pub archived: bool,
}
```

Notes on the choices:

- `description` stops being the label. Previously it was the only text, and it
  was doing the job of a project name. Now `project_id` carries the grouping and
  `description` is genuinely optional.
- Timestamps stay **UTC milliseconds**. Local time is a display concern only.
  This keeps arithmetic exact across DST.
- `source` exists so that hand-entered time is distinguishable from measured
  time. For billing, that distinction may matter; for personal tracking it is
  useful to sanity-check yourself.
- **v2 additions (§17, §20):** `id` becomes a UUID in a dedicated later
  migration (the sequential `e{n}` / `p{n}` counters collide across
  machines; §17 preserves ids through the storage swap itself), and both
  records gain `created_at`, `updated_at`, `deleted_at` (a tombstone:
  deletion sync needs to be able to say "gone", not just be absent), and
  `device_id` of the last writer — carried as store metadata (§17) until
  the sync design needs them in the model itself. `EntrySource` gains
  `External` for entries confirmed from a
  suggestion (§19). The fields above are otherwise unchanged, and the sums
  in §4 ignore all metadata.

## 4. Totals: sum of durations **[DECIDED]**

Manual entry makes overlapping entries normal, so "total time" has to mean one
specific thing:

| | Two overlapping entries of 1h | Meaning |
|---|---|---|
| **Sum of durations** | 2h | how much you worked on it (can exceed 24h in a day) |
| Wall-clock union | 1h | how much of your day it occupied |

**Decision: sum of durations**, everywhere — daily, weekly, monthly totals and
exports. It is the quantity the user actually means by "I spent three hours on
X". Wall-clock union is not computed.

Consequences accepted:

- A day's total may exceed 24h, and a week's may exceed 168h. That is not
  prevented; it is a signal the data is wrong.
- **A sanity warning is wanted** when a day's summed time exceeds 24h, so a
  duplicated or overlapping entry gets noticed. Displayed, not enforced: the
  entry is still stored and still counted.
- Entries overlap freely and are never rejected for it. Overlap is only a
  reporting concern.
- The data model already stores raw intervals, so switching to union later would
  need no migration. Only the aggregation code would change.

## 5. Manual entry **[five methods, all need add *and* remove]**

Every method below must work in both directions. "Remove" means shortening an
entry that turns out to have been overstated — which happens constantly when
the time is estimated rather than measured.

| # | Method | Add | Remove |
|---|---|---|---|
| 1 | **Explicit start/end** — pick both timestamps | create an entry over any interval | drag either endpoint to an earlier time |
| 2 | **Duration, ending now** — "3 hours, just now" | create `[now−3h, now]` | shorten a recent entry to "it was really 1 hour" |
| 3 | **Duration, into the past** — "3 hours, ending at 14:00" | create `[14:00−3h, 14:00]` | shorten an entry you just wrote for this morning |
| 4 | **Quick-add** — the four buttons (§6) | tap to add 5/15/30/60 min | tap again to undo, or reduce a grouped entry |
| 5 | ~~**Running timer → stop**~~ | removed — see §7 | — |

### What "remove" actually means

This is the part worth being precise about, because there are three different
operations and only one of them is destructive:

- **Shorten** an entry (1, 2, 3): its `ended_at` moves earlier. Reversible by
  moving it back; nothing is lost.
- **Undo** a quick-add (4): removes the entry that tap created. Only meaningful
  because quick-add creates a *separate* short entry rather than incrementing an
  existing one (§6) — so undo is unambiguous.
- **Delete** an entry entirely: only via the explicit delete action, never as
  an accidental side effect.

**No method may delete an entry as a side effect of reducing time.** A user who
meant "subtract 30 minutes" must never lose an hour of data.

### Rules that apply to all five

- An entry must not end before it starts; the UI prevents it rather than
  rejecting after the fact.
- Entries may overlap freely and are never rejected for it (§4).
- Quick-add must not require opening a dialog.
- Every destructive or hard-to-undo action is undoable in one step.

### v1 scope **[DECIDED]**

**Methods 2, 3, 4 and 1 ship in v1**, deferring only the drag-to-trim gesture
— which is a refinement of method 1's dialog, not a separate feature.

**Methods 1 and 2 are one dialog.** Method 2 is method 1 with "end = now"
pre-filled, so they are the same form with a different default rather than two
forms. That avoids duplicating validation, and "end = now" is the common case
anyway.

The daily/monthly views are *read* views, not entry methods, so they are in v1
by way of §9 without depending on this decision.

## 6. Quick-add **[DECIDED]**

"Quick add / remove in 5-minute slots." With the timer cut (§7), this is no
longer a convenience — it is what replaced it, and it is the fastest path to
recording time. It deserves real design attention.

Still ambiguous, and the two readings are genuinely different features:

- **(a) An increment on an existing entry.** "+5 min" extends the last entry on
  this project. Cheap, but the entry's `ended_at` moves, which is surprising
  after the fact.
- **(b) A new short entry of its own.** "+5 min" creates a separate 5-minute
  entry. More predictable, and each click is independently undoable — but it
  produces a list with many tiny rows unless the UI groups them.

**Recommendation: (b),** with the list collapsing consecutive short entries on
the same project into a single "1h 15m · 15 taps" style row. (a) silently
rewrites history, which is exactly what §8's editing exists to avoid.

**[DECIDED]** Durations are **5, 15, 30 and 60 minutes**, one button each.

- Four buttons is the whole set. More invites deliberation; fewer forces
  arithmetic, and the point of quick-add is to avoid that.
- Applies to the **selected** project, always.
- **Undo must be the last thing pressed**, always visible, and undo the whole
  step. With four one-click buttons, mistakes are certain.

Where they live is §14: on the Home tab, under the clock.

Not in scope: custom durations from the quick-add row. Use the manual entry
dialog (§5) for anything unusual.

## 7. Running timer: cut **[DECIDED]**

The timer is **removed**. It is replaced by quick-add (§6) plus the merge
action (§8) — the two things it was standing in for.

Rationale: with sum-of-durations as the total, a running timer and a manual
entry are the same kind of record. Keeping both meant maintaining two ways to
create the same thing, and the timer brought its own failure mode (forgetting
to stop it, entries left `ended_at: null` indefinitely).

### Consequences

This is a genuine simplification, and it reaches further than the model:

- **`ended_at` becomes non-optional.** Every entry is closed.
  `Entry.ended_at: Option<i64>` becomes `i64`, and every `duration_ms(now)`
  call site loses its `now` argument. The "is this entry running?" question
  disappears from the code entirely.
- **`EntrySource::Timer` is removed.** `source` is reduced to
  `Manual | QuickAdd`.
- **The service's "running" concept goes away.** No `running` field in
  `Snapshot`, no `AlreadyRunning`/`NotRunning` errors, and no start/stop/cancel
  D-Bus methods. The service becomes a straightforward CRUD-and-aggregate
  server rather than a state machine owner.
- **The GUI loses the live clock**, which was its most prominent element. It
  needs a replacement focal point — the week total, presumably. The window
  layout is affected.
- **The CLI loses `start`, `stop`, `cancel`.** `status`, `list` and (new)
  add/edit/export remain.
- The state machine becomes smaller and easier to reason about, which is a real
  win for something with this much editing surface.

**Net:** less code overall, but the GUI needs redesign work that the timer was
previously carrying.

## 8. Editing **[DECIDED]**

- Change project, times, description of any entry.
- Delete.
- **Split** one entry into two at a chosen point.
- **Merge** adjacent or overlapping entries into one.
- All of the above must go through the service and be persisted; the GUI and CLI
  are clients.

### Merge and overlap (§4)

Merging two overlapping entries has to decide the result's interval, and this
genuinely collides with the sum-of-durations decision:

- If the merged entry spans **earliest start → latest end**, the overlap is
  collapsed. Two overlapping 1h entries become one 1h entry, so merging them
  *reduces* the total. That contradicts §4, which counts the overlap twice.
- If the merged entry **preserves the summed duration**, its interval no longer
  matches the wall clock — it would claim to run longer than it did, or the two
  halves would have to overlap on purpose.

**Resolution: merge takes the union of the intervals — earliest start to latest
end — and the total it produces may be less than the sum of its parts.** This is
the one place where the app does not report a pure sum, and it does so
deliberately: merging overlapping entries is an explicit statement by the user
that the overlap was a mistake. Silently inflating a merged entry to preserve a
total would be worse.

Documented in the UI (the confirmation says the duration will shrink by the
overlap), and covered by tests.

## 9. Weekly and monthly totals **[DECIDED]**

The stated goal: "how much time I spend on a particular project any given week
and month."

Needed:

- Per-project totals for a week, and for a month.
- A single total for the same periods.
- Navigation between weeks/months; "this week", "last week".
**[DECIDED] ISO weeks** (Monday 00:00 to Sunday 23:59:59, local time). Stated
explicitly because it determines what "this week's total" means when a week
straddles a month boundary.

Months are calendar months, local time.

**[DECIDED] Aggregation lives in the service.** One implementation, both UIs
agree, and it is testable once. A week boundary rule implemented twice is a bug
waiting to happen — and the CLI and GUI would be free to disagree, which is
exactly the class of problem the shared core exists to prevent.

Consequences: the service gains weekly/monthly aggregate methods, and the
snapshot grows the current week's figures so a client can render the Home tab
without a second round trip.

## 10. Export **[DECIDED]**

Export to something a stakeholder can open.

**[DECIDED] CSV.** Opens in Excel, is diffable and greppable, needs no library,
and any spreadsheet tool reads it. XLSX is deferred: it earns its place only if
someone wants a formatted multi-sheet workbook, which nobody has asked for.

- One row per entry, with a header row.
- Times as ISO 8601 local timestamps (`2026-09-29T14:00:00`), not epoch millis —
  a stakeholder opens this in Excel, not in a hex editor.
- Duration in both `HH:MM:SS` and raw minutes, so the file is readable without
  arithmetic.
- Include the `source` column, so hand-entered time is visible to the reader.
- Must be exportable **from the GUI**, not only the CLI — the stakeholder
  workflow should not require opening a terminal.
- Scope of an export: current week, a chosen week, a chosen month, or all time.
- Should include the `source` column, so hand-entered time is visible.

## 11. GUI structure **[DECIDED]**

The window is tabbed. The **Home** tab is the only one that is a dashboard;
everything else is reached by switching tabs.

### Home tab, top to bottom

1. **This week's total, large, at the very top.** The sum of every entry this
   ISO week (§4, §9), in big bold characters. It is the number the app exists
   to produce, so it gets the position of honour and nothing sits above it.

   No clock. No greeting, no time-of-day flourish — not even a "good morning".
   The first thing on screen is the answer.

   **This hero is an invariant (§16):** every platform's main page opens
   with this total, this large, with nothing above it, and no port, theme,
   or new tab may demote it. The CLI honours the same rule in text: the
   week total is the first line of `timetrack status` (§18).

2. **Quick-add buttons.** The four durations from §6 (5 / 15 / 30 / 60), each
   applying to whichever project is selected.

3. **Undo**, immediately after the quick-add buttons — the last thing pressed.

4. **Per-project totals for this week**, one row per project: name, time, and a
   bar proportional to the total, so the distribution is readable without
   reading every number.

### Tabs

**Three, for now:**

- **Home** — the layout above.
- **Projects** — create, rename, recolour, archive.
- **Export** — CSV (§10) for the chosen period.

Entry list and editing are **not a tab yet**. They live behind the per-project
rows and the quick-add undo path until v1 says otherwise; §5 and §8 will settle
where editing actually happens, and that may not need a fourth tab at all.

### Notes

- Project selection is global window state, not per-tab, so quick-add always
  has an unambiguous target.
- Tabs are switched with keyboard shortcuts as well as clicks, since every
  other action in this app is a keystroke.
- The week total is the only large element. Everything else is deliberately
  quieter, so it is not competing with the one number that matters.

## 12. Non-goals

Stating these now, to keep them from arriving later as "small features":

- Multiple users, or a shared/synced database.
- Accounts, login, sync, cloud storage. *(Amended by §20: still non-goals
  to **build** in v2, but the data model is now deliberately sync-ready —
  UUIDs, tombstones, writer timestamps — so adding one person's sync later
  needs no second migration. The non-goal is the sync service, not the
  readiness.)*
- A server component. (The local per-machine service of §13/§16 is not
  this: it serves one user's own clients on one machine.)
- A built-in calendar or task manager (§19): external sources are fetched
  read-only instead; TimeTrack never becomes a second calendar to keep in
  step with the first.
- Mobile clients in v2 (§16): Android/iOS wait until the desktop service
  and sync story exist to build on.
- Invoicing, rates, currencies.
- Recurring or scheduled entries.
- Timezone-aware "work hours" or overtime logic.
- Importing arbitrary foreign CSV formats through a column-mapping UI
  (#112): re-importing TimeTrack's own export is in scope (#132); a
  mapping surface waits for a concrete source format to design against.
- Drag-to-trim with the mouse (#112): there is no timeline surface for
  a drag to happen on, and building one for a single gesture inverts
  the cost. Time editing stays fields and keys until a timeline exists
  for its own sake; §5's deferral of drag-to-trim is therefore a
  settled non-goal for v1, not an unbuilt specification item.

## 13. Architecture consequences

The service/CLI/GPUI split is unaffected and stays. What changes:

| Area | Impact |
|---|---|
| `timetrack-core` | `Entry` gains `project_id`, `source`, `note`. New `Project` type. New split/merge and time-edit operations in the state machine. Overlap is no longer an error. |
| Store format | Schema change. Needs a migration for existing `store.json` files, or a version bump with a clear "unsupported format" message. |
| D-Bus protocol | New methods: create/update entry, create/list projects, split, merge, weekly/monthly aggregates (§9). `Snapshot` gains the new fields and loses `running`. |
| Store | Version field added; no migration (start clean, §2). |
| Service | Becomes the aggregation point (§9). |
| GUI | Rebuild the window as the tabbed Home/Entries/Projects/Export layout (§11), plus a manual-entry dialog and a project selector. |
| CLI | New subcommands for the same. |

Both crates keep their existing properties: core stays free of GUI/IPC/async,
and the state machine keeps taking an explicit timestamp so it stays testable
without a clock.

*(v2 note: the table above describes the v1 build. Its D-Bus and store rows
are superseded by §16 (local-socket IPC) and §17 (SQLite in a
service-owned store crate); its core/service/client shape is exactly what
§16 keeps.)*

## 14. Open questions, collected

**All questions are answered.** There is nothing open.

Settled, in the order asked:

- **Totals** → sum of durations (§4), with a >24h/day warning.
- **Running timer** → cut (§7); quick-add and merge replace it.
- **Quick-add durations** → 5 / 15 / 30 / 60 (§6).
- **Week boundaries** → ISO weeks; aggregation in the service (§9).
- **Export format** → CSV (§10).
- **Store migration** → start clean (§2) for the timer-era files;
  **auto-migrate** real histories into SQLite in v2 (§17).
- **Large clock on Home** → dropped; the week total is the large element at the
  top with nothing above it (§11).
- **Tabs** → Home, Projects, Export (§11).
- **Archived projects** → remain in historical totals (§14 note).
- **v1 entry scope** → methods 1, 2, 3, 4, with 1 and 2 sharing one dialog
  (§5).

Two things are deliberate gaps rather than open questions, and should be
treated as follow-ups rather than blockers:

- **A dedicated entry list tab** (§11); editing currently lives behind the
  per-project rows and the undo path. If §5/§8 in practice need more room than
  that, a fourth tab is the likely fix.

Four scope questions raised once histories started growing were settled
together (#112), by whether the work is client-only over data the clients
already hold, or reshapes the model/wire:

- **Search/filter by entry text → in scope** (#131). Client-side over the
  snapshot; totals and exports must stay unfiltered. No tags or query
  language.
- **Import/backup → in scope, bounded** (#132). Re-importing TimeTrack's
  own CSV export, idempotently, with fresh ids and
  match-by-name projects; a hand-copied `store.json` +
  `store.json.journal` pair remains the whole-store backup. Generic
  foreign-format mapping is a non-goal above.
- **Custom date-range reporting → in scope** (#133). Week/month anchors
  stay the defaults; a range is an explicit pair of local days under the
  same started-at-wins rule (ADR 0002), needing one core export, one
  call per live wire interface, and the three pickers.
- **Drag-to-trim → non-goal for v1** (above): no timeline exists to drag
  on, so its real cost is that surface, not the gesture.

Settled by the v2 rework, in §16–§20:

- **Platforms** → Linux, Windows, macOS desktop, local-first and
  sync-ready; mobile deferred, no account yet (§16, §20).
- **Database** → yes: embedded SQLite owned by the service only,
  auto-migrating existing JSON histories (§17).
- **Calendar/tasks** → connect to external services read-only
  (`.ics`, then CalDAV), suggest-don't-create; nothing built in (§19).
- **Terminal client** → one-shot CLI only; the TUI is removed (§18).
- **Hero** → unchanged: the week total, big and bold, at the top of the
  main page on every platform (§11, §16).

The app is ready to build. It has since been built — see §15.

## 15. Build status

Where the code stands against the sections above. Recorded here so the
specification and the implementation do not quietly diverge.

### Built and verified

| Requirement | Where | Verified by |
|---|---|---|
| §3 Model, `ended_at: i64` | `core/src/model.rs` | 172 core tests |
| §4 Sum of durations, overlap counted twice | `core/src/aggregate.rs` | unit tests + e2e totals |
| §4 >24h/day warning, never a rejection | `core/src/rules.rs::days_exceeding_24h` | e2e |
| §5 Methods 1–4 | core, service, CLI | e2e exercises all four |
| §5 Shorten never deletes | `rules.rs::set_times` | service test |
| §5 Undo refuses a non-quick-add | `rules.rs` + service | e2e and unit tests |
| §6 Quick-add 5/15/30/60, own entry each | `rules.rs::quick_add` | e2e |
| #110 Typed D-Bus API, dual-served transition | ADR 0012: `Entries2` typed structs + named `WireError`s, clients probe `ApiVersion` and fall back to legacy JSON | proto round-trip/size tests, e2e typed checks, both-directions skew probe |
| §6/#105 Quick-add reachable per desktop | ADR 0011: tray bundled in AppImage for watcher desktops, launcher quick-add actions + `quick --notify` on stock GNOME, watcherless tray notice | CLI notification unit test, tray notice unit test, AppImage contents check |
| §7 Timer cut; no `running` anywhere | whole tree | a test asserts no `running` field exists |
| §8 Split and merge | core + service D-Bus, GUI `s`/`m`, CLI `split`/`merge` | unit tests + e2e + gui-smoke |
| §8 Split/merge confirmation stating the shrink | shared `merge_shrink_ms`/`format_merge_delta` in `rules.rs`; GUI confirm banner, CLI prompt/`--yes` | core/GUI/CLI tests + e2e (touching + overlap merges, 5 refusal guards) |
| §8 Merge takes the union, may shrink the total | `rules.rs::merge_entries` | tests assert both cases |
| §9 ISO weeks, months, per-project | `core/src/aggregate.rs` | boundary tests |
| §9 Aggregation applied by the service | `service/src/interface.rs` | e2e |
| §6 Quick-add from the GUI — by click and by keystroke | `gui/src/app.rs` | `gui-smoke.sh`, XTest click and key |
| §11 Three tabs | `gui/src/app.rs` | `gui-smoke.sh` (screenshot) |
| §11 Tabs switch by keyboard (`Tab`/`Shift+Tab`) | `gui/src/app.rs::cycle_tab` | GUI tests + gui-smoke (away-change, 3-press return) |
| §11 Per-project rows show proportional bars | `gui/src/app.rs::bar_width` (week rows) | GUI tests |
| §11 Week total large, at the top, nothing above it | `gui/src/app.rs`, 72px | `gui-smoke.sh` (text drawn; the value itself is not read back) |
| §14 Archived projects stay in totals | `rules.rs::set_archived` | service test |
| §2 Version field, no migration | `model.rs::STORE_VERSION` | storage test |
| §2 Corrupt store refused, not reset | `storage.rs` | storage test |
| §2/#103 Incremental persistence, push refresh | `storage.rs` (`Journal` append + checkpoint, ADR 0010), service `Changed` signal, clients wake on signal with slow fallback poll | storage journal tests (incl. 20k-entry byte bound, torn-tail, pre-journal load), tray real-service `Changed` assertion |
| §5 Methods 1 and 2 as one dialog, GUI + CLI | `core/src/parse.rs` (shared spellings), GUI dialog (`a` new / `e` edit), CLI wrappers | core/GUI tests + gui-smoke (open, cancel, save, typed text) |
| §10 CSV export, ISO 8601 local time, `HH:MM:SS` + minutes, `source`, GUI + CLI, week/month/all | `core/src/export.rs`, service `ExportCsv`, CLI `export`, GUI Export tab | core/service tests + e2e (157 checks) |
| §9 Named-zone DST bucketing per instant | `core/src/tz.rs` (`Tz`, tz-database via `chrono-tz`), service `Tz::try_from_env` (strict, refreshed live; ADR 0009), snapshot `tz` + per-instant `local_offset_ms`, CLI/GUI strict `Tz::from_snapshot` | core DST tests (Rome winter/summer, both transition days' wall clocks, week-split), service client/service agreement test across both transitions, live `TZ=Europe/Rome` snapshot check |
| §11 Project rename and recolour | `core::update_project`, service `UpdateProject`, CLI `rename`/`recolour`, GUI naming prompt (`n` new / `r` rename) + colour dot | core/service/CLI/GUI tests + e2e (rename, recolour, 5 guards, snapshot asserts) + gui-smoke (prompt paint, keyboard create/rename) |
| §13 Core free of GUI/IPC/async/clock | `core/` | by construction |
| §18 CLI-only terminal client: TUI removed, bare `timetrack` is `status` (week total first), `status`/`list --json` | `cli/src/main.rs` (ADR 0016) | CLI unit tests (default command, `tui` refused, JSON shapes) + e2e CLI-only section |
| §17 SQLite store behind the existing wire: `timetrack-store` (schema v1, WAL, tombstones/device metadata), one-transaction mutation diffs, JSON checkpoint+journal auto-imported once and left byte-identical, ids preserved | `store/src/lib.rs`, `service/src/interface.rs` (ADR 0014) | store unit tests (round-trip, tombstones, import-once, corrupt/too-new refusals), service tests (restart, write-lock rollback, legacy import), e2e store section |
| §16 Local-socket IPC dual-served with D-Bus: length-prefixed frames, one request per connection, `Changed` subscription, socket ownership = single instance, client auto-spawn (sibling binary, override-guarded), socket-only serving with no bus | `proto/src/socket.rs`, `service/src/socket.rs` (ADR 0013) | proto socket tests (frames, caps, paths, round-trip), service socket tests (round-trip, typed refusals, stale/owned socket), tray real-service rig over socket, e2e bus-less status section, scratch-home auto-spawn check |
| §16 platform data dirs (Linux XDG unchanged, macOS Application Support, Windows LOCALAPPDATA) + native Windows/macOS CI job for the portable stack | `service/src/platform.rs`, `.github/workflows/ci.yml` (`cross`) | pure per-OS unit tests on Linux; local `cargo check` of core/proto/CLI for `x86_64-pc-windows-gnu` and `x86_64-apple-darwin` (store needs a native C toolchain, so it is proved by the CI job, not locally) |
| §19-adjacent client slices: `list --search` (#131, snapshot-local, totals unfiltered) and `import` of our own CSV (#132, match-by-name projects, whole-second dedupe, `--dry-run`, notes restored over the socket) | `core/src/export.rs::parse_export_csv`, `cli/src/main.rs` | core round-trip/refusal tests, CLI filter/plan tests, e2e search-import section (own-export no-op, foreign row, re-import skip) |
| §19 `.ics` external sources, CLI surface: core parser (VEVENT/VTODO, TZID/floating/UTC, folding, durations; all-day/no-UID skipped, RRULE first occurrence only), store schema v2 (sources + suggestions), service fetch (file + capped HTTPS, read-only), socket wire, `source-add`/`sources`/`refresh`/`suggestions`/`confirm`/`dismiss`; confirm creates an `External` entry | `core/src/ics.rs`, `store/src/lib.rs`, `service/src/interface.rs`, `cli/src/main.rs` | core parser tests, store dedupe/decide tests, service-seam socket round-trip (refresh/confirm/dismiss/set-note), e2e .ics section |
| §16 recurrence expansion (windowed RRULE: FREQ/INTERVAL/COUNT/UNTIL/BYDAY/BYMONTHDAY/BYMONTH/EXDATE, wall-clock across DST, refresh window −31d…+93d), CalDAV read-only collections (PROPFIND+GET, Basic auth from userinfo + `TIMETRACK_CALDAV_PASSWORD`, passwords refused in locators), GUI suggestions section on Home, custom date-range export (#133, started-at-wins) | `core/src/ics.rs`, `service/src/caldav.rs`, `gui/src/app/render/home.rs`, `core/src/export.rs` | core recurrence/DST tests, CalDAV local-server tests, GUI unit tests 126/126 and a headless-backend launch against a seeded service (no virtual display exists in the dev container, so rendered pixels remain CI's gui-smoke job to verify), e2e range section |
| #129 D-Bus service transport retired: socket-only service and client, tray fake re-served over socket frames, e2e oracle is `timetrack snapshot`, service/CLI drop zbus (notifications and the tray menu remain D-Bus protocols, as they are) | ADR 0017, `proto/src/lib.rs`, `service/src/main.rs`, `scripts/e2e.sh` | all crate tests green after removal, e2e 159 checks with no session bus started |

### Specified but not built

- **§5 drag-to-trim.** Deferred by decision, not by omission.
- **The v2 rework (§16–§20), in plan order.** §15 records each slice here
  as it lands; until a row appears in the table above, the v1 behaviour in
  that row is what ships.
  1. §16–§20 requirements, ADR 0013–0016, glossary terms (this document).
  2. ~~§18 CLI-only terminal client~~ — **landed**, see the table above.
  3. ~~§17 SQLite store + automatic migration~~ — **landed**, see the
     table above. (The core's JSON `storage` module remains as the
     legacy import reader, and `Journal` remains the diff type the store
     applies; the UUID remint is §20's separate migration, not landed.)
  4. ~~§16 portable local-socket IPC~~ — **landed** (see the table
     above), and the D-Bus service interfaces are since retired
     outright (#129, ADR 0017): the socket is the service's only wire.
  5. §16 Windows/macOS: **data dirs, CI matrix + artifacts, named-pipe
     transport, and GUI portal gating landed** (table above); still
     open — native save dialogs/tray per OS and installers/app bundles.
  6. §19: **search (#131), CSV import (#132, incl. notes), `.ics` and
     CalDAV suggestions with recurrence expansion, the GUI
     suggestions surface, and custom ranges (#133) landed** (table
     above); still open — provider-specific APIs if CalDAV proves
     insufficient, and a GUI editor for source registration.

### Known defects and gaps

(none open — the placeholder project names are gone: every name is typed.)

## 16. Multiplatform **[DECIDED — v2]**

TimeTrack is thought of as multiplatform from v2 on. The same
core → service → GUI/CLI stack (§13) runs on **Linux, Windows, and
macOS desktops**. Mobile is deferred (§12): it reuses this core and
service API when it comes, it does not shape them now.

What is already portable, and stays untouched: `timetrack-core` (no GUI,
IPC, async, or clock — §13) and the gpui toolkit itself. What is
Linux-bound today and must move:

| Linux-bound piece (v1) | v2 replacement |
|---|---|
| D-Bus service transport (`Entries2`, ADR 0012) | Local-socket IPC: Unix sockets on Linux/macOS, named pipes on Windows; same typed contract and `Client` API, the `Changed` push becomes a subscription on the connection (ADR 0013) |
| Bus-name ownership as single-instance | Socket ownership: the service that binds the socket is the service; a second copy exits harmlessly, exactly as today |
| AppImage `AppRun` shell starting service + tray | Any client auto-spawns the sibling service binary when the socket does not answer; per-OS packaging (AppImage stays the **Linux** install, ADR 0008 scoped by ADR 0013) |
| `$XDG_DATA_HOME` store path | Per-OS data dir from one small platform module in the service (XDG on Linux, `%LOCALAPPDATA%` on Windows, `~/Library/Application Support` on macOS) |
| StatusNotifier tray, launcher actions, portal dialogs, AppImage self-update | Stay Linux-only behind `cfg` until per-OS equivalents earn their place; they never block the core/GUI/CLI port |

Invariants every port must keep:

- **The hero (§11):** the week total in big bold characters at the top of
  the main page, nothing above it, identical meaning (§4, §9) everywhere.
- **The service is the only writer** and the only timezone authority
  (ADR 0009); clients on every OS are equally thin.
- **Local-first:** a fresh install on any OS works fully offline with no
  account. Multiplatform means "runs there", not "phones home".

The transport spike must prove the IPC works without a tokio runtime:
this workspace's ban on tokio (root `Cargo.toml`) is load-bearing and
survives v2. Dual-serving D-Bus and sockets for one Linux release keeps
old clients working through the cutover, the same pattern as ADR 0012.

## 17. Storage: embedded SQLite **[DECIDED — v2]**

**Yes, add a database: embedded SQLite, opened only by the service.**
This supersedes the storage half of ADR 0010 (the `Changed` push it also
introduced stays, over the new transport). ADR 0010 rejected SQLite to
keep a human-readable file and avoid a C build and a one-way migration;
those costs are now the smaller side of the trade, because v2 needs what
a JSON document cannot give: partial queries instead of whole-history
snapshots, entry search (#131), custom-range totals (#133), a cache for
fetched external events (§19), sync metadata (§20), and identical
locking behaviour on three filesystems.

Rules:

- **The service is the only process that opens the database.** Clients
  keep talking to the service; "just query the DB from the CLI" is the
  multi-writer world §2 already refused.
- **The core stays pure.** Storage moves out of `timetrack-core` into a
  service-owned store crate; the core's rules keep operating on the
  model in memory and stay testable without any I/O.
- WAL mode, bundled SQLite (no system library to version-skew), schema
  versioned by numbered migrations applied on open. A database newer
  than the binary is refused loudly, never silently reset — the §2 rule,
  unchanged.
- **Automatic migration, once:** on first v2 start, if a `store.json`
  (+ journal) exists and no database does, import it — replaying the
  journal exactly as the loader does today, torn tail and all — and
  stamp sync metadata (§20). The JSON
  files are kept untouched as the backup and the rollback; the importer
  never modifies them. Corrupt or legacy-format stores keep today's
  refusal messages. The import **preserves existing ids**, so the migrated
  database is logically identical to the JSON store and the swap is
  verifiable entry-for-entry; reminting ids as UUIDs is a separate later
  migration, coupled with unique-prefix resolution so the CLI stays
  typeable (§20), not something smuggled into the storage swap.
- Backup becomes "copy the database file (plus `-wal`) while the
  service is stopped", or the CSV export (§10) / re-import pair (#132);
  the README states this where it used to name the two JSON files.

## 18. Terminal client: CLI only **[DECIDED — v2]**

The terminal client is used **between other work, in a terminal, on other
projects** — `timetrack quick -m 15` between two `git` commands, not a
second full-screen home. So the ratatui TUI is **removed** and
`timetrack` is one-shot commands only (ADR 0016).

- Every action is a subcommand that prints one result and exits; the
  existing one-shot surface already covers every TUI action, so nothing
  is lost but the full-screen mode.
- Bare `timetrack` prints `status` — week total first, the hero rule in
  text (§11) — instead of opening a UI. `timetrack tui` is a breaking
  removal, announced in the release notes, not an alias kept forever.
- `status` and `list` gain `--json` so scripts, shell prompts, and
  status bars can consume them without scraping prose. Human output
  stays the default.
- Interactive editing beyond one command happens in the GUI; destructive
  CLI commands keep their current confirm/`--yes` behaviour.

## 19. External calendar and task sources **[DECIDED — v2]**

**Connect to external services; do not build a calendar or a task
manager** (ADR 0015). Users already keep calendars and tasks somewhere;
TimeTrack's job is to turn "that meeting happened" into tracked time
with one confirmation.

- **Read-only, suggest-don't-create.** The service fetches events and
  tasks, caches them in SQLite (§17), and offers each as a *suggestion*:
  a proposed entry (its interval, its title as the description) the user
  confirms, edits, or dismisses. Fetching never creates an entry and
  never writes back, so there are no two-way conflicts to resolve.
- **Mapping:** each external calendar/collection maps to at most one
  project, chosen by the user; unmapped sources suggest into the
  selected project. Confirmed entries record `source = External` and
  the external UID, so re-fetching the same event never suggests it
  twice and the §10 export shows where the time came from.
- **Order:** `.ics` file/URL first (no credentials involved), then
  CalDAV (`VEVENT` for calendar, `VTODO` for tasks) with etag/sync-token
  dedupe. Proprietary per-vendor APIs only if a concrete user need
  outlives what CalDAV covers.
- **Secrets** (CalDAV passwords, tokens) live in the OS credential
  store, referenced by name from the database — never stored in it.
- Connectors run in the service, so GUI and CLI see the same
  suggestions (`timetrack suggestions` lists, `confirm`/`dismiss` act)
  and a closed GUI never pauses fetching.

## 20. Sync-ready, not sync **[DECIDED — v2]**

v2 ships **no sync, no account, no server** (§12) — but it stops making
sync impossible, which the v1 model was quietly doing:

- UUID ids on entries and projects (v1's `e{n}`/`p{n}` counters collide
  the moment a second machine creates anything).
- `created_at` / `updated_at` / `deleted_at` tombstones and the
  `device_id` of the last writer on every record, maintained by the
  rules, invisible in totals and exports.
- External-source configuration (but **not** its secrets) is ordinary
  store data, so it can travel with the same future sync.

That is the whole v2 commitment: when one person's sync is designed
later, it reconciles records that already know their identity and
history, instead of needing a third storage migration first.
