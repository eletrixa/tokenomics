---
rule: tui-design
title: TUI Visual Design — the house style
category: design
scope: [rust, go, tui]
priority: recommended
applies-to: [rust, go]
tags: [tui, ratatui, design, color, tables, gauges, accessibility]
---

# TUI Visual Design — the house style

**Enforcement**: code review + insta snapshots. Sourced 2026-07-26 from the design systems of
k9s, lazygit, btop, bottom, gitui, oxker, bandwhich, the Charm ecosystem (lipgloss/bubbles,
glow, gh-dash), the ratatui recipes, atuin, yazi, trippy, taskwarrior-tui, clig.dev, and the
NO_COLOR spec. Where the sources disagree, §Adjudications records the ruling for this app —
follow the ruling, not the raw source.

---

## Core principle

> The user's terminal theme is the palette; color is a budget you spend on meaning.
> Chrome is dim, data is plain, exactly one thing per panel may shout.

---

## Color

- **ANSI-16 named colors are the foundation** (`Color::Magenta`, not `Rgb(...)`). Named colors
  resolve through the user's terminal theme and stay legible on any background; hardcoded RGB is
  an opt-in enhancement behind a capability check, never the base. (ratatui maintainers,
  discussion #877: "usable at 16 colors, beautiful at true color".)
- **Foregrounds only.** Never paint a panel background; `bg` is spent solely on selection bars
  and mode/status chips. (k9s skins `default`, lazygit `defaultFgColor [default]`, atuin's
  always-unset Base.)
- **Every accent hue has exactly one job**, fixed app-wide:

  | Hue | Job here | Never |
  |---|---|---|
  | magenta | plane/section chrome — titles, section borders | alarm, data |
  | cyan | interactive — keys, focus | severity |
  | green / yellow / red | severity ramp on `utilization_pct`-class data only | decoration |
  | dim / DarkGray | metadata — provenance, ages, units, labels | primary data |
  | bold white | the data itself (money, %, counts) | chrome |

  If everything is a different color, the color means nothing (clig.dev). One saturated accent
  per screen region; adjacent chrome in the same hue must step down to dim (glow's
  fuchsia → dullFuchsia → dimFuchsia ladder).
- **Severity is a 3-band ramp** — ok = default/green, warn = yellow, crit = red — applied to the
  value itself (the % cell, the account name), not a separate STATUS column (lazygit colors the
  commit hash; k9s colors the whole row). No smooth gradients: they need truecolor and don't
  survive terminal cells (k9s dials are exactly green→orangered).
- **Red appears only at the point of failure** — at most one red element per panel; everything
  explaining it stays normal/dim (clig.dev). An all-red panel hides which value matters.
- **Bold for emphasis, dim for de-emphasis, never italics** (poor terminal support). Text
  hierarchy = dim metadata / plain data / bold emphasis — three tiers, no more.

## Never color alone

- **Every semantic color carries a redundant glyph or word**: `92% !`, `✓ fresh`, `? unknown` —
  meaning must survive NO_COLOR, grayscale, and color-blindness (Bloomberg terminal UX; oxker's
  `✓ running` / `✖ dead`). Provenance states map naturally: Fresh `✓`, Stale `!`, Missing `?`,
  Off `✖`.
- Honor `NO_COLOR` (present and non-empty ⇒ no color, but bold/dim may stay) and non-TTY
  streams in CLI output paths (`once --json`, `doctor`); per-app flags override the env var.

## Layout & borders

- **One border weight for the whole app**: single-line box drawing. Differentiate panels by
  border *color* (accent = section identity, dim = secondary), never by switching to
  double/heavy styles. Focus, if a pane can have it, = recolor + bold the border — geometry
  never changes (lazygit `activeBorderColor [green, bold]`, k9s `focusColor`).
- **Titles live in the border line** and may carry status: `Block` accepts multiple aligned
  titles — name left, provenance centered, reset-time right — zero extra rows (ratatui block
  recipe; k9s titles embed item counts and active filters).
- **Whitespace before another border.** Group with a blank line and dim text first; a border is
  for the section boundary, not every subgroup (Charm ships border-free components; the only
  bordered lipgloss element is the modal dialog). Never butt text against a border — 1-space
  inner padding minimum.
- Adjacent bordered panels share an edge: `Spacing::Overlap(1)` + `merge_borders` — kills the
  doubled-border seam and buys back a row/column at 80×24 (ratatui collapse-borders recipe).

## Tables

- **No gridlines, no vertical rules.** The pane border is the only box; the header row is
  distinguished by intensity (dim or bold — pick one per app and keep it), not by a divider
  (k9s: zero gridlines; bubbles: bold header + padding is the whole treatment).
- **Right-align numerics, left-align text, fixed-width formats so cells never jitter** as values
  tick: `{:>8.2}`-class formats, `$NN.NN`, `NN.N%`, `Nh NNm` — one format per column, decided
  once (oxker's `01.50%`; A List Apart on numeric alignment).
- **Explicit column constraints always**: `Length(n)` for numeric/fixed columns, `Fill(1)` for
  the one elastic identity column, constant column gap (ratatui: unset widths render 0-wide).
- **Overflow = ellipsis truncation at exact width** (`…`), middle-truncate long identifiers so
  both ends survive (bandwhich); wrap never, bleed never. Truncation of user-supplied strings
  must be display-width-aware (CJK/emoji are 2 cells — measure, don't `len()`).
- **Absent value = dim `—`**, same width as the column — never blank (blank reads as broken),
  never `0` (zero is data).
- Paired metrics share a column with a ` / ` separator (`5h / weekly`) instead of two columns
  (bandwhich's `Rate (Up / Down)`).
- **No emoji or double-width chars inside aligned cells or bordered panels** — terminals
  disagree on emoji width and one shears the column and breaks the right border (Textual's
  Will McGugan, lesson #1). Personality lives in titles/footers; in-grid glyphs come from the
  verified single-width set: `✓ ✗ ○ ● ◐ ▸ ! ? ↑ ↓ —`.

## Gauges & meters

- **A gauge always shows its track**: unfilled portion renders as a dim/dark-gray bed, so 30%
  and 90% occupy the same visual width (btop `meter_bg`). In ratatui: `Gauge` with a DarkGray
  background style.
- **The printed number rides with the bar** — `NN%` label on or next to the fill; the bar is the
  glance, the number is the truth and the color-blind fallback.
- Fill color follows the severity ramp keyed off the same threshold the alerts use.
- `use_unicode()` eighth-blocks for smooth narrow gauges; `LineGauge` for one-bar-per-row
  tables. Prefer block glyphs with an ASCII fallback over braille (WSL font roulette; btop
  ships `graph_symbol = braille|block|tty` for a reason).

## States (empty / loading / error / stale)

- **Never render a silent void** (clig.dev). Empty section = a single quiet dim sentence in the
  content area — no box, no icon, no color (`awaiting manual figures`, `no snapshots yet —
  collector warming up`; bubbles `NoItems` style).
- **Loading keeps the last data on screen** with an inline dim age/spinner note — no modal, no
  blanking, nothing else dims (lazygit's inline `|/-\` spinner next to the loading item).
  Matches the degrade-to-last-good model: value stays, provenance note changes.
- **Errors scope to the affected row/cell**, dim-or-yellow note + one red element max; details
  on demand, not a permanent banner (k9s Ctrl-Z toggleable fault info).
- **Stale escalates in the note, not the value**: value keeps last-good, the provenance word
  goes yellow (`! file stale`), and layout never jumps between healthy and degraded states
  (glow's error chip = same geometry as the healthy chip, recolored).
- If selection exists: reversed-video bar + `▸` prefix; severity stays on the fg glyph so a
  selected critical row still reads critical (atuin #2663: never dress selection in a severity
  hue). Unfocused panes drop selection to bold-only (lazygit).

## Density & small terminals

- **Design to an 80-column floor, 120-column ideal.** The layout skeleton is fixed — panels
  never move or reflow; spatial memory is the navigation. Degrade by *dropping* — columns first
  (least-identifying first: auxiliary IDs before metrics before names), then notes, then whole
  chrome sections — at hard-coded breakpoints, not computed "fit" (bandwhich's `<50 / <80 /
  80+` column arms; the existing Full/Compact/Micro tiers are this rule).
- Below the absolute minimum, a "terminal too small (WxH, need 80×24)" screen — which **must
  still process `q`/Ctrl-C** (btop #926 trapped users on exactly this screen).
- Density beyond the automatic tiers is a user toggle, not a designer guess (gh-dash
  `table.compact`; bottom's basic mode).

## Footer & help

- **Footer keymap = `key` in accent + label in dim, pairs separated by dim `·`, one line,
  only the 3-5 most-used keys.** The full reference lives behind `?` (k9s `keyColor` +
  dim labels; bubbles short-vs-full help). A footer listing 12 keys is a help screen leaking
  into chrome.
- Derive footer and help from the same keymap table the event loop dispatches on — they cannot
  drift if they share a source (bubbles generates both from `key.Binding`).
- An unavailable action may stay visible dimmed (DarkGray) rather than vanish — discoverability
  without lying (gitui `disabled_fg`).

## Numbers, money, time

- Money: fixed 2-decimal, currency glyph attached (`$35.66`), right-aligned; never mix
  currencies into a total. Real-dollar and notional figures must be visually disjoint grammars.
- Countdowns: one fixed grammar app-wide (`Nh NNm`), absolute times rendered verbatim from the
  source — never recomputed into a second timezone mid-render.
- Ages: `Nm ago` dim, next to the figure they date. Freshness is metadata — dim until it's a
  problem, yellow when it is.

---

## Adjudications (where good apps disagree — the ruling here)

Research surfaced genuine contradictions between excellent TUIs. Rulings for this app:

| Dispute | Schools | Ruling |
|---|---|---|
| Selection style | full bg bar (k9s/gitui) vs left-bar + accent fg (Charm) vs reverse+glyph (ratatui/yazi) | reverse-video + `▸` glyph; severity stays on fg |
| Severity resolution | 2-color (k9s) vs 3-band (trippy/yazi) vs 101-step gradient (btop) | 3-band green/yellow/red; gradients need truecolor |
| Palette base | ANSI-16 (ratatui) vs hex ramps (Charm/btop) | ANSI-16; hex only behind a truecolor check |
| Table header | dim same-bg row (k9s) vs inverted band (trippy) vs bold+margin (bubbles) | intensity-only (dim), no band, no divider |
| Row separators | none (k9s) vs faint rules (gh-dash) | none; whitespace and color do the work |
| Narrow terminals | re-stack panels (bandwhich) vs fixed skeleton, drop detail (minimal-design) | fixed skeleton, drop columns/notes at breakpoints |
| Focus signal | border recolor (lazygit/k9s/bottom) vs shape change (lipgloss tabs) | border recolor for panes; shape only if real tabs ever exist |
| Error surface | red bg chip (glow) vs one red cell + toggleable detail (k9s/clig) | scoped red + dim note; no banners |
| Row color scope | whole-row state hue (k9s) vs per-field fixed hues (gitui) | per-field hues; whole-row only for selection |

Known gaps the sources don't cover (decide deliberately if they come up): resize-transition
behavior, light-background terminals (ANSI-16-first mostly covers it), copy-paste friendliness
across borders, mouse policy (this app: keyboard-only, deliberately), scroll-position stability
when rows reorder under a cursor.
