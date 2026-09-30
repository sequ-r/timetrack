# TimeTrack requirements

Status: draft for discussion. Sections marked **[DECIDED]** were settled with
the author; **[OPEN]** needs an answer before the work is scoped.

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

**[DECIDED]** Edition 2024, gpui-ce for the GUI, ratatui for the CLI.

## 3. Data model (proposed)

Today an `Entry` is `{id, description, started_at, ended_at}`. Proposed:

```rust
pub struct Entry {
    pub id: String,
    pub project_id: String,        // was: no grouping at all
    pub description: String,       // optional note, not the label
    pub started_at: i64,           // ms since epoch, UTC
    pub ended_at: Option<i64>,     // None while running
    pub source: EntrySource,       // Manual | Timer | QuickAdd
    pub note: Option<String>,      // free-form, never parsed
}

pub enum EntrySource { Manual, Timer, QuickAdd }

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

## 4. The overlap question **[OPEN — needs an answer first]**

This is the most consequential thing in this document, and it is *new*: manual
entry makes overlapping entries normal. The current invariant — at most one
running entry — disappears the moment a user types "10:00–11:00" and "10:30–11:30"
for the same morning.

With overlap, "total time" has two defensible meanings:

| | Two overlapping entries of 1h | Meaning |
|---|---|---|
| **Sum of durations** | 2h | how much you worked on it (can exceed 24h in a day) |
| **Wall-clock union** | 1h | how much of your day it occupied (never exceeds 24h) |

Sum is what most people mean by "I spent three hours on X". Union is what a
stakeholder means by "you were working 3 hours". They disagree exactly when
entries overlap, which is precisely what manual entry encourages.

**Recommendation:** store enough to compute both (the raw intervals already do),
report **sum** as the headline number, and **warn when a day or week exceeds 24h**
of summed time. That catches the mistake without pretending the data is clean.

## 5. Manual entry **[OPEN — scope]**

The ways time should be enterable:

1. **Explicit start/end.** User picks both. Most precise, most typing.
2. **Duration, ending now.** "3 hours, just now." Fewest keystrokes.
3. **Duration, into the past.** "3 hours, ending at 14:00."
4. **Quick-add increments.** "Add 5 minutes" as a one-click action, per
   requirement in §6.
5. **Running timer → stop.** The existing model, kept if §7 says so.

The distinction that matters: (1) creates an arbitrary interval; (2)–(4) create
an interval ending at *now*. Both must handle **overlap** (§4).

Required behaviour:

- An entry must not end before it starts.
- Entries may overlap freely (§4). They must not be *rejected* for it.
- Adding a 5-minute slot should not require opening a dialog.
- Undo should be available for quick-add, at minimum — it is a one-click action
  and mistakes will happen.

## 6. Quick-add **[DECIDED as a requirement, details OPEN]**

"Quick add / remove in 5-minute slots." Open questions:

- Is 5 minutes fixed, or selectable (5 / 15 / 30 / 60)?
- Does the slot attach to the running entry, or create a new 5-minute entry?
  These are quite different features; the second is closer to a tally.
- Does quick-add apply to the *selected* project?

## 7. Running timer **[OPEN — keep or cut?]**

The original design centred on a running timer. §1 reframes it as secondary.
Options:

- **Keep it.** It is built, tested and working. Costs one running entry in the
  model and keeps `ended_at: Option` meaningful.
- **Cut it.** Simplifies the model: every entry is closed, no live ticking, no
  "did I forget to stop it" class of bug. The service loses its only reason to
  be long-lived.

**Recommendation: keep it, but demote it.** The `source` field means timer
entries are just another kind of entry, so keeping it does not complicate the
model much, and "I started something now" is a real flow worth one keystroke.

## 8. Editing **[DECIDED]**

- Change project, times, description of any entry.
- Delete.
- **Split** one entry into two at a chosen point.
- **Merge** adjacent or overlapping entries into one.
- All of the above must go through the service and be persisted; the GUI and CLI
  are clients.

Split and merge interact with overlap (§4): a merge of two overlapping entries
must decide what the result's interval is. Recommend: merged interval spans from
the earlier start to the later end, and the overlap is collapsed — which is a
union, not a sum, and will therefore disagree with §4's headline number. This
needs to be an explicit, documented behaviour rather than an accident.

## 9. Weekly and monthly totals **[OPEN — scope]**

The stated goal: "how much time I spend on a particular project any given week
and month."

Needed:

- Per-project totals for a week, and for a month.
- A single total for the same periods.
- Navigation between weeks/months; "this week", "last week".
- **Week boundaries need defining**: ISO weeks (Monday start) is the obvious
  default, but it must be a stated decision, because it determines what a
  "weekly total" means at a month boundary.

Do these aggregate live in the service or in the clients? Recommend the
**service**: one implementation, both UIs agree, and it can be tested once.

## 10. Export **[DECIDED as a requirement, format OPEN]**

Export to something a stakeholder can open.

- **CSV vs XLSX.** CSV opens in Excel, is trivially diffable, needs no library,
  and any spreadsheet tool reads it. XLSX allows multiple sheets and formatting.
  Recommend **CSV first**; add XLSX only if someone actually asks for a
  formatted multi-sheet workbook.
- Must be exportable **from the GUI**, not only the CLI — the stakeholder
  workflow should not require opening a terminal.
- Scope of an export: current week, a chosen week, a chosen month, or all time.
- Should include the `source` column, so hand-entered time is visible.

## 11. Non-goals

Stating these now, to keep them from arriving later as "small features":

- Multiple users, or a shared/synced database.
- Accounts, login, sync, cloud storage.
- A server component.
- Invoicing, rates, currencies.
- Recurring or scheduled entries.
- Timezone-aware "work hours" or overtime logic.

## 12. Architecture consequences

The service/CLI/GPUI split is unaffected and stays. What changes:

| Area | Impact |
|---|---|
| `timetrack-core` | `Entry` gains `project_id`, `source`, `note`. New `Project` type. New split/merge and time-edit operations in the state machine. Overlap is no longer an error. |
| Store format | Schema change. Needs a migration for existing `store.json` files, or a version bump with a clear "unsupported format" message. |
| D-Bus protocol | New methods: create/update entry, create/list projects, split, merge, weekly/monthly aggregates. Existing `Snapshot` needs the new fields. |
| Service | Becomes the aggregation point (§9). |
| GUI | Manual-entry dialog, project selector, weekly/monthly view. |
| CLI | New subcommands for the same. |

Both crates keep their existing properties: core stays free of GUI/IPC/async,
and the state machine keeps taking an explicit timestamp so it stays testable
without a clock.

## 13. Open questions, collected

1. **§4** — sum or wall-clock union as the headline number?
2. **§5** — which manual entry methods are in scope for v1?
3. **§6** — is quick-add an increment on the current entry, or its own tally?
4. **§7** — keep the running timer or cut it?
5. **§9** — ISO weeks? Does aggregation live in the service? (recommend yes/yes)
6. **§10** — CSV or XLSX? (recommend CSV)
7. Store format: migrate existing files, or start clean?
