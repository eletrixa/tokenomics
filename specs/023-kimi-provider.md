# Spec 023 — Kimi (Moonshot AI coding plan) provider: limits-only adapter, dormant until live key

Status: **Active**

## Motivation

Kimi coding-plan subscription is invisible on the board. Kimi exposes a z.ai-style quota endpoint
(one HTTP GET, `Bearer sk-kimi-` subscription key). No key exists yet (signups paused
~2026-07-19) — the provider ships fixture-tested and **dormant**: an account whose `api_key_env`
var is unset is skipped with a logged reason (exactly zai's eligibility path).

## Behaviour

### A. Provider + config contract

- `Provider::Kimi` (string `"kimi"`) round-trips config → store → display like `"zai"`.
- `api_key_env` **required** (env-var **NAME**, never a value); `config_dir` optional, accepted
  but unused, never existence-checked. Identical to the zai arm of `validate_provider_fields`.
- Secret read from process env at fetch time, held only for the request, never
  stored/logged/printed. `doctor` reports presence only.

### B. Limits lane (authoritative overlay)

- `providers/kimi/quota.rs`: pure `parse_quota_response(bytes, account_id, warn_pct, crit_pct)
  -> AppResult<Vec<Limit>>` over `GET https://api.kimi.com/coding/v1/usages`.
- **Fixture is community-sourced** (2 corroborating captures), **not live-probed**. Mapping:
  - `usages[]` entry with `duration "300"` + `durationUnit "TIME_UNIT_MINUTE"` →
    `LimitKind::Session`; % **derived** as `used/limit×100` from the string-typed numerics;
    `resets_at` = the entry's ISO-8601 `resetTime` normalized once to RFC 3339 (Kimi, unlike
    z.ai, provides a reset instant for the rolling 5h window — use it, `""` only when
    absent/unparsable).
  - Entry with `duration "10080"` (**unverified** weekly discriminator) → `LimitKind::WeeklyAll`,
    same derivation.
  - `limit`/`used` absent, non-numeric, or `limit ≤ 0` ⇒ entry skipped (never a 0% gauge);
    unknown entries skipped; **both** windows missing ⇒ error (drift degrades, never lies).
  - Provenance `Authoritative`; severity via `format::severity_for`; derived % not clamped.
- Fetch behind the `QuotaEndpoint` seam (zai posture): bearer auth, `Accept: json`, 10s timeout;
  429 → `AppError::RateLimited` (status only — body never parsed; known envelope
  `error.type == "exceeded_current_quota_error"` documented for future use, never match message
  text); other non-200 → `AppError::Overlay`. Rides `poll_overlay_secs` cadence, per-account
  backoff, TTL demotion to `Estimate` (spec 011 machinery reused).

### C. Usage lane

- `collect` returns `Ok(None)` — always idle. No usage row, no `cost_notional`, no derived %.

### D. Surfaces

- Collector: additional select-loop `JoinSet` arm mirroring zai; opted-in kimi account gets
  limits on the overlay pass; env-var-unset/empty or un-opted-in ⇒ skipped with logged reason
  (the dormant shipping state). All sibling provider paths untouched.
- TUI: session gauge % with countdown; weekly % with countdown; no usage/cost row. Zero
  kimi-specific TUI code (generic weekly-hint arms apply).
- `doctor`: env-var NAME + presence (never value); endpoint probe only when opted in AND active
  AND key present. No MCP-analog line.
- `tok accounts` / `tok once --json`: kimi appears like any account; R16 goldens stay
  byte-identical (they contain no kimi account).

## Live verification (deferred — blocks nothing)

When a subscription key exists: `tok doctor` probe → adjust fixture/discriminators to the live
shape, re-run `check.sh`, flip Status to Done.

## Non-goals

- PAYG balance (`api.moonshot.ai/v1/users/me/balance`) — dollars, not quota-%; no gauge concept
  exists. Not even a `doctor` line. *(Superseded by spec 024: the balance is now a live figure
  of the separate API-billing plane — still nothing here in the subscription lanes.)*
- Any usage lane, cost display, new `LimitKind`, key validation beyond presence.
- Parsing the 429 body / `error.type` surfacing.
- New dependencies.
- Folding per-provider overlay lanes into one collection — revisit when a seventh provider
  lands, not before.

## Acceptance criteria

1. `"kimi"` round-trips config → domain → store → display; kimi without `api_key_env` errors
   naming the account; `config_dir` accepted-unused, never existence-checked. (A)
2. `parse_quota_response` on the community-shape fixture yields Session (derived 75%,
   `resets_at` = RFC 3339 from the fixture's `resetTime`) + WeeklyAll (derived 25%, `resets_at`
   populated), both `Authoritative`; zero/non-numeric/missing numerics skipped not zeroed;
   unknown entries skipped; both-windows-missing ⇒ error; malformed ⇒ error. (B)
3. Fetch seam with canned transcripts: 200 → limits; 429/garbage → error path feeding backoff +
   TTL demotion to `Estimate`; no secret in any error/Debug output. (B)
4. Collector integration: opted-in kimi account produces limits on the overlay pass;
   un-opted-in AND env-var-missing accounts skipped (dormant contract); mixed-fleet pass never
   touches other providers' accounts. (D)
5. TUI model: kimi account renders session % with countdown, weekly % with countdown, no
   usage/cost row; ledger clause + verified pill unchanged. (D)
6. `doctor` shows env-var presence + skip reason for the dormant account; `tok accounts` /
   `tok once --json` byte-identical to the R16 goldens. (D)
7. `./check.sh` green.
