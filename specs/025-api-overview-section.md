# Spec 025 — API overview section: the billing plane on the board (`o` to toggle)

Status: **Done**

## Motivation

Spec 024 lands real-dollar API billing figures (live store rows + manual TOML) with doctor as
the only surface. This spec renders them: a hideable section on the main board — per API
account one dense line with balance / owed / month-to-date spend, each figure carrying its
per-figure provenance. First real-money surface in the TUI, so the plane separation has to be
visible, not implied.

## Behaviour

### A. Toggle + placement

- `Action::ToggleApi` on key `o` (free; release-ignored like every binding). One `App` flag
  `show_api`, default **false** — the board is subscriptions-first, and default-hidden keeps
  every existing insta snapshot byte-identical. Session-only, no persistence (the `i` posture).
- The section renders as one chrome block in `render`'s constraint vec, after the agg-bar row,
  before the `Min(1)` body: `Length(1 + rows)` — a header line plus one line per configured
  `[[api]]` entry (plus manual-only divergent ids; see §B).
- Height-gated like fleet/agg: shown only when `show_api && !show_help` and the terminal is
  tall enough to keep the body floor; otherwise it silently drops. Toggling may legitimately
  flip account panels Full→Compact via `choose_tier` — the tier system absorbs it.
- Plane Off (no `[[api]]`, no manual path) + `o` pressed: header + one dim line
  `not configured — see tokenomics.toml [api] (tok doctor explains)`.

### B. Row grammar + states

- Header: `api · real $ — bills & balances, never notional` (`api` bold magenta — an accent no
  other plane uses; rest dim). Width-laddered literal like the title.
- Row: 2-space indent, `label` left-justified 10, then ` · `-joined segments **dropped from the
  right** to fit width (the `render_fleet` mechanism, extracted into a shared `fit_segments`
  helper — no new widget). A segment renders whole or not at all.
- Segment order: `bal $X.XX` → `owed €X.XX` → `mtd $X.XX` → provenance notes. Real figures are
  ALWAYS 2 decimals + currency glyph — a grammar disjoint from the notional proxy's, and real
  figures render only inside this section (the three-cue dogma: header text, grammar, locality).
  **No totals row** — mixed currencies don't sum.
- Provenance notes are plain-word segments, never color alone: `live <ago>` (dim; yellow when
  older than 2× `poll_billing_secs`), `manual <as_of>` (dim; `manual <as_of> · stale` yellow
  past the 35-day const), `waiting for collector` (figure expected from the live lane, no row
  yet), `dormant (<env var> unset)` (xai state). When a row mixes planes, each note names its
  figure (`mtd live 2m ago · bal manual 2026-07-20`) — a fresh poll must not imply a fresh
  manual figure.
- Merge rule (spec 024 §C): live wins per `(id, figure)`, manual fills gaps. Degraded live
  figures keep last-good value + aging note (store rows persist; honesty rides the age).
- A manual row whose `id` joins no `[[api]]` entry still renders (labelled by its id) — money
  the user wrote down doesn't vanish; doctor names the divergence (spec 024 §E).

### C. Model + wiring (pure model, I/O only in `tui/mod.rs`)

- New view types in `tui/model.rs`: `ApiSegment { text, color }`, `ApiRowView { name,
  segments }`, `ApiView { rows }`; pure `build_api_view(inputs, now, use_color)` builder beside
  `build_fleet_view`, unit-tested per state in §B. `Dashboard` gains `api: Option<ApiView>`
  riding the existing `Msg::Data` box; `App.show_api` + `handle` arm (no reload — data is
  rebuilt every tick anyway).
- `format_money(amount_minor, currency) -> String` in `src/format.rs` (2 dp, `$`/`€` glyph for
  USD/EUR, `<code> ` prefix otherwise); `format_dollars` untouched.
- `tui/mod.rs`: `LedgerRead` widens to a `PlanesRead` bundle (same clippy-arg-count dodge);
  the tick arm polls the manual TOML after `poll_ledger`, before `Msg::Tick` (order preserved);
  `read_rows` additionally reads `latest_api_billing()` and hands both to the builder.

### D. Surfaces

- Help: `o  toggle the api pay-as-you-go section (real dollars)` plus one line: "The api
  section shows REAL money (balances & bills) — never the notional proxy." Footer untouched
  (`?` is the discovery surface; a footer hint would re-approve five snapshots for one word).
- Snapshots: the existing insta set and the `tok once --json` / `tok accounts` R16 goldens stay
  **byte-identical** (default hidden; neither CLI path touched). New: `board_api` (full, all §B
  states) + `board_api_compact` insta snapshots; toggle/height-gate/unconfigured contains-tests;
  the NO_COLOR identical-grid test gains an api-on board.
- `tok doctor`: unchanged (spec 024 owns it).

## Non-goals

- Dollar-keyed alerts (future-spec candidate: "owed > $X"), persistence of `show_api`, a footer
  hint, per-model breakdown rows, totals, scrolling within the section, currency conversion.
- Any new data reads beyond `latest_api_billing()` + the manual TOML (the planes are spec 024's).

## Acceptance criteria

1. `o` toggles the section; default hidden; every pre-existing insta snapshot and both R16
   goldens byte-identical. (A/D)
2. `build_api_view` renders each §B state exactly: live fresh/aging, manual fresh/stale, mixed
   planes with per-figure notes, `waiting for collector`, dormant xai, unconfigured plane,
   divergent manual-only id. (B/C)
3. Segments drop from the right at narrow widths — never truncated mid-figure; `fit_segments`
   is shared with `render_fleet` (fleet snapshots unchanged). (B)
4. `format_money`: cents → `$12.34` / `€3.50` / `XYZ 1.00`; negative amounts render with a
   leading `-`. (C)
5. Toggle needs no reload; height gate drops the section silently on short terminals; help
   lists `o`. (A/D)
6. NO_COLOR grid with the section on is character-identical to the colored render. (D)
7. `./check.sh` green.
