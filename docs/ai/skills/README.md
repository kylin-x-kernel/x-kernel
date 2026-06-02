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
