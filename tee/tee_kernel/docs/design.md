# TEE Ownership and Lifecycle

## Fallible Htree Child Ownership

`fs_htree.rs` stores child nodes as `Option<KBox<HtreeNode>>`. Child attachment
consumes an already allocated owner and asserts that the destination is empty.
It does not allocate or replace an existing subtree. Disk formats, node numbering,
cryptographic algorithms and growable buffers are unchanged.

### Context and Failure Contract

REE callers may hold `REE_FS_MUTEX` and `REE_FS_STATE`. Sleeping reclaim under
these locks is not proven safe, so node reservation explicitly uses
`AllocFlags::NOWAIT`. Allocation errors use the existing `TeeError` conversion.
This remains a task-context storage operation: NOWAIT does not make RPC,
encryption, nested allocation or destruction IRQ/NMI safe.

A failed growth reservation leaves the original topology, existing node addresses
and contents, parent IDs, dirty/version flags and `max_node_id` unchanged.
Existing-node and no-create lookups do not reserve owners. Invalid IDs return
BAD_PARAMETERS before allocation; IDs must be nonzero, representable as u32,
and below usize::MAX for traversal. Block-to-node addition is checked.

### Growth Transaction

`HtreeGrowth` holds exclusive tree access from preparation through commit or
cancellation. It reserves all absent nodes in the same ascending candidate range
as the previous algorithm; it does not change growth to a target-only ancestor
path. Live parent/child links and metadata remain untouched during reservation.

`PreparedNodes` temporarily connects detached nodes through their right child
links, with empty left links. This is a private staging representation of the
existing tree owners, not an additional queue allocation or public container.
It cannot be accessed through live-tree traversal. Both cancellation and commit
take each staging link before releasing/transferring its owner, so abandoning
a large preparation does not recursively drop a linear chain.

Commit attaches nodes in ascending ID order, so required new parents precede
their children, and updates `max_node_id` after all attachments. It does not
allocate, perform RPC or return a recoverable error. Existing structural tree
invariants remain required. `find_closest_node` now reads path bits directly
rather than allocating a scratch Vec, making lookups in commit allocation-free.

### Write and Load Integration

Block write prepares growth before `rpc_write_init`. Write-init failure drops
the detached nodes. Success commits them before changing block version, IV,
tag or dirty state. Owner exhaustion returns OUT_OF_MEMORY, rather than the
previous blanket CORRUPT_OBJECT mapping, and issues no write-init/write-final
call for that block. Other preparation errors retain their corruption mapping;
invalid block-to-node conversion returns BAD_PARAMETERS directly.

Loading may read a node image before preparing its owner. Failure propagates
through tree open; the unpublished tree owns and releases earlier loaded nodes.
The existing REE open/create cleanup still explicitly closes the backing fd and
may remove a created file; its cleanup-error precedence is unchanged.

These guarantees cover one growth transaction, not an entire multi-block write.
Earlier blocks and directory-cache activity may already have changed. Crypto,
IO and sync failures after commit keep their existing behavior. Large valid
growth requests and existing nested allocations remain resource-limit concerns;
this change does not establish global OOM safety or bounded total growth time.

### Test Surface

The private reservation seam substitutes only allocation; production and tests
use the same growth and write/load ordering. Tests in
`ut-tests/tee/fs_htree/tests_get_node.rs` cover each owner failure in a multi-node
growth, NOWAIT selection, unchanged state and existing addresses, retry,
existing/no-create lookup, cancellation of a large staged chain, truncation and
regrowth, sparse-tree success, prepared-address reuse, ID bounds, write RPC
ordering and partial-load error propagation/owner cleanup. These are targeted
owner-failure tests, not global allocator or encrypted-backend fault injection.
Existing authentication, persistence and REE regression tests remain applicable.

## Persistent Object ID Ownership

`tee_pobj.rs` stores persistent IDs in `KBox<[u8]>`. New registry entries
prepare the ID after reuse/access checks and before publication. Preparation
uses `NOWAIT` under `POBJS_MUTEX`: reclaim reentry into metadata operations is
not safe there. Allocation errors use `TeeError` and return
`TEE_ERROR_OUT_OF_MEMORY` without publishing an entry. Reusing an existing
entry does not allocate ID storage. The registry's `Arc` and `VecDeque`
allocations remain outside this migration.

