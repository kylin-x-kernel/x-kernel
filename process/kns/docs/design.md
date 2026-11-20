# kns design

Kernel unit tests live in `ut-tests/` and are mounted from their original
source modules with `unittest::path_mod_test!` under `cfg(unittest)`.
The namespace-fd cases in `ut-tests/nsfd.rs` retain the private
`nsfd::tests_nsfd` module and the serial anonymous-file round-trip case.

## Purpose and background

`kns` bundles process namespace references in `NsProxy`, exposes namespaces as
first-class file descriptors through `NsFd`, and owns the two-phase `unshare`
preparation that the syscall layer commits. Process runtime code
owns/replaces the bundle; syscall code parses flags and authorizes operations.
Mount trees belong to `kvfs::MntNamespace`, process paths to
`fs_context::FsStruct`, task-active PID identity to `kidentity`, credential/user
namespace semantics to `kcred`, and cgroup hierarchy/controller state to
`kcgroup`. `NsProxy` does not duplicate those owners.

## Scope and architecture

`src/lib.rs` exports modules `error`, `ipc`, `net`, `nsfd`, `nsproxy`, `pid`,
`plan`, `time`, `types`, and `uts`. It re-exports `CgroupNamespace`,
`NamespaceId`, `UserNamespace`, `MntNamespace`, `PidNamespace`, and
`NamespacePlan` from their owner crates.

```text
process runtime -> Arc<NsProxy>
     +-> mnt_ns: Arc<MntNamespace>
     +-> uts_ns: Arc<UtsNamespace> -> RwLock<UtsInner>
     +-> ipc_ns: Arc<IpcNamespace> -> NamespaceId
     +-> pid_ns_for_children: Arc<PidNamespace>
     +-> net_ns: Arc<NetNamespace> -> NamespaceId
     +-> cgroup_ns: Arc<CgroupNamespace>
     +-> time_ns / time_ns_for_children: Arc<TimeNamespace>

NsFd (namespace file descriptor)
     +-> exactly one namespace reference, pinned for the fd lifetime
     +-> anon_inode:[<type>_ns] display path, reads EOF, writes denied
```

`NamespaceFlags` represents Linux creation bits. `NamespaceType` supplies names
for namespace displays. IPC/time types hold IDs, not their managers: the current
`posix-ipc` implementation still uses global message/shared-memory managers
without keying them by this IPC identity, so creating a new IPC namespace ID
does not currently isolate those resources, and this crate does not own their
queues, segments or manager migration.

### Net namespace lifecycle

`net.rs` holds more than an ID. `NetNamespace` carries the stable identity that
also serves as the `/proc/<pid>/ns/net` inode, and it owns two lifecycle hooks:

- `NetNamespace::new_initial()` records the boot namespace's id in the
  `INITIAL_NET_ID` global. `NamespaceId` is a kernel-wide counter shared by
  every namespace kind, so the initial net id is whatever the counter holds at
  boot-time construction — not a fixed constant. `NetNamespace::initial_id()`
  exposes it to the stack owner.
- `impl Drop for NetNamespace` invokes the callback installed by
  `set_net_namespace_drop_hook`, letting the protocol-stack owner (`knet`)
  release per-namespace state when the last reference goes away.

`set_net_namespace_drop_hook` is a cross-crate callback interface: the provider
is `knet` (the protocol-stack owner) and the caller is `NetNamespace::drop`
inside this crate. The direction is kns → external owner through a registered
function pointer, so `kns` does not depend on `knet`. `NEWNET` creation,
`setns` entry and clone all produce a fresh `NetNamespace` whose stack is
materialized by `knet::netns::ensure_stack` on the syscall path.

