//! Native Claude transcript scanner: `projects/**/*.jsonl` → active 5h block, in-process (spec 026).
//!
//! Project: Tokenomics — monitor LLM subscription accounts (usage, limits, time-left) in a TUI
//! Module:  src/providers/claude/transcripts.rs
//! Deps:    serde / serde_json (line parse), jiff (block math), tokio (blocking pool + mutex)
//! Tested:  inline `#[cfg(test)]` — line parse, dedup, block table, pricing, reduce, scanner cache
//!
//! Key responsibilities:
//! - `parse_line`: one JSONL line → `UsageEvent` (assistant turns with `message.usage` only).
//! - `active_block`: ccusage's 5h session-block semantics over sorted events (pure).
//! - `price_for`: static longest-prefix pricing table → notional USD (a proxy, never a bill).
//! - `reduce_snapshot`: events + `now` → `UsageSnapshot` (pure).
//! - `TranscriptScanner`: the stateful walker — mtime-pruned, per-file append-only cache (only bytes past the last complete line are
//!   parsed), one scan per
//!   `(canonical projects dir, now)` so N accounts symlinked to one tree cost one scan per tick.
//!
//! Design constraints:
//! - Streaming: files are read line by line; memory is O(events in lookback), never O(bytes).
//! - Bounded: only files with mtime ≥ `now − LOOKBACK` are opened; symlinked dirs are not followed.
//! - Serialised: all scans share one async mutex (`ponytail:` global lock; per-dir if it ever matters).
//! - Malformed lines / unreadable files are skipped, never errors; a missing tree is idle (`None`).

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use jiff::{SignedDuration, Timestamp};
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::domain::{Provider, UsageSnapshot, Window};
use crate::error::{AppError, AppResult};

/// ccusage's session-block length.
pub const BLOCK_LEN: Duration = Duration::from_hours(5);
/// `BLOCK_LEN` as jiff's signed duration (what `Timestamp::duration_since` yields).
const BLOCK_SPAN: SignedDuration = SignedDuration::from_hours(5);
/// Files older than this are not opened. Blocks chain from the last ≥5h gap; a day of lookback
/// covers any realistic chain (spec 026 FR-4, deferred: phase drift when no gap falls inside it).
pub const LOOKBACK: Duration = Duration::from_hours(24);

/// One assistant turn's token usage, as recorded in a transcript line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageEvent {
    /// Dedup key: `message.id` + `requestId` (or file+line when either is absent).
    pub key: String,
    /// When the turn happened.
    pub ts: Timestamp,
    /// The model that produced it (drives pricing).
    pub model: String,
    /// Non-cache input tokens.
    pub input: u64,
    /// Output tokens.
    pub output: u64,
    /// Cache-creation input tokens.
    pub cache_creation: u64,
    /// Cache-read input tokens.
    pub cache_read: u64,
}

#[cfg(test)]
impl UsageEvent {
    /// All four buckets summed (ccusage's `totalTokens`).
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_creation + self.cache_read
    }
}

#[derive(Deserialize)]
struct Line {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<String>,
    #[serde(rename = "requestId")]
    request_id: Option<String>,
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    id: Option<String>,
    model: Option<String>,
    usage: Option<Usage>,
}

#[derive(Deserialize, Default)]
#[allow(clippy::struct_field_names)] // wire names from the transcript schema, verbatim
struct Usage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

