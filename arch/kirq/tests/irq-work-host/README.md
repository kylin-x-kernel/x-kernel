# IRQ-work standalone HOST / Miri fixture

This dependency-free binary compiles the real `arch/kirq/src/irq_work.rs`
using `#[path = "../../../src/irq_work.rs"]`. Its independent workspace does
not load kernel UT modules: `unittest::path_mod_test!` expands to nothing.

Four scoped threads each own one fixed CPU for their entire lifetime. The
`cpu_slot!` mock creates four statically initialized `LocalState` values;
there is no permanently leaked heap-backed CPU state. Thread-local context
flags model explicit ordinary IRQ entry and nested NMI entry, restoring the
previous flags on exit. `NoPreemptIrqSave` is a no-op under this exclusive
thread ownership assumption. The platform provider returns a function that
only increments the current CPU's notification counter, never dispatching.

Each CPU checks:

- NotReady before initialization, including retry of the same unclaimed node;
- initialization rejection in IRQ/NMI and idempotent successful initialization;
- duplicate rejection before and after execution of a retained one-shot node;
- no dispatch from task or NMI, including NMI nested inside ordinary hardirq;
- 193 self-freeing Box nodes drained in batches of 64, 64, 64, 1, with exact
  callback, destructor and continuation-notification counts;
- callback-time NMI publication of a child allocated before entering IRQ,
  rejecting both NMI inline dispatch and recursive ordinary IRQ dispatch;
- parallel CPU isolation via owner assertions, local counts and barriers.

All transferred Boxes are reclaimed. The retained duplicate-probe allocation
is reclaimed only after dispatch and the final duplicate probe. Self-freeing
nodes use a `repr(C)` first-field cast with no pointer/integer round trips.
Miri's default leak and alias checks remain enabled.

The startup layout line measures the real `IrqWork` and private `LocalState`
types using `size_of` and `align_of`. The mock slot macro exposes the latter
measurement without editing production code. Sizes describe the current host
target and compiler; they are not a cross-architecture ABI guarantee.

Run from `/tmp` to avoid the kernel's Cargo configuration. Keep build artifacts
outside the worktree. Substitute the checkout path if needed:

```bash
cd /tmp
fixture=/root/codes/x-kernel-worktrees/deferred-reclamation/arch/kirq/tests/irq-work-host/Cargo.toml
CARGO_TARGET_DIR=/tmp/irq-work-host-target cargo +1.95.0 run --locked --manifest-path "$fixture"
CARGO_TARGET_DIR=/tmp/irq-work-host-miri MIRIFLAGS="-Zmiri-strict-provenance" cargo +nightly-2026-03-08 miri run --locked --manifest-path "$fixture"
```

This is a software notification/context simulation, not a hardware test. It
does not validate real IRQ masking, interrupt-controller delivery, priority,
latency, CPU migration/offline, or asynchronous NMI interruption at arbitrary
instructions. The nested NMI case exercises callback-time publication only.
No kernel configuration, kernel build or QEMU run is involved.
