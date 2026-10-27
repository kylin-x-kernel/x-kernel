# Code Guidelines

Machine-checkable code rules, one rule per file, each with the human
explanation. The YAML frontmatter is read by tooling such as x-review; the body
is written for people. English only.

## Frontmatter

| Field | Meaning |
| --- | --- |
| `id` | `CG-<nnn>-<descriptive>`; equals the filename without `.md`. |
| `level` | `MUST` or `SHOULD`. |
| `paths` | Glob patterns the rule applies to, e.g. `["**/*.rs"]`. |
| `checks` | List of `{ backend, lint }`. `backend` is `rustc`, `clippy`, `dylint` or `ai`; `lint` lists the lint names that enforce the rule (cargo JSON `code.code`). |
| `triggers` | Optional list of case-sensitive ECMAScript regular expressions matched against added/modified diff lines in files matched by `paths`; read by x-review-ng, ignored by lint backends. |

## Body

`# <id>: <title>`, then bullets in this order: **Rule**, **Scope**, **Why**,
**How to check** (numbered steps), **Exceptions**, **Counter-example**
(code block plus one sentence), **Example**. Keep it short; runtime details
belong to the tool's own README.

## Index

- [lints.md](lints.md): rustc/Clippy levels, configuration, coverage, pending
  candidates, auxiliary checks, and open questions.
- `CG-*.md`: the rule registry for all backends, indexed below. Rules with
  mechanical and AI checks state both the enforced condition and the remaining
  judgment; CG-017 separately covers Send/Sync arguments.
- [pending.md](pending.md): Dylint candidates; landing a lint removes its row.
  Rustc/Clippy candidates stay in [lints.md](lints.md).

## Suppressing a check

Use the narrowest scope and start every reason with the full rule id:

```rust
#[expect(clippy::unwrap_used, reason = "CG-021-no-panic-on-reachable-failure: <specific invariant and why it holds>")]
```

The default is `#[expect(<lint>, reason = "<rule-id>: <why>")]`. Use `allow`
only where an expectation is unreliable: cfg-, feature- or platform-dependent
hits, macro-generated code, or Dylint lints unknown to ordinary Clippy.
An outer `allow` must also satisfy the workspace's `clippy::allow_attributes`
deny by pairing it with an expectation at the same scope:

```rust
#[expect(clippy::allow_attributes, reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy")]
#[allow(unknown_lints, static_mut_option, reason = "CG-004-static-mut-option: <specific permitted exception>")]
static mut TEST_STATE: Option<u32> = None;
```

Dylint allowances include `unknown_lints` so ordinary `make clippy` accepts
custom lint names. The wrapper's reason identifies CG-024; the underlying
allowance identifies the substantive rule. The reason must satisfy that rule's
**Exceptions** and explain the actual invariant or constraint, rather than
say only "needed" or "legacy". Where a default auxiliary lint has no registry
rule, use `CG-024-lint-exceptions: <lint>: <why>`; this identifies the exception
policy without inventing a substantive rule for a default check. Module-docs
checks remain on their separate review track.

After the id, keep the established reason categories: `TODO(cg): <planned
repair>` for authorized outstanding repairs, `test scenario: <purpose>` for
helpers outside shared test macros, and `Fatal boot: <failure condition,
no recovery and no error-return channel>` for individually authorized boot
sites. Internal-invariant reasons name the invariant and why it holds. Rustc
ignores expectations on macro invocation statements such as `panic!();`;
place them on the smallest containing arm or item instead.

### Per-crate opt-outs

Keep `#![allow(<lint>, reason = "<rule-id>: case (a)/(b): <specific reason>")]`
at each actually linted target root. Case (a) means the rule does not apply
(e.g. intentional host build failure or a pure test crate). Case (b) means
at least ten legacy sites in the union of Jenkins configurations, unless the
rule sets a stricter threshold. Register the targets in the rule file. This
is not a quota for new code: new code in case (b) targets still follows the
rule. Do not exempt targets that are never linted. Inner allowances do not
need the outer `allow_attributes` wrapper.

A transitional crate-root expectation still requires an issue:
`#![expect(<lint>, reason = "<rule-id>: TODO(cg): legacy cleanup, tracked in <issue>")]`.
The fatal-boot policy stays per site; do not change boot interfaces or disable
boot crates merely to remove those expectations.

