# Spec 019 — z.ai (GLM coding plan) provider: limits-only adapter

Status: **Active**

Plan: `plans/002-multi-provider/` (research `01-zai.md`, synthesis `10-adapter-plan.md` §1 —
verdict GREEN, build first). The quota endpoint was live-probed 2026-07-19 against Robert's real
Lite-plan key: HTTP 200, `data.level = "lite"`.

## Motivation

Robert has a paid z.ai GLM coding subscription (`zai-glm` in the ledger) that is invisible on the
board. z.ai exposes an authoritative quota endpoint (one HTTP GET — simpler than both existing
overlays: no subprocess, no OAuth refresh). No local usage lane exists today (GLM runs through an
ephemeral-HOME wrapper), so v1 is **limits-only and honestly idle** — real gauges, no invented
usage, no fabricated cost.

## Behaviour

### A. Provider + config contract

- `Provider::Zai` (string `"zai"`) round-trips through config, store, and display like
  `"claude"`/`"codex"`.
- `Account.config_dir` becomes `Option<PathBuf>`; new optional field `api_key_env: Option<String>`
  (an env-var **NAME**, never a value). Per-provider validation:
  - `claude`/`codex`: `config_dir` **required** (unchanged semantics), `api_key_env` rejected.
  - `zai`: `api_key_env` **required**, `config_dir` optional (accepted but unused this wave).
  - Existing configs parse unchanged; validation error messages name the offending account.
- Attribution: the key **is** the identity — one key = one z.ai account. The secret is read from
  the collector/TUI process environment at fetch time, held only for the request, and **never**
  stored, logged, printed, or written to SQLite. `doctor` reports env-var *presence* only.

### B. Limits lane (authoritative overlay)

- `providers/zai/quota.rs`: pure `parse_quota_response(bytes, account_id, warn_pct, crit_pct)
  -> AppResult<Vec<Limit>>` over `GET https://api.z.ai/api/monitor/usage/quota/limit`
  (`Authorization: Bearer <key>`). Mapping (fixture = live-probed shape):
  - `TOKENS_LIMIT` entry with `unit = 3, number = 5` → `LimitKind::Session`; `percentage` used
    directly (0–100); **`resets_at = ""`** — the rolling-5h window has no reset instant and the
    empty-string idiom already renders "no countdown". Never fabricate a reset.
  - `TOKENS_LIMIT` entry with `unit = 6, number = 1` → `LimitKind::WeeklyAll`; `percentage`
    direct; `nextResetTime` (epoch ms) converted once to RFC 3339 UTC, rendered verbatim after.
  - `TIME_LIMIT` (monthly MCP-tool quota) → **skipped**, surfaced by `doctor` as one
    informational line. No new `LimitKind`.
  - Unknown entries skipped defensively; **both** expected `TOKENS_LIMIT` entries missing ⇒
    error (schema drift must degrade, never render a wrong gauge).
  - Provenance `Authoritative`; severity from the existing warn/crit thresholds.
- Fetch behind an injectable seam (same posture as the Claude overlay's endpoint seam), riding
  the existing overlay cadence: `poll_overlay_secs`, per-account backoff, TTL demotion to
  `Estimate` on sustained failure (spec 011 machinery reused, not reimplemented). Timeout on the
  request as everywhere.
- 401/403 (revoked key), 429, non-200, malformed body ⇒ error → backoff/demotion. No
  Claude-specific token-warmth gates.

### C. Usage lane

- `collect` returns `Ok(None)` — always idle this wave. No usage row, no `cost_notional`, no
  derived session %. (A future local GLM `CLAUDE_CONFIG_DIR` lane would reuse `ccusage.rs` with
  `cost_notional` forced to `None` — out of scope, noted in the plan.)

### D. Surfaces

- Collector: an opted-in (`limits_overlay = true`) zai account gets limits on the overlay pass;
  skipped with a `doctor`-visible reason when not opted in or when the env var is unset/empty.
  claude/codex paths byte-identically untouched.
- TUI: session gauge = authoritative % with no countdown; weekly gauge = % + reset countdown; no
  usage/cost line (idle render, as an idle Codex account today). Alerts key off `utilization_pct`
  as everywhere. Spec-017 ledger clause + spec-018 pill work unchanged for a `zai-lite`-style id.
- `tok doctor`: env-var presence (name only), endpoint reachability when opted in, the MCP-quota
  informational line, and the standard freshness/provenance reporting.
- `tok accounts` / `tok once --json`: a zai account appears with its identity like any account
  (the R16 goldens use claude/codex fixtures only and stay byte-identical).

## Non-goals

- Any usage lane, cost display, or derived gauges for zai this wave.
- New `LimitKind` variants (no `Monthly`).
- Key rotation, key validation beyond presence, or any logging of key material.
- Scraping z.ai web surfaces (driver track owns billing pages; ledger owns dates).
- New external dependencies (reqwest/serde/jiff already in tree).

## Acceptance criteria

1. `"zai"` round-trips config → domain → store → display; per-provider validation passes/rejects
   exactly per §A (claude/codex existing tests stay green; zai without `api_key_env` errors with
   the account named; claude with `api_key_env` errors). (A)
2. `parse_quota_response` on the real-shape fixture yields Session (`resets_at = ""`) +
   WeeklyAll (RFC 3339 from epoch-ms) with `Authoritative` provenance and correct severities;
   `TIME_LIMIT` skipped; unknown entries skipped; both-entries-missing ⇒ error; malformed JSON ⇒
   error. (B)
3. Fetch seam with canned transcripts: 200 → limits; 401/429/garbage → error path feeding the
   existing backoff/TTL-demotion (demotion to `Estimate` asserted); no secret appears in any
   error string, log output, store row, or debug formatting (asserted on the error/Debug
   representations). (B)
4. Collector integration (fake adapter/endpoint): opted-in zai account produces limits on the
   overlay cadence; un-opted-in and env-var-missing accounts are skipped with recorded reason;
   claude/codex collection untouched. (D)
5. TUI snapshot: zai account renders session % without countdown, weekly % with countdown, no
   usage/cost row; ledger clause + verified pill render for a matching ledger row. (D)
6. `doctor` shows env-var presence, opt-in state, MCP informational line; `tok accounts` /
   `tok once --json` stay byte-identical to the R16 goldens. (D)
7. `./check.sh` green.
