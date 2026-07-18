//! The Gemini provider adapter: reduce chats-JSON/JSONL usage across all project-hash dirs.
//!
//! Project: Tokenomics — monitor LLM subscription accounts (usage, limits, time-left) in a TUI
//! Module:  src/providers/gemini/mod.rs
//! Deps:    async-trait, jiff; chats (pure core)
//! Tested:  inline `#[cfg(test)]` in chats.rs; adapter test on a fixture tree below (spec 020 §B/C)
//!
//! Key responsibilities:
//! - `GeminiAdapter`: implement `ProviderAdapter::collect` over the account's `GEMINI_CLI_HOME`
//!   (`config_dir`), fanning out across `tmp/<project-hash>/chats/session-*.json[l]` for every
//!   project hash under it — unlike Codex's single `sessions/` tree, a Gemini account's usage is
//!   spread across N workspace-keyed subdirectories (spec 020 §B).
//!
//! Design constraints:
//! - Attribution is the account's `config_dir` (its `GEMINI_CLI_HOME`), never the logs.
//! - No limits/overlay plane exists for Gemini (spec 020 §C) — this adapter only ever produces a
//!   usage snapshot or idle; it never fetches anything over the network.

pub mod chats;

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use jiff::Timestamp;

use crate::domain::{Account, UsageSnapshot};
use crate::error::{AppError, AppResult};
use crate::providers::ProviderAdapter;

use chats::{parse_chat_events, reduce_gemini_snapshot};

/// Cheap mtime prune bound for the chats-tree walk: the reduce window (5h) plus slack for clock
/// skew / buffered writes — mirrors Codex's `MTIME_LOOKBACK` (`codex/mod.rs`). Not a windowing
/// claim; `reduce_gemini_snapshot` enforces the exact 5h cutoff per event.
const MTIME_LOOKBACK: Duration = Duration::from_mins(330);

/// The Gemini usage adapter. Zero-sized — no injected runner: the usage plane is pure filesystem
/// I/O, no subprocess (mirrors `CodexAdapter`).
#[derive(Debug, Default, Clone, Copy)]
pub struct GeminiAdapter;

impl GeminiAdapter {
    /// Build a Gemini adapter.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ProviderAdapter for GeminiAdapter {
    async fn collect(&self, account: &Account, now: Timestamp) -> AppResult<Option<UsageSnapshot>> {
        // Validation (spec 020 §A) guarantees a gemini account always carries a config_dir; this
        // is a defensive early return, never a panic, if that guarantee is ever violated upstream.
        let Some(config_dir) = account.config_dir.as_deref() else {
            return Err(AppError::SessionsScan(format!(
                "account '{}': gemini requires config_dir",
                account.id
            )));
        };
        let cutoff: SystemTime = (now - MTIME_LOOKBACK).into();
        let tmp_dir = config_dir.join("tmp");
        let account_id = account.id.clone();

        // The walk + file reads are synchronous filesystem I/O; run them on the blocking pool so a
        // large chats tree can never stall the collector's current-thread runtime (mirrors
        // `CodexAdapter::collect`, see rules/rust/async-tokio.md).
        tokio::task::spawn_blocking(move || {
            let mut paths = Vec::new();
            collect_chat_paths(&tmp_dir, cutoff, &mut paths);

            let events: Vec<_> = paths
                .iter()
                .filter_map(|p| std::fs::read(p).ok())
                .flat_map(|bytes| parse_chat_events(&bytes))
                .collect();

            reduce_gemini_snapshot(&events, &account_id, now)
        })
        .await
        .map_err(|e| AppError::SessionsScan(e.to_string()))
    }
}

