# Tokenomics

Tokenomics is a single-binary Rust TUI that monitors LLM **subscription** accounts — today 3 Claude
Max accounts on one WSL machine, later Codex / Gemini / Grok. Per account it shows token **usage**
(notional cost as a labeled proxy, never a bill), **limit % utilization** (5h + weekly), and
**time-left until reset**. Built for WSL2.

Background: `RESEARCH.md` (data sources, the `/api/oauth/usage` overlay, `CLAUDE_CONFIG_DIR`
attribution, ToS) and `STACK-DECISION.md` (why Rust + ratatui).

## Tech Stack

| Layer | Technology |
|-------|-----------|
| Language | Rust (2021, strict — `forbid(unsafe_code)`, clippy pedantic `-D warnings`) |
| TUI | ratatui + crossterm |
| Async | tokio (never block the UI task; async results arrive as messages over channels) |
| Local data | ccusage CLI (`blocks --json`) per account via `CLAUDE_CONFIG_DIR`; direct-JSONL fallback |
| Store | SQLite (rusqlite `bundled`, WAL) — collector writes, TUI reads |
| Limits overlay | reqwest (rustls) → `GET /api/oauth/usage` (opt-in, provenance-tagged, degrades to derived) |
| Config | TOML (`serde` + `toml`) at `~/.config/tokenomics/tokenomics.toml` |
| Time | jiff (reset countdowns) |

## Commands

```bash
./check.sh                 # THE GATE: fmt --check + clippy -D warnings + test (must be green)
cargo run -- validate      # validate tokenomics.toml
cargo run -- accounts      # list configured accounts
cargo run -- once --json   # one snapshot per account, as JSON
cargo run -- collector     # run the background collector (writes the store)
cargo run                  # launch the TUI
cargo run -- doctor        # read-only diagnostics
cargo build --release      # -> target/release/tok
```

Binary is `tok`. Config source of truth: `~/.config/tokenomics/tokenomics.toml`; store at
`~/.local/share/tokenomics/tokenomics.db`. Path resolution is **cwd-independent** (`src/paths.rs`) —
for dev, point at repo-local files explicitly:
`TOKENOMICS_CONFIG=./tokenomics.toml TOKENOMICS_DB=./tokenomics.db cargo run -- …`.

## Rules

**Read before writing any code.** All coding rules live in `rules/`. Start at `rules/_index.md`;
route via `rules/crossroads.md`. Every `.rs` file carries a `//!` module header per
`rules/file-headers.md`. Rust specifics: `rules/rust/{strict-lints,ratatui-architecture,
subprocess-safety,async-tokio,error-handling,anti-patterns}.md`.

## Specs

**Development is spec-driven TDD.** One spec per wave in `specs/` (index: `specs/README.md`).
Cycle per wave: **spec → 🔴 red → 🟢 green → ♻ refactor-for-specs → ♻ refactor-for-rules**. Mark
ambiguities `[NEEDS CLARIFICATION]`; never guess. Update the spec alongside the code when they diverge.

## Versioning

- Maintain `CHANGELOG.md` `[Unreleased]` — add an entry for every user-facing change, in the same commit.
- Never bump the version or cut a release — only the user does.

## Git

- **Default branch: `dev`.** Never push directly to `main`. Handoffs go in `docs/handoff/`.

## Architecture

**Three data planes, never conflated.** (1) Local ccusage / JSONL token usage = the ToS-safe core.
(2) The `/api/oauth/usage` overlay = **opt-in**, provenance-tagged, and degrades silently to derived
estimates on any 429/failure. (3) The subscription ledger (`src/ledger.rs`, spec 017) = billing/
lifecycle dates (purchased/renews/cancelled_on/paid_through) read-through from an external
git-versioned TOML file — **TUI-only** (the collector never reads it), never persisted to SQLite,
provenance-tagged (`Fresh`/`Stale`/`Missing`/`Off`), degrades to a blank clause on any parse failure,
and joins to `Account.id` by exact match only. Rendering is a pure function of state; the event loop
is the only place that does I/O — collection runs as tokio tasks that send results back as messages;
`view` only reads `App`. **Account attribution is the `CLAUDE_CONFIG_DIR`, never the logs** (logs
carry no identity).

## Conventions

- Design seams so core logic is pure and testable: config, ccusage parse/reduce, severity/format,
  alerts, keymap — all unit-tested without touching the OS or network (see `src/providers/claude/ccusage.rs`).
- Shell out via explicit **argv** (never `sh -c`); every external call and HTTP request has a timeout.
- Cost is a **NOTIONAL proxy, never a bill**. Limits are **% + reset, never "X of Y"**. `resets_at`
  is rendered verbatim. Alerts key off `utilization_pct`, never cost.

## Boundaries

- **Always**: run `./check.sh` green before calling a wave done. Follow `rules/`. Update the spec + CHANGELOG.
- **Ask first**: new external dependency; enabling the overlay by default; anything that writes to a
  Claude config dir beyond an atomic token rotation.
- **Never**: `unsafe`. `unwrap`/`expect`/`panic!` in runtime paths. Log or print an access/refresh
  token. Poll the overlay for a stale-token or opted-out account. Present notional cost as a real bill.

## Key Files

| File | Purpose |
|------|---------|
| `~/.config/tokenomics/tokenomics.toml` | Accounts + thresholds (source of truth) |
| `src/providers/claude/ccusage.rs` | ccusage JSON → `UsageSnapshot` (pure core) |
| `src/providers/claude/overlay.rs` | `/api/oauth/usage` parse + backoff (opt-in) |
| `src/ledger.rs` | Subscription ledger (billing dates) — read-through, TUI-only, third plane |
| `src/domain.rs` | `Account` / `UsageSnapshot` / `Limit` / `Provenance` contracts |
| `src/store.rs` | SQLite (WAL) — collector writes, TUI reads |
| `rules/_index.md` | Coding rules index · `rules/crossroads.md` task routing |
| `specs/README.md` | Spec index (one per wave) |
| `CHANGELOG.md` | `[Unreleased]` history |