### Verification

A scratch crate using the pair above passed Clippy 1.95.0 with both allowance
lints denied and `-D warnings`, and the nightly-2026-03-08 Dylint driver with
`-D warnings`. The driver loaded the repository lint library: removing the
allowance produced `static_mut_option`. The Clippy tool-lint expectation was
**not** reported as unfulfilled by Dylint; no conditional wrapper or broad
`unfulfilled_lint_expectations` suppression is needed. The same pair is also
checked through the configured kernel's `make clippy` during this migration.

## Rules

| Rule | Level | Backend | Lints |
| --- | --- | --- | --- |
| [CG-001-static-spinlock-registration](CG-001-static-spinlock-registration.md) | MUST | dylint | `unregistered_static_spinlock` |
| [CG-013-no-sleep-under-spinlock](CG-013-no-sleep-under-spinlock.md) | MUST | ai | — |
| [CG-014-spin-noirq-needs-irq-contender](CG-014-spin-noirq-needs-irq-contender.md) | SHOULD | ai | — |
| [CG-015-no-split-critical-section](CG-015-no-split-critical-section.md) | MUST | ai | — |
| [CG-016-safety-comment-substance](CG-016-safety-comment-substance.md) | MUST | clippy, ai | `clippy::undocumented_unsafe_blocks` |
| [CG-017-unsafe-impl-send-sync-argument](CG-017-unsafe-impl-send-sync-argument.md) | MUST | ai | — |
| [CG-018-uninitialized-storage-model](CG-018-uninitialized-storage-model.md) | MUST | clippy, ai | `clippy::disallowed_methods` |
| [CG-019-user-pointers-stop-at-syscall-boundary](CG-019-user-pointers-stop-at-syscall-boundary.md) | MUST | ai | — |
| [CG-020-raw-output-buffer-capacity](CG-020-raw-output-buffer-capacity.md) | MUST | ai | — |
| [CG-021-no-panic-on-reachable-failure](CG-021-no-panic-on-reachable-failure.md) | MUST | clippy, ai | `clippy::panic`, `clippy::unwrap_used`, `clippy::expect_used` |
| [CG-022-log-level-matches-event](CG-022-log-level-matches-event.md) | SHOULD | ai | — |
| [CG-023-unfinished-implementations](CG-023-unfinished-implementations.md) | MUST | clippy | `clippy::todo`, `clippy::unimplemented` |
| [CG-024-lint-exceptions](CG-024-lint-exceptions.md) | MUST | clippy | `clippy::allow_attributes`, `clippy::allow_attributes_without_reason` |
| [CG-025-minimal-unsafe-scope](CG-025-minimal-unsafe-scope.md) | MUST | clippy, rustc | `clippy::multiple_unsafe_ops_per_block`, `unsafe_op_in_unsafe_fn` |
| [CG-026-unsafe-api-safety-docs](CG-026-unsafe-api-safety-docs.md) | MUST | clippy | `clippy::missing_safety_doc` |
| [CG-027-narrow-visibility](CG-027-narrow-visibility.md) | SHOULD | rustc | `unreachable_pub` |
| [CG-028-focused-functions](CG-028-focused-functions.md) | SHOULD | clippy | `clippy::too_many_lines` |
| [CG-029-shallow-control-flow](CG-029-shallow-control-flow.md) | SHOULD | clippy | `clippy::excessive_nesting` |
| [CG-030-explicit-boolean-parameters](CG-030-explicit-boolean-parameters.md) | SHOULD | clippy | `clippy::fn_params_excessive_bools` |
| [CG-031-intrusive-linked-lists](CG-031-intrusive-linked-lists.md) | SHOULD | clippy | `clippy::linkedlist` |
| [CG-032-camel-case-types](CG-032-camel-case-types.md) | SHOULD | clippy | `clippy::upper_case_acronyms` |
| [CG-033-explicit-result-discard](CG-033-explicit-result-discard.md) | SHOULD | clippy | `clippy::unused_result_ok` |
| [CG-034-used-binding-names](CG-034-used-binding-names.md) | SHOULD | clippy | `clippy::used_underscore_binding` |
| [CG-035-declared-cfgs](CG-035-declared-cfgs.md) | SHOULD | rustc | `unexpected_cfgs` |
| [CG-036-ordinary-error-propagation](CG-036-ordinary-error-propagation.md) | SHOULD | clippy | `clippy::question_mark` |
| [CG-037-spin-raw-context-precondition](CG-037-spin-raw-context-precondition.md) | MUST | ai | — |
| [CG-038-prepare-reusable-work-handles](CG-038-prepare-reusable-work-handles.md) | MUST | ai | — |
| [CG-039-execution-context-contract](CG-039-execution-context-contract.md) | MUST | ai | — |
| [CG-040-lock-stat-accounting](CG-040-lock-stat-accounting.md) | MUST | ai | — |
| [CG-041-reuse-safe-boundary-abstractions](CG-041-reuse-safe-boundary-abstractions.md) | SHOULD | ai | — |
| [CG-042-abi-carrier-semantic-conversion](CG-042-abi-carrier-semantic-conversion.md) | SHOULD | ai | — |
| [CG-043-reuse-once-lazy-lifecycle](CG-043-reuse-once-lazy-lifecycle.md) | SHOULD | ai | — |
| [CG-044-unsafe-precondition-assertions](CG-044-unsafe-precondition-assertions.md) | SHOULD | ai | — |
| [CG-045-non-time-integer-units](CG-045-non-time-integer-units.md) | SHOULD | ai | — |
| [CG-046-atomic-ordering-rationale](CG-046-atomic-ordering-rationale.md) | SHOULD | ai | — |
| [CG-047-kconfig-feature-ownership](CG-047-kconfig-feature-ownership.md) | MUST | ai | — |

