# Spec 022 — Grok weekly quota: authoritative gauge from the local billing log

Status: **Done**

Amends spec 021 §C, which dismissed the `billing: fetched credits config` line's
`creditUsagePercent` as *on-demand credit* telemetry. Live evidence (2026-07-23, the same
`~/.grok/logs/unified.jsonl` the usage lane reads) shows that reading was wrong — the field is the
**weekly subscription quota utilization** — so the Grok limits lane is real, local, and ToS-safe.

## Evidence (2026-07-23, live log)

On a machine whose only Grok product is a SuperGrok Heavy subscription (no credits ever purchased):

- Every `billing: fetched credits config` line carries `onDemandCap.val = 0`,
  `onDemandUsed.val = 0`, `prepaidBalance.val = 0` — the on-demand/prepaid plane is identically
  zero, so `creditUsagePercent` cannot be measuring it.
- `creditUsagePercent` moved absent → `2.0` → `3.0` across 2026-07-19 in lockstep with that day's
  627 real `inference_done` events — it tracks *subscription* usage.
- The same `ctx` carries `subscriptionTier: "SuperGrok Heavy"` and a
  `currentPeriod: { type: "USAGE_PERIOD_TYPE_WEEKLY", start, end }` whose `start` equals the
  subscription purchase instant — a weekly quota window, exactly Tokenomics' `% + reset` model.
- Early lines in a session may *lack* `creditUsagePercent` entirely (fetched before the quota is
  known) — absence is schema state, never a real 0%.

## Behaviour

### A. Pure parser (`providers/grok/billing.rs`)

- `parse_weekly_quota(bytes, account_id, now, warn_pct, crit_pct) -> Vec<Limit>` — pure, injected
  `now`, mirrors `zai::quota::parse_quota_response`'s severity/build discipline.
- A line **counts** iff `msg == "billing: fetched credits config"` AND
  `ctx.config.creditUsagePercent` is present, finite, and `>= 0` AND `ctx.config.currentPeriod.end`
  parses as a timestamp. Every other line — other `msg`, missing pct (early-session fetch),
  malformed JSON, negative/non-finite pct — is skipped per-line, never failing the file.
- The **newest** counting line by `ts` wins (the log is append-only, but multiple pids interleave —
  order by `ts`, not file position).
- If the winner's `currentPeriod.end <= now`, the period has lapsed and the percent is from a dead
  week: return **empty** (never render an expired number as live).
- Otherwise return one `Limit`: `kind = WeeklyAll`, `scope = None`, `utilization_pct` verbatim,
  `resets_at` = the `currentPeriod.end` **string verbatim**, `severity = severity_for(pct, warn,
  crit)`, `source = Authoritative` (it is the provider's own number, read from the provider's own
  telemetry — no derivation, no network).

### B. Provider seam: local limits (additive)

- `ProviderAdapter` gains `collect_local_limits(&self, account, now, warn_pct, crit_pct) ->
  AppResult<Vec<Limit>>` with a **default `Ok(Vec::new())`** — every existing adapter is untouched;
  the registry dispatches like `collect`.
- `GrokAdapter::collect_local_limits`: read `<config_dir>/logs/unified.jsonl` on the blocking pool
  and hand it to the parser. **No mtime prune** — unlike the usage lane, an old billing line still
  carries a live, unexpired period (the pct cannot change without a CLI run, and a CLI run appends
  a fresh line). Missing file ⇒ `Ok(vec![])` (fresh install), never an error.

### C. Collector wiring

- The per-account collect task calls both methods; `CollectOutcome` carries `local_limits`.
- Harvest passes `local_limits` into the shared `apply_limits` merge point on **both** arms:
  fresh snapshot ⇒ derived session limit ++ `local_limits`; idle/failed ⇒ `local_limits` instead of
  the empty vec (Grok is usage-idle most of the time; the weekly quota must not decay just because
  no inference ran in 5h).