/// Parse one transcript line. `fallback_key` is used when the line lacks `message.id`+`requestId`
/// (so such a line is still counted, once). Anything that is not an assistant turn with usage —
/// user lines, meta lines, malformed JSON — yields `None`.
pub fn parse_line(line: &str, fallback_key: &str) -> Option<UsageEvent> {
    // Cheap pre-filter: the vast majority of lines carry no usage at all.
    if !line.contains("\"usage\"") {
        return None;
    }
    let parsed: Line = serde_json::from_str(line).ok()?;
    if parsed.kind.as_deref() != Some("assistant") {
        return None;
    }
    let message = parsed.message?;
    let usage = message.usage?;
    let ts = parsed.timestamp?.parse::<Timestamp>().ok()?;
    let key = match (message.id, parsed.request_id) {
        (Some(id), Some(req)) => format!("{id}:{req}"),
        _ => fallback_key.to_string(),
    };
    Some(UsageEvent {
        key,
        ts,
        model: message.model.unwrap_or_default(),
        input: usage.input_tokens,
        output: usage.output_tokens,
        cache_creation: usage.cache_creation_input_tokens,
        cache_read: usage.cache_read_input_tokens,
    })
}

/// Parse a whole transcript (bytes) into events, in file order. Test/fixture helper; the scanner
/// streams the same logic per line.
#[cfg(test)]
pub fn parse_transcript(bytes: &[u8], file_tag: &str) -> Vec<UsageEvent> {
    bytes
        .split(|b| *b == b'\n')
        .enumerate()
        .filter_map(|(i, line)| {
            let line = std::str::from_utf8(line).ok()?;
            parse_line(line, &format!("{file_tag}#{i}"))
        })
        .collect()
}

// ── pricing ─────────────────────────────────────────────────────────────────────────────────────

/// `(model-id prefix, input $/MTok, output $/MTok, cache-read $/MTok)`. Cache write = 1.25 × input.
/// Longest matching prefix wins. Notional only — a usage proxy, never a bill.
const PRICING: &[(&str, f64, f64, f64)] = &[
    ("claude-fable-5-1", 10.0, 50.0, 0.25),
    ("claude-fable-5", 10.0, 50.0, 1.0),
    ("claude-mythos", 10.0, 50.0, 0.25),
    ("claude-opus-5", 5.0, 25.0, 0.5),
    ("claude-opus-4-8", 5.0, 25.0, 0.5),
    ("claude-opus-4-7", 5.0, 25.0, 0.5),
    ("claude-opus-4-6", 5.0, 25.0, 0.5),
    ("claude-opus-4-5", 5.0, 25.0, 0.5),
    ("claude-opus-4", 15.0, 75.0, 1.5),
    ("claude-opus-3", 15.0, 75.0, 1.5),
    ("claude-sonnet-5", 2.0, 10.0, 0.2),
    ("claude-sonnet-4", 3.0, 15.0, 0.3),
    ("claude-3-7-sonnet", 3.0, 15.0, 0.3),
    ("claude-3-5-sonnet", 3.0, 15.0, 0.3),
    ("claude-haiku-4", 1.0, 5.0, 0.1),
    ("claude-3-5-haiku", 0.8, 4.0, 0.08),
    ("opus", 5.0, 25.0, 0.5),
    ("sonnet", 2.0, 10.0, 0.2),
    ("haiku", 1.0, 5.0, 0.1),
];

/// Notional USD for one event, or `None` for an unknown model (its tokens still count; its cost
/// contributes nothing rather than a guess).
#[allow(clippy::cast_precision_loss)] // token counts are far below 2^53
pub fn price_for(event: &UsageEvent) -> Option<f64> {
    let (_, input, output, cache_read) = PRICING
        .iter()
        .filter(|(prefix, ..)| event.model.starts_with(prefix))
        .max_by_key(|(prefix, ..)| prefix.len())?;
    let per_m = |tokens: u64, rate: f64| tokens as f64 / 1_000_000.0 * rate;
    Some(
        per_m(event.input, *input)
            + per_m(event.output, *output)
            + per_m(event.cache_creation, input * 1.25)
            + per_m(event.cache_read, *cache_read),
    )
}

// ── blocks ──────────────────────────────────────────────────────────────────────────────────────

