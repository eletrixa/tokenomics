//! ccusage JSON → normalized `UsageSnapshot` + derived session `Limit` (the pure core).
//!
//! Project: Tokenomics — monitor LLM subscription accounts (usage, limits, time-left) in a TUI
//! Module:  src/providers/claude/ccusage.rs
//! Deps:    serde_json, jiff, serde (pure); no I/O
//! Tested:  inline `#[cfg(test)]` on fixtures/blocks_active.json + fixtures/blocks_empty.json
//!
//! Key responsibilities:
//! - `parse_ccusage_blocks`: `ccusage blocks --json` bytes → typed blocks (tolerant of new fields).
//! - `reduce_snapshot`: pick the active block → normalized `UsageSnapshot`.
//! - `derive_session_limit`: the active window → a `Derived` session `Limit` (time-in-window %).
//! - `ccusage_command_spec`: the pure argv builder (sets `CLAUDE_CONFIG_DIR`, explicit argv).
//!
//! Design constraints:
//! - Everything here is pure and deterministic (time is injected), so it is fully table-testable.
//! - Tolerate schema/flag drift: no `deny_unknown_fields`; every field defaults.
//! - The derived session % is TIME-elapsed-in-window (a `Derived` proxy). The authoritative
//!   token % comes only from the overlay (Wave 7). Never present the proxy as authoritative.

use std::path::Path;
use std::time::Duration;

use jiff::Timestamp;
use serde::Deserialize;

use crate::domain::{Limit, LimitKind, Provenance, Provider, Severity, UsageSnapshot, Window};
use crate::error::{AppError, AppResult};
use crate::runner::CommandSpec;

/// How to invoke ccusage. Defaults to a bare `ccusage` on `PATH`; a config override can supply a
/// launcher prefix (e.g. `["npx", "ccusage"]`) for machines without a global install.
#[derive(Debug, Clone)]
pub struct CcusageInvocation {
    /// The program to run (argv[0]).
    pub program: String,
    /// Leading arguments before ccusage's own subcommand (e.g. `["ccusage"]` when program is `npx`).
    pub prefix_args: Vec<String>,
}

impl Default for CcusageInvocation {
    fn default() -> Self {
        Self {
            program: "ccusage".to_string(),
            prefix_args: Vec::new(),
        }
    }
}

impl CcusageInvocation {
    /// Build from an optional config override (`ccusage_cmd`). An empty/absent list ⇒ default.
    /// The first element is the program; the rest are prefix args.
    pub fn from_override(cmd: Option<&[String]>) -> Self {
        match cmd {
            Some([program, prefix @ ..]) => Self {
                program: program.clone(),
                prefix_args: prefix.to_vec(),
            },
            _ => Self::default(),
        }
    }
}

/// Build the `ccusage blocks --json --active` command for one account, with `CLAUDE_CONFIG_DIR`
/// pinned to that account's config dir — the only attribution handle. Pure and testable.
pub fn ccusage_command_spec(
    inv: &CcusageInvocation,
    config_dir: &Path,
    timeout: Duration,
) -> CommandSpec {
    let mut args = inv.prefix_args.clone();
    args.extend(["blocks", "--json", "--active"].map(String::from));
    CommandSpec {
        program: inv.program.clone(),
        args,
        env: vec![(
            "CLAUDE_CONFIG_DIR".to_string(),
            config_dir.display().to_string(),
        )],
        timeout,
    }
}

/// Top-level `ccusage blocks --json` payload.
#[derive(Debug, Deserialize)]
pub struct CcusageOutput {
    /// The 5h usage blocks (empty when the account is idle).
    #[serde(default)]
    pub blocks: Vec<CcusageBlock>,
}

/// One 5h usage block. Unknown fields (entries, models, actualEndTime, isGap, …) are ignored.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CcusageBlock {
    /// Block start (ISO-8601).
    pub start_time: String,
    /// Block end (ISO-8601) — the reset boundary.
    pub end_time: String,
    /// Whether this is the currently-active block.
    #[serde(default)]
    pub is_active: bool,
    /// Notional USD for the block (a usage proxy, never a bill).
    #[serde(default, rename = "costUSD")]
    pub cost_usd: Option<f64>,
    /// All token buckets summed.
    #[serde(default)]
    pub total_tokens: u64,
    /// The individual token buckets.
    #[serde(default)]
    pub token_counts: TokenCounts,
    /// Current burn telemetry.
    #[serde(default)]
    pub burn_rate: Option<BurnRate>,
    /// ccusage's projection for the block.
    #[serde(default)]
    pub projection: Option<Projection>,
}

