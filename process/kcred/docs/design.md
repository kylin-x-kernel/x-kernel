# kcred — Design

## Purpose and boundaries

`kcred` owns credential values, their checked UID/GID transitions, and Linux
capability state for X-Kernel. `Cred` stores real, effective, saved and
filesystem IDs, supplementary groups, effective/permitted/inheritable and
ambient capability sets, a bounding set, and the supported keep-capabilities
flags. `NamespaceId` and `UserNamespace` separately model namespace identity and
parentage.

The crate does not locate a current task, commit credentials, authorize file
access, read syscall pointers, or load executable metadata. `kprocess` owns task
credential snapshots and publication; `ksyscall` translates wire arguments and
applies syscall-target policy; `kvfs` consumes explicit credentials for
discretionary access checks. Capability authority is the effective set:
`Cred::has_cap` is the primitive, and named predicates such as
`has_sys_admin` and `has_sys_tty_config` are thin aliases for it. Set-ID
transitions test `CAP_SETUID`/`CAP_SETGID`; filesystem access checks in `kvfs`
test `CAP_DAC_OVERRIDE`/`CAP_DAC_READ_SEARCH`. The namespace types do not scope
capability sets.

```text
ksyscall -> kprocess: prepare / commit task snapshot
                 |
                 v
               kcred <--- kvfs: inspect explicit &Cred
```

## Source and dependency scope

This document covers the entire crate, including its namespace module and tests.

| Source | Role |
| --- | --- |
| `src/lib.rs` | Root re-exports and `INITIAL_CRED` / `initial_cred` |
| `src/credentials/mod.rs` | Private credential module and type re-exports |
| `src/credentials/caps.rs` | `CapSet`, `CapSets`, capability constants, Linux transition validation |
| `src/credentials/file_caps.rs` | `security.capability` wire codec (`FileCapabilities`, revision and flag constants) and write validation |
| `src/credentials/model.rs` | `Cred` storage, queries, capability and ID transitions |
| `src/credentials/user.rs`, `src/credentials/group.rs` | `Uid` and `Gid`, both `u32` aliases, `INVALID_UID`/`INVALID_GID` sentinels, and `is_valid_uid`/`is_valid_gid` predicates |
| `src/credentials/securebits.rs` | Crate-private `SecureBits` bitflags |
| `src/namespace.rs` | IDs, namespace parentage and initial namespace |
| `ut-tests/kcred_unittest.rs`, `ut-tests/namespace_unittest.rs` | Credential and namespace tests under `cfg(unittest)` |
| `Cargo.toml` | Dependency declarations; no crate-specific features |

`credentials` and `namespace` are private modules. The root exports `Cred`,
`Uid`, `Gid`, `INVALID_UID`, `INVALID_GID`, `is_valid_uid`, `is_valid_gid`,
`CapSet`, `CapSets`, the `CAP_*` constants including `CAP_LAST_CAP` and
`CAP_FS_MASK`, `FileCapabilities`, `SECURITY_CAPABILITY_XATTR`, the
`VFS_CAP_REVISION_*` and `VFS_CAP_FLAGS_EFFECTIVE` constants, `ExecIds`,
`ExecSecurity`, `PtraceAccessMode`, `NamespaceId`, `UserNamespace`,
`initial_cred`, and `initial_user_namespace`; there are no publicly named
submodules or exported macros. Securebits themselves remain crate-private and are exposed to user space
only through the `prctl(2)` adapter.

The crate is `no_std` and always uses `alloc` for `Arc` and `Vec`.
Its workspace dependencies are `bitflags`, `kerrno`, `klazy`, and `unittest`.
`kerrno::KResult`/`KError` provide transition results without another error type.
`klazy::Once` publishes the two initial singletons. The crate has no dependency
on `kprocess`, `ksyscall`, or `kvfs`, and no build script or optional alloc feature.

## Objects and interface roles

