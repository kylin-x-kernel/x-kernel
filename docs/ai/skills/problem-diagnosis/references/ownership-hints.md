# Ownership Hints

Use this reference when the symptom is known
but the likely owner is not obvious.

## Symptom To Boundary Hints

- config expansion or feature mismatches:
  build system, Kconfig flow, or crate feature wiring
- address-space, mapping, or page-fault symptoms:
  `mm/` and related memory-ownership boundaries
- user-memory copy or access validation symptoms:
  `core/kuaccess`
- exec image, binary load, or process-start symptoms:
  `process/kexec` and process startup flow
- scheduler, wait, wake, or task-lifecycle symptoms:
  `task/` and `process/`
- filesystem ABI or path behavior symptoms:
  `fs/`
- socket, packet, or network syscall symptoms:
  `net/`
- device bring-up or platform-only failures:
  `drivers/`, `arch/`, or `platforms/`

## Use Carefully

These are first-pass hints, not proof.
Use them to narrow the search,
then verify ownership against the first real failure signal.
