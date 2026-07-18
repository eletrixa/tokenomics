//! Pure core: Gemini CLI chat-session JSON/JSONL events → a normalized `UsageSnapshot`.
//!
//! Project: Tokenomics — monitor LLM subscription accounts (usage, limits, time-left) in a TUI
//! Module:  src/providers/gemini/chats.rs
//! Deps:    serde_json, jiff (no I/O — bytes in, domain out)
//! Tested:  inline `#[cfg(test)]` on synthetic `.json`/`.jsonl` fixtures (spec 020 §B)
//!
//! Key responsibilities:
//! - `parse_chat_events`: extract timestamped per-turn `tokens` buckets from one session file's
//!   bytes, normalizing BOTH real shapes on read (machine-verified against `~/.gemini/tmp/**`,
//!   see `plans/002-multi-provider/02-gemini.md` §3):
//!   - `.json` is a **session wrapper object** — `{sessionId, projectHash, startTime,
//!     lastUpdated, messages: [...]}` — with each turn's `tokens`/`timestamp` on an element of
//!     `messages` (token-less user-message elements naturally filter out).
//!   - `.jsonl` is **event-sourced**: a header line, `{"$set": {...}}` envelope-patch lines
//!     (skipped explicitly), and the SAME message `id` re-appended on multiple lines as it's
//!     updated (e.g. once with `content` only, again once `tokens` lands) — so every parsed turn
//!     is deduped by `id`, last occurrence wins, before reduction. Undeduped summation
//!     double-counts real sessions (verified: one real file overcounted total_tokens by +62%).
//!
//!   A malformed line/element degrades to whatever parsed, never fails the whole file.
//! - `reduce_gemini_snapshot`: sum in-window deltas into `UsageSnapshot` buckets
//!   (`input = input − cached + tool`, `cache_read = cached`, `output = output + thoughts`,
//!   `cache_creation = 0`, `total_tokens = tokens.total`). `cost_notional` and `window` stay
//!   `None` — no honest basis (same posture as Codex, spec 013 §B).
//!
//! Design constraints:
//! - `tool` folds into the INPUT bucket (tool results are prompt-side context) — an assumption
//!   `[NEEDS CLARIFICATION]` per spec 020 §B until a real `tool > 0` fixture exists; whichever side
//!   it lands on, buckets must sum to `total_tokens` (asserted below).
//! - An all-zero `tokens` object (every bucket 0) is treated as a non-event, not a degenerate
//!   idle-but-present snapshot — a line like `{"tokens":{},...}` must never fabricate a turn.
//! - Time is injected (`now`) so reduction stays pure and deterministic.

use std::time::Duration;

use jiff::Timestamp;
use serde::Deserialize;

use crate::domain::{Provider, UsageSnapshot};

/// The reduction lookback: sum only turns from the trailing 5 hours (mirrors Codex's `LOOKBACK`,
/// spec 013 §B) — a scan bound, not a window claim (`window` stays `None` below).
const LOOKBACK: Duration = Duration::from_hours(5);

/// One chat turn's timestamp plus its `tokens` buckets, already isolated from the session-file
/// envelope and deduped by message id, ready to sum in [`reduce_gemini_snapshot`]. Field names
/// match the observed local shape verbatim (`plans/002-multi-provider/02-gemini.md` §3):
/// `input`/`output`/`cached`/`thoughts`/`tool`/`total`, with the observed invariant `total =
/// input + output + thoughts + tool` and `cached ⊆ input`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChatTurnEvent {
    /// When this turn was recorded.
    pub timestamp: Timestamp,
    /// Raw `tokens.input` (includes `cached`).
    pub input: u64,
    /// Raw `tokens.output`.
    pub output: u64,
    /// Raw `tokens.cached` (a subset of `input`).
    pub cached: u64,
    /// Raw `tokens.thoughts` (reasoning tokens).
    pub thoughts: u64,
    /// Raw `tokens.tool` (tool-result tokens; `0` in every fixture observed so far except the
    /// synthetic `tool > 0` fixture below).
    pub tool: u64,
    /// Raw `tokens.total` — the file's own per-turn sum.
    pub total: u64,
}

