# LLM run evidence — tokenomics

Manual/offline/spawned-agent LLM runs made on this project (per the global no-silent-spend
rule). In-app runs don't exist here (tokenomics only reads billing/usage APIs).

`when` is `YYYY-MM-DD HH:MM UTC` — time-of-day is required for new rows (rows below the
2026-08-03 line predate the rule and stay date-only; they are not backfilled).

| when (UTC) | what | lane | workload | billed cost |
|---|---|---|---|---|
| 2026-07-25 | specs 024+025 build: unimatrix swarm `tok024` — 3 gemini spike cards (all failed, lane fault), 9+6+2 build cards, 1 codex review card, 1 glm docs card | glm (Z.ai GLM Coding Pro sub) + grok (SuperGrok sub) + claude session (Max sub) + codex (ChatGPT sub); kimi PAYG NOT used | ~18 build cards, 1 review, 1 docs | $0 metered — all subscription lanes; gemini free-tier calls failed before tokens |
| 2026-07-25 | same build: session-side agents (3 Explore, 3 Plan [2 died at session limit], 1 structural-miss reviewer, orchestrator) | Claude Max session (subscription) | plan + review fan-out | $0 metered (subscription) |
| 2026-07-25 | live smoke: Anthropic Admin cost_report/usage_report, OpenAI organization/costs, Moonshot balance | direct HTTPS, Robert's own admin/API keys | spec 024 live verification (3 collector ticks + 3 probe scripts) | $0 — billing/usage reads are unmetered |
| 2026-07-26 | TUI design research workflow (7 agents: 5 design lenses + GCP billing export + completeness critic, ~332k subagent tokens) → `rules/tui-design.md` + `docs/ops/gemini-gcp-billing.md` | Claude Max session (subscription), session-side Workflow fan-out | web research + synthesis | $0 metered (subscription) |
| 2026-07-26 | spec-13 onboarding build: 2 Explore + pre-push scan (haiku) + 3 doc writers (sonnet ×2, haiku) + skills port (haiku, salvaged by orchestrator); wrote `docs/multi-account.md` here + `_refactor/onboarding/` in monorepo-development | Claude Max session (subscription), direct Agent fan-out | docs + gates | $0 metered (subscription) |
| 2026-08-26 08:00–08:15 | same-usage-bars troubleshoot (eletrixa/rob7): 2 Explore (widget source, account-dir identities) + 2 Plan (doctor/title-tag design, relogin runbook), orchestrator; implemented 11:40 in a resumed session (prior one died at session limit) | Claude Max session (subscription), direct Agent fan-out | diagnosis + design + build of `identity.rs` / doctor section / TUI tag | $0 metered (subscription) |
| 2026-09-04 13:10–13:40 | spec 026 native transcript scanner: orchestrator (Fable 5.1) wrote spec + `transcripts.rs` + adapter rewire; 2 spawned agents — `pm-docs` (sonnet: discovery / brainstorm / pre-mortem docs), `docs-026` (sonnet: README / SETUP / unit / CHANGELOG doc edits) | Claude Max session (subscription), direct Agent fan-out | design + build + docs | $0 metered (subscription) |
| 2026-11-14 09:05–09:30 UTC | 5h-row fix + review + whole-repo audit: orchestrator (Fable 5.2); 3 diff reviewers (simplify sonnet, bug review sonnet, silent-failure haiku); 5 audit agents (sonnet: collector/store, Claude lane, other lanes + billing, TUI, config/ledger/doctor/main); 3 fix agents (sonnet: transcript scanner offsets, billing/grok/zai parsers, config/ledger/main). ~921k subagent tokens. Also 6 manual `GET /api/oauth/usage` reads (4 accounts) to confirm `five_hour: null` | Claude Max session (subscription), direct Agent fan-out; overlay reads = direct HTTPS with the accounts' own OAuth tokens | diagnosis + review + audit + fixes | $0 metered (subscription); overlay reads unmetered |
