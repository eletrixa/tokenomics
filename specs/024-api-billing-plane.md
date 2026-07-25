# Spec 024 — API billing plane: real-dollar balances & month-to-date spend (fourth plane)

Status: **Active**

Amends the PAYG exclusions in specs 020 §A, 021 §A and 023 §Non-goals, which ruled API-key
setups and dollar figures out of scope. Those rulings said "wrong plane for a subscription
monitor", not "never" — spec 020/021's `api_key_env`-rejected rules for `[[account]]` stay
exactly as they are. This spec adds the plane those rulings were waiting for: a separate
`[[api]]` account list whose numbers are **real money** (a bill or a balance), never mixed
with the notional proxy, limits gauges, or subscription dates.

## Evidence (2026-07-25, key inventory + provider docs)

- RESEARCH.md:80-85 parked the Anthropic/OpenAI admin billing endpoints as "only relevant if a
  real API-key account is later added". That precondition is met: `ANTHROPIC_ADMIN_API_KEY`
  ("Admin key for billing/usage APIs") and `OPENAI_ADMIN_API_KEY` exist in the secrets master,
  alongside live PAYG keys for Anthropic (3 org keys), OpenAI, Google AI Studio and Moonshot
  ($50 top-up 2026-07-23).
- Documented, supported endpoints exist for three of five providers:
  Anthropic `GET /v1/organizations/cost_report` (Admin API; ~5-min freshness, poll ≤1/min),
  OpenAI `GET /v1/organization/costs` (daily buckets, admin key),
  Moonshot `GET /v1/users/me/balance`. A better ToS posture than the plane-2 overlay.
- No endpoint exists for: Anthropic/OpenAI credit balance (console-only), anything Gemini
  (AI Studio personal tier, no Cloud billing export configured), xAI without a management key
  (none on disk). Gaps are filled by a **manual TOML** beside the subscription ledger, or ship
  **dormant** (xai — the kimi spec-023 shipping state).

## Behaviour

### A. Plane contract + config

- New plane module `src/billing.rs`: `ApiProvider { Anthropic, OpenAi, Gemini, Moonshot, Xai }`
  (strings `"anthropic"`/`"openai"`/`"gemini"`/`"moonshot"`/`"xai"`, `as_str`/`parse` like
  `Provider` — a **separate enum**; API accounts are not subscription accounts),
  `ApiFigureKind { Balance, Owed, MtdSpend }` (`"balance"`/`"owed"`/`"mtd_spend"`),
  `ApiFigure { kind, amount_minor: i64, currency: String }`.
- **Money is minor units (cents) as `i64`**. Wire representations differ and each parser owns
  its exact conversion (verified 2026-07-25 against the reference docs):
  Anthropic reports **decimal strings already in cents** (`"123.78912"` = $1.23…) — accumulate
  exactly (integer micro-cents), round half-even to cents once per figure, no f64 in the path;
  OpenAI (`amount.value`) and Moonshot (`available_balance`) report **f64 dollars** — ×100,
  round half-even (precision is bounded by the wire itself there). `currency` is a verbatim
  label (`"USD"`, `"EUR"`; OpenAI sends `"usd"` — uppercase for display) — no conversion, no
  cross-currency arithmetic anywhere in the app.
- Config: new top-level `[[api]]` array (`Config.api`, `deny_unknown_fields` kept) with
  `id` (unique among api entries), `label` (optional, defaults to id), `provider`,
  `api_key_env` (env-var **NAME**, never a value), `active` (default true). Validation (pure):
  unique ids; `api_key_env` **required** for anthropic/openai/moonshot/xai, **rejected** for
  gemini (no billing endpoint exists to call — gemini is manual-only); absent `[[api]]` section
  ⇒ plane Off, config parses and validates exactly as today.
- New `[settings]` keys: `poll_billing_secs` (default 3600, floor 300) and `api_manual_path`
  (like `ledger_path`). Both optional.

