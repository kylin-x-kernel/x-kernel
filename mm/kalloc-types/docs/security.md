# kalloc-types Safety and Reliability

## Shared Owner Boundary

KArc and KWeak expose only sized, typed ownership. They do not export the
internal allocator or permit reconstructing owners from foreign pointers.
The standard Arc implementation owns the strong/weak counters and their
ordering; its Send/Sync constraints require both Send and Sync from T.
This avoids PR #385's weaker Sync bound, which could permit cloning/upgrading
on a different thread and transferring destruction of a non-Send payload.

The unsafe implementation includes SharedAllocator and raw KArc recovery.
Allocation returns an
aligned region for the requested Layout through KBox's backend; deallocation
accepts only the matching live allocation transferred by Arc after all strong
and weak users finish. Zero-size allocator requests return an aligned dangling
region and require no free; actual Arc count headers are nonzero-sized even
when T is a ZST. The host allocator pair and kernel allocator pair never mix.

| Failure / risk | Handling and remaining obligation |
| --- | --- |
| Invalid flags or reservation OOM | Return InvalidInput or NoMemory; input drops once; no owner published |
| Upgrade racing final strong drop | Standard Arc either grants a live strong reference or returns None; no resurrection |
| Payload destructor unwinds | Standard Arc releases the implicit weak through its guard; surviving weak handles cannot upgrade |
| Final weak release in an unsuitable context | Caller must obey allocator lock/reentrancy constraints; constructor flags are not destructor permissions |
| Strong reference cycle | Leak unless callers break the cycle; KWeak is available for back-references |
| Reference-count overflow | Inherited standard Arc abort protection, not a recoverable allocation failure or wrapping counter |
| Raw reference recovered twice, from an address borrow, or from another owner type | Violates from_raw's unsafe contract; one unclaimed KArc strong reference is required |
| Stateful allocator introduced without preserving its release authority | Invalidates pointer-only raw recovery; backend changes must revise the representation and recovery proof |

`into_raw` does not decrement the count or drop the payload. Recovery uses the
same SharedAllocator type and allocation domain, with canonical NOWAIT flags;
release never inspects allocation flags or the test-only failure switch. This
is equivalent release authority, not conversion to Global or reconstruction
from an arbitrary address. as_ptr does not extend lifetime or grant mutable
access. Transferring a raw pointer does not bypass Send/Sync requirements.
Final strong/weak destruction after recovery retains the normal context rules.

Raw round-trip tests cover both allocation policies, independently transferred
references, extraction, aligned ZSTs and weak release through recovered
allocator metadata. Optional KArcPointerOps delegates directly to that raw-owner
contract, without a new allocator, refcount or queue identity. Its safety depends
on the concrete adapter preserving link projections and the library returning
each transferred pointer once. Conversion does not authorize arbitrary removal
or release under a lock; those obligations remain with the subsystem.
Library duplicate-insertion panics and fast_clear's intentional ownership leak
are not converted into recoverable cancellation. Normal teardown is covered;
complete draining after a panicking payload destructor is not guaranteed by
this adapter. Kernel panic handling and any hosted unwind-cleanup protocol remain
outside its ownership-conversion contract.
Hosted intrusive_collections probes exercise independent links, duplicate-insertion
panic recovery, removal and normal list teardown. Production adapters must own
and release every linked reference and enforce the list's removal preconditions.

Allocation-failure injection changes only the private allocator result before
the same Arc::try_new_in call used in production. It does not inject payload
construction failure or global allocator exhaustion. Concurrent and unwind
tests are hosted; guest tests do not unwind the kernel. No timer call site is
migrated by introducing these types, and no complete OOM-safety claim is made.
The empty-weak test checks identity/upgrade behavior, not allocator traffic;
invalid-flag tests check the returned error and input cleanup. Allocation-free
clone/downgrade/upgrade and validation-before-allocation are additionally
established by source inspection, not an allocation-counting regression test.

## KBox raw ownership boundary

A raw transfer suppresses owner destruction, not pointee initialization.
Recovery requires exactly one outstanding ownership right and the original
non-null, aligned pointer, allocation provenance, type and DST metadata.
No conflicting references may remain. Repeated recovery, foreign pointers and
premature recovery during RCU reads violate the unsafe contract.
`KBox::leak` is the safe alternative that never recovers ownership: the caller
receives an unbounded borrow, so the allocation and value are intentionally
retained for the remainder of execution.
Dynamic drop still computes the layout while the value is valid, then destroys
it once and frees with the matching backend. ZSTs never allocate or deallocate.
Tests cover sized/DST address and metadata retention, single destruction,
aligned ZSTs and empty slices; no malformed pointer is passed to the unsafe API.

