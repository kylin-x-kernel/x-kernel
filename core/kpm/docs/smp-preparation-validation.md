# Multicore preparation integration validation

Validated on 2026-11-10 with the replacement SMP task/worker freezer and the
`freeze_system_workqueues` API. Base `ab6b72a2` includes the updated EEVDF eligible
lookup and VFS invalidation changes. This report covers software preparation,
not firmware or physical suspend.

## Scenario

The production task registry and activation gate select ordinary targets;
a test-only complete snapshot supplies explicit exemptions for the boot/test
infrastructure. One real participant is pinned to each present CPU. Each private
queue has one allowed CPU, and its real callback asserts that execution CPU.

For three rounds the serial QEMU fixture:

1. Freezes admitted participant tasks, then the selected queues.
2. Marks a fixture-owned simulated device unavailable.
3. Submits callbacks and business wake hints; checks that tasks stay Frozen,
   callbacks stay pending, and no business visit completes.
4. Cancels both preparation handles after success and verifies retained ownership.
5. Attempts nested public preparation with cancelled handles and verifies Busy
   without releasing the existing task/queue guards, then attempts new task
   activation; the activation gate retains it.
6. Restores the simulated resource, releases queues then tasks, and checks exact
   callback/visit counts and completion of the deferred task.

Worker waits are hints: the fixture rechecks a pending-business flag rather than
counting every `park_freezable` return as work. It additionally supplies the same
queue twice, checks Busy at index 1, verifies prefix/task rollback, and retries a
successful transaction. Cleanup joins participants, shuts down work handles and
closes every private queue.

## Reproduction

Use the root-image setup from
[the freezer validation report](../../../task/ktask/docs/smp-freeze-validation.md).

```sh
cp platforms/kplat-x86_64/qemu_defconfig .config
make defconfig
truncate -s 256M /tmp/xkernel-smp-test-root.img
mkfs.ext4 -q -F -b 4096 -O ^metadata_csum,^64bit /tmp/xkernel-smp-test-root.img
XKERNEL_ENABLE_COVERAGE=0 make unittest \
  UNITTEST_CRATE=kpm,ktask,kworkerpool_ng,kworkng ACCEL=n SMP=4 \
  DISK_IMG=/tmp/xkernel-smp-test-root.img XKMAKE_ARGS='--no-net --no-vsock'
XKERNEL_ENABLE_COVERAGE=0 make justrun UNITTEST=y \
  UNITTEST_CRATE=kpm,ktask,kworkerpool_ng,kworkng ACCEL=n SMP=1 \
  DISK_IMG=/tmp/xkernel-smp-test-root.img XKMAKE_ARGS='--no-net --no-vsock'
```

The first command was bounded by `timeout 900`; image reuse by `timeout 300`.
Rust 1.95.0, x86_64 QEMU TCG, configured `NR_CPUS=4`. Both runs use the same
SMP-enabled image. Coverage instrumentation is disabled.

## Results

| Present CPUs | Passed | Failed | Ignored | Total |
| --- | --- | --- | --- | --- |
| 4 | 324 | 0 | 0 | 324 |
| 1 | 314 | 0 | 10 | 324 |

Both runs returned success and printed `UNITTEST_STATUS: ALL_TESTS_PASSED`.
The new `smp_preparation_holds_tasks_and_callbacks_until_simulated_device_recovery`
case executed and passed in both topologies. Four-CPU output shows CPU 0–3
initialization and the distinct-CPU worker test also passes without skipping.
The one-CPU skips belong to SMP-only scheduler/worker tests.

## Unverified boundaries

The device flag is a controlled resource-availability model; no actual device
power callback, CPU power-off, IRQ-controller suspend, RTC injection or firmware
entry runs. User-mode NOHZ delivery is not established by these kernel fixtures.
The existing platform-suspend API still rejects multiple present CPUs. This
preparation guard must not be used as a CPU/IRQ/DMA or hardware-suspend barrier.
