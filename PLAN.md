# Build plan

Derived from `REQUIREMENTS.md`, which is fully agreed. Ordered so that each
increment leaves the app working and is independently verifiable.

The ordering rule: **core before service, service before UI.** The core holds
the rules and has no I/O, so it can be tested exhaustively and cheaply. The
service aggregates (§9) so both UIs agree. The UIs come last because they are
where requirements are still cheapest to change.

**Status: increments 1 and 2 are built, and increment 3 and 4 are partly
built.** What follows marks each increment done, partial or not started, and
lists what is left. `REQUIREMENTS.md` §15 tracks the same gap from the
specification's side.

## Increment 1 — core model and rules — **done**

`timetrack-core` only. No service, no UI, no protocol changes. This is the
foundation everything else stands on, and the rules it encodes are the ones
both UIs depend on.

1. **Model** (§3, §7) — done, `model.rs`
   - `Entry { id, project_id, description, started_at, ended_at: i64, source,
     note }` — `ended_at` non-optional; the timer is gone.
   - `Project { id, name, colour, archived }`.
   - `EntrySource { Manual, QuickAdd }`.
   - `Store` gains a version field (§2) so a future migration has something to
     branch on.
   - `duration_ms()` loses its `now` argument — every entry is closed.

2. **Rules** (§4, §5, §6, §8) — done, `rules.rs`
   - `create_entry` — arbitrary interval, overlap allowed.
   - `quick_add` — 5/15/30/60, ends at now, new entry each time.
   - `set_times` — move either endpoint, earlier, reversible. Never deletes.
   - `set_project`, `set_text`, `delete_entry`.
   - `split`, `merge` — merge takes the union of intervals, so merging
     overlapping entries shrinks the total (§8).
   - Validation: an entry may not end before it starts. Overlap is **not** an
     error.

3. **Aggregation** (§4, §9) — done, `aggregate.rs`
   - Sum of durations, per project and overall.
   - ISO weeks (Monday start) and calendar months, local time. Timezone
     conversion happens here and nowhere else.
   - A day exceeding 24h of summed time is *flagged*, never rejected.

4. **Storage** (§2) — done, `storage.rs`
   - Atomic saves: a sibling temp file, `fsync`, then rename. A crash mid-write
     leaves the previous good file rather than a truncated one.
   - A corrupt or too-new store is an **error**, not a silent reset. Quietly
     discarding tracked time would be the worst available failure.

Verification: 72 unit tests, no I/O, no clock — the rules take an explicit
timestamp and stay deterministic. This is where the overlap, merge, and
week-boundary edge cases are pinned down.

Four bugs were found this way and are worth remembering, because each looked
fine in review: `local_day_of` had the offset sign inverted; `month_of` and
`iso_week_of` used year arithmetic that returned 1970 for a 2024 date;
`quick_add` delegated to `create_duration`, which hardcoded `EntrySource::Manual`
and so made every quick-add un-undoable; and `split_entry` wrote its first half
to a temporary that was never stored.

## Increment 2 — service — **done**

1. Store the new model; drop the `running` concept, the start/stop/cancel
   methods, and `AlreadyRunning`/`NotRunning` errors (§7). Done: the interface
   is `org.sequ.timetrack.Entries` and carries none of them.
2. Add D-Bus methods for entry CRUD, projects, split/merge. Done.
3. Add weekly/monthly aggregates to the service, so both UIs agree (§9). Done:
   `Snapshot` carries the week, month, all-time and the over-24h day list.
4. CSV export (§10). **Not done.** No code writes a CSV anywhere in the tree.
5. **Added, not in the original plan:** a fresh store seeds one `General`
   project. Without it a new install had nothing to attribute time to, so every
   quick-add was inert until the user went and made a project. Only on a
   genuinely empty store, so an archived-only store is never re-seeded over.

The service holds exactly one clock read and one timezone resolution, in one
place, so every entry's `ended_at` is traceable to a single instant per call.

Verification: `scripts/e2e.sh`, 60 checks — it drives a real service over a
private bus and covers all four add methods, the three removal operations,
overlap being summed, the 24h warning, persistence across restart, and id
continuation.

That script needs an explicit bus config. A plain `dbus-daemon --session` reads
`standard_session_servicedirs` and auto-activates whatever service is installed
on the host, so a stale binary can win the name and look exactly like a test
failure.

## Increment 3 — GUI — **mostly done**

Built:

1. Week total, large, at the top, nothing above it. 72px, explicitly sized
   rather than using the largest step in gpui's scale, which was too small.
2. Quick-add buttons — 5 / 15 / 30 / 60, each a real click target.
3. Undo, reachable from the keyboard and the `+` marker on quick-added rows.
4. Per-project totals for the week, sorted longest first. **The proportional bar
   from §11 is not drawn.**
5. Three tabs: Home, Projects (create, archive), Export (column list and a
   preview — it writes no file).
6. Deletion, project switching, and a version-mismatch message that distinguishes
   "no service" from "wrong service", which need different fixes.

Not built:

- The manual-entry dialog (§5 methods 1 and 2, which share one form). There is
  no way to type an interval in the GUI at all yet; time can only be quick-added.
- Split and merge (§8). On the service, unreachable from the UI.
- A text field for naming a project, so "New project" invents `Project 1`.
- Tab switching by keyboard, and the merge confirmation that must warn the total
  will shrink.

