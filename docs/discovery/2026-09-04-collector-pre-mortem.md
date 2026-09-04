# Pre-Mortem: Claude Collector Overload Fix (2026-09-04)

Scope: replacing the `ccusage` subprocess with an in-process streaming JSONL
scanner (mtime-pruned lookback, per-file incremental cache, memoised per
canonical projects dir per tick), serialised heavy collects, `MemoryMax=` in
the shipped systemd unit, and `ccusage` demoted to opt-in — see
`2026-09-04-collector-discovery.md` for the full assumption set.

## Risk Summary

| Category | Count |
|---|---|
| Launch-Blocking Tigers | 3 |
| Fast-Follow Tigers | 4 |
| Track Tigers | 2 |
| Paper Tigers | 2 |
| Elephants in the Room | 1 |

## Launch-Blocking Tigers

| # | Risk | Likelihood | Impact | Mitigation | Owner | Deadline |
|---|---|---|---|---|---|---|
| 1 | JSONL schema drift — Claude Code changes field names (`message.usage.*`) or record shape, scanner silently under/over-counts | Medium | High | Golden fixture tests pinned to the documented shape; a fixture per known field; `doctor` warns if a `type:"assistant"` line has none of the expected usage fields instead of silently zeroing it | agent | Before merge |
| 2 | A 20 GB+ projects tree on a public user's disk blows the same memory/time budget the fix is meant to close | Medium | High | Lookback pruning by mtime must bound scan size independent of total tree size; validate with a synthetic oversized fixture tree, not just Robert's 11 GB, before calling A6 (see discovery doc) confirmed | agent | Before merge |
| 3 | The TUI and `tok once` share the same `ProviderAdapter`, so a scanner bug (block math, dedup) hits both surfaces at once with no fallback | Medium | High | Fixture coverage (E1) plus the side-by-side ccusage comparison (E2) gate the swap; keep `ccusage_cmd` as an escape hatch users can flip back to if the scanner regresses | agent | Before merge |

## Fast-Follow Tigers

| # | Risk | Likelihood | Impact | Mitigation | Owner | Deadline |
|---|---|---|---|---|---|---|
| 4 | Block phase drift vs. `ccusage` when there's no >5h gap inside the lookback window (block start computed from truncated history diverges from ccusage's full-history view) | Low-Medium | Medium | Document as known limitation; add fixture E5 (gap straddling window edge); consider widening default lookback if soak testing shows drift | agent | Within 1-2 weeks post-launch |
| 5 | First tick after boot/restart is a cold cache — re-reads the full lookback window with no incremental benefit, so the "fast tick" story doesn't hold on every restart | High (by design) | Low-Medium | Acceptable if bounded (it's still one scan per canonical dir, not 8); document expected cold-tick cost in README so it isn't mistaken for a regression | agent | Document at launch, no code change needed |
| 6 | Pricing table goes stale as new models ship (already true today: fable-5, fable-5-1 not necessarily priced) | High (ongoing) | Low | Cost is explicitly labeled notional/non-billing; add a "model not in pricing table" fallback (zero-cost + flag) instead of a panic or silent wrong number | agent | Within 1-2 weeks post-launch |
| 7 | `MemoryMax=` set too low still lets oomd kill the unit under a legitimately large scan | Low-Medium | Medium | Base the value on measured peak RSS from E3/E4 plus headroom, not a guess; re-tune after the 24h soak | agent | After E4 soak completes |

## Track Tigers

| # | Risk | Notes |
|---|---|---|
| 8 | ccusage users lose the subprocess path silently if the config default flips without a changelog/README callout | Low likelihood if the default-flip is documented in CHANGELOG `[Unreleased]` per repo convention; watch for confused issues post-release |
| 9 | Uncommitted WIP already in the working tree (`src/tui`, `doctor`, `identity.rs`) gets swept into the same commit as this fix, muddying the diff and review | This is a process risk for whoever implements the fix, not a design risk; flag it now so the implementing session commits it separately or stashes it first |

## Paper Tigers

- **"8 in-process scans will still be too slow"** — sounds scary but the
  chosen design memoises the scan per canonical dir per tick, so it's
  structurally 1 scan not 8; the discovery doc's E3 measurement will
  confirm actual cost, but the fear is about the old (subprocess) design,
  not the new one.
- **"Streaming parser is a big engineering lift"** — the repo already has
  this exact pattern twice (codex, gemini providers: mtime-pruned walk +
  pure `parse_*`/`reduce_*_snapshot` + fixtures). This is porting a proven
  in-repo pattern to a third provider, not inventing one.

## Elephants in the Room

- **The 8 configured Claude accounts share one real `projects/` directory,
  so per-account local numbers were never meaningful in the first place —
  only the opt-in authoritative overlay (`/api/oauth/usage`, genuinely
  per-account) is trustworthy for "which account did what."** The scanner
  fix makes the shared-dataset problem cheap and safe instead of expensive
  and crash-prone, but it does not make the 8 numbers independently true.
  This should be said plainly in the README and in the TUI ("shared logs
  ×8" badge), not left for users to discover the way this incident
  discovered it. Also worth naming: public release should not ship
  Robert's account names/paths as defaults — a separate but adjacent
  "personal paths in tokenomics.toml/docs" cleanup is needed before public
  release, tracked but not gating this fix.

## Go/No-Go Checklist

- [ ] Golden fixture tests pass for block-boundary and dedup edge cases (E1)
- [ ] Side-by-side scanner vs. `ccusage` on the real 11 GB tree agrees
      within tolerance (E2)
- [ ] Single-tick wall time / peak RSS measured and bounded (E3)
- [ ] `MemoryMax=` value set from measured peak, not guessed
- [ ] `ccusage_cmd` flipped to opt-in in default config; documented in
      CHANGELOG `[Unreleased]`
- [ ] `doctor` still warns on shared canonical projects dir (existing
      check) and the TUI surfaces it too
- [ ] Oversized-tree fixture (>11 GB equivalent file count) exercised, not
      just Robert's tree
- [ ] Working-tree WIP unrelated to this fix is not swept into the same
      commit
- [ ] 24h soak scheduled/running with a zero-oomd-kill target (E4) — can
      trail launch as a fast-follow verification, not a hard gate, given
      Launch-Blocking Tiger #3's fixture+comparison coverage already
      substitutes for immediate production confidence
