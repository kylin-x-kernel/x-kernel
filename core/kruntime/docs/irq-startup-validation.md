# IRQ Startup Phase Validation

## Scope

The platform boot hooks separate controller preparation from local source
activation. Runtime installs current and checks mandatory timer/IPI handler
registration before activation. AP activation precedes pending-notification
drain; primary activation precedes softirqd task placement. Ordinary CPU IRQ
delivery remains masked through activation.

`ut-tests/irq_startup.rs` records the actual boot transitions on each CPU.
It checks preparation before activation, current and handler readiness at
activation entry, IRQ exclusion at entry/return, and completion on every
participating CPU. Observations persist for the test boot, with no runtime
provider replacement or timing threshold.

## QEMU Results

Each run starts with the platform's `qemu_defconfig` copied to `.config`, then
`make defconfig`. Runs disable networking and vsock, use a disposable disk for
coverage, and have an external 300-second build/run timeout.

| Platform | CPUs | UNITTEST_CRATE | Passed | Failed / ignored |
| --- | --- | --- | --- | --- |
| AArch64 | 4 | irq_driver,timer_driver,kruntime | 84 | 0 / 0 |
| x86_64 (TCG) | 4 | x86_apic,irq_driver,timer_driver,kruntime | 81 | 0 / 0 |
| RISC-V | 4 | irq_driver,timer_driver,kruntime | 74 | 0 / 0 |
| LoongArch | 1 | kplat_loongarch64,kruntime | 71 | 0 / 0 |

All four runs passed the new startup-order test and exported coverage. The
selected runtime tests also exercise scheduling, workqueue, RCU and IRQ-work
integration as applicable to each configuration.

Reproduce with the selected configuration and suite:

```sh
make unittest UNITTEST_CRATE=irq_driver,timer_driver,kruntime \
    XKMAKE_ARGS='--no-net --no-vsock'
```

Use `ACCEL=n` and the x86 suite above for the x86 TCG run; use the LoongArch
suite above for its default single-CPU configuration.

## Review And Delivery Boundary

The phase-split change replaces the old final-init hooks across all four
platforms and the dummy provider, without a compatibility path. Source review
confirmed that scheduler installation precedes foundation registration and that
local clockevent initialization precedes softirqd placement or pending IPI drain.
The existing startup preemption guard and separate AP rendezvous are retained.
Mandatory registration and hook errors stop bring-up rather than publishing
success or claiming rollback of already installed handlers.

The four configurations passed `make UNITTEST=y clippy`; x86 also passed normal
Clippy through the commit hook. Pinned formatter and diff whitespace checks
passed. These checks support delivery of the phase-split change, not completion
of the entire IRQ startup prerequisite series.

The remaining early-console transition belongs to Ticket06. Series-level
integration acceptance (Ticket07) must reconcile the limits below, especially
failure injection and source-delivery evidence, rather than count successful
boot alone as proof of those contracts.

## Limits

- LoongArch AP activation was not exercised by its single-CPU configuration.
- No mandatory-registration or controller-failure injection was performed.
  Error exits were reviewed for fail-stop behavior, not hardware rollback.
- Entry masking under adversarial firmware handoff was not fault-injected.
- Early console input registration/source control is a separate change;
  these results do not claim that it already obeys the new phase boundaries.
- These are functional regressions, not performance or long-duration stress
  measurements, and do not establish real-hardware coverage.
