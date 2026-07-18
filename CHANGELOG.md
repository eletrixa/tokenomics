# Changelog

All notable changes to Tokenomics are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com); this project adheres to Semantic Versioning.
Agents maintain the `[Unreleased]` section as work lands; **only the user cuts a release**.

## [Unreleased]

### ADDED
- **Subscription dates from the ledger — a third, read-through-only data plane (spec 017).** A
  git-versioned subscription ledger (external TOML, `id`/`status`/`purchased`/`renews`/
  `cancelled_on`/`paid_through`) already tracks which Max/Codex accounts are active vs. cancelled —
  previously invisible in Tokenomics, so a cancelled account looked identical to a live one until the
  overlay started failing. The header line now appends a clause per state: an active account with a
  known `renews` date shows `· period 2026-07-14 → · renews in 27d (2026-08-14)`; a cancelled account
  shows `· cancelled · ends in 4d (2026-07-22)` while still paid-through, `· cancelled · ended
  2026-07-22` once lapsed, or a bare `· cancelled` when the date is unknown — never a placeholder, and
  never a negative countdown for a stale `renews`. Path resolution is `$TOKENOMICS_LEDGER` >
  `[settings] ledger_path` > off (unconfigured is a permanent, silent, valid state — nothing renders,
  nothing warns). The join to `Account.id` is exact-string-match only; a near-miss id gets no clause
  and shows up in `tok doctor`. The ledger is hot-reloaded per tick like `tokenomics.toml` (keep-
  last-good on a mid-edit/garbage file, `Stale` until it's fixed); a malformed row (e.g. the
  `"canceled"` typo) degrades only that row, never the whole read. **`Account.active` (spec 014) and
  the ledger's `status` stay independent bits** — the ledger never drives monitoring on/off, and the
  config flag never drives the date display. `tok doctor` gains a ledger section: resolved path +
  provenance, past-dated rows, failed-parse rows with reason, and join divergence in both directions.
  Read-through only — never written by Tokenomics, never persisted to SQLite, and the collector never
  reads it (dates are a render concern). `tok accounts` / `tok once --json` are byte-identical this
  wave (golden-snapshot enforced). `check.sh` gained a PII gate (`.pii-allowlist`) so no real email
  ever lands in a committed fixture.
- **A verified pill on the subscription clause (spec 018).** The ledger now carries `verified =
  <date>` — the date an agent last confirmed a row against the provider's billing web UI. A
  verified-current row (`verified >= purchased`, or within 31 days when `purchased` is absent) gets
  a dim-green ` ✓ 2026-07-18` appended to its FULL-tier clause and a trailing ` ✓` on its COMPACT
  clause, so a human-typed renewal date and a web-verified one read differently on the board. The
  pill never appears on a stale-`renews` or derived-`ended` clause (a contradiction otherwise), and
  never in MICRO. FULL degrades in one more step than before: pill's own date drops first (bare `✓`
  stays) → start segment → absolute date → whole clause, still never truncating a date mid-string.
  `tok doctor` annotates every ledger row matched to a config account: `verified <date> (current)`,
  `verified <date> (outdated — before current period)`, or `human-entered (no verified)`. `tok
  accounts` / `tok once --json` stay byte-identical (golden-snapshot enforced).
- **`tok init` writes a starter config (spec 016).** On a fresh machine every command used to fail
  with a bare `cannot read config …` and no next step. `tok init` now writes a commented starter
  `tokenomics.toml` (one Claude account, a commented Codex account, thresholds, and the overlay
  opt-in left OFF with a pointer to the ToS notice) to the resolved config path, creating the parent
  dir; it refuses (exit 1) rather than overwrite an existing config. The starter is embedded from
  `tokenomics.example.toml` via `include_str!`, so the file, the subcommand, and the README example
  can never drift. Any config-loading command that hits a missing file now suggests `tok init`.
- **Config hot-reload — the collector and the TUI pick up `tokenomics.toml` edits without a
  restart (spec 015).** Both long-running processes used to read the config once at startup and
  silently diverge from every later edit (live incident: an account flipped back to `active`
  stayed unmonitored for 5+ hours because the running collector still held the old config). Each
  process now polls the file on its existing tick — triggering on any content change (hash, not
  just mtime, so `cp -p`/`rsync --times` edits still land) — re-validates with the same rules as
  startup, and swaps the whole config: account add/remove/`active` flips, `limits_overlay` flips,
  warn/crit thresholds, and poll cadences all take effect within one tick. A bad edit keeps the
  last-good config running (collector warns once per distinct bad content; the daemon never
  crashes on a config typo). Overlay/collect results that land after a deactivating reload are
  dropped, never stamped onto the deactivated account.
