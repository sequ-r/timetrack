# TODO

Derived from `REQUIREMENTS.md` §15, `PLAN.md` remaining work, and a code-health
survey (2026-10-05). Ordered by value. Check off as done.

## P0 — spec gaps (blocks workflows)

- [x] **1. CSV export end-to-end (§10)** — DONE 2026-10-05: `core/src/export.rs` + service `ExportCsv` + CLI `timetrack export --scope week|month|all [--out FILE]` + GUI Export week/month/all buttons writing `~/timetrack-export-<scope>.csv`. E2e: 65 checks incl. header/row/scope-refusal.
  - Scope: `timetrack-core/src/aggregate.rs`, `timetrack-service/src/interface.rs`, `timetrack-proto/src/lib.rs`, GUI `app.rs:950`, CLI `main.rs`, `scripts/e2e.sh`.
  - Format: header row, ISO-8601 local timestamps, duration as `HH:MM:SS` + minutes, `source` column. Scopes: week / month / all.
  - GUI: file picker + write. CLI: `timetrack export --week/--month --out file`.
  - Accept: e2e writes a file with the spec'd columns.

- [ ] **2. GUI manual-entry dialog (§5 methods 1+2, one form)**
  - Scope: `timetrack-gui/src/app.rs:391` (key handling), shared validation in `timetrack-core/src/rules.rs`.
  - Fields: start/end, "end = now" default. Rules: no end-before-start, overlap allowed.
  - Accept: create + shorten an entry without the CLI.

- [ ] **3. Real timezone lookup per instant (§9)**
  - Scope: `timetrack-service/src/interface.rs:284` (`resolve_offset`), `timetrack-core/src/aggregate.rs`.
  - Today: fixed-offset `TZ` resolved once at startup; named zones (e.g. `Europe/Rome`) fall back to UTC, mis-bucketing near DST/week boundaries.
  - Fix: tz-database lookup per timestamp (e.g. `chrono-tz`/`jiff`).
  - Accept: DST-boundary test buckets correctly for a named zone.

## P1 — correctness / data safety

- [ ] **4. Fix persist-failure path**
  - Scope: `timetrack-service/src/interface.rs:153` (`store.load().unwrap_or_default()` on save failure silently resets to empty).
  - Fix: return error, keep in-memory state, surface to client.
  - Accept: unit test for double-failure keeps data.

- [ ] **5. Fix GUI Projects total disagreement**
  - Scope: `timetrack-gui/src/app.rs:855` (all-time recomputed client-side from recent-only `snapshot.entries`; week comes from service).
  - Fix: use service aggregates for both.
  - Accept: large store shows agreeing totals.

- [ ] **6. Typed D-Bus errors**
  - Scope: `timetrack-service/src/main.rs:32` (`to_dbus_err` flattens to string), `timetrack-proto/src/lib.rs`, `timetrack-gui/src/app.rs:109`.
  - Fix: typed error enum, keep backward-compat strings.
  - Accept: GUI distinguishes no-service vs version-mismatch vs validation without string parsing.

## P1 — reachable features (in core/service, not in UI)

- [ ] **7. Split/merge in GUI + CLI (§8)**
  - Scope: D-Bus `Split`/`Merge` exist; no `Action` variant in `timetrack-gui/src/app.rs:54`, no CLI subcommand in `timetrack-cli/src/main.rs`.
  - Include merge confirmation stating the total shrinks by the overlap (`timetrack-core/src/rules.rs:486`).
  - Accept: e2e + gui-smoke cover both.

- [ ] **8. Project rename / recolour / text field (§11)**
  - Scope: `timetrack-gui/src/app.rs:822` invents `Project N`; `update_project` exists in core but is unreachable via D-Bus/GUI/CLI.
  - Fix: D-Bus `UpdateProject`, GUI text field, CLI `project rename`/`recolour`.
  - Accept: projects can be named, no numbered placeholders.

- [ ] **9. CLI parity: `set_project`, `set_text`, `split`, `merge`, `unarchive`**
  - Scope: `timetrack-cli/src/main.rs`, `scripts/e2e.sh`.
  - Accept: subcommands exist + e2e coverage for each guard.

## P2 — UX polish

- [ ] **10. Tab keyboard shortcuts + proportional bars (§11)**
  - Scope: `timetrack-gui/src/app.rs:391` (`on_key` handles `1-4/u/d/h/l/j/k/q` only, tabs click-only), `render_week_by_project`.
  - Accept: shortcuts switch tabs; per-project rows show proportional bars.

- [ ] **11. Destructive-action confirmations + un-archive**
  - Scope: `timetrack-gui/src/app.rs` (bare `d` delete, archive without un-archive, no merge shrink warning — see `PLAN.md` risks).
  - Accept: delete/merge confirm, merge text states shrink amount, archive reversible.

## P3 — engineering hygiene

- [ ] **12. Fix clippy + gate it in CI**
  - `cargo clippy --workspace -- -D warnings` fails at `timetrack-core/src/model.rs:133` (`unnecessary_sort_by`); no `[lints]`, no clippy/fmt job in `.github/workflows/release.yml`.
  - Accept: sort fixed, `clippy` + `fmt --check` in CI.

- [ ] **13. E2e / gui-smoke coverage**
  - `scripts/e2e.sh` skips split/merge behaviour, `set_*` guards, month nav, export. `scripts/gui-smoke.sh` never reads back the total value, no undo/delete/tab/Export interaction.
  - Add: service DST test, storage empty-file test, month navigation.
  - Accept: new paths covered in scripts.
