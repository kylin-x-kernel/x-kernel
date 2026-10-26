---
id: CG-039-execution-context-contract
level: MUST
paths: ["**/*.rs"]
checks:
  - backend: ai
triggers:
  - '\bcurrent_(?:process|execution|fs|files)\b'
  - '\bfn\s+(?:current|with_current)_[A-Za-z0-9_]+\s*[(<]'
  - '\bfn\s+(?:irq|interrupt|softirq)(?:_[A-Za-z0-9_]+)?\s*[(<]|\bfn\s+[A-Za-z0-9_]+_(?:irq|interrupt|softirq)(?:_handler)?\s*[(<]'
---

# CG-039-execution-context-contract: State the execution-context contract

- **Rule**: APIs that depend on a current resource or execution context MUST
  state whether they require a current process thread or only a shared execution
  path, whether IRQ execution and sleeping are allowed, and which relevant
  caller-held lock families are permitted. Names must not hide those assumptions.
- **Scope**: Context-dependent APIs and current-context helpers, including IRQ
  and softirq entry contracts. An adequate enclosing trait or module contract
  suffices. Initial triggers cover the measured current-resource and handler
  names, not every possible context-dependent API.
  Not this rule: [CG-013](CG-013-no-sleep-under-spinlock.md) owns actual
  sleepable work under exclusion guards and [CG-014](CG-014-spin-noirq-needs-irq-contender.md)
  owns IRQ-contender lock selection. This rule owns the stated contract;
  concrete bad process/IRQ context or interleavings remain defect review.
- **Why**: A current process is not interchangeable with a kernel task's
  current execution path; callers need the distinction before selecting helpers.
- **How to check**:
  1. Read the enclosing API, trait or module contract and the changed current-context helper.
  2. Check that it distinguishes process-thread from shared execution-path needs
     and states IRQ allowance, sleepability and relevant caller-held locks.
  3. Read immediate callers to resolve the intended contract and naming. Report
     an omitted or concealed requirement without expanding review to the entire scheduler.
- **Exceptions**: No separate per-function repetition is needed when an applicable
  enclosing contract already states all relevant requirements.
- **Counter-example**:

```rust,ignore
/// Return the files for the current context.
fn current_files() -> Files {
    current_process().files()
}
```

The contract hides its process-thread requirement and allowable call context.

- **Example**:

```rust,ignore
/// Requires a current process thread; kernel tasks and IRQ handlers may
/// not call this helper. May sleep; callers must hold no spinlock or
/// IRQ-disable/preempt-disable guard.
fn current_process_files() -> Files {
    current_process().files()
}
```