/// The active 5h block: bounds plus the totals of the events inside it.
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    /// Block start (first entry's timestamp floored to the hour).
    pub start: Timestamp,
    /// `start + 5h` — the reset boundary.
    pub end: Timestamp,
    /// First entry in the block.
    pub first: Timestamp,
    /// Last entry in the block.
    pub last: Timestamp,
    /// Non-cache input tokens.
    pub input: u64,
    /// Output tokens.
    pub output: u64,
    /// Cache-creation input tokens.
    pub cache_creation: u64,
    /// Cache-read input tokens.
    pub cache_read: u64,
    /// Summed notional USD over priced events.
    pub cost: f64,
}

impl Block {
    fn open(event: &UsageEvent) -> Self {
        let start = floor_to_hour(event.ts);
        Self {
            start,
            end: start + BLOCK_LEN,
            first: event.ts,
            last: event.ts,
            input: 0,
            output: 0,
            cache_creation: 0,
            cache_read: 0,
            cost: 0.0,
        }
    }

    fn absorb(&mut self, event: &UsageEvent) {
        self.last = event.ts;
        self.input += event.input;
        self.output += event.output;
        self.cache_creation += event.cache_creation;
        self.cache_read += event.cache_read;
        self.cost += price_for(event).unwrap_or(0.0);
    }

    /// All four buckets summed.
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_creation + self.cache_read
    }
}

fn floor_to_hour(ts: Timestamp) -> Timestamp {
    let secs = ts.as_second().div_euclid(3600) * 3600;
    // A multiple of 3600 inside jiff's range is always representable; fall back to `ts` (never panic).
    Timestamp::from_second(secs).unwrap_or(ts)
}

/// Fold DEDUPED, TIME-SORTED events into ccusage session blocks and return the active one. A new
/// block opens when there is none, when `ts − block.start ≥ 5h`, or when `ts − prev ≥ 5h`. Active
/// ⇔ `now < end` and `now − last < 5h`. Events after `now` are ignored (clock skew).
pub fn active_block(events: &[UsageEvent], now: Timestamp) -> Option<Block> {
    let mut current: Option<Block> = None;
    for event in events.iter().filter(|e| e.ts <= now) {
        let opens_new = match &current {
            None => true,
            Some(b) => {
                event.ts.duration_since(b.start) >= BLOCK_SPAN
                    || event.ts.duration_since(b.last) >= BLOCK_SPAN
            }
        };
        if opens_new {
            current = Some(Block::open(event));
        }
        if let Some(b) = current.as_mut() {
            b.absorb(event);
        }
    }
    current.filter(|b| now < b.end && now.duration_since(b.last) < BLOCK_SPAN)
}

/// Dedup by key (first wins) and sort by timestamp — the precondition of `active_block`.
pub fn dedup_sorted(events: &[UsageEvent]) -> Vec<UsageEvent> {
    let mut seen = HashSet::new();
    let mut out: Vec<UsageEvent> = events
        .iter()
        .filter(|e| seen.insert(e.key.as_str()))
        .cloned()
        .collect();
    out.sort_by_key(|e| e.ts);
    out
}

/// Raw events → the account's `UsageSnapshot` for its active block, or `None` when idle.
#[allow(clippy::cast_precision_loss)] // millisecond spans are far below 2^53
pub fn reduce_snapshot(
    events: &[UsageEvent],
    account_id: &str,
    now: Timestamp,
) -> Option<UsageSnapshot> {
    let block = active_block(&dedup_sorted(events), now)?;
    let span_min = (block.last.as_millisecond() - block.first.as_millisecond()) as f64 / 60_000.0;
    // A sub-minute span makes tok/min a meaningless spike (two events 3 s apart ⇒ ~200k/min).
    let (tokens_per_minute, cost_per_hour) = if span_min >= 1.0 {
        (
            block.total() as f64 / span_min,
            block.cost / span_min * 60.0,
        )
    } else {
        (0.0, 0.0)
    };
    Some(UsageSnapshot {
        account_id: account_id.to_string(),
        provider: Provider::Claude,
        collected_at: now,
        input: block.input,
        output: block.output,
        cache_read: block.cache_read,
        cache_creation: block.cache_creation,
        total_tokens: block.total(),
        cost_notional: Some(block.cost),
        window: Some(Window {
            start: block.start,
            end: block.end,
            remaining_minutes: Some((block.end.as_millisecond() - now.as_millisecond()) / 60_000),
            tokens_per_minute,
            cost_per_hour,
        }),
    })
}

