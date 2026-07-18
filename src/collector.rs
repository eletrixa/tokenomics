//! The background collector loop: drive every account on a cadence, guarded, into the store.
//!
//! Project: Tokenomics — monitor LLM subscription accounts (usage, limits, time-left) in a TUI
//! Module:  src/collector.rs
//! Deps:    tokio (interval, JoinSet, signal), jiff; providers/store/format/alerts (domain logic)
//! Tested:  inline `#[cfg(test)]` — `should_apply` table, a fake-adapter loop, degrade-to-derived,
//!          concurrent-overlay + loop-not-blocked timing, an opt-in overlay run, inactive-account
//!          skip on both cadences (spec 014), a Codex app-server overlay pass + failure backoff (013),
//!          config hot-reload activation/deactivation + bad-reload resilience (spec 015), a z.ai
//!          quota overlay pass + opt-in/env-var eligibility gating (spec 019 §D)
//!
//! Key responsibilities:
//! - `run_collector`: local-cadence tick → `collect` → `insert_snapshot` + derived limit; overlay
//!   cadence tick → spawn opt-in Claude `/api/oauth/usage`, Codex app-server, and z.ai quota fetches,
//!   harvest → authoritative limits (degrade to derived on stale/failure); a slow retention sweep
//!   prunes old snapshots + checkpoints the WAL.
//! - Guards: inflight (never stack a second collect per account), generation (`should_apply`),
//!   isolation (a failing account keeps last-good and never crashes the loop), bounded concurrency,
//!   `MissedTickBehavior::Skip` (resume cadence from now after a suspend, not a catch-up burst).
//! - `apply_limits`: demote a stale authoritative set → merge by provenance → persist → fire
//!   edge-triggered alerts on upward crossings.
//! - `shutdown_signal`: resolve on SIGINT/SIGTERM for a clean daemon stop.
//!
//! Design constraints:
//! - The loop task solely owns the `Store` (single writer) — no locks; subprocess collects AND
//!   overlay network fetches run as spawned tasks that return their result for the loop to persist.
//! - Overlay is opt-in and backed-off on 429; its network I/O runs OFF the loop (each fetch hard-
//!   capped) so a pass never stalls local collection, and every eligible account refreshes per pass.
//! - Authoritative limits degrade to derived once the last overlay success ages past a TTL, so a
//!   stale-token / persistently-throttled account never shows a frozen "authoritative" number.
//! - The working `Config` is hot-reloaded via an injectable [`ConfigSource`] polled on the local tick:
//!   the whole config swaps on a validated CONTENT change (accounts, thresholds, cadences — content
//!   hashed each tick so an mtime-preserving edit still triggers), a bad reload keeps last-good, and a
//!   result that lands for an account a reload dropped is discarded (harvest guard). The loaded config
//!   path/mtime + the running binary's path/mtime are stamped into the heartbeat for doctor's
//!   divergence + rebuild checks (spec 015 §A/§B).

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use jiff::Timestamp;
use tokio::task::JoinSet;

use crate::alerts::{notify_desktop, AlertTracker};
use crate::config::Config;
use crate::domain::{Account, Limit, LimitKind, Provider, Severity, UsageSnapshot};
use crate::error::{AppError, AppResult};
use crate::format::{demote_stale_authoritative, merge_limits, severity_label};
use crate::providers::claude::ccusage::derive_session_limit;
use crate::providers::claude::creds;
use crate::providers::claude::overlay::{
    next_backoff, parse_oauth_usage, BackoffOutcome, UsageEndpoint,
};
use crate::providers::codex::rate_limits::{parse_rate_limits_response, RateLimitsSource};
use crate::providers::zai::quota::{parse_quota_response, QuotaEndpoint};
use crate::providers::zai::resolve_api_key;
use crate::providers::ProviderAdapter;
use crate::store::{Store, TokenStatus};

/// Max concurrent in-flight collects across all accounts (bounded concurrency).
const MAX_INFLIGHT: usize = 8;
/// Cap for the overlay 429 backoff.
const OVERLAY_BACKOFF_CAP_SECS: u64 = 900;
/// Per-account, per-window alert cooldown (so re-crossings don't spam notifications).
const ALERT_COOLDOWN_SECS: u64 = 300;
/// Hard budget for one overlay pass, so a slow/hung account can't stall the loop or shutdown.
/// (Accounts not reached within the budget are simply retried on the next overlay tick.)
const OVERLAY_TICK_BUDGET_SECS: u64 = 20;
/// Authoritative limits are considered stale — and demoted to `Estimate` rank, keeping their last
/// values on the board — once the last overlay success is older than `TTL_FACTOR × poll_overlay_secs`.
/// Without this a frozen authoritative row keeps winning the merge on a stale token / persistent 429,
/// hiding real usage (spec 011 §C).
const OVERLAY_TTL_FACTOR: u64 = 2;
/// How often the collector prunes old snapshots + checkpoints the WAL (a slow sweep — spec 011 §E).
const RETENTION_SWEEP_SECS: u64 = 3600;
/// Snapshots older than this many days are pruned (comfortably longer than the sparkline needs), so
/// the table, the aggregate scan, and the `.db`/`-wal` files stay bounded on a 24/7 run.
const RETENTION_DAYS: i64 = 3;

/// Pure generation guard: apply a result only when it is strictly newer than the last applied for
/// that account. Combined with the inflight guard, this drops any stale result that outlives a
/// newer one. Table-tested.
pub fn should_apply(result_gen: u64, latest_applied_gen: u64) -> bool {
    result_gen > latest_applied_gen
}

/// The result of one spawned collect, handed back to the loop for persistence.
#[derive(Debug)]
struct CollectOutcome {
    account: Account,
    generation: u64,
    now: Timestamp,
    result: AppResult<Option<UsageSnapshot>>,
}

/// A source of hot-reloaded configs, polled on each local tick (spec 015 §A). Injectable so the loop
/// is tested with an in-memory source (no file, no env mutation) exactly as the adapter / endpoint /
/// rate-source seams are. `poll` returns a freshly-validated `Config` ONLY when the underlying source
/// changed since the last successful load; `None` means "no change" — which also covers a change that
/// failed to parse/validate (the source keeps last-good and warns once, so the loop never crashes).
pub trait ConfigSource {
    /// The mtime (epoch-ms) of the currently-loaded config, for the heartbeat divergence stamp, or
    /// `None` when unknown (an in-memory source, or an unstattable file).
    fn mtime_ms(&self) -> Option<i64>;
    /// The resolved absolute path of the config being watched, recorded in the heartbeat so doctor
    /// stats the file the collector actually loaded (spec 015 §B). `None` for an in-memory source.
    fn config_path(&self) -> Option<String>;
    /// Poll for a changed, validated config; `None` = unchanged, or kept-last-good after a bad reload.
    fn poll(&mut self) -> Option<Config>;
}

/// Production [`ConfigSource`]: re-reads + validates the config file when its CONTENT changes. It
/// reads + hashes the file every local tick and compares the hash to the last config it acted on —
/// the file is a few KB, so a per-tick read+hash at a 10–20s cadence is negligible, and content
/// (not stat) is the only trigger that catches an mtime-preserving, same-size edit (`cp -p`,
/// `rsync --times`) that a stat-only watch and the doctor mtime check would go blind on *together*
/// (spec 015 §A). The parse + validation only run on an actual content change, and a persistently-bad
/// file warns exactly once (its hash is recorded on failure too, so it reads as "unchanged" after).
pub struct FileConfigSource {
    path: std::path::PathBuf,
    /// A `DefaultHasher` digest of the bytes we last acted on (success OR failure). Within-process
    /// comparison only — never persisted, so the hash algorithm is free to change between runs.
    last_hash: Option<u64>,
    /// The mtime we last acted on, surfaced as the heartbeat stamp (`mtime_ms`). Content drives the
    /// reload; this only records "when", so doctor's file-vs-recorded comparison has a value to use.
    last_modified: Option<SystemTime>,
    #[cfg(test)]
    warns: u32,
}

impl FileConfigSource {
    /// Seed from the current config file so the first poll doesn't spuriously reload the startup
    /// config the caller already loaded.
    #[must_use]
    pub fn new() -> Self {
        let path = crate::paths::config_path().unwrap_or_default();
        Self::seed(path)
    }

    fn seed(path: std::path::PathBuf) -> Self {
        let last_hash = std::fs::read(&path).ok().as_deref().map(hash_bytes);
        let last_modified = file_modified(&path);
        Self {
            path,
            last_hash,
            last_modified,
            #[cfg(test)]
            warns: 0,
        }
    }
}

impl Default for FileConfigSource {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfigSource for FileConfigSource {
    fn mtime_ms(&self) -> Option<i64> {
        self.last_modified.and_then(system_time_ms)
    }

    fn config_path(&self) -> Option<String> {
        Some(self.path.display().to_string())
    }

