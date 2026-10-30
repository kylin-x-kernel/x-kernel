# TEE Session Lifecycle

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
interaction with resource cleanup and process exec/exit. TEE dispatch, storage
protocols, crypto algorithms and TA loading are outside this lifecycle scope.

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
