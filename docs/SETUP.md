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
account is monitored under its own config dir, and **attribution is that dir, never the logs**. If
you're running more than one Claude subscription, see [multi-account.md](multi-account.md) for how to
set up an isolated `CLAUDE_CONFIG_DIR` (e.g. `~/.claude-acct/<lane>`) per account without re-logging.
The per-provider shape:

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

# Kimi (Moonshot AI coding plan) — no local usage lane; identity is the NAME of an env var holding
# your API key (never the key itself). Limits-only, and dormant until you have a subscription key.
[[account]]
id = "kimi-k2"
label = "Kimi K2"
provider = "kimi"
api_key_env = "KIMI_API_KEY"
limits_overlay = true
```

Field rules `tok validate` enforces:

- `claude` / `codex` / `gemini` / `grok` require `config_dir` and reject `api_key_env`.
- `zai` / `kimi` require `api_key_env` (the **name** of an env var, e.g. `Z_AI_CODING_KEY`, not the
  key) and ignore `config_dir`.
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

## API billing (PAYG) overview

`tok` shows your **subscription** usage and a notional API-equivalent proxy — never a bill. But if you
also run **pay-as-you-go API keys** (an Anthropic org, an OpenAI org, a Moonshot balance), that is
**real money**: an actual bill or balance, tracked as a **fourth data plane** beside local usage, the
opt-in limits overlay, and the subscription ledger. It never mixes with the notional proxy, the %
gauges, or the subscription dates.

The plane has two lanes. **Live** figures (collector-polled, one per `[[api]]` entry) come from the
providers' own admin/billing endpoints — Anthropic's cost report summed to month-to-date spend,
OpenAI's organization costs, Moonshot's balance. **Manual** figures (a read-through TOML, exactly like
the ledger) fill the gaps no endpoint exposes: Gemini AI Studio, xAI without a management key,
console-only credit balances. Neither lane appears unless you configure it — an absent `[[api]]`
section and no manual path leave the plane silently off.

Two honesty rules hold across the whole plane: real money is a **bill or a balance, never the notional
cost proxy**, and the **% utilization gauges never become dollars** (spend-keyed alerts are out of
scope).

### Config

Add one `[[api]]` block per API-key account in `tokenomics.toml`. Identity is `api_key_env` — the
**NAME** of an env var holding the key (e.g. `ANTHROPIC_ADMIN_API_KEY`), never the key itself. Pollable
providers (anthropic/openai/moonshot/xai) require it; **gemini is manual-only** and **rejects** it (no
billing endpoint exists to call). `tok validate` enforces every rule.

```toml
[settings]
poll_local_secs = 10
poll_overlay_secs = 300
poll_billing_secs = 3600   # how often the collector polls billing endpoints — floor 300 (default hourly)
# api_manual_path = "~/tokenomics/api-billing.toml"   # manual figures — see "Manual figures" below

# Anthropic — admin billing API; month-to-date spend summed from daily cost buckets.
[[api]]
id = "anthropic-org"
label = "Anthropic Org"            # optional — defaults to id
provider = "anthropic"
api_key_env = "ANTHROPIC_ADMIN_API_KEY"   # NAME of the env var — never the key

# OpenAI — admin billing API; month-to-date spend summed from daily cost buckets.
[[api]]
id = "openai-org"
provider = "openai"
api_key_env = "OPENAI_ADMIN_API_KEY"      # NAME of the env var — never the key

# Gemini — AI Studio personal tier has no billing endpoint, so it is MANUAL-ONLY (no api_key_env);
# its figures come from api_manual_path.
[[api]]
id = "gemini-studio"
provider = "gemini"

# Moonshot (Kimi) — balance endpoint; current available balance.
[[api]]
id = "moonshot"
provider = "moonshot"
api_key_env = "MOONSHOT_API_KEY"          # NAME of the env var — never the key