    fn poll(&mut self) -> Option<Config> {
        let bytes = std::fs::read(&self.path).ok();
        let hash = bytes.as_deref().map(hash_bytes);
        if hash == self.last_hash {
            return None; // content unchanged (or both unreadable) — the common case, no parse
        }
        self.last_hash = hash; // record even on failure, so a bad file warns only once per content
        self.last_modified = file_modified(&self.path);
        let result = bytes
            .ok_or_else(|| "config file unreadable".to_string())
            .and_then(|b| reload_config_bytes(&b));
        match result {
            Ok(cfg) => Some(cfg),
            Err(msg) => {
                eprintln!("collector: config reload failed, keeping last-good ({msg})");
                #[cfg(test)]
                {
                    self.warns += 1;
                }
                None
            }
        }
    }
}

/// Hash config bytes with `std`'s `DefaultHasher` — the content change trigger. In-process only.
fn hash_bytes(bytes: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// Parse + validate already-read config bytes, applying the SAME validation rule set as the startup
/// gate (`config::validate`), so a hot-reload can never accept a config the startup gate would reject.
fn reload_config_bytes(bytes: &[u8]) -> Result<Config, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
    let cfg = Config::parse(text).map_err(|e| e.to_string())?;
    let findings = crate::config::validate(&cfg);
    if findings.is_empty() {
        Ok(cfg)
    } else {
        Err(findings
            .iter()
            .map(|f| f.message.as_str())
            .collect::<Vec<_>>()
            .join("; "))
    }
}

fn file_modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

fn system_time_ms(t: SystemTime) -> Option<i64> {
    let ms = t.duration_since(SystemTime::UNIX_EPOCH).ok()?.as_millis();
    i64::try_from(ms).ok()
}

/// Build a cadence interval that resumes "from now" after a suspend/stall rather than firing a burst
/// of catch-up ticks (spec 011 §F). Recreated on a reload that changes the cadence (spec 015 §A).
fn cadence_interval(secs: u64) -> tokio::time::Interval {
    let mut interval = tokio::time::interval(Duration::from_secs(secs.max(1)));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval
}

/// Apply a validated config change on the local tick (spec 015 §A): swap the whole config, re-record
/// account identity + the loaded mtime, and recreate any interval whose cadence actually changed. A
/// `None` poll — no change, or a bad reload kept last-good — is a no-op, so the loop never restarts.
/// The pre-swap `cfg` already holds the previous cadence, so no separate tracking state is needed.
fn apply_config_reload<S: ConfigSource>(
    cfg: &mut Config,
    source: &mut S,
    store: &Store,
    exe_path: Option<&str>,
    exe_mtime: Option<i64>,
    local: &mut tokio::time::Interval,
    overlay: &mut tokio::time::Interval,
) {
    let Some(new_cfg) = source.poll() else {
        return;
    };
    let local_secs_changed = new_cfg.settings.poll_local_secs != cfg.settings.poll_local_secs;
    let overlay_secs_changed = new_cfg.settings.poll_overlay_secs != cfg.settings.poll_overlay_secs;
    *cfg = new_cfg;
    if let Err(e) = store.upsert_accounts(&cfg.accounts) {
        eprintln!("collector: post-reload upsert_accounts failed: {e}");
    }
    // Re-stamp the loaded config path/mtime; the exe stamp is the startup value (constant across the
    // run), so a binary rebuilt mid-run still reads as newer-than-recorded for doctor (spec 015 §B2).
    if let Err(e) = store.record_collector_stamp(
        "collector",
        std::process::id(),
        source.config_path().as_deref(),
        source.mtime_ms(),
        exe_path,
        exe_mtime,
    ) {
        eprintln!("collector: collector stamp failed: {e}");
    }
    if local_secs_changed {
        *local = cadence_interval(cfg.settings.poll_local_secs);
    }
    if overlay_secs_changed {
        *overlay = cadence_interval(cfg.settings.poll_overlay_secs);
    }
}

/// Run the collector loop until `shutdown` resolves. Generic over the adapter, the Claude overlay
/// endpoint, the Codex rate-limits source, the z.ai quota endpoint, and the config source so the
/// loop is tested with fakes (no process spawn, no network, no filesystem).
#[allow(clippy::too_many_arguments)] // one collaborator per provider's overlay lane, threaded explicitly
pub async fn run_collector<A, E, C, Z, S>(
    cfg: &Config,
    mut config_source: S,
    adapter: A,
    endpoint: E,
    rate_source: C,
    zai_endpoint: Z,
    store: Store,
    shutdown: impl Future<Output = ()>,
) -> AppResult<()>
where
    A: ProviderAdapter + Send + Sync + 'static,
    E: UsageEndpoint + 'static,
    C: RateLimitsSource + 'static,
    Z: QuotaEndpoint + 'static,
    S: ConfigSource,
{
    let mut cfg = cfg.clone();
    store.upsert_accounts(&cfg.accounts)?;
    // Capture the running binary's path + mtime ONCE at start: doctor compares the recorded mtime to
    // the path's current mtime to flag a rebuild after start (spec 015 §B2). Constant across the run.
    let (exe_path, exe_mtime) = crate::paths::current_exe_stamp();
    // Stamp the config + binary we started with, so doctor can flag a later config edit the collector
    // never reloaded, and a binary rebuilt after start.
    if let Err(e) = store.record_collector_stamp(
        "collector",
        std::process::id(),
        config_source.config_path().as_deref(),
        config_source.mtime_ms(),
        exe_path.as_deref(),
        exe_mtime,
    ) {
        eprintln!("collector: collector stamp failed: {e}");
    }
    let adapter = Arc::new(adapter);
    let endpoint = Arc::new(endpoint);
    let rate_source = Arc::new(rate_source);
    let zai_endpoint = Arc::new(zai_endpoint);
    let mut local = cadence_interval(cfg.settings.poll_local_secs);
    let mut overlay = cadence_interval(cfg.settings.poll_overlay_secs);
    let mut retention = tokio::time::interval(Duration::from_secs(RETENTION_SWEEP_SECS));
    retention.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut inflight: HashSet<String> = HashSet::new();
    let mut latest_gen: HashMap<String, u64> = HashMap::new();
    let mut generation: u64 = 0;
    let mut overlay_state = OverlayState::default();
    let mut alerts = AlertTracker::new(Duration::from_secs(ALERT_COOLDOWN_SECS));
    let mut tasks: JoinSet<CollectOutcome> = JoinSet::new();
    // Overlay network fetches run OFF the loop's critical path (spawned here, harvested below) so an
    // overlay pass never stalls local collection, and all opted-in accounts refresh each pass (§D).
    let mut overlay_tasks: JoinSet<OverlayOutcome> = JoinSet::new();
    // The Codex limits overlay (`codex app-server` subprocess) runs on the same cadence, off the loop
    // task, harvested on its own arm — the Codex twin of `overlay_tasks` (spec 013 §C).
    let mut codex_overlay_tasks: JoinSet<CodexOverlayOutcome> = JoinSet::new();
    // The z.ai quota overlay runs on the same cadence, off the loop task, harvested on its own arm —
    // the zai twin of `codex_overlay_tasks` (spec 019 §D).
    let mut zai_overlay_tasks: JoinSet<ZaiOverlayOutcome> = JoinSet::new();

    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = local.tick() => {
                // Hot-reload BEFORE this tick's work, so a (re)activated account joins immediately and
                // a deactivated one is skipped this very pass (spec 015 §A). The whole config swaps.
                apply_config_reload(
                    &mut cfg, &mut config_source, &store,
                    exe_path.as_deref(), exe_mtime, &mut local, &mut overlay,
                );
                if let Err(e) = store.heartbeat("collector", std::process::id()) {
                    eprintln!("collector: heartbeat failed: {e}");
                }
                let now = Timestamp::now();
                spawn_local_collects(&cfg, &adapter, now, &mut inflight, &mut generation, &mut tasks);
                // Off the slow overlay cadence: pick up a just-re-logged-in account's warm token now
                // (≤ poll_local_secs) instead of after the next ≤5-min overlay pass.
                recover_warm_tokens(&cfg, &endpoint, &store, &mut overlay_state, &mut overlay_tasks);
            }
            Some(joined) = tasks.join_next() => {
                match joined {
                    // Thresholds + TTL read from the CURRENT config, so a reload takes effect here;
                    // a result whose account a reload dropped is discarded (harvest guard, §A).
                    Ok(outcome) => apply_outcome(
                        &cfg, &store, outcome, &mut inflight, &mut latest_gen, &mut alerts,
                    ),
                    Err(join_err) => eprintln!("collector: task join error: {join_err}"),
                }
            }
            _ = overlay.tick() => {
                // Only the network/subprocess fetches are spawned here (they touch no Store); token
                // reads + token-state writes + backoff bookkeeping stay on the loop task. Each fetch
                // is individually time-boxed, so a slow account can't stall the loop or shutdown.
                spawn_overlay_fetches(&cfg, &endpoint, &store, &mut overlay_state, &mut overlay_tasks);
                spawn_codex_overlay_fetches(&cfg, &rate_source, &mut overlay_state, &mut codex_overlay_tasks);
                spawn_zai_overlay_fetches(&cfg, &zai_endpoint, &mut overlay_state, &mut zai_overlay_tasks);
            }
            Some(joined) = overlay_tasks.join_next() => {
                match joined {
                    // Parse + merge + persist + backoff on the loop task — single writer preserved.
                    Ok(outcome) => {
                        apply_overlay_outcome(&cfg, &store, &mut overlay_state, &mut alerts, outcome);
                    }
                    Err(join_err) => eprintln!("collector: overlay task join error: {join_err}"),
                }
            }
            Some(joined) = codex_overlay_tasks.join_next() => {
                match joined {
                    Ok(outcome) => {
                        apply_codex_overlay_outcome(&cfg, &store, &mut overlay_state, &mut alerts, outcome);
                    }
                    Err(join_err) => {
                        eprintln!("collector: codex overlay task join error: {join_err}");
                    }
                }
            }
            Some(joined) = zai_overlay_tasks.join_next() => {
                match joined {
                    Ok(outcome) => {
                        apply_zai_overlay_outcome(&cfg, &store, &mut overlay_state, &mut alerts, outcome);
                    }
                    Err(join_err) => {
                        eprintln!("collector: zai overlay task join error: {join_err}");
                    }
                }
            }
            _ = retention.tick() => sweep_retention(&store),
            () = &mut shutdown => break,
        }
    }
    Ok(())
}

/// Spawn one local collect per active, not-already-inflight account (bounded by `MAX_INFLIGHT`),
/// tagging each with a fresh generation for the harvest-side staleness guard (`should_apply`).
fn spawn_local_collects<A: ProviderAdapter + Send + Sync + 'static>(
    cfg: &Config,
    adapter: &Arc<A>,
    now: Timestamp,
    inflight: &mut HashSet<String>,
    generation: &mut u64,
    tasks: &mut JoinSet<CollectOutcome>,
) {
    for account in cfg.accounts.iter().filter(|a| a.active) {
        if inflight.len() >= MAX_INFLIGHT {
            break;
        }
        if inflight.contains(&account.id) {
            continue; // inflight guard: skip, don't queue
        }
        *generation += 1;
        let generation = *generation;
        inflight.insert(account.id.clone());
        let adapter = Arc::clone(adapter);
        let account = account.clone();
        tasks.spawn(async move {
            let result = adapter.collect(&account, now).await;
            CollectOutcome {
                account,
                generation,
                now,
                result,
            }
        });
    }
}

/// Prune snapshots older than the retention window and checkpoint the WAL. Best-effort: a failure is
/// logged, never fatal (the loop keeps collecting; the next sweep retries).
fn sweep_retention(store: &Store) {
    let cutoff = Timestamp::now().as_millisecond() - RETENTION_DAYS * 24 * 60 * 60 * 1000;
    match store.prune_snapshots(cutoff) {
        Ok(removed) if removed > 0 => {
            if let Err(e) = store.checkpoint_truncate() {
                eprintln!("collector: WAL checkpoint failed: {e}");
            }
        }
        Ok(_) => {} // nothing old to prune
        Err(e) => eprintln!("collector: snapshot retention prune failed: {e}"),
    }
}

/// Per-account overlay backoff bookkeeping (429 cooldowns).
#[derive(Debug, Default)]
struct OverlayState {
    /// Current backoff interval per account (seconds).
    backoff_secs: HashMap<String, u64>,
    /// Remaining overlay ticks to skip per account (after a 429).
    cooldown_ticks: HashMap<String, u32>,
    /// Rotating start offset for the per-tick pass, so no account is permanently last (and thus the
    /// first starved) when a slow pass hits `OVERLAY_TICK_BUDGET_SECS`. Advances one slot per tick.
    rotate: usize,
}

impl OverlayState {
    /// One successful fetch: reset the account's backoff to `base` and lift any cooldown.
    /// The single representation of the success transition — Claude and Codex harvests share it.
    fn note_success(&mut self, account_id: &str, backoff_current: u64, base: u64) {
        let reset = next_backoff(
            backoff_current,
            BackoffOutcome::Ok,
            base,
            OVERLAY_BACKOFF_CAP_SECS,
        );
        self.backoff_secs.insert(account_id.to_string(), reset);
        self.cooldown_ticks.remove(account_id);
    }

    /// One throttled/failed fetch: grow the account's backoff and convert it to whole overlay
    /// ticks of cooldown. The single representation of the failure transition (shared as above).
    fn note_throttled(&mut self, account_id: &str, backoff_current: u64, base: u64) {
        let next = next_backoff(
            backoff_current,
            BackoffOutcome::Throttled,
            base,
            OVERLAY_BACKOFF_CAP_SECS,
        );
        self.backoff_secs.insert(account_id.to_string(), next);
        let ticks = next.div_ceil(base).max(1);
        self.cooldown_ticks.insert(
            account_id.to_string(),
            u32::try_from(ticks).unwrap_or(u32::MAX),
        );
    }
}

/// One overlay fetch handed back to the loop for persistence. The fetch itself touches no `Store`
/// (single-writer invariant): the loop parses + merges + persists on harvest.
#[derive(Debug)]
struct OverlayOutcome {
    /// The account this fetch was for.
    account: Account,
    /// The backoff interval in effect when the fetch was spawned (drives [`next_backoff`] on harvest).
    backoff_current: u64,
    /// The raw body on success, or the typed error (429 → backoff, other → logged).
    result: AppResult<Vec<u8>>,
}

/// Spawn one network fetch per eligible opted-in account into `tasks` (run OFF the loop task). Token
/// reads, warm/stale token-state writes, cooldown decrements, and round-robin bookkeeping all happen
/// here ON the loop (they touch the single-writer `Store`); only the network I/O is spawned. Eligible
/// = active, opted-in, not cooling down from a 429, with a warm token. Skips stale-token / opted-out
/// / inactive accounts — a cancelled subscription 429s every pass forever, so polling it is waste and
/// noise (spec 014 §B).
fn spawn_overlay_fetches<E: UsageEndpoint + 'static>(
    cfg: &Config,
    endpoint: &Arc<E>,
    store: &Store,
    state: &mut OverlayState,
    tasks: &mut JoinSet<OverlayOutcome>,
) {
    let base = cfg.settings.poll_overlay_secs.max(1);
    let now_ms = Timestamp::now().as_millisecond();

    // Round-robin the pass start each tick so no account is permanently first/last across passes.
    // Claude only: the `/api/oauth/usage` overlay is a Claude endpoint keyed on a Claude creds token;
    // a Codex account rides the separate `codex app-server` pass (spec 013 §C).
    let opted_in: Vec<&Account> = cfg
        .accounts
        .iter()
        .filter(|a| a.active && a.limits_overlay && a.provider == Provider::Claude)
        .collect();
    if opted_in.is_empty() {
        return;
    }
    let start = state.rotate % opted_in.len();
    state.rotate = state.rotate.wrapping_add(1);
    for offset in 0..opted_in.len() {
        let account = opted_in[(start + offset) % opted_in.len()];
        try_spawn_overlay_fetch(account, endpoint, store, state, tasks, now_ms, base, true);
    }
}

/// Fast-path token recovery, run on the frequent **local** tick. The moment a re-login / `open Claude`
/// rotates a stale-token account's credentials file to a warm token, fire its overlay fetch now instead
/// of waiting up to `poll_overlay_secs` (5 min) for the periodic pass — closing the "I opened it and it
/// still says stale" gap. An account the store last saw **warm** is already owned by the periodic pass
/// and is skipped, so this never adds off-cadence network fetches; a still-stale account costs only a
/// local file read here (`log_stale = false` keeps it a silent no-op — no per-tick log, no stale write).
// ponytail: re-reads each not-warm opted-in account's creds file every local tick (cheap: ≤6 files,
// gated by an indexed status read). If account count ever grows large, gate on creds-file mtime.
fn recover_warm_tokens<E: UsageEndpoint + 'static>(
    cfg: &Config,
    endpoint: &Arc<E>,
    store: &Store,
    state: &mut OverlayState,
    tasks: &mut JoinSet<OverlayOutcome>,
) {
    let base = cfg.settings.poll_overlay_secs.max(1);
    let now_ms = Timestamp::now().as_millisecond();
    // Claude only: token-warmth is a Claude concept (Codex never writes a token_state row and its
    // auth lives inside the binary), so the fast warm-token recovery is a no-op for Codex (§C).
    for account in cfg
        .accounts
        .iter()
        .filter(|a| a.active && a.limits_overlay && a.provider == Provider::Claude)
    {
        if matches!(
            store.latest_token_status(&account.id),
            Ok(Some(TokenStatus::Warm))
        ) {
            continue; // periodic pass owns warm accounts — don't double-fetch off cadence
        }
        try_spawn_overlay_fetch(account, endpoint, store, state, tasks, now_ms, base, false);
    }
}