`Cred` has private `ruid/euid/suid/fsuid: Uid` and `rgid/egid/sgid/fsgid: Gid`
fields, a sorted `supplementary_groups: Arc<[Gid]>`, `capabilities: CapSets`,
`cap_ambient: CapSet`, `cap_bounding: CapSet`, and `securebits: SecureBits`.
The private layout keeps direct field mutation and capability validation within
the credential model.
`CapSets` also keeps its three sets private behind getters so callers cannot
construct a state that bypasses `validate_transition`. `Clone`/`prepare` copy
all scalar and capability fields and clone the shared group array. Queries
borrow state; mutation requires exclusive `&mut Cred` access.

Callers use constructors to create trusted credentials, checked setters to apply
identity policy, and `prepare` to obtain independently mutable values.
Construction and group replacement deliberately do not authorize publication.
`Cred::root` delegates to `Cred::new(0, 0)`; `initial_cred` initializes and clones
a shared `Arc<Cred>`. It is not a current-task lookup.

`for_access` clones a credential and substitutes filesystem IDs with real IDs.
Unless `SECURE_NO_SETUID_FIXUP` is set, it also applies Linux's access-override
effective-set rule: nonzero real UID clears effective, while real UID zero sets
effective equal to permitted. Permitted, inheritable, ambient, bounding and
securebits are preserved. The caller still performs the access check. A caller
implementing `AT_EACCESS` can use its existing committed credential without this
real-ID substitution. Normal `kvfs` operations inspect
filesystem IDs and supplementary groups. Keeping identity selection in `Cred`
avoids a duplicate access-only credential type and keeps VFS independent of task
lookup. `can_read_ptrace` selects the caller's real or filesystem IDs with
`PtraceAccessMode`, requires all target real/effective/saved IDs to equal the
chosen caller ID, and then requires the target's permitted set to be a subset
of the caller's permitted set in real-credentials mode or effective set in
filesystem-credentials mode. An effective `CAP_SYS_PTRACE` overrides both
conditions. `has_scheduler_capability_over` similarly requires an effective
`CAP_SYS_NICE` or target-permitted containment in caller-permitted.
`kprocess` and `ksyscall` compose these predicates with same-thread-group and
operation-specific policy; identity alone is not permission.

`UserNamespace` stores `id: NamespaceId` and `parent: Option<Arc<UserNamespace>>`.
Only the initial namespace can currently be constructed through public APIs;
its parent is `None`. There is no child constructor or ID-mapping operation.
`NamespaceId::new` and its `Default` implementation allocate numeric identities;
`as_u64` and `Display` expose their value. `Clone`, equality and hashing operate
on that value. The counter starts at one and wraps without exhaustion handling.
IDs convey identity, not an authorization decision or namespace membership.

## Credential lifecycle and publication

The integration sequence is:

1. `kprocess::CurrentThread::prepare_creds` snapshots its subjective credential
   and calls `Cred::prepare`.
2. A caller invokes `kcred` transitions on the uncommitted value.
3. After success, `CurrentThread::commit_creds` delegates to the task's
   publication mechanism. `Thread::commit_cred` holds both credential write locks,
   asserts that objective and subjective pointers match, creates an `Arc<Cred>`,
   and replaces both pointers.
4. Operations already holding an old `Arc<Cred>` continue to observe that value.

Steps 1, 3 and operation-wide snapshot selection are external responsibilities,
not guarantees made by `kcred` alone. Exclusive Rust access does not enforce
that a value is an uncommitted credential. Callers must preserve the publication
convention, especially when using `Arc::make_mut` or constructing new identities.
A complete pathname or permission operation should retain one snapshot instead
of repeatedly reading current credentials. Open-file owners may retain an `Arc`
for the file's lifetime; that lifetime policy also belongs to the consumer.

## Transition flows

There is no enum state machine; private scalar fields, capability sets, and
securebits represent the state. There is no generic `is_privileged` predicate
that treats EUID zero as a capability proxy: privileged operations test the
effective capability named by Linux. The sysctl/procfs compatibility paths that
Linux itself gates on EUID zero remain deliberate exceptions; they spell that
identity check as `Cred::has_root_euid`, which does not consult capabilities or
the filesystem UID.

