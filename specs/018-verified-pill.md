# Spec 018 — Verified pill on the subscription clause

Status: **Done**

Follow-up to spec 017 (Robert, 2026-07-18): the ledger now carries `verified = <date>` — the date
an agent last confirmed the row against the provider's billing web UI (Tier-0 read-only run,
evidence in an external driver repo). Render it, because the load-bearing question is "is this **renewal
date** confirmed from the provider's web, so I know when to cancel" — a human-typed date and a
web-verified date must look different on the board.

## Behaviour

### A. Parser (`src/ledger.rs`)

- `Subscription` gains `verified: Option<jiff::civil::Date>` (optional TOML local-date). Same
  per-row degradation rule as the other date fields: an invalid `verified` value drops that row
  only. Absent = human-entered row, never an error.

### B. Verified-current semantics (pure, in `SubView` computation)

The pill renders only when the verification still says something about the **current period**:

- `purchased` present: verified-current ⇔ `verified <= today && verified >= purchased` (a
  verification older than the current period start proves nothing about this period's renewal; a
  future-dated `verified` — a typo'd ledger date — proves nothing either and must not render a
  confident pill).
- `purchased` absent: verified-current ⇔ `verified <= today && today − verified <= 31` days
  (best-effort recency window; stated constant).
- The active-with-past-`renews` stale state **suppresses the pill** — a stale-marked clause never
  shows `✓` (contradiction otherwise). The derived "ended" state shows no pill either.
- Verified-but-not-current renders nothing (no "stale verified" marker; doctor owns that detail).

### C. Display

- **FULL**: append ` ✓ 2026-07-18` (U+2713 + the `verified` date, ISO verbatim) after the
  renews/ends segment, styled dim green — applies to the active-with-future-`renews`,
  cancelled-with-future-`paid_through`, and bare-`cancelled` clauses when verified-current.
  Degrade order becomes: drop the pill's date (bare ` ✓` stays) → drop start segment → drop
  absolute renews/ends date → drop whole clause (pill goes with it). Never truncate a date
  mid-string, as before.
- **COMPACT**: the short clause gains a trailing ` ✓` when verified-current (`· renews 27d ✓`);
  it lives inside the existing two-step ladder — clause+pill render together or drop together.
- **MICRO**: unchanged, no dates, no pill.
- No pill ever renders without a clause; `Off`/missing-row states are untouched.

### D. Doctor

- The ledger section annotates each matched row: `verified <date> (current)`,
  `verified <date> (outdated — before current period)`, or `human-entered (no verified)`.

## Non-goals

- The TUI verifying anything itself (no network — verification is written by the driver lane).
- An alert on unverified renewals; any change to `tok accounts` / `tok once --json` (goldens
  stay byte-identical).

## Acceptance criteria

1. `parse()` reads `verified`; absent → `None`; invalid → that row drops (existing rule). (A)
2. Verified-current math (injected `today`): `verified >= purchased` true/false cases;
   purchased-absent 31-day window in/out; stale-`renews` and ended states suppress the pill even
   when verified-current. (B)
3. FULL renders ` ✓ <date>` dim green on active, cancelled-with-end, and bare-cancelled clauses
   when verified-current; degrade order pill-date → start segment → absolute date → whole clause,
   snapshot-tested at narrowing widths. (C)
4. COMPACT renders trailing ` ✓` when verified-current and drops it with the clause in the
   ladder; MICRO output contains no `✓` under any state. (C)
5. Doctor annotates verified/outdated/human-entered per matched row. (D)
6. `tok accounts` / `tok once --json` byte-identical to the R16 goldens; `./check.sh` green.