Storage rename transfers the existing user-copy owner into the source pobj
after backend success. That owner was prepared with `KERNEL` in task context
before object/registry locks and before backend mutation; the destination
reservation has its own ID owner. The metadata update neither copies nor
allocates ID storage. Backend errors drop the unused owner and retain the
source ID. The existing reference-count check can still fail after backend
commit; this change does not solve rename/enumeration concurrency or provide
a transactional backend/registry protocol.

## REE Temporary Blocks And Inline File Handles

REE read/write scratch blocks use a zeroed KBox byte slice of exactly BLOCK_SIZE.
The temporary Vec-to-Box conversion is removed; this is final owned block storage,
not migration of a growable Vec. Reservation uses NOWAIT because callers may hold
REE_FS_MUTEX or other storage serialization locks, where sleeping reclaim has not
been established safe. Shared TeeError conversions preserve OUT_OF_MEMORY, and
the local owner releases storage on both early errors and normal return.

Write scratch reservation precedes block IO and metadata mutation. Read reservation
precedes block IO and payload copying, but follows the existing requested-length
clamping; failure does not promise to restore that length argument. Nested htree
allocation and write/rename transaction rollback are unchanged.

TeeFsFd stores FileVariant inline: it is a copyable integer fd, and no pointer to
this field escapes. The auxiliary storage object copies the handle value. Moving
TeeFsFd therefore needs no separate fd allocation and preserves explicit close
semantics. The htree and its auxiliary storage use the fallible owners described below.

## CSV Temporary Ownership

The x86_64 Hygon CSV path constructs its 112-byte attestation user-data value
on the stack and copies it into the report buffer. No reference to that temporary
escapes. The report buffer remains a Vec; only the redundant Box around its Vec
descriptor is removed. Moving the descriptor does not move the backing bytes
passed to the hypercall. This change neither migrates Vec nor adds a reclaim
point, error path or new allocation alignment guarantee.

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

`new_fs_file_owner` also reserves the outer `TeeFsHtree` and concrete
`TeeFsFdAux` storage before invoking the file initializer. All three reservations
therefore precede `get_dirh()` and temporary-number mutation in public open/create,
and precede backend RPC open/create in the primitive path. Each reservation uses
NOWAIT because directory-file opens can already hold REE serialization locks.
Allocator errors use the existing `TeeError::from(KError)` conversion.
After RPC open, `KBox::write` initializes the storage reservation and coerces it
to `KBox<dyn TeeFsHtreeStorageOps>` without allocating. Htree open initializes
its supplied reservation with that storage owner, without a dummy storage object.

`TeeFsFd::ht` is `Option<KBox<TeeFsHtree>>`: default is empty, successful open
installs the tree, and close takes and drops it before explicitly closing the fd.
Close does not allocate a replacement tree or storage owner. Operations requiring
a tree return BAD_STATE when it is absent. The fd remains an explicit-close
integer handle; dropping an owner alone does not close it. Recursive `Subtree`
nodes use the fallible growth transaction described above; existing Vec and
pobj rename behavior is unchanged.

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

The metadata reservation boundary covers outer ownership, including the tree and
storage owners. `TeeFsHtree::new(storage)` replaces the tree's former Default
construction and consumes a prepared storage owner; `TeeFsFd::default()` leaves
`ht` as `None`. Recursive hash-tree nodes and their allocation behavior remain unchanged.
Scratch blocks and inline FileVariant handles have the separate boundaries
described above. In particular, incremental tree growth needs
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
persistent object ID ownership in `tee_pobj.rs` and rename owner transfer in
`tee_svc_storage.rs`, as well as
filesystem metadata owner reservation, scratch blocks and inline file handles in
`tee_ree_fs.rs` and initialization in
`fs_dirfile.rs` under `src/tee/`, including ownership held by `tee_obj.rs` and
`tee_svc_storage.rs`, as well as property-query owner preparation in
`tee_property.rs`, and byte-owner preparation in the three service modules and
the shared pointer/ownership APIs, with TEE error conversion in `mod.rs`.
The CSV section covers only temporary ownership, not its hardware protocol.
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

The unittest-only `ExecSessionFixture` in `ut-tests/tee/tee_session/exec_fixture.rs` supports
`core/ksyscall/ut-tests/task/tests/exec_lifecycle.rs`. It checks session retention
until fatal cleanup, success-only slot removal and eventual resource release.
These tests use transient resources and do not cover persistent storage,
signed-TA loading or execution of user TA instructions.

## Control-Plane Wire Mirror

