//! The local SQLite store (bundled, WAL): the collector writes it, the TUI reads it.
//!
//! Project: Tokenomics — monitor LLM subscription accounts (usage, limits, time-left) in a TUI
//! Module:  src/store.rs
//! Deps:    rusqlite (bundled SQLite), jiff (timestamp <-> epoch-millis), domain
//! Tested:  inline `#[cfg(test)]` — tempfile round-trip + idempotent re-open
//!
//! Key responsibilities:
//! - `open`: create/migrate the schema (WAL, foreign keys, busy timeout), keyed by `user_version`.
//!   `open_readonly` is a migration-free read-only variant for `tok doctor` (spec 015 §B/GAP5).
//! - Writers: `upsert_accounts`, `insert_snapshot`, `set_limits`, `set_token_state`,
//!   `record_overlay_success`, `mark_overlay_failing`, `heartbeat`, `record_collector_stamp`.
//! - Readers: `latest_snapshot`, `latest_limits`, `burn_history`, `latest_token_status`,
//!   `last_overlay_success`, `overlay_failing_since`, `heartbeat_age` (collector liveness),
//!   `collector_stamp` (the config path/mtime + exe path/mtime the collector loaded — spec 015 §B
//!   divergence + rebuild checks).
//!
//! Design constraints:
//! - Timestamps persist as epoch-milliseconds (INTEGER, sortable); `resets_at` is stored verbatim.
//! - Never panics on a busy DB — `busy_timeout` is set and every failure is a typed `AppError`.
//! - `set_limits` replaces an account's limit set atomically (callers merge by provenance first).
//! - `token_state` stores freshness only — never a token value.

use std::path::Path;
use std::time::Duration;

use jiff::Timestamp;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};

use crate::domain::{
    Account, Limit, LimitKind, Provenance, Provider, Severity, UsageSnapshot, Window,
};
use crate::error::{AppError, AppResult};

/// Schema version 1. `IF NOT EXISTS` keeps creation idempotent alongside the `user_version` guard.
const SCHEMA_V1: &str = "\
CREATE TABLE IF NOT EXISTS accounts (
    id             TEXT PRIMARY KEY,
    label          TEXT NOT NULL,
    provider       TEXT NOT NULL,
    config_dir     TEXT NOT NULL,
    color          TEXT,
    limits_overlay INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS snapshots (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id         TEXT NOT NULL REFERENCES accounts(id),
    collected_at       INTEGER NOT NULL,
    input              INTEGER NOT NULL,
    output             INTEGER NOT NULL,
    cache_read         INTEGER NOT NULL,
    cache_creation     INTEGER NOT NULL,
    total_tokens       INTEGER NOT NULL,
    cost_notional      REAL,
    win_start          INTEGER,
    win_end            INTEGER,
    win_remaining_secs INTEGER,
    burn_tpm           REAL,
    burn_cph           REAL
);
CREATE INDEX IF NOT EXISTS idx_snapshots_account_time
    ON snapshots(account_id, collected_at);
CREATE TABLE IF NOT EXISTS limits (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id      TEXT NOT NULL REFERENCES accounts(id),
    kind            TEXT NOT NULL,
    scope           TEXT,
    utilization_pct REAL NOT NULL,
    resets_at       TEXT NOT NULL,
    severity        TEXT NOT NULL,
    source          TEXT NOT NULL,
    collected_at    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_limits_account_time
    ON limits(account_id, collected_at);
CREATE TABLE IF NOT EXISTS token_state (
    account_id      TEXT PRIMARY KEY REFERENCES accounts(id),
    expires_at      INTEGER,
    last_refresh_at INTEGER,
    status          TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS heartbeat (
    component  TEXT PRIMARY KEY,
    pid        INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);";

/// Schema version 2: per-account time of the last successful authoritative overlay fetch. Distinct
/// from `limits.collected_at` (which a local derived tick re-stamps every cadence) — this advances
/// only when `/api/oauth/usage` actually returned data, so the TUI can show "refreshed Nm ago" and
/// reveal that a stale-token account's numbers are frozen.
const SCHEMA_V2: &str = "\
CREATE TABLE IF NOT EXISTS overlay_state (
    account_id      TEXT PRIMARY KEY REFERENCES accounts(id),
    last_success_at INTEGER NOT NULL
);";

/// Schema version 4: a per-account "overlay has been failing since" marker. Set on the FIRST failed
/// overlay pass (429 or transport) after a success, kept across a run of failures, cleared on the
/// next success. A dead subscription 429s every pass forever, so its `since_at` ages past a stall
/// threshold; a live account's occasional 429 clears before then — the honest "check this account"
/// signal, separate from token freshness. Its own table so a failure-before-any-success needs no
/// (NOT NULL) `last_success_at` row.
const SCHEMA_V4: &str = "\
CREATE TABLE IF NOT EXISTS overlay_failure (
    account_id TEXT PRIMARY KEY REFERENCES accounts(id),
    since_at   INTEGER NOT NULL
);";

/// Schema version 3: a covering index leading with `collected_at` so the fleet aggregate (grouped by
/// tick, and the retention prune) is index-served rather than a full-table scan (spec 011 §E). The
/// pre-existing `idx_snapshots_account_time` leads with `account_id`, so it can't serve these.
const SCHEMA_V3: &str = "\
CREATE INDEX IF NOT EXISTS idx_snapshots_time ON snapshots(collected_at, burn_tpm);";

/// The nullable `heartbeat` stamp columns (spec 015 §B/§B2), each recording what the running collector
/// loaded: the config file's resolved path + mtime (epoch-ms) and the running executable's path +
/// mtime (epoch-ms). `ALTER … ADD COLUMN` is additive — existing rows keep their data and read NULL —
/// so `tok doctor` can flag a config the collector never reloaded (persistent NULL = a collector that
/// predates hot-reload) and a binary rebuilt after start. Doctor stats the *recorded* paths, never its
/// own re-resolution, so the two never compare different files. These are added by an idempotent
/// reconcile (see [`Store::ensure_heartbeat_stamp_columns`]) rather than a version-gated `ALTER`,
/// because an earlier build already stamped some stores at `user_version = 5` with ONLY `config_mtime`
/// — a version gate would never let those gain the other three columns.
const HEARTBEAT_STAMP_COLUMNS: &[(&str, &str)] = &[
    ("config_mtime", "INTEGER"),
    ("config_path", "TEXT"),
    ("exe_path", "TEXT"),
    ("exe_mtime", "INTEGER"),
];

