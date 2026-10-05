# TimeTrack requirements

Status: **agreed and largely built.** Every question is answered; there are no
open decisions. Sections marked **[DECIDED]** were settled with the author.

> **What exists right now** is tracked in [§15 Build status](#15-build-status).
> This document is the specification; §15 is the gap between it and the code.
> Where the two disagree, the code is what ships and §15 says so.

The two decisions that shaped everything else: totals are **sum of durations**
(§4) and the **running timer is cut** (§7). Together they simplify the model
substantially — every entry is closed, and the service stops being a state
machine owner.

The centre of the app is the **Home tab** (§11): this week's total, large, at
the top with nothing above it; four quick-add buttons; undo; then the week's
time per project. Three tabs: Home, Projects, Export.

## 1. What this app is

TimeTrack records **how much time went into a project**, so you can see where a
week or a month went, and hand a stakeholder an export.

It is **not primarily a stopwatch.** The primary action is *entering* time:
manually, or in quick increments. A running timer is at most a convenience for
the case where you are starting work right now.

### The model in one line

> "I spent about three hours on *project X* this week."

### Who it is for

One person, tracking their own time, on their own machine.

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

**[DECIDED] Start clean on storage.** The existing `store.json` files are
throwaway — they were created while developing the timer model, and they hold
entries whose shape is now wrong. No migration is written; an unreadable store
produces a clear "unsupported or corrupt store" message rather than a crash.
A version field is added now so a future migration has something to branch on.

**[DECIDED]** Edition 2024, gpui for the GUI, ratatui for the CLI. (Originally
gpui-ce, the community fork, because the published gpui crate carried no
platform backend; upstream 0.2.2 ships one, so the GUI is on upstream gpui.)

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
   ISO week (§4, §9). It is the number the app exists to produce, so it gets
   the position of honour and nothing sits above it.

   No clock. No greeting, no time-of-day flourish — not even a "good morning".
   The first thing on screen is the answer.

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
- Accounts, login, sync, cloud storage.
- A server component.
- Invoicing, rates, currencies.
- Recurring or scheduled entries.
- Timezone-aware "work hours" or overtime logic.

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

## 14. Open questions, collected

**All questions are answered.** There is nothing open.

Settled, in the order asked:

- **Totals** → sum of durations (§4), with a >24h/day warning.
- **Running timer** → cut (§7); quick-add and merge replace it.
- **Quick-add durations** → 5 / 15 / 30 / 60 (§6).
- **Week boundaries** → ISO weeks; aggregation in the service (§9).
- **Export format** → CSV (§10).
- **Store migration** → start clean (§2).
- **Large clock on Home** → dropped; the week total is the large element at the
  top with nothing above it (§11).
- **Tabs** → Home, Projects, Export (§11).
- **Archived projects** → remain in historical totals (§14 note).
- **v1 entry scope** → methods 1, 2, 3, 4, with 1 and 2 sharing one dialog
  (§5).

Two things are deliberate gaps rather than open questions, and should be
treated as follow-ups rather than blockers:

- **Drag-to-trim** for adjusting times (§5, deferred).
- **A dedicated entry list tab** (§11); editing currently lives behind the
  per-project rows and the undo path. If §5/§8 in practice need more room than
  that, a fourth tab is the likely fix.

The app is ready to build. It has since been built — see §15.

## 15. Build status

Where the code stands against the sections above. Recorded here so the
specification and the implementation do not quietly diverge.

### Built and verified

| Requirement | Where | Verified by |
|---|---|---|
| §3 Model, `ended_at: i64` | `core/src/model.rs` | 72 core tests |
| §4 Sum of durations, overlap counted twice | `core/src/aggregate.rs` | unit tests + e2e totals |
| §4 >24h/day warning, never a rejection | `core/src/rules.rs::days_exceeding_24h` | e2e |
| §5 Methods 1–4 | core, service, CLI | e2e exercises all four |
| §5 Shorten never deletes | `rules.rs::set_times` | service test |
| §5 Undo refuses a non-quick-add | `rules.rs` + service | e2e and unit tests |
| §6 Quick-add 5/15/30/60, own entry each | `rules.rs::quick_add` | e2e |
| §7 Timer cut; no `running` anywhere | whole tree | a test asserts no `running` field exists |
| §8 Split and merge | core + service D-Bus | unit tests; **not in the GUI** |
| §8 Merge takes the union, may shrink the total | `rules.rs::merge_entries` | tests assert both cases |
| §9 ISO weeks, months, per-project | `core/src/aggregate.rs` | boundary tests |
| §9 Aggregation applied by the service | `service/src/interface.rs` | e2e |
| §6 Quick-add from the GUI — by click and by keystroke | `gui/src/app.rs` | `gui-smoke.sh`, XTest click and key |
| §11 Three tabs | `gui/src/app.rs` | `gui-smoke.sh` (screenshot) |
| §11 Week total large, at the top, nothing above it | `gui/src/app.rs`, 72px | `gui-smoke.sh` (text drawn; the value itself is not read back) |
| §14 Archived projects stay in totals | `rules.rs::set_archived` | service test |
| §2 Version field, no migration | `model.rs::STORE_VERSION` | storage test |
| §2 Corrupt store refused, not reset | `storage.rs` | storage test |
| §5 Methods 1 and 2 as one dialog, GUI + CLI | `core/src/parse.rs` (shared spellings), GUI dialog (`a` new / `e` edit), CLI wrappers | core/GUI tests + gui-smoke (open, cancel, save, typed text) |
| §10 CSV export, ISO 8601 local time, `HH:MM:SS` + minutes, `source`, GUI + CLI, week/month/all | `core/src/export.rs`, service `ExportCsv`, CLI `export`, GUI Export tab | core/service tests + e2e (65 checks) |
| §13 Core free of GUI/IPC/async/clock | `core/` | by construction |

### Specified but not built

- **§5 drag-to-trim.** Deferred by decision, not by omission.
- **§11 proportional bars** on the per-project rows. Rows show the time; the bar
  is not drawn.
- **§11 project rename and recolour.** Create and archive exist; rename and
  recolour do not, and the GUI's "New project" invents a numbered default name
  because there is no text field.
- **§8 split/merge in the GUI.** On the service and core, unreachable from the
  UI.
- **§11 tabs switched by keyboard.** Tabs respond to clicks; the shortcuts are
  not wired.

### Known defects and gaps

- **The timezone offset is resolved once at startup, fixed-offset only.** A
  named `TZ` such as `Europe/Rome` falls back to UTC, and a zone observing DST
  can bucket an entry into the wrong local day near a week boundary. §9 assumes
  correct local-time bucketing, so this is a real shortfall against the spec,
  not a nicety. A tz-database lookup per instant is the fix.
- **The GUI's "New project" name is a placeholder** (`Project 1`, `Project 2`).
  Functional but not what §11 asks for.
