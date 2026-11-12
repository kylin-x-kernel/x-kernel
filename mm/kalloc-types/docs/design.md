# kalloc-types Design

## Minimal Shared Ownership

`KArc<T>` and `KWeak<T>` adapt the sized shared-owner surface from PR #385,
head `e3042051e558cf65d8533dfb2edab2a2e3152648`. This migration deliberately
retains sharing, downgrade/upgrade, identity/count queries and try_unwrap,
but excludes cyclic construction, DST coercions, mutable
uniqueness and whole-kernel Arc replacement. The original manual reference
counter is not copied: its Sync bounds omitted Send, its final-value Drop did
not guard the implicit weak release against unwinding, and overflow used a
post-increment panic rather than preventing a wrapping count.

The private representation is standard `Arc<T, SharedAllocator>` and its Weak
counterpart, using the pinned Rust toolchain's allocator_api implementation.
Rust supplies strong/weak synchronization, overflow protection and cleanup
when the payload unwinds. Auto traits inherit the Send + Sync payload
requirements; no unsafe Send/Sync implementation is added by this wrapper.
The allocator's permission field is carried by handles but is not a release
token: raw recovery uses equivalent NOWAIT allocator metadata. Unlike PR #385's
single-pointer representation, this wrapper must not be assumed pointer-sized
or layout-compatible with standard Arc<T>. No representation/ABI guarantee is
exported; broad call-site migration needs a separate footprint review.

`new`, `new_nowait` and `new_with_flags` return KResult and use the same
layout allocator as KBox. Unlike `Arc::try_new` with Global, the custom
allocator calls kalloc's fallible inherent allocation method rather than the
fatal GlobalAlloc adapter. Unknown flags fail before allocation, even for ZST
payloads. Every shared owner needs a count header, so ZST construction can
return NoMemory. Hosted builds retain paired nullable host allocation/free.

The allocator is private and copied with immutable flags; Arc owns its exact
layout and transfers the live allocation once at final weak release. Cloning,
downgrading, upgrading and empty weak construction do not allocate. Dropping
the last strong owner destroys T in the caller's context; weak owners retain
only storage. Final weak drop deallocates without reclaim. Neither operation
inherits the constructor's sleeping permission, and neither claims NMI safety
or bounded latency. Callers must audit T::drop and allocator lock order.

Guest tests cover ownership/counts, weak lifetime, extraction, alignment, ZST
allocation, invalid flags and local allocation-failure injection. Hosted tests
also cover concurrent upgrade/final release and payload unwinding. Hosted
compile-fail doctests reject Sync wrappers around a Sync-but-not-Send payload.
These tests do not simulate global OOM or prove timer admission/progress; timer
and signal protocol work remains separate.

### Shared Raw Ownership

`KArc::as_ptr` borrows an address without adding ownership. `into_raw`
transfers exactly one strong reference, and unsafe `from_raw` recovers that
reference without changing its count, allocating or sleeping. Multiple
transfers may have identical addresses but each retains a separate reference.
Pointer copies alone do not create ownership. The reference must be recovered
once, with the same type and provenance; it cannot be mixed with standard Arc,
Weak or KBox pointers. The implementation delegates pointer extraction to Arc,
not a pointer derived through Deref, preserving raw allocation provenance.

SharedAllocator flags and its test failure switch only affect allocation.
All instances share the same build-time kernel/host backend and layout-based
deallocator, so raw recovery can reconstruct an equivalent release allocator.
No caller-visible policy or allocator state is recoverable from the pointer.
Introducing arenas, per-allocation release tokens or stateful backends requires
revisiting this representation before retaining the pointer-only interface.

The optional `intrusive-collections` feature exports `KArcPointerOps<T>` for
concrete adapters using the existing library. This zero-sized implementation
transfers exactly one KArc strong reference per occupied link, with no allocation
or custom queue protocol. The default configuration does not enable this optional
dependency.
Hosted tests use this production pointer adapter with test-only link projections.
They cover duplicate-insertion panic recovery, known-member removal, independent
linkage fields, normal list teardown and the library's own take operation for
moving owners without destruction. Link selection remains subsystem-specific; these
tests do not establish a production timer queue's locking or cancellation
protocol. Production integration must encapsulate those obligations separately.
See `tests/README.md` for default and opt-in host/Miri commands.