- **`tok doctor` now detects a diverged or outdated collector (spec 015).** The collector stamps
  the resolved config path + content mtime and its own binary path + mtime into the heartbeat at
  startup and on every successful reload. Doctor compares against the *recorded* paths (immune to
  `$TOKENOMICS_CONFIG` differing between environments): a config newer than what the collector
  loaded — persisting past the reload window — means the edit fails validation (`tok validate`);
  a binary newer than the running process means a rebuild needs a collector restart. Doctor opens
  the store strictly read-only and stays silent when it cannot know.
- **The weekly hint now ages: `n/a (overlay silent 5d ago)` instead of a perpetual
  `waiting for overlay` (spec 015).** An account whose overlay *used to* succeed but has gone
  quiet with no failed attempts recorded (the exact signature of a collector not polling it) no
  longer claims to be freshly waiting; a genuinely never-fetched account keeps the honest waiting
  hint.
- **Codex (OpenAI ChatGPT subscription) is now a monitored provider (spec 013).** A `[[account]]`
  with `provider = "codex"` and `config_dir` = that account's `CODEX_HOME` (default `~/.codex`) is
  collected end-to-end. Usage (the ToS-safe local plane) is read straight off
  `$CODEX_HOME/sessions/**/rollout-*.jsonl` — the per-turn `last_token_usage` deltas of the trailing
  5h are summed into the same normalized token buckets as Claude (no subprocess, no network; a
  missing `sessions/` dir is idle, not an error). Codex has no honest cost basis, so `cost_notional`
  stays `None` and never poisons the fleet cost sum, and it exposes no local block, so there is **no
  derived session guess** — without the overlay the session gauge is honestly `n/a`. Limits (the
  authoritative, opt-in plane) come from `codex app-server`'s JSON-RPC `account/rateLimits/read`:
  `primary` → the 5h session and `secondary` → the weekly-all gauge, with real `usedPercent` and
  verbatim reset times, tagged `authoritative` (Codex has no per-model scoped weeklies). Opted-in
  Codex accounts ride the existing overlay cadence (same per-fetch cap, tick budget, failure backoff,
  and TTL demotion to derived); the app-server exchange is argv-only with `CODEX_HOME` pinned, stdin
  held open, one hard timeout, and `kill_on_drop` — no token is ever read, logged, or stored (Codex
  auth stays inside the binary). `tok once` prints Codex accounts (tokens, no cost line);
  `tok doctor` reports Codex checks (`config_dir` + `sessions/` present, `auth.json` present by
  existence only, `codex --version`, and — opted-in + active — an app-server reachability probe); the
  TUI renders a Codex account with the same row grammar. Claude accounts are entirely untouched — the
  `/api/oauth/usage` overlay, token warmth, and shared-`projects/` checks all stay Claude-only.
- **Hide inactive (unsubscribed) accounts, with a key to peek (spec 014).** `[[account]]` gains
  `active: bool` (default true — existing configs unaffected); setting `active = false` marks an
  account unsubscribed/paused without deleting its config block, colour, or stored history. The
  collector skips an inactive account entirely on both cadences (no ccusage collect, no overlay
  fetch — a cancelled subscription 429s forever, so polling it is pure noise); `tok once` omits it;
  `tok doctor` still reports it but labels it `INACTIVE` and skips its overlay probe; `tok accounts`
  marks it `(inactive)`. The dashboard hides it by default from the rows, the alert banner, the warn
  count, and the fleet header reductions (shared usage / worst provenance / oldest refresh) so a dead
  account can never pin a stale badge on the fleet line or fire the banner. Press **`i`** to peek: the
  account reappears dimmed and tagged `(inactive)` in its title, still excluded from the banner/fleet
  (display-only) — press `i` again to hide it; selection stays on a visible row across the toggle.
