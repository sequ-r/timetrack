# Build plan

Derived from `REQUIREMENTS.md`, which is fully agreed. Ordered so that each
increment leaves the app working and is independently verifiable.

The ordering rule: **core before service, service before UI.** The core holds
the rules and has no I/O, so it can be tested exhaustively and cheaply. The
service aggregates (§9) so both UIs agree. The UIs come last because they are
where requirements are still cheapest to change.

## Increment 1 — core model and rules

`timetrack-core` only. No service, no UI, no protocol changes. This is the
foundation everything else stands on, and the rules it encodes are the ones
both UIs will depend on.

1. **Model** (§3, §7)
   - `Entry { id, project_id, description, started_at, ended_at: i64, source,
     note }` — `ended_at` non-optional; the timer is gone.
   - `Project { id, name, colour, archived }`.
   - `EntrySource { Manual, QuickAdd }`.
   - `Store` gains a version field (§2) so a future migration has something to
     branch on.
   - `duration_ms()` loses its `now` argument — every entry is closed.

2. **Rules** (§4, §5, §6, §8)
   - `create_entry` — arbitrary interval, overlap allowed.
   - `quick_add` — 5/15/30/60, ends at now, new entry each time.
   - `set_times` — move either endpoint, earlier, reversible. Never deletes.
   - `set_project`, `set_description`, `delete_entry`.
   - `split`, `merge` — merge takes the union of intervals, so merging
     overlapping entries shrinks the total (§8).
   - Validation: an entry may not end before it starts. Overlap is **not** an
     error.

3. **Aggregation** (§4, §9)
   - Sum of durations, per project and overall.
   - ISO weeks (Monday start) and calendar months, local time. Timezone
     conversion happens here and nowhere else.
   - A day exceeding 24h of summed time is *flagged*, never rejected.

Verification: unit tests in `timetrack-core`, no I/O, no clock — the rules take
an explicit timestamp and stay deterministic. This is where the overlap, merge,
and week-boundary edge cases get pinned down.

## Increment 2 — service

1. Store the new model; drop the `running` concept, the start/stop/cancel
   methods, and `AlreadyRunning`/`NotRunning` errors (§7).
2. Add D-Bus methods for entry CRUD, projects, split/merge.
3. Add weekly/monthly aggregates to the service, so both UIs agree (§9).
4. CSV export (§10).

Verification: the existing D-Bus e2e script, extended — it already drives a
real service over a private session bus and checks persistence across restarts.

## Increment 3 — GUI

Rebuild the window as the agreed Home tab (§11):

1. Week total, large, at the top, nothing above it.
2. Quick-add buttons — 5 / 15 / 30 / 60.
3. Undo, immediately after.
4. Per-project totals for the week, each with a proportional bar.

Then Projects (create/rename/recolour/archive) and Export (CSV) tabs. Keyboard
navigation throughout, since every other action is a keystroke.

Verification: run it on a real session. Xvfb proved the process starts but
cannot prove rendering — the earlier caveat in the README applies.

## Increment 4 — CLI

Replace `start`/`stop`/`cancel` (§7) with the manual entry surface (§5), plus
`status`, the per-project week totals, and export. Methods 1 and 2 share one
dialog in the GUI; on the command line they are one subcommand with a flag.

## Deferred, deliberately

- **Drag-to-trim** for adjusting times (§5).
- **A dedicated entry-list tab** (§11) — editing lives behind the per-project
  rows until it needs more room.

## Risks

- **Timezone handling in aggregation** is the most likely source of off-by-one
  bugs, particularly around DST and month boundaries. The ISO-week rule is
  stated precisely in §9 for that reason, and it belongs in one place.
- **Merge reducing the total** (§8) will look like a bug to a user who has not
  read the confirmation text. The confirmation has to say the duration will
  shrink by the overlap.