// ── scanner ─────────────────────────────────────────────────────────────────────────────────────

/// Per-file parse cache: unchanged `(mtime, len)` ⇒ reuse; grown ⇒ parse only the appended complete
/// lines; shrunk ⇒ parse again from zero.
#[derive(Debug, Default)]
struct FileEntry {
    mtime: Option<SystemTime>,
    len: u64,
    /// Byte position just after the last complete (`\n`-terminated) line parsed.
    offset: u64,
    /// Complete lines consumed, so the `{path}#{index}` dedup fallback matches a full parse.
    lines: usize,
    events: Vec<UsageEvent>,
}

/// One canonical `projects/` tree's cache + the per-tick memo.
#[derive(Debug, Default)]
struct DirCache {
    files: HashMap<PathBuf, FileEntry>,
    memo: Option<(Timestamp, Arc<Vec<UsageEvent>>)>,
}

/// The stateful walker shared by every Claude account (spec 026 FR-9..FR-11).
#[derive(Debug, Default)]
pub struct TranscriptScanner {
    dirs: Mutex<HashMap<PathBuf, DirCache>>,
}

impl TranscriptScanner {
    /// A fresh scanner (empty caches).
    pub fn new() -> Self {
        Self::default()
    }

    /// Events under `projects_dir` with mtime inside the lookback, memoised per `(canonical dir,
    /// now)`. `Ok(None)` when the tree does not exist (fresh install ⇒ idle).
    pub async fn events(
        &self,
        projects_dir: &Path,
        now: Timestamp,
    ) -> AppResult<Option<Arc<Vec<UsageEvent>>>> {
        let Ok(canonical) = std::fs::canonicalize(projects_dir) else {
            return Ok(None);
        };
        // Holding the lock across the blocking scan is the point: concurrent collects for any
        // Claude account queue here, and the ones sharing this tree then hit the memo.
        let mut dirs = self.dirs.lock().await;
        let cache = dirs.entry(canonical.clone()).or_default();
        if let Some((at, events)) = &cache.memo {
            if *at == now {
                return Ok(Some(Arc::clone(events)));
            }
        }
        let mut files = std::mem::take(&mut cache.files);
        let cutoff: SystemTime = (now - LOOKBACK).into();
        let (files, events) = tokio::task::spawn_blocking(move || {
            let events = scan_tree(&canonical, cutoff, &mut files);
            (files, events)
        })
        .await
        .map_err(|e| AppError::SessionsScan(e.to_string()))?;
        let events = Arc::new(events);
        cache.files = files;
        cache.memo = Some((now, Arc::clone(&events)));
        Ok(Some(events))
    }
}

/// Walk `root`, refresh `files` for every fresh-enough `*.jsonl` (evicting the rest), and return the
/// concatenated events (undeduped — `reduce_snapshot` dedups).
fn scan_tree(
    root: &Path,
    cutoff: SystemTime,
    files: &mut HashMap<PathBuf, FileEntry>,
) -> Vec<UsageEvent> {
    let mut paths = Vec::new();
    collect_jsonl_paths(root, cutoff, &mut paths);
    let mut fresh = HashMap::with_capacity(paths.len());
    for (path, mtime, len) in paths {
        // A shrunk (truncated or replaced) file restarts from zero.
        let mut entry = files
            .remove(&path)
            .filter(|e| len >= e.offset)
            .unwrap_or_default();
        if entry.mtime != Some(mtime) || entry.len != len {
            // Open failure ⇒ no entry, so the next tick retries instead of caching "empty".
            if append_from_offset(&path, &mut entry).is_none() {
                continue;
            }
            entry.mtime = Some(mtime);
            entry.len = len;
        }
        fresh.insert(path, entry);
    }
    *files = fresh;
    files
        .values()
        .flat_map(|e| e.events.iter().cloned())
        .collect()
}

