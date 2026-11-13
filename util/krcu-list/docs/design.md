# RCU List

The kernel integration test crate prepares task entries with PreparedTaskEntry
in sleepable, reclaim-safe coordinator context before starting readers. Stress
writers are all prepared before activation, so no worker waits behind a start
gate while another entry is still being allocated. Container code and the
integration dependency split are unchanged.

## Fallible entry preparation

Entry owns a KBox node. `new` returns KResult with KERNEL permissions and
requires sleepable reclaim-safe task context before taking writer locks.
`new_nowait` and `new_with_flags` expose explicit preparation permissions;
unknown flags and allocation failure drop the unpublished payload in the caller.
NOWAIT does not promise NMI safety, bounded latency or reserved memory.
Publication, unlink and embedded retirement do not allocate a second owner.
The callback recovers the original KBox only after the protecting grace period.


## Responsibilities

The list owns ordered publication, serialized topology updates and entry
retirement. krcu owns reader tracking, grace periods and callback execution.
Consumers own payload synchronization and semantic teardown, including waiting
for IRQ handlers and controller completion. Linux irqaction uses a private next
chain plus synchronize_irq; it does not itself use the generic rculist API.

Each Entry allocates one Node containing T, an atomic next pointer and an embedded
CallbackNode. Appending consumes Entry. Reads do not allocate or clone ownership.
`Entry::get_mut` permits final payload initialization while uniquely owned and
unpublished; linked entries expose no mutable payload accessor.
Asynchronous removal allocates nothing and does not wait for readers. A consumer can store
Arc<Action> to retain an action independently of its list membership; this adds
an action allocation but no extra refcount operation per traversal step.

`src/lib.rs` owns publication, iteration and writer exclusion; `src/node.rs`
owns embedded callback recovery. `src/take.rs` owns prepared task-side removal.
Kernel-only scheduler/IRQ test dependencies
reside in `crates/kernel-tests`, not in the reusable container.

The allocation lifecycle is:

```text
unpublished Entry --push_back--> linked Node --unlink--> retired Node
        |                          |                       |
        Drop                       list Drop               completed GP
        |                          |                       |
        +---- current-context free +                       callback free
```

Only unpublished ownership can be discarded without a GP while the list is
shared. Exclusive list destruction is legal because iterator borrows exclude it.

## Concurrency and Lifetime

Release publication and Acquire traversal expose initialized nodes. Writers use
one per-list SpinNoPreempt lock; interrupt readers never take it. Mutation is
task-origin only, so the list does not disable IRQs. Caller-side line state is
not protected by this lock; KIRQ must retain its own management serialization.
A consumer spinlock must not surround remove_first: preflight requires sleepable
task context and retirement submission must be outside atomic exclusion. A
sleepable management mutex may serialize the consumer's larger transaction,
provided callbacks do not need that mutex to finish.

Unlink updates the predecessor or head and, if necessary, the writer-only tail.
It never clears the removed next link. A reader paused before B may still visit B
and C after both are removed. Each removal's GP protects readers that could have
obtained that node, including through a previously detached predecessor.
Concurrent traversal is not a snapshot, and repeated appends need not yield a
fixed-length traversal. Consumers requiring admission cutoff own that policy.

There is no public raw detached-node pointer or mutable payload access while
readers may exist. Opaque task-owned retirement deliberately leaks on abandonment
rather than releasing a node before its GP.
The iterator borrows both list and ReadScope, cannot escape either, and is not
Send. Exclusive list Drop releases only still-linked nodes; retirement callbacks
are independent and never dereference next or the former list.

## Context and Failures

Task and already-watched interrupt/NMI readers reuse krcu admission. Preparing
entries follows allocator context restrictions. Appending needs task/preemption
facilities but no running RCU service. Removal checks service/context before the
predicate or any unlink, then submits after releasing its writer lock.

Preflight is a snapshot, not a transferable reservation. The predicate cannot
block or change context. Under the permanent service/CPU contract, submission
after successful preflight is expected to succeed. Unexpected rejection retains
the entire detached allocation and diagnoses the invariant violation, rather
than freeing it or pretending removal did not happen.

Arbitrary T: Send + Sync does not make destruction callback-safe. remove_first is
unsafe and explicitly requires a nonblocking, nonunwinding release chain. Sleeping
destruction needs an appropriate existing finalization owner, not a new RCU mode.
KBox deallocation is currently nonblocking; changing the allocator must preserve
an atomic-context release facility for this path.

## Prepared Task-Owned Removal

`with_prepared_take` uses krcu's prepared grace-period capability before invoking
the consumer. Its single-use `PreparedListTake` binds the list and preparing task.
`withdraw_first` may run inside a consumer short lock, then takes the list writer
lock internally. It performs the same link-preserving unlink as asynchronous
removal, without submitting the embedded callback or allocating. Predicate and
writer constraints remain unchanged; consumers order their outer locks.

The returned `RetiredListEntry` exposes only match presence. After short locks
are released, `reclaim` submits the prepared marker, waits for the post-unlink
GP and returns T to the task. Unmatched removal skips submission. KBox recovery
is shallow: the stale successor pointer is never followed. The container frees
node storage but never destroys T in an RCU callback. Consumers may retain T
through a separate execution/controller drain and drop it after management unlock.
This reuses the prepared wait already used by RcuSlot; it adds no callback-side
terminal notification or new reclamation service.

Preparation errors precede mutation. Cancelling unused preparation leaves the
list unchanged. Abandoning a detached entry leaks it; cancellation is not a GP.
Neither prepared nor retired handles may escape the preparing stack or migrate
to another task. CPU migration by the same task is allowed. Waiting requires
restored watched sleepable context and a permanent available RCU service.
The wait marker currently follows krcu's fatal-OOM allocation policy, unlike
fallible entry preparation. Existing asynchronous removal remains unchanged.

## Test Dependency Selection

The separate test crate exists to prevent a concrete consumer cycle: KIRQ's
planned list dependency would otherwise meet the context fixtures' reverse
`krcu-list -> kirq` or `krcu-list -> ktask -> kirq` dependency. That consumer
integration is not implemented by this crate. `cfg(unittest)` hides test code,
but does not remove Cargo dependency edges; this kernel runner builds libraries,
not Cargo test targets that can use dev-dependencies for these fixtures.

The acyclic shape is `kernel-tests -> {krcu-list, kirq, ktask}`; the container
and its consumers never depend on the test crate. This is not a system-wide
test organization rule. Basic UT, host tests and doctests remain in the container.
Other crates should retain their tests locally unless a concrete dependency
cycle requires this exception. Kernel-context testing by itself is insufficient
justification for creating another crate.

`entry/unittest` enables the optional container test links through
`kfeat/kernel-tests`. SMP only forwards to an already-selected integration test
crate; it does not activate that crate. The kernel context fixtures remain in
the separate child crate so production container dependencies stay independent
of IRQ and scheduler integration. Kernel test linting uses `make UNITTEST=y clippy`.

## Linux Mapping

- include/linux/rculist.h: initialized publication, serialized writers and
  list_del_rcu's preserved forward link.
- Documentation/RCU/listRCU.rst: concurrent visibility and release after GP.
- kernel/irq/manage.c: request_mutex and unlink followed by synchronize_irq are
  consumer responsibilities, not generic container semantics.

Rust uses owned entries, scoped borrows and embedded callbacks instead of exposing
arbitrary list_head adapters. Ordinary intrusive-list removal resets links and
cannot implement this protocol without changing its existing contract.