Cross-process policies over two supplied credentials live here so that
capability-set containment has one implementation. `Cred::can_read_ptrace`
implements the Linux read-side ptrace check for `PTRACE_MODE_READ_REALCREDS` or
`PTRACE_MODE_READ_FSCREDS`: identity is compared against the caller's real or
filesystem IDs respectively, the target's permitted set must be contained in
the caller's permitted or effective set, and effective `CAP_SYS_PTRACE`
overrides both checks. `Cred::has_scheduler_capability_over` implements
`cap_safe_nice()`'s target-permitted containment. Neither method resolves a
task, applies the same-thread-group exemption, or supplies the separate
scheduler ownership test; those remain process/syscall responsibilities.

`set_capabilities` delegates all transition policy to
`CapSets::validate_transition`, passing the pre-transition capability and
bounding sets. The checks run in Linux `cap_capset()` order: without effective
`CAP_SETPCAP`, inheritable must be a subset of the old inheritable union old
permitted; inheritable must always be a subset of the old inheritable union the
bounding set; permitted must never grow; and effective must be a subset of the
new permitted set. Effective `CAP_SETPCAP` relaxes only the first rule. Ambient
is not a `CapSets` member but is maintained separately under the invariant
`ambient ⊆ permitted ∩ inheritable`; the transition code enforces it after
validation rather than passing it through `validate_transition`. Every
check precedes assignment, so rejection leaves the credential unchanged.
After a successful transition the ambient set is intersected with the new
permitted and inheritable sets, matching Linux `cap_capset()`, so capabilities
that the new sets no longer authorize are silently dropped.

`CapSet` can represent all 64 ABI bits, but `capset(2)` applies Linux's
`CAP_VALID_MASK` before calling this model, so unknown bits never enter a
credential through the syscall. The model's subset rules still operate on the
raw represented bits for trusted constructors. `drop_bounding_cap` takes the
full syscall argument width, checks `CAP_SETPCAP` before Linux's
`cap > CAP_LAST_CAP` range test, and only then removes the bit, so an oversized
value cannot truncate to a removable capability. Constructors are trusted:
`Cred::root()` gets full effective/permitted sets, empty inheritable and
ambient sets, and a full bounding set; `Cred::new(nonzero, ...)` gets empty
effective/permitted/inheritable/ambient sets and the same full bounding set.

Checked set-ID functions validate all requested IDs before mutating any fields.
`set_uid` and `set_gid` reject the invalid ID sentinel (`u32::MAX`) with
`KError::InvalidInput`. Disallowed changes return
`KError::OperationNotPermitted` with the value unchanged.
The crate-private `set_resuid_unchecked` and `set_resgid_unchecked` skip policy
checks and synchronize filesystem IDs with final effective IDs. They are safe
Rust functions; “unchecked” describes authorization, not raw-memory operations.
Public checked transitions call these internal helpers only after policy checks
or a privileged-path decision.

- `set_uid`/`set_gid` replace all four corresponding IDs when the effective set
  holds `CAP_SETUID`/`CAP_SETGID`; otherwise they update only effective/filesystem
  IDs to a real or saved ID.
- `set_reuid`/`set_regid` accept optional real/effective IDs. Their allowed sets
  and saved-ID update conditions are documented on the methods. Every successful
  call synchronizes filesystem IDs, even if both arguments are `None`.
- `set_resuid`/`set_resgid` check optional real/effective/saved IDs. A changed
  real/saved ID or an explicit effective ID differing from effective/filesystem
  state triggers the internal update helper. A true no-op preserves distinct
  filesystem IDs; `None` does not mean “use the current effective ID”.
- `set_fsuid`/`set_fsgid` return the old filesystem ID on both acceptance and
  rejection; they do not return a `Result` or expose a separate rejection flag.
- `set_supplementary_groups` sorts the owned `Vec` without deduplication, then
  replaces shared storage with `Arc::from(groups)`. `in_group` compares `fsgid`
  and binary-searches that sorted array. Caller privilege and count limits are
  not checked by this setter.
- `set_uid`, `set_reuid`, and `set_resuid` snapshot the old value and apply
  Linux's `cap_emulate_setxuid` fixup after a successful user-ID transition.
  When any old real/effective/saved ID was root and all new IDs are nonzero,
  effective and permitted are cleared unless `KEEP_CAPS` is set, and ambient is
  always cleared even when `KEEP_CAPS` is set; independently, leaving effective
  UID 0 clears effective, and entering
  effective UID 0 restores effective from permitted. `SECURE_NO_SETUID_FIXUP` suppresses the fixup. Inheritable and
  bounding sets are unchanged.
