//! Pure rendering: lay the precomputed `App` rows out responsively across three density tiers.
//!
//! Project: Tokenomics — monitor LLM subscription accounts (usage, limits, time-left) in a TUI
//! Module:  src/tui/view.rs
//! Deps:    ratatui (Layout, Block, Sparkline, Paragraph, symbols)
//! Tested:  inline `#[cfg(test)]` (TestBackend buffer assertions + insta snapshots at several sizes);
//!          the ledger clause's tier degrade order (`pick_full_clause`, `compact_header_line`,
//!          `micro_line`) and the fleet header's ledger token (spec 017 §D acceptance 5); the
//!          verified pill's own degrade step + dim-green span styling (`split_pill`,
//!          `full_title_spans`) and its absence from MICRO (spec 018 §C acceptance 3/4)
//!
//! Design constraints:
//! - `render` takes `&App` only; every value it needs is precomputed on `App` (no I/O, no compute).
//! - Colour is already resolved on the rows; the view applies it and never depends on colour for
//!   meaning — severity is a glyph+word pair, bars distinguish fill by CHARACTER (`█`/`▏…▉` vs `░`),
//!   and selection is structural (double border / thick spine), so `NO_COLOR` loses nothing.
//! - One invariant row grammar (marker · severity glyph · proportional bar · percent · verbatim
//!   reset) is held across FULL / COMPACT / MICRO so the eye never re-learns where to look. As the
//!   terminal shrinks we DROP to a denser tier rather than squeeze a bordered panel to empty.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Sparkline};
use ratatui::Frame;

use crate::domain::Severity;
use crate::format::severity_label;

use super::model::{resolve_color, severity_glyph, AccountView, App, GaugeView, SubView};

/// Bordered panel height: border (2) + 2 inner lines (5h · weekly). Token/cost/burn moved to the
/// fleet header line, so the panel no longer carries a per-account meta row.
const PANEL_HEIGHT: u16 = 4;
/// A scoped-weekly gauge, when present, adds one inner line.
const PANEL_HEIGHT_SCOPED: u16 = 5;
/// Rows a COMPACT account occupies (header · 5h · weekly).
const COMPACT_ROWS: u16 = 3;
/// Smallest legible proportional bar.
const MIN_BAR: u16 = 6;
/// Cells reserved for the right-hand label in a FULL/roomy gauge row — wide enough for the longest
/// scoped-weekly label (`"Fable 92% crit · resets in 5d 9h"`) so it never truncates.
const RIGHT_ZONE_FULL: u16 = 36;
/// Fixed width of the percent column in the MICRO tier (fits `"100%"`).
const PCT_W: usize = 4;
/// Sub-cell eighth blocks, 1/8‥7/8, for smooth bar ends (full cell is `█`).
const EIGHTHS: [char; 7] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉'];

/// The responsive density tier. `Ord` runs least→most dense so `a.min(b)` picks the more compact of
/// the vertical and horizontal verdicts (the safe choice — it always fits).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Tier {
    /// One aligned line per account — survives the smallest windows.
    Micro,
    /// Borderless, three lines per account, grouped by a left accent spine.
    Compact,
    /// The bordered "celebration" panel for a roomy terminal.
    Full,
}

/// Height of one account's FULL panel — one taller when it carries a scoped-weekly gauge.
fn full_panel_height(row: &AccountView) -> u16 {
    if row.weekly_scoped.is_some() {
        PANEL_HEIGHT_SCOPED
    } else {
        PANEL_HEIGHT
    }
}

/// Pick the tier that fits `area`: the richest tier whose height fits, capped by width. Taking the
/// less-dense of the two verdicts guarantees the result fits both dimensions.
fn choose_tier(area: Rect, rows: &[AccountView]) -> Tier {
    let full_needed: u16 = rows.iter().map(full_panel_height).sum();
    let n = u16::try_from(rows.len()).unwrap_or(u16::MAX);
    let compact_needed = n.saturating_mul(COMPACT_ROWS);
    let by_height = if full_needed <= area.height {
        Tier::Full
    } else if compact_needed <= area.height {
        Tier::Compact
    } else {
        Tier::Micro
    };
    let by_width = if area.width >= 72 {
        Tier::Full
    } else if area.width >= 40 {
        Tier::Compact
    } else {
        Tier::Micro
    };
    by_height.min(by_width)
}

/// Render the whole dashboard. Pure over `&App`.
pub fn render(f: &mut Frame<'_>, app: &App) {
    let area = f.area();
    let alerts = app.alert_count();
    // Collector-down is the highest-priority chrome: if the writer stopped, every number is frozen,
    // so it gets its own row right under the title whenever there's any vertical room (spec 011 §A).
    let show_collector_alert = app.collector_alert.is_some() && !app.show_help && area.height >= 2;
    // A standalone banner row needs height to spare; below that it folds into the title's right edge.
    let banner_standalone = alerts > 0 && area.height >= 11 && !app.show_help;
    let show_footer = area.height >= 3;
    // The fleet usage line (shared token/cost/burn, once) shows whenever there's a spare row and data.
    let show_fleet = app.fleet.is_some() && !app.show_help && area.height >= 6;
    // The fleet-wide burn bar needs its own row and only earns it in a roomy window with data.
    let show_agg = !app.aggregate_burn.is_empty() && !app.show_help && area.height >= 8;

    let mut constraints = vec![Constraint::Length(1)]; // title
    if show_collector_alert {
        constraints.push(Constraint::Length(1));
    }
    if banner_standalone {
        constraints.push(Constraint::Length(1));
    }
    if show_fleet {
        constraints.push(Constraint::Length(1));
    }
    if show_agg {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Min(1)); // body
    if show_footer {
        constraints.push(Constraint::Length(1));
    }
    let chunks = Layout::vertical(constraints).split(area);

    let mut idx = 0;
    let fold_badge = alerts > 0 && !banner_standalone && !app.show_help;
    render_title(f, app, chunks[idx], fold_badge);
    idx += 1;
    if show_collector_alert {
        render_collector_alert(f, app, chunks[idx]);
        idx += 1;
    }
    if banner_standalone {
        render_banner(f, app, chunks[idx]);
        idx += 1;
    }
    if show_fleet {
        render_fleet(f, app, chunks[idx]);
        idx += 1;
    }
    if show_agg {
        render_agg_bar(f, app, chunks[idx]);
        idx += 1;
    }
    let body = chunks[idx];
    idx += 1;
    if app.show_help {
        render_help(f, body);
    } else {
        render_body(f, app, body);
    }
    if show_footer {
        render_footer(f, app, chunks[idx]);
    }
}

fn render_body(f: &mut Frame<'_>, app: &App, area: Rect) {
    if app.rows.is_empty() {
        f.render_widget(
            Paragraph::new("collecting… (start `tok collector`, or wait for the first tick)")
                .style(Style::new().fg(resolve_color(app.use_color, Color::DarkGray))),
            area,
        );
        return;
    }
    match choose_tier(area, &app.rows) {
        Tier::Full => render_full(f, app, area),
        Tier::Compact => render_compact(f, app, area),
        Tier::Micro => render_micro(f, app, area),
    }
}

// ── Chrome: title · banner · footer (all width-degraded) ────────────────────────────────────────

fn render_title(f: &mut Frame<'_>, app: &App, area: Rect, fold_badge: bool) {
    let dim = Style::new().fg(resolve_color(app.use_color, Color::DarkGray));
    let mut spans = vec![Span::styled(
        "TOKENOMICS",
        Style::new()
            .fg(resolve_color(app.use_color, Color::Cyan))
            .add_modifier(Modifier::BOLD),
    )];
    let n = app.account_count;
    let sub = if area.width >= 100 {
        format!("  ·  {n} account(s)  ·  cost is notional (usage proxy, not a bill)")
    } else if area.width >= 72 {
        format!("  ·  {n} acct  ·  cost notional (proxy, not a bill)")
    } else if area.width >= 48 {
        format!("  ·  {n} acct  ·  $ = notional")
    } else {
        format!("  ·  {n} acct · notional")
    };
    spans.push(Span::styled(sub, dim));
    if let Some(message) = &app.message {
        spans.push(Span::styled(
            format!("  ·  {message}"),
            Style::new().fg(resolve_color(app.use_color, Color::Green)),
        ));
    }
    if fold_badge {
        let count = app.alert_count();
        spans.push(Span::styled(
            format!("  ·  ▲{count} over warn"),
            Style::new()
                .fg(resolve_color(app.use_color, Color::Yellow))
                .add_modifier(Modifier::BOLD),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_banner(f: &mut Frame<'_>, app: &App, area: Rect) {
    let count = app.alert_count();
    let base = format!("▲  {count} account(s) at or above the warn threshold");
    let text = if area.width >= 90 {
        worst(&app.rows)
            .and_then(|w| w.headline.as_ref().map(|g| (w, g)))
            .map_or_else(
                || base.clone(),
                // The gauge label carries the full story — scope (e.g. "Fable"), percent, severity,
                // and the reset with the correct verb rule — so a scoped weekly names its model.
                |(w, g)| format!("{base}  ·  worst: {} {}", short_name(&w.title), g.label()),
            )
    } else if area.width >= 56 {
        base
    } else {
        format!("▲ {count} over warn")
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            text,
            Style::new()
                .fg(resolve_color(app.use_color, Color::Yellow))
                .add_modifier(Modifier::BOLD),
        ))),
        area,
    );
}

/// The collector-liveness banner (writer down/stalled): a loud red line so a frozen board is never
/// mistaken for a live one. Width-degrades to a short form; the colour is backed by the `⚠` glyph so
/// it survives `NO_COLOR`.
fn render_collector_alert(f: &mut Frame<'_>, app: &App, area: Rect) {
    let Some(alert) = &app.collector_alert else {
        return;
    };
    let full = format!("⚠ {alert}");
    let text = if u16::try_from(full.chars().count()).unwrap_or(u16::MAX) <= area.width {
        full
    } else {
        "⚠ collector down".to_string()
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            text,
            Style::new()
                .fg(resolve_color(app.use_color, Color::Red))
                .add_modifier(Modifier::BOLD),
        ))),
        area,
    );
}

