# kiface-task-api design

`kiface-task-api` owns low-level shared task capability contracts, not scheduling algorithms or
consumer lock policy. Its dependencies are `kiface`, which supplies static
exactly-one-provider wiring, and `ktask-types`, which supplies the shared
`OwnerKey` boundary type, plus `kcpu_id_map` for logical CPU identities.
Consumers do not depend on `ktask`; the runtime
implements the contracts without exposing task handles or scheduler types.

`TaskBlockingContextIf` supplies a nonallocating admission snapshot. It neither
prepares a wait nor certifies caller-held mutexes or dependency cycles. Higher
level completion waiting lives in sibling kiface-task-wait-api: kcompletion depends on
kspin, which consumes this lower-level crate. RCU-specific admission remains RCU
policy, not part of the scheduler's general blocking-context query.

`TaskExecutionIf` supplies current-task presence, a nonzero stable owner key, voluntary yielding and
synchronous execution of a borrowed pinned future. The owner key is the shared
`ktask_types::OwnerKey` value type, whose low `FLAG_BITS` bits stay clear so
lock consumers can OR lock-word flags into them without aliasing another task's
identity. The ktask provider derives the key with `OwnerKey::from_token`; a
consumer decodes an owner word back to `OwnerKey` with `OwnerKey::from_aligned`
after masking the flags off. Identity queries do not
schedule. Yield and wait require sleepable current-task context. Waiting does
not transfer ownership of the future or require it to be Send; the provider
must complete it on the calling task before returning.

KIRQ uses `has_current_task` before startup try-lock acquisition. The ktask
provider reports presence including idle, without scheduling; KIRQ separately
rejects interrupt/BH/NMI contexts and owns startup authorization. Presence does
not imply sleepability or grant permission to block.

`TaskLockDiagnosticsIf` supplies acquired, waiting and released hooks for the
current task. The provider owns time and diagnostic storage, not exclusion or
wakeup ordering. All hooks are always declared. Consumers may feature-gate
invocation; providers without diagnostics use no-op implementations.

`TaskPreemptionIf` owns nested current-task preemption credits. `kspin` consumes
it when preemption guards are enabled; `ktask/src/api.rs` supplies the provider.
Checked release may schedule; unchecked release requires a later checkpoint.
Neither changes IRQ masks. Before current-task installation both are no-ops.

`kmutex` consumes the execution and diagnostic contracts; `ktask/src/task_api.rs` implements them using
the existing owner-key fallback, scheduler and snapshot machinery. The tests
in `ktask/ut-tests/task_api.rs` cover unknown-token identity and mutex behavior
with that real provider. RCU-specific lifecycle contracts remain separate.

TaskPreemptionIf also provides request-only updates, pending observation,
ordinary checkpoints and scoped deferred checks. TaskIrqExitCheckpointIf
retains the separate IRQ-off return contract; TaskRescheduleRequestIf is the
local scheduler request endpoint. Neither is a CPU notification transport.
The khal normal-IRQ exit path calls TaskIrqExitCheckpointIf after controller
completion and dispatch cleanup; ktask/src/api.rs provides that checkpoint.
The kipi reschedule endpoint calls TaskRescheduleRequestIf during pending
notification handling; ktask/src/api.rs provides the request-only implementation.

## Task Scheduling Observers

`TaskScheduleObserver` is a consumer-implemented callback contract, not a
static kiface provider. ktask invokes it at actual scheduling transitions;
CPU-local resource owners implement load/put without adding their device
dependencies to the scheduler. `ScheduleOutReason` describes the committed
scheduling decision, even when a concurrent wake has changed task state.

Callbacks run on the observed task's stack with ordinary IRQs masked. First
entry is a load; a real switch pairs put on the source CPU with load on the
destination CPU. Same-task reselection is neither. Terminal exit is the last
put and has no matching subsequent load. Callbacks cannot block, allocate,
wait for remote execution, reenter scheduling or acquire run-queue locks.

Linux KVM uses `vcpu_load`/`vcpu_put` and preemption notifiers in
`virt/kvm/kvm_main.c` to separate task scheduling from guest world switches.
The current ktask registration is immutable and task-owned: attach an observer
to an unpublished TaskInner and retain it through terminal exit. Unlike Linux's
scoped notifier list, this supports one composed observer for the whole task;
the consumer can make it inactive outside its resource residency interval.
Dynamic registration/removal and callbacks for idle bootstrap are not provided.
No hardware completion or guest virtual-interrupt algorithm lives here.
