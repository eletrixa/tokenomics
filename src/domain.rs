//! Core domain contracts shared across the app (the extensibility spine).
//!
//! Project: Tokenomics — monitor LLM subscription accounts (usage, limits, time-left) in a TUI
//! Module:  src/domain.rs
//! Deps:    serde
//! Tested:  via config.rs (Account/Provider parsing); UsageSnapshot via providers/*/ reducers
//!          (claude/ccusage, codex/sessions); Limit via providers/*/ (overlay, codex/rate_limits)
//!
//! Key responsibilities:
//! - `Provider`: the exhaustive provider enum (Claude, Codex, Zai now; Gemini/Grok slot in later).
//! - `Account`: one monitored subscription account. Attribution is `config_dir`
//!   (`CLAUDE_CONFIG_DIR`/`CODEX_HOME`) for claude/codex, or `api_key_env` for zai (spec 019 §A) —
//!   exactly one is the identity handle per provider, enforced by `config::validate`.
//! - `UsageSnapshot` / `Window`: one account's normalized token usage + active 5h window.
//! - `Limit` / `LimitKind` / `Severity` / `Provenance`: a normalized utilization limit + its badges.
//!
//! Design constraints:
//! - `Provider` stays a compiler-checked enum so adding a provider is an exhaustive change.
//! - Attribution is `config_dir` or `api_key_env`, never the logs (logs carry no account identity).
//! - `cost_notional` is a labeled proxy, NEVER a bill; limits are `utilization_pct` + `resets_at`,
//!   never "X of Y". `resets_at` is a string rendered verbatim from its source.

use std::path::PathBuf;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

/// The LLM subscription provider behind an account. Extend this enum to add a provider;
/// the compiler then flags every place that must handle the new variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    /// Anthropic Claude (Max/Pro subscription).
    Claude,
    /// OpenAI Codex (ChatGPT subscription; spec 013).
    Codex,
    /// z.ai GLM coding plan — limits-only, API-key attributed (spec 019).
    Zai,
    // Future: Gemini, Grok — add a variant + a providers/<x>/ adapter.
}

impl Provider {
    /// The stable lowercase identifier used in config, the store, and display.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Zai => "zai",
        }
    }

    /// Parse the stable identifier back into a `Provider` (e.g. when reading a stored row).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "zai" => Some(Self::Zai),
            _ => None,
        }
    }
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One monitored subscription account. `config_dir` is the `CLAUDE_CONFIG_DIR` / `CODEX_HOME` used
/// both to run the local usage lane and to read credentials — required for `claude`/`codex`, but
/// `zai` has no directory-scoped login (its identity is `api_key_env` instead), so the field is
/// optional and per-provider validation (`config::validate`) enforces which one is required (spec
/// 019 §A).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Account {
    /// Stable, unique key (e.g. `"claude-work"`).
    pub id: String,
    /// Human display name.
    pub label: String,
    /// The provider behind this account.
    pub provider: Provider,
    /// The account's config dir (`CLAUDE_CONFIG_DIR` / `CODEX_HOME`; tilde-expanded at parse time).
    /// Required for `claude`/`codex`; optional (accepted but unused) for `zai` (spec 019 §A).
    #[serde(default)]
    pub config_dir: Option<PathBuf>,
    /// The env-var NAME (never the value) holding the z.ai API key. Required for `zai`, rejected
    /// for `claude`/`codex` (spec 019 §A). Never logged, printed, or stored — only the name is.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// False marks an unsubscribed/paused account: unmonitored, hidden by default (spec 014).
    #[serde(default = "active_default")]
    pub active: bool,
    /// Optional panel accent color (a named ratatui color or `#rrggbb`).
    #[serde(default)]
    pub color: Option<String>,
    /// Opt-in to the authoritative limits overlay for this account (`/api/oauth/usage` for Claude,
    /// `codex app-server` for Codex, the z.ai quota endpoint for zai).
    #[serde(default)]
    pub limits_overlay: bool,
}

/// Serde default for [`Account::active`] — absent means active (existing configs unchanged).
fn active_default() -> bool {
    true
}

/// One account's active 5-hour usage block: absolute bounds plus burn telemetry.
///
/// `start`/`end` are the block's boundaries; `remaining_minutes` is minutes from "now" until
/// `end` (as ccusage projects it); burn fields are informational. All from local ccusage — the
/// ToS-safe plane; no network, no identity beyond the account we ran it under.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Window {
    /// Block start (the 5h window opened here).
    pub start: Timestamp,
    /// Block end (the window resets here — the source of a derived `resets_at`).
    pub end: Timestamp,
    /// Minutes remaining until `end`, when ccusage projects it.
    pub remaining_minutes: Option<i64>,
    /// Current burn rate in tokens/minute (informational).
    pub tokens_per_minute: f64,
    /// Current burn rate in notional USD/hour (informational; a proxy, never a bill).
    pub cost_per_hour: f64,
}

