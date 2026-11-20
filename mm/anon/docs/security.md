# `mm/anon` Security and Reliability

## Trust boundaries

The runtime validates user addresses, VMA permissions, source contents, file
extent/invalidation proofs, and PTE expectations. This crate validates private
backing extent/alignment against `PageSize`, retains typed physical ownership,
and serializes object-slot publication. File-cache sources remain file-owned;
only private anonymous results enter this object model. No production unsafe
blocks or raw-frame adoption/release APIs exist in `src/private.rs`.

Object identities, lineage, view registrations, and object-relative discard
offsets retain their existing `vmobj` contracts. File truncation/EOF rounding
must not be applied to anonymous discard ranges.

## Memory and COW invariants

- The immutable PageRef selected page count is the size authority. Construction
  checks it against the supported leaf size and alignment before publication;
  compound backing geometry cannot replace selected-range geometry.
- An immutable page identity owns one physical `PageRef`; every installed managed
  PTE acquires its own hold before publication and retains it through retirement.
- Logical COW shares count owning slots, including prepared and retired
  slots. Observers and physical/PTE references never change this count.
- Checked logical reservation reports overflow as `NoMemory`; backing geometry
  failure is `InvalidInput` for typed commit or unit `InvalidPage` for callback
  transactions. Rejected candidates drop automatically.
- Successful PTE publication is followed only by infallible slot publication.
  A callback returning an error must leave the PTE unchanged and dispose of its
  own unpublished mapping hold. It must not reenter the locked object map.
- Expected-identity and child-collision checks precede publication. Failed fork
  reservation/commit releases only its own guards outside map locks.
- Physical lifetime alone does not protect private write isolation. Removed
  logical shares must remain counted until old-owner accesses end, so another
  owner cannot become exclusive while stale access remains.

## Caller synchronization and destruction

The owning-MM mutex covers each exclusivity check through writable publication
and source fork/share reservation. `is_exclusive()` cannot authorize later
unsynchronized reuse. Split/moved views remain in the same MM; child fork
objects are distinct and their page shares are counted. The public caller must
preserve these ownership/exclusion rules and the existing content-access locks.

Dropping a retired guard ends its logical share immediately. Runtime must first
confirm existing PTE completion or otherwise exclude old accesses. A failed
sparse PTE walk leaves original object slots untouched, including the cleared
prefix. After a successful walk, `retire_range` removes object association and
transfers logical guards one at a time outside the map lock. If completion is
unconfirmed, the runtime retains those guards conservatively, as for replaced
single pages; they must not drop merely because the object slot is gone.
`remove_range` is the confirmed-access specialization that drops each guard.
Dropping an observer needs no such completion. Unpublished fork rollback excludes child
access before dropping reservations and restores parent permissions.

Final object Drop with remaining slots requires owning-MM unmap/completion or
equivalent access exclusion. An object whose slots were transferred to retained
guards may drop independently. Ending a remaining logical share requires the
access proof even when PTE references keep physical storage allocated. The crate does not perform shootdowns or revoke
access on behalf of its caller.

Task-context map mutexes may block. The short logical-count lock neither sleeps
nor calls the allocator/PTE callback. Physical release occurs after leaving that
count lock; failed candidate/reservation cleanup occurs after map locks release.

## Failure handling and limits

| Failure | Behavior |
| --- | --- |
| Geometry mismatch | Consume/drop candidate, do not run callback or alter slot |
| Stale expected/absent slot | Return `Changed`, leave existing ownership intact |
| Callback failure | Return `Commit`, preserve object slot; PTE unchanged by contract |
| Fork share overflow or child collision | Return error and unwind reserved slots |
| Failed range unmap | Original slots remain owned; no rollback storage or insertion |
| Unconfirmed access retirement | Runtime retains removed logical guards; old contents no longer belong to the object |

BTreeMap/Arc allocation follows the repository's existing infallible allocation
model. No recoverable allocation protocol, reclaim/swap, completion queue, or
per-object TLB engine is added. Physical zero-fill and source-copy correctness
remain allocator/runtime obligations before publication. Tests use real backing
and check physical last release as well as logical exclusion; they do not prove
hardware invalidation or guest quiescence.

Current MM retention uses `mem::forget` for the affected guards, with no recovery
queue. The cost accumulates over failed completions and is not globally bounded.
This protects backing lifetime and COW isolation, not revocation of a stale
hardware translation. A later successful PTE flush cannot reclaim a forgotten
logical guard; the independent synchronous TLB contract remains outstanding.