## Raw ownership and hosted validation

`KBox::into_raw` transfers the sole owner without touching its pointee;
`unsafe KBox::from_raw` recovers that owner with the original type, provenance
and DST metadata. `KBox::leak` instead consumes the owner and yields an
unbounded borrow, retaining both value and storage without running `T::drop`;
it is reserved for deliberately permanent data such as `'static` boot state or
test fixtures. None of these operations allocate or sleep. Only pointers
originating from KBox are accepted; this is not interoperability with standard
Box or foreign allocators. RCU integrations retain raw ownership until old
readers finish.

Bare-metal builds continue using the fallible kalloc backend directly.
Hosted builds use paired nullable host allocation and deallocation for host/Miri
ownership tests. They do not model reclaim, IRQ context or kernel OOM behavior.


## Migration Batch Boundary

Owner migration must preserve both failure cleanup and subsystem progress.
An allocation that was previously infallible cannot be replaced by a fallible
call until callers have an error, rollback or preallocated-resource protocol.
NOWAIT does not guarantee success; KERNEL grants sleeping reclaim permissions
but does not implement a progress guarantee. Adding expect, discarding accepted
work, or retrying indefinitely is not a substitute for that protocol.

The current runtime/callback migration batch can close with the following
explicitly deferred production paths. They remain standard Box owners, not
completed migrations or permanent exemptions:

| Deferred path | Required separate design and validation |
| --- | --- |
| `ktask::TaskEntry::Closure` and the legacy kernel task factory | Audit all spawn callers, boot/IRQ-disabled contexts and publication boundaries. For dynamic workqueue workers, define Creating-slot rollback, baseline startup failure, retry wakeups and continued execution under memory pressure. Review whether reclaim-facing services need preallocated execution resources. Validate sustained allocation failure, pending demand, concurrent stop and partial startup. |
| `ksignal::PendingSignals::info_std` and `put_signal` | Define allocation failure separately from standard-signal coalescing. Keep pending bits and payload publication consistent under the signal lock. Preserve fatal-signal behavior and POSIX timer dequeue identity/acknowledgement; do not silently discard metadata. Review preallocation or bounded storage and validate exhaustion, coalescing, dequeue and timer lifetime races. |

These paths require separate design review, implementation and validation rather
than expanding the ownership-replacement patch into an unreviewed subsystem
redesign. Prepared user entries and workqueue manager entries already have
explicit failure boundaries; they do not make legacy kernel spawning, dynamic
worker growth or signal delivery fallible end to end. Other subsystems and
test-only Box uses remain subsequent migration work. The system-Box lint stays
deferred until the remaining migration and any reviewed exceptions are resolved.

## Moving A Sized Value Out

`KBox::into_inner(owner)` consumes a sized owner and returns its value without
cloning or running the value's destructor. A ManuallyDrop owner prevents the
ordinary KBox destructor from touching the moved value. The existing Storage
guard releases the original allocation with `Layout::new::<T>()` after the
pointer read; aligned zero-sized values require no storage release.

Extraction does not allocate, sleep or enter reclaim, but still requires the
allocator's deallocation lock order and reentrancy constraints. It does not
transfer allocation flags to the returned value's use or destruction. This
operation neither supports unsized extraction nor changes the existing callback
forwarding bound: a sized consuming-only FnOnce can be extracted and invoked,
but `KBox<dyn FnOnce()>` is not callable through this API. This supplies the
move-out primitive needed for pending payloads and consuming entry ownership;
it does not resolve their allocation failure or publication contracts.

Guest tests cover non-Copy destructor ownership, mutated aligned values with
owned arrays, aligned ZSTs, and a closure that consumes its captured resource.

## Fallible work callbacks

`KBox` forwards reusable `Fn` and `FnMut` callbacks, including erased trait
objects and borrowed captures, using `fn_traits`, `unboxed_closures`, and
`tuple_trait`. Arguments and return values are forwarded without allocation or
new unsafe code. The required `FnOnce` implementation invokes `FnMut` through a
mutable borrow and then drops the owner; consuming-only `FnOnce` closures are
not supported. Allocation flags do not constrain callback execution or Drop.

