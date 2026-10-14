# kresources design

## Purpose and background

`ProcessResources` owns the resource limits and detachable files owner of one
process runtime. `kprocess` and POSIX adapters use it to access `kfd::FdTable` and
`krlimit::Rlimits`. Descriptor layout and close semantics belong to `kfd`/`kvfs`;
credential policy, process lookup, fork flags, and syscall decoding belong to the
callers. This crate does not locate the current task.

## Source scope and architecture

The entire implementation and kernel unit tests are in `src/lib.rs`.

```text
process runtime -> Arc<ProcessResources>
                       +-> RwLock<Rlimits>
                       +-> RwLock<Option<Arc<RwLock<FdTable>>>> -> VfsFile
```

`new(user_stack_size)` builds default limits (stack size in bytes) and an empty
shared table. Callers can copy limits and use `replace_fd_table` to implement
sharing or copying during fork. The public `rlimits` lock permits trusted kernel
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
operation. Insertion takes owner, table, then limits locks. Callers using the
public limits lock must not invert that order. `fd_table()` returns an independent
strong reference; it can outlive detachment and must not be treated as evidence
that the process still has a files owner.

`duplicate_file_from` snapshots the soft limit and releases the limits guard
before taking owner/table locks, so it introduces no reverse nested lock order.
The owner read guard and a single table write guard cover source lookup and
minimum-based allocation together. Detachment returns `NoSuchProcess`.

## State and resource flows

The files owner starts as `Some(table)` and `exit_files` changes it to `None`.
Repeated exit is harmless. Replacement and unsharing reject `None` with
`NoSuchProcess`; they cannot resurrect an exited owner.

- Limit reads validate the resource index. Setters reject soft greater than hard
  and reject every hard-limit increase until capability checks are implemented.
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

The detachable owner separates a dead process object from live descriptors.
Snapshot references and `CLONE_FILES` sharing can extend table/file lifetime;
`exit_files` releases only this process's reference. The final `FdTable` drop
closes its remaining entries. No custom `ProcessResources::drop` is needed.

Limits are stored independently from descriptor ownership so they remain
queryable after files are detached. This is storage and local policy, not an
enforcement engine for every Linux resource limit.

File-lock cleanup pins the selected table through each close callback so its
address-based POSIX owner cannot be reused during cleanup. Binding selection and
slot removal share the outer owner read guard. Blocking record-lock callers must
release that guard before waiting and revalidate the current binding with
`with_fd_table` before reporting success.