- Degradation falls out of the existing machinery, no new code: while the parser yields the row
  each tick it re-wins the merge as `Authoritative`; when it stops (period expired, file gone) the
  stored row demotes to `Estimate` (frozen %, live countdown) and goes dormant once `resets_at`
  passes (spec 011 §C / 012 §A).
- `limits_overlay` stays **ignored** for grok — this lane is plane-1 local file I/O, always on,
  like a derived limit; there is nothing to opt into and nothing polls the network.

### D. Surfaces

- TUI: the weekly gauge renders through the existing `WeeklyAll` machinery (zai already exercises
  it) — no gauge code changes. The `weekly_hint` fallback (shown only when no row exists) gets a
  Grok-specific arm — `"n/a (awaiting grok billing log)"` — because for grok the honest reason is
  "the CLI hasn't logged a quota line for the current period yet", not gemini's "no limits surface".
  Gemini's arm is unchanged.
- `tok doctor`: the grok section now reports the parsed quota — pct + reset verbatim + `[live]`,
  `[expired <end>]` when the newest line's period has lapsed, or `no billing line found`. (Amends
  spec 021 §D "existence only": doctor now inspects content for this one report.)
- `tok once` stays session-limit-only (zai weekly parity — `once` has never surfaced weekly rows).
- Session gauge: unchanged (`n/a` — Grok exposes no session-window surface).

## Non-goals

- Any network fetch, on-demand credit / prepaid balance surfacing, or cost from the quota.
- Multi-machine freshness (another machine's usage moves the real pct without a local line; the
  single-machine assumption is accepted and the demotion machinery bounds the lie).
- Surfacing weekly rows in `tok once --json` (would fork `OnceRecord` for one provider).
- Tail-by-offset reads (same ponytail ceiling as spec 021).

## Acceptance criteria

1. Parser: newest-by-`ts` valid line wins across interleaved pids; missing-pct / malformed /
   negative-pct / other-`msg` lines are skipped defensively; expired period ⇒ empty; pct and
   `resets_at` land verbatim; severity honours thresholds; empty/non-UTF-8 input ⇒ empty. (A)
2. Trait default: a provider without an override returns `Ok(vec![])` through the registry;
   claude/codex/zai/gemini behaviour is byte-identical (R16 goldens unchanged). (B)
3. Grok adapter: fixture log ⇒ exactly one `WeeklyAll` `Authoritative` limit; missing file ⇒ empty;
   a **stale-mtime** file with a live period still yields the limit (no mtime prune — asserted). (B)
4. Collector: an idle grok outcome still persists/refreshes the weekly row (idle arm carries
   `local_limits`); a snapshot outcome merges derived session ++ local limits. (C)
5. TUI model: grok with a stored `WeeklyAll` row renders the gauge (not the hint); grok with no row
   shows `"n/a (awaiting grok billing log)"`; gemini hint unchanged. (D)
6. Doctor: live / expired / absent billing-line states each render their §D report line. (D)

## Amendment 2026-11-15

- The weekly hint is now `"n/a (open /usage in grok)"` (was `"n/a (awaiting grok billing log)"`).
  Verified 2026-11-15 on Grok Build 1.0.77: the `billing: fetched credits config` line is written
  only when the `/usage` modal opens (not at startup), and `unified.jsonl` is head-truncated at
  about 5 MB, so the line lives for minutes under heavy use.
- A stored grok weekly row whose period lapsed is dropped in the TUI (the hint shows), not
  rendered as `waiting for reset`: no reset arrives without a new billing line.
- The 5h row of a provider with a snapshot but no session limit reads
  `<tokens> tok in 5h · <age>` in place of the `collecting…` placeholder.
- [NEEDS CLARIFICATION] The truncated log makes the 5h token sum an undercount under heavy use;
  a durable source (`~/.grok/sessions/*/events.jsonl`) is not yet evaluated.
- Weekly gauges carry a pace mark (elapsed fraction of an assumed 7-day window, from `resets_at`).
  The billing line also carries `currentPeriod.start`; it is not read yet.
