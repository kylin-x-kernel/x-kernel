# cgroup2fs — Design Document

## Fallible superblock preparation

Cgroup2 constructors return VfsResult and use SimpleFs's KERNEL private-owner
allocation before invoking the root-node builder. The namespace root has already
been sampled and no cgroup hierarchy lock is retained across allocation. Boot
construction means late-init sleepable task context, not early boot or IRQ
context; reclaim, I/O and filesystem callbacks must be allowed.

## Fallible directory iterators

CgroupDir child_names uses a KBox iterator with default KERNEL policy under
SimpleDirOps' sleepable-task contract. begin_operation returns a lifecycle
reservation, not a held hierarchy mutex: its transaction guard is released
before return. Child-name and inode-registry snapshot locks are also released
before iterator allocation; the iterator owns names rather than any guard.

## Positioning

`cgroup2fs` is the Linux cgroup v2 filesystem ABI adapter. The hierarchy, controller state and task
membership are held by `kcgroup`; process lookup and stable task identity validation are held by `kprocess`.
This crate translates VFS operations into owner API calls and owns the per-mount VFS node registry; the registry
only stores inode identity/metadata and does not duplicate hierarchy or membership state.

## Cross-crate interface

- `cgroup_from_file(&VfsFile) -> Option<Arc<Cgroup>>` (`src/lib.rs`): resolves a
  cgroup2 directory descriptor into its cgroup object for `posix/bpf`, which
  uses it as the attach target of the cgroup-v2 device controller commands
  (`BPF_PROG_ATTACH`/`DETACH`/`LINK_CREATE`/`QUERY`). Provider: cgroup2fs;
  call direction `bpf(2)` -> `cgroup_from_file` ->
  `state::CgroupFsState::cgroup_of(inode)` (per-mount registry lookup) ->
  `Arc<Cgroup>`. Descriptors outside this filesystem return `None` (mapped to
  `EBADF` by the caller).

## Scope

Kernel unit tests are located in `ut-tests/lib.rs`, are mounted as the `tests` module by
`unittest::path_mod_test!` in `src/lib.rs`, and are compiled under `cfg(unittest)`.

```text
src/
├── lib.rs         # crate entry and public exports
├── mount.rs       # filesystem type, FsContext, superblock
├── state.rs       # per-mount stable directory/control-file inode registry
├── dir.rs         # dynamic directory enumeration, lookup, mkdir, rmdir
├── control.rs     # control file mode
├── command.rs     # control command parsing in kernel buffers
├── controller.rs  # controller registry and generic control files
├── pids.rs        # pids controller file adaptation
└── process.rs     # cgroup.procs display, authorization and process migration
```

## Architecture

```text
VFS
 │
 ├─ user mount ─────────────> current CgroupNamespace root
 ├─ fs_boot mount ──────────> initial CgroupNamespace root
 └─ CgroupFsState ──────────> stable VFS inode identity
     └─ dir.rs
         ├─ process.rs ─────> kprocess cgroup facade
         └─ controller.rs ──> pids.rs ─────> kcgroup
```

`state.rs` constructs the directory inode and the fixed control-file inodes once for each `Arc<Cgroup>`. Repeated lookups in `dir.rs`
only create or reuse dentries pointing to these inodes. Controller file visibility is determined from the canonical cgroup state on each
lookup/readdir. When adding a new controller, add the corresponding entries to the file declarations in state
and to the adapter in `controller.rs`.

## Calling Constraints / Execution Context

Ordinary user mounts and file operations require the current task to be a user process thread, and the scheduler, process
registry, VFS and cgroup namespace to be initialized. `new_initial_cgroup2fs()` is used only for the `fs_boot`
boot-time mount and does not require a current process. Paths use sleepable locks and must not be called from IRQ, softirq
or contexts holding a spinlock. The input has already been copied into the kernel by the VFS; this crate does not access user pointers.

## Algorithm Flow

### Mounting

An ordinary mount obtains the view root from the current process's `CgroupNamespace`. When `KFEAT_FS_CGROUP2=y`,
`fs_boot` mounts the initial namespace hierarchy at `/sys/fs/cgroup` before any user process is created. Each
superblock creates one `CgroupFsState` and first materializes the stable root inode.

### Control Files

`command.rs` rejects non-UTF-8 input, embedded NUL, empty `subtree_control` commands, and operations missing a `+`/`-` prefix.
The controller registry resolves all names before mutation. All writable control files use the KVFS
`CommandFile`: a single write is at most 4096 bytes and is handled as an independent command; the file offset and
earlier writes do not take part in concatenation. `pids.max` accepts only `max` or a numeric value within the Linux PID domain.

### Directories and Inode Identity

`mkdir` uses the parent inode, the prepared mode and the credential passed in by the VFS, and initializes the owner through
`inode_init_owner()`. `rmdir` uses the already-locked victim dentry, and deletes only after verifying that the
inode it references is the same as the target cgroup inode in the registry. After a node is deleted, the registry keeps a tombstone
inode until unmount; old fds continue to point to the old identity, and recreating the same name yields a new cgroup and a new inode.

### Process Display and Migration

`process.rs` obtains deduplicated PIDs through `kprocess::cgroup_member_process_ids()`. The facade verifies the
`Arc<PidHandle>` identity of the registry task and the membership, avoiding incorrect mapping caused by TID reuse.

When `cgroup.procs` is written, `kprocess` hands the stable source and destination to the adapter inside the process cgroup gate.
The adapter verifies that both ends are located under the mount view root, finds the nearest common ancestor, and uses the
credential saved when the fd was opened to check write permission on the common ancestor's `cgroup.procs` inode; only after this passes
is the migration of the whole group committed.

## Concurrency Model

`CgroupFsState` protects the node registry with a mutex and uses `Weak<SimpleFs>` to avoid reference cycles. Each
control-file operation first acquires the `kcgroup` operation guard: while the guard is alive, removal returns `EBUSY`, and
after the node is deleted, new operations return `ENODEV`. Hierarchy transactions are serialized by `kcgroup`, and process publication and
whole-group migration are serialized by `kprocess`. The directory dentry cache is kept disabled so that, after a controller is activated, a stale
negative lookup does not hide newly visible files; each lookup still reuses the stable inode identity in the registry.

## Design Decisions

- The per-mount registry belongs to the VFS adapter rather than to `Cgroup`, so that the core hierarchy does not depend on the filesystem.
- hierarchy/controller mutation uses VFS directory/control-file DAC, without layering on a hidden
  current-process administrator check.
- `cgroup.procs` additionally performs the mount view and common-ancestor write permission checks, using the opener credential.
- The boot mount explicitly selects the initial view and does not implicitly fall back when there is no current process.

## Drop / Resource Release

The registry holds the stable inode and cgroup node of each live cgroup; `rmdir` drops the entry via `remove_node`, and remaining entries are released with the superblock. Old fds keep their inode alive through their own `Arc`. Membership, controller charge and hierarchy lifetime remain managed by `kcgroup`.
