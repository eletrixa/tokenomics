# Specs — Tokenomics

Spec-driven TDD: one spec per wave. Each wave runs
**spec → 🔴 red → 🟢 green → ♻ refactor-for-specs → ♻ refactor-for-rules**.
Status: **Draft** (don't implement) → **Active** (implement against this) → **Done** (acceptance criteria pass).

| # | Spec | Wave | Status |
|---|------|------|--------|
| 000 | [scaffold-and-conventions](000-scaffold-and-conventions.md) | Scaffold + strict lints + `tok --help` | Done |
| 001 | [config-and-accounts](001-config-and-accounts.md) | `tokenomics.toml` parse / validate | Done |
| 002 | [provider-seam-and-claude-collector](002-provider-seam-and-claude-collector.md) | ccusage collector + Runner + ProviderAdapter | Done |
| 003 | [limits-severity-and-format](003-limits-severity-and-format.md) | severity / merge / format (pure) | Done |
| 004 | [sqlite-store](004-sqlite-store.md) | rusqlite WAL store | Done |
| 005 | [collector-loop-and-daemon](005-collector-loop-and-daemon.md) | multi-account loop + guards + daemon | Done |
| 006 | [tui-dashboard](006-tui-dashboard.md) | ratatui board + keymap | Done |
| 007 | [authoritative-overlay-optin](007-authoritative-overlay-optin.md) | `/api/oauth/usage` opt-in + tokens | Done |
| 008 | [alerts](008-alerts.md) | edge-triggered alerts | Done |
| 009 | [e2e-verification-and-doctor](009-e2e-verification-and-doctor.md) | `tok once` / `doctor` + QA | Done |
| 010 | [aggregate-burn-and-per-account-rate](010-aggregate-burn-and-per-account-rate.md) | header aggregate burn bar + per-account tokens/hour + shared-`projects/` doctor guard | Active |
| 011 | [refresh-freshness-hardening](011-refresh-freshness-hardening.md) | collector liveness + local-plane freshness + degrade-to-derived + concurrent overlay + bounded aggregate/retention | Done |
| 012 | [waiting-for-reset](012-waiting-for-reset.md) | expired limits render "waiting for reset" + stop alarming; idle/failed collects still re-evaluate | Done |
| 013 | [codex-provider](013-codex-provider.md) | Codex adapter: sessions-JSONL usage + `codex app-server` rate-limits overlay | Done |
| 014 | [hide-inactive-accounts](014-hide-inactive-accounts.md) | `active = false` accounts: unmonitored, hidden by default, `i` to peek | Done |
| 015 | [config-hot-reload](015-config-hot-reload.md) | collector + TUI hot-reload config on change; doctor flags config/binary divergence; age-aware overlay hint | Done |
| 016 | [init-subcommand](016-init-subcommand.md) | `tok init` writes a starter `tokenomics.toml` (never clobbers); embedded from `tokenomics.example.toml`; missing-config commands hint at `tok init` | Done |
| 017 | [subscription-dates](017-subscription-dates.md) | ledger plane (third data plane): subscription start / renewal / end dates on the header line; doctor section; PII gate in check.sh | Active |

Scope now: **Claude** (6 accounts) + **Codex** (1 account, spec 013). Gemini / Grok remain future
adapters implementing the same `ProviderAdapter` trait.

## Future spec candidates (from the 2026-07-15 discovery/premortem pass)

- **Multi-instance guard** — nothing stops two collectors racing on one store (heartbeat pid is
  written, never checked for liveness/identity flapping).
- **Systemd watchdog** — `Restart=on-failure` misses alive-but-hung; heartbeat data exists for
  `sd_notify`/`WatchdogSec` wiring.
- **Crash cause surfacing** — the collector-down banner says dead, never why (no last-panic
  breadcrumb persisted).
- **Codex misconfig trap** — a typo'd `config_dir` renders as perpetual idle (spec 013 treats a
  missing `sessions/` as idle by design); a logged-out Codex account pays a real subprocess
  timeout every overlay pass (no cheap warmth check exists).
- **Failure-cause in hints** — the TUI distinguishes token-stale vs stalled, but not 429-backoff
  vs transport failure; the collector knows, the store doesn't.