- **A dead/blocked account now flags `wk n/a (overlay stalled — check account)` instead of sticking
  on `waiting for overlay` forever.** A cancelled subscription 429s `/api/oauth/usage` on every pass
  (with a large `retry-after`), so the overlay never delivers and the weekly line used to read
  "waiting for overlay" indefinitely — indistinguishable from a normal startup. The collector now
  records a per-account *failing-since* marker (set on the first failed pass, cleared on the next
  success — the signal it previously swallowed at the `eprintln!`), and the row flags "check account"
  once that failure has been continuous for 15 minutes. A live account's transient 429 clears well
  within the grace, so only a sustained outage trips it. No new network and no ToS change — it just
  surfaces the overlay error we already saw. (There is no subscription end-date anywhere readable, so
  this is a *"looks dead"* flag, not a countdown.)

### FIXED
- **A stale token no longer wipes the board to `n/a (token stale — open Claude)` — the last-known
  limits stay visible with a live countdown.** The stale-overlay demotion (spec 011 §C) used to
  *drop* every authoritative row past the TTL, so an account whose token expired lost its weekly /
  scoped gauges entirely, even though the store knew the last values and each `resets_at`. The
  demotion now re-ranks those rows to `estimate` instead of deleting them: the frozen percent keeps
  rendering, the reset countdown keeps ticking down (recomputed every draw), a fresh derived session
  still out-ranks the demoted row, a recovered overlay wins authority back, and once a demoted row's
  reset passes it goes dormant via the existing `waiting for reset` machinery. The `n/a` hints now
  only appear for accounts that never had an overlay pass.
- **Opening Claude for a stale-token account now clears `wk n/a (token stale — open Claude)` within
  one local tick, not up to 5 minutes.** Token warmth was re-checked only on the periodic overlay
  pass (`poll_overlay_secs`, default 300s), so a freshly re-logged-in account kept reading "stale"
  until the next pass. The collector now re-checks not-warm opted-in accounts on the frequent local
  tick and fires the overlay fetch the moment the credentials file rotates warm — a still-stale
  account stays a silent, network-free no-op; already-warm accounts remain owned by the periodic
  pass. (Idle, logged-out accounts still say "open Claude" — auto-refresh stays out of scope per
  RESEARCH §8 / spec 012.)
- **A configured-but-never-collected account no longer shows `store read error: Query returned no
  rows`.** `latest_limits` looked up the account's provider with a hard `query_row`; before the
  collector had ever written that account's row (new account, or one blocked by a stale token), the
  lookup errored and blanked the whole account row. It now degrades to empty limits (`wk n/a`) like
  every other reader already does.
- **A maxed-out account no longer loses its weekly limits to an idle session (spec 007).** When an
  account has no active 5h session, `/api/oauth/usage` sends `"resets_at": null` on the session
  entry; the overlay parser required a string, so the *whole body* failed
  (`malformed usage body: invalid type: null…`) and the account fell back to
  `wk n/a (enable overlay)` — hiding a 95% weekly / 99% Fable crit exactly when it mattered most
  (observed live on one account). A null `resets_at` now maps to "no countdown": the gauge
  renders its percent and severity without a `resets` tail, and the weekly + scoped (Fable) rows
  survive.
- **The weekly `n/a` fallback stops nudging "enable overlay" at accounts that already opted in.**
  It now says why the gauge is missing: `n/a (enable overlay)` (opted out),
  `n/a (token stale — open Claude)` (overlay on, token expired), or `n/a (waiting for overlay)`
  (overlay on, first pass pending).
- **Expired limits go dormant instead of alarming for days (spec 012).** A limit whose reset time
  has passed describes a window that already reset — yet the board kept drawing the frozen percent,
  the crit colour, and the `worst: …` banner off it indefinitely (observed: an account at
  "100% crit" for a day while its real usage was ~0%). The moment a countdown crosses zero the gauge
  now renders dormant — dim, empty bar, no stale percent, labelled **`waiting for reset`** — and the
  row stops counting toward the alert banner; when fresh evidence arrives (a collect, or an overlay
  success after you log in / open Claude on that account) the new countdown replaces it automatically.
- **Idle accounts now re-evaluate their limits every tick (spec 012).** The spec 011 stale-overlay
  demotion only ran when *new* data landed (an active-block snapshot or an overlay success), so
  exactly the accounts that needed it — idle, or with a stale token (whose overlay is never polled,
  by design) — kept their frozen authoritative crit rows for days. The collector now runs the shared
  merge point on idle (`Ok(None)`), failed, and windowless-snapshot outcomes too (with an empty
  incoming set), so a stale authoritative set demotes within one local tick of the TTL for every
  account; an early-out skips the store write when nothing changed.
