# TEE Ownership and Lifecycle

## Stack-local ECC dispatch

ECC key generation dispatch constructs its borrowed `EccKeypairOpsCtx` on the
stack and immediately invokes the selected implementation. No context escapes
the call, so neither Box nor KBox ownership is needed. This removes an allocation
without adding a reclaim point under the caller's object lock. Algorithm
selection, error propagation and nested crypto-library allocations are unchanged.

## Fallible syscall and crypto buffers

`tee_svc_storage`, `tee_svc_cryp` and `tee_svc_cryp2` use KBox byte slices
for syscall staging, attribute memrefs, exported attributes and secret storage.
There is no TEE buffer or user-access module. Kernel slices use the common
`KBox::new_slice_with_flags` and `KBox::from_slice_with_flags` APIs directly.
User payloads use `UserConstPtr::load_vm_kbox_with_flags` in `posix-types`, backed
by the common `osvm`/`kuaccess` copy path. This public helper reserves storage
before fault-checked user copying and assumes
initialization only after the complete copy succeeds. Partial copies drop the
uninitialized reservation. Empty buffers never read user memory. Layout overflow
maps to BAD_PARAMETERS; owner allocation failure maps to OUT_OF_MEMORY.
`TeeError` next to `TeeResult` implements `From<KError>` and `From<MemError>`;
conversion back to `u32` preserves existing entry-point signatures. Unknown
errors become GENERIC rather than being misclassified as allocation failure.
Existing optional/required null-pointer conventions remain with their callers.
Output staging still reads existing user bytes rather than substituting zeros.

| Preparation boundary | Policy | Reason |
| --- | --- | --- |
| Crypto syscall input, output and OAEP label | KERNEL | Outside crypto-state locks and before state advancement |
| Storage object IDs and create input | KERNEL | Outside object/session locks; before pobj admission |
| Storage read buffer | KERNEL | After access validation, before reacquiring the object lock and backend read |
| Enumeration temporary TeeObj owner | KERNEL | Before acquiring the enumeration lock and consuming readdir |
| Storage write staging and attribute serialization | NOWAIT | Object or source-attribute lock is held |
| Attribute export, secret storage and populate memrefs | NOWAIT | Shared helpers can run under object/usage locks |

KERNEL requires IRQs and preemption enabled, sleepable reclaim-safe task context,
no non-sleepable RCU section, and no locks needed by reclaim, IO or filesystem
callbacks. NOWAIT prevents reclaim only for the owner allocation; existing user
access, storage IO and nested allocations retain their own context requirements.
It provides no reserve, bounded-latency or NMI guarantee.

Create input is prepared before obtaining a manually counted pobj reference,
so copy/owner failure cannot leak that reference. This moves input errors ahead
of pobj conflict errors. Enumeration reserves its temporary object before
readdir, so owner failure does not consume an entry; it can precede directory-state
validation. Later backend/user-copy errors retain the existing behavior.
Attribute serialization fails before backend create; existing create rollback
releases pobj ownership and restores the converted object's flags. Secret
construction finishes before attribute installation. The unused infallible
secret Clone implementation is removed.

`copy_in_attrs` returns its existing Vec directly instead of converting it to a
boxed slice; partial failure drops already prepared memrefs. The Vec itself,
crypto-library internals, CString names, persistent-object IDs and hash-tree
growth remain outside this owner migration. No new zeroization guarantee is made.

## Fallible property query owners

Property queries prepare a `KBox<dyn TeeProps>` before accessing session state
or writing user outputs. The syscall dispatch reaches this preparation without
holding a session or object lock. Default KERNEL allocation requires sleepable,
reclaim-safe task context with IRQs and preemption enabled, outside non-sleepable
RCU sections and without locks needed by reclaim or IO. Invalid sets and indices
retain ITEM_NOT_FOUND precedence over owner allocation. Owner failure returns
OUT_OF_MEMORY before publishing any output. Client and TA property owners are
zero-sized; implementation properties own static metadata, not session resources.
Property names still use the existing CString allocation, outside this guarantee.

## Fallible REE filesystem metadata owners

REE file handles (`TeeFsFd`), directory iterators (`TeeFsDir`) and cached
directory handles (`TeeFsDirfileDirh`) now use KBox. `new_fs_owner` reserves the
outer storage before invoking its initializer, and writes the completed value
into that reservation without another allocation. Public REE open/create reserve
before taking the cached directory reference or allocating a temporary file
number, so owner OOM never enters their cache-discard rollback. Their shared
primitive initializer returns a value without allocating another outer owner.
Direct primitive and dirfile reservations precede backing-file open/create;
iterator reservations precede taking a cached
directory reference. `open_dirh` returns the existing owner instead of moving
its contents into a second allocation. Dirfile initialization consumes a file
owner with `KBox::into_inner`, preserving the existing explicit close protocol.