### B. Live lane (HTTP, collector-polled)

- One seam for all billing providers (not one lane per provider):
  `trait BillingEndpoint { async fn fetch(&self, provider, api_key, month_start, cursor)
  -> AppResult<Vec<u8>> }`. `HttpBillingEndpoint` matches on provider to build URL/headers
  (anthropic: admin key header + `anthropic-version`; openai: bearer + `start_time`;
  moonshot: plain bearer). One `reqwest::Client`, 10s request / 5s connect timeouts;
  429 → `AppError::RateLimited` (status only, body never parsed); other non-200 →
  `AppError::Overlay("HTTP {n}")`. `#[cfg(test)]` canned double (kimi posture).
- Pure parsers, one per provider, fixture-tested:
  `parse_anthropic_cost_report` / `parse_openai_costs` → sum the page's daily amounts into one
  `MtdSpend` figure + next-page cursor; `parse_moonshot_balance` → `available_balance` →
  one `Balance` figure. Unusable numerics **skip, never zero**; nothing usable ⇒ `Err`.
- Pagination loop lives in the lane task (fetch → parse → accumulate → follow cursor), hard cap
  8 pages; a UTC month of daily buckets fits one page in practice.
- **MTD window = UTC calendar month**; `month_start` computed with jiff in the collector loop
  and injected, so parsers stay pure. A near-zero MTD in the first hours of a month is correct.
- **xai ships dormant**: config-accepted, never fetched in this spec (no fetcher, no URL) —
  env-var unset is the expected state; a follow-up spec adds the management-API fetcher when a
  key exists.

### C. Manual lane (TOML, TUI-side read-through)

- `src/billing/manual.rs` mirrors `ledger.rs` exactly: path from `TOKENOMICS_API_MANUAL` env >
  `[settings] api_manual_path` > Off; provenance `Off/Fresh/Stale/Missing` once per read;
  two-tier degradation (bad row → per-row error, rest survive; whole-file failure →
  keep-last-good + `stale_reason`); content-hash no-op; injectable source trait (tests never
  touch the filesystem); collector never reads it; nothing lands in SQLite.
- Row shape: `[[api_manual]]` with `id` (joins `[[api]].id` exact + case-sensitive), `figure`
  (`balance`/`owed`/`mtd_spend`), `amount` (TOML decimal → cents without f64 drift), `currency`,
  `as_of` (date), `notes` (optional). **No email/PII field**; exhaustive-destructure test
  enforces it (ledger discipline).
- A manual row whose `as_of` is older than **35 days** (const) is `stale` — kept, rendered with
  the stale marker, named by doctor.
- Merge rule (consumed by spec 025): **live wins per `(id, figure)`; manual fills gaps**. Every
  figure carries per-figure provenance: live + polled-at vs manual + as-of (+ stale).

### D. Collector wiring + store

- New `billing.tick()` select arm on its own `cadence_interval(poll_billing_secs)`
  (`MissedTickBehavior::Skip`) → `spawn_billing_fetches` (filter `active` && key resolves via
  the **hoisted** `resolve_api_key` — moved from its zai/kimi duplicates into `providers/mod.rs`,
  the planned third-consumer trigger; unset/empty env ⇒ skip with logged reason, the dormant
  state) → per-card task wrapped in `tokio::time::timeout(OVERLAY_TICK_BUDGET_SECS)` → one
  `JoinSet<BillingOutcome>` harvest arm → `apply_billing_outcome`: present-and-active guard,
  then `set_api_billing` on success; 429 feeds the shared pure `next_backoff` with a billing-sized cap —
  `max(4 × poll_billing_secs, OVERLAY_BACKOFF_CAP_SECS)`, since the shared overlay cap can sit
  below the hourly billing base (per-api-id cooldown). Single-writer invariant untouched — only the loop task writes.
