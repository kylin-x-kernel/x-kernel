# posix-ipc — Security and reliability

## Scope, assets and trust

This analysis covers the entire crate: `src/lib.rs`, `src/msg.rs`, `src/shm.rs`,
including their public data structures. Assets are message payloads, shared-file
contents, IPC IDs, permission metadata, queue accounting and process attachment
ranges. Kernel callers of mutable queue/manager APIs are trusted to preserve
accounting and paired indices. Syscall IDs, flags, lengths, types, metadata and
user pointers are untrusted. Current-process identity comes from `kprocess`;
user copying belongs to `osvm`, backing files to `memfs`/`kvfs`, and mapping and
fault isolation to `filemap`/`memspace`.

## External inputs and checks

| Entry | Input / direction | Local checks and failure |
|---|---|---|
| `sys_msgget` | User key and create/mode flags into manager | Queue limit → ENOSPC; missing key without create → ENOENT; create/exclusive collision → EEXIST; removed queue → EIDRM. |
| `sys_msgsnd` | User type/payload into a queue | Size above `MSGMAX`, nonpositive type or unknown ID → EINVAL; byte/message capacity → EAGAIN; fault-aware input copies propagate errors. |
| `sys_msgrcv` | User selector/flags and output buffer | Invalid `MSG_COPY` combination/unknown ID → EINVAL; removed queue → EIDRM; no match → ENOMSG; oversized message without truncation → E2BIG; copies propagate errors before removal. |
| `sys_msgctl` | User command/metadata; metadata to user buffer | Unsupported command/index/ID → EINVAL; removed queue → EIDRM; copy failures propagate. Owner/creator/root and qbytes privilege branches exist, but caller UID is currently zero. |
| `sys_shmget` / `ShmInner::new` | Size/flags/key and explicit credential snapshot into file construction | Zero page count, missing keyed object or size/mode mismatch → InvalidInput; file create/unlink/open/truncate errors propagate. Size arithmetic is not comprehensively guarded here. |
| `sys_shmat` | User ID/address/flags into MM | Unknown ID, unrounded unaligned address or SHM_REMAP with an address that is zero after rounding → InvalidInput; no free range → NoMemory; mapping errors propagate. Flags are truncated; segment lock is released before mapping. |
| `sys_shmctl` | User command and `shmid_ds` buffer | Unknown ID/command → InvalidInput; fault-aware copies propagate. IPC_STAT with null buffer succeeds without copying. IPC_SET replaces the whole structure. |
| `sys_shmdt` | User start address into attachment lookup | Unknown address/segment/range → InvalidInput; MM errors propagate before accounting removal. |

`has_ipc_permission` compares owner/group/mode and bypasses checks for UID zero.
Message syscalls pass hard-coded zero IDs, so the EACCES/EPERM branches do not
provide effective user isolation. Shared-memory calls do not perform full owner,
mode or capability authorization. These are explicit residual risks, not
security guarantees. Constructors use explicit credentials for backing-file
ownership, but that does not authorize subsequent IPC operations.

## Unsafe and memory safety

There are no explicit unsafe blocks/functions/impls, FFI declarations or inline
assembly in this crate, including its tests. Raw user pointers are passed to
fault-aware `osvm` helpers rather than dereferenced directly. Provider safety
contracts still apply to those safe interfaces. Shmem contents remain in
inode-owned file state and VMAs; this crate holds references and bookkeeping,
not raw frame pointers.

Queue counters must reflect owned payloads; public mutable fields mean trusted
kernel callers can violate this invariant. `ShmInner` records multiple ranges per
PID and its attach count is the sum of their vector lengths. `shm_nattch` is
updated with attach/detach, but user replacement of `shmid_ds` can invalidate
that ABI counter. Manager address indices and per-segment ranges must stay in
sync. Mapping succeeds before attachment publication, which spans separate locks.

## Thread safety

Manager mutexes protect IDs and lookup tables. Queue/segment mutexes protect
payloads and object metadata. Acquire manager before object when nested.
`sys_shmdt` unmaps under the segment lock, releases it before acquiring the
manager, and rechecks `rmid` plus attach count under manager then segment.
`clear_proc_shm` follows the same nested order. Do not claim that all object locks
are released across MM calls or that attachment publication is atomic.