/// One session-file turn object's `tokens` sub-object, matching the observed shape verbatim
/// (`plans/002-multi-provider/02-gemini.md` §3). `#[serde(default)]` on each field tolerates any
/// bucket the real CLI omits; the whole `tokens` object is required on [`TurnObject`] below — a
/// turn with no `tokens` at all is not a usable event (spec 020 §B: "no-tokens line" skipped).
#[derive(Debug, Default, Deserialize)]
struct Tokens {
    #[serde(default)]
    input: u64,
    #[serde(default)]
    output: u64,
    #[serde(default)]
    cached: u64,
    #[serde(default)]
    thoughts: u64,
    #[serde(default)]
    tool: u64,
    #[serde(default)]
    total: u64,
}

impl Tokens {
    /// Whether every bucket is zero — a degenerate `{"tokens":{}}` line, not a real turn.
    fn is_all_zero(&self) -> bool {
        self.input == 0
            && self.output == 0
            && self.cached == 0
            && self.thoughts == 0
            && self.tool == 0
            && self.total == 0
    }
}

/// One turn object as written by gemini-cli — either a JSONL event-stream line (once `tokens`
/// lands on it) or one element of a session wrapper's `messages` array. `id` is required and is
/// the dedup key (spec 020 §B: the same id is re-appended across JSONL lines as a message is
/// updated). Unknown fields (e.g. `model`, `content`) are ignored, not rejected.
#[derive(Debug, Deserialize)]
struct TurnObject {
    id: String,
    tokens: Tokens,
    timestamp: String,
}

impl TurnObject {
    /// Convert to a keyed `(id, ChatTurnEvent)`, or `None` if `timestamp` doesn't parse or
    /// `tokens` is all-zero (a degenerate line, not a real turn).
    fn into_keyed_event(self) -> Option<(String, ChatTurnEvent)> {
        if self.tokens.is_all_zero() {
            return None;
        }
        let timestamp: Timestamp = self.timestamp.parse().ok()?;
        let event = ChatTurnEvent {
            timestamp,
            input: self.tokens.input,
            output: self.tokens.output,
            cached: self.tokens.cached,
            thoughts: self.tokens.thoughts,
            tool: self.tokens.tool,
            total: self.tokens.total,
        };
        Some((self.id, event))
    }
}

/// The real `.json` session-file envelope — a wrapper OBJECT, never a bare array
/// (`plans/002-multi-provider/02-gemini.md` §3, verified against 8 real files). `messages` is
/// REQUIRED (no `#[serde(default)]`) so a single bare turn object — a lone `.jsonl` line, which
/// carries `tokens`/`timestamp` but no `messages` key — never accidentally matches this shape
/// with a fabricated empty array; it correctly falls through to per-line `.jsonl` parsing
/// instead. `messages` elements that aren't usable turn objects (e.g. token-less user messages)
/// are filtered out individually below rather than failing the whole document.
#[derive(Debug, Deserialize)]
struct SessionFile {
    messages: Vec<serde_json::Value>,
}

/// Extract chat-turn events from one session file's bytes, normalizing BOTH real shapes on read
/// (spec 020 §B): the `.json` session-wrapper object (`messages: [...]`, each element tried
/// independently as a turn object) or the `.jsonl` event stream (each non-empty line parsed
/// independently, `$set` patch lines skipped explicitly). Either way, events are deduped by
/// message `id` — last occurrence wins, since a `.jsonl` stream re-appends the same id as a
/// message is updated. A line/element that isn't valid JSON, isn't a turn object, carries no
/// `tokens`/unparseable `timestamp`, or is an all-zero `tokens` object is skipped — never fails
/// the file, and entirely malformed bytes yield an empty vec, never an error.
pub fn parse_chat_events(bytes: &[u8]) -> Vec<ChatTurnEvent> {
    let keyed = parse_wrapper_json(bytes).unwrap_or_else(|| parse_jsonl_events(bytes));
    dedup_last_wins(keyed)
}

