# kcompletion safety and reliability

## Trust boundaries and external inputs

Only kernel callers provide wake capabilities and predicates. There are no user
pointers, DMA buffers, MMIO accesses, firmware inputs, or external byte parsers.
IRQ and device events reach this crate through host notification calls. Their
rate controls callback frequency and IRQ-off broadcast duration. The unsafe
SyncWake implementation and unsafe wait entry points are host trust boundaries.

## Unsafe inventory and memory invariants

All production unsafe code lives in `src/completion.rs`:

| Site | Invariant and enforcement |
| --- | --- |
| `notify_borrowed` | The erased pointer is paired with its monomorphized W trampoline in `with_waiter`; the guard retains the shared W borrow. W implements Sync. |
| `CompletionState: Send` | Access to links, list and callbacks is serialized by the owning SpinNoIrq; no node or borrowed W moves through the list. |
| `wake_one` | A popped node remains pinned and alive until source-locked cleanup completes, including the callback after removal. |
| `UnsafeRef::from_raw` insertion | A private pinned node belongs only to this list. Its storage remains live and no mutable node reference exists until all non-owning entries are gone. A locked `is_linked` check skips duplicate insertion without modifying FIFO order. |
| `cursor_mut_from_ptr` removal | Called only when the node is linked; private registration permits membership in this source only. The same source lock covers the membership check and cursor removal. |
| `Registration::drop` | Removes only its own scoped node and always acquires the source lock, including after another CPU popped the node. |

No raw pointer or registration guard is exposed. The synchronous scope keeps
the node pinned and wake borrowed until cleanup finishes. This handles return,
spurious wake, token stealing, and closure unwind. The unsafe caller must also
prevent stack, source and wake reclamation if nonunwinding task exit skips Drop.
Rust borrowing does not enforce that scheduler-specific guarantee. Pinning
alone cannot prevent a dangling node after such an exit. A safe scheduler
wrapper must retain task/stack ownership throughout the raw wait and all its
callbacks; skipped cleanup may leak that owner, but must not free the stack.
Referents outside that stack need their own retention. SyncWake cannot enforce
this for arbitrary closures, hence the raw wait entry points are unsafe too.
Links use the intrusive library's interior mutability; callbacks only receive
shared W references. No detached stack-node wake lists escape the lock.

## Thread safety and execution context

Completion and WaitQueue serialize their state with SpinNoIrq. Notification
may run on another CPU while the waiting stack remains suspended or resumes;
cleanup waits for notification under the same lock. Callbacks must obey the
unsafe SyncWake IRQ/noallocation/nonblocking/nonunwind/no-retention contract.
No direct or indirect lock cycle may lead back to the source. The independent
parker must close the notification-versus-block race. The source itself never
holds its lock during park or predicate execution.

## Threat analysis

| ID | Threat | Impact | Mitigation / residual risk |
| --- | --- | --- | --- |
| T-01 | IRQ races stack teardown | High: dangling pointer | Private pinned scope and locked cleanup serialize with all callback access. |
| T-06 | Nonunwinding task exit reclaims an enrolled stack | High: dangling pointer | Unsafe caller prevents cleanup bypass or retains stack, source and wake if bypass occurs. Ownership must span all callbacks, not just park. |
| T-02 | Host callback reentry or lock inversion | High: kernel deadlock | Explicit unsafe host contract; completion cannot inspect arbitrary host lock graphs. |
| T-03 | Notification arrives before park | Medium: stuck wait | Register/check protocol plus required host notification latch; a faulty host can still lose wakeups. |
| T-04 | Event flood or many readers | Medium: IRQ latency | Wake-one is constant work; broadcast is explicitly linear with no detached node processing. |
| T-05 | Reset abandons old waiters or deferred handle is dropped | Medium: stuck wait | Documented reset/dispatch obligations; delayed dispatch never republishes stale completion. |

## Failure modes

| ID | Failure | Effect | Severity | Handling |
| --- | --- | --- | --- | --- |
| F-01 | Predicate or park unwinds | Interrupted operation | 3 | Private guard removes the stack node before propagation. |
| F-02 | notify unwinds or blocks | Violated host contract | 1 | Prohibited by unsafe SyncWake; implementations require audit. |
| F-03 | Task context is not sleepable | Host failure/deadlock | 2 | Host validates before entering wait; no task dependency exists here. |
| F-04 | Final deferred Arc release in unsuitable allocator context | Host allocator failure | 2 | Caller obeys allocator context requirements; no release is moved under the source lock. |
| F-05 | Ordinary token count saturates | Permanent readiness until reset | 3 | Intentional Linux semantics, covered by a boundary test. |

## Privacy and known limits

No user payloads are processed or logged. Host probes replace IRQ locking with
a mutex and therefore cannot prove interrupt masking, preemption handling,
scheduler lifetime, or host lock ordering. Allocation checks exclude test setup
and final owner destruction; the final deferred release may legitimately free
the source. Timeout/cancellation and forced task teardown are not implemented.

## Audit checklist

- All link changes and borrowed callbacks hold the same source lock.
- Return and unwind take that lock before releasing node and W lifetimes.
- Nonunwinding exits cannot reclaim stack, source or wake before cleanup; audit the unsafe host wrapper's retention across all callbacks.
- No callback captures an erased pointer for later access.
- Host notify and park use the same persistent latch and correct lock order.
- Predicate evaluation follows enrollment and runs outside the source lock.
- Reinit cannot abandon old waiters; deferred wake does not republish state.
- No future change exposes a forgettable registration or detached stack list.
