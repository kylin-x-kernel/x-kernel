---
id: CG-013-no-sleep-under-spinlock
level: MUST
paths: ["**/*.rs"]
checks:
  - backend: ai
triggers:
  - "SpinNoIrq"
  - "SpinNoPreempt"
  - "SpinRaw"
  - "irq_save"
  - "preempt_disable"
  - "lock_irqsave"
---

# CG-013-no-sleep-under-spinlock: Keep sleepable work outside spinlock scopes

- **Rule**: Code MUST NOT perform I/O or blocking work while holding a
  spinlock, or enter sleepable operations or acquire blocking synchronization
  primitives while a spinlock, IRQ-disable guard, or preempt-disable guard
  is held.
- **Scope**: All Rust paths with these guards live, including calls through
  helpers and callbacks; blocking primitives include `ksync::Mutex`, `RwLock`,
  `Semaphore`, and `WaitQueue`.
  Not this rule: [CG-001](CG-001-static-spinlock-registration.md),
  the pending Dylint lint `unregistered_static_lock` ([pending](pending.md)) and
  the pending Dylint lint `untracked_runtime_lock` ([pending](pending.md)) own lock registration and runtime
  constructor tracking; this rule owns work done while guards are live.
  Attribute an unnecessary IRQ mask to
  [CG-014](CG-014-spin-noirq-needs-irq-contender.md), and an atomicity gap
  between acquisitions to [CG-015](CG-015-no-split-critical-section.md).
- **Why**: `ktask` sleep entry points (`sleep`, `sleep_until`,
  `interruptible_sleep_until`, `wait_for_poll_event_until`) panic in
  interrupt-like contexts, with IRQs disabled, or with preemption suppressed: a context switch could migrate the task while the guarded
  section still assumes it is pinned.
- **How to check**:
  1. Identify each live spinlock, IRQ-disable, and preempt-disable guard and
     where it is dropped.
  2. Trace calls made before that drop for I/O, blocking lock acquisition,
     `block_on`, `wait`, `sleep`, event listeners, scheduler operations, or
     handling of `WouldBlock` by waiting.
  3. Require sleepable work to run after the guards are dropped; a callback
     behind a class or runtime driver spinlock must keep that scope around
     actual driver register or queue access and release it before using
     sleepable subsystem locks.
- **Exceptions**:
  1. None.
- **Counter-example**:

```rust,ignore
let mut state = device.state.lock(); // SpinNoIrq
state.pending = true;
event.wait();
```

The event wait can sleep while the spinlock guard is still live.

- **Example**:

```rust,ignore
{
    let mut state = device.state.lock(); // SpinNoIrq
    state.pending = true;
}
event.wait();
```