The drop-hook and initial-id globals are `ksync::Mutex`-protected and declared
through `static_lock!` so they compile under both the plain and the lock-stat
lock implementations. They are written once at boot (initial id) or when the
owner installs its hook (`knet`'s first `ensure_stack` call) and read on
namespace creation/destruction; neither is taken while holding a process-runtime
lock, and the hook body only removes one map entry under its own mutex.

### NsFd

`nsfd.rs` defines `NsFd` with one variant per namespace type (`Uts`, `Ipc`,
`Mnt`, `Pid`, `Net`, `User`, `Cgroup`), each holding `Arc` to the namespace
object. Key behavior:

- `ns_type()` maps to `NamespaceType`; `namespace_flag()` maps to the
  `CLONE_NEW*` flag that creates the type.
- Opening `/proc/<pid>/ns/<type>` (procfs `ThreadNsDir`) materializes an
  `NsFd` snapshot of the process's current namespace and attaches it as the
  open description's private data, so `setns(2)` downcasts it.
- Display names follow Linux's anonymous-inode convention:
  `anon_inode:[uts_ns]`, `[ipc_ns]`, `[mnt_ns]`, `[pid_ns_for_children]`,
  `[net_ns]`, `[user_ns]`, `[cgroup_ns]`.
- Reads return EOF, writes are denied (the fd is a capability handle only).
- `NsFd::Pid` carries whichever PID namespace the entry named: procfs
  `ns/pid` snapshots the task's own namespace (task_active_pid_ns) while
  `ns/pid_for_children` snapshots the children slot, and `setns` installs
  either into the caller's `pid_ns_for_children` slot exactly as Linux does.
  The two entries therefore share one NsFd variant distinguished only by the
  namespace object it pins.

## Execution context

Bundle creation/cloning, unshare preparation, and UTS operations require a
working allocator and sleepable locks and are intended for normal task context.
They are not interrupt or atomic-context primitives. `new_initial` requires boot
to have installed the initial VFS mount namespace; the explicit-mount
constructor avoids that global lookup. There is no current-task lookup,
CPU-local state, or MMIO here. For `clone_for_child` NEWNS, callers provide
exclusive access to a private `FsStruct` whose root and pwd are already
initialized.

## Construction and clone flow

`NamespacePlan::build(flags, parent, cred, fs_context, current_cgroup)` is the
single construction entry shared by `clone(2)`, `clone3(2)`, and `unshare(2)`.
It is provided by `kns`; the syscall layer supplies the decoded flags, the
caller's `Arc<Cred>`, an optional private `FsStruct`, and the caller's current
cgroup. It returns a `NamespacePlan` holding the freshly created namespaces (or
`None` for an unselected flag) or an `UnshareError` (`NotPermitted`,
`InvalidArgument`, `NoSpace`, `Mount`). The process layer then commits the plan
into a child `NsProxy` or `unshare`'s `PreparedUnshare`; `plan.rs` never
installs a namespace into a running process itself.

`new_initial` caches a bundle with `Once`. The explicit-mount constructor
creates fresh UTS/IPC/net/time objects, shares the global root PID namespace,
and reuses `CgroupNamespace::initial`. Independent explicit-mount bundles still
share the initial cgroup hierarchy so boot mounts and PID 1 do not acquire
separate system roots.

`clone_for_child` handles every `CLONE_NEW*` flag through `NamespacePlan`.
`CLONE_NEWUSER` creates the child user namespace first; it becomes the owner of
the sibling namespaces and is installed in the child credential. Without
`CLONE_NEWUSER`, the caller's existing user namespace owns them, and creating
any other namespace requires `has_cap_in(caller_ns, CAP_SYS_ADMIN)`. A child
receives a fresh `NetNamespace` whose protocol stack is materialized by
`knet::netns::ensure_stack`, the selected time namespace, and a cgroup namespace
rooted at the caller's current cgroup. With NEWNS it rejects
`NamespaceFsContext::Shared`; otherwise it snapshots root/pwd, calls KVFS
`clone_with_root_and_pwd_in(..., owner)`, retargets the private `FsStruct` as a
pair, and retains the new mount namespace. NEWUTS copies both names into a new
identity; NEWIPC creates a new namespace ID; NEWPID creates a child
`PidNamespace` nested below the parent's `pid_ns_for_children`. Unselected
references are shared. The function does not install the result into a running
process.

## unshare two-phase protocol

`NsProxy::prepare_unshare` performs all Linux checks and returns a
`PreparedUnshare`, so the commit phase only installs precomputed state:

1. NEWUSER creates a child user namespace and installs it in the caller's
   credential; NEWNET creates a fresh `NetNamespace` identity and the syscall
   path materializes its protocol stack with `knet::netns::ensure_stack`.
   NEWCGROUP creates a view rooted at `UnshareContext::current_cgroup`;
   NEWPID/NEWUTS/NEWIPC/NEWNS/NEWTIME follow the same plan.
2. Namespace creation is authorized through `UnshareContext::cred`;
   `NEWUSER` needs no capability in the parent, while the other `NEW*` flags
   require `has_cap_in(caller_user_ns, CAP_SYS_ADMIN)`. The new user namespace
   (when requested) becomes the owner of every sibling namespace. Plain
   `CLONE_FS`/`CLONE_FILES` do not.
3. `NEWNS` while the `fs_struct` is shared and `CLONE_FS` was not requested is
   `InvalidArgument` (`EINVAL`), matching Linux `check_fs_flag()`.
4. `NEWNS` clones the mount tree at the current root/pwd and builds a private
   `FsStruct` retargeted into the clone; `CLONE_FS` without NEWNS detaches a
   private copy; `CLONE_FILES` is recorded for the commit phase.

The syscall layer supplies process facts through `UnshareContext`
(privilege and fs-sharing) and commits via `Process::apply_prepared_unshare`;
the descriptor-table detach then runs on the calling thread. Per Linux
`ksys_unshare`, the syscall layer adds `CLONE_FS` itself whenever `CLONE_NEWNS`
is requested, so callers never need the explicit bit; the `check_fs_flag`
rejection above remains as defense for direct `prepare_unshare` callers.

`NsProxy::with_namespace` builds a proxy with one namespace slot swapped,
used by `setns(2)`. User switches are committed through the credential path;
a Net slot swaps in the target `NetNamespace`
and the syscall path materializes its protocol stack via
`knet::netns::ensure_stack`. Socket creation, netlink dumps and the listen
table still resolve through knet's global singletons — per-namespace
data-plane routing is the follow-up layer — so a net namespace provides the
lifecycle boundary (creation, `setns` entry, `/proc/<pid>/ns/net` identity)
but not yet data-plane isolation.

## setns semantics

`setns(2)` in `core/ksyscall` authorizes each requested namespace type against
the target namespace's owner user namespace (Linux's per-type install checks)
and resolves an fd through two channels:

