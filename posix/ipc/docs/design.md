# posix-ipc — Design

## Purpose and ownership

This crate implements System V message-queue and shared-memory syscall adapters.
The complete source scope is `src/lib.rs` (common IPC constants, ID allocation,
time and permission helper), `src/msg.rs` (messages) and `src/shm.rs` (segments).
All public items in the latter modules are re-exported at the crate root.
`kprocess` provides current identity/address spaces, `osvm` provides user copying,
`memfs::shmem` owns segment backing files, and `filemap`/`memspace` own VMAs and
page mappings. IPC metadata and attachment bookkeeping belong here; page frames,
filesystem namespace objects and process lifecycle orchestration do not.

## Components and interaction

Syscall dispatch calls `sys_msgget`, `sys_msgsnd`, `sys_msgrcv`, `sys_msgctl` or
the corresponding `sys_shm*` entry points. `MSG_MANAGER` maps keys/IDs to
`Arc<Mutex<MessageQueue>>`; queues hold `Message` vectors and ABI counters.
`SHM_MANAGER` maps keys/IDs to `Arc<Mutex<ShmInner>>` and maps each process's
attachment start addresses to IDs. `BiBTreeMap` maintains the key/ID bijection.
`ShmInner` holds a backing `VfsFile`, ABI metadata and a vector of ranges per PID.
The process-exit owner calls `ShmManager::clear_proc_shm` after releasing the mm
owner to remove attachment accounting and reap marked segments.

Both IPC object classes allocate IDs from the shared relaxed `AtomicI32`
`IPC_ID`. Namespace-scoped ID allocation is not implemented.

## Message flow

`sys_msgget` checks the queue limit, resolves a key, handles create/exclusive
flags and publishes a new queue when required. `sys_msgsnd` copies the type and
payload from userspace, checks positive type, `MSGMAX`, byte and message limits,
then appends and updates sender/time metadata. `sys_msgrcv` chooses FIFO, exact
type, first nonmatching type, smallest qualifying type, or indexed `MSG_COPY`;
it copies type/payload out before removing a consumed message. `MSG_NOERROR`
permits truncation. Control operations query metadata/statistics, change selected
metadata or mark removal. An empty removed queue is removed from the manager;
nonempty removed queues remain stored. Blocking send/receive and wakeups are
not implemented: full queues return EAGAIN and no match returns ENOMSG.

## Shared-memory flow and state

`sys_shmget` derives page count and mapping flags, then resolves or creates a
segment. `ShmInner::new` receives explicit credentials, creates a shmem file,
unlinks its directory entry, opens it and truncates to the page-aligned length.
Its `shmid_ds` stores the requested byte size and effective creator/owner IDs.
An existing keyed segment requires exactly matching size and mapping flags.

`sys_shmat` snapshots the segment file/flags/length, drops the segment lock,
obtains a mapping owner and selects a free range. `SHM_RDONLY` removes write
permission; `SHM_RND` rounds a supplied address down. `SHM_REMAP` with a zero
address (including a nonzero address rounded to zero) returns `EINVAL` before
free-range selection, VMA installation or attachment bookkeeping. It calls
`mmap_shared_file`, installs the runtime VMA, records the range in `ShmInner`,
then updates the manager address index. Multiple attachments of the same segment
by the same process are supported and tracked independently by start address.

`sys_shmdt` finds the exact registered start address, holds the segment lock
while unmapping, then removes its attachment record. After dropping that lock,
it takes manager then segment locks to recheck removal and attachment count.
`IPC_RMID` sets `rmid`; a marked segment is destroyed from global lookup only
when the rechecked attachment count is zero. `clear_proc_shm` removes all ranges
for a PID without unmapping them: the process-exit caller handles mm release.

## Execution context and concurrency

Syscall entries require a current user thread, scheduler, allocator, filesystem
and address-space facilities. They can sleep on mutexes and copy user memory;
they must not run in interrupt context or early boot. Kernel tests can construct
standalone queues and can construct segment backing using explicit credentials.
A concrete queue usage sequence is `tests_msg::test_message_queue_remove_updates_accounting`
in `ut-tests/msg.rs`: create, enqueue, remove and verify accounting.

The manager mutexes protect global indices; each queue/segment mutex protects
its local metadata and contents. When both manager and object are needed, take
manager before object. `sys_shmat` drops the object lock before entering MM;
`sys_shmdt` currently holds it across `MmSpace::unmap`, but never acquires the
manager under that object lock. User copies in message/control paths may execute
under object locks. Attachment publication spans separate MM, object and manager
steps; it is not documented as a single atomic transaction.

## Decisions, lifecycle and limitations

File-backed shmem shares content ownership with inode pagecache rather than
creating another physical-page owner. Unlinking the backing file makes final
file/mapping references determine its lifetime. Removing global IPC metadata
only releases this crate's references; mappings may still keep file state alive.

Message permission helpers are called with hard-coded UID/GID zero, so they do
not enforce real per-user isolation. Shared-memory authorization and Linux flag
semantics are incomplete: nonzero-address `SHM_REMAP` replacement is not
implemented, unknown bits are truncated, and creation/exclusive/huge-page
semantics are not fully checked.
`IPC_SET` for shm replaces the user-supplied ABI structure, including fields
normally maintained by the kernel. IPC namespaces, full Linux accounting and
robust global ID exhaustion handling are not provided here.

## Kernel unit-test layout

Guest test sources live in `ut-tests/` and are mounted at their original
module boundaries with `unittest::path_mod_test!`. They use `#[def_test]`
and run in the configured kernel with `make unittest`, not host Cargo tests.
The migration preserves module visibility, feature gates and execution modes.

- `ut-tests/shm.rs`: mounted by `src/shm.rs` as `tests_shm`.
- `ut-tests/msg.rs`: mounted by `src/msg.rs` as `tests_msg`.
