# Phase 1: Topic Freeze

## Purpose

Turn a broad request into a bounded design topic.

## Required Output

Use `templates/topic-brief-template.md`.

The brief must define:

- topic name;
- exact scope;
- explicit non-goals;
- expected output documents;
- phase target;
- blocking open assumptions.

## Rules

- Refuse topics that try to design the whole MM subsystem at once.
- Split broad topics into one of the canonical slices:
  - MemorySpace
  - VMA plus mmap/munmap/mprotect
  - PageTable abstraction
  - PageFault core path
  - Anonymous memory
  - File-backed mmap
  - COW
  - User/kernel copy

## Exit Criteria

The topic brief is short, concrete, and excludes obvious out-of-scope items.