- `NsFd` channel: `nstype` must be zero or match the fd's namespace type
  (`EINVAL` otherwise). User swaps install the matching credential transition;
  mount swaps require `CAP_SYS_CHROOT`+`CAP_SYS_ADMIN`; time swaps reject a
  multithreaded caller with `EUSERS`; entering the current cgroup namespace is a
  successful no-op.
- `PidFd` channel (Linux 5.8+): `nstype` is an OR-combination mask; every
  requested slot is validated against the pidfd's live process after the ptrace
  read check, and unknown bits fail with `EINVAL` before anything commits.

Both channels build the replacement proxy with `with_namespace` (the pidfd
channel once per requested slot) and commit through `Process::apply_setns`;
`CLONE_NEWNS` additionally retargets the fs root/pwd into the target mount
namespace's visible root, drafted on a private `clone_for_process` clone so a
failed retarget commits nothing and no exec-transition marker is inherited.

## UTS data flow and concurrency

`UtsInner` owns two 65-byte `c_char` arrays. Setters reject names longer than 64
bytes, zero the array, then copy bytes; arbitrary byte values and embedded NUL
are accepted. Slice getters stop at the first NUL. `UtsNamespace` uses an
`RwLock<UtsInner>`; vector getters allocate a copy, while `read_names_into`
copies both full arrays into caller buffers under one read lock.

`NsProxy` is immutable after construction; its namespace objects provide their
own synchronization. Runtime pointer replacement belongs to `kprocess`.
Namespace IDs use the `kcred` allocator; cgroups have a separate ID type. ID
allocation is not a memory-ordering/publication protocol.

## Decisions, lifecycle, and limitations

