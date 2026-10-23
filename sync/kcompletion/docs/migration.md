# Unified synchronous completion migration

## Scope and requirements

This change is independent of the driver-model redesign. Its base is main
`23f8673d1`; it does not introduce model/class/os-api or move the hwrng contract.

- Keep one completion state machine: counted completion, persistent broadcast,
  and explicitly synchronized reset, following Linux completion/swait semantics.
- After source construction, synchronous registration, parking, notification,
  and waiter removal must not allocate or fail due to registration capacity.
- Preserve early completion, spurious-wake rechecks, publication visibility,
  and stack-node validity through concurrent notification and removal.
- Validate sleepable task context; allow notification from task/IRQ context.
- Migrate existing completion users without changing ownership of their work,
  IRQ actions, process state, requests, DMA buffers, or cancellation protocols.
- Preserve dynamic readiness polling. Pidfd uses PollSet plus authoritative
  process exit state, not a second completion implementation.
- Do not add host-kernel dependencies to portable driver contracts.

Timeouts, interruptible completion waits, arbitrary Future/block_on changes,
and replacement of every kernel WaitQueue are outside this change.

## Implementation boundaries

`kcompletion` owns state and scoped intrusive stack nodes. `ktask::completion`
owns context validation and scheduler park/wake integration. The existing
Future parker and the synchronous adapter share the private park handshake.
See [design](design.md) and [security](security.md) for lifetime/lock proofs.

IRQ synchronization, task join, workqueue receipts, worker detachment, RCU,
block terminal waits and RNG condition waits use the new mechanism. RNG remains
a broadcast predicate wait, not a consumable completion token.

Default unfiltered scheduler tracing now captures into preallocated bounded
storage with inline names/events. Formatting and name-cache allocation happen
at dump time. External tracing callbacks and allocating upstream filter AST
clones are not covered by the no-allocation claim.

## Main-specific integration

Main already uses BioWait and BlockWaitProvider, not the older admission/terminal
pair in the driver-review branch. Preserve these APIs and lazy preparation.
An incomplete bio may allocate its boxed waiter and HostSignals Arc after
submission under main's infallible OOM policy. Only subsequent completion wait
registration/notification is allocation-free. Do not infer that the entire
bio lifecycle or all post-submission operations are allocation-free.

The hwrng contract stays in `drivers/contracts/hwrng` and keeps DriverResult.
Its fallible wait preparation must precede the first readiness predicate call;
later stack registration is infallible. No device request state is moved here.

Work/Queue observation snapshots still allocate independent receipts and
containers; RCU still prepares callback markers. These really are remaining
wait-resource allocations, not renamed business payload. Whole drain/shutdown
and synchronize operations are not certified allocation-free by this change.

## Acceptance

The original driver-review worktree passed 736 kernel tests, 13 production-core
host tests, 13 Miri tests, build and Clippy. Those results are historical and
are not evidence that the main integration passes unchanged.

Main integration validation on 2026-10-23:

- AArch64 qemu_defconfig, `make defconfig`, `make build`: passed.
- `make clippy`: passed, including dependency/header/layout/lint checks.
- `make check_test_layout check_host_tests check_lints`: passed.
- Kernel regression: 755 passed, zero failed/ignored; coverage persisted.
- Production-core host tests: 13 passed; Miri: 13 passed.

Kernel regression command:

```sh
make unittest UNITTEST_CRATE=kcompletion,ktask,kpoll,kirq,kworkng,kworkqueue_ng,kworkerpool_ng,krcu,kdriver,kprocess,ktracing,hwrng,virtio,block,device_res_xkernel XKMAKE_ARGS='--no-net --no-vsock'
```

Host/Miri commands use the independent manifest in `../tests/host` from outside
the kernel workspace; see its README. Local validation logs are
`/tmp/completion-main-{build,clippy,ut,host,miri,hygiene}.log`.
No performance or other-architecture claim is made. QEMU uses a separate disk
with networking and vsock disabled. Existing upstream dependency warnings remain.