fn render_footer(f: &mut Frame<'_>, app: &App, area: Rect) {
    let text = if area.width >= 62 {
        "↑/↓ (or j/k) move · r refresh · i inactive · ? help · q quit"
    } else if area.width >= 53 {
        "↑/↓ move · r refresh · i inactive · ? help · q quit"
    } else if area.width >= 40 {
        "↑↓ move · i inactive · ? help · q quit"
    } else if area.width >= 25 {
        "↑↓ move · ? help · q quit"
    } else {
        "↑↓ · q quit"
    };
    f.render_widget(
        Paragraph::new(text).style(Style::new().fg(resolve_color(app.use_color, Color::DarkGray))),
        area,
    );
}

// ── FULL tier: bordered panels (the roomy design), now with visible eighth-block bars ────────────

fn render_full(f: &mut Frame<'_>, app: &App, area: Rect) {
    let heights: Vec<Constraint> = app
        .rows
        .iter()
        .map(|r| Constraint::Length(full_panel_height(r)))
        .collect();
    let slots = Layout::vertical(heights).split(area);
    for (i, (row, slot)) in app.rows.iter().zip(slots.iter()).enumerate() {
        render_full_panel(f, app, row, *slot, i == app.selected);
    }
}

fn render_full_panel(f: &mut Frame<'_>, app: &App, row: &AccountView, area: Rect, selected: bool) {
    let accent = if selected {
        row.accent
    } else {
        resolve_color(app.use_color, Color::DarkGray)
    };
    // Selection is structural (survives NO_COLOR): a double-line border swaps every border glyph,
    // inner verticals included, plus a `▶ ` caret in the title.
    let border_set = if selected {
        symbols::border::DOUBLE
    } else {
        symbols::border::PLAIN
    };
    let marker = if selected { "▶ " } else { "" };
    // The border's own title-text budget: `area.width` minus the two corner cells (ratatui's
    // `Block::titles_area` reserves exactly one column per side) — see `full_title_spans`.
    let title_spans = full_title_spans(
        &row.title,
        marker,
        &row.sub,
        area.width.saturating_sub(2),
        app.use_color,
    );
    let block = Block::bordered()
        .border_set(border_set)
        .border_style(Style::new().fg(accent))
        .title(Line::from(title_spans));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let has_scoped = row.weekly_scoped.is_some();
    let line_count = if has_scoped { 3 } else { 2 };
    if usize::from(inner.height) < line_count || inner.width == 0 {
        return; // too squeezed to lay out every line; the border alone reads
    }
    let lines = Layout::vertical(vec![Constraint::Length(1); line_count]).split(inner);
    let bar_w = inner.width.saturating_sub(4 + RIGHT_ZONE_FULL).max(MIN_BAR);

    let status = row.status.as_deref().unwrap_or("collecting…");
    f.render_widget(
        Paragraph::new(gauge_line(
            app,
            "5h",
            row.session.as_ref(),
            status,
            bar_w,
            false,
        )),
        lines[0],
    );
    f.render_widget(
        Paragraph::new(gauge_line(
            app,
            "wk",
            row.weekly.as_ref(),
            row.weekly_hint.as_str(),
            bar_w,
            false,
        )),
        lines[1],
    );
    if has_scoped {
        f.render_widget(
            Paragraph::new(gauge_line(
                app,
                "wk",
                row.weekly_scoped.as_ref(),
                "n/a",
                bar_w,
                false,
            )),
            lines[2],
        );
    }
}

/// The header aggregate burn bar: a fixed `burn · all accts` label + a `Sparkline` of `Σ burn_tpm`
/// per tick across all accounts. A shape signal only — no number is printed (the summed rate is a
/// cache-inclusive notional proxy). Falls back to just the label when there's no width for the bar.
fn render_agg_bar(f: &mut Frame<'_>, app: &App, area: Rect) {
    let dim = Style::new().fg(resolve_color(app.use_color, Color::DarkGray));
    let label = if area.width >= 24 {
        "burn · all accts "
    } else if area.width >= 12 {
        "burn "
    } else {
        ""
    };
    let label_w = u16::try_from(label.chars().count()).unwrap_or(0);
    f.render_widget(Paragraph::new(Span::styled(label, dim)), area);
    let spark_area = Rect {
        x: area.x + label_w,
        width: area.width.saturating_sub(label_w),
        ..area
    };
    if spark_area.width >= MIN_BAR {
        let widget = Sparkline::default()
            .data(app.aggregate_burn.clone()) // small bounded series; owned copy for the widget
            .style(Style::new().fg(resolve_color(app.use_color, Color::Cyan)));
        f.render_widget(widget, spark_area);
    }
}

/// The one fleet-wide usage line: `tokens · cost · burn · usage-age · provenance · limits-age`, shared across all
/// accounts (see [`FleetView`]) so it's shown once here instead of on every panel. Segments are
/// dropped from the right (least→most important) until the line fits `area.width` — never overflows.
fn render_fleet(f: &mut Frame<'_>, app: &App, area: Rect) {
    const SEP: &str = " · ";
    let Some(fleet) = &app.fleet else { return };
    let dim = Style::new().fg(resolve_color(app.use_color, Color::DarkGray));
    // Roomy: full "(notional)" cost label + spelled-out provenance. Tight: "$382n" + "drv" (the title
    // carries the standing `$ = notional` legend, so the short forms lose no meaning).
    let roomy = area.width >= 60;
    let cost = if roomy {
        fleet.cost_notional.clone()
    } else {
        fleet.cost_short.clone()
    };
    // Ordered most→least important; each carries its own colour (the provenance badge stays coloured).
    let mut segments: Vec<(String, Style)> = vec![(fleet.tokens.clone(), dim), (cost, dim)];
    if let Some(rate) = &fleet.burn_rate {
        segments.push((rate.clone(), dim));
    }
    // Local ccusage-plane freshness — its own segment, styled a warning when stale so it *looks*
    // stale (never conflated with the overlay's age below). This is the always-on liveness cue.
    if let Some(usage_age) = &fleet.usage_age {
        let style = if fleet.usage_stale {
            Style::new().fg(resolve_color(app.use_color, Color::Yellow))
        } else {
            dim
        };
        segments.push((usage_age.clone(), style));
    }
    if let Some(badge) = &fleet.provenance {
        let text = if roomy { &badge.text } else { &badge.short };
        segments.push((text.clone(), Style::new().fg(badge.color)));
    }
    // Overlay/authoritative-plane age, distinctly labelled "limits …" so a fresh overlay can never
    // imply the local numbers are fresh (spec 011 §B).
    if let Some(overlay_age) = &fleet.overlay_age {
        segments.push((overlay_age.clone(), dim));
    }

    let width = usize::from(area.width);
    let mut spans = Vec::with_capacity(segments.len() * 2);
    let mut used = 0usize;
    for (text, style) in segments {
        let cells = text.chars().count();
        let add = if spans.is_empty() {
            cells
        } else {
            SEP.chars().count() + cells
        };
        if used + add > width {
            break; // this segment (and every less-important one after it) doesn't fit
        }
        if !spans.is_empty() {
            spans.push(Span::styled(SEP, dim));
        }
        spans.push(Span::styled(text, style));
        used += add;
    }
    // The ledger plane's one dim fleet token (spec 017 §D), lowest priority of all — it already
    // carries its own leading `"· "` bullet (see `ledger_fleet_note`), so it's appended with a
    // plain space rather than through the `SEP`-joined segments above.
    if let Some(note) = &fleet.ledger_note {
        let add = 1 + note.chars().count();
        if used + add <= width {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(note.clone(), dim));
        }
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// A gauge row without a leading spine: `"5h <bar> <label>"`, or `"5h <fallback>"`. `tight` swaps the
/// roomy `"pct sev · resets when"` label for a compact `"pct when"` (the severity word then rides on
/// the account's header line instead), so a narrow row spends its cells on the bar, not filler.
fn gauge_line(
    app: &App,
    prefix: &str,
    gauge: Option<&GaugeView>,
    fallback: &str,
    bar_w: u16,
    tight: bool,
) -> Line<'static> {
    match gauge {
        Some(g) => {
            let label = if tight { tight_label(g) } else { g.label() };
            let mut spans = Vec::with_capacity(4);
            spans.push(Span::raw(format!("{prefix} ")));
            spans.extend(bar_spans(g.ratio, bar_w, g.color, app.use_color));
            spans.push(Span::raw(" "));
            spans.push(Span::styled(label, Style::new().fg(g.color)));
            Line::from(spans)
        }
        None => Line::from(Span::styled(
            format!("{prefix} {fallback}"),
            Style::new().fg(resolve_color(app.use_color, Color::DarkGray)),
        )),
    }
}

