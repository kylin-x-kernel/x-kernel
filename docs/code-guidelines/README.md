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
- `CG-*.md`: the rule registry for all backends, indexed below. Reused ids
  CG-016, CG-018 and CG-021 receive their AI checks when the stack lands;
  CG-017 stays the separate Send/Sync argument rule.
- `pending.md` (stack placeholder): Dylint candidates. Add its link when
  part 2 lands. Rustc/Clippy candidates stay in `lints.md`.

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
| [CG-016-safety-comment-substance](CG-016-safety-comment-substance.md) | MUST | clippy | `undocumented_unsafe_blocks` |
| [CG-018-uninitialized-storage-model](CG-018-uninitialized-storage-model.md) | MUST | clippy | `disallowed_methods` |
| [CG-021-no-panic-on-reachable-failure](CG-021-no-panic-on-reachable-failure.md) | MUST | clippy | `panic`, `unwrap_used`, `expect_used` |
| [CG-023-unfinished-implementations](CG-023-unfinished-implementations.md) | MUST | clippy | `todo`, `unimplemented` |
| [CG-024-lint-exceptions](CG-024-lint-exceptions.md) | MUST | clippy | `allow_attributes`, `allow_attributes_without_reason` |
| [CG-025-minimal-unsafe-scope](CG-025-minimal-unsafe-scope.md) | MUST | clippy, rustc | `multiple_unsafe_ops_per_block`, `unsafe_op_in_unsafe_fn` |
| [CG-026-unsafe-api-safety-docs](CG-026-unsafe-api-safety-docs.md) | MUST | clippy | `missing_safety_doc` |
| [CG-027-narrow-visibility](CG-027-narrow-visibility.md) | SHOULD | rustc | `unreachable_pub` |
| [CG-028-focused-functions](CG-028-focused-functions.md) | SHOULD | clippy | `too_many_lines` |
| [CG-029-shallow-control-flow](CG-029-shallow-control-flow.md) | SHOULD | clippy | `excessive_nesting` |
| [CG-030-explicit-boolean-parameters](CG-030-explicit-boolean-parameters.md) | SHOULD | clippy | `fn_params_excessive_bools` |
| [CG-031-intrusive-linked-lists](CG-031-intrusive-linked-lists.md) | SHOULD | clippy | `linkedlist` |
| [CG-032-camel-case-types](CG-032-camel-case-types.md) | SHOULD | clippy | `upper_case_acronyms` |
| [CG-033-explicit-result-discard](CG-033-explicit-result-discard.md) | SHOULD | clippy | `unused_result_ok` |
| [CG-034-used-binding-names](CG-034-used-binding-names.md) | SHOULD | clippy | `used_underscore_binding` |
| [CG-035-declared-cfgs](CG-035-declared-cfgs.md) | SHOULD | rustc | `unexpected_cfgs` |
| [CG-036-ordinary-error-propagation](CG-036-ordinary-error-propagation.md) | SHOULD | clippy | `question_mark` |

Ids CG-001–022 are reserved for the Dylint/AI stack. Reused files here are
CG-016 (unsafe comments), CG-018 (initialization validity), and CG-021
(reachable-failure panics). CG-017 is not reused: its Send/Sync concurrency
argument is more specific than comment presence. The first free id is CG-037.

## Adding a rule

First choose the backend. Rustc/Clippy checks inherit `[workspace.lints]` in
root `Cargo.toml`; parameters live in root `clippy.toml`. Validate through
`make clippy` after preparing a platform defconfig, as described in
`docs/ai/skills/build-workflow/SKILL.md`. Dylint candidates will be listed in
`pending.md`; landing one removes its row. AI checks describe judgments the
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
