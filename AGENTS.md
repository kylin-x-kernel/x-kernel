# X-Kernel Agent Notes

## Shared AI Skills

Tool-neutral shared skills live under `docs/ai/skills/`.
Any AI agent used in this repository should prefer those shared skills
over tool-specific copies when both exist.

Current shared skills:

- `docs/ai/skills/build-workflow/SKILL.md`:
  project configuration, build, run, clippy, and fmt workflow.

## Project Overview

X-Kernel is a multi-architecture Rust OS/kernel project.

Major areas:

- `arch/`: architecture HAL and CPU support.
- `boot/`: boot protocol handoff, boot stubs, and loaders.
- `core/`: core runtime, syscall, tracing, and services.
- `mm/`: memory management and address-space abstractions.
- `drivers/`: shared hardware drivers and device subsystems.
- `platforms/`: platform glue and defconfigs.
- `process/`, `task/`, `fs/`, `io/`, `net/`, and `tee/`: subsystem crates.
- `util/`: shared utility crates such as `klazy`.

## Current Refactor Context

Recent work removed the old catch-all `kcore` crate and tightened subsystem
ownership boundaries.

- Move reusable utility crates out of `core/` into `util/`.
- Keep address-space logic inside `mm/memspace`.
- Keep kernel user-memory access glue in `core/kuaccess`.
- Keep user program loading and exec image setup in `process/kexec`.
- Avoid reintroducing broad catch-all crates for responsibilities owned by
  `memspace`, `kuaccess`, or `kexec`.
- Prefer shared driver, HAL, and memory-management layers over platform-local
  forks.
- Keep refactoring commits separate from feature or behavior commits.

## Build And Validation

Always prepare `.config` from a platform defconfig before build, run, or
QEMU-based unit-test commands.

```bash
cp platforms/aarch64-qemu-virt/defconfig .config
make defconfig
```

Common commands:

```bash
make build
make clippy
make run
make UNITTEST=y run
make unittest
cargo +nightly-2026-03-08 fmt --all
```

Do not use bare `cargo check -p <crate>` as the main validation path.
The project relies on Kconfig-generated features and `.cargo/.xconfig.toml`,
so prefer the Makefile/Kconfig flow.

The current checked-in toolchain is described by `rust-toolchain.toml`.

## Coding Guidelines

Read `.claude/skills/asterinas-coding-guidelines/SKILL.md` before writing or
reviewing Rust kernel code.

Key local rules:

- Use accurate, descriptive names and encode units where the type does not.
- Keep boolean names assertion-like, such as `is_*`, `has_*`, or `can_*`.
- Comments should explain why, not restate what the code does.
- Every `unsafe` block needs a preceding `SAFETY:` justification.
- Validate at subsystem boundaries and trust validated internal invariants.
- Use `?` for ordinary error propagation.
- Avoid casual atomics and linear scans on hot paths.
- Use workspace dependencies and existing local helper APIs where available.
