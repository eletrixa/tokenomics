# Setting up `tok`

This guide takes a fresh machine to a running dashboard. You end up with two files:

1. **`~/.config/tokenomics/tokenomics.toml`** — the accounts `tok` monitors (required).
2. **A subscription ledger** — the billing/lifecycle dates (purchase, renewal, cancellation)
   `tok` overlays on the board (optional, and can live in a separate private repo).

There are two ways to get there: do it by hand ([Manual quickstart](#manual-quickstart)), or paste
the [Claude Code setup prompt](#let-claude-code-set-it-up) into a Claude Code session and let it
interview you and write both files.

Cost on the board is a **notional API-equivalent proxy, never a bill**; limits are **% used + reset
time, never "X of Y"**. Nothing in your setup changes that — it only decides which accounts appear.

## Manual quickstart

### 1. Build the binary

```bash
cargo build --release      # -> target/release/tok
# or: cargo install --path .   (puts `tok` on your PATH)
```

A Rust toolchain (1.96+) and, for Claude accounts, a local [`ccusage`](https://github.com/ryoppippi/ccusage)
are the only prerequisites.

### 2. Write a starter config

```bash
tok init      # writes a commented ~/.config/tokenomics/tokenomics.toml (refuses if one exists)
```

`tok init` never clobbers an existing config — if you want to start over, remove the file first.

### 3. Add your accounts

Open `~/.config/tokenomics/tokenomics.toml` and add one `[[account]]` block per subscription. Each
account is monitored under its own config dir, and **attribution is that dir, never the logs**. The
per-provider shape:

```toml
[settings]
poll_local_secs = 10      # local (ccusage/JSONL) poll cadence — floor 5
poll_overlay_secs = 300   # overlay poll cadence — floor 60; only accounts with the overlay on use it
warn_pct = 75.0           # window utilization % -> warning
crit_pct = 90.0           # window utilization % -> critical  (must be > warn_pct)
# ledger_path = "~/subscriptions/subscriptions.toml"   # optional — see "The subscription ledger"

# Claude — reads local Claude Code JSONL via ccusage, once per account under this CLAUDE_CONFIG_DIR.
[[account]]
id = "claude-personal"
label = "Personal"
provider = "claude"
config_dir = "~/.claude"
limits_overlay = false    # opt-in, OFF by default (undocumented endpoint, your own OAuth token — ToS risk)

# Codex — reads the local sessions tree under this CODEX_HOME.
[[account]]
id = "codex-work"
label = "Codex"
provider = "codex"
config_dir = "~/.codex"

# Gemini — reads the local chats tree under this GEMINI_CLI_HOME. Usage-only: both gauges show n/a
# (no scriptable limits surface exists — never a fabricated daily quota).
[[account]]
id = "gemini-personal"
label = "Gemini"
provider = "gemini"
config_dir = "~/.gemini"

# Grok — reads inference events under this GROK_HOME (logs/unified.jsonl); the weekly gauge comes
# from the same log's billing lines (the CLI's own weekly-quota %). Session gauge stays n/a.
[[account]]
id = "grok-heavy"
label = "Grok"
provider = "grok"
config_dir = "~/.grok"

# z.ai (GLM) — no local usage lane; identity is the NAME of an env var holding your API key (never the
# key itself). Limits-only: turn the overlay on to see any gauges at all.
[[account]]
id = "zai-lite"
label = "z.ai GLM"
provider = "zai"
api_key_env = "Z_AI_CODING_KEY"
limits_overlay = true
```

Field rules `tok validate` enforces:

- `claude` / `codex` / `gemini` / `grok` require `config_dir` and reject `api_key_env`.
- `zai` requires `api_key_env` (the **name** of an env var, e.g. `Z_AI_CODING_KEY`, not the key) and
  ignores `config_dir`.
- Optional per-account keys: `color` (a named ratatui color or `#rrggbb`) and `active = false` (keep
  an account configured but off the board).
- A leading `~` in `config_dir` expands against `$HOME`.

### 4. Check it

```bash
tok validate      # schema + threshold + "does this config_dir exist" checks
tok doctor        # read-only diagnostics: config, credentials, ccusage, overlay, ledger
tok accounts      # list what's configured
```

### 5. Run it

```bash
tok collector     # background collector — writes the local SQLite store (~/.local/share/tokenomics/tokenomics.db)
tok               # launch the dashboard (reads the store; always instant)
```

`tok once --json` prints one snapshot per account without the store, if you just want the numbers.

## The subscription ledger

`tok` shows how much you've burned, but nothing about the **money clock** — when a subscription was
bought, when it renews, whether it's been cancelled but still paid through. That data has no API; it
lives in a small TOML file you maintain (the ledger), and `tok` renders it as a **third data plane**
beside local usage and the opt-in overlay.

The ledger is **read-only and TUI-only**: `tok` never writes it, the collector never reads it, and
none of it is ever persisted to SQLite. A malformed or mid-edit ledger degrades to a blank clause,
never a crash. Because it's just a git-versioned TOML file, it can live in a **separate private
repo** — point `tok` at it and it reads through.

### Schema

One `[[subscription]]` table per account. The `id` is the join key — it must **exactly** match an
account `id` in `tokenomics.toml` (no fuzzy or case-insensitive matching).

```toml
# subscriptions.toml — the subscription ledger. ids must match tokenomics.toml account ids exactly.

[[subscription]]
id = "claude-personal"       # required — exact match to Account.id
status = "active"            # required — "active" or "cancelled" (note the double L; "canceled" is rejected)
purchased = 2026-07-14       # optional — current-period start (a bare TOML date, no quotes)
renews = 2026-08-14          # optional — current-period end (drives the "renews in Nd" countdown)
verified = 2026-07-18        # optional — the date this row was last confirmed against the billing UI

[[subscription]]
id = "codex-work"
status = "active"
purchased = 2026-07-01
renews = 2026-08-01

[[subscription]]
id = "gemini-personal"
status = "cancelled"         # cancelled but still paid through — renders "· cancelled · ends in Nd"
cancelled_on = 2026-07-10
paid_through = 2026-08-10
```

- All dates are TOML **local dates** (`2026-08-14`), unquoted. Omit any date you don't know — `tok`
  never guesses a missing one.
- `cancelled_on` / `paid_through` are only meaningful when `status = "cancelled"`.
- Unknown fields (`plan`, `price`, `notes`, …) are ignored. A raw-email `account` field, if your
  upstream ledger has one, is **deliberately never read** — keep emails out of anything committed.

### Wire it up

Point `tok` at the file via either (env wins):

```bash
# tokenomics.toml, under [settings]:
ledger_path = "~/subscriptions/subscriptions.toml"

# or an env override (beats ledger_path; empty = ignored):
export TOKENOMICS_LEDGER=~/subscriptions/subscriptions.toml
```

Both unset = the ledger plane is **off** (no clauses, no warnings). Run `tok doctor` to see the
resolved path, provenance (`fresh` / `stale` / `missing`), any past-dated rows, and any id that
exists on one side but not the other.

## Let Claude Code set it up

Paste the block below into a Claude Code session **on the machine where your CLIs are installed**. It
interviews you and writes both files for you.

````markdown
You are setting up `tok` (tokenomics), a terminal dashboard that monitors my LLM **subscription**
accounts. Help me generate its two config files. Work carefully and NEVER read, print, echo, or copy
any API key, OAuth token, or credential — you only ever reference the NAME of an env var, never a
value.

Step 1 — detect what I have installed. Check which of these config dirs exist and report them:
  - Claude Code:  ~/.claude            (env CLAUDE_CONFIG_DIR)
  - OpenAI Codex: ~/.codex             (env CODEX_HOME)
  - Gemini CLI:   ~/.gemini            (env GEMINI_CLI_HOME)
  - Grok CLI:     ~/.grok              (env GROK_HOME)
  - z.ai (GLM):   no dir — identity is the NAME of an env var holding the API key
If I run more than one Claude account from separate dirs (e.g. ~/.claude-work), ask for each path.

Step 2 — interview me, one short question at a time:
  - For each account: a short stable `id` (e.g. "claude-personal"), a display `label`, the provider,
    and its config dir (or, for z.ai, the NAME of the env var holding the key — never the key).
  - Which accounts I have billing dates for, and for each: the purchase/renewal date, the plan, the
    price, and whether it's active or cancelled (and if cancelled, the paid-through date).

Step 3 — write `~/.config/tokenomics/tokenomics.toml`:

    [settings]
    poll_local_secs = 10      # floor 5
    poll_overlay_secs = 300   # floor 60
    warn_pct = 75.0
    crit_pct = 90.0           # must be > warn_pct
    ledger_path = "<path to the ledger from step 4>"

    # one [[account]] per subscription:
    [[account]]
    id = "claude-personal"
    label = "Personal"
    provider = "claude"       # claude | codex | gemini | grok | zai
    config_dir = "~/.claude"  # required for claude/codex/gemini/grok; attribution is this dir
    limits_overlay = false    # keep OFF unless I explicitly opt in

  Rules you must honor:
  - claude/codex/gemini/grok: `config_dir` is REQUIRED, `api_key_env` is NOT allowed.
  - zai: `api_key_env = "<ENV_VAR_NAME>"` is REQUIRED, no `config_dir`; it only shows gauges with
    `limits_overlay = true`.
  - Optional per-account: `color` (named or #rrggbb) and `active = false`.

Step 4 — write a subscription ledger TOML (put it at ~/subscriptions/subscriptions.toml unless I say
otherwise) with one `[[subscription]]` per account that HAS billing dates:

    [[subscription]]
    id = "claude-personal"   # must EXACTLY match an account id above
    status = "active"        # "active" or "cancelled" — double L; never "canceled"
    purchased = 2026-07-14   # bare TOML date, unquoted; omit any date I don't know
    renews = 2026-08-14
    # cancelled_on / paid_through only when status = "cancelled"

  Do NOT put any email address in this file. Omit dates I don't provide — never invent one.

Step 5 — verify:
  - Run `tok validate` and fix anything it flags.
  - Run `tok doctor` and summarize the ledger section (resolved path, provenance, any divergence).
  - Show me both files. Do not run the collector or launch the TUI unless I ask.
````

## Troubleshooting

- **`tok validate` fails** — read the `✗` lines; each names the account and the problem (missing
  `config_dir`, `api_key_env` on the wrong provider, `crit_pct` not above `warn_pct`, a duplicate id).
- **`tok validate` says a `config_dir` does not exist** — the path is wrong or the CLI isn't set up
  there. Fix the `config_dir`, or drop the account. (z.ai has no `config_dir` and is never checked.)
- **Board shows n/a gauges for Gemini** — expected: Gemini is usage-only, with no scriptable limits
  surface. `tok` shows `n/a` rather than a fabricated quota. Grok's weekly gauge fills in after the
  grok CLI has run at least once in the current period (it reads the CLI's own billing log); only
  Grok's *session* gauge is permanently n/a.
- **`tok doctor` shows `ledger: not configured`** — neither `ledger_path` nor `TOKENOMICS_LEDGER` is
  set. Add one, or ignore it if you don't want the money-clock overlay.
- **`tok doctor` shows `ledger: <path> [missing]`** — the path resolved but the file couldn't be
  read. Check the path and permissions; a `[stale]` state instead means the file is mid-edit or
  malformed and `tok` is holding the last good parse.
- **A ledger row shows no clause on the board** — the `id` doesn't exactly match an account id.
  `tok doctor` lists the divergence in both directions.
