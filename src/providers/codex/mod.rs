//! The Codex provider adapter: reduce sessions-JSONL usage, fetch app-server rate limits.
//!
//! Project: Tokenomics — monitor LLM subscription accounts (usage, limits, time-left) in a TUI
//! Module:  src/providers/codex/mod.rs
//! Deps:    async-trait, jiff; sessions (pure core), rate_limits (app-server seam)
//! Tested:  inline `#[cfg(test)]` in sessions.rs / rate_limits.rs; adapter test on a fixture tree
//!
//! Key responsibilities:
//! - `CodexAdapter`: implement `ProviderAdapter::collect` over the account's `CODEX_HOME`
//!   sessions tree (spec 013 §B).
//!
//! Design constraints:
//! - Attribution is the account's `config_dir` (its `CODEX_HOME`), never the logs.
//! - Usage is the local ToS-safe plane; the app-server rate-limits fetch is the opt-in overlay
//!   plane — never conflated (provenance-tagged at the source).

pub mod rate_limits;
pub mod sessions;

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use jiff::tz::TimeZone;
use jiff::Timestamp;

use crate::domain::{Account, UsageSnapshot};
use crate::error::{AppError, AppResult};
use crate::providers::ProviderAdapter;

use sessions::{parse_rollout_events, reduce_codex_snapshot};

/// Cheap mtime prune bound for the sessions-tree walk: the reduce window (5h) plus slack for
/// clock skew / buffered writes. Not a windowing claim — `reduce_codex_snapshot` enforces the
/// exact 5h cutoff per event; this only bounds which files are worth opening.
const MTIME_LOOKBACK: Duration = Duration::from_mins(330);

/// Collect Codex usage by walking the account's `<CODEX_HOME>/sessions/` rollout-JSONL tree.
/// No injected runner: the usage plane is pure filesystem I/O, no subprocess.
#[derive(Debug, Default, Clone, Copy)]
pub struct CodexAdapter;

impl CodexAdapter {
    /// Build a Codex adapter.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ProviderAdapter for CodexAdapter {
    async fn collect(&self, account: &Account, now: Timestamp) -> AppResult<Option<UsageSnapshot>> {
        // Validation (spec 019 §A) guarantees a codex account always carries a config_dir; this is
        // a defensive early return, never a panic, if that guarantee is ever violated upstream.
        let Some(config_dir) = account.config_dir.as_deref() else {
            return Err(AppError::SessionsScan(format!(
                "account '{}': codex requires config_dir",
                account.id
            )));
        };
        let cutoff: SystemTime = (now - MTIME_LOOKBACK).into();
        let window = covering_window(now);
        let sessions_dir = config_dir.join("sessions");
        let account_id = account.id.clone();

        // The walk + file reads are synchronous filesystem I/O; run them on the blocking pool so a
        // large sessions tree can never stall the collector's current-thread runtime (the loop task
        // drives this future via its JoinSet — see rules/rust/async-tokio.md).
        tokio::task::spawn_blocking(move || {
            let mut paths = Vec::new();
            collect_rollout_paths(&sessions_dir, cutoff, window, 0, &mut paths);

            let events: Vec<_> = paths
                .iter()
                .filter_map(|p| std::fs::read(p).ok())
                .flat_map(|bytes| parse_rollout_events(&bytes))
                .collect();

            reduce_codex_snapshot(&events, &account_id, now)
        })
        .await
        .map_err(|e| AppError::SessionsScan(e.to_string()))
    }
}

/// The span of calendar dates the sessions walk may touch, as `(year, month, day)` bounds. The
/// mtime-lookback window widened by a full day either side: the ±1-day margin absorbs clock skew and
/// the fact that a `sessions/YYYY/MM/DD` directory is named in LOCAL time while we reason in UTC — a
/// day of slack beats TZ math (spec 013 §B). A YYYY/MM/DD directory provably outside these bounds is
/// pruned before we recurse into it; a per-day span is far more than enough to keep the walk bounded.
#[derive(Debug, Clone, Copy)]
struct DateWindow {
    lo: (i32, i32, i32),
    hi: (i32, i32, i32),
}

/// Build the covering date window for `now`: `[now − MTIME_LOOKBACK − 1 day, now + 1 day]` in UTC.
fn covering_window(now: Timestamp) -> DateWindow {
    let day = Duration::from_hours(24);
    DateWindow {
        lo: date_tuple(now - MTIME_LOOKBACK - day),
        hi: date_tuple(now + day),
    }
}