Invalid or conflicting clone flags fail explicitly instead of silently claiming
isolation. Net, user, time, and cgroup namespaces are created and entered. Net
carries a private knet stack (loopback and `127.0.0.1/8`); user namespaces carry
credential and idmap state; cgroup namespaces carry a view rooted at the
caller's current cgroup, consumed by `cgroup2fs` and `/proc/<pid>/cgroup`.
Cgroup controller state and membership accounting remain in `kcgroup`; the
namespace does not fork the hierarchy. Net *data-plane* routing (socket
registry, listen table, netlink views) remains global as a documented
follow-up.
`NamespaceFsContext` makes mount-copy versus shared-filesystem conflicts visible
in the API. Credential and active-PID roles are not folded into `NsProxy`.

`NsProxy` itself has no custom drop: final `Arc` release drops the references
it holds. Net has a paired teardown (IPC additionally runs an optional drop observer):
`NetNamespace::drop` notifies the protocol-stack owner through the registered
hook so the private stack is released with the namespace. IPC and time are
ID-only types with no local resource manager teardown, except that `IpcNamespace::drop` runs the at-most-one observer registered through `register_ipc_ns_drop_hook` (a `ksync::Mutex`-protected map, callback invoked outside the lock), and IPC managers remain
global. `NsFd` pins the namespace for the fd lifetime, so a namespace survives
while any fd or bundle references it.

Non-goals and increments (explicitly out of scope, recorded because they echo
the tgoskits gaps):

- **Cgroup view scope**: `NEWCGROUP` creates a real `CgroupNamespace` view
  rooted at the caller's current cgroup; `cgroup2fs` and `/proc/<pid>/cgroup`
  consume it. Controller state and cgroup membership accounting remain owned by
  `kcgroup`; the namespace does not fork the hierarchy. User and time namespaces
  are implemented with credential/idmap and offset state; net *data-plane*
  routing stays global in knet as the documented follow-up.
- **fs-sharing counting**: the shared-`fs_struct` (`NEWNS` without `CLONE_FS`)
  check counts both the caller's published threads and other processes that
  adopted the same context through `CLONE_FS`; kernel threads do not own a
  filesystem context.
- **setns authorization**: each `setns` channel authorizes against the target
  namespace's owner user namespace, matching Linux's per-type install checks.
  `NsFd::User` also installs the matching credential transition; mount swaps
  require `CAP_SYS_CHROOT`+`CAP_SYS_ADMIN`; time swaps reject a multithreaded
  caller with `EUSERS`; cgroup swaps into the current view are a no-op. The PID
  install rule still requires the target to be the caller's active namespace or
  a descendant (`pidns_install`), which keeps plain-fork allocation keyed on
  `pid_ns_for_children` resolvable in the caller's namespace.
- **bind-mount lifecycle**: namespace files keep identity in their inode and
  can be bound, but re-opening a bound file after the owning process exits
  fails with `ESRCH`; a namespace registry (nsfs-style) is needed for the
  libnetwork bind/setns/GC lifecycle. `/proc/<pid>/ns/<type>` files are
  regular files whose readlink fails with `EINVAL` (documented deviation from
  Linux's nsfs magic symlinks, mirroring the Docker-netns precedent).
- **switch granularity is process-level**: `unshare(2)`/`setns(2)` commit the
  namespace bundle (and fs context) on the shared `ProcessRuntime`, so in a
  multithreaded process one thread's switch re-views the whole thread group.
  Linux switches only the calling task (`switch_task_namespaces(current)`).
  Moving nsproxy/fs to the task layer is a prerequisite for per-thread
  namespaces; until then this deviation is the accepted contract. Only the
  fd-table detach (`CLONE_FILES`) is thread-scoped.
- **single-writer commit**: `apply_setns`/`apply_prepared_unshare` perform an
  unsynchronized read-modify-write of the runtime bundle. Concurrent
  `setns`/`unshare` from sibling threads of one process is not serialized and
  not supported; the syscall surface assumes one committing caller at a time.

No complete mount-propagation or IPC implementation is implied by the presence
of a namespace reference.
