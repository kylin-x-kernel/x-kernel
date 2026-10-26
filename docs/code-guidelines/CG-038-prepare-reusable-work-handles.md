---
id: CG-038-prepare-reusable-work-handles
level: MUST
paths: ["**/*.rs"]
checks:
  - backend: ai
triggers:
  - '\b(?:WorkHandle|DelayedWorkHandle)\b'
---

# CG-038-prepare-reusable-work-handles: Prepare reusable work handles before IRQ submission

- **Rule**: Code MUST prepare allocation-bearing `WorkHandle` or
  `DelayedWorkHandle` in sleepable task context, retain it in its owner, and
  reuse it for allocation-free submissions from non-sleepable contexts.
- **Scope**: Owned work-handle construction, storage and submission from hardirq,
  softirq, timer or other non-sleepable paths.
  Not this rule: [CG-013](CG-013-no-sleep-under-spinlock.md) owns sleepable
  work under spinlock, IRQ-disable or preempt-disable guards. This rule owns
  preparation and reuse of handles before non-sleepable submission; producer
  shutdown, timer draining and queue teardown remain concurrency guidance in
  [the skill](../ai/skills/code-guidelines/concurrency.md).
- **Why**: IRQ-safe submission depends on allocating and preparing work before
  entering the non-sleepable producer path.
- **How to check**:
  1. Locate construction of each changed handle and identify whether that path is sleepable.
  2. Follow owner storage to the submit path. Confirm IRQ or other non-sleepable
     producers reuse the prepared handle rather than construct a new one per event.
  3. Distinguish task-only use from handles reachable by non-sleepable producers.
- **Exceptions**: Task-only handles need no special IRQ explanation. They still need
  a sleepable construction context when preparation can allocate.
- **Counter-example**:

```rust,ignore
fn irq_handler() -> Result<()> {
    WorkHandle::new(move || service_device())?.queue(&queue, None)?;
    Ok(())
}
```

The IRQ path constructs an allocation-bearing handle for each submission.

- **Example**:

```rust,ignore
// Sleepable device setup; the device retains this handle.
let service_work = WorkHandle::new(move || service_device())?;
// Later in the IRQ producer, submit the retained handle.
service_work.queue(&queue, None)?;
```
