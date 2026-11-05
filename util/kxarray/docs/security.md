# Safety Boundaries

The `Send`/`Sync` implementations for `PreparedRemove<P>` in `src/prepared.rs`
cover empty, unsubmitted storage only: it owns no payload or borrowed tree
pointer, exposes no shared mutation, and transfers no admission permission.
Construction and consumption require `EntryOwner`; the storage type itself
does not, allowing a future payload to retain its own removal record without
recursive trait obligations. `Retirement<P>` remains private, with owner-bound
operations. `PendingRetirement<P>` remains task-local after withdrawal.

`prepare_remove` allocates only an empty `PreparedRemove<P>` record and checks
allocation flags. Its unsafe boundary requires valid allocation/cancellation
context, not RCU readiness. `remove_prepared` requires ordinary removal's
execution and permanent-service obligations and rechecks preflight immediately
before mutation. An old successful preparation cannot authorize removal with
IRQs disabled. Writer exclusion spans occupied-path inspection and withdrawal;
`Retirement::submit_with_nodes` submits uniquely detached storage only after
unlocking, using the stack array rather than already-submitted nodes to iterate.
An admission error or absent key drops only unused storage, outside the lock.
No borrowed payload or path lives in preparation. Kernel tests cover changed
context rejection, absence, real callback drain and retained external ownership;
host tests cover failed preparation and cancellation without any RCU provider.

The unsafe `XArray::withdraw_prepared` entrance in `src/lib.rs` intentionally
does not run sleepable-context preflight while the caller's short lock is held.
The caller must preflight in watched task context first, prepare all fallible
resources and serialize any identity checks. Only temporary IRQ/preemption
exclusion may intervene through the transaction. Withdrawal transfers the raw
payload and uniquely detached nodes into `PendingRetirement` in `src/prepared.rs`;
no borrowed value or callback is exposed. Its unsafe `retire` consumes those
rights after caller locks are released and preflight context is restored, with
the permanent service/CPU contract still satisfied. No scheduler or RCU
bookkeeping lock, RCU scope, explicit context transition or arbitrary caller
callback may intervene. Abandoning the pending record leaks protected storage;
its ordinary Drop cannot reclaim a payload or node still visible to readers.
The record's raw pointers prevent Send/Sync and its single-use ownership prevents
double submission. Compile-fail examples check task transfer and repeated use.
Kernel fixtures withdraw two indexes under one IRQ-safe outer lock and submit
after release; an SMP reader retains both old borrows through submission and
observes one shared payload destructor only after exit and callback drain.

`src/prepared.rs` stores only private unpublished nodes and a fixed key.
`prepare_insert` requires valid allocation context and permissions, validates
flags, and releases partial capacity on failure. `insert_prepared` requires
the same owner-release and task/initialized-boot obligations as insertion,
without a sleeping allocation requirement. It checks conflicts and publishes
under writer exclusion; it never retains a stale tree path from preparation.
The complete-shape capacity bound is documented in design.md. Spare nodes are
released outside the lock, and no callback is submitted for unpublished nodes.
The token cannot be cloned or reused after commitment. Preparing it does not
grant IRQ/NMI access or RCU participation. Host tests inject each allocation
failure and count cleanup, change root shape before commit, and verify zero
allocations during success/conflict with exact returned-payload identity.

`src/lib.rs::get_owned_locked` protects raw traversal and Arc acquisition with
the same writer lock used by all detach operations. The shared array borrow
excludes destruction, and the stored owner stays live until the extra strong
reference is established. No raw borrow escapes. Its caller must not be an
IRQ/NMI/serving-softirq handler or reenter a held writer lock. Local lock
primitives must be initialized; an RCU service/current reader is not required.
The returned owner has the usual caller-side destruction-context obligations.
Host tests check absence, boundary keys, identity/counts, allocation-free lookup,
root-growth competition and retention past array destruction without a forged
RCU scope. They do not establish early kernel startup or interrupt-context safety.

Allocation permissions are caller obligations, not runtime context detection.
Default updates require sleepable, reclaim-safe task context with IRQs and
preemption enabled, outside non-sleepable RCU scopes and locks needed by reclaim.
Explicit NOWAIT prevents sleeping reclaim only for private node/record storage;
it does not legalize updates from IRQ/NMI or bypass replacement/removal preflight.
Unknown bits are rejected before tree access. No owner from the standard Box
allocator is accepted by the sealed KBox carrier implementation.

Failure fixtures inject NoMemory before reservation success at successive points
in a growing path, with and without an existing root. They verify root stability,
absence of the new key, returned-owner identity and unlocked allocation. Kernel
fixtures cover missing-key set failure and retirement failure for set/remove,
preserving the old association and strong counts. Conflict and missing-removal
checks do not consume allocation attempts. Invalid flags cover all public update
families, including occupied and unused slots of an already allocated leaf.
Host failure tests cover every reservation in both empty-root and anchored
full-width growth. A thread-local counting System allocator checks that all
successful reservations are deallocated before the failing operation returns,
while the array still exists. Miri additionally checks partial-node cleanup;
these fixtures do not simulate reclaim,
global OOM or the kernel RCU preflight. Nested payload and Arc allocation failures
remain outside this migration. Concurrent-writer progress is not rolled back by
another operation's reservation failure.

