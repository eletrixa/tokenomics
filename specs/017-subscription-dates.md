# Spec 017 — Subscription dates from the ledger (third data plane)

Status: **Active**

Plan: `plans/001-subscription-lifecycle/` (PRD `10-PRD.md` R1–R18; decisions Q1=a, Q6=dates-only
recorded in `20-synthesis.md`). This spec covers Feature 1 only — the enable/disable driver lives
outside this repo.

## Motivation

Tokenomics shows per-account *usage* but nothing about the *money clock*. In July, a cancelled
Max account looked identical to an active one on the board — the cancellation was invisible until
the overlay started failing. A git-versioned subscription ledger (TOML, external repo, source of
truth for status + dates) already exists; this wave renders it: **subscription start, renewal
countdown, and — for a cancelled-but-paid-through account — the end date**, on each account's
header line.

This is a **third data plane** (billing/lifecycle metadata) beside the two existing ones (local
ccusage usage, opt-in overlay limits) — same discipline: provenance-tagged, read-only,
degrade-to-blank, never a guess.

## Behaviour

### A. Ledger plane (`src/ledger.rs`)

- New module `src/ledger.rs`: a pure `parse(&str) -> …` core producing
  `Subscription { id, status, purchased, renews, cancelled_on, paid_through }`
  (dates `Option<jiff::civil::Date>`; `status` a two-variant enum `Active | Cancelled`), plus a
  thin loader and `LedgerProvenance`. Read-through only: never persisted to SQLite, never fetched
  from any network source. **TUI-only** — the collector never reads the ledger.
- Ledger schema (external contract, `[[subscription]]` array of tables): `id` (required, join
  key), `status` (required, `"active"` | `"cancelled"` — anything else, including `"canceled"`,
  is a malformed row: blanked per row-degradation below, reported with reason by `doctor`, never
  coerced), optional TOML local-date fields `purchased` (current-period start; bumped on each
  renewal), `renews` (current-period end), `cancelled_on`, `paid_through` (access lapses after
  this date; only meaningful when cancelled). A `first_purchased` field may appear upstream
  later — the parser tolerates its presence or absence but this wave does not render it (Q1=a).
- **The ledger's `account` field (a raw email) is never deserialized** — the field does not
  exist on the struct; unknown fields are ignored (no `deny_unknown_fields`). No ledger-derived
  string is ever logged. Only `id`, `status`, and dates cross into this crate.
- Tolerant per-row degradation: one malformed row (bad status, bad date, missing id) drops that
  row only; a wholly unparseable file keeps the last-good parse and marks the plane `Stale`
  (first load with no last-good: `Stale` with zero rows). No panic/unwrap on any ledger content —
  a mid-edit file must never crash or blank the dashboard.
- `LedgerProvenance::{Fresh, Stale, Missing, Off}`, carried once per read (not per row):
  `Fresh` = last read parsed; `Stale` = last-good retained after a failed re-parse (or nothing
  parseable yet); `Missing` = configured path absent/unreadable; `Off` = not configured.

### B. Path resolution + hot reload

- Path resolution: `TOKENOMICS_LEDGER` env override > optional `ledger_path` in `[settings]` of
  `tokenomics.toml` > **off**. No hardcoded path in the binary. Both unset = plane `Off`: zero
  rendering, zero warnings in the TUI; `doctor` reports "ledger: not configured".
- Hot reload: the TUI event loop polls the ledger per tick with the spec-015 discipline
  (mtime/size/content-hash change detection, keep-last-good on failure). An edit is reflected
  without restart. The reader source is **injectable** so tests never touch the filesystem.
  Unlike spec 015's config swap, a ledger change only updates display data — it never changes
  which accounts exist or are polled.

### C. Join + independence

- Join is **exact string match** on `Account.id` ↔ ledger `id`. No fuzzy, prefix, or
  case-insensitive matching. Unmatched on either side = no clause for that account + a `doctor`
  divergence line.
- `Account.active` (spec 014 config flag) and ledger `status` are **independent bits**: the
  ledger never drives monitoring on/off; the config flag never drives the date display. A
  cancelled-in-ledger but `active = true` account keeps being monitored and shows its
  `ends in Xd` clause; an `active = false` account peeked via `i` still shows its clause.

### D. Display

All date math is pure with injected `now`, computed once in `build_account_view` into a `SubView`
on `AccountView`; `view.rs` renders tier-selected strings only and stays free of date logic.
Dates render as their ISO ledger form (`2026-08-14`), never reformatted or rolled. Day counts are
calendar-day differences in local time; a zero-day difference renders `today` in place of `in 0d`.

Per-state clause (FULL tier, appended to the header title after the existing `{label} [{provider}]`):

| State | Clause |
|---|---|
| active, future `renews` | `· period 2026-07-14 → · renews in 27d (2026-08-14)` (start segment only when `purchased` present) |
| active, past `renews` | `· renews 2026-08-14 (past — ledger stale?)` — **never a negative countdown** |
| cancelled, future `paid_through` | `· cancelled · ends in 4d (2026-07-22)` — visually parallel to active (verb swap), never reads as already-dead |
| cancelled, past `paid_through` | `· cancelled · ended 2026-07-22` (dimmed; derived condition, never a stored state) |
| cancelled, `paid_through` unknown | `· cancelled` — bare label; the status alone is information, never hidden |
| active with no dates / no row / plane off | clause omitted — header byte-identical to today |