/// Collect `session-*.json[l]` paths under every `<tmp_dir>/<project-hash>/chats/` directory whose
/// mtime is at or after `cutoff`. Fans out across all project-hash subdirs (spec 020 §B) — unlike
/// Codex's single dated tree, a Gemini account's usage is spread across N workspace-keyed dirs. A
/// missing `tmp/` (fresh install) or an unreadable directory yields an empty list — never an error.
fn collect_chat_paths(tmp_dir: &Path, cutoff: SystemTime, out: &mut Vec<PathBuf>) {
    let Ok(hashes) = std::fs::read_dir(tmp_dir) else {
        return;
    };
    for hash_entry in hashes.flatten() {
        // `file_type()` does NOT follow symlinks (unlike `path().is_dir()`) — skip a symlinked
        // tmp/<project-hash> entry so the scan can never walk outside config_dir, matching the
        // Codex sessions-tree scan-bound discipline (spec 013 §B).
        if !hash_entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let chats_dir = hash_entry.path().join("chats");
        let Ok(files) = std::fs::read_dir(&chats_dir) else {
            continue;
        };
        for file_entry in files.flatten() {
            let path = file_entry.path();
            if !is_session_filename(&path) {
                continue;
            }
            let fresh_enough = file_entry
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|mtime| mtime >= cutoff);
            if fresh_enough {
                out.push(path);
            }
        }
    }
}