## Threat analysis

| ID | Threat / asset | Severity | Trigger | Response and residual risk |
|---|---|---|---|---|
| T-01 | Unauthorized IPC data/metadata access | High | An unprivileged user accesses another IPC ID. | Existing message permission code receives UID zero and shm lacks full authorization; isolation is not guaranteed. Proper current-credential and ownership checks are required before treating this as a security boundary. |
| T-02 | Kernel resource exhaustion | Medium | Repeated creates or large/zero-length message traffic. | `MSGMNI`, `MSGMAX` and qbytes/message checks bound message paths; shared-memory/global allocation and ID wrap protection remain incomplete. |
| T-03 | Invalid user pointer interrupts a transfer | Medium | Unmapped or changed user buffer. | `read_vm`, `write_vm`, `load_vm_vec` and `write_vm_slice` return errors; receive removes data only after copyout. Partial copyout remains observable on failure. |
| T-04 | Forged shm metadata corrupts accounting | Medium | IPC_SET provides arbitrary count/permission/size fields. | No field-level restriction exists in this path; document the limitation and require restricted updates for stronger isolation. |
| T-05 | Concurrent detach/removal frees metadata prematurely | Medium | IPC_RMID races detach or process exit. | Recheck `rmid` and zero attachment count while holding manager then segment; file/mapping references preserve backing ownership. Attach publication still spans separate phases. |
| T-06 | Lock inversion blocks IPC users | Medium | A path takes manager while holding segment/queue lock. | Current removal paths drop the object lock before manager acquisition; preserve that order. MM/user-copy calls under object locks still require provider-compatible context. |

## Failure modes and management

| ID | Failure mode | Cause | Local effect | System effect | Severity (1–4) | Controls |
|---|---|---|---|---|---|---|
| F-01 | Backing file creation/open/truncate fails | VFS or allocation failure | Segment creation returns an error | Caller cannot allocate shm | 3 | Propagate before manager insertion. |
| F-02 | Mapping allocation fails | Unavailable virtual range or MM/filemap error | No new attachment is recorded | Caller cannot attach | 3 | Return MM/filemap error; references drop normally. |
| F-03 | Message capacity reached / no receive match | Capacity limit or unmatched selector | Transfer does not proceed | Applications expecting blocking can fail or retry | 3 | Return EAGAIN/ENOMSG; blocking waits/wakes are explicitly unimplemented. |
| F-04 | Removed nonempty message queue retained | IPC_RMID with queued payloads | Payload/metadata remain allocated | Memory use can persist | 2 | Mark removed; full deletion semantics remain incomplete. |
| F-05 | User-mutated shm counters diverge | Unrestricted IPC_SET metadata replacement | ABI metadata becomes inconsistent | Accounting consumers see incorrect state | 2 | No complete current mitigation; constrain IPC_SET before relying on counters. |

Ordinary errors propagate as `KResult`; this crate provides no recovery protocol
for allocation failure, integer overflow or externally corrupted kernel state.
There is no comprehensive transaction rollback covering concurrent MM and all
IPC index changes.

## Privacy, verification and audit checklist

Messages and shared pages contain user data. Metadata exposes IDs and sizes;
shmat logs PID, virtual address, size and mapping flags. Trusted log access and
provider page-reuse protections are external responsibilities; this crate does
not redact logs or explicitly zero payload allocations on drop.

Existing tests in `ut-tests/msg.rs` cover FIFO/type selection/accounting. Tests in
`ut-tests/shm.rs` cover the bidirectional map, creator credentials, backing length and
marked-segment exit cleanup; they do not establish complete permission isolation.

- Preserve copyout-before-message-removal and documented error branches.
- Audit real credentials before claiming access control is enforced.
- Preserve manager-to-object lock order and final removal rechecks.
- Keep MM ownership outside IPC and support every recorded attachment range.
- Check ID/size/counter overflow and IPC_SET field restrictions when hardening.
- Update limitations whenever blocking, authorization or namespace support changes.
