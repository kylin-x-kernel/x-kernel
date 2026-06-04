---
name: test-harness
description: Use when adding, running, or reviewing user-space regression tests for X-Kernel with the shared Starry Test Harness repository, especially for syscall semantics, CI/iter suite registration, guest-test case authoring, and result/log verification.
---

# Test Harness

Use this skill when an AI agent needs to:

- add or update a user-space regression test for X-Kernel;
- register a case in the Starry Test Harness suites;
- run one or more harness cases locally;
- inspect harness logs, artifacts, and result files;
- review whether a proposed syscall test is meaningful and non-redundant.

This skill primarily covers the shared Starry Test Harness repository:

- `git@gitee.com:openkylin/starry-test-harness.git`

In the current development environment, that repository is checked out at:

- `/home/laoyekang/code/starry-test-harness`

Load the detailed harness workflow from:

- [references/starry-test-harness.md](references/starry-test-harness.md)

## Core Rules

### 1. Treat syscall tests as behavior contracts, not white-box probes

For syscall-facing user-space tests:

- test observable ABI behavior;
- derive expectations from Linux/POSIX syscall contracts first;
- prefer standards/man-page semantics over current kernel implementation details;
- do not assert internal implementation structure, private timing assumptions,
  or incidental log messages unless the contract explicitly requires them.

If behavior is uncertain:

- first inspect existing harness cases and local kernel docs;
- then verify against Linux/POSIX manual semantics before inventing a new expectation.

### 2. Avoid redundant cases

Before adding a new case:

- search existing harness tests under
  `<starry-test-harness>/tests/cases/tests/`;
- search suite registrations under
  `<starry-test-harness>/tests/*/suite.toml`;
- check whether an existing case already covers the same syscall contract,
  failure mode, or concurrency dimension.

Only add a new case when it contributes at least one new axis, such as:

- a distinct contract branch;
- an architecture-sensitive semantic;
- a concurrency/reentrancy condition;
- a resource-lifecycle invariant;
- a regression for a previously fixed bug.

If the new behavior fits an existing case naturally, extend that case instead of
creating a sibling test with overlapping coverage.

### 3. Prefer the smallest suite that matches the goal

- use `ci-test-iter` for new or unstable regression cases;
- move stable, high-signal cases into `ci` only after they prove useful;
- use `daily` or `longevity` only for performance, soak, or long-running coverage;
- use single-case `CASES=...` runs for local iteration.

### 4. Keep guest tests self-describing

Guest Rust tests should:

- use clear test names that describe the semantic being checked;
- fail with contract-oriented assertions, not vague “works/doesn't work” checks;
- isolate setup/verification/cleanup cleanly;
- use `test-utils` helpers when they reduce boilerplate without hiding intent.

## Standard Workflow

1. Identify the contract to validate.
2. Search for overlapping harness cases and local regression coverage.
3. Choose the smallest appropriate suite, usually `ci-test-iter`.
4. Add or update the guest test case.
5. Register or adjust the suite entry in `suite.toml`.
6. Run the narrowest useful local harness command, usually one case.
7. Inspect `suite.log`, per-case logs, and any `artifacts/<case>/results.json`.
8. If the case is syscall-focused, confirm the assertions match the syscall contract rather than an implementation accident.

## When Writing New Syscall Cases

Use this checklist:

- What exact syscall contract branch is being tested?
- Is this already covered by an existing semantics/smoke case?
- Does the test assert Linux/POSIX-visible behavior only?
- Does the test avoid depending on unrelated subsystem state?
- Is the case name specific enough to describe the regression or semantic?
- Should this be folded into an existing `*_semantics.rs` file instead of a new file?

## Validation Expectations

At minimum, report:

- the harness command you ran;
- the suite and case name;
- whether the case executed or only injected/built;
- where the relevant logs or artifacts were produced;
- any remaining gap, such as not running cross-arch or host-companion paths.