# xAI — ships DORMANT until a management key exists: config-accepted, never fetched. The env var
# being unset is the expected state.
[[api]]
id = "xai"
provider = "xai"
api_key_env = "XAI_MANAGEMENT_KEY"        # NAME of the env var — never the key
```

The collector polls on `poll_billing_secs` and writes each figure into the store — a failed poll keeps
the last-good value and ages honestly (never a stale figure dressed up as fresh). Month-to-date spend
is the **UTC calendar month**, so a near-zero total in the first hours of a month is correct.

### Manual figures

For providers no endpoint exposes, keep a separate TOML of human-entered figures. It's **read-through
and TUI-only**, exactly like the subscription ledger: `tok` never writes it, the collector never reads
it, and nothing lands in SQLite — so it can live in a private repo. Point `tok` at it via either (env
wins):

```bash
# tokenomics.toml, under [settings]:
api_manual_path = "~/tokenomics/api-billing.toml"

# or an env override (beats api_manual_path; empty = ignored):
export TOKENOMICS_API_MANUAL=~/tokenomics/api-billing.toml
```

One `[[api_manual]]` row per figure. The `id` joins a `[[api]].id` **exactly** (case-sensitive, like
the ledger). `amount` is a real currency value (a TOML decimal; `tok` converts to cents with no `f64`
drift), `currency` a verbatim label (`USD`, `EUR`) — never converted, never summed across currencies.
An `as_of` older than **35 days** is rendered stale (kept, not dropped). There is deliberately no
email/account-name field.

```toml
# api-billing.toml — manual API-billing figures. ids must match tokenomics.toml [[api]] ids exactly.

[[api_manual]]
id = "gemini-studio"        # required — exact, case-sensitive match to an [[api]] id
figure = "balance"          # "balance" | "owed" | "mtd_spend"
amount = 12.34              # a real currency amount (a TOML decimal, e.g. dollars)
currency = "USD"            # verbatim label — never converted or summed across currencies
as_of = 2026-07-20          # the date you recorded this figure (a bare TOML date, no quotes)
# notes = "console credit snapshot"   # optional, free text — never an email or account name

[[api_manual]]
id = "xai"
figure = "owed"
amount = 4.50
currency = "USD"
as_of = 2026-07-20
```

Live figures win per figure; manual fills the gaps. Run `tok doctor` to see the resolved manual path
and provenance (`fresh` / `stale` / `missing`), any past-stale ids, each live figure's last-poll age
(or `never polled`), dormant entries, and manual ids that join no configured `[[api]]` — **never a
dollar amount**.

### On the board

Press **`o`** to toggle an **API overview** section (default hidden; session-only, like the `i` peek).
It renders one dense line per `[[api]]` account — balance / owed / month-to-date, each figure carrying
a plain-word provenance: `live 2m ago`, `manual 2026-07-20`, `· stale`, or `dormant (XAI_MANAGEMENT_KEY
unset)`. Real figures always show exactly two decimals and a currency glyph (`$12.34`, `€3.50`,
`CZK 1.00`) — a grammar deliberately disjoint from the notional proxy's `$335 (notional)`. **There is
no totals row** — mixed currencies are never summed.

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
  - Kimi:         no dir — identity is the NAME of an env var holding the API key; limits-only,
                  dormant until you have a subscription key
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
    provider = "claude"       # claude | codex | gemini | grok | zai | kimi
    config_dir = "~/.claude"  # required for claude/codex/gemini/grok; attribution is this dir
    limits_overlay = false    # keep OFF unless I explicitly opt in

  Rules you must honor:
  - claude/codex/gemini/grok: `config_dir` is REQUIRED, `api_key_env` is NOT allowed.
  - zai/kimi: `api_key_env = "<ENV_VAR_NAME>"` is REQUIRED, no `config_dir`; it only shows gauges with
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
  there. Fix the `config_dir`, or drop the account. (z.ai/kimi have no `config_dir` and are never
  checked.)
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
