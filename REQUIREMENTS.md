# TimeTrack requirements

Status: draft for discussion. Sections marked **[DECIDED]** were settled with
the author; **[OPEN]** needs an answer before the work is scoped.

All structural questions are now closed. Totals are **sum of durations** (§4)
and the **running timer is cut** (§7), which together simplify the model
substantially: every entry is closed, and the service stops being a state
machine owner. Quick-add durations, week boundaries, export format, storage
migration and the GUI layout are all settled too (§6, §9, §10, §2, §11).

What remains open is sizing and naming, listed in §14. The app is ready to
scope.

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
increments and (retained?) a running timer.

**[DECIDED]** The service owns all state; the GUI and CLI are clients. This
already works and is not up for renegotiation.

**[DECIDED] Start clean on storage.** The existing `store.json` files are
throwaway — they were created while developing the timer model, and they hold
entries whose shape is now wrong. No migration is written; an unreadable store
produces a clear "unsupported or corrupt store" message rather than a crash.
A version field is added now so a future migration has something to branch on.

**[DECIDED]** Edition 2024, gpui-ce for the GUI, ratatui for the CLI.

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

## 5. Manual entry **[OPEN — scope for v1]**

The ways time should be enterable:

1. **Explicit start/end.** User picks both. Most precise, most typing.
2. **Duration, ending now.** "3 hours, just now." Fewest keystrokes.
3. **Duration, into the past.** "3 hours, ending at 14:00."
4. **Quick-add increments.** "Add 5 minutes" as a one-click action, per
   requirement in §6.
5. ~~**Running timer → stop.**~~ Removed — see §7.

The distinction that matters: (1) creates an arbitrary interval; (2)–(4) create
an interval ending at *now*. Both must handle **overlap** (§4).

Required behaviour:

- An entry must not end before it starts.
- Entries may overlap freely (§4). They must not be *rejected* for it.
- Adding a 5-minute slot should not require opening a dialog.
- Undo should be available for quick-add, at minimum — it is a one-click action
  and mistakes will happen.

## 6. Quick-add **[DECIDED as a requirement, details OPEN — now load-bearing]**

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

1. **A large clock.** Replaces the live timer (§7) as the window's focal
   point. The question is what it shows, and it cannot be a ticking stopwatch —
   there is no timer. Recommended: the **current wall-clock time**, large. It
   anchors the layout, it is genuinely useful at a glance, and it needs no
   ticking.

   Alternative if it feels decorative: the week total instead, with the clock
   demoted to the title bar.

2. **This week's total.** Large, unambiguous, the sum of every entry this ISO
   week (§9). Sitting directly under the clock, it answers the question the app
   exists for in one glance.

3. **Quick-add buttons.** The four durations from §6 (5 / 15 / 30 / 60), each
   applying to whichever project is selected.

4. **Per-project totals for this week**, one row per project: name, time, and
   ideally a bar proportional to the total so the distribution is readable
   without reading every number.

5. **Undo**, immediately after the quick-add buttons — the last thing pressed.

### Other tabs

Named tentatively; content follows from §5/§8/§10:

- **Entries** — the list, filterable by project and date, fully editable
  (rename, retime, split, merge, delete).
- **Projects** — create, rename, recolour, archive.
- **Export** — CSV, for the chosen period.

### Notes

- Project selection is global window state, not per-tab, so quick-add always
  has an unambiguous target.
- Tabs are switched with keyboard shortcuts as well as clicks, since every
  other action in this app is a keystroke.

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

Answered, in the order asked:

- **Totals** → sum of durations (§4), with a >24h/day warning.
- **Running timer** → cut (§7); quick-add and merge replace it.
- **Quick-add durations** → 5 / 15 / 30 / 60 (§6).
- **Week boundaries** → ISO weeks; aggregation in the service (§9).
- **Export format** → CSV (§10).
- **Store migration** → start clean (§2).
- **GUI layout** → tabbed, with Home as the dashboard (§11).

Still open — all sizing or naming, none architectural:

1. **§5** — do all five manual entry methods ship in v1, or does v1 ship a
   subset and defer the rest?
2. **§11** — what the large clock *shows*. Wall-clock time is recommended; the
   week total is the alternative. Cosmetic, but it is the most prominent element
   in the window, so worth a deliberate answer rather than a default.
3. **§11** — tab names and shortcut keys are placeholders.
4. Do archived projects keep their entries in past totals, or drop out of them?
   Recommended: entries are never deleted, only archived, and archived projects
   remain in historical totals — a report should not change because a project
   was tidied away later.

Everything structural is decided. The app is ready to scope.
