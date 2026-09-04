# Discovery Plan: Claude Collector Overload (2026-09-04)

## Discovery Question

The Claude collector shells out to `ccusage` once per configured account every
tick. With 8 accounts sharing one real `~/.claude/projects` (11 GB, 25,651
JSONL files), 8 concurrent 1.2 GB / 10.8 s subprocesses blew the collector to
7.4 GB, triggered ~40 systemd-oomd kills and 70 restarts in one day, and took
out unrelated processes in the same `app.slice` (tmux Claude Code sessions,
Chromium). How do we make the Claude collector produce correct per-account
and aggregate usage data within bounded memory and time, on both Robert's
box and an arbitrary public user's machine, without depending on `ccusage`
as a subprocess?

## Ideas Explored

**PM lens**
- *Native JSONL scanner*: read the JSONL files in-process, no subprocess, no
  `ccusage` dependency at all for the default path.
- *Overlay-only mode*: drop local scanning entirely, rely solely on the
  opt-in authoritative `/api/oauth/usage` overlay (already per-account and
  cheap). Rejected as primary: overlay is opt-in and not universally
  available; local numbers still needed as fallback/default.
- *`tok doctor` "cost of a tick" self-check*: surface projected memory/time
  cost of the next tick so a user sees the problem before it becomes an
  outage, not after.

**Designer lens**
- *Shared-dir memo badge*: TUI/doctor shows "shared logs ×8" when multiple
  configured accounts resolve to the same real projects dir, so the
  duplication is visible instead of silently producing 8 identical numbers.
- *Honest "derived" badge*: label locally-scanned numbers as derived/
  approximate vs. the overlay's authoritative numbers.

**Engineer lens**
- *In-process streaming scanner with mtime-pruned lookback + per-file
  incremental cache*: read only files touched inside the lookback window,
  cache parsed results per file keyed on mtime, stream instead of loading
  whole files. Chosen direction.
- *Scan memoised per canonical projects dir per tick*: 8 symlinked accounts
  collapse to one scan per tick instead of 8.
- *Semaphore + `MemoryMax=` only, keep subprocess*: bound concurrency and
  let systemd cap RSS, without removing the `ccusage` dependency. Cheaper to
  ship but doesn't fix the 8x redundant full-tree parse; kept as a
  complementary hardening (unit-level backstop), not the fix.
- *`ccusage --since`*: rejected — measured no reduction (10.8 s / 1.16 GB
  either way), so it doesn't address the root cause.
- *Serialise heavy collects*: run providers sequentially instead of up to
  `MAX_INFLIGHT=16` in parallel, as a cheap concurrency cap.
- *Per-account real projects dirs (env change)*: point each configured
  account at its own non-symlinked directory. Would make the "8 identical
  scans" problem moot by construction, but requires changing how Robert's
  accounts are set up outside this repo. Out of scope for this project.
- *Ship `MemoryMax=` in the documented systemd unit*: bound the service's
  worst case regardless of what the scanner does internally.

## Selected Ideas

1. **In-process streaming JSONL scanner** (mtime-pruned lookback, per-file
   incremental cache, memoised per canonical projects dir per tick) —
   replaces the `ccusage` subprocess as the default collection path.
   `ccusage_cmd` remains as an explicit opt-in for users who want it.
2. **Serialise heavy collects** — cheap concurrency cap alongside the
   scanner; avoids re-introducing an 8x-parallel spike from any future
   provider.
3. **`MemoryMax=` in the documented systemd unit** — backstop so a scanner
   bug or an unexpectedly large tree fails safe (unit restarts) instead of
   taking out sibling processes via oomd.

## Critical Assumptions

| # | Assumption | Category | Impact | Uncertainty | Priority |
|---|---|---|---|---|---|
| A1 | The streaming scanner's block math (5h blocks, dedup by `message.id`+`requestId`) matches `ccusage`'s output closely enough to replace it | Feasibility | High — wrong numbers undermine the whole tool | Medium | P0 |
| A2 | A bounded lookback window (e.g. 36h) is enough to reconstruct the currently-active block without drifting from the true block boundaries over days of uptime | Feasibility | High — silent phase drift is worse than a crash | Medium | P0 |
| A3 | 36h of lookback is small enough to scan in bounded memory/time on the real 11 GB tree | Feasibility | High — this is the actual bug we're fixing | Low (partially measured) | P0 |
| A4 | Pricing-table drift (new/renamed models, price changes) is acceptable because the cost figure is explicitly "notional, never a bill" | Value/Usability | Medium — wrong-but-labeled-approximate cost is a UX issue, not a trust issue, if labeled correctly | Medium | P1 |
| A5 | Public users' machines mostly don't have `ccusage`/npm installed, so a scanner-first default (no subprocess dependency) is the right default for adoption | Viability | Medium — affects onboarding friction for open-source release | Medium | P1 |
| A6 | A 20 GB+ projects tree (heavier user than Robert) still scans within the same memory bound because of mtime pruning + per-file streaming, not because of tree size luck | Feasibility | High — public users are unbounded in scale | Medium | P0 |

## Validation Experiments

| # | Tests Assumption | Method | Success Criteria | Effort | Timeline |
|---|---|---|---|---|---|
| E1 | A1 | Golden fixture tests: hand-built JSONL fixtures with known block boundaries/dedup edge cases, assert scanner output against expected blocks | Scanner matches expected block start/end/active/token totals on every fixture | S | 1 day |
| E2 | A1, A6 | Side-by-side `tok once` (scanner) vs `ccusage blocks --json --active` on the real 11 GB tree, same timestamp | Active block token totals and cost within tolerance (e.g. <1% token diff, cost diff explained by pricing table) | S | 1 day |
| E3 | A2, A3 | Measure wall time + peak RSS of one full tick against the real tree, with and without warm per-file cache | Cold tick: bounded time and memory (no OOM risk at realistic worker count); warm tick: materially faster | S | 1 day |
| E4 | A3, A6 | 24h soak: run the service normally, log tick timings/RSS, count oomd kills | Zero oomd kills, tick time stays roughly flat (no unbounded growth from the incremental cache) | M | 1-2 days elapsed (background) |
| E5 | A2 | Synthetic fixture with a >5h gap inside the lookback window vs. one just outside it | Scanner produces the same "active block" as `ccusage` would in both cases; documents the known failure mode when a gap straddles the window edge | S | half day |

## Decision Framework

- **Ship now**: E1 (fixtures) and E2 (side-by-side) both pass — this is the
  correctness gate and is achievable today, no waiting.
- **Ship with `MemoryMax=` as a stated safety net**: even if E4's 24h soak
  isn't complete before merge, the unit-level bound plus E3's single-tick
  measurement is enough evidence the acute failure mode (8x parallel 1.2 GB
  subprocesses) is gone. Track E4 as a fast-follow before calling the fix
  "verified in production."
- **Escalate / reconsider approach** if E2 shows systematic divergence from
  `ccusage` beyond rounding/pricing (i.e., A1 is false) — in that case, do
  not replace the subprocess path yet; ship the concurrency/memory
  hardening only (`MemoryMax=`, serialised collects, per-tick memoisation)
  as an interim fix while the scanner's block math is debugged against more
  fixtures.
- **Known accepted gap**: A2's edge case (a >5h gap straddling the lookback
  boundary) is a documented, bounded risk — not a blocker — because it only
  affects a block that started right at the edge of the window, is rare in
  practice, and is covered as a Fast-Follow Tiger in the pre-mortem.