/// Whether `path`'s filename matches the session naming convention (`session-*.json` or
/// `session-*.jsonl`).
fn is_session_filename(path: &Path) -> bool {
    let stem_matches = path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("session-"));
    stem_matches
        && path.extension().is_some_and(|ext| {
            ext.eq_ignore_ascii_case("json") || ext.eq_ignore_ascii_case("jsonl")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Provider;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    fn account(config_dir: PathBuf) -> Account {
        Account {
            id: "gemini-personal".to_string(),
            label: "Gemini Personal".to_string(),
            provider: Provider::Gemini,
            config_dir: Some(config_dir),
            api_key_env: None,
            active: true,
            color: None,
            limits_overlay: false,
        }
    }

    /// One synthetic turn line, matching the real observed shape (spec 020 §B) — never real
    /// session content, all values invented for this test. `id` is unique per call so two
    /// fixture files' turns are never accidentally deduped against each other.
    fn turn_line(timestamp: Timestamp, input: u64, cached: u64, output: u64, total: u64) -> String {
        format!(
            r#"{{"id":"synthetic-{timestamp}-{input}","tokens":{{"input":{input},"output":{output},"cached":{cached},"thoughts":0,"tool":0,"total":{total}}},"model":"gemini-3.5-flash","timestamp":"{timestamp}"}}"#
        )
    }

    // ── AC3 (spec 020 §B/C): missing tmp/ ⇒ Ok(None) — already true of the stub ────────────────

    #[tokio::test]
    async fn missing_tmp_dir_is_idle() {
        let dir = tempfile::tempdir().expect("tempdir");
        let snap = GeminiAdapter::new()
            .collect(&account(dir.path().to_path_buf()), Timestamp::now())
            .await
            .expect("collect ok");
        assert!(snap.is_none());
    }

    // ── AC3: a fixture tree spanning multiple project hashes merges into one snapshot ───────────

    #[tokio::test]
    async fn collect_fans_out_across_multiple_project_hashes_and_merges() {
        let now = Timestamp::now();
        let dir = tempfile::tempdir().expect("tempdir");
        let recent = now - Duration::from_hours(1); // in-window

        let hash_a = dir.path().join("tmp/aaa111/chats");
        std::fs::create_dir_all(&hash_a).expect("mkdir -p");
        std::fs::write(
            hash_a.join("session-a.jsonl"),
            turn_line(recent, 1000, 100, 50, 1050),
        )
        .expect("write session a");

        let hash_b = dir.path().join("tmp/bbb222/chats");
        std::fs::create_dir_all(&hash_b).expect("mkdir -p");
        std::fs::write(
            hash_b.join("session-b.jsonl"),
            turn_line(recent, 2000, 500, 100, 2500),
        )
        .expect("write session b");

        let snap = GeminiAdapter::new()
            .collect(&account(dir.path().to_path_buf()), now)
            .await
            .expect("collect ok")
            .expect("a real fixture tree with in-window turns must yield a snapshot, not idle");
        assert_eq!(snap.account_id, "gemini-personal");
        assert_eq!(snap.provider, Provider::Gemini);
        // input = (1000-100) + (2000-500) = 900 + 1500 = 2400
        assert_eq!(snap.input, 2_400);
        assert_eq!(snap.cache_read, 600); // 100 + 500
        assert_eq!(snap.output, 150); // 50 + 100
        assert_eq!(snap.total_tokens, 3_550); // 1050 + 2500
        assert_eq!(snap.cost_notional, None);
        assert!(snap.window.is_none());
    }

    // ── AC3: mtime pruning skips a stale file before its (in-window) content is even read ───────

    #[tokio::test]
    async fn stale_files_are_pruned_by_mtime_before_reduction() {
        let now = Timestamp::now();
        let dir = tempfile::tempdir().expect("tempdir");
        let hash = dir.path().join("tmp/aaa111/chats");
        std::fs::create_dir_all(&hash).expect("mkdir -p");

        let recent = now - Duration::from_hours(1); // the event's own timestamp IS in-window
        let path = hash.join("session-old.jsonl");
        std::fs::write(&path, turn_line(recent, 1000, 100, 50, 1050)).expect("write");
        let file = std::fs::File::open(&path).expect("reopen for mtime");
        file.set_modified(SystemTime::now() - Duration::from_hours(24))
            .expect("backdate mtime"); // only the FILE's mtime is stale

        let snap = GeminiAdapter::new()
            .collect(&account(dir.path().to_path_buf()), now)
            .await
            .expect("collect ok");
        assert!(
            snap.is_none(),
            "a file pruned by mtime must never contribute, even with an in-window event inside it"
        );
    }

    // ── a symlinked tmp/<project-hash> dir must never be walked (scan-bound discipline) ─────────

    #[tokio::test]
    #[cfg(unix)]
    async fn a_symlinked_project_hash_dir_is_never_walked() {
        let now = Timestamp::now();
        let dir = tempfile::tempdir().expect("tempdir");
        let recent = now - Duration::from_hours(1);

        // A real, in-window session file OUTSIDE config_dir, reachable only via a symlink planted
        // inside tmp/ — this must never be picked up (spec-013 scan-bound discipline, spec 020 §B).
        let outside = tempfile::tempdir().expect("outside tempdir");
        let outside_chats = outside.path().join("chats");
        std::fs::create_dir_all(&outside_chats).expect("mkdir -p");
        std::fs::write(
            outside_chats.join("session-outside.jsonl"),
            turn_line(recent, 1000, 100, 50, 1050),
        )
        .expect("write outside session");

        let tmp_dir = dir.path().join("tmp");
        std::fs::create_dir_all(&tmp_dir).expect("mkdir -p tmp");
        std::os::unix::fs::symlink(outside.path(), tmp_dir.join("symlinked-hash"))
            .expect("symlink");

        let snap = GeminiAdapter::new()
            .collect(&account(dir.path().to_path_buf()), now)
            .await
            .expect("collect ok");
        assert!(
            snap.is_none(),
            "a symlinked project-hash dir must never be walked, even with in-window content: {snap:?}"
        );
    }

    #[tokio::test]
    async fn account_without_config_dir_yields_an_error_not_a_panic() {
        // Validation (spec 020 §A) guarantees a gemini account always carries a config_dir; this
        // defensively covers the case where that guarantee is ever violated upstream.
        let mut a = account(PathBuf::new());
        a.config_dir = None;
        let result = GeminiAdapter::new().collect(&a, Timestamp::now()).await;
        assert!(
            result.is_err(),
            "a gemini account with no config_dir must error, never panic: {result:?}"
        );
    }
}
