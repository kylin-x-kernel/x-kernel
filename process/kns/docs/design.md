# kns design

Kernel unit tests live in `ut-tests/` and are mounted from their original
source modules with `unittest::path_mod_test!` under `cfg(unittest)`.

## Purpose and background

`kns` bundles process namespace references in `NsProxy`. Process runtime code
owns/replaces that bundle; syscall code parses flags and authorizes operations.
Mount trees belong to `kvfs::MntNamespace`, process paths to
`fs_context::FsStruct`, task-active PID identity to `kidentity`, credential/user
namespace semantics to `kcred`, and cgroup hierarchy/controller state to
`kcgroup`. `NsProxy` does not duplicate those owners.

## Scope and architecture

`src/lib.rs` exports modules `error`, `ipc`, `net`, `nsproxy`, `pid`, `time`,
`types`, and `uts`, defined in their corresponding `.rs` files. It also
re-exports `CgroupNamespace`, `NamespaceId`, `UserNamespace`, `MntNamespace`, and
`PidNamespace` from their owner crates.

```text
process runtime -> Arc<NsProxy>
     +-> mnt_ns: Arc<MntNamespace>
     +-> uts_ns: Arc<UtsNamespace> -> RwLock<UtsInner>
     +-> ipc_ns: Arc<IpcNamespace> -> NamespaceId
     +-> pid_ns_for_children: Arc<PidNamespace>
     +-> net_ns: Arc<NetNamespace> -> NamespaceId
     +-> cgroup_ns: Arc<CgroupNamespace>
     +-> time_ns / time_ns_for_children: Arc<TimeNamespace>
```

`NamespaceFlags` represents Linux creation bits. `NamespaceType` supplies names
for namespace displays. IPC/net/time types hold IDs, not their managers. The
current `posix-ipc` implementation still uses global message/shared-memory
managers without keying them by this IPC identity. Creating a new IPC namespace
ID therefore does not currently isolate those resources. This crate does not
own their queues, segments or manager migration.

## Execution context

Bundle creation/cloning and UTS operations require a working allocator and
sleepable locks and are intended for normal task context. They are not interrupt
or atomic-context primitives. `new_initial` requires boot to have installed the
initial VFS mount namespace; the explicit-mount constructor avoids that global
lookup. There is no current-task lookup, CPU-local state, or MMIO here.
For NEWNS, callers provide exclusive access to a private `FsStruct` whose root
and pwd are already initialized, and must perform flag authorization themselves.

## Construction and clone flow

`new_initial` caches a bundle with `Once`. The explicit-mount constructor creates
fresh UTS/IPC/net/time objects, shares the global root PID namespace, and reuses
`CgroupNamespace::initial`. Initial current/child time references point at the
same object. Independent explicit-mount bundles still share the initial cgroup
hierarchy so boot mounts and PID 1 do not acquire separate system roots.

`clone_for_child` first rejects NEWNET, NEWUSER, NEWCGROUP, NEWPID, and NEWTIME
with `Unimplemented`. With NEWNS it rejects `NamespaceFsContext::Shared`;
otherwise it snapshots root/pwd, calls KVFS `clone_with_root_and_pwd`, retargets
the private `FsStruct` as a pair, and retains the new mount namespace. These
failures are returned as `CloneNsError::Mount`.

NEWUTS copies both names under the parent UTS read lock into a new identity.
NEWIPC creates a new namespace ID. Unselected references are shared. The result
is a complete `Arc<NsProxy>` for the process owner to publish. This function does
not install it into a running process or implement `setns`/`unshare`.

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

Unsupported clone flags fail explicitly instead of silently claiming isolation.
`NamespaceFsContext` makes mount-copy versus shared-filesystem conflicts visible
in the API. Credential and active-PID roles are not folded into `NsProxy`.

There is no custom bundle drop: final `Arc` release drops references and the
respective namespace owners clean their resources. IPC/net/time ID-only types
have no local resource manager teardown. NEWPID/NEWNET/NEWUSER/NEWTIME/NEWCGROUP,
namespace-FD/setns policy, and full user namespace capability checks are not
implemented by this crate. No complete mount-propagation or IPC implementation
is implied by the presence of a namespace reference.
