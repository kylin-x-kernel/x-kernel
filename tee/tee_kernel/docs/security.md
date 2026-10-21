# TEE Session Lifecycle Safety

## Scope and Trust Boundaries

This analysis covers session ownership and access in `src/tee/tee_session.rs`,
the close-all entry points in `tee_obj.rs`, `tee_svc_storage.rs` and
`tee_svc_cryp2.rs` under `src/tee/`, and their exec/exit handoff.
It does not audit those modules' other resource operations, storage backends,
crypto algorithms, TA loading or syscall argument validation.

`kprocess::Thread` supplies the current-thread identity and session-slot mutex.
`posix/process` owns exit ordering; `core/ksyscall` owns the exec commitment
and success-reset boundary. Resource implementations own backend cleanup and
crypto-state destruction. Session lifecycle code relies on those contracts,
rather than independently validating user buffers or TA images.

## Ownership and Concurrency Invariants

- Session registries retain resources through `Arc`; removing an entry releases
  that ownership but does not prove all other references have disappeared.
- Session and resource access occurs under their respective sleepable mutexes.
  Callbacks must not recursively acquire the session mutex.
- Close-all routines snapshot IDs before calling operations that reacquire the
  session slot. Callers must not already hold that slot lock.
- Fatal exit must retain access to the old session until resource cleanup.
  Successful exec takes the slot and drops it after releasing the slot mutex.
- Lifecycle observations use Thread's noncreating accessor; the TEE convenience
  accessors can create a session and cannot establish that a slot stayed absent.

The session access/release and unittest fixture code in `tee_session.rs`
contains no explicit unsafe operations. This statement does not extend to the
resource implementations or other TEE modules.

## Failure Handling and Residual Risks

Resetting the session before fatal cleanup would lose access to its resource
registries. Keeping reset at the successful exec tail preserves the exit path.
Holding the slot mutex across close-all would deadlock on reentry; collecting
IDs first avoids that recursion.

Close-all routines log individual resource cleanup errors and continue.
Session-access errors propagate through `TeeResult`; a successful close-all
return alone does not prove that every backend operation succeeded. Backend
cleanup guarantees therefore require separate analysis.

Successful exec releases session ownership without actively clearing every
externally retained resource. Neither slot removal nor allocation reclamation
establishes key zeroization. Persistent-file closure, related-key cleanup,
HUK support and TA signature enforcement are outside this analysis.

## Audit Checklist

- Keep fatal cleanup and success-only session removal distinct.
- Do not hold the session slot mutex while invoking reentrant cleanup or
  destroying the removed session owner.
- Check cleanup errors as well as registry removal; do not infer backend success
  from an empty slot or a top-level successful return.
- Use noncreating observations, and account for retained resource references
  when checking cleanup and final reclamation.

The `ExecSessionFixture` regressions in
`core/ksyscall/ut-tests/task/tests/exec_lifecycle.rs` verify these lifecycle
distinctions with transient objects, enumerators and a keyless crypto operation.
They do not validate persistent backends or secret erasure.
