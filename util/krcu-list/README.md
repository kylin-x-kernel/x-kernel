# RCU List

Ordered, forward RCU traversal with owned entries. See [design](docs/design.md),
[safety](docs/security.md), and crate rustdoc for API examples and contracts.

## Tests

Prepare the normal platform configuration and build once to generate kbuild
constants. Host tests then use that configuration without kernel unittest flags:

```sh
K_PLAT_NAME=kplat-aarch64 RUSTFLAGS= cargo test -p krcu-list --target x86_64-unknown-linux-gnu
K_PLAT_NAME=kplat-aarch64 RUSTFLAGS= cargo +nightly-2026-03-08 miri test -p krcu-list --target x86_64-unknown-linux-gnu --lib
```

Host tests retain detached storage explicitly; they test real pointer/ownership
operations, not the kernel GP engine. Doctests check non-escape, single-use Entry
ownership and list-borrow lifetimes. Miri checks the host fixtures' raw-pointer
provenance, aliasing and concurrent access.

Basic tests belong to this crate. Scheduler/IRQ integration tests belong to
`crates/kernel-tests`, so the reusable container does not depend on kirq/ktask.
This split prevents a dependency cycle when the intended IRQ consumer adopts
the list: `kirq -> krcu-list -> kirq` (also via `ktask -> kirq`). It is a
dependency-cycle exception, not the default layout for kernel tests. Tests
without that reverse dependency stay in the owning crate's `ut-tests/`, host
tests or doctests; needing a running kernel alone is not a reason to split.
The existing `entry/unittest` feature enables `kfeat/kernel-tests`; ordinary
kernel builds do not select these test dependencies, including through `smp`.
Use `make UNITTEST=y clippy` to lint the enabled kernel test modules.
With an isolated writable test disk:

```sh
make unittest UNITTEST_CRATE=krcu_list,krcu_list_kernel_tests DISK_IMG=/path/to/test.img XKMAKE_ARGS='--no-net --no-vsock'
```

Normal UT includes bounded functional scenarios. The larger workload is skipped
unless explicitly enabled at build time:

```sh
KRCU_LIST_STRESS_ROUNDS=10000 make unittest UNITTEST_CRATE=krcu_list_kernel_tests DISK_IMG=/path/to/test.img XKMAKE_ARGS='--no-net --no-vsock'
```

Four workers each execute the requested number of append/remove pairs while a
reader checks initialized contents. Exact release counts are checked after drain.
The report separates the workload interval from final callback drain latency;
it is not isolated per-operation latency or proof of IRQ performance improvement.
The variable accepts 1..100000. An external runner timeout should bound hangs.
Cargo tracks `option_env!` inputs and rebuilds this crate when the variable is
set, changed or removed; no clean build or build script is needed. Run the
build-and-run command above, not `justrun` with an already-built image. Check
the `RCU_LIST_STRESS updates=...` output to confirm the requested workload ran;
an unset variable reports the workload as ignored.

The delayed-reader case pins reader/coordinator to different CPUs, removes 128
entries, verifies none were destroyed, then releases the reader and verifies full
drain. It uses event acknowledgement, not a wall-clock success threshold.
Timer reads use real hardirq delivery. The NMI test uses a controlled software
entry under writer exclusion; it does not claim real hardware NMI delivery.
