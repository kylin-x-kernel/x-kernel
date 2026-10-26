# kiface-time-api Design

`ClockSourceIf` and `TimerTicks` describe one hardware tick domain. The timer
backend supplies counter reads and conversions; consumers can use
`monotonic_time_nanos()` as a convenience reader over that same source.
`ClockEventIf` programs local events; `ClockResumeIf` provides post-idle counter
repair before the caller releases its pinned post-wait context.

The task timer service in ktask supplies `LocalTimerReprogramIf` and
`ScopedOneShotTimerIf`, owning arbitration and timer storage. The scoped provider
lends a `OneShotTimer` to consumers such as the RCU progress service. Consumers
own deadlines, retry policy and expiry meaning; the borrowed handle cannot
escape. No timer registry or executor lives in this contract crate.

## Execution Context

`OneShotTimer::arm` requires initialized scheduler and timer IRQ services on the
current CPU. Expiry runs in timer IRQ context under the restrictions documented
in security.md and the unsafe method contract. `cancel_sync` and scope teardown
must leave callback-needed locks free and its CPU able to progress.
`LocalTimerReprogramIf::on_timer_reprogram` runs with local IRQs disabled after
per-CPU timer initialization, during boot drain or IPI handling; it must not
allocate, sleep, invoke callbacks or acquire a run-queue lock.

See [capability ownership rules](../../../docs/design.md) for the common
contract/provider boundary. This crate contains no implementation state.
