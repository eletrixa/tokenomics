# Spec 026 — Native Claude Transcript Scanner

**Status:** Active
**Wave:** 26
**Related:** specs/002 (ccusage collector), specs/005 (loop + MAX_INFLIGHT), specs/010 (shared `projects/`),
specs/013/020 (codex/gemini in-process JSONL scanners), `docs/discovery/2026-09-04-collector-*.md`

## Why
The Claude local-usage lane shells out to `ccusage blocks --json` once per account per tick. Measured
2026-09-04 on an 11 GB / 25,651-file `projects/` tree: one run = 10.8 s wall, 1.2 GB RSS; `--since`
does not help. With 8 accounts whose `projects/` all symlink to the same tree, a tick spawns 8 of them
concurrently (≈9 GB), every one hits the 30 s timeout, and systemd-oomd kills the unit (70 restarts/day)
plus bystanders in the same slice (Chromium, tmux Claude sessions). Codex and Gemini already read
their JSONL in-process; Claude is the only provider that shells out.

## Requirements

### Functional
| ID | Requirement | Acceptance criteria |
|----|-------------|---------------------|
| FR-1 | In-process scanner is the default Claude usage source | `ClaudeAdapter::collect` with `ccusage_cmd` absent reads `<config_dir>/projects/**/*.jsonl` itself; no subprocess is spawned (CannedRunner sees no spec). |
| FR-2 | ccusage stays as explicit opt-in | `ccusage_cmd = [...]` in `[settings]` selects the subprocess lane unchanged (spec 002 behaviour, same fixtures pass). |
| FR-3 | Streaming, bounded parse | Files are read line-by-line (`BufRead`), only `"type":"assistant"` lines with `message.usage` become events; a malformed line is skipped, never an error. Memory is O(events in lookback), not O(bytes). |
| FR-4 | mtime lookback prune | Only files with mtime ≥ `now − LOOKBACK` (24h) are opened. A pruned file never contributes even if its content is in-window. |
| FR-5 | Dedup | Events keyed by `message.id` + `requestId` count once across all files (first wins). Lines without both keys are keyed by file+line (never dropped). |
| FR-6 | ccusage block semantics | Entries sorted by ts. New block when no block, `ts − block.start ≥ 5h`, or `ts − prev.ts ≥ 5h`; `block.start = floor_to_hour(ts)`, `end = start + 5h`. Active ⇔ `now < end && now − last.ts < 5h`. Idle ⇒ `None`. |
| FR-7 | Snapshot parity | `UsageSnapshot` fields as today: four buckets, `total_tokens` = sum, `window{start,end,remaining_minutes,tokens_per_minute,cost_per_hour}`, `cost_notional` from the pricing table (FR-8). |
| FR-8 | Notional cost | Static longest-prefix pricing table (per MTok: input, output, cache-write = 1.25×in, cache-read = 0.1×in unless listed). Unknown model ⇒ its tokens count, its cost contributes 0. Never a bill. |
| FR-9 | Shared-dir memo | Scans are memoised per `(canonical projects dir, now)`: N accounts symlinked to one tree cost one scan per tick. `now` is the collector's per-tick timestamp. |
| FR-10 | Incremental cache | Per-file `(mtime, len) → events` cache survives across ticks; an unchanged file is not re-read; a file gone from the walk is evicted. |
| FR-11 | Serialised heavy lane | All Claude scans go through one async mutex (`ponytail:` global lock; per-dir if it ever matters). Concurrent collects for the same dir wait, then hit the memo. |
| FR-12 | Missing tree ⇒ idle | No `projects/` (fresh install) ⇒ `Ok(None)`, not an error. Symlinked subdirectories inside the tree are not followed (scan-bound discipline, as gemini). |

### Non-Functional
| ID | Requirement | Acceptance criteria |
|----|-------------|---------------------|
| NFR-1 | One tick on the 11 GB tree: < 5 s cold, < 1 s warm, < 300 MB RSS for the whole process. Recorded in `docs/ops/llm-runs.md`-style evidence in the CHANGELOG entry. |
| NFR-2 | `docs/running-the-collector.md` unit ships `MemoryMax=1G` + `MemoryHigh=768M` so a regression is contained to the unit. |
| NFR-3 | Pure core (`parse_line`, `active_block`, `price_for`, `reduce_snapshot`) table-tested with a checked-in fixture; adapter tested with `tempfile` trees (memo, prune, symlink, missing). |
| NFR-4 | `tok doctor` reports the active lane (`native` / `ccusage`) and no longer requires ccusage on PATH unless `ccusage_cmd` is set. |

## Landed alongside
- **Golden `tok once --json` (spec 017 R16) amended:** a Claude account whose `projects/` tree does not
  exist is now idle (no `error` field) instead of surfacing a ccusage subprocess error —
  `tests/golden/once.json` updated in the same commit, per the golden's own instruction.

## Deferred
- Phase drift: if no ≥5h gap falls inside the lookback, block boundaries may differ from ccusage's
  (which chains from the first entry ever). Local blocks are `Derived`; the overlay stays authoritative.
- Pricing overrides in config — add when a user reports a wrong notional figure.

## Acceptance Criteria (rollup)
FR-1..FR-12, NFR-1..NFR-4; `check.sh` green; the live unit runs 24h with zero oomd kills.
