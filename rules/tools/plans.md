---
rule: plans
title: Plans Folder Convention
category: tools
scope: [workflow, planning, docs]
applies-to: [all]
priority: required
tags: [plans, planning, process, conventions, docs]
created: 2026-05-16
---

# Plans Folder

> [!info] Global agent rule
> Planning documents live in `plans/NNN-<kebab-topic>/`. Every plan folder has a `README.md` with frontmatter. The 3-digit `NNN-` prefix is a sequential identifier so `ls plans/` reads as a timeline. Loose `.md` files at `plans/` root are not allowed. Runtime code never lives in `plans/`.

Part of [[Global Code Settings]].

---

## When this rule applies

- You are about to create a new `plans/` folder
- You are about to add a markdown file under `plans/`
- You are renaming, archiving, or restructuring an existing plan folder
- You are reviewing a PR that touches `plans/`

---

## Folder naming

**Form:** `NNN-<kebab-topic>(-vN)?`

- 3-digit zero-padded sequential prefix `NNN-` (001–999). Reflects first-commit order across the lifetime of `plans/`.
- Topic in lowercase kebab-case — letters, digits, hyphens only.
- Optional `-vN` suffix (kebab + lowercase v + integer) for explicit topic versions on the same subject.

**Regex:** `^[0-9]{3}-[a-z0-9]+(-[a-z0-9]+)*$`

**Picking the next number:**

```bash
ls plans/ | grep -E '^[0-9]{3}-' | tail -1   # see the highest existing number
# increment, zero-pad to 3 digits, that's your prefix
```

Numbers are stable identifiers. **Once assigned, a number is never reused.** When a plan is archived (moves to `plans/_archive/`), its number stays with it — `ls plans/` shows a gap. That's intentional.

**Tiebreaker for same-day commits:** alphabetical by topic name. So if two plans land same day, the one whose topic sorts first gets the lower number.

**Examples:**

| ✅ | Why |
|---|---|
| `009-pilot-operations-hub/` | Standard form |
| `016-code-audit-v2/` | Versioned with `-vN` suffix |
| `004-billing-service/` | Multi-word topic |
| `010-upgrade-deps/` | Single-word topic, no version |

| ❌ | Why |
|---|---|
| `pilot-operations-hub/` | Missing `NNN-` prefix |
| `09-pilot-operations-hub/` | 2-digit prefix — must be 3 |
| `009-PilotOperationsHub/` | PascalCase topic — must be kebab |
| `009-pilot-operations-hub-v2/` | If `009-` is taken, pick the next number; don't `-v2` to dodge a number conflict |
| `lint-rules-tightening.md` | Loose `.md` at `plans/` root — wrap in a numbered folder |
| `2026-05-16-upgrade-deps/` | Date prefixes not used — `NNN-` carries ordering |

**Naming rules:**

1. **Lowercase only.** Letters, digits, hyphens.
2. **No date prefixes anywhere.** The `NNN-` prefix carries chronological order; created-date lives in frontmatter.
3. **Topic must be in the name.** `001/` is forbidden; `001-marketing-homepage/` is fine.
4. **Versioning uses `-vN` suffix** (kebab + lowercase v + integer). The first version doesn't get retroactively renamed to `-v1`.
5. **No spaces, no underscores, no PascalCase, no camelCase, no acronyms-in-caps.**
6. **Folder name (topic part) is frozen on first `status: active` commit.** Rename freely while `status: draft`. After active, prefer "supersede + cross-link" over renaming.
7. **The `NNN-` prefix is frozen as soon as the folder exists.** Don't renumber.

---

## Folder contents

**Every plan folder must have `README.md`.** No exceptions.

**Two shapes are supported:**

### Single-doc plan (minimal)

```
plans/NNN-my-plan/
  README.md            # frontmatter + content
```

Use when the plan is fundamentally one decision or one short investigation.

### Multi-doc plan (default for ≥2 docs)

```
plans/NNN-my-plan/
  README.md            # index + frontmatter — mandatory
  00-SYNTHESIS.md      # OR 00-INDEX.md — the anchor doc
  01-brainstorm.md
  02-discovery.md
  03-prd.md
  04-pre-mortem.md
  ...
  evidence/            # optional subfolders for spike data, screenshots, prototypes
  prototypes/
```

**Inside-folder rules:**

