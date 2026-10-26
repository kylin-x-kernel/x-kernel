---
id: CG-037-spin-raw-context-precondition
level: MUST
paths: ["**/*.rs"]
checks:
  - backend: ai
triggers:
  - '\bSpinRaw\b'
---

# CG-037-spin-raw-context-precondition: Establish the SpinRaw context precondition

- **Rule**: `SpinRaw` users MUST establish that IRQs and preemption are already
  disabled for the whole critical section and explicitly document that precondition.
- **Scope**: Acquisition and use of `SpinRaw`, including the immediate caller's
  context token or guard. An enforced enclosing contract may supply the proof.
  Not this rule: [CG-014](CG-014-spin-noirq-needs-irq-contender.md) owns
  whether `SpinNoIrq` has an IRQ contender; [CG-013](CG-013-no-sleep-under-spinlock.md)
  owns sleepable work under live guards. This rule owns the prior exclusion
  required by `SpinRaw`, which supplies neither exclusion itself.
- **Why**: A raw spinlock cannot prevent local IRQ re-entry or task preemption
  from interrupting a section that depends on current-CPU exclusion.
- **How to check**:
  1. Identify the changed acquisition and the full lifetime of the protected section.
  2. Read the immediate caller and its token, guards or enforced context contract.
     Confirm that both IRQ and preemption exclusion precede acquisition and
     remain established until the section ends.
  3. Check that the API or enclosing contract names this precondition; an
     undocumented assumption that callers happen to disable IRQs is insufficient.
- **Exceptions**: Enforced boot or NMI confinement may establish the precondition
  without redundant guards; document why that context excludes both interference
  paths for this section.
- **Counter-example**:

```rust,ignore
fn update() {
    let guard = RAW.lock(); // No caller contract or exclusion guards.
    update_local_state(&guard);
}
```

The raw acquisition has no established IRQ and preemption precondition.

- **Example**:

```rust,ignore
// Contract: only called from the switch path while its IRQ-disable and
// preempt-disable guards remain live through this entire function.
fn update_in_switch() {
    let guard = RAW.lock();
    update_local_state(&guard);
}
```
