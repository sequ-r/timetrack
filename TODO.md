# TODO

Derived from `REQUIREMENTS.md` §15, `PLAN.md` remaining work, and a code-health
survey (2026-10-05). Ordered by value. Check off as done.

## P0 — spec gaps (blocks workflows)

- [x] **1. CSV export end-to-end (§10)** — DONE 2026-10-05: `core/src/export.rs` + service `ExportCsv` + CLI `timetrack export --scope week|month|all [--out FILE]` + GUI Export week/month/all buttons writing `~/timetrack-export-<scope>.csv`. E2e: 65 checks incl. header/row/scope-refusal.
  - Scope: `timetrack-core/src/aggregate.rs`, `timetrack-service/src/interface.rs`, `timetrack-proto/src/lib.rs`, GUI `app.rs:950`, CLI `main.rs`, `scripts/e2e.sh`.
  - Format: header row, ISO-8601 local timestamps, duration as `HH:MM:SS` + minutes, `source` column. Scopes: week / month / all.
  - GUI: file picker + write. CLI: `timetrack export --week/--month --out file`.
  - Accept: e2e writes a file with the spec'd columns.

- [x] **2. GUI manual-entry dialog (§5 methods 1+2, one form)** — DONE 2026-10-05: `a` new (end=`now` prefilled) / `e` edit (empty keeps endpoint, rewritten text via new `SetText` D-Bus method); spellings shared with CLI in `core/src/parse.rs` (+`now` keyword, `format_local_hm`); 13 core + 10 GUI tests; gui-smoke 15 checks incl. dialog paint/cancel/save + no-motion refresh. Also fixed: `Entity::update` never repaints without `cx.notify()`, so every mutation site + conditional poll-loop notify; one poll loop (was spawned per frame).
  - Scope: `timetrack-gui/src/app.rs:391` (key handling), shared validation in `timetrack-core/src/rules.rs`.
  - Fields: start/end, "end = now" default. Rules: no end-before-start, overlap allowed.
  - Accept: create + shorten an entry without the CLI.

- [x] **3. Real timezone lookup per instant (§9)** — DONE 2026-10-06: `core/src/tz.rs` (`Tz` + `chrono-tz` per-instant offsets, fixed-point `day_start`); `aggregate`/`rules`/`export`/`parse` take `&Tz`; service `Tz::from_env` + snapshot `tz` name; CLI/GUI `Tz::from_snapshot`. DST tests: Rome +1/+2, transition days, Sunday→Monday week-split, export wall-clock. Live: `TZ=Europe/Rome` reports correct offset (was UTC fallback).
  - Scope: `timetrack-service/src/interface.rs:284` (`resolve_offset`), `timetrack-core/src/aggregate.rs`.
  - Today: fixed-offset `TZ` resolved once at startup; named zones (e.g. `Europe/Rome`) fall back to UTC, mis-bucketing near DST/week boundaries.
  - Fix: tz-database lookup per timestamp (e.g. `chrono-tz`/`jiff`).
  - Accept: DST-boundary test buckets correctly for a named zone.

## P1 — correctness / data safety

- [x] **4. Fix persist-failure path** — DONE 2026-10-06: `mutate` returns the save error and keeps in-memory state (removed `store.load().unwrap_or_default()` reset); next successful save heals a transient failure. Test: `a_failed_save_keeps_in_memory_state` (double-failure keeps seed + old + unpersisted entries, then heals on disk).
  - Scope: `timetrack-service/src/interface.rs:153` (`store.load().unwrap_or_default()` on save failure silently resets to empty).
  - Fix: return error, keep in-memory state, surface to client.
  - Accept: unit test for double-failure keeps data.

- [x] **5. Fix GUI Projects total disagreement** — DONE 2026-10-06: snapshot gains service-computed `all: TotalsView` (`core::all_totals`); Projects tab reads both columns via `Snapshot::project_totals` (old-service fallback to entry sum). Tests: core/service/proto agreement + truncated-entries regression; e2e block grows the store by 30 entries and compares `all` against the entries array (67 checks).
  - Scope: `timetrack-gui/src/app.rs:855` (all-time recomputed client-side from recent-only `snapshot.entries`; week comes from service).
  - Fix: use service aggregates for both.
  - Accept: large store shows agreeing totals.

