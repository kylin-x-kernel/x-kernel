# Boundaries And Context

Use this file when the change touches syscalls,
user pointers, file descriptors, filesystem context,
or any API whose meaning depends on the current execution context.

## API Semantics

- prefer passing validated context objects into deeper helpers
  rather than re-reading implicit global current-state repeatedly.

## Validation Boundaries

- validate at subsystem boundaries, then trust validated internal invariants;

## X-Kernel-Specific Context Rules

- for process-only filesystem paths,
  use process-specific context helpers;
- for helpers that may run from either a user thread
  or a kernel task, use shared execution-context helpers;
- for file descriptor operations,
  use the highest abstraction layer that satisfies the need
  instead of reaching directly into lower-level tables.

## When Reviewing

Check specifically for:

- low-level fd-table manipulation in code that should use higher-level wrappers.