- **Past-reset countdown reads correctly.** Once a limit's known reset time passes, the board showed
  `resets resetting…` — present tense and a doubled verb. It now reads `waiting for reset` (the
  `resets` verb is dropped for the past case), signalling the limit has reset and we're awaiting
  fresh evidence of the new window; the live countdown then resumes on its own when the fresh
  `resets_at` lands.

- **Refresh & freshness hardening (spec 011)** — a verified adversarial review of the collector → store
  → TUI refresh path found six ways the board could show stale data as if current, or slow down over
  time. All fixed:
  - **Collector liveness is now visible.** The `heartbeat` table was written every tick but read by
    nobody, so a dead / stalled / never-started collector left the TUI redrawing frozen rows forever,
    looking perfectly healthy. New `Store::heartbeat_age` reader; the dashboard shows a loud red
    `⚠ collector not running / stalled` banner when the writer's heartbeat ages past `3×poll_local_secs`.
  - **The local ccusage plane now has its own freshness signal.** The only "refreshed" label was fed
    solely by the opt-in overlay, so an overlay-off account (the default) showed *no* age at all, and a
    fresh overlay made the whole line read "refreshed just now" while the local numbers were frozen. The
    fleet line now shows a distinct local-plane age (`usage 12s ago`, styled a warning past
    `2×poll_local_secs`) and scopes the overlay age as `limits …` so the two planes never conflate.
  - **Stale authoritative limits now degrade to derived.** On a stale token or persistent 429 the
    collector kept the old `authoritative` %/reset winning the merge forever (frozen, and hiding a real
    crit behind a calm number). `apply_limits` now demotes an authoritative set once the last overlay
    success ages past `2×poll_overlay_secs`, so the fresh derived session takes over — honouring the
    "degrade silently to derived on any 429/failure" invariant.
  - **The overlay pass no longer stalls the local plane, and refreshes every account per pass.** It was
    awaited inline in the `select!` loop (freezing local collection up to 20s each overlay cycle) and
    fetched accounts sequentially under that budget (~2 of N refreshed on a slow network). Overlay
    network fetches now run **off** the loop task (spawned into a `JoinSet`, each hard-capped, harvested
    back on the loop — single-writer preserved); the round-robin start offset is retained.
  - **Refresh no longer slows as the store grows.** The header aggregate ran a full-table `GROUP BY`
    over the ever-growing `snapshots` table every second; it is now bounded to the last N ticks by a
    covering index (`idx_snapshots_time`), and the collector prunes snapshots older than 3 days on an
    hourly sweep and checkpoints the WAL (`PRAGMA wal_checkpoint(TRUNCATE)`), bounding disk too.
  - **Suspend/resume hardening.** All collector intervals and the TUI tick set
    `MissedTickBehavior::Skip` (resume cadence from now, not a catch-up burst); the ccusage child sets
    `kill_on_drop(true)` (a timed-out poll is reaped, not leaked); `PRAGMA synchronous=NORMAL` on the
    WAL store.

### CHANGED
- Default `poll_local_secs` lowered `20 → 10` so the always-on local plane reads as live (spec 011).
- Dashboard activity display reworked (spec 010). The four per-account token-burn sparklines (a
  cumulative-total sawtooth that also read identically across accounts) are replaced by **one
  fleet-wide aggregate burn-rate bar** in the header (`burn · all accts` + a `Sparkline` of
  `Σ burn_tpm` per collection tick). New store reader `aggregate_burn_history`; `App` carries the
  series; rows, aggregate, and the fleet line are read in one store pass (`Dashboard`).
- **Token / cost / burn / provenance / refresh are now one fleet-wide header line, not repeated per
  panel.** Every account reads the same physical logs (shared `projects/` symlink — spec 010), so the
  per-account meta line (`280.54M · $335.95 (notional) · 271.20M/h · derived`) was the same number on
  every panel; it now renders **once** under the title/banner (`build_fleet_view` reduces the shared
  usage, plus the *worst* provenance and *oldest* refresh so a single degraded account still shows).
  FULL panels drop their meta row (one line shorter), and the COMPACT/MICRO tiers drop their per-row
  cost chip — each account row now carries only what differs (its gauges, headline, severity). The
  fleet line degrades by width (drops refresh → provenance → burn, and `(notional)`→`$Nn`, `derived`
  →`drv`) and never overflows.