/// A timestamp's UTC calendar date as `(year, month, day)` — ordered so tuple comparison is calendar
/// comparison. Widened to `i32` so a many-digit directory name can be parsed uniformly (below).
fn date_tuple(ts: Timestamp) -> (i32, i32, i32) {
    let d = ts.to_zoned(TimeZone::UTC).date();
    (
        i32::from(d.year()),
        i32::from(d.month()),
        i32::from(d.day()),
    )
}

/// Recursively collect `rollout-*.jsonl` paths under `dir` whose mtime is at or after `cutoff`, while
/// pruning `sessions/YYYY/MM/DD` subtrees provably outside `window` before descending (spec 013 §B).
/// `depth` (seeded at 0 by the caller) is 0/1/2 below `sessions/` = the year/month/day level, which
/// is how a directory name is placed on the calendar. A directory or file that can't be read is
/// skipped, not an error — a missing `sessions/` root (fresh install) yields an empty list (idle).
fn collect_rollout_paths(
    dir: &Path,
    cutoff: SystemTime,
    window: DateWindow,
    depth: usize,
    out: &mut Vec<PathBuf>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if file_type.is_dir() {
            if dir_out_of_window(&path, depth, window) {
                continue; // provably outside the covering dates — skip the whole subtree
            }
            collect_rollout_paths(&path, cutoff, window, depth + 1, out);
        } else if is_rollout_filename(&path) {
            let fresh_enough = entry
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|mtime| mtime >= cutoff);
            if fresh_enough {
                out.push(path);
            }
        }
    }
}

/// Whether a sessions subdirectory `path` at `depth` below `sessions/` (0/1/2 = year/month/day) is
/// PROVABLY outside `window` and can be skipped without recursing. Its trailing `depth + 1` path
/// components spell `YYYY[/MM[/DD]]`; a component that isn't a plain number (a stray non-date folder)
/// makes the date unprovable ⇒ NOT prunable (recurse anyway — never skip what we can't place). Depth
/// ≥3 (unexpected nesting) is likewise never pruned.
fn dir_out_of_window(path: &Path, depth: usize, window: DateWindow) -> bool {
    let Some(parts) = trailing_date_parts(path, depth + 1) else {
        return false;
    };
    match parts.as_slice() {
        [y] => *y < window.lo.0 || *y > window.hi.0,
        [y, m] => (*y, *m) < (window.lo.0, window.lo.1) || (*y, *m) > (window.hi.0, window.hi.1),
        [y, m, d] => (*y, *m, *d) < window.lo || (*y, *m, *d) > window.hi,
        _ => false,
    }
}

/// The last `n` path components parsed as integers, or `None` if any of them isn't a plain number
/// (a non-date directory name, or fewer than `n` components) — the "can't prove it's a date" case.
fn trailing_date_parts(path: &Path, n: usize) -> Option<Vec<i32>> {
    let comps: Vec<&std::ffi::OsStr> = path
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s),
            _ => None,
        })
        .collect();
    if comps.len() < n {
        return None;
    }
    comps[comps.len() - n..]
        .iter()
        .map(|s| s.to_str()?.parse::<i32>().ok())
        .collect()
}