/// The four token buckets ccusage reports (field names map to ccusage's `tokenCounts` keys).
#[derive(Debug, Default, Deserialize)]
pub struct TokenCounts {
    /// Non-cache input tokens.
    #[serde(default, rename = "inputTokens")]
    pub input: u64,
    /// Output tokens.
    #[serde(default, rename = "outputTokens")]
    pub output: u64,
    /// Cache-creation input tokens.
    #[serde(default, rename = "cacheCreationInputTokens")]
    pub cache_creation: u64,
    /// Cache-read input tokens.
    #[serde(default, rename = "cacheReadInputTokens")]
    pub cache_read: u64,
}

/// Current burn rate for the active block.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BurnRate {
    /// Notional USD per hour.
    #[serde(default)]
    pub cost_per_hour: f64,
    /// Tokens per minute.
    #[serde(default)]
    pub tokens_per_minute: f64,
}

/// ccusage's projection for the active block.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Projection {
    /// Minutes from now until the block's `end_time`.
    #[serde(default)]
    pub remaining_minutes: Option<i64>,
}

/// Parse `ccusage blocks --json` stdout. Tolerant of unknown fields (schema drift).
pub fn parse_ccusage_blocks(bytes: &[u8]) -> AppResult<CcusageOutput> {
    serde_json::from_slice(bytes).map_err(|e| AppError::CcusageParse(e.to_string()))
}

/// Reduce parsed blocks to a normalized snapshot for one account. Returns `None` when the account
/// is idle (no active block). `collected_at` is injected so this stays pure.
pub fn reduce_snapshot(
    output: &CcusageOutput,
    account_id: &str,
    provider: Provider,
    collected_at: Timestamp,
) -> Option<UsageSnapshot> {
    let block = output.blocks.iter().find(|b| b.is_active)?;
    let window = reduce_window(block);
    Some(UsageSnapshot {
        account_id: account_id.to_string(),
        provider,
        collected_at,
        input: block.token_counts.input,
        output: block.token_counts.output,
        cache_read: block.token_counts.cache_read,
        cache_creation: block.token_counts.cache_creation,
        total_tokens: block.total_tokens,
        cost_notional: block.cost_usd,
        window,
    })
}

/// Build the `Window` from a block, when both timestamps parse.
fn reduce_window(block: &CcusageBlock) -> Option<Window> {
    let start = block.start_time.parse::<Timestamp>().ok()?;
    let end = block.end_time.parse::<Timestamp>().ok()?;
    Some(Window {
        start,
        end,
        remaining_minutes: block.projection.as_ref().and_then(|p| p.remaining_minutes),
        tokens_per_minute: block
            .burn_rate
            .as_ref()
            .map_or(0.0, |b| b.tokens_per_minute),
        cost_per_hour: block.burn_rate.as_ref().map_or(0.0, |b| b.cost_per_hour),
    })
}