The keyboard needs one non-obvious thing and it is easy to get wrong: gpui
dispatches key events along the ancestor path of the **focused** node, so a root
element with `on_key_down` but no `FocusHandle` has a dead keyboard while
looking perfectly alive. Tabs still respond, because those are click handlers.
The root is given an id, a `FocusHandle`, and focus on the first frame.

Verification: `scripts/gui-smoke.sh`, 10 checks. It starts a private bus, a real
service and the real GUI on Xvfb, screenshots the window, clicks the 15-minute
quick-add button through the X11 XTest extension, and asserts that the service
then holds a 15-minute quick add, that the week total on screen re-rendered, and
that `1` and `q` reach the app as a quick add and a quit. Pixels are the
evidence: a rendered frame is the only thing that proves the renderer ran rather
than the window merely existing.

The headless keyboard check needed two things that were missing the first time
round: an Xvfb server has no keymap until `setxkbmap` gives it one (without one
every keysym lookup returns 0), and with no window manager the input focus is
PointerRoot, so the pointer has to be over the window for a keystroke to arrive
at all. Both are properties of the test rig, not of the app — on a real display
a window manager handles the focus. **The keyboard is verified now**, which it
was not before.

One more trap, for whoever runs this next: Xvfb implements no DRI3, and Mesa's
GPU drivers need it to present, so the app needs a software Vulkan driver
(Arch: `vulkan-swrast`; Debian/Ubuntu: `mesa-vulkan-drivers`). Without one the
window opens, is the right size, and stays black forever — and gpui does not
paint its first frame until an event arrives, so the script nudges the pointer
into the window before it starts looking.

### The toolkit moved to upstream gpui

The GUI was built on **gpui-ce**, the community fork, for one reason: the
published `gpui` crate carried no platform backend, so a crates.io-only
dependency produced a test platform that could not open a window, and gpui-ce
shipped the real one in a separate `gpui_platform` crate. Upstream 0.2.2 carries
the Linux backends behind its default features, so the fork bought nothing any
more. What the migration changed:

- `gpui = "0.2.2"` from crates.io replaces both git dependencies, and the
  flatpak no longer needs a checkout of the toolkit.
- The entry point is `Application::new()`, which picks the backend from the
  session's environment, instead of `gpui_platform::application()`.
- Exactly one call site in this repository changed:
  `FocusHandle::focus(window, cx)` lost its `cx` argument. The `image` pin in
  the GUI's manifest went away with it — that pin existed because gpui-ce's
  Wayland backend called `ImageBuffer::into_raw_bgra`, which upstream 0.2.2
  does not.

The lockfile moves more than the toolkit. gpui-ce was *ahead* of upstream on
several UI dependencies — wgpu 29 and cosmic-text 0.19 against upstream's
blade-graphics and cosmic-text 0.14 — so those come back down to what upstream
pins. Nothing here depends on either; the GUI uses boxes, text, colour and
click handlers only.

## Increment 4 — CLI — **done**

`start`/`stop`/`cancel` (§7) are replaced by one subcommand per entry method —
`add`, `duration`, `past`, `quick` — plus `status`, `list`, `project` and
`archive`. The three removal operations are three separate subcommands
(`shorten`, `undo`, `delete`) because they are three different things to do to
an entry, and collapsing them would hide the distinction §5 is precise about.

Methods 1 and 2 are one subcommand with a flag, as planned. Timestamps take
`-90` for "90 minutes ago" or `09:30` for today; durations take `90m`, `1h30m`
or `45s`.

**Not built:** export (§10).

## Remaining work, in the order it is worth doing

1. **CSV export** (§10). **Done 2026-10-05:** `core/src/export.rs`, service
   `ExportCsv`, CLI `timetrack export --scope week|month|all [--out FILE]`,
   GUI Export week/month/all buttons. E2e covers header, rows, bad scope.
2. **The manual-entry dialog** in the GUI (§5 methods 1 and 2). Right now the
   GUI can only quick-add, so an over-estimated entry has no way to be
   corrected except the CLI.
3. **A real timezone database lookup per instant.** §9's local-time bucketing
   assumes this. A named `TZ` currently falls back to UTC, and a DST-observing
   zone can mis-bucket near a week boundary. This is the likeliest remaining
   source of wrong numbers.
4. **Split and merge in the GUI** (§8), with the confirmation that states the
   total will shrink by the overlap.
5. **A project text field**, so projects can be named rather than numbered.
6. **Tab shortcuts**, and the proportional bars from §11.

## Deferred, deliberately

- **Drag-to-trim** for adjusting times (§5).
- **A dedicated entry-list tab** (§11) — editing lives behind the per-project
  rows until it needs more room.

## Risks

- **Timezone handling in aggregation** is the most likely source of off-by-one
  bugs, particularly around DST and month boundaries. The ISO-week rule is
  stated precisely in §9 for that reason, and it belongs in one place. *This
  risk has already materialised once* — see item 3 above.
- **Merge reducing the total** (§8) will look like a bug to a user who has not
  read the confirmation text. The confirmation has to say the duration will
  shrink by the overlap, and it does not exist yet, so the feature is unwired
  rather than misused.
- **A stale service binary outliving an interface rename** cost the most time
  during the last round: a GUI speaking `.Entries` against a service still
  answering to `.Timer` looks like a dead app, not a version mismatch. The GUI
  now names the problem. The README says to reinstall the service after
  upgrading.