/// Whether `path`'s filename matches the rollout naming convention (`rollout-*.jsonl`).
fn is_rollout_filename(path: &Path) -> bool {
    let stem_matches = path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("rollout-"));
    stem_matches
        && path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Provider;

    fn account(config_dir: PathBuf) -> Account {
        Account {
            id: "codex-acct".to_string(),
            label: "Codex".to_string(),
            provider: Provider::Codex,
            config_dir: Some(config_dir),
            api_key_env: None,
            active: true,
            color: None,
            limits_overlay: false,
        }
    }

    fn token_count_line(
        timestamp: Timestamp,
        input: u64,
        cached: u64,
        output: u64,
        total: u64,
    ) -> String {
        format!(
            r#"{{"timestamp":"{timestamp}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":{input},"cached_input_tokens":{cached},"output_tokens":{output},"reasoning_output_tokens":0,"total_tokens":{total}}},"last_token_usage":{{"input_tokens":{input},"cached_input_tokens":{cached},"output_tokens":{output},"reasoning_output_tokens":0,"total_tokens":{total}}},"model_context_window":353400}},"rate_limits":null}}}}"#
        )
    }

    #[tokio::test]
    async fn missing_sessions_dir_is_idle() {
        let dir = tempfile::tempdir().expect("tempdir");
        let snap = CodexAdapter::new()
            .collect(&account(dir.path().to_path_buf()), Timestamp::now())
            .await
            .expect("collect ok");
        assert!(snap.is_none());
    }

    /// `now`'s UTC date as a `sessions/YYYY/MM/DD` sub-path, so a walk test's dated dir is always
    /// inside the covering window regardless of when the suite runs.
    fn today_dir(now: Timestamp) -> String {
        let d = now.to_zoned(TimeZone::UTC).date();
        format!("sessions/{:04}/{:02}/{:02}", d.year(), d.month(), d.day())
    }

    #[tokio::test]
    async fn collect_walks_the_sessions_tree_and_reduces() {
        let now = Timestamp::now();
        let dir = tempfile::tempdir().expect("tempdir");
        let day_dir = dir.path().join(today_dir(now));
        std::fs::create_dir_all(&day_dir).expect("mkdir -p");

        let recent = now - Duration::from_hours(1); // 1h ago: in-window
        let line = token_count_line(recent, 100, 10, 5, 105);
        std::fs::write(day_dir.join("rollout-abc.jsonl"), line).expect("write rollout file");

        let snap = CodexAdapter::new()
            .collect(&account(dir.path().to_path_buf()), now)
            .await
            .expect("collect ok")
            .expect("snapshot present");
        assert_eq!(snap.account_id, "codex-acct");
        assert_eq!(snap.provider, Provider::Codex);
        assert_eq!(snap.input, 90);
        assert_eq!(snap.cache_read, 10);
        assert_eq!(snap.output, 5);
        assert_eq!(snap.total_tokens, 105);
        assert_eq!(snap.cost_notional, None);
        assert!(snap.window.is_none());
    }

    #[tokio::test]
    async fn stale_files_are_pruned_by_mtime_before_reduction() {
        let now = Timestamp::now();
        let dir = tempfile::tempdir().expect("tempdir");
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).expect("mkdir -p");

        // The event timestamp itself is in-window; only the file's mtime is stale, which the
        // cheap prune must catch before the content is even read.
        let recent = now - Duration::from_hours(1);
        let line = token_count_line(recent, 100, 10, 5, 105);
        let path = sessions.join("rollout-old.jsonl");
        std::fs::write(&path, line).expect("write rollout file");
        let file = std::fs::File::open(&path).expect("reopen for mtime");
        file.set_modified(SystemTime::now() - Duration::from_hours(24))
            .expect("backdate mtime");

        let snap = CodexAdapter::new()
            .collect(&account(dir.path().to_path_buf()), now)
            .await
            .expect("collect ok");
        assert!(
            snap.is_none(),
            "a stale file must be pruned before its content is read"
        );
    }

    #[tokio::test]
    async fn old_dated_dirs_are_date_pruned_while_non_date_dirs_are_still_recursed() {
        // Spec 013 §B: the walk stays bounded to the covering dates. An old `YYYY/MM/DD` dir is
        // pruned before recursing even when its rollout file's MTIME is fresh AND its event is
        // in-window — only the date prune (not mtime, not the reduce cutoff) can keep it out. A
        // non-date directory name can't be placed on the calendar, so it is recursed anyway.
        let now = Timestamp::now();
        let recent = now - Duration::from_hours(1); // in-window either way
        let dir = tempfile::tempdir().expect("tempdir");

        // Years-old dated dir with a FRESH file — must be date-pruned, so its tokens never appear.
        let old = dir.path().join("sessions/2020/01/01");
        std::fs::create_dir_all(&old).expect("mkdir old");
        std::fs::write(
            old.join("rollout-2020-01-01T00-00-00-old.jsonl"),
            token_count_line(recent, 7000, 0, 0, 7000),
        )
        .expect("write old");

        // Non-date dir with a fresh in-window file — must be recursed and read.
        let misc = dir.path().join("sessions/misc");
        std::fs::create_dir_all(&misc).expect("mkdir misc");
        std::fs::write(
            misc.join("rollout-live.jsonl"),
            token_count_line(recent, 100, 10, 5, 105),
        )
        .expect("write misc");

        let snap = CodexAdapter::new()
            .collect(&account(dir.path().to_path_buf()), now)
            .await
            .expect("collect ok")
            .expect("the non-date dir's file is read");
        // Only the misc file contributes: the 2020 subtree was skipped before its fresh file opened.
        assert_eq!(
            snap.total_tokens, 105,
            "the old-dated dir must be date-pruned, not summed in"
        );
        assert_eq!(snap.input, 90);
    }
}
