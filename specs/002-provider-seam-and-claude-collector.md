# Spec 002 — Provider Seam & Claude Collector

**Status:** Done
**Wave:** 2
**Related:** `RESEARCH.md` §ccusage schema, ghmonitor `client.go`, `src/providers/`, `src/runner.rs`

## Why
The core value: turn one account's ccusage output into a normalized `UsageSnapshot`. Follow the
pure-core pattern (groundcontrol `probe.rs`): pure `parse` + `reduce`, thin **injectable** async
shell-out. Define the `ProviderAdapter` seam here so Codex/Gemini/Grok are additive later.

## Contracts (`src/domain.rs`)
`Account`, `Provider { Claude, .. }`, `UsageSnapshot { input, output, cache_read, cache_creation,
total_tokens, cost_notional, window }`, `Window { start, end, remaining, tokens_per_minute,
cost_per_hour }`. (Full shapes in the build plan.)

## ccusage (verified schema)
`ccusage blocks --json --active` → `{ blocks: [ { id, startTime, endTime, actualEndTime, isActive,
entries, costUSD, models[], tokenCounts { inputTokens, outputTokens, cacheCreationInputTokens,
cacheReadInputTokens }, totalTokens, burnRate { costPerHour, tokensPerMinute }, projection {
remainingMinutes, .. } } ] }`. `totalTokens` already sums cache.

## Requirements

### Functional
| ID | Requirement | Acceptance criteria |
|----|-------------|---------------------|
| FR-1 | `parse_ccusage_blocks` (pure) | `serde` over the JSON; tolerate unknown fields; empty `blocks` ⇒ no active block. |
| FR-2 | `reduce_snapshot` (pure) | pick the `isActive` block; map `tokenCounts`→fields; `total_tokens` from `totalTokens`; `window` from start/end; `remaining` from `projection.remainingMinutes`; `cost_notional` from `costUSD`. |
| FR-3 | `derive_session_limit` (pure) | from the active `Window`, `Limit { Session, utilization = elapsed/5h × 100 (time-in-window proxy), resets_at = window.end, source = Derived, severity = Ok always }`. A time proxy is never classified against `warn_pct`/`crit_pct` — see Resolved decisions. |
| FR-4 | `Runner` trait + `Exec` | The pure argv builder returns a `CommandSpec { program, args, env, timeout }` (subprocess-safety: the builder is a tested pure fn); `Runner::run(&CommandSpec)` executes it. `Exec` via `tokio::process`, bounded by `spec.timeout`, stdin nulled; explicit **argv** (never `sh -c`). |
| FR-5 | `ClaudeAdapter::collect` | `ccusage_command_spec` sets `CLAUDE_CONFIG_DIR = config_dir` and argv `[<prefix> blocks --json --active]` → run → `parse` → `reduce`. Returns `Option<UsageSnapshot>` (`None` = idle / no active block; a valid state, not an error). `now` is injected so reduction stays pure. |
| FR-6 | `CannedRunner` test seam | returns fixture bytes AND records the last `CommandSpec` (incl. env), so `collect` is tested with no process spawn. Test-only (`#[cfg(test)]`). |
| FR-7 | ccusage invocation override | `[settings].ccusage_cmd` (optional argv prefix, e.g. `["npx","ccusage"]`) → `CcusageInvocation`; absent/empty ⇒ a bare `ccusage` on `PATH`. Added because this machine has no global ccusage install. |

### Non-Functional
| ID | Requirement | Acceptance criteria |
|----|-------------|---------------------|
| NFR-1 | `parse`/`reduce`/`derive` pure & table-tested on `fixtures/blocks.json`. |
| NFR-2 | `collect` sets `CLAUDE_CONFIG_DIR` (asserted via `CannedRunner`). |
| NFR-3 | Schema/flag drift tolerant (`serde` defaults; unknown fields ignored). |

## GATE (load-bearing) — ✅ RESOLVED 2026-07-04
Verified the **real** ccusage honors `CLAUDE_CONFIG_DIR`: default (`~/.claude`) returned the active
block (193M tokens); `CLAUDE_CONFIG_DIR=<empty dir> ccusage blocks --json --active` returned
`{"blocks":[]}`. Conclusion: per-account collection via `CLAUDE_CONFIG_DIR` works — **the direct-JSONL
fallback is NOT needed for v1** (keep it as a documented future option only).

## Resolved decisions
- **Session utilization % (was NEEDS CLARIFICATION):** RESOLVED — the derived session limit expresses
  **time-elapsed-in-window** (`(now − start) / (end − start) × 100`, clamped 0–100), tagged
  `Provenance::Derived`. `now` is injected so `derive_session_limit` is pure. The authoritative
  token % replaces it only when the overlay is enabled (Wave 7). Rationale: a countdown-style proxy
  is more useful on the dashboard than `n/a`, and the `Derived` badge prevents plane-conflation.
- **A time proxy never carries severity (amended 2026-09-04):** `derive_session_limit` stamps
  `Severity::Ok` unconditionally. It originally ran its percent through `severity_for`, which meant
  a 5h window simply *elapsing* past `warn_pct` painted the gauge yellow and fired a desktop alert —
  an alarm on the clock, on an account that may have spent nothing. Thresholds classify quota
  readings only. Surfaces mark the proxy instead: the TUI prefixes a non-authoritative percent with
  `~` and replaces the severity word with `elapsed` (`~76% elapsed`), so the `Derived` badge is no
  longer the *only* thing separating a clock from real usage — the row itself says which it is.
- **No clock under a fresh overlay that reports no session (amended 2026-11-14):** observed on
  2026-11-14, `/api/oauth/usage` answers `five_hour: null` and a `limits[]` with only `weekly_all` /
  `weekly_scoped` for every account. The overlay succeeded; it has no 5h figure. The TUI row
  (`build_account_view`) therefore drops a `Derived` session whenever the same account carries an
  `Authoritative` non-session limit, and the 5h line reads
  `n/a (no 5h limit reported)`. The clock stays the fallback for an
  overlay-off account and for a stale overlay (weekly demoted to `Estimate`). An authoritative
  session, if the endpoint sends one again, wins as before. Store and `tok once` are unchanged.
- **Early `tok once` (human + `--json`):** the minimal `once` command (spec 009) landed here so the
  collector core is reachable from `main` (keeps the strict-lints build free of dead code) and gives
  immediate end-to-end value. `--json` serializes `UsageSnapshot` + derived `Limit`. The `doctor`
  distinct-config-dir check and full runbook remain in Wave 9.
- **Contracts pulled forward:** `Limit`/`LimitKind`/`Severity`/`Provenance` live in `domain.rs` now
  (needed by `derive_session_limit`); their not-yet-constructed variants carry a reasoned
  `#[allow(dead_code)]` until Wave 3 (severity/merge) and Wave 7 (overlay/weekly) construct them.

## Acceptance Criteria (rollup)
FR-1..FR-7 + NFR + gate resolved; parse/reduce/derive table tests + `collect` env test green;
`tok once` proven against a real account (npx ccusage under `CLAUDE_CONFIG_DIR`); `check.sh` green.
