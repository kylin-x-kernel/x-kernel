# kcred — Design

## Purpose and boundaries

`kcred` owns credential values and their checked UID/GID transitions for
X-Kernel. `Cred` stores real, effective, saved and filesystem IDs, supplementary
groups, and the supported keep-capabilities flags. `NamespaceId` and
`UserNamespace` separately model namespace identity and parentage.

The crate does not locate a current task, commit credentials, authorize file
access, read syscall pointers, or load executable metadata. `kprocess` owns task
credential snapshots and publication; `ksyscall` translates and validates user
arguments; `kvfs` consumes explicit credentials for discretionary access checks.
The namespace types do not currently participate in `Cred` privilege decisions.
`Cred::has_sys_admin` supplies the administrative capability decision separately
from the set-ID `is_privileged` predicate. Both currently use effective UID zero;
capability-set support must implement their respective semantics in this layer.

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
| `src/credentials/model.rs` | `Cred` storage, queries and transitions |
| `src/credentials/user.rs`, `src/credentials/group.rs` | `Uid` and `Gid`, both `u32` aliases |
| `src/credentials/securebits.rs` | Crate-private `SecureBits` bitflags |
| `src/namespace.rs` | IDs, namespace parentage and initial namespace |
| `ut-tests/kcred_unittest.rs`, `ut-tests/namespace_unittest.rs` | Credential and namespace tests under `cfg(unittest)` |
| `Cargo.toml` | Dependency declarations; no crate-specific features |

`credentials` and `namespace` are private modules. The root exports `Cred`,
`Uid`, `Gid`, `NamespaceId`, `UserNamespace`, `initial_cred`, and
`initial_user_namespace`; there are no publicly named submodules or exported
macros. Internal securebits are not part of the public API.

The crate is `no_std` and always uses `alloc` for `Arc` and `Vec`.
Its workspace dependencies are `bitflags`, `kerrno`, `klazy`, and `unittest`.
`kerrno::KResult`/`KError` provide transition results without another error type.
`klazy::Once` publishes the two initial singletons. The crate has no dependency
on `kprocess`, `ksyscall`, or `kvfs`, and no build script or optional alloc feature.

## Objects and interface roles

`Cred` has private `ruid/euid/suid/fsuid: Uid` and `rgid/egid/sgid/fsgid: Gid`
fields, a sorted `supplementary_groups: Arc<[Gid]>`, and `securebits: SecureBits`.
The private layout keeps direct field mutation within the credential model.
`Clone`/`prepare` copy scalar fields and clone the shared group array.
Queries borrow state; mutation requires exclusive `&mut Cred` access.

Callers use constructors to create trusted credentials, checked setters to apply
identity policy, and `prepare` to obtain independently mutable values.
Construction and group replacement deliberately do not authorize publication.
`Cred::root` delegates to `Cred::new(0, 0)`; `initial_cred` initializes and clones
a shared `Arc<Cred>`. It is not a current-task lookup.

`for_access` clones a credential and substitutes only filesystem IDs with real
IDs. The caller still performs the access check. A caller implementing
`AT_EACCESS` can use its existing committed credential without this real-ID
substitution. Normal `kvfs` operations inspect
filesystem IDs and supplementary groups. Keeping identity selection in `Cred`
avoids a duplicate access-only credential type and keeps VFS independent of task
lookup. `matches_real_credential_ids` compares the caller's real UID/GID against
all three real/effective/saved IDs of the target (`self`). This asymmetric
predicate excludes filesystem IDs, groups, task identity and capability bypass;
`kprocess` composes it into its own cross-task access policy.

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

There is no enum state machine; private scalar fields and securebits represent
the state. `is_privileged` is exactly `euid == 0`, standing in for the not-yet
implemented capability policy for set-ID operations.

Checked set-ID functions validate all requested IDs before mutating any fields.
Disallowed changes return `KError::OperationNotPermitted` with the value unchanged.
The crate-private `set_resuid_unchecked` and `set_resgid_unchecked` skip policy
checks and synchronize filesystem IDs with final effective IDs. They are safe
Rust functions; “unchecked” describes authorization, not raw-memory operations.
Public checked transitions call these internal helpers only after policy checks
or a privileged-path decision.

- `set_uid`/`set_gid` replace all four corresponding IDs for a privileged value;
  otherwise they update only effective/filesystem IDs to a real or saved ID.
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
- `keep_caps_enable`/`keep_caps_disable` test `KEEP_CAPS_LOCKED` before changing
  `KEEP_CAPS`. `apply_exec` resets saved/filesystem IDs to effective IDs and
  clears `KEEP_CAPS`, preserving its lock. It does not inspect file mode bits or
  implement capability-set side effects. Lock insertion is currently test-only.

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

There are no capability sets, LSM hooks, file capabilities, user-ID mappings,
idmapped mount rules, or setuid/setgid executable handling in this crate.
`UserNamespace` is not stored in `Cred`. The root privilege approximation is
therefore not namespace-scoped authority. Supplementary-group bounds and syscall
sentinel decoding belong to `ksyscall`. Allocations are infallible API calls and
are not mapped to a recoverable `NoMemory` result. ID exhaustion is unchecked.

There is no custom `Drop`. Group replacement releases the old array reference;
the array is freed when the final owning credential releases it. Dropping a
namespace releases its parent reference. The two singleton statics retain their
initial `Arc`s for the kernel lifetime. `Once` poisoning after an unwinding
initializer is not recovered here. No credential or group data is securely erased.
