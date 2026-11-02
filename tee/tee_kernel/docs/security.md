# TEE Ownership and Lifecycle Safety

## Syscall buffer ownership boundary

TEE uses common KBox slice constructors and `UserConstPtr::load_vm_kbox_with_flags`
rather than a private buffer/access module. The pointer helper allocates storage
before copying through `osvm::read_vm_bytes`, backed by `kuaccess`. Its reservation
must have exactly the requested length. `TeeError` conversions own TEE error
mapping; unexpected errors map to GENERIC, not OUT_OF_MEMORY.
Only a complete successful copy permits conversion to initialized bytes; all
byte bit patterns are valid. Copy failure frees the reservation without exposing
partially initialized data. Layout overflow is BAD_PARAMETERS and owner OOM is
OUT_OF_MEMORY. Syscall-specific limits and null checks remain at the callers.

Crypto syscall staging uses KERNEL before state changes; object-locked attribute
export, secret creation, populate memrefs, and storage serialization/write staging
use NOWAIT to avoid reclaim under their serialization locks. NOWAIT does not
make user faults or filesystem calls nonblocking. Allocation policy applies only
to these owners, not nested Vec, Arc or crypto-library allocations.

Storage create stages data before pobj reference admission. Enumeration reserves
its temporary object before readdir. Owner OOM therefore cannot leak that manual
reference or consume an entry. Existing errors after backend effects are not
rolled back by this migration. Read preparation precedes backend reads and
position changes; export preparation precedes output length/value writes.

Common pointer tests inject reservation failure before user access. TEE tests
exercise the public copy path and error conversions, and inject failure in
secret construction, enumeration readdir and the shared crypto update callback;
the latter checks unchanged output bytes and length. Common pointer tests cover
empty buffers, layout overflow and mismatched reservations. TEE tests cover
byte and multi-byte user copies, user-copy failure, and existing crypto/storage
behavior. They do not force global OOM,
exhaust nested collections, or prove rollback after backend execution. Key
buffers retain their existing destruction policy, without guaranteed erasure.

## Property query owner boundary

Property metadata ownership uses fallible KBox construction before any user
output is written or session lock is acquired. Invalid indices are rejected
before allocation; owner OOM leaves output values and lengths unchanged.
This does not make CString name construction or user-memory access allocation
fallible end to end. Property tests cover dynamic dispatch for zero-sized and
nonzero-sized owners, invalid selectors, and short-buffer length reporting;
they do not inject global allocator failure.

## REE metadata reservation boundary

`new_fs_owner` reserves a typed uninitialized KBox with NOWAIT before invoking
resource initialization. Failed reservation returns TEE_ERROR_OUT_OF_MEMORY and
does not open a file, increment a directory reference or publish that owner.
REE open/create reserve before directory-cache admission, rather than adding
an owner failure inside the transaction's forced-cache-close rollback path.
Initializer failure frees the reservation and preserves the initializer's error;
existing resource-specific error paths remain responsible for explicit close.
Successful initialization has no remaining owner allocation that could fail
after opening resources. No raw ownership conversion or new unsafe code is used.

NOWAIT is required because callers may hold filesystem serialization and object
locks; it neither grants NMI safety nor makes the complete storage operation
nonblocking. Nested allocation and existing explicit-close behavior are outside
this boundary. A failed enumeration restart does not restore the old iterator.
Tests inject reservation failure locally, verify NOWAIT and that initialization
is skipped, preserve initializer errors and cleanup, and check successful pointer
identity. Existing REE primitive and storage syscall tests exercise actual IO/close.
The injection does not simulate global OOM, nested Box failure or disk failure.

## Session-owner allocation boundary

Session initialization uses a fallible KBox under the thread's session-slot
mutex. NOWAIT prevents owner allocation from sleeping or entering reclaim,
not from failing, and gives no NMI, reserve or bounded-latency guarantee.
The initialized-slot path does not allocate a replacement owner. In particular,
close-all helpers do not gain an avoidable owner-allocation failure when the
session already exists. Their internal resource operations are not made OOM-safe
by this change and retain their previous error contracts.

The slot remains absent if initialization fails, and the supplied resource
callback is not entered. A deterministic initializer failure test checks the
empty slot, successful retry, and pointer-preserving reuse without executing
the initializer for an existing context. This test does not exhaust the global
allocator. Existing exec/exit fixtures continue exercising real session cleanup.

## Scope and Trust Boundaries

This analysis covers session ownership and access in `src/tee/tee_session.rs`,
the close-all entry points in `tee_obj.rs`, `tee_svc_storage.rs` and
`tee_svc_cryp2.rs` under `src/tee/`, and their exec/exit handoff. It also covers
the REE metadata owner reservation and initialization boundary in `tee_ree_fs.rs`
and `fs_dirfile.rs` under `src/tee/`, and property-query owner preparation in
`tee_property.rs`, and buffer preparation in the crypto/storage service modules
and the `TeeError` conversions in `mod.rs`. Other storage-backend operations, nested
allocations, crypto algorithms, TA loading and syscall argument validation remain
outside this analysis, as do resource operations unrelated to these boundaries.

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
