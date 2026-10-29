# Shared AI Skills

This directory contains tool-neutral skills
for AI agents working in the X-Kernel repository.

Goals:

- Keep reusable project guidance in one place.
- Avoid duplicating the same instructions across `.claude/`, `.codex/`,
  `.agents/`, or future tool-specific directories.
- Make it easy for both humans and AI tools
  to discover the canonical workflow documents.

Conventions:

- Each skill lives in its own directory.
- Each skill entry file is named `SKILL.md`.
- Skills should describe when they apply,
  the assumptions they rely on,
  and the exact commands or checks to run.
- Tool-specific directories may add thin adapters or pointers,
  but the canonical content should stay here.

Current skills:

- `build-workflow/`:
  baseline project configuration, build, run, clippy, and formatting flow.
- `module-docs/`:
  module documentation generation flow for per-crate `design.md`,
  `security.md`, and required rustdoc coverage.
- `code-guidelines/`:
  executable coding and code-review conventions for Rust kernel code,
  backed by the rule registry in `docs/code-guidelines/`.
- `test-harness/`:
  external Starry Test Harness workflow for guest regression cases,
  suite registration, execution, result inspection, and syscall-focused
  test design constraints.
- `problem-diagnosis/`:
  first-pass issue classification and localization workflow for build,
  boot, panic, hang, regression, and performance problems.
- `performance-analysis/`:
  performance investigation workflow (workloads, baselines, reporting);
  `lock_stat` is under `references/lock-stat.md`; scheduler (schbench /
  EEVDF / IRQ / affinity) is under `references/scheduler/`.
- `linux-mm-design-knowledge/`:
  Linux MM semantic knowledge base used by X-Kernel memory design work,
  covering address space (mm struct), VMA, mmap, page faults, anonymous
  memory, file-backed mmap, COW, brk, madvise, msync, and mlock.
- `xkernel-mm-design-workflow/`:
  multi-agent design-first workflow for X-Kernel memory subsystem topics,
  coordinating Linux semantic analysis, X-Kernel adaptation, arbiter review,
  and implementation task splitting without code generation.
- `pr-ci-review/`:
  Gitee PR lifecycle, Jenkins CI status, and AI review via xreview (status,
  requesting a review, responding to findings).
