# Phase 8: Post-run Audit

## Owner

Main coordinator or Design Arbiter

## Purpose

Check whether the workflow actually produced an implementer-ready design, not
just a sequence of documents.

## Required Output

Use `templates/post-run-audit-template.md`.

## Rules

- Verify that every file in the expected design-run package exists.
- Verify that both cross-review directions were produced.
- Verify that every critical review finding appears in the arbiter decision
  table.
- Verify that open questions from the adaptation draft were either resolved,
  moved to deferred items, or explicitly listed as follow-ups.
- Verify that the frozen design names crates, key structures, and core
  interfaces.
- Verify that task split items map to crates and named structures/interfaces.

## Exit Criteria

The workflow run can be handed to implementation planning without re-opening
the design process.
