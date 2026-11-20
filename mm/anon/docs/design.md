# `mm/anon` Design

## Scope and ownership

`mm/anon` owns shared/private anonymous identities, private fork/COW lineage,
and object-side mapped views expressed through `vmobj`. Private file-COW result
pages use the same private object model. Shared storage, content access, page
faults, permissions, and PTE mutation remain runtime responsibilities.

`src/private.rs` owns private page identities and logical page slots;
`src/shared.rs` owns shared identities/views; `src/ids.rs` owns typed identity
allocation. Kernel unit tests live in `ut-tests/` and are mounted with
`unittest::path_mod_test!`.

## Physical backing and logical COW shares

Each immutable `Arc<AnonPrivatePageState>` owns one `PageRef` and a short locked
logical-share count. Construction validates the selected page count and physical
alignment against the requested `PageSize`; the state stores no duplicate size.
`page_size()` derives the validated leaf size from `PageRef::page_count()`, not
from allocation order or the possibly larger compound retained range. Exact and
explicitly compound PageRef values retain their existing allocator protocols;
contiguity does not imply compound ownership.

A private `AnonPrivatePageSlot` owns one logical share. Slots live in the object
map, prepared fork reservations, or retired replacement guards.
Checked fork reservation increments the count; slot Drop decrements it. Moving
slots between these containers does not alter the count. The last object drops
its remaining slots.

`AnonPrivatePageHandle` is an observer/source handle. Its Clone retains the same
identity without adding logical shares. `try_clone_mapping_ref()` acquires an
independent physical hold for a managed PTE without adding a logical owner.
Physical references and observer counts must never decide COW exclusivity.

## Calling context and synchronization

Object maps/views use task-context `ksync::Mutex` and may block. These operations
require initialized heap, scheduler synchronization, page metadata, and physical
allocation infrastructure; they are not IRQ/NMI or early-boot interfaces. The
short existing `SpinNoIrq` count lock covers only a counter operation, never
allocation, map callbacks, physical release, or a sleepable lock acquisition.
Physical hold clone/drop retains the underlying `PageRef` context contract.

Runtime callers hold the owning-MM mutex across fork/share reservation and
exclusive check through in-place reuse/writable publication. `is_exclusive()`
is only a snapshot. A private mapping starts with one root object; split/moved
views stay within that MM; fork creates a distinct child object whose shares
already count against exclusivity. This excludes a new logical share between
an exclusive observation and writable publication. Public callers must preserve
this association and serialization.

Lock order remains owning-MM, object-map mutex, then short logical-count lock.
A commit callback runs under the object-map lock; it must not reenter the object.
The runtime retains its existing content-access and page-table locking rules.

## Publication and rollback

First touch prepares an absent offset. `commit_after(PageRef, PageSize, closure)`
validates geometry, locks/rechecks the absent slot, and gives the callback a
reference to the candidate handle. The callback clones PTE lifetime protection
before publication and returns an error only if the PTE remains unchanged.
After callback success, object insertion has no fallible step under the
repository's existing infallible collection-allocation model. Rejected candidates
release outside the map lock. Typed `commit` supports publication without a
callback only when the caller excludes access during its fallible operation.

COW replacement checks the observed identity and invokes the same candidate
callback while holding the slot lock. It changes the slot only after successful
PTE compare/replace and returns `RetiredAnonPrivatePage` owning the old logical
share. Callback/geometry/identity errors leave the old slot intact and release
only the candidate. No raw PA ownership or released-frame record escapes.

Fork preparation reserves owned logical slots before parent write protection and
child mapping. Commit checks every child offset before moving any reservation
into the child. Failure drops uncommitted reservations after map locks release.
The child remains unpublished/unrunnable; rollback must exclude its accesses and
restore parent permissions before prepared shares end.

Range teardown leaves slots in the original map during the sparse PTE walk;
a failed walk needs no object rollback or reinsertion. After PTE removal,
`retire_range` transfers each removed logical share to the runtime outside the
map mutex. Confirmed access retirement permits `remove_range` to drop those
guards directly. Unconfirmed completion instead retains the removed guards,
without keeping stale contents associated with the object. Neither operation
allocates a key list, release array or detached-slot container. Owning-MM
exclusion covers the whole operation, including the per-slot callback.

## Retirement and final destruction

A removed logical share must remain counted until accesses entitled to its old
contents have ended. Otherwise a sibling could write in place while an old PTE
still observes the page, even with enough physical references to prevent free.
Runtime retains `RetiredAnonPrivatePage` until confirmed old-access retirement.
A range can end its object association before that boundary by transferring the
old logical shares through `retire_range`. The current runtime permanently
retains affected guards when existing completion cannot be confirmed, as for
single-page COW. This may retain old backing and COW shares indefinitely, but
never retains the whole object or associates discarded contents with a new
fault. There is no recovery queue or new TLB protocol in this crate. Removing
object association or returning from software cleanup does not prove remote
permission revocation; the synchronous completion contract is separate work.

Final destruction of an object that still owns slots must follow owning-MM
unmap/completion or equivalent old-access exclusion, since ordinary slot Drop
ends sharing. An empty object may drop while its removed logical guards remain
retained independently. Observer handles may drop independently of revocation;
all physical storage returns through `PageRef` only after its final hold ends.
The retained logical guard and physical mapping hold solve different problems.

## Validation

`ut-tests/private.rs` uses actual typed allocations. It covers geometry and
candidate errors, fork overflow/collision rollback, sparse in-place range removal,
observer/PTE independence from logical shares, a retired share blocking sibling
reuse, and both directions of object-versus-PTE final physical release. These
fixtures exercise ownership without publishing hardware PTEs; runtime completion
and concurrent MM integration need the caller's tests.
