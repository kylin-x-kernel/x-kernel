# Deferred finalization host probes

The raw-owner probe also covers count-preserving transfer, intrusive field-offset
round trips with original allocation provenance, a simultaneous shared payload
borrow, 4096-byte alignment, zero-sized values, and final release in modeled NMI.

This standalone executable compiles the production finalizer and kfinalize.
It mocks only task-context detection, IRQ locks and queue admission. It is not
a substitute for kernel context/routing tests or a kernel panic recovery model.

It also compiles the real ksched Fair/RT/Deadline adapters with DeferredArc owners,
checking intrusive cloning, stealing, migration, nonempty teardown and current
exit. Generate the AArch64 config with the normal Makefile flow first, then export
`K_PLAT_NAME=kplat-aarch64` and `TARGET_DIR=<checkout>/target` for these host/Miri
runs so kbuild_config reads that generated snapshot. CARGO_TARGET_DIR still keeps
host artifacts separate. Enable `--features scheduler-probe` for this additional
case. These runs do not simulate Task stacks or CPU switching.

Keep strict-provenance Miri enabled for the default raw-owner/finalizer cases.
The optional real-tree probe cannot run in that mode: intrusive-collections
0.10.2 encodes parent/color in an integer and casts it back in rbtree.rs:525.
Run the optional probe with Miri's default provenance mode instead, retaining
its warnings and the default alias/leak checks. This is weaker provenance
coverage, not a strict-provenance pass for the trees; do not patch the dependency
or disable alias checks merely to obtain a passing report.
For real RT-list coverage under strict provenance, select
`--features scheduler-probe -- --rt-only`; this excludes the incompatible RB
trees but still runs all default finalizer/raw/NMI cases and the RT regression.

From the repository root, preserve the manifest path before changing directory
to avoid inheriting kernel Cargo configuration:

```bash
manifest="$PWD/task/kworkng/tests/deferred-finalization/Cargo.toml"
export K_PLAT_NAME=kplat-aarch64
export TARGET_DIR="$PWD/target"
cd /tmp
RUSTC_BOOTSTRAP=1 CARGO_TARGET_DIR=/tmp/deferred-fault-probe-target cargo +1.95.0 run --offline --locked --manifest-path "$manifest"
CARGO_TARGET_DIR=/tmp/deferred-accept-miri-final MIRIFLAGS=-Zmiri-strict-provenance cargo +nightly-2026-03-08 miri run --offline --locked --manifest-path "$manifest"
```

Both runs must exit zero and print eight PASS records. Three injected panics are
expected and observed through catch_unwind/join. Cases cover boot handoff
(including unchanged input before installation, concurrent producers while
binding is paused, and preparation visibility before admission returns), ordinary success,
rejection, accepted-notification panic, 193 nodes across four bounded batches,
and destructor panic with a retained tail.

Additional cases use a pre-reserved mock ordinary-IRQ queue and a global allocator
that rejects allocation/free while the thread-local NMI flag is set. They cover
no-Weak last strong, immediate Weak expiry with pending payload, late last Weak,
zero/small identity regions, alignment through 4096 and Layout overflow. This
fixture runs the real finalizer and identity allocator but mocks IRQ delivery;
the irq-work-host fixture and guest PMU tests independently cover that layer.

The rejection/destructor-panic cases resume the mock and submit a new object
only to inspect and clean up retained nodes. This is not a production retry or
progress guarantee after fatal failure. Host serialization and barriers cover
the selected interleavings, not all SMP/NMI behavior.
