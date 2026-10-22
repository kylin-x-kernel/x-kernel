# Diagnostic slot ownership probe

This standalone host fixture compiles the production `task_registry/slot.rs`
directly. It checks paused readers during remote retirement, last-reader removal,
slot reuse, and concurrent publication/read/retirement without double destruction.
It does not model hardware NMI delivery or benchmark kernel latency.

Run from outside the kernel's Cargo configuration directory:

```sh
cd /tmp
cargo +1.95.0 test --manifest-path /path/to/x-kernel/task/ktask/tests/registry-slot/Cargo.toml
MIRIFLAGS=-Zmiri-strict-provenance cargo +nightly-2026-03-08 miri test --manifest-path /path/to/x-kernel/task/ktask/tests/registry-slot/Cargo.toml
```

Guest integration, registration-token idempotence, cross-CPU exit and actual
Weak/slot layout checks live in `ktask/ut-tests/task_registry.rs` and run through
the repository Makefile/Kconfig test workflow. Native/Miri tests supplement those
guest tests; neither is an exhaustive proof of all memory-model interleavings.