- Store `SCHEMA_V6` (additive gate, `SCHEMA_VERSION` → 6): table `api_billing`
  `(api_id, figure, amount_minor, currency, polled_at_ms, PRIMARY KEY (api_id, figure))`.
  Writer `set_api_billing(api_id, figures)` — atomic per-id replace, written **only on
  successful parse** (failure keeps last-good rows, ages honestly). Reader
  `latest_api_billing()`.

### E. Surfaces

- `tok doctor`: new `report_api` section (read-only, presence-not-value — **never prints a
  dollar amount**): manual path + Fresh/Stale/Missing, per-row parse errors, past-stale ids,
  live figures' last-poll ages (or `never polled`), dormant list (env-var name + unset),
  manual rows joining no configured `[[api]]` id (divergence).
- TUI: **none in this spec** — the board section is spec 025. The TUI must not read the new
  table yet.
- `tok accounts` / `tok once --json`: **byte-identical** to the R16 goldens — the plane touches
  neither code path.
- `tokenomics.example.toml`: commented `[[api]]` + `api_manual_path` block (feeds `tok init`).

### F. Hygiene

- `api_key_env` is a NAME; the value is read at fetch time, held only for the request, never
  stored/logged/printed — no-secret-leak test over `Debug`/`Display` of every new type and
  error path.
- Fixtures use fake org/project ids; `./check.sh`'s PII gate covers `fixtures/`.
- Every new `.rs` file carries the `//!` module header per `rules/file-headers.md`.

## Live verification (deferred — blocks nothing)

With the real env keys on this machine: one collector billing tick → `api_billing` rows exist;
`tok doctor` shows poll ages, no values in any log line; Anthropic/OpenAI MTD ≈ their console
figures; moonshot balance ≈ $50 minus usage. Then flip Status to Done.

## Non-goals

- Any TUI rendering (spec 025), dollar-keyed alerts (alerts stay `utilization_pct`-only; an
  "owed > $X" alert is a future-spec candidate), spend caps/auto-top-up, per-model or per-key
  spend breakdown, currency conversion or cross-currency totals.
- xai live fetching (dormant until a management key exists — follow-up spec).
- OpenAI credit-grants (unofficial dashboard endpoint — manual TOML instead).
- Gemini BigQuery billing export (no Cloud SA configured; manual TOML instead).
- Touching `[[account]]` semantics: subscription validation, `Limit`, `cost_notional`, ledger.

## Acceptance criteria

1. `ApiProvider`/`ApiFigureKind` round-trip their strings; decimal-string → cents parsing is
   exact for 2-dp inputs (no f64 in the path) and rejects malformed input. (A)
2. Config: `[[api]]` parses; dup id / unknown provider / missing-`api_key_env` (pollable
   providers) / present-`api_key_env` (gemini) each produce a named finding; absent section ⇒
   zero behaviour change (existing config-suite untouched); floors enforced
   (`poll_billing_secs` ≥ 300). (A)
3. Parsers: fixture matrix per provider — good page ⇒ correct cents sum / balance; paginated ⇒
   cursor surfaced and accumulated with cap 8; empty month ⇒ zero-bucket success (MtdSpend 0);
   malformed / unusable numerics ⇒ skip-or-`Err` per §B, never a fabricated 0. (B)
4. Manual TOML: good / partial-row / broken-file / stale-`as_of` / unknown-id fixtures behave
   per §C; whole-file failure keeps last-good; exhaustive-destructure PII test. (C)
5. Store: V5→V6 migration on a real v5 fixture db; round-trip; per-id replace is atomic;
   failed poll leaves prior rows byte-identical. (D)
6. Collector: canned endpoint success/429/failure/timeout paths; 429 doubles the per-id
   backoff (capped); unset env ⇒ skipped with logged reason and **no task spawned**; no secret
   in any Debug/error output. (D)
7. Doctor renders every §E state; `tok accounts` + `tok once --json` byte-identical to R16
   goldens. (E)
8. `./check.sh` green.
