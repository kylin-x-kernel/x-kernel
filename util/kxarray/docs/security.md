# Safety Boundaries

- Index arithmetic handles all usize values without shifting by the word width.
  Node heights are initialized privately and decrease during descent.
- Every published slot retains exactly one owning reference. Readers use only
  immutable heights and acquire-loaded slots, never an exclusive node reference.
- The sealed Box/Arc conversions preserve pointer provenance. Borrowed payload
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
- Allocation follows the existing infallible kernel allocator policy. Duplicate
  insertion returns the original owner. Prepared-node capacity is recomputed
  under the same lock that protects publication; insufficient capacity is an
  internal invariant failure, not a recoverable allocation error.
- Exclusive Drop has no overlapping array borrows and traverses an acyclic graph
  with bounded depth. The array neither synchronizes business shutdown nor waits
  for external Arc users. Detached callbacks retain their own records and payload
  owners, not the array; they can safely finish after array destruction.
- Set and removal require static ownership. A displaced Box remains raw until its GP ends;
  never reconstruct an exclusive Box while readers may hold shared references.
  Callback recovery uses the actual node-field offset and allocation provenance.
  Unused preparation releases only an empty record. Rejected post-commit
  submission releases the record but intentionally leaks the protected raw
  owner before reporting a fatal invariant violation, never reclaiming early.

Kernel unit fixtures cover full-width sparse growth, conflicts, alignment-one
and zero-sized payloads, Arc retention and exact destruction counts. Kernel
fixtures in `ut-tests/` exercise competing tasks after boot initializes RCU.
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
reuse, full-width paths, preflight rejection and exact Box/Arc release. Kernel
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