/// One account's normalized token usage for its active window. `total_tokens` already sums the
/// four token buckets (ccusage's `totalTokens`). `cost_notional` is a labeled proxy, never a bill.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UsageSnapshot {
    /// The account this snapshot belongs to (attribution is the config dir we ran under).
    pub account_id: String,
    /// The provider behind the account.
    pub provider: Provider,
    /// When this snapshot was reduced (injected, so reduction stays pure/testable).
    pub collected_at: Timestamp,
    /// Non-cache input tokens.
    pub input: u64,
    /// Output tokens.
    pub output: u64,
    /// Cache-read input tokens.
    pub cache_read: u64,
    /// Cache-creation input tokens.
    pub cache_creation: u64,
    /// All four buckets summed (ccusage's `totalTokens`).
    pub total_tokens: u64,
    /// Notional USD for the window — a labeled usage proxy, NEVER a bill.
    pub cost_notional: Option<f64>,
    /// The active 5h window, when a block is active.
    pub window: Option<Window>,
}

/// Where a limit's numbers came from — rendered as a UI badge so the plane is never conflated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Provenance {
    /// From the account's own `/api/oauth/usage` overlay (opt-in).
    Authoritative,
    /// Computed locally from ccusage (e.g. time-elapsed-in-window).
    Derived,
    /// A coarse guess (last-resort fallback).
    // Reserved for a future coarse fallback; matched everywhere but not yet constructed.
    #[allow(dead_code)]
    Estimate,
}

impl Provenance {
    /// The stable lowercase identifier used in the store and display.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Authoritative => "authoritative",
            Self::Derived => "derived",
            Self::Estimate => "estimate",
        }
    }

    /// Parse the stable identifier back into a `Provenance` (e.g. when reading a stored row).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "authoritative" => Some(Self::Authoritative),
            "derived" => Some(Self::Derived),
            "estimate" => Some(Self::Estimate),
            _ => None,
        }
    }
}

/// Which limit a `Limit` describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitKind {
    /// The rolling 5-hour session limit.
    Session,
    /// The weekly all-usage limit (from the overlay).
    WeeklyAll,
    /// A weekly scoped limit, e.g. a single model family (from the overlay).
    WeeklyScoped,
}

impl LimitKind {
    /// The stable snake_case identifier used in the store.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::WeeklyAll => "weekly_all",
            Self::WeeklyScoped => "weekly_scoped",
        }
    }

    /// Parse the stable identifier back into a `LimitKind` (e.g. when reading a stored row).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "session" => Some(Self::Session),
            "weekly_all" => Some(Self::WeeklyAll),
            "weekly_scoped" => Some(Self::WeeklyScoped),
            _ => None,
        }
    }
}

/// Severity tier for a utilization %, classified against the configured thresholds.
/// The classifier itself lives in `format::severity_for` (the single presentation entry point).
/// `Ord` follows declaration order (`Ok < Warn < Crit`) so the worst of several limits is `.max()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Below the warn threshold.
    Ok,
    /// At or above warn, below crit.
    Warn,
    /// At or above crit.
    Crit,
}

impl Severity {
    /// The stable lowercase identifier used in the store and display.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Crit => "crit",
        }
    }

    /// Parse the stable identifier back into a `Severity` (e.g. when reading a stored row).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ok" => Some(Self::Ok),
            "warn" => Some(Self::Warn),
            "crit" => Some(Self::Crit),
            _ => None,
        }
    }
}

/// A normalized limit: **% utilization + when it resets**, never "X of Y". `resets_at` is a string
/// rendered verbatim from its source; `source` tags which plane produced the number.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Limit {
    /// The account this limit belongs to.
    pub account_id: String,
    /// The provider behind the account.
    pub provider: Provider,
    /// Which limit this is.
    pub kind: LimitKind,
    /// Optional scope label (e.g. a model family) for scoped weekly limits.
    pub scope: Option<String>,
    /// Utilization on 0–100. For a derived session limit this is time-elapsed-in-window.
    pub utilization_pct: f64,
    /// When the limit resets — rendered verbatim (never reformatted away from the source).
    pub resets_at: String,
    /// Severity tier for `utilization_pct` against the configured thresholds.
    pub severity: Severity,
    /// Which plane produced this limit.
    pub source: Provenance,
}
