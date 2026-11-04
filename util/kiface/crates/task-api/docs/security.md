# kiface-task-api security

This crate is a trusted kernel contract boundary. It contains no runtime state,
allocation, raw-pointer operations, task pointers, or user-input handling. `kiface`
generates static Rust-ABI calls; the linked provider must obey the documented
context, identity, lifetime and progress obligations.

Blocking-context queries require initialized CPU context outside NMI, preserve
exclusion and pending work, and return false without a usable current task.
They are snapshots, not permits to block after changing execution context.

`TaskExecutionIf::has_current_task` may observe absence after CPU-local storage
initialization, outside interrupt/NMI contexts. It permits masked IRQs and
preemption exclusion, does not schedule, and treats idle as present. Consumers
must not use presence as a substitute for blocking-context validation.

Preemption credits must balance on the same task and must not cross initial
current-task installation. The provider must not schedule on acquisition or
unchecked release. Checked release may schedule only when surrounding context
permits it; consumers combining IRQ exclusion must restore masks first. These
operations neither mask IRQs nor provide NMI mutual exclusion.

`OwnerKey` excludes the unlocked sentinel and encodes the flag-alignment
invariant in its constructors, but key uniqueness and stability
remain provider obligations. Keys must survive task migration and must not be
reused while a consumer still records the owner. Token-derived keys go through
`OwnerKey::from_token`; a consumer must mask the flags off and decode with
`OwnerKey::from_aligned` before treating the field as an identity, so a set
flag bit never turns one task's key into another's.
Absent current-task state must not fabricate an identity. Queries must not block
or unwind on valid use.

Yield and wait require sleepable task context. The pinned borrowed future must
not escape the synchronous wait or move to another task; completion requires
correct wake registration and scheduler progress. This interface offers no
cancellation, timeout, or recovery from invalid execution context.

Diagnostic addresses are opaque identities, never dereferenced. Hooks may run
under IRQ or preemption exclusion and must not block, unwind or reenter the
calling lock. They must not determine lock admission. Numeric keys and lock
addresses remain kernel diagnostics, not public process identifiers.

## Scheduling Callback Boundary

`src/lib.rs::TaskScheduleObserver` is an unsafe implementation contract.
Its unsafe schedule_in/schedule_out methods require the observed task to be
current on the supplied CPU, IRQ/migration exclusion, serialized paired calls,
and an accurate committed outgoing reason. ktask supplies these conditions at
first entry and actual switch sites; arbitrary callers cannot invoke them safely.
ktask retains the observer and calls it only on actual task transitions with
ordinary IRQs masked. The implementer must preserve IRQ/preemption state,
must not allocate, sleep, unwind, enter scheduling, acquire run-queue locks,
or wait for remote work or callbacks needing scheduler progress. The consumer
must establish NMI exclusion separately for resources accessible to NMI.
Migration serializes callbacks for one task, not different tasks sharing an
observer. Resource ownership, lock ordering, and device completion remain the
consumer's responsibility. Strong references back to the observed task must
not create a retention cycle. Scheduler notification alone is not a reclamation
barrier; terminal callback return precedes final stack handoff.
