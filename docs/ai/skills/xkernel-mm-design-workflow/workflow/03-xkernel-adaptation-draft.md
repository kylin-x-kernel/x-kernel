# Phase 3: X-Kernel Adaptation Draft

## Owner

X-Kernel Memory Designer

## Purpose

Translate the Linux baseline into an X-Kernel design that fits current project
boundaries and phase-1 goals.

## Required Output

Use `templates/xkernel-adaptation-template.md`.

## Rules

- Every major decision must carry a tag:
  - `Linux-required`
  - `xkernel-adaptation`
  - `explicit-simplification`
  - `deferred-compatibility`
- Do not implement code.
- Do not hide unresolved design in implementation notes.

## Exit Criteria

The draft covers architecture, components, data structures, interfaces,
locking/lifetime, and phased simplifications.