Hosted allocation returns NoMemory on null and frees through the same host
allocator. Kernel allocation never takes that path. Host/Miri results establish
ownership properties, not kernel allocator timing or execution-context safety.


## Copied Slice Ownership

`from_slice_with_flags` reserves the complete slice before copying valid Copy
elements. The source remains unchanged on error, and assume_init runs only after
all non-ZST elements have been written. ZST validity comes from the source slice;
Copy excludes destructors and no per-element loop is needed. Tests cover aligned
elements, independent ownership, empty slices, invalid flags and huge ZST slices.

## Sized Ownership Extraction

`KBox::into_inner` takes exclusive ownership, so no safe borrow can survive the
move. Its pointer read transfers exactly one initialized, correctly aligned T;
ManuallyDrop prevents KBox destruction from dropping that value again. The
Storage guard only releases memory, using the same sized layout as construction,
and never dereferences the moved-from slot. ZSTs skip deallocation. This extraction
exposes no raw pointer or allocator injection and is not a pinning operation.
The returned value's destructor runs in its eventual owner's context, not during
extraction. Tests cover single destruction and preserved payloads; they do not
force global OOM or establish that future task/signal migration is failure-safe.

## Fallible work callbacks

Callback allocation failure drops captured values without invoking the callback.
Consuming a reusable callback destroys its captures exactly once after invocation.
Borrowed captures remain bounded by their original lifetime. Tests cover generic
Fn/FnOnce bounds, erased FnMut state, and injected failure with KERNEL/NOWAIT.
These are local allocator tests, not global OOM injection or proof that nested
allocations inside callbacks are fallible.

## Fallible directory iterators

Iterator forwarding preserves the underlying iterator's execution-context
requirements. Allocation flags neither constrain next/nth nor the iterator's
destructor. Failed allocation of a chained iterator drops both input owners;
private allocator-injection tests cover this cleanup without exhausting memory.

## Boundaries and Invariants

The inputs are valid Rust values and an allocation policy declared by a trusted kernel caller; there are no raw user pointers.
The type cannot prove that the caller is actually in a sleepable context, so the caller must audit lock and interrupt state.
The underlying `kalloc` must return exclusive storage that satisfies the layout and must support freeing across CPUs.
`new` allows sleeping by default; even if the current backend does no reclaim, it must not be used on paths where sleeping is forbidden.
`new_with_flags` rejects unknown bits; all known bits are reclaim permissions and do not require the backend to perform them.
Without `DIRECT_RECLAIM`, sleeping is not allowed, and the IO/FS bits cannot bypass this restriction.
NOIO/NOFS constrain only this one allocation and do not establish an implicit reclaim scope across the call chain.
Both policies allow sleeping and cannot be used as a substitute for NOWAIT. Even when the lock held is a sleepable lock, the caller
must check reclaim dependencies to avoid reclaim waiting on a lock the caller already holds. The interface does not automatically detect
the current context or downgrade the policy. NOWAIT does not promise NMI safety or reentrancy under arbitrary locks.
On failure the input value is destroyed immediately; the final Drop runs in the context of the releasing party and does not inherit the allocation flags.
The caller must separately guarantee that these destruction paths are allowed to run in their respective contexts.

`src/kbox.rs::new_with_allocator` is a private audit boundary: production passes
only `allocate`; test substitutes only return failure. Any successful substitute
must satisfy the same layout, exclusivity and allocation-origin requirements. Allocator
injection remains private; raw ownership recovery has its separate unsafe contract.

## Unsafe Inventory

| Location | Invariants and constraints |
| --- | --- |
| `KBox::new_with_allocator` | The backend provides storage with a validly aligned layout; ZSTs use an aligned dangling pointer; no reference is created before the write |
| `Deref` / `DerefMut` | A KBox can be constructed only after full initialization; the reference lifetime is bounded by the borrow of the owner, and write access requires an exclusive borrow |
| `KBox::drop` | Compute the dynamic layout while the value is valid; the stack-local Storage guard frees storage after destruction or unwinding, without accessing the destroyed value |
| `KBox::into_inner` | A consumed sized owner supplies initialized aligned storage; ManuallyDrop prevents duplicate destruction, and Storage frees only the original allocation after the move |
| `Send` / `Sync` | Only values that satisfy the corresponding trait may be moved or shared across threads; the internal pointer is not exposed |