`src/tee/control_protocol.rs` mirrors the x-kernel ↔ vsock-manager control
plane defined by plan 004 §4. The two ends do not share a crate: the kernel
workspace does not depend on `teec-protocol`, so consistency is enforced only
by the F1–F9 fixed golden byte sequences committed on both sides. Any field or
variant order change requires a design change first, then synchronized golden
updates; one side's test turning red is protocol drift and must not be fixed
unilaterally.

The module keeps three responsibilities and nothing else:

- wire types (`ControlRequest`/`ControlResponse` families) with the external
  `TEE_*` names retained verbatim so both mirrors can be compared field by
  field;
- little-endian framing helpers: `control_frame_payload_len` validates the
  4-byte length prefix against the 65,536-byte payload cap before any
  allocation, `encode_control_frame` serializes a whole frame, and
  `decode_control_frame` rejects truncated or malformed input;
- the version constant pinned to 1.

Postcard is the payload codec; `TEE_ParamType` travels as its declaration-order
variant index, not its GlobalPlatform value, and the module forbids custom
serializers that would change that. The existing internal protocol in
`src/tee/protocal.rs` uses a native-endian length prefix and remains separate;
the two framings must not be mixed on one connection.

Socket I/O, per-stage timeouts, and TEE error/origin mapping belong to the
control-plane client in `src/tee/tee_ta_manager.rs` (ticket 06); the golden
tests run in the QEMU unittest image.

## Control-Plane Client

`src/tee/tee_ta_manager.rs` owns the kernel half of the manager control plane.
Every operation opens a fresh connection to the fixed manager socket
`/tmp/tee-manager-control.sock`, writes one request frame, reads one response
frame, and drops the connection: no connection reuse, hence no cross-thread
response interleaving and no socket state inside `TeeTaCtx`. Both the send and
receive timeouts are configured before `connect` — the Unix stream backlog
also honors the send timeout — and one monotonic absolute deadline covers
connect, all partial sends, and all partial receives: 5 seconds per ordinary
operation and 1 second for `ReleaseOwner`. Each blocking operation receives
only the remaining budget. The 4-byte length prefix is validated against the
65,536-byte cap before any allocation, the response version must equal
`CONTROL_PROTOCOL_VERSION`, and the response variant must match the request
operation (the generic `Error` response is always accepted). There is no
request replay. A read or write interrupted with EINTR continues at the
current byte offset and refreshes the remaining part of that same deadline.
This includes a pending task interruption at last-thread exit; it does not
reconnect or resend an Open, Invoke, Close or ReleaseOwner. Every terminal
transport or codec failure maps to `TEE_ERROR_COMMUNICATION` + `TEE_ORIGIN_COMMS`.

`tee_ta_release_sessions` is the process-exit cleanup entry point exported for
the named `posix/process::exit_tee` stage. Processes without open sessions or an
uncertain Close do not touch the socket. The `ReleaseOwner` exchange shares the single 1-second
budget; per-close failures from the manager are logged and ignored, the local
table is cleared regardless of the manager outcome, and an unreachable manager
returns `TEE_ERROR_COMMUNICATION` to the caller without retrying or blocking
exit.

`posix/process` invokes this entry only from the thread that reserved the
last-thread qualification through `kprocess::process_exit::finish_thread_exit`.
The explicit `exit_tee` stage runs after TID unpublication, the thread's exit mark
and final CPU accounting, with the membership lock released. The current user
thread remains schedulable and retains its process runtime and filesystem
context; neither session lookup nor connection credential capture depends on
the global TID directory. The stage completes before `exit_mm`,
`clear_tee_runtime_private`, `exit_fs`, namespace detach and parent-visible
process completion. It may sleep for the bounded socket exchange, but must not
touch user memory or process descriptors. Path lookup uses the still-installed
filesystem context. Fork creates a fresh
`ProcessRuntime` and `TeeTaCtx`, so child processes do not inherit session
handles; successful exec preserves the existing `TeeTaCtx`, so sessions remain
owned by that process across exec.

## Inter-TA Syscalls over the Control Plane

`src/tee/tee_inter_ta.rs` implements the three syscalls (plan 004 §6.4–§6.6).
`TEE_OpenTASession` performs the checked reads, converts the parameters,
exchanges one `OpenSession` request, writes back the process-local handle from
`TeeTaCtx::insert_session` (handles start at 1, skip 0, and the table-full
failure is `TEE_ERROR_OUT_OF_MEMORY`), and rolls the manager session back when
handle allocation or the `ta_sees` write fails. `TEE_InvokeTACommand` looks up
the local handle before any network traffic, applies the response parameters
per §7.4, and deletes the local entry when the manager reports the session
dead (`ITEM_NOT_FOUND/TEE`, `TARGET_DEAD`, owner mismatch). A TA command's
`ITEM_NOT_FOUND/TRUSTED_APP` preserves the handle for subsequent Invoke and
Close. `TEE_CloseTASession` deletes the entry on success, `ITEM_NOT_FOUND`,
`TARGET_DEAD`, and communication failure, including a normally decoded
`CloseSession { COMMUNICATION/COMMS }` response. It keeps the entry on
`ACCESS_DENIED` so a retry stays possible.

