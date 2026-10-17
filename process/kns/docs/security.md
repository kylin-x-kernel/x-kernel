# kns security and reliability

## Scope, assets, and boundaries

All modules under `src/` are covered. Assets are namespace references, mount
view selection, namespace file descriptors (`NsFd`), UTS name arrays, and the
unshare two-phase validation. Kernel callers pass decoded flags, namespace
handles, filesystem contexts, copied UTS bytes, and privilege/sharing facts
through `UnshareContext`. Syscall adapters own user-pointer copying and
authorization. KVFS owns mount-tree visibility and lifetime; `fs_context` owns
process path references; other namespace owners retain their respective policy
responsibilities.

No direct user-pointer, MMIO, DMA, firmware, device/network packet, FFI, or
assembly input is consumed. UTS setters receive user-controlled byte content
through safe slices. Clone exchanges resolved paths with KVFS and updates the
caller's private filesystem context. `NsFd` is only constructed by procfs open
and pidfd resolution paths; readers and writers are rejected at the fd level.

## Unsafe inventory

The sole local unsafe boundary is `src/uts.rs::bytes_from_uts`, which calls
`slice::from_raw_parts` to borrow a `c_char` array prefix as `u8`.

Its private callers pass 65-element `UtsInner` arrays. The length is the first
NUL index or 65, hence within the same allocation. `c_char` is `i8` or `u8`, each
one byte and aligned like `u8`; every stored bit pattern is a valid `u8`.
The output lifetime borrows the original immutable slice, preventing mutation
or release while it is used. `UtsNamespace` callers hold the appropriate lock
while borrowing/copying. No ASCII premise is required: setters only check length.
There is no local unsafe trait impl or FFI/assembly operation.

## Memory invariants and thread safety

`Arc`s keep namespace objects alive. A pinned `NsFd` keeps the referenced
namespace alive for the fd's lifetime. NEWNS requires private root/pwd state and
retargets both paths before returning the bundle. Initial construction reuses
the canonical cgroup namespace. `UtsInner` setters preserve a zero terminator by
reserving the final array byte; embedded NUL is accepted and shortens slice views.

UTS mutable data is protected by its `RwLock`; bundle mutation/publication locks
are owned by process runtime. Mount copy delegates locking to KVFS. Callers
should prepare the new bundle without holding locks that reentrant VFS work
could need, then publish the completed pointer through their runtime protocol.
`NsFd` file opens take no bundle locks; they snapshot the process's current
namespace references.

## Threat analysis

| ID | Threat and asset | Severity | Trigger | Response and residual risk |
|---|---|---|---|---|
| T-01 | False namespace isolation | High | Caller requests unsupported NEW flags | `clone_for_child`/`prepare_unshare` reject `Unimplemented`/`Unsupported`; `with_namespace` rejects Net/User/Cgroup swaps; ID-only types do not prove manager isolation. Syscall policy and consumer state must be audited separately. |
| T-02 | Wrong root/pwd after mount copy | High | NEWNS is combined with shared filesystem state or paths are not retargeted | Shared state returns `InvalidFlagCombination`; `prepare_unshare` checks `fs_shared_with_others` + `CLONE_FS`; private copy uses KVFS retargeting and paired `replace_root_and_pwd`. Caller must supply initialized paths. |
| T-03 | UTS buffer overrun | High | Name length is 65 or greater | Setters return `NameTooLong` before mutation; safe copying and zero-filled arrays retain termination. |
| T-04 | Unauthorized hostname/namespace changes | High | Unprivileged request reaches trusted APIs | Authorization is external (syscall layer passes `UnshareContext::privileged`); these APIs check data/flag consistency, not caller credentials. |
| T-05 | Misinterpreted hostname text | Low | Name contains NUL or non-UTF-8 bytes | Stored bytes are preserved and slice reads stop at NUL; consumers must handle bytes rather than assume ASCII or full-string round trips. |
| T-06 | Namespace ID collision | Medium | External `NamespaceId` counter wraps | No local wrap prevention; IDs are metadata, not a substitute for live object identity and authorization. |
| T-07 | Stale namespace fd after process exit | Medium | `NsFd` from a bound procfs file opened after the owner exited | procfs nodes upgrade a weak task reference and fail with `ESRCH`; the `NsFd` fd itself pins the namespace, so the same-fd pattern stays valid. Bind-after-exit requires a registry (documented non-goal). |
| T-08 | Unshare commit partial application | Medium | fd-table unshare fails after bundle install | `apply_prepared_unshare` installs the bundle before `unshare_fd_table`; a failure leaves the running process with the new bundle and the old `CLONE_FILES` intent. Callers should treat fd-unshare failure as process-fatal diagnostic. |

## Failure modes and effects (FMEA)

| ID | Failure mode | Cause | Local effect | System effect | Severity (1-4) | Handling |
|---|---|---|---|---|---|---|
| F-01 | Unsupported clone | Unimplemented namespace bit | Typed error | Child creation rejected | 3 | Caller translates `Unimplemented` to its ABI. |
| F-02 | Mount clone fails | KVFS copy/retarget error | `CloneNsError::Mount`/`UnshareError::Mount` | No child bundle or unshare | 3 | Preserve underlying `VfsError`. |
| F-03 | Premature initial construction | VFS mount namespace absent | `expect` panic | Process boot fails | 2 | Initialize VFS before `new_initial`. |
| F-04 | Private paths absent | NEWNS on uninitialized `FsStruct` | Root/pwd reader panic | Clone fails fatally | 2 | Construct a mounted private context first. |
| F-05 | Name too long | More than 64 input bytes | `NameTooLong`, no change | UTS update rejected | 3 | Report typed error. |
| F-06 | Unprivileged unshare | Missing CAP_SYS_ADMIN | `NotPermitted` | Syscall EPERM | 3 | Syscall layer maps to `OperationNotPermitted`. |
| F-07 | Shared-fs NEWNS | No `CLONE_FS` | `InvalidArgument` | Syscall EINVAL | 3 | Syscall layer maps to `InvalidInput`. |

## Failure handling, privacy, and limitations

This crate chooses typed errors, not errno. It has no retry/rollback manager
beyond ownership cleanup of partially constructed values. Allocation follows the
kernel allocator policy. Hostname/domainname and namespace IDs may reveal system
identity to consumers; this crate does not log them or handle other user payload.
User/capability, namespace-FD policy, net isolation, and full user/cgroup
namespace switching are external or documented non-goals; IPC/net/time payloads
are not owned here. `unshare` maps to `EOPNOTSUPP` for NEWNET/NEWUSER/NEWCGROUP/
NEWTIME, deliberately avoiding pseudo-isolation.

## Audit checklist

- Reject unsupported flags before constructing a child bundle or unshare.
- Enforce privilege and shared-fs rules during `prepare_unshare` only.
- Preserve private, initialized filesystem context for NEWNS.
- Keep UTS bounds, one-allocation borrowing, and byte semantics aligned.
- Preserve the single initial cgroup hierarchy reference.
- Ensure `setns` rejects Net/User/Cgroup swaps rather than switching references.
- Define owner, locks, authorization, and cleanup when adding namespace state.