- `set_fsuid` applies only the `CAP_FS_MASK` half of Linux's setfsuid fixup:
  leaving filesystem UID 0 removes those bits from effective, and entering it
  restores them from permitted. Permitted/inheritable sets are unchanged and
  `KEEP_CAPS` does not affect this path. `SECURE_NO_SETUID_FIXUP` suppresses it.
  `set_fsgid` has no capability effect.
- `set_securebits` accepts the full syscall-width word, rejects any bit outside
  the supported mask before narrowing, and replaces the whole word only when no
  locked bit changes and no lock is cleared. Without effective `CAP_SETPCAP`, it
  additionally requires a real change confined to the modern
  `SECURE_EXEC_RESTRICT_FILE`/`SECURE_EXEC_DENY_INTERACTIVE` bits and their locks.
  These exec-check bits are stored and validated but not enforced by the kernel;
  the executing user-space checker interprets them. This method owns the width
  and lock policy; `ksyscall` only adapts the ABI.
- `raise_cap_ambient` validates the capability number, requires membership in
  both permitted and inheritable and requires `SECURE_NO_CAP_AMBIENT_RAISE` to
  be clear, then adds the bit. It deliberately does not test the bounding set,
  matching the inspected Linux implementations. `lower_cap_ambient` validates
  the number and removes the bit even when absent; `clear_cap_ambient` empties
  the set without a capability check. `ksyscall` validates `prctl(2)` argument
  width and unused arguments before calling these methods.
- `keep_caps_enable`/`keep_caps_disable` test `KEEP_CAPS_LOCKED` before changing
  `KEEP_CAPS`. `apply_exec(no_new_privs, fs_shared_with_other_process)` is the
  no-metadata spelling of `apply_exec_with_security(..., ExecSecurity::NONE)`.
  `ExecSecurity` carries what `execve(2)` sampled from the image: `ids`, the
  set-ID identities its mode bits request, and `file_caps`, the decoded
  `security.capability` sets. The transition installs `ids` first, then resets
  saved/filesystem IDs to the final effective IDs and computes the sets.
  Linux `handle_privileged_root()` classifies by the post-fill IDs: a non-real
  caller elevated to effective UID 0 by a set-UID file whose capability
  attribute exists (`__is_suid`) keeps the file formula — the file's permitted
  set bounded by the bounding set, unioned with the intersection of the old and
  file inheritable sets — and the file's effective flag. A real-root caller, or
  an effective-root caller whose file has no capability attribute, instead
  keeps the root reconstruction (bounding set plus the whole old inheritable
  set) and gains the effective set with effective UID 0; with a file attribute
  present it also keeps the file's effective flag when a set-UID bit moves the
  effective UID off zero. Without reconstruction or file capabilities, the
  permitted set starts empty.
  An effective file set the file formula cannot deliver fails the transition
  with `OperationNotPermitted`, mirroring `bprm_caps_from_vfs_caps()`; the check
  never consults the root reconstruction.
  Ambient is merged into permitted after the growth clamp and supplies
  effective when the other rules do not. It is cleared when Linux's set-ID
  condition holds — the post-fill effective UID or GID differs from the
  pre-exec real UID or GID — or when the file carries capabilities. Group
  membership does not exempt a set-GID target, and a pre-existing euid/egid
  mismatch already satisfies the condition.
  `apply_exec_with_security()` returns the `bprm->secureexec` flag or
  `OperationNotPermitted` when an effective file set cannot be delivered, so
  its callers must treat the result as fallible.
  Linux's downgrade path runs when permitted would grow or the set-ID condition
  holds, under either `no_new_privs` or cross-process `CLONE_FS` sharing. It always intersects the prospective permitted set with the pre-exec
  permitted set, and resets effective/group IDs to the real IDs when
  `no_new_privs` is set or the pre-exec credential lacks effective `CAP_SETUID`
  — the reset does not depend on the permitted set growing, so a set-UID or
  set-GID file under a shared filesystem context is normalized even when it
  grants no capability. `no_new_privs` makes the loader decline only the
  set-ID identity change, as Linux `bprm_fill_uid()` does; the file capability
  set is still sampled and applied, and the clamp above keeps it from growing
  the permitted set (which is also what clears ambient for a
  capability-bearing image). The transition clears
  `KEEP_CAPS` even when locked and preserves every other securebit plus the
  inheritable/bounding sets. Its return value is `bprm->secureexec`, sampled
  like Linux: the set-ID condition holds, or a non-real-root caller's exec
  either receives an effective set or ends with a permitted set wider than the
  new ambient set. A set-ID bit naming the caller's own identity is not an
  elevated exec on its own, and a real-root caller never gets the flag from
  the capability rules.
  Lock insertion is currently test-only.