- [x] **6. Typed D-Bus errors** — DONE 2026-10-06: `proto::ServiceError` (`Rule`/`UnknownExportScope`/`Storage`/`Other`, byte-identical Displays) travels as JSON in the `Failed` body; `proto::ClientError` (`Service`/`NoService`/`VersionMismatch`/`Transport`) triages by D-Bus error NAME. Service methods return `ServiceError`, `to_dbus_err` encodes it; GUI `StatusMsg`/`StatusKind` dispatches by variant (adversarial-name tests); CLI/TUI unchanged via `?` conversion. Tests: proto triage + round-trips, service wire test, GUI mapping tests; e2e 67 checks pin the unchanged wordings live.
  - Scope: `timetrack-service/src/main.rs:32` (`to_dbus_err` flattens to string), `timetrack-proto/src/lib.rs`, `timetrack-gui/src/app.rs:109`.
  - Fix: typed error enum, keep backward-compat strings.
  - Accept: GUI distinguishes no-service vs version-mismatch vs validation without string parsing.

## P1 — reachable features (in core/service, not in UI)

- [x] **7. Split/merge in GUI + CLI (§8)** — DONE 2026-10-06: GUI `s` split prompt (typed moment) + `m` merge confirm banner (count/project/delta, `m` confirms); CLI `split ID --at MOMENT` + `merge IDS... [--yes]` (TTY prompt, off-terminal refusal); shared `merge_shrink_ms`/`format_merge_delta` in core. E2e: halves, touching + overlap merges, 5 refusal guards (84 checks). Gui-smoke: prompt/confirm paint + keyboard split/merge with count/total asserts.
  - Scope: D-Bus `Split`/`Merge` exist; no `Action` variant in `timetrack-gui/src/app.rs:54`, no CLI subcommand in `timetrack-cli/src/main.rs`.
  - Include merge confirmation stating the total shrinks by the overlap (`timetrack-core/src/rules.rs:486`).
  - Accept: e2e + gui-smoke cover both.

- [x] **8. Project rename / recolour / text field (§11)** — DONE 2026-10-06: `core::update_project` exported + tested (new `BlankProjectName` guard); D-Bus `UpdateProject` (`""`/`-1` keep sentinels); CLI `rename` (local blank refusal) + `recolour` (strict RRGGBB); GUI naming prompt (`n`/`r`, prefilled) replacing invented `Project N` + colour dot on rows. E2e: rename/recolour/5 guards with snapshot asserts (94 checks). Gui-smoke: prompt paint + keyboard create/rename via duplicate-proof.
  - Scope: `timetrack-gui/src/app.rs:822` invents `Project N`; `update_project` exists in core but is unreachable via D-Bus/GUI/CLI.
  - Fix: D-Bus `UpdateProject`, GUI text field, CLI `project rename`/`recolour`.
  - Accept: projects can be named, no numbered placeholders.

- [x] **9. CLI parity: `set_project`, `set_text`, `split`, `merge`, `unarchive`** — DONE 2026-10-06: CLI `set-text`/`set-project`/`unarchive` (split/merge landed in task 7); new `SetProject` service→D-Bus→client path (the TODO scope missed it — no such method existed) with typed guards. E2e: self-contained scratch-entry section (zero net totals) + snapshot asserts for unarchive, all guards (107 checks).

## P2 — UX polish

- [x] **10. Tab keyboard shortcuts + proportional bars (§11)** — DONE 2026-10-06: `Tab`/`Shift+Tab` cycles the three tabs via pure `cycle_tab` (dialog keeps `Tab` for fields); per-project week rows gain a proportional bar (`bar_width`, longest fills 120px track). Footer documents `tab tabs`. Tests: cycle both directions + bar proportions; gui-smoke asserts tab-away change and 3-press return to pixel-identical Home.
  - Scope: `timetrack-gui/src/app.rs:391` (`on_key` handles `1-4/u/d/h/l/j/k/q` only, tabs click-only), `render_week_by_project`.
  - Accept: shortcuts switch tabs; per-project rows show proportional bars.

- [x] **11. Destructive-action confirmations + un-archive**
  - Scope: `timetrack-gui/src/app.rs` (bare `d` delete, archive without un-archive, no merge shrink warning — see `PLAN.md` risks).
  - Accept: delete/merge confirm, merge text states shrink amount, archive reversible.

## P3 — engineering hygiene

- [x] **12. Fix clippy + gate it in CI**
  - `cargo clippy --workspace -- -D warnings` fails at `timetrack-core/src/model.rs:133` (`unnecessary_sort_by`); no `[lints]`, no clippy/fmt job in `.github/workflows/release.yml`.
  - Accept: sort fixed, `clippy` + `fmt --check` in CI.

- [x] **13. E2e / gui-smoke coverage**
  - `scripts/e2e.sh` skips split/merge behaviour, `set_*` guards, month nav, export. `scripts/gui-smoke.sh` never reads back the total value, no undo/delete/tab/Export interaction.
  - Add: service DST test, storage empty-file test, month navigation.
  - Accept: new paths covered in scripts.
