# Line Management Validation

Ticket01 validates stable Line identity and request/free serialization. It keeps
the existing directory, action snapshots and short global control lock.

## Contract Coverage

- Core fixtures cover original-Line retention through number reuse, stale
  acquisition retry/rejection, metadata revalidation, startup try-acquisition,
  management admission versus unused/MSI removal, NMI fallback ownership and
  final handler destruction outside locks.
- Scheduler mutex-provider tests exercise the shared sleeping mutex, independent
  instances, task contention, release and unknown-token owner identity. Runtime
  lifecycle tests retain IRQ-specific context rejection and drain coverage.
- `request_waits_for_free_to_drain_retained_inflight` registers disabled synthetic
  Lines through the production API. A retained in-flight fixture holds the old
  Line while the remover blocks on drain and a requester blocks on management.
  An unrelated Line still completes registration/removal. Explicitly releasing
  the fixture lets both tasks finish; old and replacement handlers are destroyed
  exactly once. No hardware claim is fabricated and no IRQ handler sleeps.
- `guard_keeps_exclusion_across_task_migration` changes its worker's
  singleton affinity while the mutex guard remains on that task's stack. It
  verifies actual CPU0/CPU1 execution, exclusion at both endpoints, and later
  reacquisition. It does not independently observe every instant during migration.

The combined test observes task `Blocked` state after start handshakes, rather
than interpreting a pre-call flag as proof of waiting. Workers have no unrelated
blocking operation between those handshakes and their IRQ APIs. These waits use
no wall-clock success threshold; an external runner timeout bounds a stall.

## Recorded Runs

### Shared Mutex Extraction

Validation on 2026-10-25 after replacing the runtime IRQ-specific lock adapter
with `kmutex`, backed by the existing `lock_api` and a `ktask` task provider:

| Configuration | Result |
| --- | --- |
| AArch64 qemu_defconfig, SMP4, UNITTEST Clippy | Passed |
| AArch64 SMP4, `kirq,kruntime,ksync,ktask` QEMU | 358 passed, 0 failed, 0 ignored |
| AArch64 SMP4, LOCK_STAT, UNITTEST Clippy | Passed |
| AArch64 SMP4, LOCK_STAT, `kirq,kruntime,ksync,ktask` QEMU | 358 passed, 0 failed, 0 ignored |
| x86_64 qemu_defconfig, UNITTEST Clippy | Passed |
| kmutex host doctests | 2 passed, including non-Send guard compile-fail check |

Both QEMU runs include the six mutex-provider tests and export coverage.
They use `--no-net --no-vsock`. Logs: `/tmp/kirq01-kmutex-clippy.log`,
`/tmp/kirq01-kmutex-qemu.log`, `/tmp/kirq01-kmutex-lockstat-clippy.log`,
`/tmp/kirq01-kmutex-lockstat-qemu.log`, and `/tmp/kirq01-kmutex-doctest.log`.
The x86_64 compile/lint log is `/tmp/kirq01-kmutex-x86-clippy.log`; no x86_64
QEMU run is claimed for the extraction.

### Earlier IRQ-Specific Adapter

Local validation on 2026-10-25, based on production commit `c6356a1a8` with the
acceptance-test changes in this series:

| Configuration | Result |
| --- | --- |
| AArch64 qemu_defconfig, SMP4, UNITTEST Clippy | Passed |
| AArch64 SMP4, `kirq,kruntime` QEMU | 204 passed, 0 failed, 0 ignored |
| Same SMP image, QEMU `--smp 1` | 197 passed, 0 failed, 7 ignored |

Both QEMU runs exported coverage successfully and used `--no-net --no-vsock`.
The single-runtime-CPU run is not a pure-UP build. Its lifecycle test passes;
the migration test explicitly skips because two CPUs are required.

Earlier production validation at `c6356a1a8` passed AArch64 default and LOCK_STAT
QEMU suites (202/202 each), plus AArch64 and x86_64 UNITTEST Clippy. The new
acceptance tests are not claimed to have run under x86_64 or LOCK_STAT.

One initial 203-test run exposed a test-observation flaw: the destructor probe
used `IRQ_STATE.try_lock()`, which could fail under unrelated remote timer
activity. The corrected probe checks local IRQ exclusion first, then performs
actual registry reentry; the private Line mutex still uses try-acquisition.
This avoids treating global contention as evidence of a leaked local guard.

Local logs: `/tmp/kirq01-acceptance-final-clippy.log`,
`/tmp/kirq01-acceptance-qemu.log`,
`/tmp/kirq01-acceptance-singlecpu-qemu.log`. The initial failed run is retained
as `/tmp/kirq01-migration-qemu.log`.

## Remaining Boundaries

Static review rechecked Linux `kernel/irq/manage.c`: `request_mutex` surrounds
request/free, short descriptor/chip locks are released before synchronization,
and management exclusion ends before final action release. No blocking finding
remained in the Ticket01 production review.

This does not validate controller-completion drain, MSI hardware shutdown/reuse,
full NMI retirement, lock-free action traversal or every possible SMP schedule.
Those remain separate tickets. These tests do not replace real device delivery
tests or constitute a performance result.
