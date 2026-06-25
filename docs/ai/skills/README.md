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
  module documentation generation flow for per-crate design,
  security, and rustdoc content.
- `code-guidelines/`:
  executable coding and code-review conventions for Rust kernel code,
  backed by `docs/coding-guidelines/`.
- `unsafe-audit-workflow/`:
  standards-first workflow for unsafe auditing, independent confirmation,
  remediation ordering, and reporting standard gaps during unsafe cleanup.
- `test-harness/`:
  shared Starry Test Harness workflow for regression cases,
  suite registration, execution, and result inspection.
- `problem-diagnosis/`:
  first-pass issue classification and localization workflow for build,
  boot, panic, hang, regression, and performance problems.
- `linux-mm-design-knowledge/`:
  Linux MM semantic knowledge base used by X-Kernel memory design work,
  including mm struct, VMA, mmap, page faults, anonymous memory, file-backed
  mmap, COW, brk, madvise, msync, and mlock.
- `xkernel-mm-design-workflow/`:
  multi-agent design-first workflow for X-Kernel memory subsystem topics,
  coordinating Linux semantic analysis, X-Kernel adaptation, arbiter review,
  and implementation task splitting without code generation.
