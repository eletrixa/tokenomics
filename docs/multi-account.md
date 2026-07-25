# Running multiple Claude accounts without re-logging

`tok` monitors N Claude Max subscription accounts on one machine. This doc is the other half: how to
actually **run** N accounts in parallel — logging in once per account, ever, and switching between
them by flipping an environment variable instead of logging out and back in.

## Why this exists

Each Claude Max subscription carries its own 5h and weekly rate-limit budget, independent of every
other subscription. N logged-in accounts is N times the agent throughput you can burn through at
once — but Claude Code's login is tied to whichever config directory it started with, and the
default setup points every terminal at the same one (`~/.claude`). Point two tabs at the same config
dir and the second login silently evicts the first; you're back to logging in and out every time you
switch accounts, which is the single most annoying part of this workflow to repeat.

The fix: give each account its own config directory and log in exactly once per directory. The OAuth
refresh token then keeps that login alive indefinitely — switching accounts becomes switching an env
var, not re-authenticating.

## The isolation model

`CLAUDE_CONFIG_DIR` is the entire boundary Claude Code respects for account identity. Point it at a
fresh directory and Claude Code treats that as a brand-new install: its own `.credentials.json` (the
OAuth tokens, mode 0600), its own `.claude.json` (holds `oauthAccount` — the identity Claude Code
actually trusts — plus onboarding state), its own `settings.json`, and its own per-account state
(todos, shell snapshots, file-history cache, and so on).

None of that needs to duplicate skills, agents, commands, memory, or session history — those live
under `~/.claude` and get symlinked into every account directory, so every account shares one brain
and only the login differs.

Convention: one directory per account under `~/.claude-acct/<lane>` (lane names below are
placeholders — `lane-a`, `work`, `personal`, whatever you call yours):

```
~/.claude-acct/
├── lane-a/
│   ├── .credentials.json      # real file, 0600 — OAuth tokens for THIS account only
│   ├── .claude.json           # real file — oauthAccount identity + onboarding state
│   ├── settings.json          # symlink -> ~/.claude/settings.json
│   ├── skills   -> ~/.claude/skills
│   ├── agents   -> ~/.claude/agents
│   ├── projects -> ~/.claude/projects
│   └── ...                    # commands, plugins, helpers, tasks, teams, sessions,
│                               # planning, plans, history.jsonl, CLAUDE.md — all symlinks
└── lane-b/
    └── ...                    # same shape, different login
```

Two consequences worth knowing:

- **Attribution is the directory, never the logs.** A session's JSONL carries no account identity —
  tokenomics (and anything else that needs to know "which account") reads it off `CLAUDE_CONFIG_DIR`,
  never by parsing log content. This is the same rule `tok` follows internally.
- Because sessions, projects, and history are shared symlinks, anything that scans for sessions only
  needs to look in one place (`~/.claude`) — only identity is actually per-account.

## One-time setup per account

For each account:

1. Create the directory and build the symlink farm — link everything shareable back to `~/.claude`;
   leave `.credentials.json` / `.claude.json` for the login step to create.
2. Optionally seed `.claude.json` from your existing `~/.claude.json`, stripped of identity keys
   (`oauthAccount`, `userID`, `hasCompletedOnboarding`, `lastOnboardingVersion`, any `cached*` /
   `*Cache` keys). This carries over MCP servers and per-project trust so the new lane doesn't
   re-prompt for everything, while still forcing the first-run wizard — including the login step —
   because the identity keys are gone.
3. Launch Claude Code with `CLAUDE_CONFIG_DIR` pointed at that directory and run the login flow. Pick
   **"Claude account with subscription"**, not the API/console option — a console login bills an API
   key, not the Max subscription's 5h/weekly pool tokenomics is built to watch.
4. Confirm: the directory's `.claude.json` should now have an `oauthAccount.emailAddress`. Check for
   that value, not just file existence — a half-initialized directory launches unauthenticated and
   silently falls back to API billing instead of failing loudly.

That's the whole setup. The refresh token in `.credentials.json` renews itself; you do not repeat this
per session, per day, or across machine reboots.

