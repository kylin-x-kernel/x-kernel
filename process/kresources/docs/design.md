# kresources design

## Purpose and background

`ProcessResources` owns process-wide resource limits. `TaskFiles` owns the
detachable fd-table binding of one thread, matching Linux `task_struct.files`. `kprocess` and POSIX adapters use it to access `kfd::FdTable` and
`krlimit::Rlimits`. Descriptor layout and close semantics belong to `kfd`/`kvfs`;
credential policy, process lookup, fork flags, and syscall decoding belong to the
callers. This crate does not locate the current task.

## Source scope and architecture

Limits are in `src/lib.rs`; task files are in `src/files.rs`; kernel unit tests are in
`ut-tests/lib.rs`, registered through `unittest::path_mod_test!`.

```text
process runtime -> Arc<ProcessResources> -> RwLock<Rlimits>
thread -> TaskFiles -> RwLock<Option<Arc<FdTableOwner>>> -> Arc<RwLock<FdTable>> -> VfsFile
```

`ProcessResources::new(user_stack_size)` builds default limits (stack size in
bytes). `TaskFiles::new` attaches an initial table; `fork(shared)` creates a
separate binding to a shared or copied table. Sibling unshare cannot replace
another thread's binding. The public `rlimits` lock permits trusted kernel
callers to access the table directly; it does not enforce the setter policy.

## Execution context and concurrency

Use task context with allocator and scheduler/lock services available. Access
can acquire sleepable `RwLock`s and close can invoke filesystem operations, so
these operations are unsuitable for interrupt context or incompatible spinlock
critical sections. No current user process, CPU-local state, or hardware mapping
is required by this crate. Creation is valid during process boot setup after its
dependencies have initialized.

The outer files-owner lock protects attachment, the inner table lock protects
slots, and the limits lock protects limit pairs. `with_fd_table` holds an owner
read guard through its callback, preventing concurrent detach during that table
operation. Mutating operations retain that guard through slot removal and pin
the selected table until close callbacks finish. Callers read the descriptor
limit before insertion; the limits lock is not nested with table locks. `fd_table()` returns an independent
strong reference; it can outlive detachment and must not be treated as evidence
that the task still has a files owner. It does not keep descriptors open:
last-binding cleanup drains the table while such references remain.
`fork()` prepares a shared or private binding under the binding guard. Only these
bindings participate in the sharing check used by unshare.

`CurrentFiles::duplicate_file_from` snapshots the process soft limit before
calling `TaskFiles::duplicate_file_from`, so the limits guard is released before
taking binding/table locks. No reverse nested lock order is introduced.
The owner read guard and a single table write guard cover source lookup and
minimum-based allocation together. Detachment returns `NoSuchProcess`.

## State and resource flows

The files owner starts as `Some(table)` and `exit_files` changes it to `None`.
Repeated exit is harmless. Fork and unsharing reject `None` with
`NoSuchProcess`; they cannot resurrect an exited owner.

- Limit reads validate the resource index. Passing `None` to
  `update_rlimit_and_get_old` delegates to `rlimit` for a checked snapshot under
  a limits read guard, so pure queries can share the lock. With `Some`, soft/hard
  validation, comparison against the current hard cap, the old snapshot and
  replacement share one limits write guard. The returned old pair is exactly
  the one replaced.
  `set_rlimit` is the ordinary typed setter for callers that do not need the old
  pair; it delegates to that write transaction and discards the old value.
  Every hard-limit increase remains rejected until capability checks are
  implemented.
- Insertion passes `RLIMIT_NOFILE.current` to `FdTable::add_file`. Fixed-slot
  duplication uses the table's capacity check, not that soft-limit policy.
  Minimum-based duplication passes a limit snapshot to `FdTable::duplicate_from`.
  Ordinary insertion and minimum-based duplication share the table's lowest-free
  slot allocator; both bound the allocated number rather than occupied count.
- Close, range close, close-on-exec, and fixed-slot replacement remove entries
  under the table lock together with its POSIX lock owner, then run descriptor close outside both table and owner
  locks. Single close returns its flush error; batch cleanup ignores each error
  and continues.
- `unshare_fd_table` keeps a sole-owned table or clones a shared table while
  retaining the open-file `Arc`s. It swaps the owner under the outer write lock
  and drops the old owner after unlocking.

## Design decisions and cleanup

The detachable binding separates an exited task object from live descriptors.
Snapshot references and `CLONE_FILES` sharing can extend table/file lifetime;
`exit_files` releases only this task's reference. The final `FdTable` drop
closes its remaining entries. No custom `TaskFiles::drop` is needed.

Limits are stored independently from descriptor ownership so they remain
queryable after files are detached. This is storage and local policy, not an
enforcement engine for every Linux resource limit.

File-lock cleanup pins the selected table through each close callback so its
address-based POSIX owner cannot be reused during cleanup. Binding selection and
slot removal share the outer owner read guard. Blocking record-lock callers must
release that guard before waiting and revalidate the current binding with
`with_fd_table` before reporting success.

`FdTableOwner` separates binding lifetime from table-storage lifetime. There is
no manually maintained owner count: `Arc<FdTableOwner>` counts sharing bindings,
while temporary `FdTableRef` handles only pin storage and expose read-only access.
Mutations use `with_fd_table` or borrow the owner under the binding guard; the
writable table reference cannot escape that lifetime. Last-owner
Drop removes and closes one descriptor at a time, outside table locks, without
allocating a cleanup collection. Once drained, later table Drop closes nothing.
The process-wide binding model is unchanged; per-thread bindings remain separate
work. Existing blocking fcntl binding revalidation still handles real replacement.