## Threat Analysis

| ID | Threat description | Impact level | Trigger condition | Mitigation |
| --- | --- | --- | --- | --- |
| T-01 | Memory pressure causes a fatal OOM | Medium | The layout can be satisfied but the backend has insufficient resources | Call the fallible kalloc interface directly and return KError::NoMemory, bypassing the GlobalAlloc fatal path |
| T-02 | Memory corruption from an incorrect layout or double free | High | Corruption of private storage provenance or DST metadata | Private fields and no Clone/raw API; compiler-generated unsizing preserves provenance, and the dynamic layout matches the original allocation |
| T-03 | Later reclaim blocks in a non-sleepable context | High | Misuse of the default new or ignoring NOWAIT | Non-sleeping call sites use new_nowait; future dispatch must honor flags; the current backend performs no sleeping reclaim |

## FMEA

| ID | Failure mode | Failure cause | Local effect | System effect | Severity | Mitigation |
| --- | --- | --- | --- | --- | --- | --- |
| F-01 | Allocation failure | Out of memory | The input value is released and no owner is returned | The caller decides whether to propagate or retry | 3 | Error return; local failure injection verifies destruction |
| F-02 | VFS context initialization failure | Out of memory | No context is published and no lock is installed | The current lock/flock returns ENOMEM | 3 | Once is reset and can be retried; reused after success |
| F-03 | Value construction or destruction itself fails | External logic of T | Not converted into a KError by KBox | A kernel panic is still possible | 2 | The failure guarantee explicitly covers only object storage allocation and does not promise complete OOM safety |

## Reserved storage initialization

AsRef/AsMut delegation introduces no raw pointers or extended lifetimes. Debug
borrows the pointee and may do whatever its formatter does; allocation flags do
not make formatting safe under arbitrary locks. A slice regression verifies
borrow identity, mutation and delegated debug output.

The stored value before initialization is a valid `MaybeUninit<T>`, never an
uninitialized `T`. Its size and alignment equal those of T, including aligned
ZSTs. `KBox::write` first initializes the slot through exclusive safe access,
then suppresses destruction of the old owner and transfers its pointer to the
initialized owner. No reference to T is formed before initialization and the
allocation is released exactly once using the unchanged layout. Overwriting an
already written MaybeUninit slot does not drop its former value, matching the
underlying type's contract. Tests cover address/alignment preservation, single
destruction, abandoned sized/ZST reservations, invalid flags, and local allocator
failure. These tests do not force global OOM.

## Runtime-length slice boundary

Layout::array checks size overflow before allocator entry. Slice metadata is
created from the validated element count and an aligned pointer; MaybeUninit<T>
has the same size/alignment as T. The unsafe assume_init API requires every
element to be initialized; partial writers must destroy their initialized prefix
before abandoning the reservation. Dropping uninitialized storage never drops T.
Copied initialization cannot panic or run user code. Dynamic drop computes the
original layout before dropping elements and frees storage with that layout.
Local tests cover overflow, flags, allocator failure, alignment, empty/ZST
metadata, address preservation and element destruction; not global OOM.

ZST layout validation cannot limit element counts through byte-size overflow.
Copy ZST filling explicitly skips element iteration, and Copy types have no
destructor. Tests cover an aligned Copy ZST with usize::MAX elements and verify
that flags are still validated. Initialized non-Copy ZSTs may have observable
destructors: those must run once per element, as a separate small-count test
checks. Callers must bound untrusted counts and per-element work; the allocator
does not promise bounded initialization or destruction latency. An arbitrary
global length cap would not bound the cost of user-defined destructors.

The reference comparison in design.md separates byte-layout safety from work
limits. Linux Vec drop explicitly destroys its initialized slice, including ZST
destructors; this must not be optimized away based on allocation size. Likewise,
generic Clone or initializer callbacks may have side effects even for ZSTs.
Only the current Copy-fill API can skip element writes without skipping user
code. No generic all-zero constructor is provided: zero bytes are not valid for
arbitrary T, unlike copying an already valid value.
