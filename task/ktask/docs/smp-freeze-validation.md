# SMP task and worker freeze validation

Validated on 2026-11-10, x86_64 QEMU TCG, Rust 1.95.0, with the repository
`platforms/kplat-x86_64/qemu_defconfig` resolved through `make defconfig`.
`NR_CPUS=4`. Coverage instrumentation was disabled; this report asserts behavior,
not coverage percentages. Tests use private sources and boot-installed services.

## Reproduction

Prepare a private empty root filesystem so the kernel's normal filesystem
initialization completes before tests:

```sh
cp platforms/kplat-x86_64/qemu_defconfig .config
make defconfig
truncate -s 256M /tmp/xkernel-smp-test-root.img
mkfs.ext4 -q -F -b 4096 -O ^metadata_csum,^64bit /tmp/xkernel-smp-test-root.img
XKERNEL_ENABLE_COVERAGE=0 make unittest \
  UNITTEST_CRATE=ktask,kworkerpool_ng,kworkng ACCEL=n SMP=4 \
  DISK_IMG=/tmp/xkernel-smp-test-root.img XKMAKE_ARGS='--no-net --no-vsock'
```

Run the same validated image with one present CPU:

```sh
XKERNEL_ENABLE_COVERAGE=0 make justrun UNITTEST=y \
  UNITTEST_CRATE=ktask,kworkerpool_ng,kworkng ACCEL=n SMP=1 \
  DISK_IMG=/tmp/xkernel-smp-test-root.img XKMAKE_ARGS='--no-net --no-vsock'
```

The commands were bounded by host `timeout` (900 seconds for build/run,
300 seconds for image reuse). `--no-vsock` avoids requiring `/dev/vhost-vsock`.
This is an SMP-enabled kernel run on both one and four present CPUs, not a
separate non-SMP compilation.

## Results

| Present CPUs | Passed | Failed | Ignored | Total |
| --- | --- | --- | --- | --- |
| 4 | 294 | 0 | 0 | 294 |
| 1 | 284 | 0 | 10 | 294 |

Both runs returned success and printed `UNITTEST_STATUS: ALL_TESTS_PASSED`.
The one-CPU run skips SMP-only scheduler cases and the new distinct-CPU fixture;
the four-CPU run executes every selected test.

The four-CPU output confirms success for:

- `smp_cooperative_workers_hold_work_on_distinct_cpus_and_resume_each_round`:
  one real worker per present CPU; actual callback CPU IDs checked; three
  freeze/notify/cancel-after-success/thaw rounds; exact callback counts.
- `confirmation_follows_outgoing_observer_and_completed_stack_switch`:
  no acknowledgment on the outgoing stack; a returned guard observes Frozen
  with the target off CPU.
- `late_switch_completion_cannot_confirm_a_replacement_request`:
  deterministic staged request-identity regression.
- Existing idle-wait, running-callback, cancellation/rollback, system admission,
  default policy and manager-stop cases, now also executed on SMP.

## Limits

The fixtures validate real scheduler and worker behavior inside QEMU. Callback
sources are controlled test adapters. They do not establish a real userspace
NOHZ workload, physical CPU parking, device power transitions, firmware sleep,
RTC continuity or multicore platform resume. The kpm suspend entry remains
single-present-CPU until its separate CPU rendezvous and platform backend exist.