- `tok doctor` now reports **`projects/` isolation**: when two or more accounts' `<config_dir>/projects`
  canonicalize to the same real path (e.g. all symlinked to a shared `~/.claude/projects`), it names
  the shared group and explains that per-account usage attribution is disabled until each account has
  its own real `projects/` — the precise root cause of identical per-account token totals.

### FEATURES
- Project scaffold: strict Rust (2021, `forbid(unsafe_code)`, clippy pedantic `-D warnings`) bin
  crate `tok`, vendored `rules/`, spec-driven TDD harness, and the `tok --help` / `--version` CLI.
- `tok validate` / `tok accounts`: parse + validate `tokenomics.toml` — accounts each pinned to their
  own `CLAUDE_CONFIG_DIR`, threshold/cadence `[settings]`, `deny_unknown_fields`, and `~` expansion.
- Claude collector core: pure `parse_ccusage_blocks` → `reduce_snapshot` → `derive_session_limit`
  (normalized `UsageSnapshot` + a `Derived`, time-in-window session `Limit`), behind an injectable
  `Runner`/`Exec` subprocess seam (explicit argv, per-call timeout) and a `ProviderAdapter` trait so
  Codex/Gemini/Grok are additive. `ClaudeAdapter` runs `ccusage blocks --json --active` under each
  account's `CLAUDE_CONFIG_DIR`.
- `tok once` (and `once --json`): collect one snapshot per account and print it — tokens, notional
  cost (labeled a usage proxy, never a bill), and session % + verbatim reset time.
- `[settings].ccusage_cmd`: optional launcher prefix (e.g. `["npx", "ccusage"]`) for machines with
  no global `ccusage` on `PATH`.
- Pure formatting toolkit (`format.rs`): `severity_for` (threshold classifier), `format_pct`,
  `format_tokens` (`"1.23M"`/`"12.3K"`), `format_cost` (always labeled notional), and `format_reset`
  (`"in 2h 41m"` countdown; verbatim when unparseable). Wired into `tok once` output.
- Local SQLite store (`store.rs`, bundled + WAL): `user_version` migrations, `upsert_accounts`,
  `insert_snapshot`/`insert_limits`/`heartbeat` writers, and `latest_snapshot`/`latest_limits`/
  `burn_history` readers — timestamps as epoch-millis, `resets_at` stored verbatim.
- `tok collector` (single pass via `--once`): collect every account, persist to the store, and print
  a read-back summary; history accumulates across runs.
- `tok collector` (daemon): 24/7 cadence loop with inflight + generation guards, per-account
  isolation, bounded concurrency, per-tick heartbeat, and clean SIGINT/SIGTERM shutdown. Example
  `systemd --user` unit in `docs/running-the-collector.md`.
- `tok` dashboard (ratatui): one panel per account with a 5h utilization gauge (colored by
  severity), a weekly `n/a (enable overlay)` slot, a provenance badge, a token-burn sparkline, a
  reset countdown, and a notional-cost label; an alert banner when any account is at/over the warn
  threshold. Keys: `↑`/`↓`/`j`/`k` select, `r` refresh, `?` help, `q`/`Esc` quit. Reads the store the
  collector writes; honors `NO_COLOR`; panic-safe terminal restore. Pure `view`/`update`/`keys`
  seams, table- and snapshot-tested.
- Opt-in authoritative overlay (`limits_overlay = true` per account): the collector polls
  `/api/oauth/usage` (rustls) for real 5h + weekly utilization % and verbatim reset times, tagged
  `authoritative`; degrades silently to derived on any 429/error (capped backoff, no `Retry-After`
  needed). Passive token reuse from `.credentials.json` (owner-only mode enforced; token never
  logged, errored, or stored); expired ⇒ `stale — open Claude to refresh` in the TUI. Limits are
  merged by provenance so a derived tick never clobbers an authoritative row. Overlay defaults off.
- Edge-triggered alerts: the collector fires once on an upward severity crossing (per account +
  window, with a cooldown), never re-firing while unchanged; best-effort desktop notification
  (non-fatal if no notification daemon). The in-TUI banner remains the source of truth.
- `tok doctor`: read-only diagnostics per account — config_dir exists, `.credentials.json` present +
  owner-only (`0600`), ccusage version, active-block summary, `CLAUDE_CONFIG_DIR` round-trip
  distinctness, and overlay reachability (opted-in accounts only). No secret is ever printed.
- Docs: `docs/token-refresh-hook.md` (optional SessionStart hook / periodic warm-up to keep overlay
  tokens fresh, since Tokenomics never refreshes tokens itself).