## Fallible directory iterators

KBox<T: Iterator + ?Sized> delegates next, size_hint and nth to T. This supports
owned chain/enumerate/skip adapters even after erasure to a borrowed-lifetime
trait object. Forwarding adds no allocation and does not introduce unsafe code.

## Scope and Interface

The initial API provides `KBox` for exclusive ownership of a single object,
fallible construction, `Deref`, `DerefMut`, and automatic destruction.
`KBox<T: ?Sized>` allows sized owners to unsize into trait objects or slices
without allocating, preserving pointer metadata and borrowed lifetimes.
Runtime-length slices support uninitialized reservation and Copy-value filling.
`from_slice` and `from_slice_with_flags` copy an existing kernel slice into
fallible ownership, without intermediate Vec allocation or user-memory access.
The default uses KERNEL; explicit flags retain the existing allocation context
contract. Copy ZST slices require no per-element work.
String construction, cloning, growable containers, and
pinning are not provided.
Callers do not use the standard `Box` through this API.

The construction interfaces uniformly return `KResult<KBox<T>>` and do not use a `try_` prefix:

| Interface | Policy |
| --- | --- |
| `KBox::new(value)` | Defaults to `AllocFlags::KERNEL`; the caller allows sleeping, reclaim IO and filesystem callbacks |
| `KBox::new_nowait(value)` | `AllocFlags::NOWAIT`; the allocation path is not allowed to sleep |
| `KBox::new_with_flags(value, flags)` | The caller chooses a common combination or composes permission bits |

Insufficient memory returns `KError::NoMemory`; unknown flag bits return `KError::InvalidInput`,
including for zero-sized values. On any failure the input value is destroyed. Nested allocations during
value construction and the blocking behavior of the input value's own
destructor are not covered by the allocation-policy guarantees of this API.

## Allocation Context

`AllocFlags` borrows Linux GFP's "permission bits + common combinations" model; it is not a Linux bit-value ABI.
`DIRECT_RECLAIM` allows sleepable synchronous reclaim, `IO` allows reclaim to initiate IO, and `FS` allows
reclaim to enter the filesystem. Without `DIRECT_RECLAIM`, sleeping reclaim must not be performed
through the IO/FS bits even if they are present; when this bit is set, the caller must be allowed to sleep.

`KERNEL` contains all three permissions and is also the `Default`; `NOWAIT` grants no permissions;
`NOIO` allows only synchronous reclaim, and `NOFS` allows synchronous reclaim and IO. Policies can be built with bit operations,
for example `AllocFlags::KERNEL & !AllocFlags::FS`. These are permissions rather than behavior that must be performed,
so the current backend without reclaim can still satisfy the restrictions. Reserve pools, must-succeed allocation, DMA or
arbitrary allocator injection are not added; these features cannot be faked by setting permission bits.

The default constructor may only be called from a sleepable task context; it must not be called while holding a non-sleeping lock or in an interrupt.
This constraint holds now and must not rely on the current backend happening not to sleep. `new_nowait` does not promise
reserve access or bounded latency. The policy applies only to a single allocation and does not change the task-wide context.
Linux recommends handling NOFS/NOIO constraints across call chains through scoped APIs; this change does not implement such
scope inheritance, so callers cannot assume that nested allocations automatically inherit the restrictions.

All currently known permission combinations call `kalloc::global_allocator().alloc`, and the existing backend
performs no sleeping reclaim, reclaim IO or filesystem callbacks. In the future, the internal `allocate` must pass
the permissions to the reclaim backend, preserving the NOWAIT, NOIO and NOFS restrictions.
It must not be rerouted to the current `GlobalAlloc` entry point, which calls the fatal handler on OOM.
Panics caused by the backend's own internal invariants are not converted into ordinary allocation failures.
Direct allocation still participates in `kalloc`'s RustHeap usage accounting, but does not go through the optional backtrace tracking of the `GlobalAlloc`
wrapper layer; when the tracking entry point is unified in the future, the failure semantics must be preserved.

## Lifecycle

