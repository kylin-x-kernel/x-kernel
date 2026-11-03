# Sparse Ownership Index

`XArray<P>` owns one KBox or Arc per usize key. The sealed carrier trait only
describes raw ownership conversion; it does not certify destructor context.
Unsafe insertion admits a value whose entire release chain has been audited
for nonblocking RCU callback execution. The implemented operations are creation,
insert-if-absent, set (insert or replace), scoped borrowing, explicit Arc
acquisition, removal and exclusive Drop.

Nodes contain 64 atomic slots, an immutable height, writer-serialized occupancy
accounting and an embedded retirement callback. Height one stores raw
payload owners; higher nodes store child-node owners. There are no pointer tags,
payload wrappers or owning parent links. The root is null or a node. Maximum
height is ceil(usize bits / 6), including a partial top digit.

Readers acquire-load a downward path under a kernel-issued RCU scope. They never
take the writer lock, allocate or automatically increment a reference count.
Borrowing is bounded by both scope and container. Payload contents require their
own synchronization. Detached nodes remain valid through the reader's grace
period; root growth preserves the previous root as
a descendant. Overlapping reads may miss a newly inserted key, but growth never
removes an existing association.

Writers serialize with one preemption-disabling spinlock. Only task-context
writers take this lock; IRQ/NMI readers use the published atomic topology and
RCU, so writer serialization need not mask ordinary interrupts.
Inspection counts missing nodes;
preparation allocates outside the lock, then repeats inspection. Spare private
nodes are released after unlocking. Published nodes are accessed only by shared
reference; slots are atomically initialized before publication. No payload Drop,
allocation, RCU wait or notification occurs under the writer lock.

Set on an occupied key first performs the RCU task/readiness preflight and
allocates a retirement record outside writer exclusion. It then reinspects the
key under the lock. One atomic exchange publishes the new owner without an
intermediate absence. The displaced owner stays raw in the record until its
post-grace-period callback reconstructs and releases it. Submission happens
after unlocking and does not wait for readers; the record may already be freed
when submission returns. Callback storage and payloads do not borrow the array.
Set requires static ownership because deferred destruction may outlive it.

Set on a missing key uses the normal insertion path without RCU readiness.
If another writer occupies the key while nodes are being prepared, reinspection
requires retirement preflight before replacing it. Preflight rejection returns
the input and preserves the association. Rejection after an admitted exchange
violates the permanent-service/context contract: the record is released but
its raw payload is deliberately retained, then a fatal assertion diagnoses the
invariant failure. Cancellation never reconstructs a retired payload owner.

Final Drop has exclusive access and walks downward, reconstructing each stored
owner exactly once. External Arc owners may outlive the array. No reclamation
service is needed for insertion, root growth or this exclusive teardown.
The allocator must support nonblocking deallocation; a future allocator change
must preserve that contract or introduce a suitable release API before use.

Removal preflights RCU even for an absent key. For an occupied key it prepares
the same independent retirement record outside exclusion, reinspects the key,
then swaps its slot to null under the writer lock. It reports whether that call
withdrew an association, not an old owner. A competing removal may make the
reinspection return false; unused preparation then drops outside the lock.
Submission and payload lifetime rules are the same as replacement. A later
insertion at the same key cannot be affected by the detached old callback.

Removal keeps a bounded root-to-leaf path under the writer lock. Empty nodes
are unlinked bottom-up using occupancy counts, with no full-slot scans. The last
entry clears the root. Nonempty single-child roots are retained, so path height
remains bounded by key width without introducing root-compaction races.
Each detached node submits its embedded callback after unlocking. Callback
submission uses pointers saved on the stack, never links in an already submitted
node. Each callback frees only its node allocation, without following slots or
borrowing the array. Node deallocation, like payload release, must not block.

## Fallible Preparation and Execution Context

Nodes, detached retirement records and unique payload carriers use KBox; Arc
carriers retain their existing ownership behavior. KBox raw conversion preserves
the original pointer and allocator pairing. A displaced owner remains raw until
its grace period ends; converting it back before then would invalidate readers.
There is no new fallible operation after publication or withdrawal.

`insert` now returns `Result<(), (KError, P)>`: AlreadyExists distinguishes a
conflict from NoMemory. `set` and `remove` also report NoMemory before mutation.
Private allocations prepared before failure are dropped outside writer exclusion;
insert/set return the exact offered owner rather than dropping or cloning it.
The failing operation publishes nothing, but does not roll back independent
concurrent writers. A valid-flag duplicate insert and missing-key removal do not
allocate. Missing removal retains the existing RCU preflight requirement.

Default update methods use KERNEL permissions: sleepable, reclaim-safe task
context, enabled IRQs/preemption, no non-sleepable RCU scope, and no held locks
needed by reclaim, IO or filesystem entry. `insert_with_flags`, `set_with_flags`
and `remove_with_flags` allow explicit permissions. They reject unknown flag bits
before inspecting the tree, including no-op operations. NOWAIT allows initialized
early-boot insertion/root growth without sleeping reclaim, but is neither NOFAIL
nor an IRQ/NMI access permission, reserve guarantee or latency bound. Replacement
and removal still require RCU-admitted watched task context outside read scopes.
NOIO and NOFS still allow sleeping. Flags do not govern payload creation, nested
allocations or destruction. Readers and publication/withdrawal remain allocation-free.

The private allocator interface is statically dispatched and returns KBox owners;
production uses AllocFlags directly. Tests substitute per-call reservation failure,
not a global allocator or a copied mutation algorithm. The host KBox backend lets
storage/Miri tests retain their isolation from kernel task and interrupt providers.

## Test Ownership

Basic kernel fixtures remain in `ut-tests/`. Task concurrency, interrupt access
and migration fixtures belong to the separate `crates/kernel-tests/` crate. The normal
boot sequence starts RCU before registered tests; XArray adds no boot fixture.
The unittest entry selects both crates through `kfeat/kernel-tests`; its optional
integration dependency receives SMP coverage through weak feature forwarding.
Ordinary system configurations select neither crate merely for tests. Runtime
contains neither XArray assertions nor an XArray dependency.

The kernel runner builds libraries with `cfg(unittest)`, so these dependencies
cannot be supplied as Cargo dev-dependencies. Keeping them in the integration
crate avoids container-to-consumer dependency cycles when kirq uses XArray.
Concretely this prevents `kirq -> kxarray -> kirq`, also through `ktask`.
This is a dependency-cycle exception, not the default kernel test layout;
ordinary UT, host tests and doctests stay in the owning crate.
The test crate resolves its kernel dependencies on kernel targets, but is not a
default system dependency. The container exposes three observation helpers
only under `cfg(unittest)`; they are not part of its production API.
