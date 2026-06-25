# Runbook

Use this runbook to execute one full design cycle and, when explicitly
requested, one bounded implementation cycle.

## Step 0: Pick One Topic

Choose exactly one bounded topic.

Recommended first run:

- `VMA + mmap/munmap/mprotect + page fault + private COW`

Do not start with:

- full memory subsystem
- reclaim
- swap
- memcg
- NUMA
- THP

## Step 1: Freeze Scope

Create:

- `01-topic-brief.md`

Use:

- `templates/topic-brief-template.md`

Arbiter checks that the topic is narrow enough.

## Step 2: Produce Linux Baseline

Create:

- `02-linux-baseline.md`

Owner:

- Linux MM Expert

Use:

- `templates/linux-baseline-template.md`
- `docs/ai/skills/linux-mm-design-knowledge/`

## Step 3: Produce X-Kernel Adaptation

Create:

- `03-xkernel-adaptation.md`

Owner:

- X-Kernel Memory Designer

Use:

- `templates/xkernel-adaptation-template.md`
- `mm/docs/linux-aligned-final-architecture.md`
- crate-local memory docs

## Step 4: Run Cross Review

Create:

- `04-cross-review.md`

Use:

- `templates/review-findings-template.md`

Both experts must review each other.

## Step 5: Resolve Conflicts

Create:

- `05-arbiter-decisions.md`

Owner:

- Design Arbiter

Use:

- `templates/arbiter-decision-table-template.md`

## Step 6: Freeze Design

Create:

- `06-frozen-design.md`

Owner:

- Design Arbiter

Use:

- `templates/design-freeze-template.md`

## Step 7: Split Implementation Tasks

Create:

- `07-task-split.md`

Owner:

- Design Arbiter with X-Kernel designer input

Use:

- `templates/implementation-task-split-template.md`

## Step 8: Audit The Run

Create:

- `08-post-run-audit.md`

Use:

- `templates/post-run-audit-template.md`

The audit checks whether the workflow truly resolved review findings and
produced implementer-ready design artifacts.

## Step 9: Freeze Implementation Scope

Only run this after Step 8 is complete and the user explicitly asks to start
implementation.

Create:

- `09-implementation-scope.md`

Use:

- `templates/implementation-scope-template.md`

Select one bounded slice from `07-task-split.md`.

## Step 10: Implement The Selected Scope

Create:

- `10-implementation-report.md`

Owner:

- Implementation Coder

Use:

- `agents/implementation-coder.md`
- `templates/implementation-report-template.md`

The coder implements only the selected scope and records changed files.

## Step 11: Review The Implementation

Create:

- `11-code-review.md`

Owner:

- Code Reviewer

Use:

- `agents/code-reviewer.md`
- `templates/code-review-template.md`

The reviewer checks design conformance, ownership, safety, docs, and tests.

## Step 12: Validate The Implementation

Create:

- `12-validation-report.md`

Owner:

- Validation Tester

Use:

- `agents/validation-tester.md`
- `templates/validation-report-template.md`

Run relevant build, lint, unit, and guest tests according to repo workflow.

## Step 13: Audit Implementation

Create:

- `13-implementation-audit.md`

Use:

- `templates/implementation-audit-template.md`

The audit decides whether the implementation is ready, needs fixes, is blocked,
or requires reopening design.

## Stop Conditions

Stop and re-scope if:

- the Linux baseline still reads like a broad tutorial;
- the X-Kernel draft cannot state top-level owner and VMA/object boundaries;
- cross-review exposes unresolved core-model conflicts;
- the design still depends on reclaim/swap/memcg details that are out of scope.

Stop implementation and reopen design if:

- the coder needs to move responsibility across crate boundaries;
- validation requires changing the Linux compatibility floor;
- review finds a conflict with `06-frozen-design.md`;
- the selected task depends on unfinished earlier tasks.