/// The narrow label: `"[scope ]pct[ reset]"` (e.g. `"42% 1h 11m"`) — the time value is never
/// reformatted (verbatim rule); only the `"resets in "` filler is shed.
fn tight_label(g: &GaugeView) -> String {
    if g.expired {
        return g.label(); // "[scope ]waiting for reset" — no stale pct to show
    }
    let mut s = String::new();
    if let Some(scope) = &g.scope {
        s.push_str(scope);
        s.push(' ');
    }
    s.push_str(&g.pct);
    if let Some(reset) = g.reset_short() {
        s.push(' ');
        s.push_str(reset);
    }
    s
}

/// Pick the widest ledger-clause form (spec 017 §D, spec 018 §C) that fits `avail` cells, trying the
/// degrade order roomiest→narrowest: the full form (start segment + absolute date + verified pill
/// with its own date) → the pill's date dropped (bare `✓` stays) → the start segment also dropped →
/// the absolute date also dropped → omit entirely. Each candidate is rendered WHOLE or not at all —
/// this never truncates a date mid-string (the FULL-tier degrade rule, acceptance 3/5).
fn pick_full_clause(sub: &SubView, avail: usize) -> Option<&str> {
    [
        &sub.full,
        &sub.full_no_pill_date,
        &sub.full_no_start,
        &sub.full_no_date,
    ]
    .into_iter()
    .flatten()
    .find(|candidate| candidate.chars().count() <= avail)
    .map(String::as_str)
}

/// Split the verified pill off the end of a picked FULL-tier clause string (spec 018 §C), so it can
/// be styled distinctly (dim green) from the rest of the title. Compares `clause` against `sub.full`
/// to know whether the roomiest (date-bearing) pill suffix applies or the bare `" ✓"` suffix shared
/// by every narrower degrade step; returns the clause with the pill stripped, plus the pill text
/// itself (`None` when the picked candidate carries no pill at all).
fn split_pill<'a>(sub: &'a SubView, clause: &'a str) -> (&'a str, Option<&'a str>) {
    let is_widest = sub.full.as_deref() == Some(clause);
    let suffix = if is_widest {
        sub.pill_full.as_deref()
    } else {
        sub.pill_bare.as_deref()
    };
    suffix
        .and_then(|p| clause.strip_suffix(p).map(|base| (base, Some(p))))
        .unwrap_or((clause, None))
}

/// Build the FULL-tier border title's spans: `" {marker}{title}[ {clause}]"` plus a trailing space,
/// with the verified pill (when present in the picked clause) styled as its own dim-green span. The
/// clause is picked via [`pick_full_clause`] against whatever room is left after the (always-shown)
/// marker + account title + the title's own leading/trailing padding spaces — so the whole title,
/// clause included, is guaranteed to fit the border's `titles_area` (`area.width - 2`, the two corner
/// cells) and is therefore never truncated by ratatui itself (which would otherwise cut a date
/// mid-string).
fn full_title_spans(
    title: &str,
    marker: &str,
    sub: &SubView,
    avail_cols: u16,
    use_color: bool,
) -> Vec<Span<'static>> {
    let prefix = format!(" {marker}{title}");
    let prefix_w = prefix.chars().count();
    // One cell for the separating space before the clause, one for the title's own trailing space.
    let clause_budget = usize::from(avail_cols)
        .saturating_sub(prefix_w)
        .saturating_sub(2);
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let Some(clause) = pick_full_clause(sub, clause_budget) else {
        return vec![Span::styled(format!("{prefix} "), bold)];
    };
    let (base, pill) = split_pill(sub, clause);
    let mut spans = vec![Span::styled(format!("{prefix} {base}"), bold)];
    if let Some(pill) = pill {
        spans.push(Span::styled(
            pill.to_string(),
            Style::new()
                .fg(resolve_color(use_color, Color::Green))
                .add_modifier(Modifier::DIM),
        ));
    }
    spans.push(Span::raw(" "));
    spans
}

// ── COMPACT tier: borderless, three lines per account, grouped by a left accent spine ────────────

fn render_compact(f: &mut Frame<'_>, app: &App, area: Rect) {
    let n = app.rows.len();
    if n == 0 {
        return;
    }
    let want_spacer = spacer_fits(n, usize::from(area.height), usize::from(COMPACT_ROWS));
    let mut constraints = Vec::with_capacity(n * 2);
    for i in 0..n {
        constraints.push(Constraint::Length(COMPACT_ROWS));
        if want_spacer && i + 1 < n {
            constraints.push(Constraint::Length(1));
        }
    }
    let slots = Layout::vertical(constraints).split(area);
    let step = if want_spacer { 2 } else { 1 };
    // Wide enough for the roomy label; otherwise the severity word rides the header and the gauge
    // row runs a tight `pct reset` label so nothing truncates.
    let tight = area.width < 72;
    // Tight label is `pct reset` — 18 cells fits the widest case, the `"waiting for reset"`
    // sentinel (17), which must render whole (never-mutilate-a-reset rule).
    let right_zone: u16 = if tight { 18 } else { 29 };
    let bar_w = area.width.saturating_sub(5 + right_zone).max(MIN_BAR);
    for (i, row) in app.rows.iter().enumerate() {
        render_compact_block(
            f,
            app,
            row,
            slots[i * step],
            i == app.selected,
            bar_w,
            tight,
        );
    }
}

fn render_compact_block(
    f: &mut Frame<'_>,
    app: &App,
    row: &AccountView,
    area: Rect,
    selected: bool,
    bar_w: u16,
    tight: bool,
) {
    if area.height == 0 {
        return;
    }
    let spine_color = if selected {
        row.accent
    } else {
        resolve_color(app.use_color, Color::DarkGray)
    };
    let spine_ch = if selected { "▊" } else { "▏" };
    let spine = || Span::styled(spine_ch, Style::new().fg(spine_color));
    let sub = Layout::vertical([Constraint::Length(1); 3]).split(area);

    f.render_widget(
        Paragraph::new(compact_header_line(
            row,
            selected,
            spine_ch,
            spine_color,
            area.width,
            resolve_color(app.use_color, Color::DarkGray),
        )),
        sub[0],
    );
    let status = row.status.as_deref().unwrap_or("collecting…");
    let mut g5 = gauge_line(app, "5h", row.session.as_ref(), status, bar_w, tight);
    g5.spans.insert(0, spine());
    f.render_widget(Paragraph::new(g5), sub[1]);
    let mut gw = gauge_line(
        app,
        "wk",
        row.weekly.as_ref(),
        row.weekly_hint.as_str(),
        bar_w,
        tight,
    );
    gw.spans.insert(0, spine());
    f.render_widget(Paragraph::new(gw), sub[2]);
}

fn compact_header_line(
    row: &AccountView,
    selected: bool,
    spine_ch: &str,
    spine_color: Color,
    width: u16,
    dim: Color,
) -> Line<'static> {
    let caret = if selected { "▶ " } else { "  " };
    let name = format!("{caret}{}", row.title);
    let name_w = name.chars().count();
    let (cluster, cluster_w) = compact_cluster(row);
    let avail = usize::from(width).saturating_sub(1); // minus the spine column

    // spec 017 §D: two-step ladder — try the short dim clause first; if adding it would leave no
    // padding at all (name + clause + cluster would fill or overflow the line), drop the clause.
    // Name and the severity cluster always win over the ledger clause.
    let clause = row
        .sub
        .compact
        .as_deref()
        .filter(|c| name_w + 1 + c.chars().count() + cluster_w < avail);

    let mut spans = Vec::with_capacity(cluster.len() + 4);
    spans.push(Span::styled(
        spine_ch.to_string(),
        Style::new().fg(spine_color),
    ));
    spans.push(Span::styled(
        name,
        Style::new().add_modifier(Modifier::BOLD),
    ));
    let content_w = if let Some(clause) = clause {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(clause.to_string(), Style::new().fg(dim)));
        name_w + 1 + clause.chars().count()
    } else {
        name_w
    };
    let pad = avail.saturating_sub(content_w + cluster_w);
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
    }
    spans.extend(cluster);
    Line::from(spans)
}

