# Brainstorm: Claude Collector Overload (2026-09-04)

Context: the Claude collector shells out to `ccusage` per account, 8 accounts
share one real projects dir (11 GB, 25,651 files), 8 parallel subprocesses
hit ~9 GB combined and blew past the collector's 30s timeout, and
systemd-oomd killed the service ~40 times and several bystander processes in
one day. See the incident facts in `2026-09-04-collector-discovery.md`.

## PM Perspective

- **Native JSONL scanner** — read files in-process instead of shelling out.
  Removes the runtime dependency on `ccusage` entirely for the default path.
  Impact: H, Effort: M
- **Overlay-first messaging** — position the opt-in authoritative overlay as
  "the real numbers" and local scan as "the free estimate," so users don't
  mistake a derived figure for a bill. Impact: M, Effort: S
- **`tok doctor` "cost of a tick" check** — before the fix ships, and as an
  ongoing guardrail, `doctor` reports projected time/memory for the next
  tick given configured accounts and lookback. Impact: M, Effort: S
- **Public-release scope cut: drop `ccusage` as a hard dependency** — the
  README and defaults should not assume `ccusage` is installed; it becomes
  optional/opt-in. Impact: H (adoption), Effort: S (it's already optional
  via `ccusage_cmd`, just needs to become non-default)
- **Document the "shared dir = shared numbers" limitation up front** — set
  expectation before the first daily-check user hits it themselves.
  Impact: M, Effort: S

## Designer Perspective

- **"Shared logs ×8" tag** — TUI and `doctor` show when N configured
  accounts resolve to the same canonical projects dir, next to the account
  list. Impact: M, Effort: S
- **Scan cost readout** — a small "last scan: 2.1s, 340MB, 1,402 files" line
  in `doctor` or a debug view, so the cost of a tick is visible instead of
  invisible until it OOMs. Impact: M, Effort: S
- **Honest "derived" badge** — local per-account numbers get a small
  "derived, not billed" indicator distinct from the overlay's "verified"
  pill (which already exists per specs/018). Impact: M, Effort: S
- **Degrade visibly, not silently** — if a scan is skipped (rate-limited,
  memoised, still running from last tick), the TUI shows "stale — scan in
  progress" rather than a blank or frozen number. Impact: M, Effort: M
- **Doctor warning promoted, not buried** — the existing "accounts share one
  projects/ realpath" warning becomes a visible top-of-output line, not one
  item in a long list. Impact: L, Effort: S

## Engineer Perspective

- **In-process streaming JSONL scanner, mtime-pruned lookback** — walk only
  files touched within the lookback window, stream-parse rather than
  loading whole files. Impact: H, Effort: M
- **Per-file incremental cache keyed on mtime** — skip re-parsing files that
  haven't changed since the last tick. Impact: H, Effort: M
- **Scan memoised per canonical projects dir per tick** — 8 symlinked
  accounts collapse to 1 scan instead of 8, using existing realpath
  resolution `doctor` already does. Impact: H, Effort: S
- **Serialise heavy collects** — run provider collects sequentially instead
  of up to `MAX_INFLIGHT=16` in parallel; removes the combinatorial memory
  spike class entirely, at the cost of some tick latency. Impact: H,
  Effort: S
- **`MemoryMax=` in the shipped systemd unit** — bound worst-case RSS so a
  bug fails as a clean restart, not an oomd rampage through the app.slice.
  Impact: H, Effort: S
- **Keep `ccusage` as explicit opt-in (`ccusage_cmd`)** — existing knob,
  just flip the default off; no new code. Impact: M, Effort: S (config
  default change only)
- **Local pricing table for notional cost** — `costUSD` is present in ~2
  lines per file; everything else needs a pricing table keyed on model
  name/alias (`claude-opus-5`, `claude-sonnet-5`, etc.), following the
  codex/gemini providers' existing pure `parse_*`/`reduce_*_snapshot` +
  fixtures pattern. Impact: H, Effort: M
- **Fixture-based golden tests for block math** — port the existing
  fixture-test pattern from codex/gemini providers to the new Claude
  scanner (5h block boundary + dedup-by-id cases). Impact: H, Effort: S

## Top 5

| Rank | Idea | Why | Quick Win? |
|---|---|---|---|
| 1 | In-process streaming JSONL scanner + per-file mtime cache | Removes the root cause (8x parallel 1.2GB subprocesses); matches existing codex/gemini pattern in the repo | No |
| 2 | Scan memoised per canonical projects dir per tick | Turns 8 redundant scans into 1 with a small, self-contained change on top of the scanner | Yes |
| 3 | `MemoryMax=` in the systemd unit | One-line hardening, ships independently of the scanner, bounds worst case immediately | Yes |
| 4 | "Shared logs ×8" + "derived" badges in TUI/doctor | Makes the already-true "8 accounts, 1 dataset" fact visible instead of a silent surprise | Yes |
| 5 | Fixture-based golden tests for block math | Cheapest way to build confidence the replacement is correct before removing the subprocess path | Yes |

## Next Steps

1. Build the streaming scanner + per-file cache behind the existing
   `ProviderAdapter` trait, following the codex/gemini pure-function +
   fixtures pattern.
2. Add per-canonical-dir memoisation in the collector loop (uses the same
   realpath logic `tok doctor` already has for its shared-dir warning).
3. Write golden fixture tests for 5h block boundaries and dedup-by-id before
   wiring the scanner in as the default path.
4. Ship `MemoryMax=` in the documented systemd unit and flip `ccusage`
   collection to opt-in via `ccusage_cmd`.
5. Add the "shared logs ×N" and "derived" TUI/doctor surfaces once the
   scanner numbers exist to badge.
