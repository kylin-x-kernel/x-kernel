# fs_context security and reliability

## Scope and trust model

This analysis covers `src/lib.rs`, the whole crate. Assets are the process root,
working directory, umask, exec flag, and process-owner count. Trusted KVFS/POSIX callers pass resolved
`kvfs::Path`s; they must perform pathname copying, permission checks, chroot and
namespace policy before updating the context. No raw user pointer, MMIO, DMA,
firmware, FFI, assembly, or direct network input is consumed here.

## Unsafe inventory and invariants

There is no local unsafe code. `Path` ownership pins referenced VFS objects.
Directory checks precede replacement; paired replacement validates both inputs
before committing either one. Initialized readers require root/pwd to be present.
`replace_umask` masks unwanted bits. `begin_exec` checks the owner count and
sets `in_exec`, while `try_add_process_user` rejects adoption while that flag is
set; callers serialize both through the external context mutex. These checks do
not prove namespace membership or path ancestry, and `in_exec` serializes only
`CLONE_FS` adoption, not the whole exec transition.

## Thread safety

Path and exec state have no interior locking. Shared process access uses the
external `Mutex`; snapshots preserve path lifetimes after the lock is released.
The only interior shared state is the optional `Arc<AtomicUsize>` process-owner
count, allocated on first adoption and permitting lock-free release during
deferred finalization. Changes to the mutable state must be serialized with
callers' exec/namespace protocols.
Reentrant VFS work must not acquire the same context lock while it is already
held.

## Threat analysis

| ID | Threat and asset | Severity | Trigger | Response and residual risk |
|---|---|---|---|---|
| T-01 | Incorrect filesystem confinement | High | Caller installs an unauthorized root or a pwd in another tree | Setters check directories only; KVFS/POSIX authorization and paired namespace retargeting are required. This crate cannot establish confinement. |
| T-02 | Boot denial of service | Medium | Root/pwd read before mount attachment | `expect` prevents silent invalid state; boot must install paths before readers run. |
| T-03 | Cross-process path-state contamination | Medium | Caller shares a context when it should copy | `clone_for_process` provides an independent state object; `CLONE_FS` selection and flag validation belong to process/namespace code. |
| T-04 | Reentrant lock deadlock | Medium | Slow VFS work tries to reacquire the context lock | Clone path snapshots and unlock before I/O; direct callers must maintain that discipline. |
| T-05 | False or stale `CLONE_FS` sharing | Medium | Owner count is leaked, double-released, copied by a snapshot, or a child attaches during exec | Process runtimes adopt exactly one `ProcessUserGuard` and release it once; a snapshot has no owner count until adoption allocates an independent one. `begin_exec` and `try_add_process_user` are serialized by the context mutex, making the exec sharing test and adoption decision atomic with respect to each other. Guard destruction is a single atomic decrement and works in deferred finalization. Underflow indicates an ownership bug and panics rather than reporting a false sharing state. |

## Failure modes and effects (FMEA)

| ID | Failure mode | Cause | Local effect | System effect | Severity (1-4) | Handling |
|---|---|---|---|---|---|---|
| F-01 | Non-directory path | Bad caller target | `NotADirectory`, unchanged fields | Operation rejected | 3 | Validate before update; `new` instead panics on this error. |
| F-02 | Missing root | `set_pwd` before initialization | `InvalidInput` | Context update rejected | 3 | Attach root first. |
| F-03 | Uninitialized reader | Premature root/pwd access | Panic | Process startup may fail | 2 | Respect boot ordering. |
| F-04 | Old namespace paths retained | Clone without retargeting | Wrong filesystem view | Isolation semantics fail | 2 | `kns` clones mount tree and replaces both paths in private context. |

## Failure handling, privacy, and limitations

Fallible setters return `VfsResult` without partial paired updates. Constructors
and path readers documented as panicking have no recovery path here. Allocation
and referenced-object teardown follow their owning subsystem policies.

The object retains paths and permission metadata, not pathname text or file
contents, and emits no logs. Retained paths can reveal filesystem relationships
to authorized consumers. It does not enforce root/pwd ancestry, namespace
consistency, access permissions, or exec lifecycle transitions by itself.

## Audit checklist

- Preserve directory validation before mutations.
- Attach paths before initialized readers are exposed.
- Use `clone_for_process` for every private namespace replacement so it cannot
  inherit `in_exec`; snapshots copy state but never process ownership or allocate
  an owner count before adoption.
- Start an exec transition with `begin_exec` and end it on every path.
- Retarget root and pwd together on namespace copy.
- Keep the `process_users` guard balanced across runtime construction,
  replacement, and exit; never lock the filesystem context to release it.
- Release the context lock before slow/reentrant VFS work.
