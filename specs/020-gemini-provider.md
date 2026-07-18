# Spec 020 — Gemini provider: usage-only adapter

Status: **Active**

Plan: `plans/002-multi-provider/` (research `02-gemini.md`, synthesis `10-adapter-plan.md` §2 —
verdict YELLOW, usage-only, after z.ai). Local chats JSONL was machine-verified 2026-07-19.

## Motivation

Official gemini-cli writes per-turn token counts to local session files — a direct analog of the
Codex sessions lane (spec 013), ToS-safe. No scriptable limits/quota surface exists (the daily
request ceiling resets at midnight Pacific and is exposed nowhere machine-readable), so v1 is
**usage-only**: real token counts, both gauges honestly `n/a`, no invented cost. The account
activates once a Google-account-OAuth gemini-cli login exists on this machine.

## Behaviour

### A. Provider + config contract

- `Provider::Gemini` (string `"gemini"`) round-trips config → store → display.
- Config: `config_dir` **required** (the `GEMINI_CLI_HOME` dir, default `~/.gemini`),
  `api_key_env` **rejected** (an API-key Gemini setup is PAYG, not a subscription — same
  exclusion as a raw Anthropic Console key). `limits_overlay` is accepted but **ignored** (no
  limits surface exists); `doctor` states this so a set flag isn't a silent mystery.

### B. Usage lane (local, ToS-safe)

- `providers/gemini/chats.rs` — pure parse + reduce over
  `<config_dir>/tmp/<project-hash>/chats/session-*.json[l]`:
  - **Both file shapes**: single-document `.json` and line-per-event `.jsonl` — normalized on
    read. Malformed lines/files degrade per-line/per-file, never fail the account.
  - Per-turn event shape: `{tokens: {input, output, cached, thoughts, tool, total}, model,
    timestamp}` (observed invariant: `total = input + output + thoughts + tool`, `cached ⊆
    input`).
  - Bucket mapping: `input = input − cached` (floor 0), `cache_read = cached`,
    `output = output + thoughts` (reasoning folds into output, as Codex), `cache_creation = 0`,
    `total_tokens = tokens.total`. **`tool` folds into the input bucket** (tool results are
    prompt-side context — an assumption `[NEEDS CLARIFICATION]` until a real `tool > 0` fixture
    exists); the acceptance test asserts buckets sum to `total_tokens` regardless of placement.
  - `cost_notional = None` (no public subscription pricing basis — never fabricate),
    `window = None` (the 5h lookback is a scan bound, not a window claim — same posture as
    Codex).
- `GeminiAdapter`: fan-out glob across all `tmp/*/chats/` project hashes with per-file mtime
  pruning (bounded scan, spec-013 §B discipline). Missing `tmp/` ⇒ `Ok(None)` idle (fresh
  install, not an error). A logged-out account stops producing sessions ⇒ idle.

### C. No limits lane

- No overlay code, no new `LimitKind` (`Daily` explicitly deferred — a derived
  midnight-Pacific counter is out of scope until Robert wants a `Derived`-badged approximation).
- Both gauges render `n/a` — exactly the un-opted-in Codex render. A daily request ceiling must
  never be dressed up as a Session gauge.

### D. Surfaces

- TUI: usage row (tokens, no cost), both gauges `n/a`; ledger clause + verified pill work
  unchanged for a matching id. Fleet reductions: `cost_notional = None` never poisons the fleet
  cost line (existing None-handling reused).
- `tok doctor`: `config_dir` exists; `gemini --version` runs (argv subprocess, timeout, output
  never parsed beyond success); `oauth_creds.json` **existence only** (content never read);
  the ignored-`limits_overlay` note when set.
- `tok accounts` / `tok once --json`: gemini account appears like any account; R16 goldens
  (claude/codex fixtures) stay byte-identical.

## Non-goals

- Any limits/quota code, `LimitKind::Daily`, or derived gauges.
- Cost display; parsing Google's undocumented Code Assist HTTP API.
- Reading OAuth credential contents; auto-login.
- Multi-account `GEMINI_CLI_HOME` isolation promises (read-from-source but not two-account
  verified — not claimed until exercised).
- New external dependencies (fs + serde only).

## Acceptance criteria

1. `"gemini"` round-trips config → domain → store → display; validation: `config_dir` required,
   `api_key_env` rejected with the account named; existing provider validations stay green. (A)
2. Parser handles `.json` and `.jsonl` fixtures; skips malformed lines/files defensively;
   reduce sums only in-window turns; buckets sum to `total_tokens` (incl. a `tool > 0`
   synthetic fixture); no events ⇒ `None`. (B)
3. Adapter on a fixture tree spanning multiple project hashes returns the merged snapshot;
   mtime pruning skips stale files (asserted); missing `tmp/` ⇒ `Ok(None)`. (B)
4. `cost_notional = None` flows through store and fleet reduction without poisoning the fleet
   cost line. (D)
5. TUI snapshot: tokens shown, both gauges `n/a`, no cost line, no derived anything; ledger
   clause + pill render for a matching id. (C, D)
6. `doctor`: dir check, `gemini --version` probe (with timeout), `oauth_creds.json` existence
   line, ignored-overlay note. `tok accounts` / `tok once --json` byte-identical to R16
   goldens. (D)
7. `./check.sh` green.
