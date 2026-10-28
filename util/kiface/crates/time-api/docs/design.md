# kiface-time-api Design

`CounterSourceIf` describes platform counter sampling, stable-frequency
conversion and explicit resume anchoring. The HAL implements `ClockSourceIf`
over that counter, supplying one logical monotonic domain that excludes formal
suspend intervals. `monotonic_time_nanos()` uses this logical provider, including
for consumers that do not depend on khal. Counter facts, logical clock mapping
and timer expiry ownership remain separate.

`ClockEventIf` programs local relative intervals. HAL converts logical deadlines
to remaining intervals before programming; backend hardware compares must never
interpret a logical absolute deadline as a raw counter value. `ClockResumeIf`
retains its separate post-idle anomaly repair contract.

The task timer service in ktask supplies `LocalTimerReprogramIf` and
`ScopedOneShotTimerIf`, owning arbitration and timer storage. The scoped provider
lends a `OneShotTimer` to consumers such as the RCU progress service. Consumers
own deadlines, retry policy and expiry meaning; the borrowed handle cannot
escape. No timer registry or executor lives in this contract crate.

## Execution Context

`CounterSourceIf::resume_ticks` runs only after counter access is restored,
with competing readers, event programming and backend repair handlers quiesced
by the PM owner. It must not dispatch repair IPIs, enable clock events, sleep,
or allocate. Counter frequency remains stable. Backend state may be reanchored
only at this explicit boundary; ordinary sampling retains its running contract.


`OneShotTimer::arm` requires initialized scheduler and timer IRQ services on the
current CPU. Expiry runs in timer IRQ context under the restrictions documented
in security.md and the unsafe method contract. `cancel_sync` and scope teardown
must leave callback-needed locks free and its CPU able to progress.
`LocalTimerReprogramIf::on_timer_reprogram` runs with local IRQs disabled after
per-CPU timer initialization, during boot drain or IPI handling; it must not
allocate, sleep, invoke callbacks or acquire a run-queue lock.

See [capability ownership rules](../../../docs/design.md) for the common
contract/provider boundary. This crate contains no implementation state.

## Local event restoration

ClockEventIf::restore_local_timer restores the initialized current CPU's event
mode after platform interrupt-controller access and routing have been restored.
Its postcondition is a stopped countdown; it does not calibrate or reset the
clock source, select deadlines or invoke callbacks. The PM owner calls it with
IRQs masked and competing producers/repair quiesced. The task timer service
separately re-arbitrates software deadlines only after timekeeping and sleep
accounting are ready. No PM registry or scheduling policy lives in time-api.
