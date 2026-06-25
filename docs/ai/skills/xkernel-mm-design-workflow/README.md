# X-Kernel MM Design Workflow

This skill package is the orchestration layer above the existing memory design
knowledge and the controlled implementation pipeline that follows a frozen
design.

It does not replace:

- `docs/ai/skills/linux-mm-design-knowledge/`
- `mm/docs/linux-aligned-final-architecture.md`
- crate-local memory design documents

It tells the agents how to use those materials together.

## What This Adds

- strict role boundaries for design agents;
- strict role boundaries for implementation, review, and validation agents;
- a fixed phase order;
- review and arbitration rules;
- canonical templates for design outputs;
- canonical templates for implementation outputs;
- a recommended first runnable topic.

## Recommended First Topic

Run the workflow first on:

- `VMA + mmap/munmap/mprotect + page fault + private COW`

Why this topic first:

- it defines the user-visible address-space core;
- it constrains `MmSpace`, `VmArea`, page-table integration, and fault flow;
- it is small enough to freeze without dragging in reclaim, swap, or memcg.

## Suggested Artifact Storage

Generated outputs from a workflow run should live outside this skill directory.

Suggested location:

```text
docs/ai/design-runs/mm/<topic>/
```

Do not store workflow run artifacts under `mm/docs/`. That directory is for
current memory architecture documentation, not process records.

Suggested files:

- `01-topic-brief.md`
- `02-linux-baseline.md`
- `03-xkernel-adaptation.md`
- `04-cross-review.md`
- `05-arbiter-decisions.md`
- `06-frozen-design.md`
- `07-task-split.md`
- `08-post-run-audit.md`

If implementation is explicitly started from the frozen design, continue in
the same run directory:

- `09-implementation-scope.md`
- `10-implementation-report.md`
- `11-code-review.md`
- `12-validation-report.md`
- `13-implementation-audit.md`

Implementation should select one small slice from `07-task-split.md`, not an
entire subsystem.