## Execution context and concurrency

Existing-value queries and scalar transitions require no current process, CPU
pinning, scheduler, platform initialization, or device mapping. They perform no
sleeping I/O or callbacks. `prepare` and `for_access` clone `Arc` storage rather
than copying the group array. Constructors and group replacement allocate;
final `Arc` release may deallocate, inheriting the global allocator's context
requirements. Early-boot use of allocating APIs therefore needs heap setup.

The crate has no per-credential locks, but it is not synchronization-free:
`INITIAL_CRED` and `INIT_USER_NS` are `Once<Arc<_>>` singletons; `NamespaceId::new`
uses a function-local `AtomicU64` with `Ordering::Relaxed`. Atomic increment
provides distinct counts before wrap, not publication of other memory.
`Once` supplies singleton publication ordering and may spin while initialization
is in progress. Re-entering a singleton from an interrupt that preempts its
initializer can prevent completion. Initialize before such use; do not assume
that first access is IRQ-safe simply because it does not sleep.

`Cred`, group arrays and namespace objects gain `Send`/`Sync` automatically from
their fields; there is no unsafe trait implementation. Independent prepared
values can be mutated concurrently; one mutable value still requires exclusive
access. Task-level credential replacement is synchronized by `kprocess` locks.

## Decisions, limits, and cleanup

Immutable shared snapshots avoid holding task locks across filesystem operations;
sharing sorted group arrays makes credential preparation cheap and membership
queries logarithmic. Sorting retains duplicates because group-list output may
need them. This trades allocation at replacement time for cheap cloned snapshots.
A relaxed ID counter is sufficient for numeric allocation because namespace
publication is handled separately by ownership and `Once`.

Capability sets, including ambient, exist. The `security.capability` wire format
and its validation live in `file_caps`; `execve(2)` reads that attribute, the
executable's set-ID bits and the mount's `nosuid` flag before the transition and
passes the result as `ExecSecurity`. There are still no LSM hooks, user-ID
mappings or idmapped mount rules in this crate, so a version 3 file capability
owned by a foreign namespace is rejected, and set-ID elevation is
namespace-blind because `UserNamespace` is not stored in `Cred`: capability
authority remains global rather than namespace-scoped. `PR_SET_NO_NEW_PRIVS`
gates seccomp install and subjects exec identity changes and capability
reconstruction to the downgrade path; the loader still samples file
capabilities under it, because Linux applies them and relies on the clamp. `PR_SET_DUMPABLE` is still a stub, so a
privileged exec does not lower dumpability.
Supplementary-group bounds and syscall sentinel decoding belong to `ksyscall`.
Allocations are infallible API calls and are not mapped to a recoverable
`NoMemory` result. ID exhaustion is unchecked.

There is no custom `Drop`. Group replacement releases the old array reference;
the array is freed when the final owning credential releases it. Dropping a
namespace releases its parent reference. The two singleton statics retain their
initial `Arc`s for the kernel lifetime. `Once` poisoning after an unwinding
initializer is not recovered here. No credential or group data is securely erased.

`Cred::has_sys_tty_config` names VT configuration authority and tests
`CAP_SYS_TTY_CONFIG` in the effective set. TTY combines it with
controlling-terminal object identity; real/saved/filesystem UID zero and
file-open credentials do not grant authority.
