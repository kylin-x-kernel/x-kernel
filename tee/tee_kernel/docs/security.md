# TEE Ownership and Lifecycle Safety

## REE Scratch And Backing Handle Boundary

Scratch allocation is fallible, zero-initialized and fixed at BLOCK_SIZE bytes.
NOWAIT avoids sleeping reclaim under REE serialization locks; it does not make
RPC, VFS IO or the complete storage transaction nonblocking. Failure returns
through the existing TeeError conversion before block IO or payload copying.
Read length clamping still occurs before reservation. RAII frees scratch storage
on every exit without an explicit put operation.

The inline FileVariant remains an integer handle with no Drop implementation.
Copying it into TeeFsFdAux neither duplicates ownership nor closes the underlying
table entry; the existing explicit close protocol remains responsible for release.
No address of the inline handle is retained. Tests check scratch zeroing, size
and independent lifetime, plus handle identity across a move. These tests do not
inject global OOM or establish htree transaction rollback guarantees.

## CSV Temporary Lifetime

Attestation user data is borrowed only while copying into the separately owned
report buffer. Removing its Box and the Box around the report Vec descriptor
does not change the hypercall buffer address or lifetime. The existing Vec
allocation remains infallible and does not guarantee page alignment. Hardware
buffer suitability and hypercall response handling remain separate concerns;
ordinary QEMU validation cannot establish Hygon CSV hardware behavior.

## ECC dispatch lifetime

The key-generation dispatch wrapper contains only an exclusive key borrow and
type metadata. Stack-local dispatch keeps that borrow within the call and
removes the temporary heap owner without changing key storage or zeroization.
Tests exercise all five supported object selectors, size rejection and an
unsupported selector. Nested algorithm allocations remain outside this change;
removing the wrapper does not make complete key generation allocation-free.

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
the REE metadata owner reservation, scratch-block and inline backing-handle
boundaries in `tee_ree_fs.rs`
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

## Control-Plane Wire Mirror Safety

`src/tee/control_protocol.rs` parses bytes that cross a trust boundary: the
control-plane socket carries data originating outside the kernel, so every
input must be size-bounded before allocation and fully validated before use.

- `control_frame_payload_len` rejects a length prefix above 65,536 before any
  buffer is allocated, so a hostile peer cannot request an oversized
  allocation through the prefix alone; the same cap bounds `encode` output.
- `decode_control_frame` requires the prefix to match the remaining buffer
  length exactly, and deserialization failures are returned as typed errors
  (`Oversize`, `Truncated`, `Deserialize`) rather than panics.
- The module contains no `unsafe` code; the future client (ticket 06) is
  responsible for mapping these errors to `TEE_ERROR_COMMUNICATION` with
  `TEE_ORIGIN_COMMS` per plan 004 §6.7.
- Version mismatches and request/response variant mismatches are detected by
  the client, which owns the one-request-per-connection rule and the 5-second
  budget; the mirror intentionally stays decision-free.

The golden tests only prove encoding stability between the two mirrors, not
end-to-end security; socket-level behavior belongs to the client below and the
manager side.

## Control-Plane Client Safety

`src/tee/tee_ta_manager.rs` and `src/tee/tee_inter_ta.rs` operate on the same
trust boundary from the kernel side: the manager socket peer is outside the
kernel, so every response byte is untrusted input and every request carries
kernel-copied user data toward that peer.

- The client bounds its own exposure before any allocation: the response
  length prefix is validated against the 65,536-byte payload cap by the mirror
  helper, partial reads are looped to the exact frame length, and an
  early-close peer yields `Eof` rather than a short-buffer misparse.
- The response version and the request/response variant pairing are checked
  before any syscall consumes the payload; a mismatched, oversized, or
  malformed response degrades to `TEE_ERROR_COMMUNICATION` +
  `TEE_ORIGIN_COMMS` and never reaches user memory.
- Response parameters are validated as a whole before any user write: type
  mismatches are `BAD_FORMAT + COMMS`, the 60 KiB response-data budget is
  enforced, kernel-detected capacity overflow copies no data, and a
  TA-initiated SHORT_BUFFER length must lie between the response data length
  and the budget. A bad response therefore cannot turn into a partial
  write-back.
- User memory is only touched through the checked `osvm` accessors;
  `b == 0` memrefs skip the user address entirely, and writes of output data
  precede the `utee_params` commit so a failed user write leaves the caller's
  lengths untouched.