Non-zero-sized values are allocated with `Layout::new::<T>()`, and initialization is completed with a single write after success.
A ZST uses an aligned dangling pointer and does not call the allocation or deallocation interfaces, but its destructor still runs normally.
`KBox` stores only `NonNull<T>` and an ownership marker, with a transparent
representation and a derived `CoercePointee` implementation. Following Linux
KBox's DST ownership model, it does not store a layout in every owner.
Before destroying the value, it computes `Layout::for_value` from the still-valid
value and creates a private stack-local `Storage` guard. The guard frees storage
with the saved layout and never creates a reference after value destruction.
The guard also frees storage if destruction unwinds. The kernel normally uses
panic abort, so this is not a kernel recovery guarantee. Unsizing neither
allocates nor changes the allocation-policy contract.
Moving ownership does not change the heap address, but `DerefMut` allows replacing the value, so pinning is not provided.
`Send` and `Sync` require `T: Send` and `T: Sync`, respectively, including for DSTs.

## Call-Site Migration Constraints

`Box::new` must not be bulk-replaced with the default `KBox::new`. Every migration point must
check the following conditions along the call chain, and state the execution context and error behavior in the rustdoc of the interface it belongs to:

1. Determine whether the path runs in sleepable task context, or in interrupt context, with IRQs/preemption disabled, in a non-sleepable RCU read-side section, or
   while holding a spinlock. The default `new` is only for the former; neither a ZST nor the fact that nothing is currently reclaimed is an exception.
2. Check the dependencies between all locks currently held and potential reclaim paths. A sleepable mutex does not automatically
   mean the default allocation can be called; reclaim may re-enter the same subsystem and wait on that lock. Depending on the actual
   dependencies, choose NOFS/NOIO, move the allocation, or use NOWAIT.
3. NOFS/NOIO still allow sleeping; if sleeping is not allowed, DIRECT_RECLAIM must be removed.
   NOWAIT only constrains storage allocation; it does not promise NMI safety, reentrancy in arbitrary lock contexts,
   reserves, or guaranteed success. The lock ordering and reentrancy conditions of the underlying allocator must still be checked.
4. Check the construction of the input value, its destruction on failure, and the context in which it is finally freed after success.
   flags do not constrain this code, and are not retained to affect a future Drop; Deref itself does not
   allocate or sleep, but methods of T called through the reference have their own constraints.
5. Confirm that the error can be returned or there is a clear fallback, that no half-built object is published before failure, and that resources and existing state are
   cleaned up correctly. A fallible allocation must not be turned back into a fatal failure by adding unwrap/expect.

The default policy is not switched implicitly based on whether a lock is currently held: the policy is a stable contract of the calling interface.
If the same helper function must serve multiple contexts, the upper layer should pass in flags, or the helper should be explicitly split into
normal/non-sleeping entry points; the lower layer must not unconditionally use the default `new`.

## First Caller and Verification

The file-lock context of a VFS inode uses `Once<KBox<FileLockContext>>`.
`try_call_once` remains uninitialized after a failure, so the next call can retry; after success,
all requests reuse the same context. Because contenders of Once spin, initialization uses `new_nowait`.
`lock`/`flock` return the failure as `NoMemory`; queries and unlocks without a context need no allocation.
The lock-record nodes inside the context still use the existing implementation and are not part of this migration.

The guest tests cover actual allocation, alignment, moves, mutable access, destruction, ZSTs, and local failure injection.
Failure injection only returns an error from the private allocation callback and introduces no global fault switch; the VFS tests verify that
a failure does not publish the context, that a retry succeeds, and that no allocation is performed again once initialized.
The policy tests cover default construction, the passing of common and custom flags to the private allocation callback, and
that when unknown bits are rejected, no allocation is performed and the input is destroyed correctly.

The targeted command is `make unittest UNITTEST_CRATE=kalloc_types,kvfs`;
the filter uses the Rust crate name `kalloc_types`, not the hyphenated Cargo package name.

## Second batch: mount-transaction private state

Both private-state slots in `FsContext` use `KBox<dyn Any + Send + Sync>`.
After parsing initialization options, tmpfs/ext4 allocate owners with the default
KERNEL policy before mount publication or superblock reconfiguration locks.
The interfaces require sleepable task context with no locks needed by reclaim.
The process fs_context snapshot lock is released before transaction construction.
`set_private` and `set_super_private` return `VfsResult<()>` and allocate before
replacing the slot, preserving the old state on failure. Callers propagate errors
without introducing a fatal OOM path for owner allocation. Persistent superblock
private state and other Box uses remain for subsequent migration batches.