/// Parse the complete lines past `entry.offset` into `entry`; a trailing line without `\n` (caught
/// mid-write) is left for a later tick. `None` on open/seek/read failure (entry left untouched
/// beyond lines already consumed).
fn append_from_offset(path: &Path, entry: &mut FileEntry) -> Option<()> {
    let mut file = std::fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(entry.offset)).ok()?;
    let mut reader = BufReader::new(file);
    let tag = path.to_string_lossy();
    let mut buf = Vec::new();
    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf).ok()? == 0 || buf.last() != Some(&b'\n') {
            return Some(());
        }
        if let Ok(text) = std::str::from_utf8(&buf) {
            entry
                .events
                .extend(parse_line(text, &format!("{tag}#{}", entry.lines)));
        }
        entry.offset += buf.len() as u64;
        entry.lines += 1;
    }
}

/// Recursively collect `(path, mtime, len)` for `*.jsonl` files under `dir` with mtime ≥ `cutoff`.
/// Symlinked directories are not followed; unreadable entries are skipped.
fn collect_jsonl_paths(dir: &Path, cutoff: SystemTime, out: &mut Vec<(PathBuf, SystemTime, u64)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if file_type.is_dir() {
            collect_jsonl_paths(&path, cutoff, out);
        } else if file_type.is_file() && path.extension().is_some_and(|e| e == "jsonl") {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if let Ok(mtime) = meta.modified() {
                if mtime >= cutoff {
                    out.push((path, mtime, meta.len()));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const SAMPLE: &[u8] = include_bytes!("../../../fixtures/transcripts/sample.jsonl");

    fn ts(s: &str) -> Timestamp {
        s.parse().expect("valid timestamp")
    }

    fn event(key: &str, at: &str, model: &str, output: u64) -> UsageEvent {
        UsageEvent {
            key: key.to_string(),
            ts: ts(at),
            model: model.to_string(),
            input: 10,
            output,
            cache_creation: 0,
            cache_read: 0,
        }
    }

    // ── parse ───────────────────────────────────────────────────────────────────────────────────

    #[test]
    fn fixture_parses_only_assistant_turns_with_usage_and_dedups_by_id_and_request() {
        let events = parse_transcript(SAMPLE, "sample");
        // 4 assistant+usage lines in the fixture, one of which is a byte-identical re-append.
        assert_eq!(events.len(), 4);
        let deduped = dedup_sorted(&events);
        assert_eq!(deduped.len(), 3);
        let first = &deduped[0];
        assert_eq!(first.model, "claude-opus-5");
        assert_eq!(
            (
                first.input,
                first.output,
                first.cache_creation,
                first.cache_read
            ),
            (2, 362, 39294, 26574)
        );
        assert_eq!(first.total(), 2 + 362 + 39294 + 26574);
    }

    #[test]
    fn user_meta_malformed_and_usage_less_lines_are_not_events() {
        assert!(parse_line(r#"{"type":"user","message":{"usage":{}}}"#, "k").is_none());
        assert!(parse_line(r#"{"type":"assistant","message":{"id":"m"}}"#, "k").is_none());
        assert!(parse_line("not json \"usage\"", "k").is_none());
        assert!(parse_line("", "k").is_none());
    }

    #[test]
    fn a_line_without_ids_falls_back_to_the_file_key_and_still_counts() {
        let line = r#"{"type":"assistant","timestamp":"2026-09-04T10:00:00Z","message":{"model":"claude-sonnet-5","usage":{"output_tokens":5}}}"#;
        let e = parse_line(line, "f#7").expect("parses");
        assert_eq!(e.key, "f#7");
        assert_eq!(e.output, 5);
    }

    // ── pricing ─────────────────────────────────────────────────────────────────────────────────

    #[test]
    fn pricing_uses_longest_prefix_and_bucket_rates() {
        let mut e = event("k", "2026-09-04T10:00:00Z", "claude-opus-4-8", 1_000_000);
        e.input = 1_000_000;
        e.cache_creation = 1_000_000;
        e.cache_read = 1_000_000;
        // opus-4-8 → 5 in + 25 out + 6.25 write + 0.5 read (NOT the bare "claude-opus-4" 15/75 row).
        let usd = price_for(&e).expect("priced");
        assert!((usd - 36.75).abs() < 1e-9, "{usd}");
        let fable = event("k", "2026-09-04T10:00:00Z", "claude-fable-5-1", 1_000_000);
        assert!((price_for(&fable).expect("priced") - 50.0001).abs() < 1e-6);
    }

    #[test]
    fn unknown_model_is_unpriced_but_its_tokens_still_count() {
        let unknown = event("k", "2026-09-04T10:00:00Z", "<synthetic>", 7);
        assert!(price_for(&unknown).is_none());
        let snap = reduce_snapshot(&[unknown], "a", ts("2026-09-04T10:01:00Z")).expect("active");
        assert_eq!(snap.total_tokens, 17);
        assert_eq!(snap.cost_notional, Some(0.0));
    }

    // ── blocks ──────────────────────────────────────────────────────────────────────────────────

    #[test]
    fn block_starts_at_the_floored_hour_and_ends_five_hours_later() {
        let events = [event("a", "2026-09-04T07:42:10Z", "claude-opus-5", 1)];
        let b = active_block(&events, ts("2026-09-04T08:00:00Z")).expect("active");
        assert_eq!(b.start, ts("2026-09-04T07:00:00Z"));
        assert_eq!(b.end, ts("2026-09-04T12:00:00Z"));
    }

    #[test]
    fn a_five_hour_gap_opens_a_new_block_and_the_old_one_is_not_active() {
        let events = [
            event("a", "2026-09-04T01:10:00Z", "claude-opus-5", 100),
            event("b", "2026-09-04T06:20:00Z", "claude-opus-5", 1), // gap 5h10 → new block
        ];
        let b = active_block(&events, ts("2026-09-04T06:30:00Z")).expect("active");
        assert_eq!(b.start, ts("2026-09-04T06:00:00Z"));
        assert_eq!(b.output, 1, "the old block's tokens must not leak in");
    }

    #[test]
    fn an_entry_past_block_end_opens_a_new_block_even_without_a_gap() {
        let events = [
            event("a", "2026-09-04T07:05:00Z", "claude-opus-5", 1),
            event("b", "2026-09-04T11:55:00Z", "claude-opus-5", 1),
            event("c", "2026-09-04T12:03:00Z", "claude-opus-5", 1), // ≥ 07:00+5h → new block
        ];
        let b = active_block(&events, ts("2026-09-04T12:10:00Z")).expect("active");
        assert_eq!(b.start, ts("2026-09-04T12:00:00Z"));
        assert_eq!(b.output, 1);
    }

    #[test]
    fn idle_when_now_is_past_block_end_or_five_hours_after_the_last_entry() {
        let events = [event("a", "2026-09-04T07:05:00Z", "claude-opus-5", 1)];
        assert!(active_block(&events, ts("2026-09-04T12:00:00Z")).is_none());
        assert!(active_block(&[], ts("2026-09-04T12:00:00Z")).is_none());
        let late = [event("a", "2026-09-04T07:59:00Z", "claude-opus-5", 1)];
        assert!(active_block(&late, ts("2026-09-04T13:00:00Z")).is_none());
    }

    #[test]
    fn future_events_are_ignored_and_duplicates_count_once() {
        let events = [
            event("dup", "2026-09-04T07:05:00Z", "claude-opus-5", 10),
            event("dup", "2026-09-04T07:05:00Z", "claude-opus-5", 10),
            event("future", "2026-09-04T09:00:00Z", "claude-opus-5", 999),
        ];
        let snap = reduce_snapshot(&events, "a", ts("2026-09-04T08:00:00Z")).expect("active");
        assert_eq!(snap.output, 10);
    }

    #[test]
    fn reduce_fills_window_burn_and_remaining_minutes() {
        let events = [
            event("a", "2026-09-04T07:00:00Z", "claude-sonnet-5", 600),
            event("b", "2026-09-04T07:10:00Z", "claude-sonnet-5", 600),
        ];
        let snap = reduce_snapshot(&events, "acct", ts("2026-09-04T07:30:00Z")).expect("active");
        let w = snap.window.expect("window");
        assert_eq!(w.remaining_minutes, Some(270));
        // 1220 tokens over 10 minutes.
        assert!((w.tokens_per_minute - 122.0).abs() < 1e-9);
        assert!(w.cost_per_hour > 0.0);
        assert_eq!(snap.account_id, "acct");
        assert_eq!(snap.provider, Provider::Claude);
    }

    // ── scanner ─────────────────────────────────────────────────────────────────────────────────

    fn write(path: &Path, body: &[u8]) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::File::create(path)
            .expect("create")
            .write_all(body)
            .expect("write");
    }

    fn line(at: &str, id: &str, out: u64) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{at}","requestId":"r-{id}","message":{{"id":"{id}","model":"claude-opus-5","usage":{{"output_tokens":{out}}}}}}}"#
        )
    }

    #[tokio::test]
    async fn missing_tree_is_idle_not_an_error() {
        let scanner = TranscriptScanner::new();
        let got = scanner
            .events(Path::new("/nonexistent/projects"), Timestamp::now())
            .await;
        assert!(matches!(got, Ok(None)));
    }

    #[tokio::test]
    async fn walks_nested_dirs_and_memoises_per_tick_across_symlinked_roots() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let real = tmp.path().join("real/projects");
        let now = Timestamp::now();
        let at = (now - Duration::from_secs(60)).to_string();
        write(
            &real.join("slug/s1.jsonl"),
            format!("{}\n", line(&at, "m1", 1)).as_bytes(),
        );
        write(
            &real.join("slug/s1/subagents/a.jsonl"),
            format!("{}\n", line(&at, "m2", 2)).as_bytes(),
        );
        let link = tmp.path().join("acct/projects");
        std::fs::create_dir_all(link.parent().expect("parent")).expect("mkdir");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        let scanner = TranscriptScanner::new();
        let a = scanner.events(&real, now).await.expect("ok").expect("tree");
        let b = scanner.events(&link, now).await.expect("ok").expect("tree");
        assert_eq!(a.len(), 2, "nested subagent transcript walked");
        assert!(
            Arc::ptr_eq(&a, &b),
            "same tick + same canonical dir ⇒ one scan, shared result"
        );
        assert_eq!(
            scanner.dirs.lock().await.len(),
            1,
            "one cache for the shared tree"
        );
    }

    #[tokio::test]
    async fn stale_files_are_pruned_by_mtime_and_changed_files_are_re_read() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("projects");
        let now = Timestamp::now();
        let at = (now - Duration::from_secs(60)).to_string();
        let live = root.join("s/live.jsonl");
        let stale = root.join("s/stale.jsonl");
        write(&live, format!("{}\n", line(&at, "live", 1)).as_bytes());
        write(&stale, line(&at, "stale", 1).as_bytes());
        let old = SystemTime::now() - LOOKBACK - Duration::from_secs(3600);
        std::fs::File::open(&stale)
            .expect("reopen")
            .set_modified(old)
            .expect("backdate mtime");

        let scanner = TranscriptScanner::new();
        let first = scanner.events(&root, now).await.expect("ok").expect("tree");
        assert_eq!(first.len(), 1, "stale file never opened");

        // Append a line: (mtime, len) changes ⇒ re-read on the next tick (a new `now`).
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&live)
            .expect("append");
        writeln!(f, "{}", line(&at, "live2", 1)).expect("write");
        let later = now + Duration::from_secs(1);
        let second = scanner
            .events(&root, later)
            .await
            .expect("ok")
            .expect("tree");
        assert_eq!(second.len(), 2);
    }

    #[tokio::test]
    async fn a_symlinked_subdirectory_is_never_walked() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("projects");
        let outside = tmp.path().join("outside");
        let now = Timestamp::now();
        let at = (now - Duration::from_secs(60)).to_string();
        write(&outside.join("x.jsonl"), line(&at, "x", 1).as_bytes());
        std::fs::create_dir_all(&root).expect("mkdir");
        std::os::unix::fs::symlink(&outside, root.join("escape")).expect("symlink");

        let scanner = TranscriptScanner::new();
        let got = scanner.events(&root, now).await.expect("ok").expect("tree");
        assert!(got.is_empty());
    }

    /// Fresh-scan helper: events of `root` as sorted keys.
    fn scan(root: &Path, files: &mut HashMap<PathBuf, FileEntry>) -> Vec<String> {
        let mut keys: Vec<String> = scan_tree(root, SystemTime::UNIX_EPOCH, files)
            .iter()
            .map(|e| e.key.clone())
            .collect();
        keys.sort();
        keys
    }

    fn append(path: &Path, body: &str) {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .expect("append");
        f.write_all(body.as_bytes()).expect("write");
    }

    #[test]
    fn an_appended_file_parses_only_the_tail_and_equals_a_full_parse() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("projects");
        let p = root.join("s/a.jsonl");
        let at = "2026-09-04T07:00:00Z";
        write(&p, format!("{}\nnot json\n", line(at, "m1", 1)).as_bytes());
        let mut files = HashMap::new();
        assert_eq!(scan(&root, &mut files).len(), 1);
        append(&p, &format!("{}\n", line(at, "m2", 2)));
        let incremental = scan(&root, &mut files);
        assert_eq!(incremental.len(), 2);
        assert_eq!(incremental, scan(&root, &mut HashMap::new()));
    }

    #[test]
    fn a_partial_last_line_waits_for_its_newline() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("projects");
        let p = root.join("s/a.jsonl");
        let at = "2026-09-04T07:00:00Z";
        let full = line(at, "m1", 1);
        write(&p, &full.as_bytes()[..20]);
        let mut files = HashMap::new();
        assert_eq!(scan(&root, &mut files).len(), 0);
        append(&p, &format!("{}\n", &full[20..]));
        assert_eq!(scan(&root, &mut files).len(), 1);
    }

    #[test]
    fn a_truncated_file_is_parsed_again_from_zero() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("projects");
        let p = root.join("s/a.jsonl");
        let at = "2026-09-04T07:00:00Z";
        write(
            &p,
            format!("{}\n{}\n", line(at, "m1", 1), line(at, "m2", 2)).as_bytes(),
        );
        let mut files = HashMap::new();
        assert_eq!(scan(&root, &mut files).len(), 2);
        write(&p, format!("{}\n", line(at, "n1", 3)).as_bytes());
        assert_eq!(scan(&root, &mut files), scan(&root, &mut HashMap::new()));
        assert_eq!(scan(&root, &mut files).len(), 1);
    }

    #[test]
    fn burn_rate_is_zero_for_a_sub_minute_span() {
        let events = [
            event("a", "2026-09-04T07:00:00Z", "claude-sonnet-5", 600),
            event("b", "2026-09-04T07:00:03Z", "claude-sonnet-5", 600),
        ];
        let snap = reduce_snapshot(&events, "acct", ts("2026-09-04T07:30:00Z")).expect("active");
        let w = snap.window.expect("window");
        assert!(w.tokens_per_minute.abs() < f64::EPSILON && w.cost_per_hour.abs() < f64::EPSILON);
    }
}