/// The header's right cluster: `● 42% ok  $335.95 (notional) · drv` (wide) or `● 42% ok  $335n`
/// Returns the spans plus their display width so the header can right-align them. The per-account
/// token/cost/provenance figures moved to the fleet header line (they were the same on every row),
/// so this cluster now carries only the account's differentiating `● 42% ok` headline.
fn compact_cluster(row: &AccountView) -> (Vec<Span<'static>>, usize) {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut w = 0usize;
    if let Some(g) = row.headline.as_ref() {
        let glyph = severity_glyph(g.severity);
        spans.push(Span::styled(glyph.to_string(), Style::new().fg(g.color)));
        w += glyph.chars().count();
        let txt = format!(" {} {}", g.pct, severity_label(g.severity));
        w += txt.chars().count();
        spans.push(Span::styled(txt, Style::new().fg(g.color)));
    }
    (spans, w)
}

// ── MICRO tier: one aligned line per account, with a scroll viewport when they overflow ──────────

fn render_micro(f: &mut Frame<'_>, app: &App, area: Rect) {
    let n = app.rows.len();
    let h = usize::from(area.height);
    if n == 0 || h == 0 {
        return;
    }
    // Size the reset column to its widest value across accounts so rows align and NEVER truncate the
    // (verbatim) reset — an over-long value is dropped whole rather than mutilated.
    let reset_w = micro_reset_width(&app.rows);
    if n <= h {
        let want_spacer = spacer_fits(n, h, 1);
        let mut constraints = Vec::with_capacity(n * 2);
        for i in 0..n {
            constraints.push(Constraint::Length(1));
            if want_spacer && i + 1 < n {
                constraints.push(Constraint::Length(1));
            }
        }
        let slots = Layout::vertical(constraints).split(area);
        let step = if want_spacer { 2 } else { 1 };
        for (i, row) in app.rows.iter().enumerate() {
            f.render_widget(
                Paragraph::new(micro_line(app, row, area.width, i == app.selected, reset_w)),
                slots[i * step],
            );
        }
        return;
    }

    // More accounts than rows: scroll a window that always contains the selection, with chips for
    // the hidden runs. The banner names the global worst offender, so a scrolled-out crit is never
    // silently lost.
    let (start, count, chip_top, chip_bot) = micro_viewport(n, app.selected, h);
    let cells = Layout::vertical(vec![Constraint::Length(1); h]).split(area);
    let mut ci = 0;
    if chip_top {
        f.render_widget(
            Paragraph::new(micro_chip(app, true, &app.rows[..start], area.width)),
            cells[ci],
        );
        ci += 1;
    }
    for i in start..start + count {
        f.render_widget(
            Paragraph::new(micro_line(
                app,
                &app.rows[i],
                area.width,
                i == app.selected,
                reset_w,
            )),
            cells[ci],
        );
        ci += 1;
    }
    if chip_bot && ci < cells.len() {
        f.render_widget(
            Paragraph::new(micro_chip(
                app,
                false,
                &app.rows[start + count..],
                area.width,
            )),
            cells[ci],
        );
    }
}

fn micro_line(
    app: &App,
    row: &AccountView,
    width: u16,
    selected: bool,
    reset_w: usize,
) -> Line<'static> {
    let spine_color = if selected {
        row.accent
    } else {
        resolve_color(app.use_color, Color::DarkGray)
    };
    let dim = resolve_color(app.use_color, Color::DarkGray);
    let head = row.headline.as_ref();
    let color = head.map_or(dim, |g| g.color);
    let glyph = head.map_or("·", |g| severity_glyph(g.severity));
    let word = head.map_or("—", |g| severity_label(g.severity));
    let pct = head.map_or_else(|| "—".to_string(), |g| g.pct.clone());
    let ratio = head.map_or(0.0, |g| g.ratio);
    // Show this row's reset only if it fits the shared column — never truncate a verbatim reset.
    let reset = head
        .and_then(GaugeView::reset_short)
        .filter(|r| r.chars().count() <= reset_w)
        .unwrap_or("");
    let caret = if selected { "▶ " } else { "  " };

    let mut spans = Vec::with_capacity(12);
    spans.push(Span::styled(
        if selected { "▊" } else { "▏" }.to_string(),
        Style::new().fg(spine_color),
    ));
    spans.push(Span::styled(
        caret.to_string(),
        Style::new().fg(spine_color),
    ));
    spans.push(Span::styled(
        ljust(short_name(&row.title), 8),
        Style::new().add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(glyph.to_string(), Style::new().fg(color)));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(ljust(word, 4), Style::new().fg(color)));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(rjust(&pct, PCT_W), Style::new().fg(color)));

    // Fixed left columns consume 23 cells; reserve the aligned reset column (uniform across rows, so
    // bars align), then spend whatever remains on the bar. Cost moved to the fleet header line.
    let mut budget = usize::from(width).saturating_sub(23);
    let reserve_reset = reset_w > 0 && budget > reset_w;
    if reserve_reset {
        budget -= reset_w + 1;
    }
    if budget > usize::from(MIN_BAR) {
        let bar_w = u16::try_from(budget - 1).unwrap_or(u16::MAX);
        spans.push(Span::raw(" "));
        spans.extend(bar_spans(ratio, bar_w, color, app.use_color));
    }
    if reserve_reset {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(ljust(reset, reset_w), Style::new().fg(color)));
    }
    Line::from(spans)
}

/// The shared reset-column width for the MICRO tier: the widest reset across all rows, capped so one
/// pathological verbatim reset can't starve the row (over-cap resets are dropped, not cut). A uniform
/// width keeps every account's bar and reset column aligned.
fn micro_reset_width(rows: &[AccountView]) -> usize {
    const MAX_RESET_W: usize = 17; // fits every countdown and the "waiting for reset" sentinel
    rows.iter()
        .filter_map(|r| r.headline.as_ref())
        .filter_map(GaugeView::reset_short)
        .map(|r| r.chars().count())
        .filter(|&w| w <= MAX_RESET_W)
        .max()
        .unwrap_or(0)
}

fn micro_chip(app: &App, is_top: bool, hidden: &[AccountView], width: u16) -> Line<'static> {
    let (glyph, dir) = if is_top {
        ("▴", "above")
    } else {
        ("▾", "below")
    };
    let suffix = if usize::from(width) >= 44 {
        worst(hidden)
            .and_then(|w| w.headline.as_ref().map(|g| (w, g)))
            .map(|(w, g)| {
                // Name the scope (e.g. "Fable") but keep the chip reset-free — micro width is tight.
                let scope = g
                    .scope
                    .as_deref()
                    .map_or_else(String::new, |s| format!("{s} "));
                format!(
                    " · worst: {} {scope}{} {}",
                    short_name(&w.title),
                    g.pct,
                    severity_label(g.severity),
                )
            })
            .unwrap_or_default()
    } else {
        String::new()
    };
    let text = format!("  {glyph} {} more {dir}{suffix}", hidden.len());
    Line::from(Span::styled(
        text,
        Style::new().fg(resolve_color(app.use_color, Color::DarkGray)),
    ))
}

/// Choose the scroll window `[start, start+count)` that keeps `selected` visible in `h` rows, plus
/// whether a "more above"/"more below" chip is shown. Pure; unit-tested.
fn micro_viewport(n: usize, selected: usize, h: usize) -> (usize, usize, bool, bool) {
    if h == 0 {
        return (0, 0, false, false);
    }
    if n <= h {
        return (0, n, false, false);
    }
    if h < 3 {
        // No room for chips; just centre the selection on the available rows.
        return (center(selected, h, n), h, false, false);
    }
    // Reduce the account count until it plus its chips exactly fills h (a monotone fixpoint).
    let mut count = h;
    loop {
        let start = center(selected, count, n);
        let fit = h - usize::from(start > 0) - usize::from(start + count < n);
        if fit == count {
            break;
        }
        count = fit;
    }
    let start = center(selected, count, n);
    (start, count, start > 0, start + count < n)
}

/// Left edge of a `count`-wide window over `n` items that contains `selected` (clamped to bounds).
fn center(selected: usize, count: usize, n: usize) -> usize {
    if count >= n {
        return 0;
    }
    let start = selected.saturating_sub(count / 2);
    start.min(n - count)
}

// ── Shared helpers ───────────────────────────────────────────────────────────────────────────────

/// Build the coloured spans for a proportional bar: `█`/eighth-block fill in `filled_color` over a
/// `░` track in dim. Sub-cell precision keeps a 6-cell bar able to separate 76% from 93%.
fn bar_spans(ratio: f64, width: u16, filled_color: Color, use_color: bool) -> Vec<Span<'static>> {
    let (filled, empty) = bar_parts(ratio, width);
    let mut spans = Vec::with_capacity(2);
    if !filled.is_empty() {
        spans.push(Span::styled(filled, Style::new().fg(filled_color)));
    }
    if !empty.is_empty() {
        spans.push(Span::styled(
            empty,
            Style::new().fg(resolve_color(use_color, Color::DarkGray)),
        ));
    }
    spans
}

