# LLM run evidence — tokenomics

Manual/offline/spawned-agent LLM runs made on this project (per the global no-silent-spend
rule). In-app runs don't exist here (tokenomics only reads billing/usage APIs).

| when (UTC) | what | lane | workload | billed cost |
|---|---|---|---|---|
| 2026-07-25 | specs 024+025 build: unimatrix swarm `tok024` — 3 gemini spike cards (all failed, lane fault), 9+6+2 build cards, 1 codex review card, 1 glm docs card | glm (Z.ai GLM Coding Pro sub) + grok (SuperGrok sub) + claude session (Max sub) + codex (ChatGPT sub); kimi PAYG NOT used | ~18 build cards, 1 review, 1 docs | $0 metered — all subscription lanes; gemini free-tier calls failed before tokens |
| 2026-07-25 | same build: session-side agents (3 Explore, 3 Plan [2 died at session limit], 1 structural-miss reviewer, orchestrator) | Claude Max session (subscription) | plan + review fan-out | $0 metered (subscription) |
| 2026-07-25 | live smoke: Anthropic Admin cost_report/usage_report, OpenAI organization/costs, Moonshot balance | direct HTTPS, Robert's own admin/API keys | spec 024 live verification (3 collector ticks + 3 probe scripts) | $0 — billing/usage reads are unmetered |
