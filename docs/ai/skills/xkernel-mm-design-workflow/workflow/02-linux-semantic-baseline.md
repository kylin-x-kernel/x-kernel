# Phase 2: Linux Semantic Baseline

## Owner

Linux MM Expert

## Purpose

Build the semantic and mechanism baseline that the X-Kernel design must react to.

## Required Output

Use `templates/linux-baseline-template.md`.

## Rules

- Do not propose X-Kernel crate structure.
- Do not write implementation tasks.
- Distinguish:
  - user-visible semantics
  - internal mechanisms
  - invariants
  - compatibility requirements
  - simplification candidates

## Exit Criteria

The baseline is source-indexed and clearly separates must-preserve semantics
from optional Linux complexity.
