---
rule: _index
title: Tokenomics — Coding Rules
type: index
version: 1.0
---

# Tokenomics — Coding Rules

> Rules this project is built against. Ported subset of an internal rules vault
> plus Rust/ratatui rules authored for this repo.
> Check each rule's `applies-to` frontmatter; skip what doesn't match the task.

**Read [[crossroads]] first** to route your current task to the right rule.
**Every source file** follows [[file-headers]].

## Always (required)

| Rule | What |
|---|---|
| [file-headers](file-headers.md) | Structured `//!` module header on every file |
| [rust/strict-lints](rust/strict-lints.md) | Pinned toolchain, forbid unsafe, clippy pedantic `-D warnings`, the `check` gate |
| [security/owasp-compliance](security/owasp-compliance.md) | Baseline security posture |
| [security/security-testing](security/security-testing.md) | Security test expectations |

## Rust (this crate)

| Rule | What |
|---|---|
| [rust/ratatui-architecture](rust/ratatui-architecture.md) | model/view/keys seams; pure render; RAII terminal; TestBackend |
| [rust/subprocess-safety](rust/subprocess-safety.md) | argv not shell; timeouts; pgid/cgroup kills; defensive parsing |
| [rust/async-tokio](rust/async-tokio.md) | never block the UI task; channels; cancellation |
| [rust/error-handling](rust/error-handling.md) | `thiserror` libs / `anyhow` boundaries; no `unwrap` in runtime paths |
| [rust/ownership-borrowing](rust/ownership-borrowing.md) | borrow discipline, lifetimes |
| [rust/module-structure](rust/module-structure.md) | module layout |
| [rust/naming](rust/naming.md) | naming conventions |
| [rust/api-guidelines](rust/api-guidelines.md) | idiomatic public APIs |
| [rust/anti-patterns](rust/anti-patterns.md) | what not to do |

## Design

| Rule | What |
|---|---|
| [tui-design](tui-design.md) | TUI visual design house style — color budget, tables, gauges, states, density (evidence-cited from k9s/lazygit/btop/Charm/ratatui ecosystem) |

## Clean code (all languages)

[clean-code/](clean-code/) — DRY, SOLID, deep-modules, design-twice, obvious-design,
readability-patterns, research-and-reuse, general-purpose, boolean-hell, component-size.

## Testing

[testing/](testing/) — first-principles, test-organization, test-quality, mock-boundaries,
mocking-strategy, trophy-model, factories. This project is **spec-driven TDD**: red → green →
refactor-for-specs → refactor-for-rules per the plan.

## Dev workflow (tools)

[tools/](tools/) — changelog-workflow, git-commit-workflow, linting-workflow, when-to-lint,
plans, environment-config.