/// Derive a `Session` limit from the active window: utilization = time-elapsed-in-window %,
/// `resets_at` = window end (verbatim), `source = Derived`. `now` is injected (pure). Returns
/// `None` when there is no window or the window has zero span.
///
/// Severity is ALWAYS `Ok`. This percent is a **clock** — how far through the 5h window we are —
/// not a quota reading. Running it through `severity_for` painted every window yellow past 75% and
/// fired a desktop alert on nothing but elapsed time. A proxy never alarms; the `Derived` provenance
/// is what the surfaces key off to label it as a proxy.
///
/// The i64→f64 millisecond casts are exact for any window shorter than ~285,000 years.
#[allow(clippy::cast_precision_loss)]
pub fn derive_session_limit(snapshot: &UsageSnapshot, now: Timestamp) -> Option<Limit> {
    let window = snapshot.window.as_ref()?;
    let span_ms = window.end.as_millisecond() - window.start.as_millisecond();
    if span_ms <= 0 {
        return None;
    }
    let elapsed_ms = now.as_millisecond() - window.start.as_millisecond();
    let utilization_pct = (elapsed_ms as f64 / span_ms as f64 * 100.0).clamp(0.0, 100.0);
    Some(Limit {
        account_id: snapshot.account_id.clone(),
        provider: snapshot.provider,
        kind: LimitKind::Session,
        scope: None,
        utilization_pct,
        resets_at: window.end.to_string(),
        severity: Severity::Ok,
        source: Provenance::Derived,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Severity;

    const ACTIVE: &[u8] = include_bytes!("../../../fixtures/blocks_active.json");
    const EMPTY: &[u8] = include_bytes!("../../../fixtures/blocks_empty.json");

    fn ts(s: &str) -> Timestamp {
        s.parse().expect("valid timestamp")
    }

    fn active_snapshot() -> UsageSnapshot {
        let out = parse_ccusage_blocks(ACTIVE).expect("fixture parses");
        reduce_snapshot(&out, "acct", Provider::Claude, ts("2026-07-04T10:29:00Z"))
            .expect("active block reduces")
    }

    #[test]
    fn parses_and_reduces_the_active_block() {
        let snap = active_snapshot();
        assert_eq!(snap.account_id, "acct");
        assert_eq!(snap.input, 1_584_660);
        assert_eq!(snap.output, 2_203_186);
        assert_eq!(snap.cache_creation, 9_079_481);
        assert_eq!(snap.cache_read, 231_953_563);
        assert_eq!(snap.total_tokens, 244_820_890);
        // total_tokens == sum of the four buckets (ccusage contract).
        assert_eq!(
            snap.total_tokens,
            snap.input + snap.output + snap.cache_creation + snap.cache_read
        );
        assert_eq!(snap.cost_notional, Some(295.648_566_499_999_84));
    }

    #[test]
    fn reduces_the_window_and_burn() {
        let window = active_snapshot().window.expect("has a window");
        assert_eq!(window.start, ts("2026-07-04T07:00:00Z"));
        assert_eq!(window.end, ts("2026-07-04T12:00:00Z"));
        assert_eq!(window.remaining_minutes, Some(91));
        assert!((window.cost_per_hour - 105.192_964_350_013_1).abs() < 1e-9);
        assert!(window.tokens_per_minute > 1_000_000.0);
    }

    #[test]
    fn empty_blocks_reduce_to_none() {
        let out = parse_ccusage_blocks(EMPTY).expect("empty parses");
        assert!(
            reduce_snapshot(&out, "acct", Provider::Claude, ts("2026-07-04T10:00:00Z")).is_none()
        );
    }

    #[test]
    fn malformed_json_is_a_parse_error() {
        assert!(parse_ccusage_blocks(b"not json").is_err());
    }

    #[test]
    fn unknown_fields_are_tolerated() {
        let json = br#"{"blocks":[{"startTime":"2026-07-04T07:00:00Z","endTime":"2026-07-04T12:00:00Z","isActive":true,"totalTokens":10,"surpriseField":true}]}"#;
        let out = parse_ccusage_blocks(json).expect("tolerates unknown fields");
        let snap = reduce_snapshot(&out, "a", Provider::Claude, ts("2026-07-04T08:00:00Z"))
            .expect("reduces");
        assert_eq!(snap.total_tokens, 10);
    }

    #[test]
    fn derives_session_limit_as_time_in_window() {
        let snap = active_snapshot();
        // 07:00 → 12:00 is 300 min; at 10:00 that is 180/300 = 60%.
        let limit =
            derive_session_limit(&snap, ts("2026-07-04T10:00:00Z")).expect("derives a limit");
        assert_eq!(limit.kind, LimitKind::Session);
        assert_eq!(limit.source, Provenance::Derived);
        assert!((limit.utilization_pct - 60.0).abs() < 1e-6);
        assert_eq!(limit.resets_at, "2026-07-04T12:00:00Z");
        assert_eq!(limit.severity, Severity::Ok);
    }

    #[test]
    fn derived_utilization_clamps_and_never_alarms() {
        let snap = active_snapshot();
        // Past the window end clamps to 100 — but a clock at 100% is still `Ok`: it means the 5h
        // window elapsed, NOT that the quota is spent. Alarming here was the false-warn bug.
        let limit = derive_session_limit(&snap, ts("2026-07-04T13:00:00Z")).expect("derives");
        assert!((limit.utilization_pct - 100.0).abs() < 1e-6);
        assert_eq!(limit.severity, Severity::Ok);
        // Before the window start clamps to 0.
        let before = derive_session_limit(&snap, ts("2026-07-04T06:00:00Z")).expect("derives");
        assert!((before.utilization_pct - 0.0).abs() < 1e-6);
        assert_eq!(before.severity, Severity::Ok);
    }

    #[test]
    fn command_spec_pins_config_dir_and_uses_explicit_argv() {
        let spec = ccusage_command_spec(
            &CcusageInvocation::default(),
            Path::new("/home/x/.claude"),
            Duration::from_secs(30),
        );
        assert_eq!(spec.program, "ccusage");
        assert_eq!(spec.args, ["blocks", "--json", "--active"]);
        assert!(spec
            .env
            .iter()
            .any(|(k, v)| k == "CLAUDE_CONFIG_DIR" && v == "/home/x/.claude"));
    }

    #[test]
    fn command_spec_honors_launcher_prefix() {
        let inv =
            CcusageInvocation::from_override(Some(&["npx".to_string(), "ccusage".to_string()]));
        let spec = ccusage_command_spec(&inv, Path::new("/d"), Duration::from_secs(5));
        assert_eq!(spec.program, "npx");
        assert_eq!(spec.args, ["ccusage", "blocks", "--json", "--active"]);
    }
}