1. **`README.md` is mandatory** in every plan folder.
2. **No loose `.md` files at `plans/` root.** Wrap single-file plans in a numbered folder.
3. **`00-` is reserved for the anchor doc.** Use `00-SYNTHESIS.md` when a decision is captured. Use `00-INDEX.md` when the folder is pure navigation. Don't have both.
4. **`01..NN` are sequential by reading order**, not strict dependency. Gaps are allowed (skip numbers for docs you didn't produce).
5. **Subfolders for evidence are allowed and encouraged.** Standard names: `evidence/`, `data/`, `spike-data/`, `prototypes/`, `attachments/`, `examples/`. Same kebab-case rules apply to subfolders (no `NNN-` prefix on subfolders — that's only for the top-level plan folder).

---

## Frontmatter (required on every `README.md`)

```yaml
---
plan: NNN-folder-name                # required, string, must match folder name exactly
status: draft|active|superseded|archived  # required, enum
owner: <single human name>           # required, string — no "team"
created: YYYY-MM-DD                  # required, ISO date (historical creation date)
type: feature|research|rule|cleanup|investigation|other  # required, enum
supersedes: NNN-other-plan           # optional, only if this plan supersedes another
superseded_by: NNN-other-plan        # optional, only on archived plans replaced by a successor
---
```

**Five fields required, two optional. Don't add more.**

---

## Lifecycle states

```
[draft] ──→ [active] ──→ [archived]
                │
                └─→ [superseded] ──→ [archived]
```

| State | Lives at | When |
|---|---|---|
| `draft` | `plans/NNN-name/` | Ideation. Topic name not yet frozen (number is). |
| `active` | `plans/NNN-name/` | Implementation queued or in progress. Both number and topic name frozen. |
| `superseded` | `plans/_archive/NNN-name/` | A later plan replaces this. Add `superseded_by`. |
| `archived` | `plans/_archive/NNN-name/` | Done, dropped, or moot. |

**Move-to-archive command:**

```bash
git mv plans/NNN-name plans/_archive/NNN-name
# edit README.md frontmatter: status: archived (or superseded)
git commit -m "plans: archive NNN-name"
```

`_archive/` is **one flat folder**, not per-year. Numbers stay with archived plans (the gap in `plans/` is intentional, not a bug to renumber away).

---

## What never lives in `plans/`

- ❌ Runnable code with its own dependency manager (`requirements.txt`, `package.json`, `Cargo.toml`)
- ❌ Compiled artifacts (`__pycache__/`, `*.pyc`, `*.duckdb`, `*.sqlite`, `.next/`, `dist/`, `target/`)
- ❌ Test caches (`.pytest_cache/`, `coverage/`)
- ❌ Agent runtime data (`.claude-flow/`, `.claude/`, agent session state)
- ❌ Secrets or `.env*` files
- ❌ Files >200 KB (upload to a cloud bucket, reference by URL)
- ❌ Sub-folders that mirror runtime source paths (e.g., `plans/NNN-foo/src/`)
- ❌ Implementation deliverables (those go in `specs/`, `src/`, or `docs/`)

**If a plan grows runtime code:** stop. Relocate the runtime to its real path (`src/`, a sibling package, or a sibling repo). The plan references it via cross-link.

---

## Plans vs specs vs docs vs handoffs

| Where | Purpose | Lifecycle |
|---|---|---|
| `plans/NNN-name/` | Exploratory thinking that produces a decision or a spec. Brainstorm, PRD, pre-mortem. | Plans get archived once their output (a spec, a shipped feature, an abandoned direction) is captured elsewhere. |
| `specs/<name>.md` | The contract between intent and implementation. `Draft → Active → Deprecated`. | Specs live forever as the source of truth for what was built. |
| `docs/<area>/` | Reference material — how to use the system, where things are, integration inventory. | Docs are maintained continuously. |
| `docs/handoff/YYYY-MM-DD-...md` | Time-stamped narrative when pausing mid-task. | Handoffs accumulate. They're not plans, they're context dumps. |

**Quick test:** if you're trying to *decide what to build*, that's a plan. Once you've decided, the output is a spec. Once you've built it, it's described in docs.

---

## Templates

### Single-doc plan README

```markdown
---
plan: NNN-my-thing
status: draft
owner: the maintainer
created: 2026-05-16
type: cleanup
---

# My Thing — planning folder

**Goal:** <one sentence>

**Status:** <one-sentence elaboration>

**Trigger:** <link to handoff / Asana / conversation>

## Decision

<the content>
```

### Multi-doc plan README

```markdown
---
plan: NNN-my-thing
status: draft
owner: the maintainer
created: 2026-05-16
type: feature
---

# My Thing — planning folder

**Goal:** <one sentence>

**Status:** Draft — planning only.

**Trigger:** <link>

## Read order

| # | Doc | What it does |
|---|-----|--------------|
| **00** | [SYNTHESIS](./00-SYNTHESIS.md) | 5-minute decision summary |
| 01 | [Brainstorm](./01-brainstorm.md) | <1-line description> |

## Out of scope

- <what this plan deliberately does NOT cover>

## Cross-references

| Path | What |
|---|---|
| `specs/<file>` | The spec this plan feeds |
| `src/app/<path>` | Implementation surface |
```

---

## Cross-references in commit messages and external systems

When you cite a plan from outside `plans/` (in a commit message, Asana task, Slack message, ADR), use the full `NNN-name` form. The number is the stable identifier — even if the topic gets renamed later, the number doesn't change.

**Good:**
- "Implements `plans/009-pilot-operations-hub/08-rollout-plan.md`"
- "See plan 014 for the deal copy decisions"

**Bad:**
- "Implements the pilot-operations-hub plan" — ambiguous if a future v2 exists
- "Implements plan #9" — drop the leading zeros and you can't grep

---

## Migration policy

**For new plans:** follow this rule from day one. Pick the next number, use kebab topic.

**For existing non-compliant plans (e.g., a sibling repo that hasn't migrated yet):** rename in one sweep, not opportunistically. Sequential numbering only works if the whole `plans/` folder participates — you can't half-number it.

**Before a sweep:**

```bash
git grep -l "plans/<old-name>"
grep -r "plans/<old-name>" docs/ specs/ data/ src/ functions/
```

**What to sweep:** live surfaces (`docs/` except `handoff/`, `specs/`, `src/`, `functions/`, `data/`, `workflows/`, config files, internal plan-to-plan cross-refs).

**What to leave frozen:** `CHANGELOG.md`, `CHANGELOG.archive.md`, `docs/handoff/*` — these are historical artifacts. Their references to old paths reflect what was true at the time. Links will break for old entries; that's correct behavior.

---

## Enforcement

A linter script at `scripts/lint-plans.{sh,mjs}` may be added per-project. Until one exists, this is a code-review rule — PR reviewers check:

- Folder name matches `^[0-9]{3}-[a-z0-9]+(-[a-z0-9]+)*$`
- `README.md` present
- Frontmatter has 5 required fields
- `plan:` field matches folder name
- No runtime artifacts (`__pycache__/`, `*.pyc`, `*.duckdb`, `.claude-flow/`, etc.)
- No loose `.md` at `plans/` root
