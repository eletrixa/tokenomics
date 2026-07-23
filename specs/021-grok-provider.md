# Spec 021 — Grok provider: usage-only adapter

Status: **Done** — §C's no-limits ruling is **superseded by spec 022**: live evidence showed
`creditUsagePercent` IS the weekly subscription quota (not on-demand credit), so the limits lane
now exists, fed from the local billing log.

Plan: `plans/002-multi-provider/03-grok.md` (research — verdict RED at research time: "don't build,
buys a permanently-n/a limits column"). **Re-scoped to GREEN-usage-only 2026-07-19** once the
maintainer acquired SuperGrok Heavy and real `~/.grok/logs/unified.jsonl` existed on the machine: the local usage
lane is real and ToS-safe (a direct analog of Codex spec 013 / Gemini spec 020), so v1 ships the
usage plane and leaves limits honestly `n/a`. Real file shapes were verified against the live
`~/.grok/logs/unified.jsonl` on 2026-07-19 (617 real `shell.turn.inference_done` events) — see §B.

## Motivation

Official Grok Build CLI (`grok`, binary `~/.grok/bin/grok`) writes a per-inference token log to
`~/.grok/logs/unified.jsonl` — the same shape Tokenomics already reduces for Codex and Gemini. No
scriptable **subscription** quota surface exists (the local `billing: fetched credits config` line
carries `creditUsagePercent`, but that is *on-demand credit* usage — prepaid $, 0 for a pure
subscription — never the message quota %; §C), so v1 is **usage-only**: real token counts, both
gauges honestly `n/a`, no invented cost. Attribution is the account's `config_dir` (its `GROK_HOME`,
default `~/.grok`), never the logs.

## Behaviour

### A. Provider + config contract

- `Provider::Grok` (string `"grok"`) round-trips config → store → display.
- Config: `config_dir` **required** (the `GROK_HOME` dir, default `~/.grok`), `api_key_env`
  **rejected** (an `XAI_API_KEY` Grok setup is PAYG API, not a subscription — same exclusion as a raw
  Anthropic Console key / API-key Gemini). `limits_overlay` accepted but **ignored** (no subscription
  limits surface exists); `doctor` states this so a set flag isn't a silent mystery. Same posture as
  Gemini (spec 020 §A).

### B. Usage lane (local, ToS-safe)

- `providers/grok/logs.rs` — pure parse + reduce over `<config_dir>/logs/unified.jsonl`. **Real
  shape** (verified 2026-07-19 against the live file, 617 events):
  - One JSON object per line (append-only shell log). Token events carry
    `"msg":"shell.turn.inference_done"`, an ISO-8601 `ts`, a session `sid`, and
    `ctx: {loop_index, prompt_tokens, cached_prompt_tokens, completion_tokens, reasoning_tokens, ...}`.
  - Every other line (auth, marketplace, `billing: fetched credits config`, session lifecycle) has a
    different `msg` and is skipped by the `msg` filter. Malformed lines degrade per-line, never fail
    the account.
  - **No dedup needed** (unlike Gemini's event-sourced `.jsonl`): each `inference_done` is a
    distinct billable inference emitted once. An agentic turn emits several (`loop_index` 1..n),
    each a real separate model call — sum them all, no id-keyed collapse.
  - Observed invariants (all 617 events): `cached_prompt_tokens ≤ prompt_tokens` (cache ⊆ prompt)
    and **`reasoning_tokens ≤ completion_tokens` with zero exceptions** — reasoning is the reasoning
    *portion of* completion, NOT additive. (The research doc's "reasoning billed as output, add it"
    is WRONG for Grok — adding it double-counts. `reasoning_tokens` is informational only.)
  - Bucket mapping: `input = prompt_tokens − cached_prompt_tokens` (saturating floor 0),
    `cache_read = cached_prompt_tokens`, `output = completion_tokens` (reasoning already inside),
    `cache_creation = 0`, `total_tokens = prompt_tokens + completion_tokens`. Buckets sum to
    `total_tokens` by construction: `(prompt−cached) + cached + completion = prompt + completion`.
  - An all-zero token event is not a real turn (skipped).
  - `cost_notional = None` (no public subscription pricing basis — never fabricate),
    `window = None` (the 5h lookback is a scan bound, not a window claim — same as Codex/Gemini).
- `GrokAdapter`: read the single `logs/unified.jsonl` with per-file mtime pruning (bounded scan,
  spec-013 §B discipline). Missing file (fresh install / logged-out, never ran a turn) ⇒ `Ok(None)`
  idle. Reads on the blocking pool (a large log can never stall the collector runtime).

### C. No limits lane

- No overlay code, no new `LimitKind`. The `billing: fetched credits config` line's
  `creditUsagePercent` + weekly `currentPeriod` are **on-demand credit** telemetry (prepaid $ /
  usage-based add-on), not the SuperGrok message quota — surfacing them as a utilization gauge would
  misrepresent the subscription plane, so they are deferred (out of scope, noted in `doctor`).
- Both gauges render `n/a` — exactly the un-opted-in Codex / Gemini render.

### D. Surfaces

- TUI: usage row (tokens, no cost), both gauges `n/a`; ledger clause + verified pill work unchanged
  for a matching id (`grok-heavy`). Fleet reductions: `cost_notional = None` never poisons the fleet
  cost line (existing None-handling reused).
- `tok doctor`: `config_dir` exists; `grok --version` runs (argv subprocess, timeout, output never
  parsed beyond success); `logs/unified.jsonl` **existence only** (content never inspected for the
  report); the ignored-`limits_overlay` note when set.
- `tok accounts` / `tok once --json`: grok account appears like any account; R16 goldens
  (claude/codex fixtures) stay byte-identical.

## Non-goals

- Any limits/quota code, a derived gauge, or parsing the `billing: fetched credits config` line.
- Cost display; parsing xAI's console/API rate-limit headers (that's the separate PAYG API product).
- Reading `~/.grok/auth.json` contents; auto-login.
- Tail-by-offset reads of a huge `unified.jsonl` (v1 reads the whole file + filters by `ts`; the
  file has no rotation — `ponytail:` ceiling noted in code, upgrade to a byte-offset tail if it ever
  grows to MBs).
- New external dependencies (fs + serde only).

## Acceptance criteria

1. `"grok"` round-trips config → domain → store → display; validation: `config_dir` required,
   `api_key_env` rejected with the account named; existing provider validations stay green. (A)
2. Parser keeps only `shell.turn.inference_done` lines; skips other-`msg`/malformed/all-zero lines
   defensively; **no dedup** (two events with the same `sid` but different `loop_index` both count);
   reduce sums only in-window events; buckets sum to `total_tokens`; `reasoning_tokens` never added
   to output (asserted on a fixture where `reasoning < completion`); no events ⇒ `None`. (B)
3. Adapter on a fixture `logs/unified.jsonl` returns the merged snapshot; mtime pruning skips a stale
   file (asserted); missing file ⇒ `Ok(None)`. (B)
4. `cost_notional = None` flows through store and fleet reduction without poisoning the fleet cost
   line. (D)
5. TUI snapshot: tokens shown, both gauges `n/a`, no cost line, no derived anything; ledger clause +
   pill render for a matching id. (C, D)
6. `doctor`: dir check, `grok --version` probe (with timeout), `unified.jsonl` existence line,
   ignored-overlay note. `tok accounts` / `tok once --json` byte-identical to R16 goldens. (D)
7. `./check.sh` green.