Never a placeholder ("?", "n/a", "unknown") for missing data.

- **FULL** degrade order when the border title would overflow the panel width: drop start
  segment → drop absolute `(…)` date → drop whole clause. Never truncate a date mid-string.
- **COMPACT**: short dim clause after the name — `· renews 27d` / `· ends 4d` / `· cancelled` /
  `· ended`; past-`renews` stale state renders `· renews ?`. Two-step ladder in
  `compact_header_line`: if padding would hit 0, recompute without the clause — name + severity
  cluster always win.
- **MICRO**: no dates, unchanged output (8-char name column; stated decision, not an oversight).
- **Fleet header**, one dim token for degraded plane states: `Missing` → `· no ledger`,
  `Stale` → `· ledger stale`, `Fresh` with zero matched rows → `· ledger: 0 matched`.
  `Off` renders nothing anywhere in the TUI (deliberate deviation from the plan's pre-mortem,
  recorded in the PRD: unconfigured is a valid permanent state; `doctor` owns the distinction).

### E. CLI surfaces

- `tok doctor` gains a ledger section: resolved path + provenance state ("not configured" when
  `Off`); every row whose `renews`/`paid_through` is in the past (freshness); every row that
  failed to parse, **with reason** (a blanked row must never be diagnosable nowhere); join
  divergence both directions (config account without ledger row, ledger row without config
  account).
- `tok accounts` and `tok once --json` are **unchanged this wave**: their contract is
  config/usage data, and JSON consumers must not grow fields mid-wave. Enforced by golden
  snapshots (synthetic fixtures) committed **before** the implementation lands, asserted in
  tests; volatile fields (timestamps, absolute tmp paths) are normalized by the test harness and
  the normalization is part of the golden contract.

### F. Hygiene

- `check.sh` gains a PII gate: email regex `[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}` over
  `plans/ specs/ src/ docs/`; policy is zero matches outside a committed `.pii-allowlist` file.
- All fixtures in this spec's tests are synthetic (`claude-alpha`-style ids, made-up dates) —
  never live ledger content.
- Same commit as the implementation: `CLAUDE.md` Architecture gains the ledger as the third
  never-conflated plane, Key Files gains `src/ledger.rs`; `CHANGELOG.md` `[Unreleased]` entry.

## Non-goals

- Renewal-due alerts (decided Q6: dates only; alerts still key off `utilization_pct` exclusively).
- Rendering `plan`/`price` from the ledger; rendering `first_purchased` (Q1=a).
- Tokenomics writing the ledger, ever; provider billing APIs (none exist — settled finding).
- Lifecycle data in SQLite; collector reading the ledger.
- Driving `Account.active` from ledger `status` or vice versa (spec 014 non-goal upheld).
- The cancel/re-subscribe driver (lives in an external driver repo; see plan).

## Acceptance criteria

1. `parse()` on a synthetic ledger covering all field combinations yields the expected rows; the
   `account` email field does not exist on the struct and cannot be deserialized; a malformed row
   (incl. `status = "canceled"`) degrades that row only; a fully malformed file → keep-last-good
   + `Stale` (first load: `Stale`, zero rows). (A)
2. Path resolution: `TOKENOMICS_LEDGER` beats `[settings] ledger_path` beats off; both unset →
   `Off`, no render, no warning; configured-but-missing file → `Missing`. (B)
3. Join: exact match only — a near-miss id fixture (`claude-rob7` vs `claude-rob-7`) produces no
   clause and a doctor divergence line; `Fresh` with zero matched rows puts `· ledger: 0 matched`
   on the fleet header. (C, D, E)
4. Clause math (pure, injected `now`): each state row in the D table renders exactly as
   specified, including `today` for zero days, stale marker for past `renews`, dimmed `ended`,
   bare `cancelled`, and clause-omitted (header byte-identical to the no-ledger render) only when
   status carries no information. (D)
5. Tier rendering: FULL degrades in order (start segment → absolute date → whole clause) without
   truncating a date mid-string; COMPACT drops the clause before name/severity collide (two-step
   ladder test at narrow width); MICRO output contains no date under any state. Snapshot coverage
   for active, cancelled, ended, and unknown states in FULL and COMPACT. (D)
6. Hot reload via the injectable source: editing ledger content mid-run updates the clause next
   tick; replacing it with garbage keeps the previous clause and sets `Stale` (fleet header token
   asserted); restoring valid content clears `Stale`. (B)
7. Independence: `active = false` + ledger `active`, and `active = true` + ledger `cancelled`,
   both render per their own rules — neither field mutates the other's behaviour. (C)
8. `doctor` reports: provenance + resolved path; past-dated rows; failed-parse rows with reason
   (unknown-`status` fixture asserted); unmatched ids in both directions. `tok accounts` and
   `tok once --json` are byte-identical to the golden snapshots (post-normalization). (E)
9. `check.sh` email gate: zero matches outside `.pii-allowlist`; all fixtures synthetic. (F)
10. `./check.sh` green.