/// Try to spawn ONE overlay network fetch for `account`. The token read + token-state write + cooldown
/// bookkeeping run ON the loop task (single-writer `Store`); only the network I/O is spawned. Eligible
/// = not cooling down from a 429, with a warm token. `log_stale` marks + logs a cold token and advances
/// the 429 cooldown — `true` on the periodic pass, `false` on the fast recovery recheck (which must stay
/// a silent no-op for a still-stale account). Returns whether a fetch was spawned.
#[allow(clippy::too_many_arguments)] // loop-owned collaborators threaded explicitly (no shared state)
fn try_spawn_overlay_fetch<E: UsageEndpoint + 'static>(
    account: &Account,
    endpoint: &Arc<E>,
    store: &Store,
    state: &mut OverlayState,
    tasks: &mut JoinSet<OverlayOutcome>,
    now_ms: i64,
    base: u64,
    log_stale: bool,
) -> bool {
    if let Some(ticks) = state.cooldown_ticks.get_mut(&account.id) {
        if *ticks > 0 {
            if log_stale {
                *ticks -= 1; // only the periodic pass advances the 429 cooldown
            }
            return false; // still cooling down from a 429
        }
    }
    // Validation (spec 019 §A) guarantees a Claude account always carries a config_dir; this is a
    // defensive early return, never a panic, if that guarantee is ever violated upstream.
    let Some(config_dir) = account.config_dir.as_deref() else {
        if log_stale {
            eprintln!(
                "collector: [{}] overlay skipped — claude account missing config_dir",
                account.id
            );
        }
        return false;
    };
    let token = match creds::read_token(config_dir) {
        Ok(token) => token,
        Err(e) => {
            if log_stale {
                mark_stale(store, &account.id, None);
                eprintln!("collector: [{}] overlay token unavailable: {e}", account.id);
            }
            return false;
        }
    };
    if !token.is_warm(now_ms) {
        if log_stale {
            mark_stale(store, &account.id, Some(token.expires_at_ms));
            eprintln!(
                "collector: [{}] token stale — open Claude to refresh",
                account.id
            );
        }
        return false;
    }
    let _ = store.set_token_state(
        &account.id,
        TokenStatus::Warm,
        Some(token.expires_at_ms),
        None,
    );

    let backoff_current = state.backoff_secs.get(&account.id).copied().unwrap_or(base);
    let endpoint = Arc::clone(endpoint);
    let account = account.clone();
    // The bearer is moved into the task (used for the request, never logged). The per-fetch hard cap
    // backstops the reqwest client timeout so a pathological hang still can't outlive a pass.
    let access = token.access_token().to_string();
    let cap = Duration::from_secs(OVERLAY_TICK_BUDGET_SECS);
    tasks.spawn(async move {
        let result = match tokio::time::timeout(cap, endpoint.fetch(&access)).await {
            Ok(result) => result,
            Err(_elapsed) => Err(AppError::Overlay(
                "overlay fetch exceeded budget".to_string(),
            )),
        };
        OverlayOutcome {
            account,
            backoff_current,
            result,
        }
    });
    true
}

/// Harvest one overlay fetch on the loop task: parse + merge + persist the body (degrading to derived
/// on error), and advance the 429 backoff / cooldown. Single writer — the only overlay `Store` writes.
fn apply_overlay_outcome(
    cfg: &Config,
    store: &Store,
    state: &mut OverlayState,
    alerts: &mut AlertTracker,
    outcome: OverlayOutcome,
) {
    // Harvest guard: an in-flight fetch that lands after a reload removed/deactivated this account is
    // dropped — never stamp a fresh success/failure or fire an alert onto a just-gone account (§A).
    if !present_and_active(cfg, &outcome.account.id) {
        return;
    }
    let base = cfg.settings.poll_overlay_secs.max(1);
    let OverlayOutcome {
        account,
        backoff_current,
        result,
    } = outcome;
    let now = Timestamp::now();
    match result {
        Ok(body) => {
            apply_overlay_body(cfg, store, &account, &body, now, alerts);
            state.note_success(&account.id, backoff_current, base);
        }
        Err(AppError::RateLimited) => {
            state.note_throttled(&account.id, backoff_current, base);
            // A dead subscription 429s every pass forever; a live account's 429 clears on the next
            // success. Pin the failing-since so the TUI can flag a *sustained* outage.
            let _ = store.mark_overlay_failing(&account.id, now.as_millisecond());
            eprintln!(
                "collector: [{}] overlay 429 — backing off ~{}s",
                account.id,
                state.backoff_secs.get(&account.id).copied().unwrap_or(base)
            );
        }
        Err(e) => {
            let _ = store.mark_overlay_failing(&account.id, now.as_millisecond());
            eprintln!("collector: [{}] overlay fetch failed: {e}", account.id);
        }
    }
}

/// Parse an overlay body → authoritative limits → merge + store + alert (degrade on error).
fn apply_overlay_body(
    cfg: &Config,
    store: &Store,
    account: &Account,
    body: &[u8],
    now: Timestamp,
    alerts: &mut AlertTracker,
) {
    match parse_oauth_usage(
        body,
        &account.id,
        account.provider,
        cfg.settings.warn_pct,
        cfg.settings.crit_pct,
    ) {
        Ok(authoritative) if !authoritative.is_empty() => {
            apply_authoritative_limits(
                cfg,
                store,
                &account.id,
                authoritative,
                now,
                alerts,
                "overlay",
            );
        }
        Ok(_) => {} // empty body — nothing authoritative to merge
        Err(e) => {
            // A persistently-malformed HTTP-200 body is a real overlay outage: pin failing-since so
            // the TUI can trip "overlay stalled — check account", mirroring the Codex twin's failure
            // handling (best-effort — a store hiccup here must not abort the loop).
            let _ = store.mark_overlay_failing(&account.id, now.as_millisecond());
            eprintln!("collector: [{}] overlay parse failed: {e}", account.id);
        }
    }
}

/// Merge + persist a parsed authoritative limit set, then stamp the overlay-success timestamp on a
/// clean write (best-effort — a store hiccup here must not abort the loop). The one place the Claude
/// and Codex overlay harvests share: only the log line's provider tag (`log_prefix`) differs.
fn apply_authoritative_limits(
    cfg: &Config,
    store: &Store,
    account_id: &str,
    authoritative: Vec<Limit>,
    now: Timestamp,
    alerts: &mut AlertTracker,
    log_prefix: &str,
) {
    let ttl = cfg
        .settings
        .poll_overlay_secs
        .saturating_mul(OVERLAY_TTL_FACTOR);
    match apply_limits(store, account_id, authoritative, now, alerts, ttl) {
        Ok(()) => {
            // Stamp the last authoritative refresh (drives the TUI's "refreshed Nm ago").
            let _ = store.record_overlay_success(account_id, now.as_millisecond());
        }
        Err(e) => eprintln!("collector: [{account_id}] {log_prefix} store write failed: {e}"),
    }
}

/// Record a stale token (best-effort; a store failure here must not abort the loop).
fn mark_stale(store: &Store, account_id: &str, expires_at_ms: Option<i64>) {
    let _ = store.set_token_state(account_id, TokenStatus::Stale, expires_at_ms, None);
}

/// One Codex `codex app-server` rate-limits fetch handed back to the loop for persistence — the
/// Codex twin of [`OverlayOutcome`]. The subprocess exchange touches no `Store` (single-writer
/// invariant): the loop parses + merges + persists on harvest.
#[derive(Debug)]
struct CodexOverlayOutcome {
    /// The account this fetch was for.
    account: Account,
    /// The backoff interval in effect when the fetch was spawned (drives [`next_backoff`] on harvest).
    backoff_current: u64,
    /// The raw `account/rateLimits/read` response line on success, or the typed error.
    result: AppResult<String>,
}

/// Spawn one `codex app-server` rate-limits fetch per eligible opted-in Codex account, run OFF the
/// loop task (single-writer preserved). Eligible = active, opted-in, provider Codex, not backing off
/// from a prior failure. Unlike the Claude overlay there is NO creds/token read here — Codex auth
/// stays inside the binary; a logged-out or missing-binary account simply fails the fetch and is
/// backed off on harvest (spec 013 §C). Each fetch is hard-capped so a hung subprocess can't stall
/// the loop (the real client already `kill_on_drop`s under its own timeout — this is the backstop).
fn spawn_codex_overlay_fetches<C: RateLimitsSource + 'static>(
    cfg: &Config,
    source: &Arc<C>,
    state: &mut OverlayState,
    tasks: &mut JoinSet<CodexOverlayOutcome>,
) {
    let base = cfg.settings.poll_overlay_secs.max(1);
    for account in cfg
        .accounts
        .iter()
        .filter(|a| a.active && a.limits_overlay && a.provider == Provider::Codex)
    {
        if let Some(ticks) = state.cooldown_ticks.get_mut(&account.id) {
            if *ticks > 0 {
                *ticks -= 1;
                continue; // still backing off from a prior failure
            }
        }
        // Validation (spec 019 §A) guarantees a Codex account always carries a config_dir; this is
        // a defensive skip, never a panic, if that guarantee is ever violated upstream.
        let Some(config_dir) = account.config_dir.clone() else {
            eprintln!(
                "collector: [{}] codex overlay skipped — missing config_dir",
                account.id
            );
            continue;
        };
        let backoff_current = state.backoff_secs.get(&account.id).copied().unwrap_or(base);
        let source = Arc::clone(source);
        let account = account.clone();
        let cap = Duration::from_secs(OVERLAY_TICK_BUDGET_SECS);
        tasks.spawn(async move {
            let result = match tokio::time::timeout(cap, source.fetch(&config_dir)).await {
                Ok(result) => result,
                Err(_elapsed) => Err(AppError::Overlay(
                    "codex rate-limits fetch exceeded budget".to_string(),
                )),
            };
            CodexOverlayOutcome {
                account,
                backoff_current,
                result,
            }
        });
    }
}

/// Harvest one Codex rate-limits fetch on the loop task: parse → authoritative limits → merge +
/// persist + record the refresh (reset backoff). ANY failure (fetch error, empty/malformed body)
/// marks the account failing and backs off, so a logged-out / broken `codex` isn't respawned every
/// pass. The TTL demotion of a now-stale authoritative set is provider-agnostic — it rides the local
/// re-evaluation tick exactly as for Claude. Single writer — the only Codex-overlay `Store` writes.
fn apply_codex_overlay_outcome(
    cfg: &Config,
    store: &Store,
    state: &mut OverlayState,
    alerts: &mut AlertTracker,
    outcome: CodexOverlayOutcome,
) {
    // Harvest guard: drop a fetch that landed after a reload removed/deactivated this account (§A).
    if !present_and_active(cfg, &outcome.account.id) {
        return;
    }
    let base = cfg.settings.poll_overlay_secs.max(1);
    let CodexOverlayOutcome {
        account,
        backoff_current,
        result,
    } = outcome;
    let now = Timestamp::now();
    let parsed = result.and_then(|line| {
        parse_rate_limits_response(
            &[&line],
            &account.id,
            cfg.settings.warn_pct,
            cfg.settings.crit_pct,
        )
    });
    match parsed {
        Ok(authoritative) if !authoritative.is_empty() => {
            apply_authoritative_limits(
                cfg,
                store,
                &account.id,
                authoritative,
                now,
                alerts,
                "codex overlay",
            );
            state.note_success(&account.id, backoff_current, base);
        }
        Ok(_) => {} // both windows absent — nothing authoritative to merge
        Err(e) => {
            state.note_throttled(&account.id, backoff_current, base);
            let _ = store.mark_overlay_failing(&account.id, now.as_millisecond());
            eprintln!(
                "collector: [{}] codex overlay fetch failed: {e}",
                account.id
            );
        }
    }
}

// ── spec 019 §D: z.ai overlay integration ──────────────────────────────────────────────────────
// Wired into `run_collector`'s `select!` loop as its 4th `JoinSet` arm, mirroring the Codex twin
// above: same cadence (`poll_overlay_secs`), same per-account backoff/cooldown/TTL-demotion
// machinery (`OverlayState`, `apply_authoritative_limits`), claude/codex paths untouched.

