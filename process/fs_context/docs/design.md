# fs_context design

## Purpose and background

`FsStruct` owns process filesystem state: root, working directory, umask, and an
exec-transition flag. `kprocess::ProcessRuntime` owns or shares its
`Arc<Mutex<FsStruct>>`. This is distinct from `kvfs::FsContext`, which describes
a filesystem creation/reconfiguration transaction. Path lookup, access policy,
and mount ownership belong to KVFS and POSIX callers.

## Scope and architecture

All implementation is in `src/lib.rs`. The initial object is published lazily
through `INIT_FS`; `init_fs()` clones its handle. `copy_init_fs_struct()` makes a
process-private copy. `FsStruct` owns `Option<Path>` root/pwd values, a `u32`
umask, a boolean `in_exec`, and an optional atomic `process_users` owner count.
`Path` references keep mount/dentry objects alive. The count is allocated on
first adoption and sits behind an `Arc<AtomicUsize>` so a process can release
its ownership without taking the sleepable mutex that protects the path and exec
state. A context that has never been adopted has no count to allocate or
decrement.

## Execution context and concurrency

Use task context with allocator and sleepable mutex support initialized. Initial
state can be created before mounting; path access requires a root to be installed.
The crate does not require a current task, CPU-local state, or device mappings.
The shared object requires the caller's outer mutex; direct `&mut FsStruct`
operations require exclusive access instead. Updates can drop VFS references and
are unsuitable for interrupt context or incompatible spinlock critical sections.
Take `root_and_pwd()` snapshots under the lock and release it before path I/O.

## State and algorithms

`for_init_task` starts with both paths absent, umask `0o022`, and `in_exec=false`.
`attach_root` validates a directory and installs it as both root and pwd. A caller
must do this before calling `root`/`pwd`, which panic on absent paths.

`from_root_and_pwd` and paired replacement validate both paths before mutation.
They do not prove that pwd is below root or in the same namespace. `set_root`
only initializes pwd if absent; `set_pwd` rejects an absent root. `new` panics
on a non-directory root instead of returning the constructor error.

- Fork without `CLONE_FS` uses `clone_for_process`, sharing `Path` references but
  clearing `in_exec`. Sharing the `Arc<Mutex<_>>` implements `CLONE_FS`.
- `snapshot` and `clone_with_pwd` retain `in_exec`, unlike `clone_for_process`.
  Neither copies `process_users`; a snapshot starts without an owner count and
  must be adopted by its new owner, which allocates an independent count.
  Private namespace replacements (`unshare`/`setns`) use `clone_for_process`, so
  a fresh context never inherits another context's exclusive-exec marker.
- `add_process_user` allocates the owner count on first adoption and returns a
  `ProcessUserGuard`; `try_add_process_user` returns `None` while `in_exec`
  blocks `CLONE_FS` adoption. `ProcessRuntime`
  keeps the guard beside the context it owns, replaces both together in
  `set_fs_context`, and releases the guard on `exit_fs` or runtime destruction.
  Guard destruction only decrements the atomic count, so it is safe during
  deferred task finalization where sleeping filesystem locks are unavailable.
- `replace_umask` keeps only `0o777` bits and returns the previous mask.
- `begin_exec` reports whether another process owns the context and, only when
  it does not, sets `in_exec`. The caller's external mutex serializes
  `begin_exec` with `try_add_process_user`, which rejects adoption while
  `in_exec` is set. `end_exec` clears it. The owning process must keep the
  admission guard alive until it commits exec credentials and must clear it on
  every error path.
- Namespace cloning may retarget a private context with `replace_root_and_pwd`.

## Decisions and resource lifecycle

A separate crate lets namespace and process code share one filesystem-state
owner without placing process lifecycle in KVFS. `Option` represents genuine
pre-mount initialization, not a user-visible path-resolution error. `FsStruct`
itself has no custom `Drop`: replacement and final context release drop `Path`
references and the optional count handle. `ProcessUserGuard::drop` is the one
explicit destructor; it performs the lock-free owner release.
The final `Arc` release destroys a private context; the lazy initial context is
retained globally. Owner counts are logical process adoptions, not `Arc` strong
counts, so temporary snapshots and handles cannot create false `CLONE_FS`
sharing. A snapshot does not allocate a count unless it is adopted.
Authorization and namespace compatibility remain caller duties.
