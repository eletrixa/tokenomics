//! Pure parse + reduce of Grok Build's `unified.jsonl` per-inference token log (spec 021 §B).
//!
//! Project: Tokenomics — monitor LLM subscription accounts (usage, limits, time-left) in a TUI
//! Module:  src/providers/grok/logs.rs
//! Deps:    jiff, serde_json; domain (UsageSnapshot)
//! Tested:  inline `#[cfg(test)]` below — parse (real fixture) + reduce (in-window sum, mapping)
//!
//! Key responsibilities:
//! - `parse_inference_events`: keep only `shell.turn.inference_done` lines, extract the raw token
//!   counts, skip every other-`msg`/malformed/all-zero line defensively.
//! - `reduce_grok_snapshot`: sum the buckets of events within the trailing 5h of `now`.
//!
//! Design constraints:
//! - No dedup: each `inference_done` (one per agentic `loop_index`) is a distinct billable
//!   inference emitted once — unlike Gemini's event-sourced re-appends (spec 020 §B).
//! - `reasoning_tokens ⊆ completion_tokens` (verified over 617 real events) — reasoning is NOT
//!   added to output; doing so double-counts. `cost_notional`/`window` stay `None` (no honest cost
//!   basis; Grok exposes no subscription-quota block).

use std::time::Duration;

use jiff::Timestamp;
use serde::Deserialize;

use crate::domain::{Provider, UsageSnapshot};

/// The reduction lookback: sum only events from the trailing 5 hours — a scan bound, not a limits
/// claim (mirrors Gemini's/Codex's `LOOKBACK`, spec 021 §B).
const LOOKBACK: Duration = Duration::from_hours(5);

/// One `shell.turn.inference_done` event's raw token counts (pre-bucket-mapping). `reasoning` is
/// informational only — it is a subset of `completion`, never added to output (spec 021 §B).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferenceEvent {
    /// The event's wall-clock time (`ts`), used for the in-window filter.
    pub timestamp: Timestamp,
    /// Total prompt-side tokens (INCLUDES `cached`).
    pub prompt: u64,
    /// Cache-read tokens (subset of `prompt`).
    pub cached: u64,
    /// Completion tokens (INCLUDES `reasoning`).
    pub completion: u64,
    /// Reasoning tokens — a subset of `completion`, informational only.
    pub reasoning: u64,
}

/// Serde shape of one `unified.jsonl` line we care about; everything else deserializes and is
/// filtered out by `msg`. `#[serde(default)]` on `ctx` lets non-token lines (which still have a
/// `msg`) parse without their heterogeneous `ctx` shapes fighting this struct.
#[derive(Debug, Deserialize)]
struct LogLine {
    ts: Timestamp,
    msg: String,
    #[serde(default)]
    ctx: Option<Ctx>,
}

/// The token-bearing `ctx` of a `shell.turn.inference_done` line. Extra fields (`loop_index`,
/// `tokens_per_sec`, timing) are ignored. The `_tokens` suffix on every field mirrors the real
/// JSON keys 1:1 (serde field-name match), so the shared postfix is deliberate, not a smell.
#[derive(Debug, Deserialize)]
#[allow(clippy::struct_field_names)]
struct Ctx {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    cached_prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    reasoning_tokens: u64,
}

/// The `msg` value that marks a per-inference token event.
const INFERENCE_MSG: &str = "shell.turn.inference_done";

/// Parse `unified.jsonl` bytes into token events: keep only `inference_done` lines with a non-zero
/// token count, skipping every other-`msg`, malformed, or all-zero line per-line (never failing the
/// whole file). No dedup — each event is a distinct inference (spec 021 §B).
pub fn parse_inference_events(bytes: &[u8]) -> Vec<InferenceEvent> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Vec::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter_map(parse_line)
        .collect()
}

/// Parse one `unified.jsonl` line into an `InferenceEvent`, or `None` if it isn't a real, non-zero
/// `inference_done` line — never fails the whole file (mirrors Codex/Gemini's per-line `parse_line`
/// idiom).
fn parse_line(line: &str) -> Option<InferenceEvent> {
    let parsed: LogLine = serde_json::from_str(line).ok()?;
    if parsed.msg != INFERENCE_MSG {
        return None;
    }
    let ctx = parsed.ctx?;
    let event = InferenceEvent {
        timestamp: parsed.ts,
        prompt: ctx.prompt_tokens,
        cached: ctx.cached_prompt_tokens,
        completion: ctx.completion_tokens,
        reasoning: ctx.reasoning_tokens,
    };
    // An all-zero token event is not a real turn (a placeholder/aborted inference).
    (event.prompt != 0 || event.completion != 0).then_some(event)
}