/// One z.ai quota fetch handed back for harvesting — the zai twin of [`CodexOverlayOutcome`].
#[derive(Debug)]
struct ZaiOverlayOutcome {
    /// The account this fetch was for.
    account: Account,
    /// The backoff interval in effect when the fetch was spawned (drives [`next_backoff`] on harvest).
    backoff_current: u64,
    /// The raw quota body on success, or the typed error (429 → backoff, other → logged).
    result: AppResult<Vec<u8>>,
}

/// Spawn one z.ai quota fetch per eligible opted-in zai account, run OFF the loop task (single-
/// writer preserved). Eligible = active, opted-in, provider zai, not cooling down from a prior
/// failure, AND its named env var resolves to a non-empty value — an account missing that (not
/// opted in, or the env var unset/empty) is skipped with a logged reason, mirroring the Claude
/// stale-token skip; no store write (zai carries no token-warmth state to persist).
fn spawn_zai_overlay_fetches<Z: QuotaEndpoint + 'static>(
    cfg: &Config,
    endpoint: &Arc<Z>,
    state: &mut OverlayState,
    tasks: &mut JoinSet<ZaiOverlayOutcome>,
) {
    let base = cfg.settings.poll_overlay_secs.max(1);
    for account in cfg
        .accounts
        .iter()
        .filter(|a| a.active && a.limits_overlay && a.provider == Provider::Zai)
    {
        if let Some(ticks) = state.cooldown_ticks.get_mut(&account.id) {
            if *ticks > 0 {
                *ticks -= 1;
                continue; // still backing off from a prior failure
            }
        }
        let key = match resolve_api_key(account) {
            Ok(key) => key,
            Err(reason) => {
                eprintln!("collector: [{}] zai overlay skipped — {reason}", account.id);
                continue;
            }
        };
        let backoff_current = state.backoff_secs.get(&account.id).copied().unwrap_or(base);
        let endpoint = Arc::clone(endpoint);
        let account = account.clone();
        let cap = Duration::from_secs(OVERLAY_TICK_BUDGET_SECS);
        // The key is moved into the task (used for the request, never logged).
        tasks.spawn(async move {
            let result = match tokio::time::timeout(cap, endpoint.fetch(&key)).await {
                Ok(result) => result,
                Err(_elapsed) => Err(AppError::Overlay(
                    "zai quota fetch exceeded budget".to_string(),
                )),
            };
            ZaiOverlayOutcome {
                account,
                backoff_current,
                result,
            }
        });
    }
}

/// Harvest one z.ai quota fetch on the loop task: parse → authoritative limits → merge + persist
/// (degrading to derived on error), and advance the 429 backoff / cooldown — the zai twin of
/// [`apply_codex_overlay_outcome`]. Single writer — the only zai-overlay `Store` writes.
fn apply_zai_overlay_outcome(
    cfg: &Config,
    store: &Store,
    state: &mut OverlayState,
    alerts: &mut AlertTracker,
    outcome: ZaiOverlayOutcome,
) {
    // Harvest guard: drop a fetch that landed after a reload removed/deactivated this account (§A).
    if !present_and_active(cfg, &outcome.account.id) {
        return;
    }
    let base = cfg.settings.poll_overlay_secs.max(1);
    let ZaiOverlayOutcome {
        account,
        backoff_current,
        result,
    } = outcome;
    let now = Timestamp::now();
    let parsed = result.and_then(|body| {
        parse_quota_response(
            &body,
            &account.id,
            cfg.settings.warn_pct,
            cfg.settings.crit_pct,
        )
    });
    match parsed {
        Ok(authoritative) if !authoritative.is_empty() => {
            apply_authoritative_limits(
                cfg,
                store,
                &account.id,
                authoritative,
                now,
                alerts,
                "zai overlay",
            );
            state.note_success(&account.id, backoff_current, base);
        }
        // Defensive-only: `parse_quota_response` returns `Err` (not `Ok(vec![])`) when neither
        // expected `TOKENS_LIMIT` entry is present, so this arm is unreachable given its current
        // contract — kept only as a guard against that contract changing underfoot.
        Ok(_) => {}
        Err(e) => {
            state.note_throttled(&account.id, backoff_current, base);
            let _ = store.mark_overlay_failing(&account.id, now.as_millisecond());
            eprintln!("collector: [{}] zai overlay fetch failed: {e}", account.id);
        }
    }
}

/// Whether `account_id` is still present AND active in the current config — the harvest-guard
/// membership check (spec 015 §A). A result for an account a reload removed or deactivated is dropped
/// so the collector never stamps fresh data onto a just-gone account.
fn present_and_active(cfg: &Config, account_id: &str) -> bool {
    cfg.accounts.iter().any(|a| a.id == account_id && a.active)
}

/// Apply one collect outcome to the store, honoring the generation guard and per-account isolation.
/// Thresholds + overlay TTL are read from the CURRENT config, so a hot-reload takes effect here.
fn apply_outcome(
    cfg: &Config,
    store: &Store,
    outcome: CollectOutcome,
    inflight: &mut HashSet<String>,
    latest_gen: &mut HashMap<String, u64>,
    alerts: &mut AlertTracker,
) {
    let CollectOutcome {
        account,
        generation,
        now,
        result,
    } = outcome;
    // Always clear the inflight guard first, even for a dropped account, so a later reactivation is
    // not blocked by a leaked in-flight marker.
    inflight.remove(&account.id);
    // Harvest guard: a result that lands after a reload removed/deactivated this account writes
    // nothing — no snapshot, no limit, no alert (spec 015 §A).
    if !present_and_active(cfg, &account.id) {
        return;
    }

    let latest = latest_gen.get(&account.id).copied().unwrap_or(0);
    if !should_apply(generation, latest) {
        return; // a newer result already landed
    }
    latest_gen.insert(account.id.clone(), generation);

    let warn = cfg.settings.warn_pct;
    let crit = cfg.settings.crit_pct;
    let overlay_ttl_secs = cfg
        .settings
        .poll_overlay_secs
        .saturating_mul(OVERLAY_TTL_FACTOR);

    match result {
        Ok(Some(snapshot)) => {
            if let Err(e) =
                persist_snapshot(store, &snapshot, now, warn, crit, alerts, overlay_ttl_secs)
            {
                eprintln!("collector: [{}] store write failed: {e}", account.id);
            }
        }
        // Idle (no active block) or failed: keep last-good usage, but STILL re-evaluate the stored
        // limits — otherwise the stale-authoritative demotion (spec 011 §C) only ever fires when
        // fresh data lands, and exactly the accounts that need it (idle, logged-out) freeze at
        // their last crit forever (spec 012 §B).
        Ok(None) => reevaluate_limits(store, &account.id, now, alerts, overlay_ttl_secs),
        Err(e) => {
            eprintln!("collector: [{}] collect failed: {e}", account.id); // isolation
            reevaluate_limits(store, &account.id, now, alerts, overlay_ttl_secs);
        }
    }
}

/// Re-run the shared merge point with no new evidence, so the stale-authoritative demotion applies
/// to idle / failing accounts too. Best-effort: a failure is logged, never fatal.
fn reevaluate_limits(
    store: &Store,
    account_id: &str,
    now: Timestamp,
    alerts: &mut AlertTracker,
    overlay_ttl_secs: u64,
) {
    if let Err(e) = apply_limits(store, account_id, Vec::new(), now, alerts, overlay_ttl_secs) {
        eprintln!("collector: [{account_id}] limit re-evaluation failed: {e}");
    }
}

/// Persist a snapshot and its derived session limit (via the shared merge+alert write path).
fn persist_snapshot(
    store: &Store,
    snapshot: &UsageSnapshot,
    now: Timestamp,
    warn: f64,
    crit: f64,
    alerts: &mut AlertTracker,
    overlay_ttl_secs: u64,
) -> AppResult<()> {
    store.insert_snapshot(snapshot)?;
    // Always run the merge point — with an empty set when no block is active — so the stale-
    // authoritative demotion applies on every outcome, not only when a derived limit exists.
    let new_limits: Vec<Limit> = derive_session_limit(snapshot, now, warn, crit)
        .into_iter()
        .collect();
    apply_limits(
        store,
        &snapshot.account_id,
        new_limits,
        snapshot.collected_at,
        alerts,
        overlay_ttl_secs,
    )?;
    Ok(())
}

/// Merge `new_limits` into an account's stored limits (authoritative never clobbered by derived),
/// persist, then fire edge-triggered alerts on any upward severity crossing.
fn apply_limits(
    store: &Store,
    account_id: &str,
    new_limits: Vec<Limit>,
    collected_at: Timestamp,
    alerts: &mut AlertTracker,
    overlay_ttl_secs: u64,
) -> AppResult<()> {
    let current = store.latest_limits(account_id)?;
    // Alert edges compare against what was actually on screen (the raw stored set), so a demotion
    // itself never fabricates an upward crossing.
    let previous: HashMap<(LimitKind, Option<String>), Severity> = current
        .iter()
        .map(|l| ((l.kind, l.scope.clone()), l.severity))
        .collect();

    // Demote a stale authoritative set to Estimate BEFORE merging, so a frozen overlay row can't keep
    // out-ranking the fresh derived session (spec 011 §C — the "degrade to derived" invariant) while
    // its last-known values stay visible with a live countdown.
    let overlay_success = store.last_overlay_success(account_id)?;
    let base = demote_stale_authoritative(
        current.clone(),
        overlay_success,
        collected_at.as_millisecond(),
        overlay_ttl_secs,
    );
    let merged = merge_limits(base, new_limits);
    if merged == current {
        return Ok(()); // nothing changed — skip the write (idle re-evaluation runs every tick)
    }
    store.set_limits(account_id, &merged, collected_at)?;

    let now = Instant::now();
    for limit in &merged {
        let prev = previous
            .get(&(limit.kind, limit.scope.clone()))
            .copied()
            .unwrap_or(Severity::Ok);
        let key = (account_id.to_string(), limit.kind, limit.scope.clone());
        if let Some(severity) = alerts.on_transition(key, prev, limit.severity, now) {
            let window = window_label(limit.kind, limit.scope.as_deref());
            notify_desktop(
                &format!(
                    "Tokenomics: {account_id} {window} {}",
                    severity_label(severity)
                ),
                &format!(
                    "{window} is now {} ({:.0}% used) — resets {}",
                    severity_label(severity),
                    limit.utilization_pct,
                    limit.resets_at
                ),
            );
        }
    }
    Ok(())
}

/// A human label for a limit window (used in the alert text).
fn window_label(kind: LimitKind, scope: Option<&str>) -> String {
    match kind {
        LimitKind::Session => "5h session".to_string(),
        LimitKind::WeeklyAll => "weekly".to_string(),
        LimitKind::WeeklyScoped => format!("weekly {}", scope.unwrap_or("scoped")),
    }
}