### Candidates not yet ruled

The second AI batch defers these candidates; their text remains in the skill:

- R02, Name semantics, scans and observable work honestly:
  [naming-and-structure.md](../ai/skills/code-guidelines/naming-and-structure.md#names),
  [performance-and-resources.md](../ai/skills/code-guidelines/performance-and-resources.md#performance)
  and the [SKILL.md checklist](../ai/skills/code-guidelines/SKILL.md#review-checklist).
- R11, Keep the crate logging facade consistent:
  [error-handling-and-logging.md](../ai/skills/code-guidelines/error-handling-and-logging.md#logging)
  and the [SKILL.md checklist](../ai/skills/code-guidelines/SKILL.md#review-checklist).
- R12, Cite the external behavioral contract:
  [comments-and-rustdoc.md](../ai/skills/code-guidelines/comments-and-rustdoc.md#comments).
- R14, Keep contract comments true after behavior changes, and R15, Explain
  non-obvious intent rather than paraphrase code:
  [comments-and-rustdoc.md](../ai/skills/code-guidelines/comments-and-rustdoc.md#comments)
  and its [review checklist](../ai/skills/code-guidelines/comments-and-rustdoc.md#when-reviewing).

## Adding a rule

First choose the backend. Rustc/Clippy checks inherit `[workspace.lints]` in
root `Cargo.toml`; parameters live in root `clippy.toml`. Validate through
`make clippy` after preparing a platform defconfig, as described in
`docs/ai/skills/build-workflow/SKILL.md`. Dylint candidates are listed in
[pending.md](pending.md); landing one removes its row. AI checks describe judgments the
lint backends cannot prove. x-review-ng loads only `CG-*.md` and selects rules
with an `ai` check; files with only rustc/Clippy checks have no `triggers` and
are ignored by AI review.

For a Dylint rule:

1. Write the lint module `tools/lints/src/<lint>.rs`, including its embedded UI test.
2. In `tools/lints/src/lib.rs`, add `mod <lint>;` and register its lint and pass in `register_lints`.
3. Add `docs/code-guidelines/CG-<nnn>-<descriptive>.md` and its row in the table above.
4. Clean up the rule's existing violations (the backlog) in the same commit.

## Relationship to existing guidance

`docs/coding-guidelines/` and `docs/coding-guidelines-cn/` are the older
general guidelines. `docs/ai/skills/code-guidelines/` is the AI-facing skill.
Rules migrate here one at a time; when a rule is migrated, its prose is deleted
from its old location without leaving a per-rule pointer. The skill routes
writing and review to this directory; `lints.md` records backend coverage.
Read the registry rule for the full requirement and judgment criteria.
