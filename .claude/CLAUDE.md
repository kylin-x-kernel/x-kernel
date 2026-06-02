# x-kernel Project Guide

## Shared Skills

For tool-neutral project workflow instructions,
prefer the shared skill at:

- `docs/ai/skills/build-workflow/SKILL.md`
- `docs/ai/skills/code-guidelines/SKILL.md`

This shared skill is the canonical source for:

- configuration, build, run, test, format, debug, clean,
  rootfs helpers, platform mapping, and related host prerequisites.

When this file and the shared build workflow skill overlap,
follow the shared skill.

## Coding Guidelines

When writing or reviewing Rust kernel code,
read and follow `docs/ai/skills/code-guidelines/SKILL.md`.

## Change Completeness

When making a module change, do not stop at code edits alone.
Use this default workflow:

1. Implement the code change.
2. Review the patch against `docs/ai/skills/code-guidelines/SKILL.md`.
3. Check whether the change requires documentation updates:
   - crate-local `docs/design.md`
   - crate-local `docs/security.md`
   - rustdoc on touched public APIs
   - shared skills or top-level docs if workflow or policy changed
4. Update the required documentation before final validation.
5. Run the relevant build, lint, and test commands from
   `docs/ai/skills/build-workflow/SKILL.md`.