## The launcher (reference implementation)

Everything past this point is a shell convenience layer around one env var. Adapt the shape, not the
names:

```bash
# The entire boundary is this one line:
cl() { CLAUDE_CONFIG_DIR="$HOME/.claude-acct/$1" claude "${@:2}"; }
# usage: cl lane-a "your prompt"   -> launches under the lane-a identity
```

A production version typically layers on:

- **Identity re-gate on every launch** — before handing off to `claude`, read the target directory's
  `.claude.json` and compare `oauthAccount.emailAddress` against an expected-email map for that lane;
  refuse to launch on a mismatch instead of silently running as the wrong account. This catches an
  in-session `/login` (browser held a different account) that rewrote the wrong directory's identity.
- **Env scrubbing** — `env -u ANTHROPIC_API_KEY -u ANTHROPIC_AUTH_TOKEN -u CLAUDE_CODE_OAUTH_TOKEN`
  ahead of the real invocation (see [Pitfalls](#pitfalls) below for why).
- **Tab-sticky pinning** — a companion command that exports an account-name env var and stashes the
  last pick in a `.last` file, so a bare launch with no argument reuses whichever lane this tab (or
  the last tab) picked, instead of asking every time.
- **A terminal tab-title hook** (optional) — emit a terminal user-var carrying the active lane name so
  the tab itself shows which account it's pinned to at a glance.

None of this is required to get parallel accounts working — the one-liner above is sufficient. The
extras exist because the pitfalls below are easy to hit by accident once you're juggling more than two
lanes.

## Daily use

Once every account has logged in once, switching accounts is just picking a different directory:

```bash
cl lane-a "your prompt"      # runs under lane-a's identity, no login prompt
cl lane-b "your prompt"      # different tab, different account, running concurrently
```

Each invocation is an independent Claude Code process with its own 5h/weekly budget — running
`lane-a` never touches `lane-b`'s usage. That's the entire payoff: N subscriptions become N agent
lanes you can burn in parallel, instead of one account you ration across every task.

## Wiring into tokenomics

Each `CLAUDE_CONFIG_DIR` set up above is exactly one `[[account]]` block in `tokenomics.toml` —
`config_dir` **is** the identity tokenomics keys off, the same as it's the identity Claude Code itself
keys off:

```toml
[[account]]
id = "claude-lane-a"
label = "Lane A"
provider = "claude"
config_dir = "~/.claude-acct/lane-a"
```

Add one block per lane and the board shows all of them side by side — usage, % utilization, and reset
countdown, independently per account. See [SETUP.md](SETUP.md) for the rest of the config.

## Pitfalls

- **Nested shells inherit the parent's env var.** If a shell already has `CLAUDE_CONFIG_DIR` exported
  and you launch a *different* lane's Claude Code from inside a session spawned by the first, the
  child silently inherits the wrong config directory unless your launcher scrubs it first. Set it
  explicitly on every launch rather than assuming a clean environment.
- **An API-key env var wins over OAuth.** `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, and
  `CLAUDE_CODE_OAUTH_TOKEN` all take priority over whatever is logged into the config directory. If any
  of these is set anywhere in your environment — a leftover export, a sourced `.env` — every request
  bills to that key's pay-as-you-go balance instead of drawing from the subscription's included usage,
  and tokenomics' numbers for that account stop meaning anything. Unset them explicitly before launch.
- **Symlink divergence.** If anything ever writes a *real* file over one of the shared symlinks inside
  an account directory (some atomic-rewrite patterns replace-then-rename instead of writing through the
  link), that account silently forks its own private copy of whatever it was — skills, settings,
  history — and stops seeing updates made through `~/.claude`. Check periodically:

  ```bash
  for item in skills agents commands settings.json history.jsonl; do
      [[ -L ~/.claude-acct/lane-a/$item ]] || echo "diverged: $item (real file, not symlink)"
  done
  ```

  If one has diverged, merge anything worth keeping back into `~/.claude/<item>`, delete the
  account-local copy, and re-run the symlink-farm step to relink it.
