# RCU List

## Responsibilities

The list owns ordered publication, serialized topology updates and entry
retirement. krcu owns reader tracking, grace periods and callback execution.
Consumers own payload synchronization and semantic teardown, including waiting
for IRQ handlers and controller completion. Linux irqaction uses a private next
chain plus synchronize_irq; it does not itself use the generic rculist API.

Each Entry allocates one Node containing T, an atomic next pointer and an embedded
CallbackNode. Appending consumes Entry. Reads do not allocate or clone ownership.
Removal allocates nothing and does not wait for readers. A consumer can store
Arc<Action> to retain an action independently of its list membership; this adds
an action allocation but no extra refcount operation per traversal step.

`src/lib.rs` owns publication, iteration and writer exclusion; `src/node.rs`
owns embedded callback recovery. Kernel-only scheduler/IRQ test dependencies
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

There is no public detached-node handle, reusable node pointer or mutable payload
access. This avoids a Drop/cancellation path that could release a node before GP.
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
Box deallocation is currently nonblocking; changing the allocator must preserve
an atomic-context release facility for this path.

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