- Index arithmetic handles all usize values without shifting by the word width.
  Node heights are initialized privately and decrease during descent.
- Every published slot retains exactly one owning reference. Readers use only
  immutable heights and acquire-loaded slots, never an exclusive node reference.
- The sealed KBox/Arc conversions preserve pointer provenance. Borrowed payload
  references are not used to reconstruct Arc allocation headers; independent
  Arc acquisition uses the original stored pointer and increments first.
- The owning carrier parameter is invariant: shared insertion cannot shorten a
  carrier lifetime and smuggle a temporary reference into a longer-lived array.
- Safe borrowed lookup requires a genuine kernel RCU scope and a live array.
  The scope cannot be forged or transferred by safe consumer code.
- Internal raw traversal requires a read scope, writer exclusion or proof that
  destructive mutation is absent. Container lifetime alone does not protect
  nodes withdrawn from the published tree.
- Writer-serialized occupancy counts track nonnull slots; readers never consume
  them. Empty-node withdrawal clears parent links before callback submission.
  Detached nodes cannot be reused and remain raw until their grace periods end.
  Node callbacks free one allocation only, never recursively destroy its slots.
- Unsafe insertion certifies caller context and the full destructor chain.
  Send/Sync alone is not evidence of callback-safe destruction. Sleeping Drop,
  unwinding and NMI destruction are unsupported. Independent external Arc owners
  remain responsible for their eventual release context.
- Node and retirement-record allocation is fallible and occurs before publication
  or withdrawal, outside the writer lock. Failure returns NoMemory; insertion
  conflicts return AlreadyExists, and insert/set return the unchanged offered owner.
  A partially prepared path owns only unpublished nodes and drops them on error.
  Prepared-node capacity is recomputed
  under the same lock that protects publication; insufficient capacity is an
  internal invariant failure, not a recoverable allocation error.
- Exclusive Drop has no overlapping array borrows and traverses an acyclic graph
  with bounded depth. The array neither synchronizes business shutdown nor waits
  for external Arc users. Detached callbacks retain their own records and payload
  owners, not the array; they can safely finish after array destruction.
- Set and removal require static ownership. A displaced KBox remains raw until its GP ends;
  never reconstruct an exclusive KBox while readers may hold shared references.
  Callback recovery uses the actual node-field offset and allocation provenance.
  Unused preparation releases only an empty record. Rejected post-commit
  submission releases the record but intentionally leaks the protected raw
  owner before reporting a fatal invariant violation, never reclaiming early.

Kernel unit fixtures cover full-width sparse growth, conflicts, alignment-one
and zero-sized payloads, Arc retention and exact destruction counts. Kernel
integration fixtures in `crates/kernel-tests/ut-tests/` exercise competing tasks after
boot initializes RCU. Their task/IRQ dependencies are not container dependencies.
Host storage tests compare boundary keys
against a reference map and exercise ownership under Miri; public API doctests
reject scope escape and container destruction while borrowed. See
`../tests/README.md` for commands and coverage boundaries. These tests do not
prove all weak-memory or NMI interleavings. Replacement fixtures check held V1/V2
borrows across publication of V3, exact release counts, independent Arc retention
and preflight rejection with IRQs masked. Ordinary UT does not cover operations
before RCU service startup. Host/Miri fixtures check raw-owner cancellation and
GP-eligible callback recovery without
claiming to validate the GP engine. Removal fixtures check missing keys, key
reuse, full-width paths, preflight rejection and exact KBox/Arc release. Kernel
integration fixtures retain a real old-value scope across removal and reinsertion,
and check that competing removers withdraw one owner. Pruning fixtures check empty roots
and retained siblings. Exhaustive mutation interleavings are not covered.

Kernel integration fixtures enter read scopes from actual timer hardirq and
serving-softirq callback delivery, checking watching and context before entry.
Timer teardown synchronizes delivery before dropping captures. Softirq records
retain their own container share through callback completion. These fixtures do
not prove early exception-entry safety or real NMI delivery.
Controlled NMI fixtures additionally read with the actual writer lock held at
stable tree states. Nested scopes must leave reader bookkeeping and watching
unchanged; the fixture measures zero successful allocation bytes on that CPU.
The helper is unittest-only; no
arbitrary callbacks execute under the production writer lock. Additional
controlled NMI fixtures interrupt partial child publication through a private
new root and an already reachable empty leaf; they check old values and absence
until payload publication. These fixed software windows do not establish
hardware NMI or exhaustive weak-memory safety.
An SMP fixture retains an old-value scope across actual reader migration and
confirms destination execution, original-node enrollment and post-exit cleanup.
A prepared-writer fixture also migrates a writer after preflight and retirement
preparation, before locked reinspection and mutation. It checks the resumed CPU,
association and exact release. This does not force migration specifically
between publication and callback submission or cover every scheduling interleaving.

## Quiescent fixture removal

`src/lib.rs::take_quiescent_fixture` exists only under `cfg(unittest)`. Its
unsafe caller must exclude same-key borrowed readers and writers, and provide
allocator-capable task context. The writer lock retains traversed nodes, but
cannot by itself exclude old RCU payload borrows. No node is pruned, so unrelated
readers remain safe. `Arc::from_raw` consumes exactly the removed slot's owner;
independently retained Arcs may survive. Production callers must use normal
RCU withdrawal and retirement instead.