/// Split a bar of `width` cells into (filled, empty) strings for `ratio` (0‥1), with an eighth-block
/// boundary cell. `filled + empty` is always exactly `width` cells wide.
// cast: `eighths` is a clamped, non-negative value bounded by width*8 (≪ u32::MAX) — no loss.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn bar_parts(ratio: f64, width: u16) -> (String, String) {
    if width == 0 {
        return (String::new(), String::new());
    }
    let cells = usize::from(width);
    let eighths = (ratio.clamp(0.0, 1.0) * f64::from(width) * 8.0).round() as u32;
    let mut full = usize::try_from(eighths / 8).unwrap_or(cells).min(cells);
    let mut rem = (eighths % 8) as usize;
    if full == cells {
        rem = 0;
    }
    let mut filled = "█".repeat(full);
    if rem > 0 {
        filled.push(EIGHTHS[rem - 1]);
        full += 1;
    }
    (filled, "░".repeat(cells - full))
}

/// The account label without its `[provider]` suffix, for tight tiers (`"Personal [claude]"` →
/// `"Personal"`).
fn short_name(title: &str) -> &str {
    title.split_once(" [").map_or(title, |(name, _)| name)
}

/// The most-severe account (tie broken by headline utilization), or `None` if all are Ok. Inactive
/// rows are excluded even when shown — the worst-offender scan never names a peeked-at dead account
/// (spec 014 §C).
fn worst(rows: &[AccountView]) -> Option<&AccountView> {
    rows.iter()
        .filter(|r| !r.inactive && r.severity != Severity::Ok)
        .max_by(|a, b| {
            a.severity
                .cmp(&b.severity)
                .then_with(|| head_ratio(a).total_cmp(&head_ratio(b)))
        })
}

fn head_ratio(row: &AccountView) -> f64 {
    row.headline.as_ref().map_or(0.0, |g| g.ratio)
}

/// Whether a 1-row spacer between accounts still fits (`each_h` rows per account + 1 gap between).
fn spacer_fits(n: usize, height: usize, each_h: usize) -> bool {
    n > 0 && n * each_h + n.saturating_sub(1) <= height
}

/// Truncate `s` to `w` display cells, else pad it out on the right (char-count width; correct for
/// the single-cell glyph set this view uses).
fn ljust(s: &str, w: usize) -> String {
    let mut out: String = s.chars().take(w).collect();
    let pad = w.saturating_sub(out.chars().count());
    out.push_str(&" ".repeat(pad));
    out
}

/// Truncate `s` to `w` cells, else right-align it (pad on the left).
fn rjust(s: &str, w: usize) -> String {
    let taken: String = s.chars().take(w).collect();
    let pad = w.saturating_sub(taken.chars().count());
    format!("{}{taken}", " ".repeat(pad))
}

