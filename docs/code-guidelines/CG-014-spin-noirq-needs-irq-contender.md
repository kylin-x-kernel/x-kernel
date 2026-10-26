---
id: CG-014-spin-noirq-needs-irq-contender
level: SHOULD
paths: ["**/*.rs"]
checks:
  - backend: ai
triggers:
  - "SpinNoIrq"
---

# CG-014-spin-noirq-needs-irq-contender: Justify IRQ masking by access context

- **Rule**: Code SHOULD use `SpinNoIrq` only when local IRQ or similarly
  IRQ-like contexts access the same state, or when the caller must be valid
  in interrupt context.
- **Scope**: Selection of `SpinNoIrq` for shared state in all Rust code.
  Not this rule: [CG-001](CG-001-static-spinlock-registration.md),
  the pending Dylint lint `unregistered_static_lock` ([pending](pending.md)) and
  the pending Dylint lint `untracked_runtime_lock` ([pending](pending.md)) own registration and runtime
  constructor tracking; this rule owns whether IRQ masking is justified.
  Sleeping while a guard is live belongs to
  [CG-013](CG-013-no-sleep-under-spinlock.md), and lost atomicity across
  acquisitions to [CG-015](CG-015-no-split-critical-section.md).
- **Why**: Choosing `SpinNoIrq` by default masks interrupts without a
  context requirement; task-only state can use a sleepable lock or
  `SpinNoPreempt`.
- **How to check**:
  1. Identify the protected state and inspect its access paths and caller
     context contracts.
  2. Confirm a local IRQ or IRQ-like accessor, or a requirement that the
     caller run in interrupt context; a generic claim of safety is insufficient.
  3. Without that requirement, prefer a sleepable lock for potentially
     blocking work, or `SpinNoPreempt` for a short non-blocking task-context
     section requiring preemption exclusion.
- **Exceptions**:
  1. None.
- **Counter-example**:

```rust,ignore
struct TaskCache {
    // Accessed only by tasks; updates are short and never block.
    entries: SpinNoIrq<Entries>,
}
```

Task-only access provides no reason to mask local IRQs for this cache.

- **Example**:

```rust,ignore
struct Device {
    // Updated by both submit() in task context and the local IRQ handler.
    completions: SpinNoIrq<Completions>,
}
```
