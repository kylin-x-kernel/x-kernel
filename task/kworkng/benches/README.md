# Kernel Benchmarks And Soak

This opt-in runner uses real boot-installed NG normal/BH/high-BH providers,
workers, timers and IPIs. It is not a host simulation or an ordinary unit test.
Cargo bench auto-discovery is disabled: the kernel compiles this runner through
the `bench` feature, not through the host test harness.
Any future host benchmark must declare an explicit `[[bench]]` Cargo target.
Production configurations leave `KFEAT_WORKQUEUE_BENCH` disabled.

Prepare a platform configuration with `make defconfig`, enable
`KFEAT_WORKQUEUE_BENCH=y`, and set
`WORKQUEUE_SOAK_SECONDS=1800`. Run `make olddefconfig`, `make build`, then
`timeout --kill-after=5 2400 make justrun XKMAKE_ARGS='--no-vsock'`.
Use 5 seconds for initial smoke validation, or 0 for benchmark-only runs.
Do not enable `UNITTEST`: coverage instrumentation invalidates comparison
with release measurements. The runner starts after upper runtime initialization
instead of userspace and powers off after `NG_RUN_PASS`.
All `NG_*` samples, progress and completion records use unfiltered console
output, including with `quiet` or `loglevel=0`. The global log filter is not
changed. Record output is outside the per-sample measured interval.
To check this on AArch64 QEMU, use a 5-second soak and run
`timeout --kill-after=5 180 make justrun ACCEL=n XKMAKE_ARGS='--no-vsock' QEMU_ARGS='-append loglevel=0'`.
Require all 132 sample records on a four-CPU boot, four `NG_SOAK_WORKER`
records, and both final pass markers even though ordinary logs are disabled.

Round 0 warms each queue/CPU path. Rounds 1-5 precede the soak; rounds 6-10
follow it. Each sample reports 1,000 sequential prepared submissions and
individual flushes, including callback CPU checks and completion accounting.
Creation and final drain are outside the measured interval. Results measure
end-to-end round trips, not enqueue-only latency or maximum throughput.
The coordinator is not pinned, so target CPU labels do not classify samples as
strictly local or remote. Preserve every sample and report spread, not just best
numbers. QEMU results are not hardware latency guarantees.

The soak creates one pinned producer per present CPU, targeting the next CPU.
The `NG_SOAK_WORKER target_cpu` field identifies the callback destination,
not the producer CPU.
Each continuously reuses normal/BH/high-BH Work with alternating cancellation,
checks executed-plus-revoked accounting, and reuses a delayed handle through
positive arm, immediate retime, flush and synchronous cancel. Every sixteenth
cycle yields a timer-expiry window before flush. Every 64 cycles it creates an
owned Work and releases its final handle with IRQs masked, waiting for exactly
one capture destruction. Progress is logged every minute; workers drain before
the final pass marker. This is one sustained mixed workload, not exhaustive
race coverage or a proof of absence of memory leaks.

Require exit success, all pass markers, no panic, and advancing progress.
Retain host elapsed time, full logs, configuration, revision, QEMU version and
host load information. A timeout, assertion or interference must be reported,
not filtered out. A legacy performance comparison requires a separately valid
equivalent baseline; this runner only measures NG.

## Coverage Boundaries

The removed legacy procfs stress driver is not replaced one-for-one by this
workload. The following tests complement it:

| Contract | Coverage |
| --- | --- |
| Reuse, CPU placement, normal/BH/high-BH progress | This release runner and src/system_tests.rs |
| Delayed retiming, drain and final-owner release | This runner and src/live_owned_tests.rs |
| Racing publication versus permanent shutdown | src/live_owned_tests.rs |
| IRQ submission allocation and capture destruction context | core/kruntime/src/tests_workqueue/allocation.rs |
| Routing and admission races | crates/kworkqueue_ng/src/queue/tests/routing.rs |
| Worker occupancy, retirement and retained demand | crates/kworkerpool_ng tests |

Short smoke runs validate the runner, not long-term stability. Report the actual
duration separately from historical soak results. Removing legacy-specific tests
does not imply that their exact scheduling or capacity assumptions apply to NG.