fn render_help(f: &mut Frame<'_>, area: Rect) {
    let block = Block::bordered().title(" help ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let lines = vec![
        Line::from("↑ / k        move selection up"),
        Line::from("↓ / j        move selection down"),
        Line::from("r            refresh from the store now"),
        Line::from("i            toggle showing inactive (unsubscribed) accounts"),
        Line::from("?            toggle this help"),
        Line::from("q / Esc      quit"),
        Line::from(""),
        Line::from("Layout adapts to the window: bordered panels when roomy, a spine-grouped"),
        Line::from("compact view when shorter, one line per account when tiny."),
        Line::from("Limits are % utilization + reset time — never \"X of Y\"."),
        Line::from("Cost is a notional usage proxy, never a bill."),
        Line::from("Badge shows provenance: derived (local) vs authoritative (overlay)."),
        Line::from("An inactive account, when shown, is dimmed and tagged \"(inactive)\"."),
    ];
    f.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Provenance, Severity};
    use crate::tui::model::{severity_color, AccountView, Badge, FleetView, GaugeView, SubView};
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::Terminal;

    fn buffer_text(buf: &Buffer) -> String {
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn gauge(pct: f64, severity: Severity, scope: Option<&str>) -> GaugeView {
        GaugeView {
            ratio: pct / 100.0,
            pct: format!("{pct:.0}%"),
            severity,
            reset: Some("in 1h 11m".to_string()),
            scope: scope.map(str::to_string),
            color: severity_color(severity),
            expired: false,
        }
    }

    fn row(title: &str, pct: f64, severity: Severity) -> AccountView {
        let g = gauge(pct, severity, None);
        AccountView {
            title: title.to_string(),
            accent: Color::Cyan,
            session: Some(g.clone()),
            weekly: None,
            weekly_hint: "n/a (enable overlay)".to_string(),
            weekly_scoped: None,
            headline: Some(g),
            status: None,
            severity,
            inactive: false,
            sub: SubView::default(),
        }
    }

    /// An overlay-on row: authoritative session + weekly-all + a scoped (Fable) weekly gauge.
    fn overlay_row() -> AccountView {
        let session = gauge(29.0, Severity::Ok, None);
        let weekly = gauge(78.0, Severity::Warn, None);
        let scoped = gauge(92.0, Severity::Crit, Some("Fable"));
        AccountView {
            title: "Primary [claude]".to_string(),
            accent: Color::Cyan,
            session: Some(session),
            weekly: Some(weekly),
            weekly_hint: "n/a (waiting for overlay)".to_string(),
            weekly_scoped: Some(scoped.clone()),
            headline: Some(scoped),
            status: None,
            severity: Severity::Crit,
            inactive: false,
            sub: SubView::default(),
        }
    }

    /// The fleet line for a derived (overlay-off) board — matches the shared per-account figures.
    fn fleet_derived() -> FleetView {
        FleetView {
            tokens: "280.54M".to_string(),
            cost_notional: "$335.95 (notional)".to_string(),
            cost_short: "$335n".to_string(),
            burn_rate: Some("271.20M/h".to_string()),
            provenance: Some(Badge {
                text: "derived".to_string(),
                short: "drv".to_string(),
                color: Color::Cyan,
            }),
            usage_age: Some("usage 8s ago".to_string()),
            usage_stale: false,
            overlay_age: None,
            ledger_note: None,
        }
    }

    /// A Codex account row (spec 013 §D): authoritative Session + WeeklyAll gauges from the
    /// `codex app-server` overlay, NO scoped weekly (Codex has no per-model limits), title `[codex]`.
    fn codex_row() -> AccountView {
        let session = gauge(42.0, Severity::Ok, None);
        let weekly = gauge(88.0, Severity::Warn, None);
        AccountView {
            title: "Codex [codex]".to_string(),
            accent: Color::Cyan,
            session: Some(session),
            weekly: Some(weekly.clone()),
            weekly_hint: "n/a (waiting for overlay)".to_string(),
            weekly_scoped: None,
            headline: Some(weekly),
            status: None,
            severity: Severity::Warn,
            inactive: false,
            sub: SubView::default(),
        }
    }

    /// The fleet line for a Codex board: tokens present, but cost is `—` (Codex has no notional
    /// basis — `cost_notional` is `None` and must not poison the fleet cost sum), authoritative.
    fn fleet_codex() -> FleetView {
        FleetView {
            tokens: "13.09K".to_string(),
            cost_notional: "—".to_string(),
            cost_short: "—".to_string(),
            burn_rate: None,
            provenance: Some(Badge {
                text: "authoritative".to_string(),
                short: "auth".to_string(),
                color: Color::Green,
            }),
            usage_age: Some("usage 5s ago".to_string()),
            usage_stale: false,
            overlay_age: Some("limits 1m ago".to_string()),
            ledger_note: None,
        }
    }

    /// The fleet line for an overlay-on board — authoritative, with both plane ages.
    fn fleet_authoritative() -> FleetView {
        FleetView {
            tokens: "412.42M".to_string(),
            cost_notional: "$327.00 (notional)".to_string(),
            cost_short: "$327n".to_string(),
            burn_rate: Some("189.00M/h".to_string()),
            provenance: Some(Badge {
                text: "authoritative".to_string(),
                short: "auth".to_string(),
                color: Color::Green,
            }),
            usage_age: Some("usage 5s ago".to_string()),
            usage_stale: false,
            overlay_age: Some("limits 2m ago".to_string()),
            ledger_note: None,
        }
    }

    fn board() -> App {
        let mut app = App::new(3, true);
        app.rows = vec![
            row("Personal [claude]", 42.0, Severity::Ok),
            row("Primary [claude]", 76.0, Severity::Warn),
            row("Research [claude]", 93.0, Severity::Crit),
        ];
        app.aggregate_burn = vec![10, 25, 18, 40, 33, 60, 55, 80];
        app.fleet = Some(fleet_derived());
        app.selected = 1;
        app
    }

    /// An inactive account's row as `build_account_view` produces it when `show_inactive` reveals
    /// one: crit severity but every colour flattened to dim grey, and the title tagged (spec 014 §C).
    fn inactive_row() -> AccountView {
        let mut g = gauge(97.0, Severity::Crit, None);
        g.color = Color::DarkGray;
        AccountView {
            title: "Retired [claude] (inactive)".to_string(),
            accent: Color::DarkGray,
            session: Some(g.clone()),
            weekly: None,
            weekly_hint: "n/a (enable overlay)".to_string(),
            weekly_scoped: None,
            headline: Some(g),
            status: None,
            severity: Severity::Crit,
            inactive: true,
            sub: SubView::default(),
        }
    }

    fn draw(app: &App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).expect("backend");
        term.draw(|f| render(f, app)).expect("draw");
        buffer_text(term.backend().buffer())
    }

    /// Every rendered line must fit the terminal width exactly — no overflow, no wrap.
    fn assert_no_overflow(text: &str, w: usize) {
        for line in text.lines() {
            assert!(
                line.chars().count() <= w,
                "line exceeds width {w}: {:?} ({} cols)",
                line,
                line.chars().count()
            );
        }
    }

    #[test]
    fn full_tier_renders_panels_bars_and_banner() {
        let text = draw(&board(), 120, 40);
        assert!(text.contains("TOKENOMICS"), "title:\n{text}");
        assert!(text.contains("Personal [claude]"), "panel title:\n{text}");
        assert!(text.contains("76% warn"), "session label:\n{text}");
        assert!(text.contains("n/a (enable overlay)"), "weekly n/a:\n{text}");
        // Token/cost/burn/provenance now live once on the fleet header line, not per panel.
        assert!(text.contains("derived"), "fleet provenance badge:\n{text}");
        assert!(text.contains("notional"), "fleet cost label:\n{text}");
        assert!(text.contains("271.20M/h"), "fleet tokens/hour:\n{text}");
        assert!(text.contains("280.54M"), "fleet total tokens:\n{text}");
        assert!(
            text.contains("burn · all accts"),
            "aggregate burn bar:\n{text}"
        );
        assert!(text.contains('█'), "eighth-block bar fill:\n{text}");
        // Selected (Primary) gets a double-line border; unselected stay single.
        assert!(
            text.contains('╔') && text.contains('┌'),
            "selection border:\n{text}"
        );
        assert!(
            text.contains("at or above the warn threshold"),
            "banner:\n{text}"
        );
        assert_no_overflow(&text, 120);
    }

    #[test]
    fn full_tier_shows_weekly_and_scoped_gauges() {
        let mut app = App::new(1, true);
        app.rows = vec![overlay_row()];
        app.fleet = Some(fleet_authoritative());
        let text = draw(&app, 120, 40);
        assert!(text.contains("5h "), "session prefix:\n{text}");
        assert!(
            text.contains("78% warn · resets in 1h 11m"),
            "weekly:\n{text}"
        );
        assert!(
            text.contains("Fable 92% crit · resets in 1h 11m"),
            "scoped weekly:\n{text}"
        );
        assert!(text.contains("authoritative"), "provenance:\n{text}");
        assert!(text.contains("limits 2m ago"), "overlay-plane age:\n{text}");
        assert!(text.contains("usage 5s ago"), "local-plane age:\n{text}");
        assert!(!text.contains("enable overlay"), "no nudge:\n{text}");
        assert_no_overflow(&text, 120);
    }

    #[test]
    fn compact_tier_is_borderless_with_spine() {
        let text = draw(&board(), 80, 16);
        assert!(!text.contains('┌'), "no box borders in compact:\n{text}");
        assert!(
            text.contains('▏') || text.contains('▊'),
            "accent spine:\n{text}"
        );
        assert!(
            text.contains("▊▶ Primary"),
            "selected spine + caret:\n{text}"
        );
        assert!(text.contains("5h "), "session gauge:\n{text}");
        assert!(text.contains("42% ok"), "headline pct:\n{text}");
        assert_no_overflow(&text, 80);
    }

    #[test]
    fn micro_tier_is_one_line_per_account() {
        let text = draw(&board(), 58, 9);
        assert!(!text.contains('┌'), "no borders in micro:\n{text}");
        // Each account collapses to a single dense line carrying glyph + pct.
        assert!(text.contains("Personal"), "name:\n{text}");
        assert!(text.contains("93%"), "crit pct:\n{text}");
        assert!(
            text.contains('✖') || text.contains("crit"),
            "severity cue:\n{text}"
        );
        assert_no_overflow(&text, 58);
    }

    #[test]
    fn micro_tier_never_truncates_the_reset() {
        // Regression: the reset column used to be a fixed 6 cells, so a 2-digit-hour countdown
        // ("23h 59m" = 7) or the "waiting for reset" sentinel (17) lost its tail — mutilating a
        // value the verbatim-reset rule says must render whole. Column sizes to the widest reset.
        let two = |title: &str, reset: &str| {
            let mut g = gauge(88.0, Severity::Warn, None);
            g.reset = Some(reset.to_string());
            AccountView {
                headline: Some(g.clone()),
                session: Some(g),
                ..row(title, 88.0, Severity::Warn)
            }
        };
        let mut app = App::new(2, true);
        app.rows = vec![
            two("Primary [claude]", "in 23h 59m"),
            two("Research [claude]", "waiting for reset"),
        ];
        // width 70 + height 4 (body = 2) forces the one-line MICRO tier.
        let text = draw(&app, 70, 4);
        assert!(
            text.contains("23h 59m"),
            "2-digit-hour reset whole:\n{text}"
        );
        assert!(
            text.contains("waiting for reset"),
            "sentinel reset whole:\n{text}"
        );
        assert_no_overflow(&text, 70);
    }

    #[test]
    fn narrow_width_stays_within_bounds() {
        let text = draw(&board(), 42, 14);
        assert!(text.contains("TOKENOMICS"), "title:\n{text}");
        assert_no_overflow(&text, 42);
    }

    #[test]
    fn empty_board_shows_placeholder() {
        let app = App::new(2, true);
        let text = draw(&app, 80, 20);
        assert!(text.contains("collecting"), "placeholder:\n{text}");
    }

    #[test]
    fn help_overlay_lists_keys() {
        let mut app = board();
        app.show_help = true;
        let text = draw(&app, 80, 24);
        assert!(text.contains("move selection up"), "help:\n{text}");
        assert!(text.contains("quit"), "help:\n{text}");
    }

    #[test]
    fn no_color_produces_identical_character_grid() {
        // Colour is pure garnish: the character grid must be byte-identical with colour off.
        let colored = board();
        let mut mono = board();
        mono.use_color = false;
        for (w, h) in [(120, 40), (80, 16), (58, 9), (42, 14)] {
            assert_eq!(
                draw(&colored, w, h),
                draw(&mono, w, h),
                "grid differs with NO_COLOR at {w}x{h}"
            );
        }
    }

    #[test]
    fn micro_viewport_keeps_selection_visible_and_reports_hidden() {
        // 8 accounts, 7 rows: a window plus chips, selection always inside it.
        assert_eq!(micro_viewport(8, 0, 7), (0, 6, false, true));
        assert_eq!(micro_viewport(8, 7, 7), (2, 6, true, false));
        let (start, count, top, bot) = micro_viewport(8, 4, 7);
        assert!(start <= 4 && 4 < start + count, "selection in window");
        assert!(top && bot, "both chips shown mid-list");
        // The window plus its chips exactly fills the available rows.
        assert_eq!(count + usize::from(top) + usize::from(bot), 7);
        // Everything fits ⇒ no scroll, no chips.
        assert_eq!(micro_viewport(3, 1, 20), (0, 3, false, false));
    }

    #[test]
    fn many_accounts_scroll_with_chips() {
        let mut app = App::new(8, true);
        app.rows = (0..8)
            .map(|i| {
                row(
                    &format!("Acct{i} [claude]"),
                    50.0 + f64::from(i),
                    Severity::Warn,
                )
            })
            .collect();
        app.selected = 7;
        let text = draw(&app, 60, 9);
        assert!(text.contains("more above"), "top chip:\n{text}");
        assert!(text.contains("Acct7"), "selection visible:\n{text}");
        assert_no_overflow(&text, 60);
    }

    #[test]
    fn choose_tier_picks_density_by_space() {
        let rows = board().rows;
        assert_eq!(choose_tier(Rect::new(0, 0, 120, 37), &rows), Tier::Full);
        // 3 panels × 4 rows = 12 needed for FULL; 11 forces COMPACT (3 × 3 = 9 fits).
        assert_eq!(choose_tier(Rect::new(0, 0, 80, 11), &rows), Tier::Compact);
        assert_eq!(choose_tier(Rect::new(0, 0, 58, 7), &rows), Tier::Micro);
        // Wide but short ⇒ height forces a denser tier than width alone would pick.
        assert_eq!(choose_tier(Rect::new(0, 0, 200, 8), &rows), Tier::Micro);
        // Tall but narrow ⇒ width caps the density.
        assert_eq!(choose_tier(Rect::new(0, 0, 30, 40), &rows), Tier::Micro);
    }

    #[test]
    fn bar_parts_are_exactly_width_cells_with_subcell_precision() {
        let cells = |(f, e): (String, String)| f.chars().count() + e.chars().count();
        assert_eq!(cells(bar_parts(0.0, 10)), 10);
        assert_eq!(cells(bar_parts(1.0, 10)), 10);
        assert_eq!(cells(bar_parts(0.5, 10)), 10);
        assert_eq!(bar_parts(1.0, 6), ("██████".to_string(), String::new()));
        assert_eq!(bar_parts(0.0, 6), (String::new(), "░░░░░░".to_string()));
        // Sub-cell precision separates neighbouring percents on a 6-cell bar.
        assert_ne!(bar_parts(0.76, 6), bar_parts(0.93, 6));
    }

    #[test]
    fn build_uses_provenance() {
        assert_eq!(
            super::super::model::provenance_color(Provenance::Derived),
            Color::Cyan
        );
    }

    #[test]
    fn collector_down_banner_shows_and_degrades() {
        let mut app = board();
        app.collector_alert =
            Some("collector not running — data frozen (start `tok collector`)".to_string());
        // Roomy: the full message with the warning glyph is shown.
        let wide = draw(&app, 120, 40);
        assert!(wide.contains('⚠'), "warning glyph:\n{wide}");
        assert!(
            wide.contains("collector not running"),
            "full alert text:\n{wide}"
        );
        assert_no_overflow(&wide, 120);
        // Narrow: it degrades to a short form rather than overflowing/wrapping.
        let narrow = draw(&app, 30, 14);
        assert!(narrow.contains("collector down"), "short form:\n{narrow}");
        assert_no_overflow(&narrow, 30);
    }

    #[test]
    fn inactive_row_shown_is_excluded_from_banner_and_worst() {
        // A crit-level inactive row, shown alongside the normal board, must not add to the warn
        // count or ever be named the worst offender (spec 014 §C, acceptance criteria 3–4).
        let mut app = board();
        app.show_inactive = true;
        app.rows.push(inactive_row());
        assert_eq!(
            app.alert_count(),
            2,
            "the pushed inactive crit row must not count"
        );
        let w = worst(&app.rows).expect("the board's real warn/crit rows still count");
        assert_ne!(
            w.title, "Retired [claude] (inactive)",
            "worst() must skip inactive rows even at crit"
        );
    }

    #[test]
    fn full_board_snapshot() {
        insta::assert_snapshot!("board", draw(&board(), 120, 40));
    }

    #[test]
    fn board_with_inactive_shown_snapshot() {
        let mut app = board();
        app.show_inactive = true;
        app.rows.push(inactive_row());
        insta::assert_snapshot!("board_inactive", draw(&app, 120, 40));
    }

    #[test]
    fn full_board_overlay_snapshot() {
        let mut app = App::new(1, true);
        app.rows = vec![overlay_row()];
        app.fleet = Some(fleet_authoritative());
        insta::assert_snapshot!("board_overlay", draw(&app, 120, 40));
    }

    #[test]
    fn full_board_codex_snapshot() {
        // A Codex account renders with the same row grammar: authoritative 5h + weekly gauges, no
        // scoped weekly, and a fleet line whose cost is `—` (no notional basis) — spec 013 §D.
        let mut app = App::new(1, true);
        app.rows = vec![codex_row()];
        app.fleet = Some(fleet_codex());
        insta::assert_snapshot!("board_codex", draw(&app, 120, 40));
    }

    #[test]
    fn compact_board_snapshot() {
        insta::assert_snapshot!("board_compact", draw(&board(), 80, 16));
    }

    #[test]
    fn micro_board_snapshot() {
        insta::assert_snapshot!("board_micro", draw(&board(), 58, 9));
    }

    #[test]
    fn narrow_board_snapshot() {
        insta::assert_snapshot!("board_narrow", draw(&board(), 42, 14));
    }

    // ── spec 017 §D (acceptance 5): tier rendering of the ledger clause ────────────────────────

    fn sub_view_all_forms() -> SubView {
        let full = "· period 2026-07-14 → · renews in 27d (2026-08-14)".to_string();
        SubView {
            full_no_pill_date: Some(full.clone()),
            full: Some(full),
            full_no_start: Some("· renews in 27d (2026-08-14)".to_string()),
            full_no_date: Some("· renews in 27d".to_string()),
            compact: Some("· renews 27d".to_string()),
            pill_full: None,
            pill_bare: None,
        }
    }

    /// [`sub_view_all_forms`] with a verified-current pill (spec 018 §C) — for the pill-specific
    /// FULL-tier degrade-order and COMPACT-ladder tests.
    fn sub_view_all_forms_with_pill() -> SubView {
        SubView {
            full: Some(
                "· period 2026-07-14 → · renews in 27d (2026-08-14) ✓ 2026-07-18".to_string(),
            ),
            full_no_pill_date: Some(
                "· period 2026-07-14 → · renews in 27d (2026-08-14) ✓".to_string(),
            ),
            full_no_start: Some("· renews in 27d (2026-08-14) ✓".to_string()),
            full_no_date: Some("· renews in 27d ✓".to_string()),
            compact: Some("· renews 27d ✓".to_string()),
            pill_full: Some(" ✓ 2026-07-18".to_string()),
            pill_bare: Some(" ✓".to_string()),
        }
    }

    fn row_with_sub(title: &str, sub: SubView) -> AccountView {
        AccountView {
            sub,
            ..row(title, 42.0, Severity::Ok)
        }
    }

    /// The other three ledger states (spec 017 §D table rows 3–5) as `SubView`s, so acceptance 5's
    /// "snapshot coverage for active, cancelled, ended, and unknown states in FULL and COMPACT" has
    /// fixtures beyond the one active-future-renews case `sub_view_all_forms` covers above.
    fn sub_view_cancelled_ends() -> SubView {
        let clause = "· cancelled · ends in 4d (2026-07-22)".to_string();
        SubView {
            full_no_pill_date: Some(clause.clone()),
            full: Some(clause.clone()),
            full_no_start: Some(clause),
            full_no_date: Some("· cancelled · ends in 4d".to_string()),
            compact: Some("· ends 4d".to_string()),
            pill_full: None,
            pill_bare: None,
        }
    }

    fn sub_view_ended() -> SubView {
        let clause = "· cancelled · ended 2026-07-22".to_string();
        SubView {
            full_no_pill_date: Some(clause.clone()),
            full: Some(clause.clone()),
            full_no_start: Some(clause.clone()),
            full_no_date: Some(clause),
            compact: Some("· ended".to_string()),
            pill_full: None,
            pill_bare: None,
        }
    }

    fn sub_view_unknown() -> SubView {
        let bare = "· cancelled".to_string();
        SubView {
            full_no_pill_date: Some(bare.clone()),
            full: Some(bare.clone()),
            full_no_start: Some(bare.clone()),
            full_no_date: Some(bare.clone()),
            compact: Some(bare),
            pill_full: None,
            pill_bare: None,
        }
    }

    #[test]
    fn tier_snapshot_coverage_for_cancelled_ended_and_unknown_states() {
        // acceptance 5: FULL (border title) and COMPACT (header line) both render each of the
        // three non-active-renewal states correctly, mirroring the active-state coverage above.
        let cases: [(&str, SubView, &str, &str); 3] = [
            (
                "cancelled-ends",
                sub_view_cancelled_ends(),
                "cancelled · ends in 4d (2026-07-22)",
                "ends 4d",
            ),
            (
                "ended",
                sub_view_ended(),
                "cancelled · ended 2026-07-22",
                "ended",
            ),
            ("unknown", sub_view_unknown(), "cancelled", "cancelled"),
        ];
        for (name, sub, full_needle, compact_needle) in cases {
            let mut app = App::new(1, true);
            app.rows = vec![row_with_sub("Personal [claude]", sub.clone())];
            let full_text = draw(&app, 140, 40);
            assert!(
                full_text.contains(full_needle),
                "[{name}] FULL tier must show '{full_needle}':\n{full_text}"
            );

            let compact_text = line_text(&compact_header_line(
                &row_with_sub("Personal [claude]", sub),
                false,
                "▏",
                Color::Cyan,
                80,
                Color::DarkGray,
            ));
            assert!(
                compact_text.contains(compact_needle),
                "[{name}] COMPACT header must show '{compact_needle}': {compact_text:?}"
            );
        }
    }

    #[test]
    fn full_tier_shows_the_roomiest_clause_when_it_fits() {
        let mut app = App::new(1, true);
        app.rows = vec![row_with_sub("Personal [claude]", sub_view_all_forms())];
        let text = draw(&app, 140, 40);
        assert!(
            text.contains("period 2026-07-14"),
            "roomy width must show the full clause with its start segment:\n{text}"
        );
        assert!(
            text.contains("renews in 27d (2026-08-14)"),
            "the absolute date must be present at roomy width:\n{text}"
        );
    }

    #[test]
    fn full_tier_degrade_order_never_truncates_a_date_mid_string() {
        // acceptance 5: start segment → absolute date → whole clause, in that order, and a date is
        // either shown WHOLE or not shown — never a fragment like "2026-08-1" missing its final
        // digit. Every occurrence of the date's 9-char prefix must be immediately followed by the
        // completing "4" (i.e. it is always the full "2026-08-14", never cut short), at every width.
        for width in [140u16, 90, 70, 50, 30] {
            let mut app = App::new(1, true);
            app.rows = vec![row_with_sub("Personal [claude]", sub_view_all_forms())];
            let text = draw(&app, width, 40);
            for (idx, _) in text.match_indices("2026-08-1") {
                let next = text[idx + "2026-08-1".len()..].chars().next();
                assert_eq!(
                    next,
                    Some('4'),
                    "a truncated date fragment appeared at width {width}:\n{text}"
                );
            }
        }
    }

    #[test]
    fn pick_full_clause_picks_the_widest_form_that_fits() {
        let sub = sub_view_all_forms();
        assert_eq!(
            pick_full_clause(&sub, 200),
            Some("· period 2026-07-14 → · renews in 27d (2026-08-14)"),
            "roomy width picks the full roomiest form"
        );
        assert_eq!(
            pick_full_clause(&sub, 40),
            Some("· renews in 27d (2026-08-14)"),
            "medium width drops the start segment first"
        );
        assert_eq!(
            pick_full_clause(&sub, 25),
            Some("· renews in 27d"),
            "narrower still drops the absolute date next"
        );
        assert_eq!(
            pick_full_clause(&sub, 5),
            None,
            "too narrow for even the bare clause ⇒ omit entirely"
        );
    }

    // ── spec 018 §C (acceptance 3): FULL-tier pill rendering + degrade order ──────────────────

    #[test]
    fn full_title_spans_pill_full_form_and_styling_when_roomy() {
        let sub = sub_view_all_forms_with_pill();
        let spans = full_title_spans("Personal [claude]", "", &sub, 200, true);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(
            text,
            " Personal [claude] · period 2026-07-14 → · renews in 27d (2026-08-14) ✓ 2026-07-18 ",
            "roomy width shows the whole pill with its date: {text:?}"
        );
        let pill_span = spans
            .iter()
            .find(|s| s.content.contains('✓'))
            .expect("a pill span must exist");
        assert_eq!(pill_span.style.fg, Some(Color::Green), "pill must be green");
        assert!(
            pill_span.style.add_modifier.contains(Modifier::DIM),
            "pill must be dim"
        );
    }

    #[test]
    fn full_title_spans_degrade_order_pill_date_then_start_then_absolute_date_then_whole_clause() {
        let sub = sub_view_all_forms_with_pill();
        let text = |avail| -> String {
            full_title_spans("Personal [claude]", "", &sub, avail, true)
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        };
        // Roomy: full pill with its own date.
        assert!(text(200).ends_with("✓ 2026-07-18 "), "{}", text(200));

        // Step 1: too narrow for the pill's date, bare ✓ survives with the start segment intact.
        let step1 = text(75);
        assert!(
            step1.contains("period 2026-07-14") && step1.trim_end().ends_with('✓'),
            "step1: {step1:?}"
        );

        // Step 2: start segment also dropped, bare ✓ still present.
        let step2 = text(55);
        assert!(
            !step2.contains("period 2026-07-14")
                && step2.contains("renews in 27d (2026-08-14)")
                && step2.trim_end().ends_with('✓'),
            "step2: {step2:?}"
        );

        // Step 3: absolute date also dropped, bare ✓ still present.
        let step3 = text(40);
        assert!(
            !step3.contains("2026-08-14") && step3.trim_end().ends_with('✓'),
            "step3: {step3:?}"
        );

        // Step 4: too narrow for even the bare clause ⇒ whole clause (pill included) drops.
        let step4 = text(30);
        assert!(!step4.contains('✓'), "step4: {step4:?}");
    }

    #[test]
    fn full_title_spans_no_pill_renders_no_check_mark_or_pill_styling() {
        let sub = sub_view_all_forms(); // no verified pill
        let spans = full_title_spans("Personal [claude]", "", &sub, 200, true);
        assert!(
            !spans.iter().any(|s| s.content.contains('✓')),
            "no pill applies ⇒ no ✓ anywhere: {spans:?}"
        );
        assert!(
            !spans.iter().any(|s| {
                s.style.fg == Some(Color::Green) || s.style.add_modifier.contains(Modifier::DIM)
            }),
            "no pill applies ⇒ no span carries pill styling: {spans:?}"
        );
    }

    /// Flatten a `Line`'s spans into plain text, for asserting on `compact_header_line`/`micro_line`
    /// output directly (pure, no tier-selection geometry involved — those are covered by `choose_tier`
    /// and the full-render tests elsewhere).
    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn compact_header_shows_the_short_clause_when_it_fits() {
        let row = row_with_sub("Personal [claude]", sub_view_all_forms());
        let text = line_text(&compact_header_line(
            &row,
            false,
            "▏",
            Color::Cyan,
            80,
            Color::DarkGray,
        ));
        assert!(
            text.contains("renews 27d"),
            "the compact header must carry the short dim clause when it fits: {text:?}"
        );
    }

    #[test]
    fn compact_header_two_step_ladder_drops_clause_before_overflowing() {
        // acceptance 5 / spec 017 §D: if padding for name + cluster + clause would hit 0, the
        // clause is dropped first — name and the severity cluster always win, and the line must
        // never overflow the given width.
        let row = row_with_sub("Personal [claude]", sub_view_all_forms());
        let text = line_text(&compact_header_line(
            &row,
            false,
            "▏",
            Color::Cyan,
            30,
            Color::DarkGray,
        ));
        assert!(
            text.chars().count() <= 30,
            "must never overflow the given width: {text:?} ({} cols)",
            text.chars().count()
        );
        assert!(text.contains("Personal"), "the name must survive: {text:?}");
    }

    #[test]
    fn micro_line_never_shows_a_date_under_any_state() {
        let app = App::new(1, true);
        let row = row_with_sub("Personal [claude]", sub_view_all_forms());
        let text = line_text(&micro_line(&app, &row, 80, false, 0));
        assert!(
            !text.contains("2026-"),
            "MICRO must never show a ledger date (stated decision, spec 017 §D): {text:?}"
        );
    }

    // ── spec 018 §C (acceptance 4): COMPACT trailing ✓, MICRO never shows ✓ ───────────────────

    #[test]
    fn compact_header_shows_trailing_pill_when_verified_current() {
        let row = row_with_sub("Personal [claude]", sub_view_all_forms_with_pill());
        let text = line_text(&compact_header_line(
            &row,
            false,
            "▏",
            Color::Cyan,
            80,
            Color::DarkGray,
        ));
        assert!(
            text.contains("renews 27d ✓"),
            "the compact clause carries the trailing ✓ inside the ladder: {text:?}"
        );
    }

    #[test]
    fn compact_header_drops_pill_together_with_clause_when_narrow() {
        // The two-step ladder drops clause+pill together — never a bare ✓ with no clause.
        let row = row_with_sub("Personal [claude]", sub_view_all_forms_with_pill());
        let text = line_text(&compact_header_line(
            &row,
            false,
            "▏",
            Color::Cyan,
            30,
            Color::DarkGray,
        ));
        assert!(
            !text.contains('✓'),
            "too narrow for the clause ⇒ the pill goes with it: {text:?}"
        );
        assert!(text.chars().count() <= 30, "must never overflow: {text:?}");
    }

    #[test]
    fn micro_line_never_shows_the_pill_under_any_state() {
        let app = App::new(1, true);
        let row = row_with_sub("Personal [claude]", sub_view_all_forms_with_pill());
        let text = line_text(&micro_line(&app, &row, 80, false, 0));
        assert!(
            !text.contains('✓'),
            "MICRO must never show a pill under any state (spec 018 §C): {text:?}"
        );
    }

    fn fleet_with_note(note: &str) -> FleetView {
        FleetView {
            ledger_note: Some(note.to_string()),
            ..fleet_derived()
        }
    }

    #[test]
    fn fleet_header_shows_missing_ledger_token() {
        let mut app = board();
        app.fleet = Some(fleet_with_note("· no ledger"));
        let text = draw(&app, 140, 40);
        assert!(text.contains("no ledger"), "Missing token:\n{text}");
    }

    #[test]
    fn fleet_header_shows_stale_ledger_token() {
        let mut app = board();
        app.fleet = Some(fleet_with_note("· ledger stale"));
        let text = draw(&app, 140, 40);
        assert!(text.contains("ledger stale"), "Stale token:\n{text}");
    }

    #[test]
    fn fleet_header_shows_zero_matched_ledger_token() {
        let mut app = board();
        app.fleet = Some(fleet_with_note("· ledger: 0 matched"));
        let text = draw(&app, 140, 40);
        assert!(text.contains("0 matched"), "zero-matched token:\n{text}");
    }

    #[test]
    fn fleet_header_off_shows_no_ledger_token_at_all() {
        // Off is `ledger_note: None` — nothing ledger-related renders anywhere (deliberate, spec
        // 017 §D: unconfigured is a valid permanent state, `doctor` owns the distinction).
        let app = board(); // fleet_derived() already carries ledger_note: None
        let text = draw(&app, 140, 40);
        assert!(!text.contains("ledger"), "Off must render nothing:\n{text}");
    }
}