### CHANGED
- **Responsive dashboard — the layout now adapts to the window instead of breaking in small ones.**
  The board picks a **density tier** from the terminal size: **FULL** bordered panels when roomy,
  **COMPACT** borderless spine-grouped 3-line blocks when shorter/narrower, and **MICRO** one aligned
  line per account when tiny — so a small window degrades gracefully rather than squeezing panels into
  empty boxes. All three tiers share one row grammar (marker · severity glyph · proportional bar ·
  percent · verbatim reset). Gauges are now **visible eighth-block bars** (`█ ▏▎▍▌▋▊▉` over a `░`
  track) instead of the low-contrast line gauge, so fill reads even without colour; severity carries
  a glyph **and** a word (`● ok` / `▲ warn` / `✖ crit`); the selected account is marked structurally
  (double-line border in FULL, thick accent spine `▊` in COMPACT/MICRO) so nothing depends on colour
  — `NO_COLOR` renders a byte-identical grid. More accounts than fit scroll a window that keeps the
  selection on-screen with `▴/▾ N more` chips, and the alert banner names the worst offender over
  **all** accounts (never just the visible window). Title, banner, and footer text degrade by width.
  Snapshot-tested at 120×40 / 80×16 / 58×9 / 42×14 plus a 6-account scroll case.
- **Per-account "last refreshed" time.** With multiple Max accounts, only the one you're logged into
  has a warm token, so the overlay can refresh only that account — the others' authoritative numbers
  are frozen at their last successful fetch. The store now records the last successful overlay fetch
  per account (`overlay_state` table, schema **v2**, migrated in place), and the dashboard shows
  `refreshed Nm ago` on each panel so you can see how current each account's data is. Written only on
  a real authoritative fetch — never by a local derived tick.
- **Both reset times per account.** Every gauge now carries its own reset countdown — the 5h reset on
  the session gauge and the weekly reset on both the weekly-all and the per-model (e.g. Fable) weekly
  gauges (like Claude's `/usage`) — instead of a single reset on the meta line, so each line reads
  consistently `<pct> <sev> · resets <when>`. The panel-height guard was corrected so a scoped panel
  under vertical squeeze falls back to border-only rather than dropping a line.
- **Overlay now reads the endpoint's canonical `limits[]` array** and renders weekly graphically.
  `/api/oauth/usage` moved from the flat `seven_day_opus`/`seven_day_sonnet` fields (now `null`) to a
  `limits[]` array whose `weekly_scoped` entries are keyed by model **display name** (e.g. `Fable`).
  `parse_oauth_usage` prefers that array (falling back to the flat windows), maps scoped weeklies by
  model name, and skips unknown `kind`s. The dashboard now draws three stacked gauges per overlay-on
  account — **5h session · weekly (all models) · weekly (top per-model, e.g. Fable)** — matching
  Claude's `/usage`; a row's severity is the worst of all its limits, so a critical scoped weekly
  lights the alert banner even when the 5h window is calm. `Severity` gained `Ord` for this.
- Config and store paths are now **cwd-independent** (new `src/paths.rs`): `tok` resolves the same
  `tokenomics.toml` and `tokenomics.db` no matter which directory it launches from. Resolution is
  `$TOKENOMICS_CONFIG` / `$TOKENOMICS_DB` if set, else the XDG paths
  (`~/.config/tokenomics/tokenomics.toml`, `~/.local/share/tokenomics/tokenomics.db`). The old
  implicit repo-local `./tokenomics.toml` / `./tokenomics.db` pickup (a footgun for an installed
  TUI — behavior changed based on the shell's cwd) is removed; the env vars are the sole dev/test
  override. `tok --help` documents both paths.

### HARDENING (from code review)
- `tok collector` / `tok` / `tok once` / `tok doctor` now run `config::validate` at startup and
  refuse on errors (e.g. a duplicate account `id` — the store key and sole attribution handle — no
  longer silently merges two accounts).
- Desktop notifications are offloaded to the blocking pool (fire-and-forget), so a slow/unreachable
  notification daemon can never stall the collector loop or its shutdown.
- Each overlay pass is time-boxed, so a slow/hung opted-in account can't block the local plane or a
  pending shutdown (unreached accounts retry next tick).
- The TUI isolates per-account store-read failures (keeps the last-good row / shows a read-error
  status) instead of the whole dashboard exiting on one bad read.