/// Try the `.json` session-wrapper shape: the whole document as one `SessionFile`, with each
/// `messages` element attempted independently as a `TurnObject` (a non-turn element, e.g. a
/// token-less user message, is filtered out rather than failing the document). `None` if the
/// bytes don't parse as a JSON object at all (a `.jsonl` stream, or malformed bytes) — the caller
/// falls through to per-line parsing.
fn parse_wrapper_json(bytes: &[u8]) -> Option<Vec<(String, ChatTurnEvent)>> {
    let file: SessionFile = serde_json::from_slice(bytes).ok()?;
    Some(
        file.messages
            .into_iter()
            .filter_map(|v| serde_json::from_value::<TurnObject>(v).ok())
            .filter_map(TurnObject::into_keyed_event)
            .collect(),
    )
}

/// Parse the `.jsonl` event-stream shape: each non-empty line independently, `$set` patch lines
/// and any other malformed/non-turn line skipped without failing the file.
fn parse_jsonl_events(bytes: &[u8]) -> Vec<(String, ChatTurnEvent)> {
    bytes
        .split(|&b| b == b'\n')
        .filter(|line| !line.is_empty())
        .filter_map(parse_jsonl_line)
        .collect()
}

/// One `.jsonl` line → a keyed event, or `None` if it's a `$set` envelope-patch line (skipped
/// explicitly, never merely relying on it failing `TurnObject` deserialization), a header line, a
/// malformed line, or a token-less/all-zero line.
fn parse_jsonl_line(line: &[u8]) -> Option<(String, ChatTurnEvent)> {
    // ponytail: one extra parse of an already-small line to name `$set` lines explicitly rather
    // than leaning on coincidental deserialization failure; session lines are tiny, not a hot path.
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) {
        if value.get("$set").is_some() {
            return None;
        }
    }
    serde_json::from_slice::<TurnObject>(line)
        .ok()
        .and_then(TurnObject::into_keyed_event)
}

/// Dedup keyed events by `id`, last occurrence wins (a `.jsonl` stream re-appends the same id as
/// a message is updated — spec 020 §B), preserving each id's FIRST-seen position so output order
/// stays stable and deterministic (no dependency on `HashMap` iteration order).
fn dedup_last_wins(keyed: Vec<(String, ChatTurnEvent)>) -> Vec<ChatTurnEvent> {
    let mut order: Vec<String> = Vec::new();
    let mut by_id: std::collections::HashMap<String, ChatTurnEvent> =
        std::collections::HashMap::new();
    for (id, event) in keyed {
        if !by_id.contains_key(&id) {
            order.push(id.clone());
        }
        by_id.insert(id, event);
    }
    order
        .into_iter()
        .filter_map(|id| by_id.remove(&id))
        .collect()
}