Parameter conversion is two-pass: all four type nibbles and the 60 KiB
input/capacity/response budgets are validated before any user memory is
touched; VALUE parameters carry the low 32 bits of both slots and MEMREF
capacities ride in the wire `values.a` slot, per the teec-protocol
conventions. Zero-length memrefs skip user-memory access entirely (`b == 0`
allows a NULL buffer). The SHORT_BUFFER contract differs by producer:
kernel-detected capacity overflow writes each param's required length without
copying data and returns `TEE_ORIGIN_API`; a TA-initiated
`TEE_ERROR_SHORT_BUFFER` takes the required length from the response
`values.a`, validates it against the response data length and the 60 KiB
budget, and passes the TA origin through. Output data is written to user
memory before the `utee_params` lengths commit, so a failed user write leaves
no partially-committed logical state.

Failures carry an origin alongside the result (`TeeFailure`): local failures
are `TEE_ORIGIN_API`, transport failures `TEE_ORIGIN_COMMS`, and wire failures
pass through the manager/TA origin verbatim — never guessed from the error
code. `returnOrigin` is a checked user write: on success it records
`TEE_ORIGIN_TEE`; on failure it records the actual origin and a failed origin
write cannot mask the primary error.

## Limited cooperative Invoke timeout

A finite `cancellationRequestTimeout`, including zero, becomes an absolute guest
CLOCK_MONOTONIC nanosecond deadline at the Invoke syscall entry. The appended
control variant `InvokeCommandWithTimeout` and the private manager-to-TA variant
4 carry that deadline without restarting it during transport or queueing.
Infinite Invoke uses the existing wire variant. Existing v1 variants are stable;
fixed golden bytes pin the extension in both endpoints.

The SDK session worker brackets only `invoke_command` with private syscall 572:
operation 0 installs the deadline, 1 ends the scope, 2 starts an unlimited scope.
The syscall changes the current thread's cancellation fields only. Begin/end
clear the cancel flag and deadline while retaining the mask and object tables.
The default deadline is `None` and cancellation starts masked. `TEE_Wait` sleeps
until its delay or an unmasked deadline, rechecks state after waking and returns
CANCEL when expired. `TEE_GetCancellationFlag` uses the same evaluator. No state
borrow or lock remains live across sleep.

The manager retains the ordinary owner checks and in-flight permit. Only the
target decides CANCEL, which keeps TRUSTED_APP origin. Close and ReleaseOwner
follow their existing lifecycle. RAII clears the scope before the worker replies,
so later requests and other session threads cannot inherit the deadline.

This phase requires coordinated kernel, manager and target SDK deployment.
Legacy SDK requests remain wire compatible. A timed request on an old kernel
returns NOT_SUPPORTED; an old target rejects the new request as a communication
failure. There is no fallback to unlimited execution. Arbitrary blocking calls,
CPU loops, Open timeout and external RequestCancellation remain outside this
phase. The existing 5-second control transport budget can expire before a masked
or noncooperative target finishes. Full cancellation needs a separate design.

## Legacy Panic Wire

`src/tee/protocal.rs` is reduced to the TA panic notification: the kernel
sends a `LegacyPanicFrame` — byte-identical to the old `TeeRequest::Panic`
postcard encoding (variant index 4 plus the panic-code varint) — with the
native-endian `send_framed_message` framing to `/tmp/{uuid}.sock`. The golden
tests pin the byte layout against the TA runtime mirror. The session variants
of the old wire have no kernel producer any more; extending this module is
forbidden, and the panic path will move to the shared `teec-protocol` schema
when that migration happens.

## Uncertain Close and owner cleanup

A Close transport/protocol failure invalidates the local handle immediately, but
cannot prove that the manager claimed cleanup. The process context retains an
owner release obligation independent of live handles. Process exit sends one
bounded ReleaseOwner even if the failed Close removed its last handle, then clears
both the session table and this obligation. Decoded Close responses retain the
existing handle invalidation rules and do not create a retryable handle.
