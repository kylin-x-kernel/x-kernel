# Phase 6: Design Freeze

## Owner

Design Arbiter

## Purpose

Produce the final accepted design document after conflict resolution.

## Required Output

Use `templates/design-freeze-template.md`.

## Rules

- Freeze only after all prior phases are complete.
- The frozen design must separate:
  - accepted phase-1 design
  - deferred compatibility
  - explicit non-goals
- The frozen design must name concrete crates or modules that will own the work.
- The frozen design must describe key structures and interfaces in enough detail
  for direct implementation planning.
- The frozen design must resolve, defer, or explicitly carry forward open
  questions raised by the adaptation draft and cross-review findings.

## Exit Criteria

A single design package exists that downstream implementation work can follow
without re-litigating core semantics.