/// Reduce parsed (already-deduped) events to a normalized snapshot for one account: sum the
/// buckets of events timestamped within the trailing 5h of `now`. Bucket mapping (spec 020 §B):
/// `input = input − cached + tool` (the `input − cached` term floored at 0 via saturating sub
/// before the `tool` term is added — `tool` folds into the input bucket per spec), `cache_read =
/// cached`, `output = output + thoughts`, `cache_creation = 0`, `total_tokens` = sum of each
/// event's raw `total`. No in-window events ⇒ `None` (idle — the `ProviderAdapter` contract).
/// `cost_notional`/`window` stay `None` (no honest cost basis; Gemini exposes no local block).
pub fn reduce_gemini_snapshot(
    events: &[ChatTurnEvent],
    account_id: &str,
    now: Timestamp,
) -> Option<UsageSnapshot> {
    let cutoff = now - LOOKBACK;
    let mut snapshot = UsageSnapshot {
        account_id: account_id.to_string(),
        provider: Provider::Gemini,
        collected_at: now,
        input: 0,
        output: 0,
        cache_read: 0,
        cache_creation: 0,
        total_tokens: 0,
        cost_notional: None,
        window: None,
    };
    let mut any = false;
    for event in events.iter().filter(|e| e.timestamp >= cutoff) {
        any = true;
        snapshot.input = snapshot
            .input
            .saturating_add(event.input.saturating_sub(event.cached))
            .saturating_add(event.tool);
        snapshot.cache_read = snapshot.cache_read.saturating_add(event.cached);
        snapshot.output = snapshot
            .output
            .saturating_add(event.output)
            .saturating_add(event.thoughts);
        snapshot.total_tokens = snapshot.total_tokens.saturating_add(event.total);
    }
    any.then_some(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Provider;

    fn ts(s: &str) -> Timestamp {
        s.parse().expect("valid timestamp")
    }

    fn event(
        timestamp: &str,
        input: u64,
        output: u64,
        cached: u64,
        thoughts: u64,
        tool: u64,
        total: u64,
    ) -> ChatTurnEvent {
        ChatTurnEvent {
            timestamp: ts(timestamp),
            input,
            output,
            cached,
            thoughts,
            tool,
            total,
        }
    }

    // ── AC2 (spec 020 §B): parse — both file shapes, malformed-line/file skipping ──────────────

    const JSONL: &[u8] = include_bytes!("../../../fixtures/gemini_chats.jsonl");
    const JSON: &[u8] = include_bytes!("../../../fixtures/gemini_chats.json");
    const JSONL_TOOL: &[u8] = include_bytes!("../../../fixtures/gemini_chats_tool.jsonl");
    const JSONL_DEDUP: &[u8] = include_bytes!("../../../fixtures/gemini_chats_dedup.jsonl");

    #[test]
    fn parses_well_formed_jsonl_skipping_header_set_malformed_and_no_tokens_lines() {
        let events = parse_chat_events(JSONL);
        assert_eq!(
            events.len(),
            2,
            "the header, $set, malformed, and no-tokens lines must all be skipped: {events:?}"
        );
        assert_eq!(events[0].timestamp, ts("2026-07-08T12:56:22.581Z"));
        assert_eq!(events[0].input, 10_000);
        assert_eq!(events[0].output, 300);
        assert_eq!(events[0].cached, 8_000);
        assert_eq!(events[0].thoughts, 800);
        assert_eq!(events[0].tool, 0);
        assert_eq!(events[0].total, 11_100);

        assert_eq!(events[1].timestamp, ts("2026-07-08T13:10:00.000Z"));
        assert_eq!(events[1].total, 5_000);
    }

    #[test]
    fn parses_the_real_session_wrapper_json_shape() {
        let events = parse_chat_events(JSON);
        assert_eq!(
            events.len(),
            2,
            "the token-less user message must be filtered, both gemini turns parse: {events:?}"
        );
        assert_eq!(events[0].timestamp, ts("2026-07-09T09:00:00.000Z"));
        assert_eq!(events[0].total, 2_000);
        assert_eq!(events[1].timestamp, ts("2026-07-09T09:05:00.000Z"));
        assert_eq!(events[1].total, 3_500);
    }

    #[test]
    fn tool_greater_than_zero_fixture_parses_and_bucket_sum_holds() {
        let events = parse_chat_events(JSONL_TOOL);
        assert_eq!(events.len(), 1, "{events:?}");
        let e = events[0];
        assert!(
            e.tool > 0,
            "this fixture exists specifically to cover tool > 0"
        );
        // Observed invariant: total = input + output + thoughts + tool.
        assert_eq!(e.total, e.input + e.output + e.thoughts + e.tool);
    }

    #[test]
    fn a_duplicate_message_id_re_appended_across_jsonl_lines_counts_once_last_wins() {
        // Real .jsonl files are event-sourced: the same message `id` is re-appended as it's
        // updated (verified against a real session — see module doc). Undeduped summation
        // double-counts; this fixture asserts one turn survives, holding the LAST values.
        let events = parse_chat_events(JSONL_DEDUP);
        assert_eq!(
            events.len(),
            1,
            "duplicate id must collapse to one turn: {events:?}"
        );
        assert_eq!(events[0].timestamp, ts("2026-07-08T12:00:05.000Z"));
        assert_eq!(
            events[0].total, 1_350,
            "the LATER re-append's values must win"
        );
    }

    #[test]
    fn empty_bytes_parse_to_no_events() {
        assert!(parse_chat_events(b"").is_empty());
    }

    #[test]
    fn entirely_malformed_bytes_parse_to_no_events_not_an_error() {
        assert!(parse_chat_events(b"not json at all\nneither is this").is_empty());
    }

    #[test]
    fn an_all_zero_tokens_line_is_not_a_fabricated_event() {
        let line = br#"{"id":"turn-zero","tokens":{},"timestamp":"2026-07-08T12:00:00.000Z"}"#;
        assert!(
            parse_chat_events(line).is_empty(),
            "an all-zero tokens object must not produce a degenerate turn"
        );
    }

    #[test]
    fn no_events_at_all_reduce_to_none() {
        assert!(reduce_gemini_snapshot(&[], "acct", ts("2026-07-08T13:00:00Z")).is_none());
    }

    // ── AC2: reduce — in-window filtering + the documented bucket mapping ──────────────────────

    #[test]
    fn reduce_sums_only_in_window_deltas_with_the_documented_bucket_mapping() {
        let now = ts("2026-07-08T18:00:00Z");
        // Cutoff is now - 5h = 2026-07-08T13:00:00Z. Values here are invented, not real usage.
        let events = vec![
            event("2026-07-08T12:59:59Z", 500, 100, 200, 10, 0, 610), // just before cutoff: excluded
            event("2026-07-08T13:00:00Z", 9_240, 410, 6_650, 730, 0, 10_380), // at cutoff: included
            event("2026-07-08T15:00:00Z", 2_000, 500, 100, 0, 0, 2_500), // well within window
        ];

        let snap = reduce_gemini_snapshot(&events, "gemini-personal", now)
            .expect("in-window events present");
        assert_eq!(snap.account_id, "gemini-personal");
        assert_eq!(snap.provider, Provider::Gemini);
        assert_eq!(snap.collected_at, now);
        // input = (9240-6650) + (2000-100) = 2590 + 1900 = 4490 (tool=0 both events)
        assert_eq!(snap.input, 4_490);
        assert_eq!(snap.cache_read, 6_750); // 6650 + 100
        assert_eq!(snap.cache_creation, 0);
        assert_eq!(snap.output, 1_640); // (410+730) + (500+0)
        assert_eq!(snap.total_tokens, 12_880); // 10380 + 2500
        assert_eq!(
            snap.cost_notional, None,
            "no public subscription pricing basis"
        );
        assert!(
            snap.window.is_none(),
            "the 5h lookback is a scan bound, not a window claim"
        );
    }

    #[test]
    fn no_in_window_events_reduce_to_none() {
        let now = ts("2026-07-08T18:00:00Z");
        let events = vec![event("2026-07-08T10:00:00Z", 500, 100, 200, 10, 0, 810)];
        assert!(reduce_gemini_snapshot(&events, "acct", now).is_none());
    }

    #[test]
    fn buckets_sum_to_total_tokens_including_a_tool_greater_than_zero_event() {
        // spec 020 §B: regardless of where `tool` is folded in, buckets must sum to total_tokens.
        let now = ts("2026-07-08T13:00:00Z");
        let events = vec![event(
            "2026-07-08T12:00:00Z",
            1_000, // input (includes cached)
            50,    // output
            200,   // cached
            30,    // thoughts
            75,    // tool > 0
            1_155, // total = 1000 + 50 + 30 + 75
        )];
        let snap = reduce_gemini_snapshot(&events, "acct", now).expect("in-window event present");
        assert_eq!(
            snap.input + snap.output + snap.cache_read + snap.cache_creation,
            snap.total_tokens,
            "buckets must sum to total_tokens regardless of tool placement"
        );
        // Pin the chosen mapping (spec 020 §B: tool folds into the INPUT bucket).
        assert_eq!(snap.input, 1_000 - 200 + 75); // 875
        assert_eq!(snap.output, 50 + 30); // 80
        assert_eq!(snap.cache_read, 200);
        assert_eq!(snap.total_tokens, 1_155);
    }
}
