# Task Wait API Design

This crate owns the task-bound completion-wait contract, not IRQ teardown, RCU
grace periods or a scheduler implementation. `ktask` implements the interface
directly using its existing CompletionWait. There is no runtime-owned adapter,
new wait algorithm, global state or allocation in this crate.

`TaskCompletionWaitIf::with_prepared_wait` requires sleepable task context:
no IRQ/BH execution, IRQ masking, preemption exclusion or active RCU read scope.
The ktask provider also rejects absent or idle current tasks before invoking
the body. `PreparedCompletionWait::wait_for_completion` and
`PreparedParking::park` require that same preparing task in sleepable context.

`TaskCompletionWaitIf::with_prepared_wait` validates current context before
calling a synchronous body exactly once. It retains the concrete wait and its
strong task reference on the provider's stack. The borrowed
`PreparedCompletionWait` does not escape and cannot move to another task.
The body may perform publication before waiting because no preparation error
can be returned after body invocation. Wait registration is allocation-free;
signals do not interrupt it. Completion tokens/broadcasts are acquired, not
merely hinted. Consumers still own and recheck their lifecycle predicates.

IRQ synchronization uses `wait_for_completion`, the prepare-and-wait convenience
function. RCU borrows the prepared handle before publishing retired ownership.
Both share one provider. RCU service parking remains separate: its reusable
waker and spurious-return contract is not a completion acquisition contract.

The separate crate represents a real dependency level in the task domain:

```text
kiface-task-wait-api -> kcompletion -> kspin -> kiface-task-api -> kiface
ktask -> kiface-task-api + kiface-task-wait-api (providers)
kirq / krcu -> kiface-task-api + kiface-task-wait-api (consumers)
```

Putting Completion into the low-level kiface-task-api would create a cycle. Neither
crate reexports or depends on the other as an umbrella. Generic Future waiting
is not substituted for this protocol, because its allocation and registration
contract differs. The task runtime owns blocking admission; IRQ error mapping
and RCU admission policy stay with their respective consumers.

TaskParkingIf lends PreparedParking directly from ktask's PreparedTaskWait.
Its waker may outlive the borrow; early wake hints survive until park. Parking
may return spuriously and never substitutes for completion acquisition.