/// Resolve on SIGINT (Ctrl-C) or, on Unix, SIGTERM — for a clean daemon shutdown.
pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let Ok(mut term) = signal(SignalKind::terminate()) else {
            let _ = tokio::signal::ctrl_c().await;
            return;
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Settings;
    use crate::domain::{Provenance, Provider};
    use crate::providers::claude::overlay::{Canned, CannedEndpoint};
    use crate::providers::codex::rate_limits::CannedSource;
    use crate::providers::zai::quota::{Canned as ZaiCanned, CannedEndpoint as ZaiCannedEndpoint};
    use async_trait::async_trait;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// An idle Codex rate-limits source (empty response) for the Claude-only collector tests: their
    /// Codex overlay pass never fires, so the source is only threaded to satisfy `run_collector`'s
    /// new `C` generic. The Codex-specific tests below inject a canned/failing source of their own.
    fn idle_rate_source() -> CannedSource {
        CannedSource {
            response: String::new(),
        }
    }

    /// A never-invoked z.ai quota endpoint for the non-zai collector tests: no zai account is
    /// configured, so `spawn_zai_overlay_fetches` never calls it — only threaded to satisfy
    /// `run_collector`'s `Z` generic.
    fn idle_zai_endpoint() -> ZaiCannedEndpoint {
        ZaiCannedEndpoint {
            canned: ZaiCanned::Fail,
        }
    }

    /// An in-memory [`ConfigSource`] that hands out queued configs on successive polls (no filesystem):
    /// each `poll` delivers the next queued config as "a change", `None` once the queue is drained.
    struct QueuedConfigSource {
        queue: std::collections::VecDeque<Config>,
    }

    impl QueuedConfigSource {
        fn new(configs: Vec<Config>) -> Self {
            Self {
                queue: configs.into_iter().collect(),
            }
        }
    }

    impl ConfigSource for QueuedConfigSource {
        fn mtime_ms(&self) -> Option<i64> {
            None
        }
        fn config_path(&self) -> Option<String> {
            None
        }
        fn poll(&mut self) -> Option<Config> {
            self.queue.pop_front()
        }
    }

    /// A config source that never reports a change — the steady state for the tests that don't
    /// exercise hot-reload.
    fn no_reload() -> QueuedConfigSource {
        QueuedConfigSource::new(Vec::new())
    }

    fn auth_limit(kind: crate::domain::LimitKind, scope: Option<&str>, pct: f64) -> Limit {
        Limit {
            account_id: "acct".to_string(),
            provider: Provider::Claude,
            kind,
            scope: scope.map(str::to_string),
            utilization_pct: pct,
            resets_at: "2026-07-10T03:00:00Z".to_string(),
            severity: Severity::Ok,
            source: Provenance::Authoritative,
        }
    }

    fn derived_session(pct: f64) -> Limit {
        Limit {
            account_id: "acct".to_string(),
            provider: Provider::Claude,
            kind: crate::domain::LimitKind::Session,
            scope: None,
            utilization_pct: pct,
            resets_at: "2026-07-04T12:00:00Z".to_string(),
            severity: Severity::Ok,
            source: Provenance::Derived,
        }
    }

    #[test]
    fn stale_overlay_demotes_authoritative_to_derived_via_apply_limits() {
        use crate::domain::LimitKind;
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(&dir.path().join("t.db")).expect("open");
        store.upsert_accounts(&[account("acct")]).expect("upsert");
        let mut alerts = AlertTracker::new(Duration::from_secs(ALERT_COOLDOWN_SECS));
        let ttl = 600; // 2× a 300s cadence

        // Seed a full authoritative set from a "previous" overlay success, recorded long ago (stale).
        let seeded = "2026-07-04T10:00:00Z".parse::<Timestamp>().unwrap();
        store
            .set_limits(
                "acct",
                &[
                    auth_limit(LimitKind::Session, None, 29.0),
                    auth_limit(LimitKind::WeeklyAll, None, 91.0),
                    auth_limit(LimitKind::WeeklyScoped, Some("Fable"), 99.0),
                ],
                seeded,
            )
            .expect("seed");
        store
            .record_overlay_success("acct", seeded.as_millisecond())
            .expect("record");

        // A local derived tick 30 min later — the overlay is now stale (>ttl), so the derived session
        // must win while the weeklies survive as demoted estimates (last-known values, not n/a).
        let now = "2026-07-04T10:30:00Z".parse::<Timestamp>().unwrap();
        apply_limits(
            &store,
            "acct",
            vec![derived_session(62.0)],
            now,
            &mut alerts,
            ttl,
        )
        .expect("apply");
        let limits = store.latest_limits("acct").expect("read");
        assert_eq!(limits.len(), 3, "weeklies must survive: {limits:?}");
        let session = limits
            .iter()
            .find(|l| l.kind == LimitKind::Session)
            .expect("session");
        assert_eq!(session.source, Provenance::Derived);
        assert!((session.utilization_pct - 62.0).abs() < 1e-9);
        let weekly = limits
            .iter()
            .find(|l| l.kind == LimitKind::WeeklyAll)
            .expect("weekly");
        assert_eq!(weekly.source, Provenance::Estimate);
        assert!((weekly.utilization_pct - 91.0).abs() < 1e-9);
        let scoped = limits
            .iter()
            .find(|l| l.kind == LimitKind::WeeklyScoped)
            .expect("scoped");
        assert_eq!(scoped.source, Provenance::Estimate);
        assert!((scoped.utilization_pct - 99.0).abs() < 1e-9);
    }

    #[test]
    fn fresh_overlay_keeps_authoritative_over_derived_via_apply_limits() {
        use crate::domain::LimitKind;
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(&dir.path().join("t.db")).expect("open");
        store.upsert_accounts(&[account("acct")]).expect("upsert");
        let mut alerts = AlertTracker::new(Duration::from_secs(ALERT_COOLDOWN_SECS));
        let ttl = 600;

        let now = "2026-07-04T10:30:00Z".parse::<Timestamp>().unwrap();
        store
            .set_limits(
                "acct",
                &[
                    auth_limit(LimitKind::Session, None, 29.0),
                    auth_limit(LimitKind::WeeklyAll, None, 91.0),
                ],
                now,
            )
            .expect("seed");
        // Overlay succeeded 2 min ago — fresh (≤ ttl); the derived tick must NOT clobber it.
        let fresh = "2026-07-04T10:28:00Z".parse::<Timestamp>().unwrap();
        store
            .record_overlay_success("acct", fresh.as_millisecond())
            .expect("record");

        apply_limits(
            &store,
            "acct",
            vec![derived_session(62.0)],
            now,
            &mut alerts,
            ttl,
        )
        .expect("apply");
        let limits = store.latest_limits("acct").expect("read");
        let session = limits
            .iter()
            .find(|l| l.kind == LimitKind::Session)
            .expect("session");
        assert_eq!(session.source, Provenance::Authoritative);
        assert!((session.utilization_pct - 29.0).abs() < 1e-9);
        assert!(limits.iter().any(|l| l.kind == LimitKind::WeeklyAll));
    }

    #[test]
    fn generation_guard_drops_stale_results() {
        assert!(should_apply(1, 0)); // first result
        assert!(should_apply(2, 1)); // newer
        assert!(!should_apply(1, 1)); // duplicate
        assert!(!should_apply(1, 2)); // stale
    }

    struct FakeAdapter;

    #[async_trait]
    impl ProviderAdapter for FakeAdapter {
        async fn collect(
            &self,
            account: &Account,
            now: Timestamp,
        ) -> AppResult<Option<UsageSnapshot>> {
            Ok(Some(UsageSnapshot {
                account_id: account.id.clone(),
                provider: Provider::Claude,
                collected_at: now,
                input: 1,
                output: 1,
                cache_read: 1,
                cache_creation: 1,
                total_tokens: 4,
                cost_notional: Some(0.1),
                window: None,
            }))
        }
    }

    fn account(id: &str) -> Account {
        Account {
            id: id.to_string(),
            label: id.to_uppercase(),
            provider: Provider::Claude,
            config_dir: Some(PathBuf::from("/tmp")),
            api_key_env: None,
            color: None,
            active: true,
            limits_overlay: false,
        }
    }

    fn config(accounts: Vec<Account>) -> Config {
        Config {
            settings: Settings {
                poll_local_secs: 5,
                poll_overlay_secs: 300,
                warn_pct: 75.0,
                crit_pct: 90.0,
                ccusage_cmd: None,
                ledger_path: None,
            },
            accounts,
        }
    }

    #[tokio::test]
    async fn loop_collects_all_accounts_then_shuts_down() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.db");
        let store = Store::open(&path).expect("open");
        let cfg = config(vec![account("a"), account("b"), account("c")]);

        // The first interval tick fires immediately, so one full collection happens well within
        // the 300ms window before shutdown. Accounts are opted-out, so the overlay never fetches.
        let endpoint = CannedEndpoint {
            canned: Canned::Fail,
        };
        run_collector(
            &cfg,
            no_reload(),
            FakeAdapter,
            endpoint,
            idle_rate_source(),
            idle_zai_endpoint(),
            store,
            async {
                tokio::time::sleep(Duration::from_millis(300)).await;
            },
        )
        .await
        .expect("loop runs clean");

        let store = Store::open(&path).expect("reopen");
        for id in ["a", "b", "c"] {
            assert!(
                store.latest_snapshot(id).expect("query").is_some(),
                "account {id} should have a persisted snapshot"
            );
        }
    }

    #[tokio::test]
    async fn inactive_account_is_skipped_by_both_local_and_overlay_passes() {
        // An inactive account — even opted into the overlay with a warm token — must get neither a
        // local collect NOR an overlay fetch (spec 014 §B, acceptance criteria 2); its active sibling
        // still gets both. `upsert_accounts` still records the inactive account's identity.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.db");
        let store = Store::open(&path).expect("open");

        let dead_dir = tempfile::tempdir().expect("dead creds dir");
        let mut dead = opted_in_account("dead", dead_dir.path());
        dead.active = false;
        let cfg = config(vec![account("alive"), dead]);

        let endpoint = CannedEndpoint {
            canned: Canned::Body(
                br#"{"five_hour":{"utilization":19.0,"resets_at":"2026-07-04T12:00:00Z"}}"#
                    .to_vec(),
            ),
        };
        run_collector(
            &cfg,
            no_reload(),
            FakeAdapter,
            endpoint,
            idle_rate_source(),
            idle_zai_endpoint(),
            store,
            async {
                tokio::time::sleep(Duration::from_millis(300)).await;
            },
        )
        .await
        .expect("loop runs clean");

        let store = Store::open(&path).expect("reopen");
        assert!(
            store.latest_snapshot("alive").expect("query").is_some(),
            "the active account must still be collected"
        );
        assert!(
            store.latest_snapshot("dead").expect("query").is_none(),
            "an inactive account must never be collected locally"
        );
        assert!(
            store.latest_limits("dead").expect("query").is_empty(),
            "an inactive account must never get an overlay fetch, even opted-in with a warm token"
        );
    }

    /// An adapter that always reports "idle" (no active block) — the account produces no new
    /// snapshots, exactly the case where the demotion used to never fire (spec 012 §B).
    struct IdleAdapter;

    #[async_trait]
    impl ProviderAdapter for IdleAdapter {
        async fn collect(
            &self,
            _account: &Account,
            _now: Timestamp,
        ) -> AppResult<Option<UsageSnapshot>> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn idle_account_still_demotes_stale_authoritative_limits() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.db");
        let store = Store::open(&path).expect("open");
        store.upsert_accounts(&[account("acct")]).expect("upsert");

        // A frozen authoritative set from an overlay success far in the past (way beyond the TTL).
        let long_ago = Timestamp::now().as_millisecond() - 24 * 60 * 60 * 1000;
        store
            .set_limits(
                "acct",
                &[
                    auth_limit(crate::domain::LimitKind::WeeklyAll, None, 100.0),
                    auth_limit(crate::domain::LimitKind::WeeklyScoped, Some("Fable"), 99.0),
                ],
                Timestamp::now(),
            )
            .expect("seed");
        store
            .record_overlay_success("acct", long_ago)
            .expect("record");

        // The account is idle (Ok(None)) and opted out of the overlay — before spec 012 this loop
        // never re-evaluated its limits and the crit rows persisted for days.
        let cfg = config(vec![account("acct")]);
        let endpoint = CannedEndpoint {
            canned: Canned::Fail,
        };
        run_collector(
            &cfg,
            no_reload(),
            IdleAdapter,
            endpoint,
            idle_rate_source(),
            idle_zai_endpoint(),
            store,
            async {
                tokio::time::sleep(Duration::from_millis(300)).await;
            },
        )
        .await
        .expect("loop runs clean");

        let store = Store::open(&path).expect("reopen");
        let limits = store.latest_limits("acct").expect("read");
        assert_eq!(
            limits.len(),
            2,
            "stale authoritative limits must survive demotion on an idle tick, got {limits:?}"
        );
        assert!(
            limits.iter().all(|l| l.source == Provenance::Estimate),
            "idle-tick demotion must drop rank to Estimate, got {limits:?}"
        );
    }

    #[tokio::test]
    async fn opted_in_account_gets_authoritative_limits_from_the_overlay() {
        // A warm creds file in the account's config dir (owner-only), and a canned 200 body.
        let creds_dir = tempfile::tempdir().expect("creds dir");
        let creds_path = creds_dir.path().join(".credentials.json");
        std::fs::write(
            &creds_path,
            br#"{"claudeAiOauth":{"accessToken":"tok","refreshToken":"r","expiresAt":4102444800000,"scopes":["a","b","c","d","e"],"subscriptionType":"max","rateLimitTier":"default"}}"#,
        )
        .expect("write creds");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&creds_path, std::fs::Permissions::from_mode(0o600))
                .expect("chmod 600");
        }

        let store_dir = tempfile::tempdir().expect("store dir");
        let store = Store::open(&store_dir.path().join("t.db")).expect("open store");

        let mut acct = account("personal");
        acct.limits_overlay = true;
        acct.config_dir = Some(creds_dir.path().to_path_buf());
        let cfg = config(vec![acct]);

        let body = br#"{"five_hour":{"utilization":19.0,"resets_at":"2026-07-04T12:00:00Z"},
                        "seven_day":{"utilization":91.0,"resets_at":"2026-07-08T00:00:00Z"}}"#;
        let endpoint = CannedEndpoint {
            canned: Canned::Body(body.to_vec()),
        };

        run_collector(
            &cfg,
            no_reload(),
            FakeAdapter,
            endpoint,
            idle_rate_source(),
            idle_zai_endpoint(),
            store,
            async {
                tokio::time::sleep(Duration::from_millis(300)).await;
            },
        )
        .await
        .expect("loop runs clean");

        let store = Store::open(&store_dir.path().join("t.db")).expect("reopen");
        let limits = store.latest_limits("personal").expect("limits");
        assert!(
            limits.iter().any(|l| l.source == Provenance::Authoritative
                && l.kind == crate::domain::LimitKind::Session),
            "expected an authoritative session limit, got {limits:?}"
        );
        assert!(
            limits
                .iter()
                .any(|l| l.kind == crate::domain::LimitKind::WeeklyAll),
            "expected a weekly limit from the overlay"
        );
        assert_eq!(
            store.latest_token_status("personal").expect("token status"),
            Some(TokenStatus::Warm)
        );
    }

    #[tokio::test]
    async fn malformed_overlay_body_marks_the_account_failing() {
        // Finding 3: a persistently-malformed HTTP-200 overlay body is a parse failure (not a 429),
        // yet it must still pin the failing-since marker so the TUI can trip "overlay stalled — check
        // account" — the Claude twin of `failing_codex_overlay_fetch_marks_the_account_failing`.
        let creds_dir = tempfile::tempdir().expect("creds dir");
        let acct = opted_in_account("acct", creds_dir.path()); // warm token ⇒ the fetch is attempted
        let cfg = config(vec![acct]);

        let store_dir = tempfile::tempdir().expect("store dir");
        let path = store_dir.path().join("t.db");
        let store = Store::open(&path).expect("open store");

        // A warm token fetches successfully (HTTP 200), but the body doesn't parse as usage JSON.
        let endpoint = CannedEndpoint {
            canned: Canned::Body(b"not usage json at all".to_vec()),
        };

        run_collector(
            &cfg,
            no_reload(),
            FakeAdapter,
            endpoint,
            idle_rate_source(),
            idle_zai_endpoint(),
            store,
            async {
                tokio::time::sleep(Duration::from_millis(300)).await;
            },
        )
        .await
        .expect("loop runs clean");

        let store = Store::open(&path).expect("reopen");
        assert!(
            store
                .overlay_failing_since("acct")
                .expect("query")
                .is_some(),
            "a malformed overlay body must pin the failing-since marker"
        );
        assert!(
            store.latest_limits("acct").expect("query").is_empty(),
            "a malformed body lands no authoritative limits"
        );
    }

    /// Write an owner-only warm creds file into `dir` and return an opted-in account pointed at it.
    fn opted_in_account(id: &str, dir: &std::path::Path) -> Account {
        let creds_path = dir.join(".credentials.json");
        std::fs::write(
            &creds_path,
            br#"{"claudeAiOauth":{"accessToken":"tok","refreshToken":"r","expiresAt":4102444800000,"scopes":["a","b","c","d","e"],"subscriptionType":"max","rateLimitTier":"default"}}"#,
        )
        .expect("write creds");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&creds_path, std::fs::Permissions::from_mode(0o600))
                .expect("chmod 600");
        }
        let mut acct = account(id);
        acct.limits_overlay = true;
        acct.config_dir = Some(dir.to_path_buf());
        acct
    }

    /// An overlay endpoint that sleeps before returning a fixed 200 body — used to prove the pass
    /// fetches accounts CONCURRENTLY (sequentially, 3×150ms would overrun the 300ms test window).
    #[derive(Debug)]
    struct DelayedEndpoint {
        delay: Duration,
        body: Vec<u8>,
    }

    #[async_trait]
    impl UsageEndpoint for DelayedEndpoint {
        async fn fetch(&self, _access_token: &str) -> AppResult<Vec<u8>> {
            tokio::time::sleep(self.delay).await;
            Ok(self.body.clone())
        }
    }

    #[tokio::test]
    async fn overlay_pass_refreshes_all_opted_in_accounts_concurrently() {
        // Three opted-in accounts, each fetch delayed 150ms. Sequentially that is 450ms and only ~1–2
        // would land inside the 300ms window (the OVL-3 starvation). Concurrently all three land.
        let dirs: Vec<tempfile::TempDir> =
            (0..3).map(|_| tempfile::tempdir().expect("dir")).collect();
        let accounts: Vec<Account> = dirs
            .iter()
            .enumerate()
            .map(|(i, d)| opted_in_account(&format!("a{i}"), d.path()))
            .collect();
        let cfg = config(accounts);

        let store_dir = tempfile::tempdir().expect("store dir");
        let path = store_dir.path().join("t.db");
        let store = Store::open(&path).expect("open store");

        let body = br#"{"five_hour":{"utilization":19.0,"resets_at":"2026-07-04T12:00:00Z"}}"#;
        let endpoint = DelayedEndpoint {
            delay: Duration::from_millis(150),
            body: body.to_vec(),
        };

        run_collector(
            &cfg,
            no_reload(),
            FakeAdapter,
            endpoint,
            idle_rate_source(),
            idle_zai_endpoint(),
            store,
            async {
                tokio::time::sleep(Duration::from_millis(300)).await;
            },
        )
        .await
        .expect("loop runs clean");

        let store = Store::open(&path).expect("reopen");
        for i in 0..3 {
            let id = format!("a{i}");
            let limits = store.latest_limits(&id).expect("limits");
            assert!(
                limits.iter().any(|l| l.source == Provenance::Authoritative),
                "account {id} should have refreshed within one concurrent pass, got {limits:?}"
            );
        }
    }

    #[tokio::test]
    async fn recovery_recheck_is_silent_when_stale_but_fetches_a_freshly_warm_token() {
        // The fast local-tick recovery path: a still-stale token is a silent no-op (no stale row
        // written, no fetch), but the moment the creds file is warm it fetches immediately — off the
        // slow overlay cadence — and flips the store to Warm. This is the "I opened Claude, why still
        // stale for 5 min?" fix.
        let store_dir = tempfile::tempdir().expect("store dir");
        let store = Store::open(&store_dir.path().join("t.db")).expect("open store");

        // Expired creds (expiresAt in the distant past).
        let stale_dir = tempfile::tempdir().expect("stale dir");
        let stale_path = stale_dir.path().join(".credentials.json");
        std::fs::write(
            &stale_path,
            br#"{"claudeAiOauth":{"accessToken":"t","refreshToken":"r","expiresAt":1000,"scopes":["a","b","c","d","e"],"subscriptionType":"max","rateLimitTier":"default"}}"#,
        )
        .expect("write stale creds");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stale_path, std::fs::Permissions::from_mode(0o600))
                .expect("chmod 600");
        }
        let mut stale_acct = account("stale");
        stale_acct.limits_overlay = true;
        stale_acct.config_dir = Some(stale_dir.path().to_path_buf());

        let warm_dir = tempfile::tempdir().expect("warm dir");
        let warm_acct = opted_in_account("warm", warm_dir.path());
        // token_state FK-references accounts, so register both rows (the loop does this via
        // upsert_accounts on startup).
        store
            .upsert_accounts(&[stale_acct.clone(), warm_acct.clone()])
            .expect("upsert accounts");

        let endpoint = Arc::new(CannedEndpoint {
            canned: Canned::Body(
                br#"{"five_hour":{"utilization":19.0,"resets_at":"2026-07-04T12:00:00Z"}}"#
                    .to_vec(),
            ),
        });
        let mut state = OverlayState::default();
        let mut tasks: JoinSet<OverlayOutcome> = JoinSet::new();
        let now_ms = 5000; // past the stale expiry (1000)

        // Still stale + recovery mode (log_stale = false): no fetch, and crucially no stale row.
        assert!(!try_spawn_overlay_fetch(
            &stale_acct,
            &endpoint,
            &store,
            &mut state,
            &mut tasks,
            now_ms,
            300,
            false,
        ));
        assert!(
            tasks.is_empty(),
            "a still-stale token must not spawn a fetch"
        );
        assert_eq!(
            store.latest_token_status("stale").expect("status"),
            None,
            "the silent recovery recheck must not write a stale row"
        );

        // A freshly warm token recovers immediately: fetch spawned, store flipped to Warm.
        assert!(try_spawn_overlay_fetch(
            &warm_acct, &endpoint, &store, &mut state, &mut tasks, now_ms, 300, false,
        ));
        assert_eq!(
            store.latest_token_status("warm").expect("status"),
            Some(TokenStatus::Warm),
            "a warm token must recover off the periodic cadence"
        );
    }

    #[tokio::test]
    async fn slow_overlay_pass_does_not_block_the_loop() {
        // The overlay fetch sleeps 2s; shutdown is requested at 150ms. Because the fetch runs OFF the
        // loop task (spawned), shutdown is honored immediately (~150ms). The old inline-await pass
        // blocked the select! until the whole fetch returned (~2s) — this asserts that regression is
        // gone. It is the true behavioral proof of "overlay off the critical path" (spec 011 §D).
        let creds_dir = tempfile::tempdir().expect("creds dir");
        let acct = opted_in_account("slow", creds_dir.path());
        let cfg = config(vec![acct]);
        let store_dir = tempfile::tempdir().expect("store dir");
        let store = Store::open(&store_dir.path().join("t.db")).expect("open");

        let endpoint = DelayedEndpoint {
            delay: Duration::from_secs(2),
            body: br#"{"five_hour":{"utilization":19.0,"resets_at":"2026-07-04T12:00:00Z"}}"#
                .to_vec(),
        };

        let start = std::time::Instant::now();
        run_collector(
            &cfg,
            no_reload(),
            FakeAdapter,
            endpoint,
            idle_rate_source(),
            idle_zai_endpoint(),
            store,
            async {
                tokio::time::sleep(Duration::from_millis(150)).await;
            },
        )
        .await
        .expect("loop runs clean");
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(1),
            "overlay pass blocked the loop for {elapsed:?} (should honor shutdown immediately)"
        );
    }

    /// An opted-in Codex account pointed at `dir` (its CODEX_HOME). No creds file — Codex auth lives
    /// inside the binary; the overlay path uses the rate-limits source, not a token.
    fn codex_account(id: &str, dir: &std::path::Path) -> Account {
        Account {
            id: id.to_string(),
            label: id.to_uppercase(),
            provider: Provider::Codex,
            config_dir: Some(dir.to_path_buf()),
            api_key_env: None,
            color: None,
            active: true,
            limits_overlay: true,
        }
    }

    /// The live-shape `account/rateLimits/read` response (codex-cli 0.144.1): primary (5h) +
    /// secondary (weekly), each `usedPercent`/`resetsAt`, plus siblings serde ignores.
    const CODEX_RATE_LIMITS: &str = r#"{"id":1,"result":{"rateLimits":{"primary":{"usedPercent":42,"windowDurationMins":300,"resetsAt":1783733780},"secondary":{"usedPercent":88,"windowDurationMins":10080,"resetsAt":1784320580},"planType":"team"}}}"#;

    #[tokio::test]
    async fn codex_account_gets_authoritative_limits_from_the_app_server_overlay() {
        // A Codex account opted into the overlay gets authoritative Session + WeeklyAll limits from
        // the `codex app-server` pass (via a canned source — no subprocess); a Claude sibling on the
        // untouched local plane still collects. Proves the two overlay planes coexist (spec 013 §C).
        let store_dir = tempfile::tempdir().expect("store dir");
        let path = store_dir.path().join("t.db");
        let store = Store::open(&path).expect("open store");

        let codex_dir = tempfile::tempdir().expect("codex home");
        let cfg = config(vec![
            account("claude"),
            codex_account("codex", codex_dir.path()),
        ]);

        // The Claude account is opted out, so its (Claude) overlay never fetches; the endpoint is
        // inert here. The Codex account rides the canned rate-limits source.
        let endpoint = CannedEndpoint {
            canned: Canned::Fail,
        };
        let source = CannedSource {
            response: CODEX_RATE_LIMITS.to_string(),
        };

        run_collector(
            &cfg,
            no_reload(),
            FakeAdapter,
            endpoint,
            source,
            idle_zai_endpoint(),
            store,
            async {
                tokio::time::sleep(Duration::from_millis(300)).await;
            },
        )
        .await
        .expect("loop runs clean");

        let store = Store::open(&path).expect("reopen");
        let limits = store.latest_limits("codex").expect("codex limits");
        let session = limits
            .iter()
            .find(|l| l.kind == crate::domain::LimitKind::Session)
            .expect("authoritative session");
        assert_eq!(session.provider, Provider::Codex);
        assert_eq!(session.source, Provenance::Authoritative);
        assert!((session.utilization_pct - 42.0).abs() < 1e-9);
        let weekly = limits
            .iter()
            .find(|l| l.kind == crate::domain::LimitKind::WeeklyAll)
            .expect("authoritative weekly");
        assert_eq!(weekly.source, Provenance::Authoritative);
        assert!((weekly.utilization_pct - 88.0).abs() < 1e-9);
        // The Claude sibling's local plane is untouched — it still gets its (fake) snapshot.
        assert!(
            store.latest_snapshot("claude").expect("query").is_some(),
            "the Claude account's local collect must be unaffected"
        );
        // Codex never gets a token_state row (no TokenStatus concept).
        assert_eq!(
            store.latest_token_status("codex").expect("token status"),
            None,
            "a Codex account must never write a token_state row"
        );
    }

    #[tokio::test]
    async fn failing_codex_overlay_fetch_marks_the_account_failing() {
        // A malformed app-server body (no rateLimits) is a fetch failure: the account is pinned
        // failing-since (the "backs off" signal) and lands no authoritative limits. A live account's
        // failure clears on the next success; a dead one keeps failing (drives the "check account"
        // flag) — same posture as a Claude overlay failure (spec 013 §C).
        let store_dir = tempfile::tempdir().expect("store dir");
        let path = store_dir.path().join("t.db");
        let store = Store::open(&path).expect("open store");

        let codex_dir = tempfile::tempdir().expect("codex home");
        let cfg = config(vec![codex_account("codex", codex_dir.path())]);

        let endpoint = CannedEndpoint {
            canned: Canned::Fail,
        };
        // A well-formed JSON-RPC response line with no `rateLimits` ⇒ `parse_rate_limits_response`
        // errors ⇒ the failure branch fires.
        let source = CannedSource {
            response: r#"{"id":1,"result":{"somethingElse":true}}"#.to_string(),
        };

        run_collector(
            &cfg,
            no_reload(),
            IdleAdapter,
            endpoint,
            source,
            idle_zai_endpoint(),
            store,
            async {
                tokio::time::sleep(Duration::from_millis(300)).await;
            },
        )
        .await
        .expect("loop runs clean");

        let store = Store::open(&path).expect("reopen");
        assert!(
            store
                .overlay_failing_since("codex")
                .expect("query")
                .is_some(),
            "a failing codex overlay fetch must pin the failing-since marker"
        );
        assert!(
            store.latest_limits("codex").expect("query").is_empty(),
            "a failing fetch must land no authoritative limits"
        );
    }

    // ── spec 019 §D (AC4) — z.ai overlay integration, exercised directly via spawn/apply (also
    // wired into `run_collector`'s select! loop as its 4th JoinSet arm, see above) ────────────────

    /// An opted-in zai account whose `api_key_env` names `env_var`. Its actual presence in the real
    /// process environment is up to each test (unique names avoid parallel-test collisions).
    fn zai_account(id: &str, env_var: &str) -> Account {
        Account {
            id: id.to_string(),
            label: id.to_uppercase(),
            provider: Provider::Zai,
            config_dir: None,
            api_key_env: Some(env_var.to_string()),
            color: None,
            active: true,
            limits_overlay: true,
        }
    }

    #[tokio::test]
    async fn opted_in_zai_account_produces_limits_on_the_overlay_pass() {
        // Encodes the end-to-end shape (spec 019 AC4): an opted-in zai account with a present key
        // and a 200 response lands Session + WeeklyAll limits via the real `parse_quota_response`
        // mapping (`providers::zai::quota`).
        let store_dir = tempfile::tempdir().expect("store dir");
        let store = Store::open(&store_dir.path().join("t.db")).expect("open store");
        let env_var = "TOK_TEST_ZAI_KEY_OPTED_IN";
        std::env::set_var(env_var, "fake-key-never-logged");
        let cfg = config(vec![zai_account("zai-lite", env_var)]);
        store.upsert_accounts(&cfg.accounts).expect("upsert");

        let endpoint = Arc::new(ZaiCannedEndpoint {
            canned: ZaiCanned::Body(
                br#"{"data":{"limits":[
                    {"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":42},
                    {"type":"TOKENS_LIMIT","unit":6,"number":1,"percentage":81,"nextResetTime":1784713715974}
                ]}}"#
                    .to_vec(),
            ),
        });
        let mut state = OverlayState::default();
        let mut tasks: JoinSet<ZaiOverlayOutcome> = JoinSet::new();
        spawn_zai_overlay_fetches(&cfg, &endpoint, &mut state, &mut tasks);
        let mut alerts = AlertTracker::new(Duration::from_secs(ALERT_COOLDOWN_SECS));
        while let Some(joined) = tasks.join_next().await {
            apply_zai_overlay_outcome(&cfg, &store, &mut state, &mut alerts, joined.expect("join"));
        }

        std::env::remove_var(env_var);
        let limits = store.latest_limits("zai-lite").expect("query");
        assert!(
            !limits.is_empty(),
            "an opted-in zai account with a 200 response should produce Session + WeeklyAll limits"
        );
    }

    #[tokio::test]
    async fn un_opted_in_zai_account_is_skipped_with_no_fetch() {
        let mut acct = zai_account("zai-lite", "TOK_TEST_ZAI_KEY_NOT_OPTED_IN");
        acct.limits_overlay = false;
        std::env::set_var("TOK_TEST_ZAI_KEY_NOT_OPTED_IN", "fake-key-never-logged");
        let cfg = config(vec![acct]);

        let endpoint = Arc::new(ZaiCannedEndpoint {
            canned: ZaiCanned::Fail,
        });
        let mut state = OverlayState::default();
        let mut tasks: JoinSet<ZaiOverlayOutcome> = JoinSet::new();
        spawn_zai_overlay_fetches(&cfg, &endpoint, &mut state, &mut tasks);

        std::env::remove_var("TOK_TEST_ZAI_KEY_NOT_OPTED_IN");
        assert_eq!(
            tasks.len(),
            0,
            "an un-opted-in zai account must never be fetched"
        );
    }

    #[tokio::test]
    async fn env_var_missing_zai_account_is_skipped_with_no_fetch() {
        // Deliberately never set — the eligibility check must reject an absent/empty env var, not
        // just an opted-out account.
        let acct = zai_account("zai-lite", "TOK_TEST_ZAI_KEY_DOES_NOT_EXIST");
        let cfg = config(vec![acct]);

        let endpoint = Arc::new(ZaiCannedEndpoint {
            canned: ZaiCanned::Fail,
        });
        let mut state = OverlayState::default();
        let mut tasks: JoinSet<ZaiOverlayOutcome> = JoinSet::new();
        spawn_zai_overlay_fetches(&cfg, &endpoint, &mut state, &mut tasks);

        assert_eq!(
            tasks.len(),
            0,
            "a zai account whose env var is unset must never be fetched"
        );
    }

    #[tokio::test]
    async fn zai_overlay_pass_never_touches_claude_or_codex_accounts() {
        // claude/codex collection stays byte-identical: `spawn_zai_overlay_fetches` must only ever
        // pick up `Provider::Zai` accounts, even when a claude/codex sibling is also opted in.
        let env_var = "TOK_TEST_ZAI_KEY_MIXED_FLEET";
        std::env::set_var(env_var, "fake-key-never-logged");
        let mut claude_acct = account("claude-acct");
        claude_acct.limits_overlay = true;
        let cfg = config(vec![claude_acct, zai_account("zai-lite", env_var)]);

        let endpoint = Arc::new(ZaiCannedEndpoint {
            canned: ZaiCanned::Fail,
        });
        let mut state = OverlayState::default();
        let mut tasks: JoinSet<ZaiOverlayOutcome> = JoinSet::new();
        spawn_zai_overlay_fetches(&cfg, &endpoint, &mut state, &mut tasks);

        std::env::remove_var(env_var);
        assert_eq!(tasks.len(), 1, "only the zai account is fetched");
        let outcome = tasks.join_next().await.expect("one task").expect("join");
        assert_eq!(outcome.account.id, "zai-lite");
    }

    #[tokio::test]
    async fn zai_overlay_failure_backs_off_and_demotes_stale_authoritative_to_estimate() {
        // spec 019 AC3 (the ERROR path, end to end): a failing zai overlay fetch must (a) advance
        // backoff/cooldown, (b) mark the account overlay-failing, and (c) — once the failure persists
        // past the TTL — demote the account's now-stale authoritative limits to `Estimate` on the next
        // re-evaluation. Same machinery, same posture as the Codex twin
        // (`failing_codex_overlay_fetch_marks_the_account_failing`) plus the TTL-demotion half that
        // twin doesn't itself assert (covered generically by `stale_overlay_demotes_authoritative_to_
        // derived_via_apply_limits` above — this pins it for zai specifically, per the contract).
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(&dir.path().join("t.db")).expect("open");
        let env_var = "TOK_TEST_ZAI_KEY_FAILURE_BACKOFF";
        std::env::set_var(env_var, "fake-key-never-logged");
        let cfg = config(vec![zai_account("zai-lite", env_var)]);
        store.upsert_accounts(&cfg.accounts).expect("upsert");

        // Seed an authoritative set from a "previous" successful overlay pass, recorded long ago.
        let seeded = "2026-07-04T10:00:00Z".parse::<Timestamp>().unwrap();
        store
            .set_limits(
                "zai-lite",
                &[
                    Limit {
                        account_id: "zai-lite".to_string(),
                        provider: Provider::Zai,
                        kind: crate::domain::LimitKind::Session,
                        scope: None,
                        utilization_pct: 42.0,
                        resets_at: String::new(),
                        severity: Severity::Ok,
                        source: Provenance::Authoritative,
                    },
                    Limit {
                        account_id: "zai-lite".to_string(),
                        provider: Provider::Zai,
                        kind: crate::domain::LimitKind::WeeklyAll,
                        scope: None,
                        utilization_pct: 81.0,
                        resets_at: "2026-07-10T00:00:00Z".to_string(),
                        severity: Severity::Ok,
                        source: Provenance::Authoritative,
                    },
                ],
                seeded,
            )
            .expect("seed");
        store
            .record_overlay_success("zai-lite", seeded.as_millisecond())
            .expect("record");

        // Drive one failing fetch through the real spawn + harvest path (a 429, same as an exhausted
        // key or dead subscription would produce).
        let endpoint = Arc::new(ZaiCannedEndpoint {
            canned: ZaiCanned::RateLimited,
        });
        let mut state = OverlayState::default();
        let mut tasks: JoinSet<ZaiOverlayOutcome> = JoinSet::new();
        spawn_zai_overlay_fetches(&cfg, &endpoint, &mut state, &mut tasks);
        std::env::remove_var(env_var);
        let mut alerts = AlertTracker::new(Duration::from_secs(ALERT_COOLDOWN_SECS));
        while let Some(joined) = tasks.join_next().await {
            apply_zai_overlay_outcome(&cfg, &store, &mut state, &mut alerts, joined.expect("join"));
        }

        // (a) backoff/cooldown advanced for the account.
        let base = cfg.settings.poll_overlay_secs.max(1);
        assert!(
            state.backoff_secs.get("zai-lite").copied().unwrap_or(base) > base,
            "a failed fetch must grow the account's backoff: {:?}",
            state.backoff_secs
        );
        assert!(
            state.cooldown_ticks.get("zai-lite").copied().unwrap_or(0) > 0,
            "a failed fetch must set a cooldown: {:?}",
            state.cooldown_ticks
        );

        // (b) the account is marked overlay-failing.
        assert!(
            store
                .overlay_failing_since("zai-lite")
                .expect("query")
                .is_some(),
            "a failing zai overlay fetch must pin the failing-since marker"
        );

        // The seeded authoritative limits are untouched immediately after the failure — the TTL
        // hasn't elapsed yet, so nothing demotes prematurely.
        let limits = store.latest_limits("zai-lite").expect("query");
        assert!(
            !limits.is_empty() && limits.iter().all(|l| l.source == Provenance::Authoritative),
            "the stale set must not demote before the TTL elapses: {limits:?}"
        );

        // (c) once the last overlay success ages past the TTL, the next re-evaluation demotes the
        // stale authoritative set to Estimate (spec 011 §C — provider-agnostic demotion).
        let ttl = cfg
            .settings
            .poll_overlay_secs
            .saturating_mul(OVERLAY_TTL_FACTOR);
        let past_ttl = "2026-07-04T10:30:00Z".parse::<Timestamp>().unwrap(); // 30 min > 10 min ttl
        reevaluate_limits(&store, "zai-lite", past_ttl, &mut alerts, ttl);

        let demoted = store.latest_limits("zai-lite").expect("query");
        assert!(
            !demoted.is_empty() && demoted.iter().all(|l| l.source == Provenance::Estimate),
            "the stale authoritative zai limits must demote to Estimate past the TTL: {demoted:?}"
        );
    }

    #[tokio::test]
    async fn reload_activating_an_account_starts_local_and_overlay_and_applies_new_thresholds() {
        // Spec 015 §A / acceptance 1 + 2: an account flipped active=false→true mid-run joins the next
        // local pass (a snapshot lands) AND the next overlay pass (authoritative limits land, via the
        // local-tick warm-token recovery — deterministic, no overlay-arm race). The reactivation config
        // also lowers crit_pct, so the reloaded threshold governs the new limit's severity.
        let store_dir = tempfile::tempdir().expect("store dir");
        let path = store_dir.path().join("t.db");
        let store = Store::open(&path).expect("open store");

        let creds_dir = tempfile::tempdir().expect("creds dir");
        let mut before = opted_in_account("acct", creds_dir.path());
        before.active = false; // starts inactive — skipped by both planes
        let cfg_before = config(vec![before]);

        let mut cfg_after = config(vec![opted_in_account("acct", creds_dir.path())]);
        cfg_after.settings.warn_pct = 50.0;
        cfg_after.settings.crit_pct = 60.0; // 91% weekly → Crit under the reloaded threshold

        let source = QueuedConfigSource::new(vec![cfg_after]);
        let endpoint = CannedEndpoint {
            canned: Canned::Body(
                br#"{"five_hour":{"utilization":19.0,"resets_at":"2026-07-04T12:00:00Z"},
                     "seven_day":{"utilization":91.0,"resets_at":"2026-07-08T00:00:00Z"}}"#
                    .to_vec(),
            ),
        };

        run_collector(
            &cfg_before,
            source,
            FakeAdapter,
            endpoint,
            idle_rate_source(),
            idle_zai_endpoint(),
            store,
            async {
                tokio::time::sleep(Duration::from_millis(300)).await;
            },
        )
        .await
        .expect("loop runs clean");

        let store = Store::open(&path).expect("reopen");
        assert!(
            store.latest_snapshot("acct").expect("query").is_some(),
            "the reactivated account must be collected on the reload tick"
        );
        let limits = store.latest_limits("acct").expect("limits");
        let weekly = limits
            .iter()
            .find(|l| l.kind == crate::domain::LimitKind::WeeklyAll)
            .expect("reactivated account must get an overlay weekly limit");
        assert_eq!(
            weekly.source,
            Provenance::Authoritative,
            "the overlay must fetch the reactivated account within the same tick"
        );
        assert_eq!(
            weekly.severity,
            crate::domain::Severity::Crit,
            "the reloaded crit_pct=60 must make the 91% weekly critical"
        );
    }

    #[tokio::test]
    async fn reload_deactivating_an_account_stops_its_local_collection() {
        // Spec 015 §A / acceptance 1 (reverse): flipping an account active=true→false mid-run stops
        // its local collect on the reload tick, while an always-active sibling keeps collecting. The
        // reload runs BEFORE the collect loop on the same tick, so the deactivated account is never
        // reached — deterministic regardless of select! ordering.
        let store_dir = tempfile::tempdir().expect("store dir");
        let path = store_dir.path().join("t.db");
        let store = Store::open(&path).expect("open store");

        let cfg_before = config(vec![account("a"), account("b")]);
        let mut a_off = account("a");
        a_off.active = false;
        let cfg_after = config(vec![a_off, account("b")]);

        let source = QueuedConfigSource::new(vec![cfg_after]);
        let endpoint = CannedEndpoint {
            canned: Canned::Fail,
        };

        run_collector(
            &cfg_before,
            source,
            FakeAdapter,
            endpoint,
            idle_rate_source(),
            idle_zai_endpoint(),
            store,
            async {
                tokio::time::sleep(Duration::from_millis(300)).await;
            },
        )
        .await
        .expect("loop runs clean");

        let store = Store::open(&path).expect("reopen");
        assert!(
            store.latest_snapshot("a").expect("query").is_none(),
            "a deactivated account must not be collected after the reload"
        );
        assert!(
            store.latest_snapshot("b").expect("query").is_some(),
            "the still-active sibling must keep collecting"
        );
    }

    /// A config with explicit cadences — the cadence-recreation test needs to start slow and reload
    /// fast (the fixed `config` helper hardcodes 5s/300s).
    fn config_with_cadence(
        accounts: Vec<Account>,
        poll_local_secs: u64,
        poll_overlay_secs: u64,
    ) -> Config {
        Config {
            settings: Settings {
                poll_local_secs,
                poll_overlay_secs,
                warn_pct: 75.0,
                crit_pct: 90.0,
                ccusage_cmd: None,
                ledger_path: None,
            },
            accounts,
        }
    }

    /// A [`ProviderAdapter`] that counts its `collect` calls, so a test can prove how often the LOCAL
    /// cadence fired. Same snapshot shape as [`FakeAdapter`].
    struct CountingAdapter {
        collects: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ProviderAdapter for CountingAdapter {
        async fn collect(
            &self,
            account: &Account,
            now: Timestamp,
        ) -> AppResult<Option<UsageSnapshot>> {
            self.collects.fetch_add(1, Ordering::SeqCst);
            Ok(Some(UsageSnapshot {
                account_id: account.id.clone(),
                provider: Provider::Claude,
                collected_at: now,
                input: 1,
                output: 1,
                cache_read: 1,
                cache_creation: 1,
                total_tokens: 4,
                cost_notional: Some(0.1),
                window: None,
            }))
        }
    }

    /// A [`UsageEndpoint`] that counts its `fetch` calls, so a test can prove how often the OVERLAY
    /// cadence fired. Returns a valid session-limit body so each harvest stays on the success path.
    struct CountingEndpoint {
        fetches: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl UsageEndpoint for CountingEndpoint {
        async fn fetch(&self, _access_token: &str) -> AppResult<Vec<u8>> {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            Ok(
                br#"{"five_hour":{"utilization":19.0,"resets_at":"2026-07-04T12:00:00Z"}}"#
                    .to_vec(),
            )
        }
    }

    #[tokio::test(start_paused = true)]
    async fn reload_to_shorter_cadence_recreates_local_and_overlay_intervals() {
        // Spec 015 acceptance 2: a reload that shortens the local AND overlay cadences must RECREATE
        // both intervals so collection speeds up at runtime (not just on the next restart). On a
        // paused clock we start at a cadence far longer than the simulated run — so WITHOUT the
        // recreation only the immediate t=0 tick fires — reload to 5s on the first tick, then assert
        // many more collects + overlay fetches land across the 60s run.
        let collects = Arc::new(AtomicUsize::new(0));
        let fetches = Arc::new(AtomicUsize::new(0));

        let creds_dir = tempfile::tempdir().expect("creds dir");
        let acct = opted_in_account("acct", creds_dir.path()); // warm token ⇒ the overlay pass fetches
        let cfg_before = config_with_cadence(vec![acct.clone()], 1000, 1000);
        let cfg_after = config_with_cadence(vec![acct], 5, 5);
        let source = QueuedConfigSource::new(vec![cfg_after]);

        let store_dir = tempfile::tempdir().expect("store dir");
        let store = Store::open(&store_dir.path().join("t.db")).expect("open");

        run_collector(
            &cfg_before,
            source,
            CountingAdapter {
                collects: Arc::clone(&collects),
            },
            CountingEndpoint {
                fetches: Arc::clone(&fetches),
            },
            idle_rate_source(),
            idle_zai_endpoint(),
            store,
            async {
                tokio::time::sleep(Duration::from_mins(1)).await;
            },
        )
        .await
        .expect("loop runs clean");

        // 60 simulated seconds at the reloaded 5s cadence ⇒ ~12 ticks each; without the interval
        // recreation the 1000s cadence fires only once (t=0). A comfortable threshold splits them.
        let n_collects = collects.load(Ordering::SeqCst);
        let n_fetches = fetches.load(Ordering::SeqCst);
        assert!(
            n_collects >= 5,
            "local interval must be recreated on the shorter cadence, got {n_collects}"
        );
        assert!(
            n_fetches >= 5,
            "overlay interval must be recreated on the shorter cadence, got {n_fetches}"
        );
    }

    #[test]
    fn file_config_source_keeps_last_good_and_warns_once_per_bad_content() {
        // Spec 015 §A / acceptance 3: an unparseable reload keeps last-good (poll → None) and warns
        // exactly once per distinct bad CONTENT; a later good edit reloads. No collector loop needed —
        // this is the production source's contract.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tokenomics.toml");
        let good = "\
[[account]]
id = \"a\"
label = \"A\"
provider = \"claude\"
config_dir = \"/tmp\"
";
        std::fs::write(&path, good).expect("write good");

        let mut src = FileConfigSource::seed(path.clone());
        assert!(src.poll().is_none(), "no change yet → no reload");

        // A bad (unparseable) edit: kept last-good, warned once.
        std::fs::write(&path, "this = = not toml\n").expect("write bad");
        assert!(src.poll().is_none(), "a bad reload keeps last-good");
        assert_eq!(src.warns, 1);
        // Same bad content on the next poll → hash unchanged → no re-parse, no second warning.
        assert!(src.poll().is_none());
        assert_eq!(src.warns, 1, "one warning per distinct bad content");

        // A subsequent good edit reloads cleanly.
        std::fs::write(
            &path,
            "[[account]]\nid = \"b\"\nlabel = \"B\"\nprovider = \"claude\"\nconfig_dir = \"/tmp\"\n",
        )
        .expect("write good2");
        let reloaded = src.poll().expect("a good edit reloads");
        assert_eq!(reloaded.accounts[0].id, "b");
        assert_eq!(src.warns, 1, "a successful reload must not warn");
    }

    #[test]
    fn file_config_source_triggers_on_content_change_with_unchanged_mtime_and_size() {
        // Spec 015 §A / GAP1: an mtime-preserving, same-size edit (`cp -p` / `rsync --times`) must
        // still trigger a reload. A stat-only (mtime, size) watch would go blind here; content
        // hashing catches it. The two ids are single chars, so the files are byte-for-byte the same
        // length — only the content differs — and we force the mtime back to prove stat can't help.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tokenomics.toml");
        let cfg = |id: &str| {
            format!(
                "[[account]]\nid = \"{id}\"\nlabel = \"X\"\nprovider = \"claude\"\nconfig_dir = \"/tmp\"\n"
            )
        };
        std::fs::write(&path, cfg("a")).expect("write a");
        let mtime = std::fs::metadata(&path)
            .expect("meta")
            .modified()
            .expect("mtime");

        let mut src = FileConfigSource::seed(path.clone());
        assert!(src.poll().is_none(), "no change yet → no reload");

        // Rewrite with the same size, then force the SAME mtime back — only the bytes differ.
        std::fs::write(&path, cfg("b")).expect("write b");
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("reopen");
        f.set_modified(mtime).expect("restore mtime");
        drop(f);
        assert_eq!(
            std::fs::metadata(&path)
                .expect("meta")
                .modified()
                .expect("mtime"),
            mtime,
            "test precondition: mtime restored so only content differs"
        );

        let reloaded = src
            .poll()
            .expect("a content change must trigger despite identical mtime+size");
        assert_eq!(reloaded.accounts[0].id, "b");
    }

    #[test]
    fn overlay_harvest_after_deactivating_reload_writes_nothing_for_the_dropped_account() {
        // Spec 015 §A / GAP4: an overlay result that lands after a reload deactivated its account is
        // dropped — no limits row, no overlay-success stamp. Never stamp a fresh success onto a
        // just-deactivated account.
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(&dir.path().join("t.db")).expect("open");
        store.upsert_accounts(&[account("acct")]).expect("upsert");
        let mut alerts = AlertTracker::new(Duration::from_secs(ALERT_COOLDOWN_SECS));
        let mut state = OverlayState::default();

        // The CURRENT config has the account inactive (a reload flipped it off mid-flight).
        let mut inactive = account("acct");
        inactive.active = false;
        let cfg = config(vec![inactive]);

        let outcome = OverlayOutcome {
            account: account("acct"), // the fetch was spawned while the account was still active
            backoff_current: 300,
            result: Ok(
                br#"{"five_hour":{"utilization":19.0,"resets_at":"2026-07-04T12:00:00Z"}}"#
                    .to_vec(),
            ),
        };
        apply_overlay_outcome(&cfg, &store, &mut state, &mut alerts, outcome);

        assert!(
            store.latest_limits("acct").expect("q").is_empty(),
            "a deactivated account must get no limits from an in-flight harvest"
        );
        assert!(
            store.last_overlay_success("acct").expect("q").is_none(),
            "a deactivated account must not be stamped a fresh overlay success"
        );
    }

    #[tokio::test]
    async fn collect_harvest_after_deactivating_reload_writes_no_snapshot_but_clears_inflight() {
        // Spec 015 §A / GAP4: a collect result that lands after a reload deactivated its account is
        // dropped (no snapshot), yet the inflight guard is still cleared so a later reactivation is
        // not permanently blocked.
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(&dir.path().join("t.db")).expect("open");
        store.upsert_accounts(&[account("acct")]).expect("upsert");
        let mut alerts = AlertTracker::new(Duration::from_secs(ALERT_COOLDOWN_SECS));
        let mut inflight: HashSet<String> = HashSet::new();
        inflight.insert("acct".to_string());
        let mut latest_gen: HashMap<String, u64> = HashMap::new();

        let mut inactive = account("acct");
        inactive.active = false;
        let cfg = config(vec![inactive]);

        let outcome = CollectOutcome {
            account: account("acct"),
            generation: 1,
            now: Timestamp::now(),
            result: FakeAdapter
                .collect(&account("acct"), Timestamp::now())
                .await,
        };
        apply_outcome(
            &cfg,
            &store,
            outcome,
            &mut inflight,
            &mut latest_gen,
            &mut alerts,
        );

        assert!(
            store.latest_snapshot("acct").expect("q").is_none(),
            "a deactivated account's in-flight collect must not be persisted"
        );
        assert!(
            !inflight.contains("acct"),
            "the inflight guard must still be cleared for the dropped account"
        );
    }
}
