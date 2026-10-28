# kiface-time-api Security

## Scope and Unsafe Boundary

This analysis covers the entire crate, `src/lib.rs`. The contract contains no
unsafe implementation block, but `OneShotTimer::arm` is an unsafe entry point:
it authorizes delivery of the previously supplied callback in timer IRQ context.
The caller must ensure that the current CPU's scheduler and timer IRQ service
are initialized. The callback must not sleep, unwind, enable IRQs, reenter timer
dispatch or synchronously cancel this timer. A safe `with_timer` call alone does
not authorize arming; the unsafe arm call accepts those obligations.

The ktask timer provider retains storage throughout the scoped body and drains
running expiry before freeing it. Scoped timer borrows cannot escape.
Cancellation and scope teardown require callers to release callback-needed locks
and leave its CPU able to progress; self-cancellation is prohibited.
Clock conversion uses the same source domain. Idle repair retains the caller's
pinned post-wait context. Local reprogramming requires IRQ exclusion and
initialized per-CPU timer state; it must not allocate, sleep, invoke callbacks
or acquire a run-queue lock.

See [capability ownership rules](../../../docs/design.md) for the common
contract/provider boundary. This crate contains no implementation state.


## Counter and logical clock boundary

CounterSourceIf is supplied by a platform backend, ClockSourceIf by the HAL.
The backend's resume sampling must not enable events or dispatch repair IPIs.
The HAL freeze/rebase control path requires the PM owner to quiesce competing
CPUs, event programming and NMI clock readers; logical reads are not NMI-safe
across those transitions. Relative event intervals cross the hardware boundary,
so an adjusted logical epoch cannot leak into a raw compare register.
