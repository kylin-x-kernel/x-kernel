# X-Kernel MM Design Workflow

Use this skill when the task is to design part of the X-Kernel memory
subsystem, or to implement a previously frozen X-Kernel memory design through
a controlled coder/reviewer/tester pipeline.

This is a multi-agent orchestration skill.
Its first goal is to force design-first convergence across three roles:

- Linux MM Expert
- X-Kernel Memory Designer
- Design Arbiter

Its second goal, only after design freeze, is to force implementation through
three additional roles:

- Implementation Coder
- Code Reviewer
- Validation Tester

It is not a bug-localization skill.
It is not a generic Linux MM tutorial.

## Goal

Produce X-Kernel memory subsystem design outputs that are:

- grounded in Linux source-level semantics where compatibility matters;
- adapted to X-Kernel's current Rust architecture and crate boundaries;
- explicitly reviewed for semantic gaps, over-engineering, and phase tradeoffs;
- frozen into design documents and implementation task splits before coding.
- if implementation is requested, implemented only against a frozen design and
  verified against an explicit validation plan.

## Hard Rules

- Do not implement code during phases 01 through 08.
- Do not enter implementation phases unless the user explicitly asks to start
  implementation from a frozen design run.
- Do not generate large code blocks.
- Do not modify production Rust source files during the design track.
- Do not skip directly from Linux reference analysis to implementation tasks.
- Do not merge conflicting expert opinions silently.
- Do not let a coder reinterpret the design contract; design changes must go
  back through the design arbiter.
- Do not let a tester weaken validation because code is hard to test.
- Every design decision must be tagged as one of:
  - `Linux-required`
  - `xkernel-adaptation`
  - `explicit-simplification`
  - `deferred-compatibility`

## Required Inputs

At minimum, each run needs:

- a concrete topic;
- a phase boundary;
- a statement of what is out of scope for this run.

Bad input:

- "design the whole memory subsystem"

Good input:

- "design VMA plus mmap/munmap/mprotect for phase 1"
- "design page-fault core path for anonymous and file-backed mappings"
- "design fork plus COW semantics for private mappings"

## Canonical Roles

Read these role specs before running the workflow:

- `agents/linux-mm-expert.md`
- `agents/xkernel-memory-designer.md`
- `agents/design-arbiter.md`
- `agents/implementation-coder.md` when starting implementation
- `agents/code-reviewer.md` when reviewing implementation
- `agents/validation-tester.md` when validating implementation

## Design Track Workflow Order

Run the phases in this exact order:

1. `workflow/01-topic-freeze.md`
2. `workflow/02-linux-semantic-baseline.md`
3. `workflow/03-xkernel-adaptation-draft.md`
4. `workflow/04-cross-review.md`
5. `workflow/05-conflict-resolution.md`
6. `workflow/06-design-freeze.md`
7. `workflow/07-implementation-task-split.md`
8. `workflow/08-post-run-audit.md`

Do not skip a phase.

## Implementation Track Workflow Order

Run these phases only after the design track is complete and the user asks to
start implementation.

9. `workflow/09-implementation-scope-freeze.md`
10. `workflow/10-coded-implementation.md`
11. `workflow/11-code-review.md`
12. `workflow/12-validation-test.md`
13. `workflow/13-implementation-audit.md`

Do not skip a phase.

Implementation must be task-scoped. One implementation run should select a
small contiguous slice from `07-task-split.md`, such as one phase task or a
short dependency chain. Do not implement an entire memory subsystem panorama in
one coding run.

## Knowledge Sources

### Linux reference base

Use:

- `docs/ai/skills/linux-mm-design-knowledge/`

Especially:

- `00-linux-mm-map.md`
- `01-address-space-mm-struct.md`
- `02-vma-design.md`
- `03-mmap-munmap-mprotect.md`
- `04-page-table-design.md`
- `05-page-fault-path.md`
- `06-anonymous-memory.md`
- `07-file-backed-mmap.md`
- `08-cow-design.md`
- `09-brk-stack-heap.md`
- `10-madvise-msync-mlock.md`

### X-Kernel design base

Use:

- `mm/docs/linux-aligned-final-architecture.md`
- `mm/docs/linux-memory-model-reference.md`
- `mm/memspace/docs/design.md`
- `mm/page_table/docs/design.md`
- `mm/pagecache/docs/design.md`
- `mm/anon/docs/design.md`
- `mm/filemap/docs/design.md`
- `mm/filemap/docs/security.md`
- `core/kuaccess/docs/design.md`
- `process/kprocess/docs/design.md`
- `process/kexec/docs/design.md`

### Shared repo guidance

Use:

- `docs/ai/skills/code-guidelines/SKILL.md`
- `docs/ai/skills/module-docs/SKILL.md`

Only for style and documentation conventions, not for implementation.

## Output Contract

Every workflow run must produce all of:

Design track:

- a topic brief;
- a Linux semantic baseline;
- an X-Kernel adaptation draft;
- cross-review findings from both experts;
- an arbiter decision table;
- a frozen design document;
- an implementation task split;
- a post-run workflow audit.

Implementation track:

- an implementation scope file;
- a coder implementation report;
- a code-review report;
- a validation report;
- an implementation audit.

Use the templates under `templates/`.

The frozen design document is the primary deliverable.
It must be detailed enough that a coder can start implementation from it
without re-deriving the architecture.

At minimum, the frozen design must include:

- crate decomposition;
- per-crate responsibility summary;
- per-crate owned data structures;
- core interface definitions at pseudocode/signature level;
- subsystem state machines and key flows;
- locking and lifetime rules;
- phase-1 versus deferred scope boundaries;
- implementation task mapping back to crates and interfaces.

## Completion Criteria

A run is complete only if:

- the topic scope is frozen;
- Linux-required semantics are explicitly listed;
- X-Kernel component boundaries are explicit;
- crate boundaries are explicit;
- each crate has a purpose statement;
- data structures and interfaces are described;
- core interfaces are specific enough for direct implementation planning;
- locking and lifetime rules are described;
- conflicts are recorded and resolved explicitly;
- deferred items are separated from phase-1 requirements;
- implementation tasks are split without writing code.
- the post-run audit confirms that critical findings and earlier open questions
  were resolved, deferred, or explicitly carried forward.

An implementation run is complete only if:

- its scope maps to specific tasks in `07-task-split.md`;
- the coder lists every changed file and design contract used;
- code review checks behavior, safety, ownership, and design conformance;
- validation runs the relevant build/lint/test commands or records why they
  could not run;
- implementation audit records unresolved review findings and follow-up tasks.