/// The current schema version (the highest migration applied by [`Store::migrate`]).
const SCHEMA_VERSION: i64 = 5;

/// The local store — one SQLite connection. Writers and readers each hold their own `Store`;
/// WAL lets them coexist.
#[derive(Debug)]
pub struct Store {
    conn: Connection,
}

/// What the running collector recorded loading, read from the `heartbeat` row (spec 015 §B/§B2).
/// Every field is nullable: a pre-hot-reload collector wrote none of them, and doctor treats any
/// absent field as "no signal" (never guesses).
#[derive(Debug, Clone, Default)]
pub struct CollectorStamp {
    /// The resolved absolute path of the config the collector loaded.
    pub config_path: Option<String>,
    /// That config file's mtime (epoch-ms) at load time.
    pub config_mtime: Option<i64>,
    /// The running executable's path (`std::env::current_exe()`).
    pub exe_path: Option<String>,
    /// That executable's mtime (epoch-ms) at collector-start time.
    pub exe_mtime: Option<i64>,
}

impl Store {
    /// Open (creating if absent) and migrate the store at `path`.
    pub fn open(path: &Path) -> AppResult<Self> {
        let conn = Connection::open(path)?;
        // synchronous=NORMAL is the recommended durability/throughput point for a WAL store: safe
        // against app crashes, only a power-loss edge can lose the last commit — fine for a monitor.
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000; \
             PRAGMA synchronous=NORMAL;",
        )?;
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    /// Open the store READ-ONLY, skipping migration entirely (spec 015 §B/GAP5). `tok doctor` uses
    /// this so a diagnostic run never migrates (or creates) the collector's store, and an older-schema
    /// store is read as-is — a reader for a column a pre-v5 store lacks degrades to "no data" rather
    /// than erroring the whole run (see [`Store::collector_stamp`]).
    pub fn open_readonly(path: &Path) -> AppResult<Self> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        // A read connection sets no journal mode (that would write); only the busy timeout, so a
        // concurrent collector write doesn't fail the read with SQLITE_BUSY.
        conn.busy_timeout(Duration::from_secs(5))?;
        Ok(Self { conn })
    }

    /// Apply pending migrations, keyed by `PRAGMA user_version` (idempotent; each step is additive
    /// and `IF NOT EXISTS`, so an existing v1 store gains the v2 table without data loss).
    fn migrate(&self) -> AppResult<()> {
        let version: i64 = self
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version < 1 {
            self.conn.execute_batch(SCHEMA_V1)?;
        }
        if version < 2 {
            self.conn.execute_batch(SCHEMA_V2)?;
        }
        if version < 3 {
            self.conn.execute_batch(SCHEMA_V3)?;
        }
        if version < 4 {
            self.conn.execute_batch(SCHEMA_V4)?;
        }
        // v5 stamp columns are reconciled unconditionally (not version-gated) so a store an earlier
        // build already stamped at v5 with only `config_mtime` still gains the other three columns.
        self.ensure_heartbeat_stamp_columns()?;
        if version < SCHEMA_VERSION {
            self.conn
                .pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        Ok(())
    }

    /// Add any missing [`HEARTBEAT_STAMP_COLUMNS`] to the `heartbeat` table (idempotent). A one-time
    /// `table_info` read + at most four `ALTER`s on a store missing them; a no-op once complete. The
    /// column name + type are compile-time literals, never user input — safe to interpolate.
    fn ensure_heartbeat_stamp_columns(&self) -> AppResult<()> {
        let mut present: Vec<String> = Vec::new();
        {
            let mut stmt = self.conn.prepare("PRAGMA table_info(heartbeat)")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
            for row in rows {
                present.push(row?);
            }
        }
        for (name, decl) in HEARTBEAT_STAMP_COLUMNS {
            if !present.iter().any(|c| c == name) {
                self.conn
                    .execute_batch(&format!("ALTER TABLE heartbeat ADD COLUMN {name} {decl};"))?;
            }
        }
        Ok(())
    }

    /// Reconcile the `accounts` table from the configured account list.
    pub fn upsert_accounts(&self, accounts: &[Account]) -> AppResult<()> {
        for account in accounts {
            self.conn.execute(
                "INSERT INTO accounts (id, label, provider, config_dir, color, limits_overlay)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(id) DO UPDATE SET
                     label=excluded.label, provider=excluded.provider,
                     config_dir=excluded.config_dir, color=excluded.color,
                     limits_overlay=excluded.limits_overlay",
                params![
                    account.id,
                    account.label,
                    account.provider.as_str(),
                    // No config_dir (zai, spec 019 §A) persists as an empty string — this column is
                    // write-only diagnostics text, never decoded back into an `Account`.
                    account
                        .config_dir
                        .as_deref()
                        .map_or_else(String::new, |d| d.display().to_string()),
                    account.color,
                    account.limits_overlay,
                ],
            )?;
        }
        Ok(())
    }

    /// Append one usage snapshot.
    pub fn insert_snapshot(&self, snapshot: &UsageSnapshot) -> AppResult<()> {
        let window = snapshot.window.as_ref();
        self.conn.execute(
            "INSERT INTO snapshots
                (account_id, collected_at, input, output, cache_read, cache_creation,
                 total_tokens, cost_notional, win_start, win_end, win_remaining_secs,
                 burn_tpm, burn_cph)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                snapshot.account_id,
                snapshot.collected_at.as_millisecond(),
                snapshot.input,
                snapshot.output,
                snapshot.cache_read,
                snapshot.cache_creation,
                snapshot.total_tokens,
                snapshot.cost_notional,
                window.map(|w| w.start.as_millisecond()),
                window.map(|w| w.end.as_millisecond()),
                window.and_then(|w| w.remaining_minutes).map(|m| m * 60),
                window.map(|w| w.tokens_per_minute),
                window.map(|w| w.cost_per_hour),
            ],
        )?;
        Ok(())
    }

    /// Replace an account's limits with `limits` (atomic delete + insert). Callers merge by
    /// provenance first (`format::merge_limits`) so an authoritative row is never clobbered by a
    /// derived one; this keeps exactly one current row per `(kind, scope)`.
    pub fn set_limits(
        &self,
        account_id: &str,
        limits: &[Limit],
        collected_at: Timestamp,
    ) -> AppResult<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM limits WHERE account_id = ?1",
            params![account_id],
        )?;
        for limit in limits {
            tx.execute(
                "INSERT INTO limits
                    (account_id, kind, scope, utilization_pct, resets_at, severity, source, collected_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    limit.account_id,
                    limit.kind.as_str(),
                    limit.scope,
                    limit.utilization_pct,
                    limit.resets_at,
                    limit.severity.as_str(),
                    limit.source.as_str(),
                    collected_at.as_millisecond(),
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Record an account's token freshness (upsert). No token value is stored — only its state.
    pub fn set_token_state(
        &self,
        account_id: &str,
        status: TokenStatus,
        expires_at_ms: Option<i64>,
        last_refresh_at_ms: Option<i64>,
    ) -> AppResult<()> {
        self.conn.execute(
            "INSERT INTO token_state (account_id, expires_at, last_refresh_at, status)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(account_id) DO UPDATE SET
                 expires_at=excluded.expires_at, last_refresh_at=excluded.last_refresh_at,
                 status=excluded.status",
            params![
                account_id,
                expires_at_ms,
                last_refresh_at_ms,
                status.as_str()
            ],
        )?;
        Ok(())
    }

    /// Record that an authoritative overlay fetch succeeded for `account_id` at `at_ms` (epoch
    /// millis). Only the overlay's success path calls this — never a derived tick — so the value is
    /// a true "last authoritative refresh" time.
    pub fn record_overlay_success(&self, account_id: &str, at_ms: i64) -> AppResult<()> {
        self.conn.execute(
            "INSERT INTO overlay_state (account_id, last_success_at) VALUES (?1, ?2)
             ON CONFLICT(account_id) DO UPDATE SET last_success_at=excluded.last_success_at",
            params![account_id, at_ms],
        )?;
        // A success ends any failing streak (the account is healthy again).
        self.conn.execute(
            "DELETE FROM overlay_failure WHERE account_id = ?1",
            params![account_id],
        )?;
        Ok(())
    }

    /// Mark that this account's overlay pass failed at `at_ms` (429 or transport). `since_at` records
    /// the FIRST failure of the current streak — `DO NOTHING` keeps it pinned across a run of failures
    /// so the TUI can tell a sustained outage (a dead subscription) from a one-off 429.
    pub fn mark_overlay_failing(&self, account_id: &str, at_ms: i64) -> AppResult<()> {
        self.conn.execute(
            "INSERT INTO overlay_failure (account_id, since_at) VALUES (?1, ?2)
             ON CONFLICT(account_id) DO NOTHING",
            params![account_id, at_ms],
        )?;
        Ok(())
    }

    /// Epoch-millis this account's overlay has been failing continuously since, if it is failing now.
    pub fn overlay_failing_since(&self, account_id: &str) -> AppResult<Option<i64>> {
        let ms: Option<i64> = self
            .conn
            .query_row(
                "SELECT since_at FROM overlay_failure WHERE account_id = ?1",
                params![account_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(ms)
    }

    /// The epoch-millis of the last successful overlay fetch for `account_id`, if one is recorded.
    pub fn last_overlay_success(&self, account_id: &str) -> AppResult<Option<i64>> {
        let ms: Option<i64> = self
            .conn
            .query_row(
                "SELECT last_success_at FROM overlay_state WHERE account_id = ?1",
                params![account_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(ms)
    }

    /// The recorded token status for an account, if any.
    pub fn latest_token_status(&self, account_id: &str) -> AppResult<Option<TokenStatus>> {
        let text: Option<String> = self
            .conn
            .query_row(
                "SELECT status FROM token_state WHERE account_id = ?1",
                params![account_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(text.as_deref().and_then(TokenStatus::from_str))
    }

    /// The age in ms of `component`'s heartbeat relative to `now_ms`, or `None` when no heartbeat is
    /// recorded (the writer never started). Lets the TUI detect a dead / stalled / never-started
    /// collector: a large or absent age means the on-screen data is frozen, not live (spec 011 §A).
    pub fn heartbeat_age(&self, component: &str, now_ms: i64) -> AppResult<Option<i64>> {
        let updated: Option<i64> = self
            .conn
            .query_row(
                "SELECT updated_at FROM heartbeat WHERE component = ?1",
                params![component],
                |row| row.get(0),
            )
            .optional()?;
        Ok(updated.map(|t| now_ms - t))
    }

    /// Record a component's liveness heartbeat (upsert by component name). The per-tick liveness beat:
    /// it touches only `pid`/`updated_at` and deliberately names none of the stamp columns, so a
    /// startup/reload stamp (see [`Store::record_collector_stamp`]) is never clobbered (spec 015 §B).
    pub fn heartbeat(&self, component: &str, pid: u32) -> AppResult<()> {
        self.conn.execute(
            "INSERT INTO heartbeat (component, pid, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(component) DO UPDATE SET pid=excluded.pid, updated_at=excluded.updated_at",
            params![component, pid, Timestamp::now().as_millisecond()],
        )?;
        Ok(())
    }

    /// Stamp what the collector just loaded — the config file's resolved path + mtime and the running
    /// executable's path + mtime — alongside a liveness beat. Written at startup and after every
    /// successful hot-reload — NOT per tick — so these track the loaded config/binary, letting
    /// `tok doctor` flag a file the collector never reloaded (spec 015 §B) or a binary rebuilt after
    /// start (§B2). Each `None` argument stores NULL (unknown path/mtime).
    pub fn record_collector_stamp(
        &self,
        component: &str,
        pid: u32,
        config_path: Option<&str>,
        config_mtime_ms: Option<i64>,
        exe_path: Option<&str>,
        exe_mtime_ms: Option<i64>,
    ) -> AppResult<()> {
        self.conn.execute(
            "INSERT INTO heartbeat
                 (component, pid, updated_at, config_path, config_mtime, exe_path, exe_mtime)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(component) DO UPDATE SET
                 pid=excluded.pid, updated_at=excluded.updated_at,
                 config_path=excluded.config_path, config_mtime=excluded.config_mtime,
                 exe_path=excluded.exe_path, exe_mtime=excluded.exe_mtime",
            params![
                component,
                pid,
                Timestamp::now().as_millisecond(),
                config_path,
                config_mtime_ms,
                exe_path,
                exe_mtime_ms
            ],
        )?;
        Ok(())
    }

    /// What the collector recorded loading (config path/mtime + exe path/mtime), or `None` when the
    /// heartbeat row is absent (never started). A pre-hot-reload row reads all-NULL fields; a store
    /// that predates the v5 columns (read via [`Store::open_readonly`], which skips migration) errors
    /// the SELECT with "no such column". This diagnostic reader can't meaningfully fail, so it returns
    /// a bare `Option`: any read error (including a missing column) degrades to `None` so doctor stays
    /// silent rather than aborting the whole run (spec 015 §B/GAP5).
    pub fn collector_stamp(&self, component: &str) -> Option<CollectorStamp> {
        self.conn
            .query_row(
                "SELECT config_path, config_mtime, exe_path, exe_mtime
                 FROM heartbeat WHERE component = ?1",
                params![component],
                |row| {
                    Ok(CollectorStamp {
                        config_path: row.get(0)?,
                        config_mtime: row.get(1)?,
                        exe_path: row.get(2)?,
                        exe_mtime: row.get(3)?,
                    })
                },
            )
            .optional()
            .ok()
            .flatten()
    }

    /// The most recent snapshot for an account (last-good), if any.
    pub fn latest_snapshot(&self, account_id: &str) -> AppResult<Option<UsageSnapshot>> {
        self.conn
            .query_row(
                "SELECT a.provider, s.collected_at, s.input, s.output, s.cache_read,
                        s.cache_creation, s.total_tokens, s.cost_notional, s.win_start, s.win_end,
                        s.win_remaining_secs, s.burn_tpm, s.burn_cph
                 FROM snapshots s JOIN accounts a ON a.id = s.account_id
                 WHERE s.account_id = ?1
                 ORDER BY s.collected_at DESC, s.id DESC LIMIT 1",
                params![account_id],
                |row| row_to_snapshot(account_id, row),
            )
            .optional()?
            .transpose()
    }

    /// The most recent batch of limits for an account (all rows at its latest `collected_at`).
    pub fn latest_limits(&self, account_id: &str) -> AppResult<Vec<Limit>> {
        // No accounts row yet (configured but never collected) → no limits can exist. Return empty
        // rather than erroring, so a fresh account degrades to "n/a" instead of a store read error.
        let Some(provider) = self.account_provider(account_id)? else {
            return Ok(Vec::new());
        };
        let mut stmt = self.conn.prepare(
            "SELECT kind, scope, utilization_pct, resets_at, severity, source
             FROM limits
             WHERE account_id = ?1
               AND collected_at = (SELECT MAX(collected_at) FROM limits WHERE account_id = ?1)
             ORDER BY id",
        )?;
        let rows = stmt.query_map(params![account_id], |row| {
            row_to_limit(account_id, provider, row)
        })?;
        let mut limits = Vec::new();
        for row in rows {
            limits.push(row??);
        }
        Ok(limits)
    }

    /// The last `n` `total_tokens` points for an account, oldest → newest (for the sparkline).
    pub fn burn_history(&self, account_id: &str, n: usize) -> AppResult<Vec<u64>> {
        let mut stmt = self.conn.prepare(
            "SELECT total_tokens FROM snapshots
             WHERE account_id = ?1 ORDER BY collected_at DESC, id DESC LIMIT ?2",
        )?;
        let limit = i64::try_from(n).unwrap_or(i64::MAX);
        let rows = stmt.query_map(params![account_id, limit], |row| row.get::<_, u64>(0))?;
        let mut points = Vec::new();
        for row in rows {
            points.push(row?);
        }
        points.reverse();
        Ok(points)
    }

    /// The last `n` collection ticks of fleet-wide burn rate — `Σ burn_tpm` across all accounts at
    /// each `collected_at`, oldest → newest — for the header aggregate sparkline. Accounts are
    /// collected on one cadence so they share exact `collected_at` values; `NULL` rates count as 0.
    /// The `Σ` and INTEGER cast happen in SQL, so no float→int cast leaks into Rust.
    pub fn aggregate_burn_history(&self, n: usize) -> AppResult<Vec<u64>> {
        // Bound the GROUP BY to rows in the last `n` distinct ticks (via the covering index on
        // `collected_at`), not the whole ever-growing table — so the per-tick TUI read stays O(n),
        // not O(total rows), and refresh does not slow as the store grows (spec 011 §E).
        let mut stmt = self.conn.prepare(
            "SELECT CAST(SUM(COALESCE(burn_tpm, 0.0)) AS INTEGER) AS tpm
             FROM snapshots
             WHERE collected_at >= (
                 SELECT MIN(ct) FROM (
                     SELECT DISTINCT collected_at AS ct FROM snapshots ORDER BY ct DESC LIMIT ?1
                 )
             )
             GROUP BY collected_at
             ORDER BY collected_at DESC",
        )?;
        let limit = i64::try_from(n).unwrap_or(i64::MAX);
        let rows = stmt.query_map(params![limit], |row| row.get::<_, i64>(0))?;
        let mut points = Vec::new();
        for row in rows {
            points.push(u64::try_from(row?).unwrap_or(0));
        }
        points.reverse();
        Ok(points)
    }

    /// Delete snapshots older than `cutoff_ms` (epoch millis), returning the number removed. Keeps the
    /// table (and thus the aggregate scan + disk) bounded on a 24/7 run. Writer-only (the collector).
    pub fn prune_snapshots(&self, cutoff_ms: i64) -> AppResult<usize> {
        let removed = self.conn.execute(
            "DELETE FROM snapshots WHERE collected_at < ?1",
            params![cutoff_ms],
        )?;
        Ok(removed)
    }

    /// Checkpoint the WAL and truncate it back to empty, reclaiming the `-wal` file's space. Called
    /// after a prune so a long-lived writer's WAL cannot grow without bound. Best-effort by contract:
    /// a checkpoint blocked by a reader is not an error (it simply retries next sweep).
    pub fn checkpoint_truncate(&self) -> AppResult<()> {
        self.conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(())
    }

    /// The provider recorded for an account (used when decoding stored limits).
    fn account_provider(&self, account_id: &str) -> AppResult<Option<Provider>> {
        let text: Option<String> = self
            .conn
            .query_row(
                "SELECT provider FROM accounts WHERE id = ?1",
                params![account_id],
                |row| row.get(0),
            )
            .optional()?;
        text.map(|text| {
            Provider::parse(&text)
                .ok_or_else(|| AppError::StoreData(format!("unknown provider '{text}' in store")))
        })
        .transpose()
    }
}

/// Decode a snapshots row into a `UsageSnapshot` (inner result carries a data-decode failure).
fn row_to_snapshot(
    account_id: &str,
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<AppResult<UsageSnapshot>> {
    let provider_text: String = row.get(0)?;
    let collected_ms: i64 = row.get(1)?;
    let input: u64 = row.get(2)?;
    let output: u64 = row.get(3)?;
    let cache_read: u64 = row.get(4)?;
    let cache_creation: u64 = row.get(5)?;
    let total_tokens: u64 = row.get(6)?;
    let cost_notional: Option<f64> = row.get(7)?;
    let win_start: Option<i64> = row.get(8)?;
    let win_end: Option<i64> = row.get(9)?;
    let win_remaining_secs: Option<i64> = row.get(10)?;
    let burn_tpm: Option<f64> = row.get(11)?;
    let burn_cph: Option<f64> = row.get(12)?;

    Ok((|| {
        let provider = Provider::parse(&provider_text)
            .ok_or_else(|| AppError::StoreData(format!("unknown provider '{provider_text}'")))?;
        let collected_at = millis_to_ts(collected_ms)?;
        let window = match (win_start, win_end) {
            (Some(start), Some(end)) => Some(Window {
                start: millis_to_ts(start)?,
                end: millis_to_ts(end)?,
                remaining_minutes: win_remaining_secs.map(|s| s / 60),
                tokens_per_minute: burn_tpm.unwrap_or(0.0),
                cost_per_hour: burn_cph.unwrap_or(0.0),
            }),
            _ => None,
        };
        Ok(UsageSnapshot {
            account_id: account_id.to_string(),
            provider,
            collected_at,
            input,
            output,
            cache_read,
            cache_creation,
            total_tokens,
            cost_notional,
            window,
        })
    })())
}

/// Decode a limits row into a `Limit` (inner result carries a data-decode failure).
fn row_to_limit(
    account_id: &str,
    provider: Provider,
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<AppResult<Limit>> {
    let kind_text: String = row.get(0)?;
    let scope: Option<String> = row.get(1)?;
    let utilization_pct: f64 = row.get(2)?;
    let resets_at: String = row.get(3)?;
    let severity_text: String = row.get(4)?;
    let source_text: String = row.get(5)?;

    Ok((|| {
        Ok(Limit {
            account_id: account_id.to_string(),
            provider,
            kind: kind_from_str(&kind_text)?,
            scope,
            utilization_pct,
            resets_at,
            severity: severity_from_str(&severity_text)?,
            source: provenance_from_str(&source_text)?,
        })
    })())
}

/// An account's token freshness, as recorded in `token_state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenStatus {
    /// The stored token is valid (safe to use passively).
    Warm,
    /// The token is expired/absent — the overlay is skipped; the user should open Claude.
    Stale,
}

impl TokenStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Warm => "warm",
            Self::Stale => "stale",
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        match s {
            "warm" => Some(Self::Warm),
            "stale" => Some(Self::Stale),
            _ => None,
        }
    }
}

fn millis_to_ts(millis: i64) -> AppResult<Timestamp> {
    Timestamp::from_millisecond(millis)
        .map_err(|e| AppError::StoreData(format!("bad timestamp {millis}: {e}")))
}

fn kind_from_str(s: &str) -> AppResult<LimitKind> {
    LimitKind::parse(s).ok_or_else(|| AppError::StoreData(format!("unknown limit kind '{s}'")))
}

fn severity_from_str(s: &str) -> AppResult<Severity> {
    Severity::parse(s).ok_or_else(|| AppError::StoreData(format!("unknown severity '{s}'")))
}

fn provenance_from_str(s: &str) -> AppResult<Provenance> {
    Provenance::parse(s).ok_or_else(|| AppError::StoreData(format!("unknown provenance '{s}'")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn ts(s: &str) -> Timestamp {
        s.parse().expect("valid timestamp")
    }

    fn account() -> Account {
        Account {
            id: "acct".to_string(),
            label: "Acct".to_string(),
            provider: Provider::Claude,
            config_dir: Some(PathBuf::from("/home/x/.claude")),
            api_key_env: None,
            color: Some("cyan".to_string()),
            active: true,
            limits_overlay: false,
        }
    }

    fn snapshot(total: u64, collected: &str) -> UsageSnapshot {
        UsageSnapshot {
            account_id: "acct".to_string(),
            provider: Provider::Claude,
            collected_at: ts(collected),
            input: 10,
            output: 20,
            cache_read: 30,
            cache_creation: 40,
            total_tokens: total,
            cost_notional: Some(1.5),
            window: Some(Window {
                start: ts("2026-07-04T07:00:00Z"),
                end: ts("2026-07-04T12:00:00Z"),
                remaining_minutes: Some(90),
                tokens_per_minute: 1234.5,
                cost_per_hour: 6.7,
            }),
        }
    }

    fn open_temp() -> (TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(&dir.path().join("t.db")).expect("open");
        (dir, store)
    }

    #[test]
    fn aggregate_burn_history_sums_burn_per_tick_oldest_first() {
        let (_dir, store) = open_temp();
        let (mut a, mut b) = (account(), account());
        a.id = "a".to_string();
        b.id = "b".to_string();
        store.upsert_accounts(&[a, b]).expect("upsert");

        let snap = |id: &str, tpm: f64, collected: &str| UsageSnapshot {
            account_id: id.to_string(),
            provider: Provider::Claude,
            collected_at: ts(collected),
            input: 0,
            output: 0,
            cache_read: 0,
            cache_creation: 0,
            total_tokens: 0,
            cost_notional: None,
            window: Some(Window {
                start: ts("2026-07-04T07:00:00Z"),
                end: ts("2026-07-04T12:00:00Z"),
                remaining_minutes: None,
                tokens_per_minute: tpm,
                cost_per_hour: 0.0,
            }),
        };
        // Two accounts, two shared ticks — the aggregate sums both accounts at each tick.
        store
            .insert_snapshot(&snap("a", 100.0, "2026-07-04T10:00:00Z"))
            .expect("a1");
        store
            .insert_snapshot(&snap("b", 200.0, "2026-07-04T10:00:00Z"))
            .expect("b1");
        store
            .insert_snapshot(&snap("a", 300.0, "2026-07-04T10:05:00Z"))
            .expect("a2");
        store
            .insert_snapshot(&snap("b", 400.0, "2026-07-04T10:05:00Z"))
            .expect("b2");

        // Oldest → newest: (100+200), (300+400).
        assert_eq!(
            store.aggregate_burn_history(10).expect("agg"),
            vec![300, 700]
        );
        // The `n` cap keeps the most-recent ticks.
        assert_eq!(store.aggregate_burn_history(1).expect("agg"), vec![700]);
        // Empty store → empty series.
        let (_d2, empty) = open_temp();
        assert!(empty.aggregate_burn_history(10).expect("agg").is_empty());
    }

    #[test]
    fn prune_snapshots_deletes_old_keeps_recent_and_bounds_aggregate() {
        let (_dir, store) = open_temp();
        store.upsert_accounts(&[account()]).expect("upsert");
        for (total, at) in [
            (10, "2026-07-01T10:00:00Z"),
            (20, "2026-07-03T10:00:00Z"),
            (30, "2026-07-04T10:00:00Z"),
        ] {
            store.insert_snapshot(&snapshot(total, at)).expect("snap");
        }
        // Cut everything before 2026-07-02 (keeps the 07-03 and 07-04 snapshots).
        let cutoff = ts("2026-07-02T00:00:00Z").as_millisecond();
        assert_eq!(store.prune_snapshots(cutoff).expect("prune"), 1);
        let history = store.burn_history("acct", 32).expect("history");
        assert_eq!(history, vec![20, 30], "only post-cutoff snapshots survive");
        // Checkpoint after prune is a no-op-safe reclaim.
        store.checkpoint_truncate().expect("checkpoint");
        // Aggregate still reads the (now bounded) table correctly.
        assert!(!store.aggregate_burn_history(10).expect("agg").is_empty());
    }

    #[test]
    fn round_trips_snapshot_and_limits() {
        let (_dir, store) = open_temp();
        store.upsert_accounts(&[account()]).expect("upsert");
        store
            .insert_snapshot(&snapshot(100, "2026-07-04T10:00:00Z"))
            .expect("snap");

        let limit = Limit {
            account_id: "acct".to_string(),
            provider: Provider::Claude,
            kind: LimitKind::Session,
            scope: None,
            utilization_pct: 60.0,
            resets_at: "2026-07-04T12:00:00Z".to_string(),
            severity: Severity::Ok,
            source: Provenance::Derived,
        };
        store
            .set_limits("acct", &[limit], ts("2026-07-04T10:00:00Z"))
            .expect("limits");

        let got = store.latest_snapshot("acct").expect("query").expect("some");
        assert_eq!(got.total_tokens, 100);
        assert_eq!(got.provider, Provider::Claude);
        let window = got.window.expect("window");
        assert_eq!(window.remaining_minutes, Some(90));

        let limits = store.latest_limits("acct").expect("limits query");
        assert_eq!(limits.len(), 1);
        assert_eq!(limits[0].kind, LimitKind::Session);
        assert_eq!(limits[0].source, Provenance::Derived);
        assert_eq!(limits[0].resets_at, "2026-07-04T12:00:00Z");
    }

    #[test]
    fn latest_snapshot_picks_the_newest() {
        let (_dir, store) = open_temp();
        store.upsert_accounts(&[account()]).expect("upsert");
        store
            .insert_snapshot(&snapshot(100, "2026-07-04T10:00:00Z"))
            .expect("s1");
        store
            .insert_snapshot(&snapshot(200, "2026-07-04T10:05:00Z"))
            .expect("s2");
        store
            .insert_snapshot(&snapshot(150, "2026-07-04T10:03:00Z"))
            .expect("s3");

        let got = store.latest_snapshot("acct").expect("query").expect("some");
        assert_eq!(got.total_tokens, 200);

        let history = store.burn_history("acct", 32).expect("history");
        assert_eq!(history, vec![100, 150, 200]); // oldest → newest
    }

    #[test]
    fn latest_snapshot_absent_is_none() {
        let (_dir, store) = open_temp();
        store.upsert_accounts(&[account()]).expect("upsert");
        assert!(store.latest_snapshot("acct").expect("query").is_none());
        assert!(store.burn_history("acct", 8).expect("history").is_empty());
    }

    #[test]
    fn latest_limits_for_uncollected_account_is_empty_not_error() {
        // Account configured but never collected → no accounts row. Must degrade to empty, not the
        // "store read error: Query returned no rows" that blanked the row.
        let (_dir, store) = open_temp();
        assert!(store
            .latest_limits("never-collected")
            .expect("query")
            .is_empty());
    }

    #[test]
    fn open_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.db");
        {
            let store = Store::open(&path).expect("open 1");
            store.upsert_accounts(&[account()]).expect("upsert");
            store
                .insert_snapshot(&snapshot(100, "2026-07-04T10:00:00Z"))
                .expect("snap");
        }
        // Re-open: migration must not wipe or fail; data survives.
        let store = Store::open(&path).expect("open 2");
        let version: i64 = store
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .expect("user_version");
        assert_eq!(version, SCHEMA_VERSION);
        assert_eq!(
            store
                .latest_snapshot("acct")
                .expect("query")
                .expect("some")
                .total_tokens,
            100
        );
    }

    #[test]
    fn overlay_success_time_round_trips_and_is_absent_by_default() {
        let (_dir, store) = open_temp();
        store.upsert_accounts(&[account()]).expect("upsert");
        assert!(store.last_overlay_success("acct").expect("q").is_none());
        store
            .record_overlay_success("acct", 1_720_000_000_000)
            .expect("record");
        assert_eq!(
            store.last_overlay_success("acct").expect("q"),
            Some(1_720_000_000_000)
        );
        // Upsert: a newer success overwrites the prior time.
        store
            .record_overlay_success("acct", 1_720_000_060_000)
            .expect("record2");
        assert_eq!(
            store.last_overlay_success("acct").expect("q"),
            Some(1_720_000_060_000)
        );
    }

    #[test]
    fn overlay_failing_pins_first_time_and_clears_on_success() {
        let (_dir, store) = open_temp();
        store.upsert_accounts(&[account()]).expect("upsert");
        assert!(store.overlay_failing_since("acct").expect("q").is_none());

        // First failure sets the streak start; later failures keep it pinned (DO NOTHING).
        store.mark_overlay_failing("acct", 1_000).expect("fail1");
        store.mark_overlay_failing("acct", 9_999).expect("fail2");
        assert_eq!(store.overlay_failing_since("acct").expect("q"), Some(1_000));

        // A success ends the streak.
        store.record_overlay_success("acct", 10_000).expect("ok");
        assert!(store.overlay_failing_since("acct").expect("q").is_none());

        // A fresh failure after recovery starts a new streak.
        store.mark_overlay_failing("acct", 20_000).expect("fail3");
        assert_eq!(
            store.overlay_failing_since("acct").expect("q"),
            Some(20_000)
        );
    }

    #[test]
    fn token_state_upserts_and_reads() {
        let (_dir, store) = open_temp();
        store.upsert_accounts(&[account()]).expect("upsert");
        assert!(store.latest_token_status("acct").expect("q").is_none());
        store
            .set_token_state("acct", TokenStatus::Warm, Some(123), None)
            .expect("set warm");
        assert_eq!(
            store.latest_token_status("acct").expect("q"),
            Some(TokenStatus::Warm)
        );
        store
            .set_token_state("acct", TokenStatus::Stale, None, None)
            .expect("set stale");
        assert_eq!(
            store.latest_token_status("acct").expect("q"),
            Some(TokenStatus::Stale)
        );
    }

    #[test]
    fn set_limits_replaces_the_prior_set() {
        let (_dir, store) = open_temp();
        store.upsert_accounts(&[account()]).expect("upsert");
        let session = |pct, source| Limit {
            account_id: "acct".to_string(),
            provider: Provider::Claude,
            kind: LimitKind::Session,
            scope: None,
            utilization_pct: pct,
            resets_at: "2026-07-04T12:00:00Z".to_string(),
            severity: Severity::Ok,
            source,
        };
        store
            .set_limits(
                "acct",
                &[session(60.0, Provenance::Derived)],
                ts("2026-07-04T10:00:00Z"),
            )
            .expect("s1");
        store
            .set_limits(
                "acct",
                &[session(19.0, Provenance::Authoritative)],
                ts("2026-07-04T10:05:00Z"),
            )
            .expect("s2");
        let limits = store.latest_limits("acct").expect("q");
        assert_eq!(limits.len(), 1); // replaced, not appended
        assert_eq!(limits[0].source, Provenance::Authoritative);
    }

    #[test]
    fn heartbeat_age_is_none_until_written_then_small_and_grows_with_now() {
        let (_dir, store) = open_temp();
        let probe = || Timestamp::now().as_millisecond();
        assert!(store
            .heartbeat_age("collector", probe())
            .expect("q")
            .is_none());
        store.heartbeat("collector", 4321).expect("hb");
        let age = store
            .heartbeat_age("collector", probe())
            .expect("q")
            .expect("some");
        assert!(
            (0..60_000).contains(&age),
            "fresh age should be small: {age}"
        );
        // Age is monotone in `now`: a now an hour later yields ~an hour more age.
        let older = store
            .heartbeat_age("collector", probe() + 3_600_000)
            .expect("q")
            .expect("some");
        assert!(
            older >= age + 3_000_000,
            "age must grow with now: {older} vs {age}"
        );
    }

    #[test]
    fn collector_stamp_written_at_startup_and_not_clobbered_by_per_tick_heartbeat() {
        let (_dir, store) = open_temp();
        // Absent until stamped.
        assert!(store.collector_stamp("collector").is_none());
        // Startup stamp — config path/mtime + exe path/mtime.
        store
            .record_collector_stamp(
                "collector",
                1,
                Some("/cfg/tokenomics.toml"),
                Some(1_700_000_000_000),
                Some("/bin/tok"),
                Some(1_699_000_000_000),
            )
            .expect("stamp");
        let stamp = store.collector_stamp("collector").expect("some");
        assert_eq!(stamp.config_path.as_deref(), Some("/cfg/tokenomics.toml"));
        assert_eq!(stamp.config_mtime, Some(1_700_000_000_000));
        assert_eq!(stamp.exe_path.as_deref(), Some("/bin/tok"));
        assert_eq!(stamp.exe_mtime, Some(1_699_000_000_000));
        // A per-tick liveness beat must NOT clobber any stamped column.
        store.heartbeat("collector", 2).expect("beat");
        let after_beat = store.collector_stamp("collector").expect("some");
        assert_eq!(
            after_beat.config_mtime,
            Some(1_700_000_000_000),
            "per-tick heartbeat clobbered the stamp"
        );
        assert_eq!(after_beat.exe_path.as_deref(), Some("/bin/tok"));
        // A successful reload updates the config columns (exe re-stamped with the same start value).
        store
            .record_collector_stamp(
                "collector",
                2,
                Some("/cfg/tokenomics.toml"),
                Some(1_700_000_060_000),
                Some("/bin/tok"),
                Some(1_699_000_000_000),
            )
            .expect("reload stamp");
        assert_eq!(
            store
                .collector_stamp("collector")
                .expect("some")
                .config_mtime,
            Some(1_700_000_060_000)
        );
    }

    #[test]
    fn collector_stamp_is_none_for_a_beat_without_a_stamp() {
        let (_dir, store) = open_temp();
        // A collector that predates hot-reload writes only pid/updated_at → all stamp columns NULL.
        store.heartbeat("collector", 1).expect("beat");
        let stamp = store.collector_stamp("collector").expect("some");
        assert!(stamp.config_path.is_none());
        assert!(stamp.config_mtime.is_none());
        assert!(stamp.exe_path.is_none());
        assert!(stamp.exe_mtime.is_none());
    }

    #[test]
    fn collector_stamp_migration_preserves_existing_heartbeat_rows() {
        // A pre-v5 store: heartbeat table WITHOUT the stamp columns, one row, user_version = 4.
        // Opening it must ALTER in the columns without dropping the row (spec 015 §B — additive).
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.db");
        {
            let conn = Connection::open(&path).expect("raw open");
            conn.execute_batch(
                "CREATE TABLE heartbeat (
                     component TEXT PRIMARY KEY, pid INTEGER NOT NULL, updated_at INTEGER NOT NULL
                 );
                 INSERT INTO heartbeat (component, pid, updated_at) VALUES ('collector', 99, 12345);
                 PRAGMA user_version = 4;",
            )
            .expect("seed a v4 store");
        }
        let store = Store::open(&path).expect("open migrates to v5");
        assert_eq!(
            store.heartbeat_age("collector", 12_345 + 1_000).expect("q"),
            Some(1_000),
            "the pre-existing heartbeat row must survive the v5 migration"
        );
        let stamp = store.collector_stamp("collector").expect("some");
        assert!(
            stamp.config_mtime.is_none() && stamp.config_path.is_none() && stamp.exe_path.is_none(),
            "the migrated-in columns read NULL for the pre-existing row"
        );
    }

    #[test]
    fn open_heals_a_store_already_stamped_v5_with_only_config_mtime() {
        // An earlier build shipped a v5 that added ONLY `config_mtime` and stamped user_version = 5.
        // A version-gated migration would skip such a store forever; this build reconciles the missing
        // config_path/exe_path/exe_mtime columns on open, so `record_collector_stamp` writes cleanly.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.db");
        {
            let conn = Connection::open(&path).expect("raw open");
            conn.execute_batch(
                "CREATE TABLE heartbeat (
                     component TEXT PRIMARY KEY, pid INTEGER NOT NULL, updated_at INTEGER NOT NULL,
                     config_mtime INTEGER
                 );
                 INSERT INTO heartbeat (component, pid, updated_at, config_mtime)
                     VALUES ('collector', 5, 100, 1700000000000);
                 PRAGMA user_version = 5;",
            )
            .expect("seed an old-v5 store");
        }
        let store = Store::open(&path).expect("open reconciles the missing v5 columns");
        // Without the reconcile this write fails with "no such column: config_path".
        store
            .record_collector_stamp(
                "collector",
                9,
                Some("/cfg/tokenomics.toml"),
                Some(1_700_000_060_000),
                Some("/bin/tok"),
                Some(1_699_000_000_000),
            )
            .expect("stamp writes after reconcile");
        let stamp = store.collector_stamp("collector").expect("some");
        assert_eq!(stamp.config_path.as_deref(), Some("/cfg/tokenomics.toml"));
        assert_eq!(stamp.exe_path.as_deref(), Some("/bin/tok"));
        assert_eq!(stamp.exe_mtime, Some(1_699_000_000_000));
    }

    #[test]
    fn open_readonly_reads_a_pre_v5_store_without_erroring_on_missing_columns() {
        // Spec 015 §B/GAP5: doctor opens read-only and skips migration, so a pre-v5 store lacks the
        // stamp columns entirely — `collector_stamp` must degrade to "no data", not error.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.db");
        {
            let conn = Connection::open(&path).expect("raw open");
            conn.execute_batch(
                "CREATE TABLE heartbeat (
                     component TEXT PRIMARY KEY, pid INTEGER NOT NULL, updated_at INTEGER NOT NULL
                 );
                 INSERT INTO heartbeat (component, pid, updated_at) VALUES ('collector', 7, 100);
                 PRAGMA user_version = 4;",
            )
            .expect("seed a v4 store");
        }
        let store = Store::open_readonly(&path).expect("read-only open");
        assert!(
            store.collector_stamp("collector").is_none(),
            "a missing-column read must degrade to no data, silently"
        );
        // A liveness read still works on the untouched v1 columns.
        assert_eq!(
            store.heartbeat_age("collector", 1_100).expect("q"),
            Some(1_000)
        );
    }

    #[test]
    fn heartbeat_upserts() {
        let (_dir, store) = open_temp();
        store.heartbeat("collector", 1234).expect("hb1");
        store.heartbeat("collector", 5678).expect("hb2");
        let pid: u32 = store
            .conn
            .query_row(
                "SELECT pid FROM heartbeat WHERE component='collector'",
                [],
                |r| r.get(0),
            )
            .expect("read pid");
        assert_eq!(pid, 5678);
    }
}
