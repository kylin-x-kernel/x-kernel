---
id: CG-015-no-split-critical-section
level: MUST
paths: ["**/*.rs"]
checks:
  - backend: ai
triggers:
  - "\\.lock\\(\\)"
  - "\\.read\\(\\)"
  - "\\.write\\(\\)"
---

# CG-015-no-split-critical-section: Preserve atomic critical sections

- **Rule**: Code MUST NOT split an operation that requires atomic access to
  protected state across separate lock acquisitions.
- **Scope**: Rust code whose checks, reads, or updates depend on an invariant
  remaining true while a lock is released and reacquired, including read-to-write
  lock transitions.
  Not this rule: [CG-001](CG-001-static-spinlock-registration.md),
  the pending Dylint lint `unregistered_static_lock` ([pending](pending.md)) and
  the pending Dylint lint `untracked_runtime_lock` ([pending](pending.md)) own registration and runtime
  constructor tracking; this rule owns an invariant broken between
  acquisitions. Sleeping under a live guard belongs to
  [CG-013](CG-013-no-sleep-under-spinlock.md), and unnecessary IRQ masking to
  [CG-014](CG-014-spin-noirq-needs-irq-contender.md).
- **Why**: Another accessor can invalidate a check between acquisitions,
  producing a time-of-check/time-of-use bug or inconsistent state.
- **How to check**:
  1. Identify the protected invariant and each acquisition and release around
     the operation.
  2. Check whether another accessor can change state in a gap so that a later
     read or update relies on stale information.
  3. Keep dependent checks and updates within one critical section; separate
     independent operations do not constitute a split atomic operation.
- **Exceptions**:
  1. None.
- **Counter-example**:

```rust,ignore
if account.lock().balance >= amount {
    account.lock().balance -= amount;
}
```

Another withdrawal can change the balance after the first guard is dropped.

- **Example**:

```rust,ignore
let mut account = account.lock();
if account.balance >= amount {
    account.balance -= amount;
}
```