These paths run in sleepable task context with the filesystem and credential
context required by the existing file helpers; IRQ handlers, IRQ/preemption-
disabled regions and non-sleeping locks must not enclose the storage operation.
They can hold REE_FS_MUTEX, REE_FS_STATE, an
object-id lock or an enumeration lock. Their owner reservation uses NOWAIT:
entering reclaim while holding these serialization locks is not established as
safe. NOWAIT applies only to that reservation, not to RPC, VFS IO, initialization,
nested allocations or close. NoMemory maps to TEE_ERROR_OUT_OF_MEMORY before
the initializer runs; existing initializer failures retain their cleanup paths.
Reopening an enumeration still closes its previous iterator first, so a failed
restart leaves it stopped rather than preserving the old iteration position.

This migration covers outer metadata ownership only. Hash-tree nodes, tree and
storage owners, FileVariant boxes, boxed buffers and their existing Default
constructions remain unchanged. In particular, incremental tree growth needs
its own failure-consistency audit before becoming fallible.

## Fallible session ownership

The thread slot owns `KBox<dyn TeeSessionCtxTrait>`. Session access ensures the
slot is initialized while holding its mutex; an existing owner is reused without
allocation. Only a missing slot constructs an empty `TeeSessionCtx` and allocates
its owner with NOWAIT. This avoids adding reclaim dependencies under the session
mutex and avoids introducing a new allocation failure into existing-session
access or exit cleanup. The default context has empty Slabs; subsequent resource
and Slab allocations retain their existing behavior.

Owner allocation failure returns `TEE_ERROR_OUT_OF_MEMORY` before installing a
context or invoking the resource callback; a later access may retry. Callback
execution still holds the session mutex and must not reenter it or acquire new
reclaim dependencies on it. The test-thread initialization helper now returns
the same error; entry-side unittest setup requires that fixture to succeed.

## Test Layout

Guest tests and the filesystem hash-tree fixture live in `ut-tests/`, mounted
with `unittest::path_mod_test!`. Architecture-specific HUK tests retain their
feature gates. File-path, protocol, payload-length and UUID cases previously
used host `#[test]` inside guest-only modules; they now use `#[def_test]` so
the guest runner registers them. Their assertions are unchanged.
The standalone HUK derivation and object-table cases are also file-backed;
their names gain a `tests` module component, with their guest execution modes
and assertions preserved.

## Scope and Ownership

This document covers session ownership in `src/tee/tee_session.rs` and its
interaction with resource cleanup and process exec/exit. It also covers REE
filesystem metadata owner reservation and initialization in `tee_ree_fs.rs` and
`fs_dirfile.rs` under `src/tee/`, including ownership held by `tee_obj.rs` and
`tee_svc_storage.rs`, as well as property-query owner preparation in
`tee_property.rs`, and byte-owner preparation in the three service modules and
the shared pointer/ownership APIs, with TEE error conversion in `mod.rs`.
TEE dispatch, storage protocols, crypto algorithms and TA
loading are outside this lifecycle scope.

`kprocess::Thread` owns an optional boxed `TeeSessionCtx` behind a sleepable
mutex. The context contains separate slab registries for objects, storage
enumerators and crypto operations. Registry entries own reference-counted,
mutex-protected resources. TEE resource APIs use the current thread's session
and may lazily provision it.

## Exec and Exit

`posix/process` calls `tee_session_release_state` during process exit before
thread resource detachment. An absent session is left absent. Otherwise cleanup
closes objects, storage enumerators and crypto operations, in that order.
`tee_cryp_state_free` replaces the crypto context with `CrypCtx::Others`
and removes its registry entry. The session slot itself can remain populated
after resource cleanup.

Successful exec has a separate policy. After process update and credential
commit, `ksyscall` calls `Thread::reset_tee_session_after_exec` to take the
session slot and drop its owner outside the slot mutex. This does not invoke
exit's close-all operations. Until that successful tail, the old session remains
available to fatal exit cleanup.

## Context and Synchronization

Session access and cleanup require sleepable current-user-thread context.
The session mutex protects the optional context and registry access; individual
resource mutexes protect resource contents. Session callbacks must not reenter
the same slot mutex.

Close-all operations collect resource IDs under the session mutex, then perform
per-resource cleanup after releasing it. Dropping session ownership does not
necessarily destroy a resource immediately: outstanding resource `Arc` values
can extend its lifetime. Active crypto cleanup changes the retained resource's
context, whereas successful exec only releases session ownership.

## Regression Coverage

The unittest-only `ExecSessionFixture` in `src/tee/tee_session.rs` supports
`core/ksyscall/ut-tests/task/tests/exec_lifecycle.rs`. It checks session retention
until fatal cleanup, success-only slot removal and eventual resource release.
These tests use transient resources and do not cover persistent storage,
signed-TA loading or execution of user TA instructions.
