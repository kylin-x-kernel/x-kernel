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
| `sys_msgget` | User key and create/mode flags into manager | Queue limit → ENOSPC; missing key without create → ENOENT; create/exclusive collision → EEXIST (before permission); removed queue → EIDRM; existing-queue access → EACCES. |
| `sys_msgsnd` | User type/payload into a queue | Fault-aware type-header copy precedes validation; size above `MSGMAX`, a negative queue ID or a nonpositive type → EINVAL before the payload copy; fault-aware payload copy precedes queue lookup; unknown ID → EINVAL; write permission failure → EACCES, then a removed queue → EIDRM; byte/message capacity → EAGAIN. |
| `sys_msgrcv` | User selector/flags and output buffer | Invalid `MSG_COPY` combination/unknown ID → EINVAL; read permission failure → EACCES, then a removed queue → EIDRM; no match → ENOMSG; oversized message without truncation → E2BIG; copies propagate errors before removal. |
| `sys_msgctl` | User command/metadata; metadata to user buffer | Unsupported command/index/ID → EINVAL; removed queue → EIDRM; failed read check → EACCES; copy failures propagate. `IPC_SET` copies the user `msqid_ds` immediately after command validation, before queue lookup or control checks, so a bad pointer yields EFAULT even for a missing, removed or foreign queue. `IPC_STAT` uses read permission. For an existing queue, `IPC_SET` first requires effective UID equal to the current owner or creator, or `CAP_SYS_ADMIN` in the queue's owning user namespace (or an ancestor), else EPERM; `CAP_IPC_OWNER` does not authorize control. It then requires `CAP_SYS_RESOURCE` when raising qbytes above `MSGMNB`, else EPERM, and finally rejects `INVALID_UID`/`INVALID_GID` in user-supplied permissions → EINVAL. All checks precede mutation. |
| `sys_shmget` / `ShmInner::new` | Size/flags/key and explicit credential snapshot into file construction | Zero page count, missing keyed object or size/mode mismatch → InvalidInput; file create/unlink/open/truncate errors propagate. Size arithmetic is not comprehensively guarded here. |
| `sys_shmat` | User ID/address/flags into MM | Unknown ID, unrounded unaligned address or SHM_REMAP with an address that is zero after rounding → InvalidInput; no free range → NoMemory; mapping errors propagate. Flags are truncated; segment lock is released before mapping. |
| `sys_shmctl` | User command and `shmid_ds` buffer | Unknown ID/command → InvalidInput; fault-aware copies propagate. IPC_STAT with null buffer succeeds without copying. IPC_SET replaces the whole structure. |
| `sys_shmdt` | User start address into attachment lookup | Unknown address/segment/range → InvalidInput; MM errors propagate before accounting removal. |

`has_ipc_permission` mirrors Linux `ipcperms()`. Both the creator and current
owner UIDs select the owner class; both creator and current owner GIDs select the
group class through `Cred::in_group`, which tests the filesystem GID and
supplementary groups (not the effective GID). The requested bits are folded as
`(flag >> 6) | (flag >> 3) | flag`, so a raw `msgget` mode such as `0666` requires
both read and write. A denial is overridden only by `CAP_IPC_OWNER` held in the
queue's owning user namespace (or an ancestor); UID zero alone grants nothing.

Queue control has a separate Linux check: effective UID must equal the current
owner or creator, or the caller must hold `CAP_SYS_ADMIN` in the queue's owning
user namespace (or an ancestor). In
particular `CAP_IPC_OWNER` overrides read/write access but not `IPC_SET` or
`IPC_RMID`. Linux's `ksys_msgctl()` copies an `IPC_SET` buffer before resolving
the queue or running `ipcctl_obtain_check()`; this crate preserves that order, so
EFAULT takes priority over EIDRM and EPERM. After the copy, `IPC_SET` validates
a qbytes increase above `MSGMNB` with `CAP_SYS_RESOURCE` before changing any
field, replaces only the low mode bits, and preserves kernel-reserved mode bits.

Shared-memory calls still do not perform full owner, mode or capability
authorization. That is an explicit residual risk, not a security guarantee.
Constructors use explicit credentials for backing-file ownership, but that does
not authorize subsequent IPC operations.

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
| T-01 | Unauthorized IPC data/metadata access | High | An unprivileged user accesses another IPC ID. | Message-queue access applies Linux `ipcperms()` with creator/owner IDs, group membership and `CAP_IPC_OWNER`; control requires owner/creator or `CAP_SYS_ADMIN`, and qbytes growth checks `CAP_SYS_RESOURCE`. Shared-memory paths still lack complete authorization, so full IPC isolation is not guaranteed. |
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

Existing tests in `ut-tests/msg.rs` cover FIFO/type selection/accounting plus the
creator/current-owner ID classes, creator/current-owner group classes,
supplementary groups, folded raw flags, UID-zero rejection, `CAP_IPC_OWNER`,
owner/creator/`CAP_SYS_ADMIN` control, the `CAP_IPC_OWNER` control exclusion,
invalid `IPC_SET` owner sentinels, and the EFAULT-before-lookup ordering.
Tests in `ut-tests/shm.rs` cover the bidirectional map, creator credentials,
backing length and marked-segment exit cleanup; they do not establish complete
shared-memory permission isolation.

- Preserve copyout-before-message-removal and documented error branches.
- Keep message-queue authorization aligned with Linux `ipcperms()` and
  `ipcctl_obtain_check()`; do not substitute UID zero or `CAP_IPC_OWNER` for
  control authority.
- Preserve manager-to-object lock order and final removal rechecks.
- Keep MM ownership outside IPC and support every recorded attachment range.
- Check ID/size/counter overflow and IPC_SET field restrictions when hardening.
- Update limitations whenever blocking, authorization or namespace support changes.