New tests cover borrowed trait objects, slices and empty slices, dynamic
destruction of aligned ZSTs, owner layout, and positive Send/Sync bounds.
VFS tests cover private-state replacement and cleanup after initializer failure.
Existing local allocation-failure injection still verifies that the input value
is destroyed exactly once.

## References

- [Linux memory allocation guide](https://docs.kernel.org/core-api/memory-allocation.html): GFP permissions, common combinations, and recommendations for NOFS/NOIO scopes.
- [Linux Rust Box](https://rust.docs.kernel.org/kernel/alloc/kbox/struct.Box.html): `new(value, flags)` is a fallible constructor; this module additionally provides a default entry point without flags and a NOWAIT shortcut entry point.

## Reserved owner storage

`AsRef<T>` and `AsMut<T>` expose the same lifetime-bounded pointee borrows as
Deref/DerefMut, including for DSTs. Debug delegates directly to T's formatter,
without adding allocation; the formatter retains its own context requirements.

`KBox::new_uninit` reserves a sized `MaybeUninit<T>` owner with KERNEL
permissions before a caller performs an irreversible operation.
`new_uninit_with_flags` provides explicit permissions, including NOWAIT;
reservation does not change the context requirements of later initialization.
`KBox::write` consumes the reservation, writes a complete value and returns
`KBox<T>` at the same address without allocating or failing. Abandoning a
reservation only frees storage, without running a destructor for an unwritten T.
This is not field-by-field initialization, pinning, or a runtime-sized allocator.

## Runtime-length slice storage

KBox<[T]> reserves Layout::array::<T>(len) directly, without a temporary Vec.
Unknown flags and overflowing layouts return InvalidInput before allocation;
backend failure returns NoMemory. Empty and ZST slices retain length and aligned
non-null pointers without allocating. new_uninit_slice uses KERNEL; its explicit
flags variant has the same context requirements as sized allocation.
new_slice fills Copy values without intermediate ownership or a fallible loop.
For Copy ZSTs it skips the fill loop entirely; the supplied valid value proves
inhabitation and there are no bytes to write or element destructors to run.
Unsafe assume_init transfers the original length and pointer and requires every
element to be valid. No generic fallible element initializer is exposed.

### Reference implementations and deliberate differences

This comparison uses the local Linux v7.2 tree at
`8d3ae59288f1e7d58d76558a6ee96d533bc5019f` and Rust 1.95.0 library sources
from rustc commit `59807616e1fa2540724bfbac14d7976d7e4a3860`.

- Rust `library/alloc/src/boxed.rs`, `try_new_uninit_slice_in`, uses an aligned
  dangling pointer for empty/ZST slices and checks array layout for nonzero
  storage. Its slice `assume_init` preserves the pointer and length. Our API
  follows those ownership rules, but validates allocation flags even when no
  allocation is needed and reports layout/flag errors as InvalidInput.
- Linux `rust/kernel/alloc/kvec.rs`, `Vec::capacity`, uses usize::MAX for ZST
  capacity. Neither this nor Rust's ZST reservation imposes an arbitrary element
  limit. Byte-layout validity is not a work-budget guarantee.
- Rust `library/alloc/src/vec/spec_from_elem.rs` specializes repeated unit values
  by setting the length without cloning elements. Generic repetition must
  preserve observable Clone behavior. Our new_slice explicitly copies T; it
  never calls Clone, so skipping writes for every inhabited Copy ZST is valid.
  This optimization must not be extended to a future Clone-based initializer.
- Linux `rust/kernel/alloc/kbox.rs`, `Box::pin_slice`, initializes elements one
  at a time and records the initialized prefix in a Vec for failure cleanup.
  A future fallible initializer here would need equivalent prefix ownership;
  the current unsafe assume_init API is not a substitute for that guard.
- Linux Box drop computes the dynamic layout and explicitly destroys its value
  before freeing storage. Our Storage guard additionally preserves freeing on
  unwind. Rust's standard Box has compiler-assisted pointee destruction before
  its handwritten Drop body; copying that body would omit destruction here.
