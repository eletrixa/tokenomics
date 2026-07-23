# tokenomics (`tok`)

**A terminal dashboard for your LLM subscription accounts — token usage, limit % utilization, and time-to-reset, at a glance.**

<!-- TODO: demo GIF docs/demo/board.tape via VHS -->

`tok` watches the LLM subscriptions you already pay for (Claude Max/Pro, OpenAI Codex) and shows,
per account, how much you've burned, how close each rate-limit window is, and when it resets — all
from data already on your machine. There is no dollar bill on a flat-fee subscription, so `tok`
never pretends there is: cost is a **notional API-equivalent proxy**, and limits are **% used +
reset time**, never "X of Y left".

## Features

- **Multi-account board** — every configured account on one screen, each attributed to its own
  isolated config dir (never guessed from logs).
- **5h + weekly gauges with live reset countdowns** — session and weekly windows as % utilization,
  with the reset timestamp rendered verbatim.
- **Alerts on % utilization** — warn/critical thresholds key off window utilization, never cost.
- **Background collector + SQLite store** — a detached collector writes a local WAL database; the
  TUI only reads it, so the dashboard is always instant.
- **Config hot-reload** — edit `tokenomics.toml` while the collector runs; it picks up account
  changes without a restart and surfaces any collector/config divergence.
- **`tok doctor`** — read-only diagnostics for config, credentials, `ccusage`, and the overlay.
- **Claude + Codex providers** — Claude via `ccusage`/JSONL, Codex via its local sessions tree;
  the provider seam is built to extend.

## Install

```bash
cargo install --path .    # builds and installs the `tok` binary
```

Prebuilt binaries are planned. For now, a Rust toolchain (1.96+) and a local `ccusage` (for Claude
accounts) are the only prerequisites.

## Quick start

```bash
tok init          # write a starter tokenomics.toml to ~/.config/tokenomics/
$EDITOR ~/.config/tokenomics/tokenomics.toml   # add your accounts
tok collector     # run the background collector (writes the local store)
tok               # launch the dashboard
```

Other commands: `tok validate` (check the config), `tok accounts` (list what's configured),
`tok once --json` (one snapshot per account as JSON), `tok doctor` (diagnostics).

For a step-by-step walkthrough — per-provider account snippets, the optional subscription-dates
ledger, and a paste-in Claude Code prompt that writes both config files for you — see
[`docs/SETUP.md`](docs/SETUP.md).

Paths are cwd-independent: config at `~/.config/tokenomics/tokenomics.toml`, store at
`~/.local/share/tokenomics/tokenomics.db`. Override either with `$TOKENOMICS_CONFIG` /
`$TOKENOMICS_DB` for local development.

## Example config

This is exactly what `tok init` writes (embedded from
[`tokenomics.example.toml`](tokenomics.example.toml)):

```toml
# Starter tokenomics.toml — written by `tok init`. Edit to add your accounts, then run
# `tok validate` to check it. Lives at ~/.config/tokenomics/tokenomics.toml ($TOKENOMICS_CONFIG overrides).

[settings]
poll_local_secs = 10      # ccusage/local poll cadence
poll_overlay_secs = 300   # overlay poll cadence (only used by accounts with the overlay on)
warn_pct = 75.0           # window utilization % → warning
crit_pct = 90.0           # window utilization % → critical

[[account]]
id = "claude-personal"
label = "Personal"
provider = "claude"
config_dir = "~/.claude"  # this account's CLAUDE_CONFIG_DIR — attribution comes from the dir
limits_overlay = false    # opt-in, OFF by default — see the README overlay notice (undocumented
                          # Anthropic endpoint; using your OAuth token there is at your own ToS risk)

# A second provider — Codex reads its local sessions tree under CODEX_HOME.
# Uncomment and adjust to monitor an OpenAI Codex subscription too:
# [[account]]
# id = "codex-work"
# label = "Codex"
# provider = "codex"
# config_dir = "~/.codex"
```

Each account is monitored under its own config dir. Optional per-account keys: `color` (a named or
`#rrggbb` gauge color) and `active = false` (keep an account configured but off the board).

## What it reads & privacy

Everything stays on your machine. `tok` never sends your usage anywhere.

- **Claude** — reads token usage from the local Claude Code JSONL logs
  (`<config_dir>/projects/**/*.jsonl`) via the [`ccusage`](https://github.com/ryoppippi/ccusage)
  CLI, once per account by setting `CLAUDE_CONFIG_DIR`.
- **Codex** — reads `<CODEX_HOME>/sessions/**` rollout events directly.
- **Store** — a local SQLite database (`~/.local/share/tokenomics/tokenomics.db`) that only this
  tool reads and writes.

The account's credentials file is read **only if you enable the opt-in overlay** (below), and even
then only to reuse the OAuth access token Claude Code already maintains. Tokens are **never logged,
never printed, and never stored** by `tok`.

## The overlay (unofficial, off by default)

Authoritative 5h/weekly percentages and reset times are only available server-side. `tok` can fetch
them through an **opt-in overlay** — but you should understand what that means before enabling it:

> **Notice.** The overlay polls an **undocumented Anthropic endpoint** using your own consumer OAuth
> token. Per Anthropic's 2026 Consumer-Terms clarification, consumer OAuth tokens in third-party
> tools are **not permitted** — enabling the overlay is **at your own risk**, and account
> enforcement has occurred elsewhere in the ecosystem. It is **off by default**, and the local plane
> (token usage + a reconstructed 5h window) needs no network at all.

When the overlay is off — or when it hits a 429/failure — `tok` degrades silently to local,
provenance-tagged estimates. The local plane is the ToS-safe core; the overlay is a labeled extra
you consciously opt into per account.

## Cost framing

Costs shown are **notional API-equivalent values, never a bill.** A flat-fee subscription has no
per-token charge; the number is a usage proxy computed from public list prices, useful only for
comparing burn across accounts and time.

## Credits

The Claude local-usage plane is powered by [`ccusage`](https://github.com/ryoppippi/ccusage) (MIT) —
`tok` shells out to it for token accounting.

## Not affiliated

Not affiliated with, endorsed by, or sponsored by Anthropic or OpenAI. Claude is a trademark of
Anthropic; Codex is a product of OpenAI.

## Maintenance

Passively maintained — this is a personal daily-driver. Issues and PRs get best-effort review; open
an issue before starting a large change.

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the
work by you, as defined in the Apache-2.0 license, shall be dual-licensed as above, without any
additional terms or conditions.
