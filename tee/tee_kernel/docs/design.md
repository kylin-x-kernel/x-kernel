# TEE Ownership and Lifecycle

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
`tee_svc_storage.rs`. TEE dispatch, storage protocols, crypto algorithms and TA
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