- The kernel never trusts manager session ids: they live behind
  process-local handles, session id 0 in an OpenSession success response is
  rejected as a communication error, and handle allocation failure triggers a
  CloseSession rollback so no orphaned manager session survives a failed
  syscall.
- Session invalidation uses both result and origin: `ITEM_NOT_FOUND/TEE`
  denotes a missing manager session; `ITEM_NOT_FOUND/TRUSTED_APP` is an
  ordinary TA command error and must not destroy the local handle. Close
  communication failures invalidate the handle whether they arrive as a
  decoded operation response or a transport failure, preventing replay of
  an operation already claimed by manager cleanup.
- The client sets both socket timeouts before `connect`, keeps no locks
  across socket I/O (the session table lock covers table mutations only),
  never replays a request, and caps every exchange at 5 seconds (1 second
  for `ReleaseOwner`). EINTR continues only the interrupted read/write at its
  existing byte offset; every attempt consumes the same absolute deadline.
  Repeated interruptions therefore cannot extend the exchange, and terminal
  errors such as BrokenPipe still fail without retrying. A hostile or wedged
  manager cannot hold a syscall thread indefinitely.
- The fixed socket path is the only production target; the test seam takes
  its path as a parameter but production call sites always pass
  `CONTROL_SOCKET_PATH`.

The manager remains responsible for peer authentication on its side
(SO_PEERCRED-style checks live in the manager implementation); the kernel
treats the manager as the trusted policy authority for session routing and
never forwards raw manager-controlled bytes into user memory without the
validation above.

The manager obtains a positive peer PID from `SO_PEERCRED` and stores that
numeric PID as the control-plane session owner. Unix stream connect snapshots
the current process identity and effective UID/GID; readers resolve that
identity in their PID namespace. Retiring the current TID does not remove its
process runtime or credential snapshot, so POSIX can run the named `exit_tee`
stage after `finish_thread_exit` without consulting the global TID directory.
The stage runs before runtime/fs/namespace detach and before parent-visible
completion, while later thread publication is blocked by the last-thread
reservation. The socket path and root trust model still do not distinguish two
root processes that use the same numeric PID at different times.

### Residual sessions after a failed ReleaseOwner

`tee_ta_release_sessions` is best-effort by design (plan 004 §3/§9.3): when
the manager is unreachable or the 1-second budget expires during process
exit, the kernel clears its own table but manager-side `SessionEntry`
records for the dying PID until a manager-side cleanup path or restart removes
them. The protocol has no PID generation or process-liveness token. If the PID
is reused first, a new root process can present the same `SO_PEERCRED` PID and
match the stale `owner_pid`; the manager's numeric owner check cannot distinguish
it from the original process. This is a residual access risk as well as a
resource leak, accepted by the design's best-effort exit cleanup. The 1-second
absolute deadline, no-request-replay policy and unconditional local table clear keep
kernel exit bounded, but do not guarantee manager-side reclamation. The
x-kernel's process-local table still prevents stale handles from surviving in
the kernel after exit.

### Cooperative Invoke cancellation scope

Syscall 572 accepts scalar operation/deadline values and updates only the current
thread's cancellation state. It cannot address another thread or session, and it
preserves the cancellation mask and object ownership. A TA may install or clear
its own deadline; cancellation is cooperative behavior rather than an authority
boundary. The manager still authenticates the caller and checks session ownership
before forwarding the timed request. Deadline expiry cannot authorize a request,
release its in-flight permit early or synthesize target success/CANCEL.

The absolute guest monotonic deadline includes queueing and avoids realtime
clock changes. The mask can hide an expired deadline until unmasked. Scope
cleanup prevents cancellation leaking to a later Invoke on the same worker.
`TEE_Wait` releases the context borrow before sleeping. CPU loops and unrelated
blocking calls remain uninterruptible by this mechanism; the control transport
budget bounds the caller's wait but does not prove the target has stopped.

### Uncertain Close and owner cleanup

A Close transport/protocol failure invalidates the local handle immediately, but
cannot prove that the manager claimed cleanup. The process context retains an
owner release obligation independent of live handles. Process exit sends one
bounded ReleaseOwner even if the failed Close removed its last handle, then clears
both the session table and this obligation. Decoded Close responses retain the
existing handle invalidation rules and do not create a retryable handle.

If Open publishes a handle successfully but writing returnOrigin subsequently
fails, the local entry remains owned by that process. The caller may already
have received the handle; the kernel does not silently replay or roll back that
successful Open. Exit ReleaseOwner remains its best-effort cleanup boundary,
with the existing unavailable-manager and PID-reuse residual risks.