/// Reduce parsed events to a normalized snapshot for one account: sum the buckets of events
/// timestamped within the trailing 5h of `now`. Bucket mapping (spec 021 §B):
/// `input = prompt − cached` (saturating), `cache_read = cached`, `output = completion` (reasoning
/// already inside — NEVER added), `cache_creation = 0`, `total_tokens = prompt + completion`.
/// No in-window events ⇒ `None` (idle — the `ProviderAdapter` contract). `cost_notional`/`window`
/// stay `None` (no honest cost basis; Grok exposes no local block).
pub fn reduce_grok_snapshot(
    events: &[InferenceEvent],
    account_id: &str,
    now: Timestamp,
) -> Option<UsageSnapshot> {
    let cutoff = now - LOOKBACK;
    let mut snapshot = UsageSnapshot {
        account_id: account_id.to_string(),
        provider: Provider::Grok,
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
            .saturating_add(event.prompt.saturating_sub(event.cached));
        snapshot.cache_read = snapshot.cache_read.saturating_add(event.cached);
        snapshot.output = snapshot.output.saturating_add(event.completion);
        snapshot.total_tokens = snapshot
            .total_tokens
            .saturating_add(event.prompt.saturating_add(event.completion));
    }
    any.then_some(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().expect("valid timestamp")
    }

    const UNIFIED: &[u8] = include_bytes!("../../../fixtures/grok_unified.jsonl");

    // ── AC2 (spec 021 §B): parse keeps only real inference events ──────────────────────────────

    #[test]
    fn parses_only_inference_events_skipping_other_msg_malformed_and_zero_lines() {
        let events = parse_inference_events(UNIFIED);
        assert_eq!(
            events.len(),
            2,
            "auth, billing, malformed, and all-zero lines must all be skipped: {events:?}"
        );
        assert_eq!(events[0].timestamp, ts("2026-07-19T08:39:27.401Z"));
        assert_eq!(events[0].prompt, 42_316);
        assert_eq!(events[0].cached, 5_504);
        assert_eq!(events[0].completion, 92);
        assert_eq!(events[0].reasoning, 89);
        // second real event is loop_index 2 of the SAME sid — both counted, no dedup.
        assert_eq!(events[1].prompt, 1_000);
        assert_eq!(events[1].completion, 50);
    }

    #[test]
    fn empty_and_nonutf8_input_yield_no_events() {
        assert!(parse_inference_events(b"").is_empty());
        assert!(parse_inference_events(&[0xff, 0xfe, 0x00]).is_empty());
    }

    // ── AC2: reduce — in-window sum, bucket mapping, reasoning NOT added ────────────────────────

    fn event(
        timestamp: &str,
        prompt: u64,
        cached: u64,
        completion: u64,
        reasoning: u64,
    ) -> InferenceEvent {
        InferenceEvent {
            timestamp: ts(timestamp),
            prompt,
            cached,
            completion,
            reasoning,
        }
    }

    #[test]
    fn reduce_sums_in_window_events_with_correct_bucket_mapping() {
        let now = ts("2026-07-19T10:00:00Z");
        let recent = "2026-07-19T09:00:00Z"; // in-window
        let events = vec![
            event(recent, 42_316, 5_504, 92, 89),
            event(recent, 1_000, 200, 50, 40),
        ];
        let snap = reduce_grok_snapshot(&events, "grok-heavy", now)
            .expect("in-window events must yield a snapshot");
        assert_eq!(snap.account_id, "grok-heavy");
        assert_eq!(snap.provider, Provider::Grok);
        // input = (42316-5504) + (1000-200) = 36812 + 800 = 37612
        assert_eq!(snap.input, 37_612);
        assert_eq!(snap.cache_read, 5_704); // 5504 + 200
                                            // output = completion only; reasoning (89, 40) is a subset, never added.
        assert_eq!(snap.output, 142); // 92 + 50
        assert_eq!(snap.cache_creation, 0);
        // total = (42316+92) + (1000+50) = 42408 + 1050 = 43458 = input+cache_read+output
        assert_eq!(snap.total_tokens, 43_458);
        assert_eq!(
            snap.input + snap.cache_read + snap.output,
            snap.total_tokens
        );
        assert_eq!(snap.cost_notional, None);
        assert!(snap.window.is_none());
    }

    #[test]
    fn reduce_drops_out_of_window_events_and_idles_when_none_remain() {
        let now = ts("2026-07-19T10:00:00Z");
        let stale = "2026-07-19T04:00:00Z"; // 6h ago, outside the 5h window
        let events = vec![event(stale, 1_000, 200, 50, 40)];
        assert!(reduce_grok_snapshot(&events, "grok-heavy", now).is_none());
    }

    #[test]
    fn reduce_of_no_events_is_idle() {
        let now = ts("2026-07-19T10:00:00Z");
        assert!(reduce_grok_snapshot(&[], "grok-heavy", now).is_none());
    }
}
