# kcred — Security and reliability

## Scope and trust model

This analysis covers the entire crate: `src/lib.rs`, `src/credentials/` including
`caps.rs` and `securebits.rs`, `src/namespace.rs`, and their registered tests. No
modules are excluded. The crate has no unsafe blocks, unsafe functions/traits/impls, raw
pointer operations, FFI, assembly, hardware registers, DMA, or boot metadata.
Its risks are authorization and identity consistency rather than an internal
raw-memory boundary. Dependency internals remain the responsibility of `alloc`,
`klazy`, `bitflags`, `kerrno`, and `kspin` (the map-state lock).

Protected assets are `Cred`'s real/effective/saved/filesystem UID/GID fields,
its sorted `Arc<[Gid]>` supplementary groups, the effective/permitted/inheritable
`CapSets`, the ambient `CapSet`, the capability bounding `CapSet`, the securebits
word (including the keep-capabilities, setuid-fixup and modern exec-check bits
and their locks), the credential's `user_ns`, each `UserNamespace`'s
`uid_map`/`gid_map` (authorized write-once resources), its `owner_euid`/`level`
and `setgroups_allowed` policy, shared initial root credentials, and namespace
ID identity.
Checked transitions restrict changes to those fields; capability predicates and
identity queries are inputs to external access-control decisions, not
authorization tokens by themselves.

The direct API boundary accepts typed integers, `Option<Uid/Gid>`, an owned group
vector, credential references, and `Vec<IdMapExtent>` values decoded from map
files. There is no direct user pointer or concrete file/network/device payload
entry. The syscall and procfs integrations can nevertheless pass untrusted
numeric identity and map requests into these APIs:

| Boundary | Data direction | Responsibility |
| --- | --- | --- |
| `ksyscall` credential adapters -> `kcred` | Decoded ID requests, copied supplementary groups, decoded capability-set replacements, ambient-capability subcommands, securebits words, and decoded `uid_map`/`gid_map`/`setgroups` payloads | `ksyscall` decodes its no-change sentinel, copies user memory, checks group count and group-change privilege, translates the capability wire format, validates ambient subcommand arguments, and parses map extents. `kcred` validates capability ownership transitions, ambient raise preconditions, the full securebits width against the supported mask, lock rules and `CAP_SETPCAP`, enforces the checked setter rules below, and authorizes map writes with `new_idmap_permitted()` (write-once, parent coverage, `CAP_SYS_ADMIN`, owner self-map, and the root-map `CAP_SETFCAP` rule). |
| `kprocess` capability lookup -> `kcred` | Target process credentials or an error | `kprocess` resolves a positive PID through the caller's PID namespace to the target task and returns its objective real credential; when a thread-group leader has exited but not been reaped, it returns the credentials retained by the process identity instead of a surviving sibling's. `kcred` supplies the typed set snapshot. |
| Trusted constructors and group setter -> credential owner | New `Cred` or replacement group state | Caller authorizes construction/publication. `Cred::new`, `root`, and `set_supplementary_groups` are intentionally not privilege gates. |
| `kprocess` -> `prepare`/transitions -> `kprocess` | Snapshot copy, mutation result, replacement value | `kprocess` owns current-task lookup and synchronized publication; the syscall adapter commits only after successful updates. |
| Explicit `&Cred` -> `kvfs` or cross-task access policy | Filesystem IDs, membership, capability-set containment or an identity-predicate result | Consumer chooses the correct snapshot and target, applies any process-level exemption (such as same-thread-group ptrace), and combines the result with resource metadata and additional policy. `kcred` does not resolve tasks, open files, or grant ptrace access. |
| `ksyscall`/procfs map path -> `UserNamespace` | `Vec<IdMapExtent>` decoded from `/proc/<pid>/uid_map`, `gid_map`, and `setgroups` writes | `ksyscall`/procfs identify the target namespace and opener credentials; `kcred` checks one-shot state, parent-map coverage, writer capability in the parent, owner self-map rules, `setgroups` ordering, and the root-map `CAP_SETFCAP` rule. Rejection is `EPERM` (or `EINVAL` for malformed extents). |
| Namespace allocation -> namespace consumer | `NamespaceId`, raw/displayed ID, parent reference, `owner_euid`, capability query result | This crate supplies ID mapping and namespace-scoped capability decisions (`has_cap_in`); consumers must still treat a numeric ID alone as identity, not authority, and must pass the correct target namespace. |

`ksyscall` currently checks `sys_setgroups` size against 65536, rejects negative
sizes and invalid group-ID entries with `KError::InvalidInput`, checks privilege,
and validates/copies the user list before calling the unchecked-for-policy group
setter. These are external
controls in `core/ksyscall/src/task/credentials.rs`, not local `kcred` guarantees.
Likewise, validation of the numeric `PR_SET_KEEPCAPS` argument is outside this
crate; the keep-caps methods only check the lock bit. `set_securebits` owns
full-width validation and rejects unsupported bits, lock changes, and
unprivileged changes outside the exec-check bits. `capget`/`capset` wire-format
validation and target-PID
policy belong to `ksyscall` and `kprocess`; `kcred` does not read user memory or
decide which task a PID names.

## Authorization and state invariants

Authorization is capability-based: `Cred::has_cap` tests the effective set only
when the credential is in the initial user namespace (the global `capable()`
form); `Cred::has_cap_in(target_ns, cap)` performs the Linux `cap_capable()`
ancestor/owner walk, and the convenience predicates (`has_sys_admin`,
`has_sys_resource`, `has_sys_tty_config`, `has_cap_chown`, `has_cap_fowner`)
delegate to the global form. Real, saved and filesystem IDs never grant
capability authority. The effective set is
bounded by the permitted set, and `capset` transitions are validated by
`CapSets::validate_transition` before mutation. The transition validator also
keeps the inheritable set within the old inheritable set union the bounding set;
effective `CAP_SETPCAP` only relaxes the separate old-inheritable/old-permitted
restriction. The permitted set can never grow through `capset`. The syscall
boundary masks unknown capability words against `CapSet::FULL` (Linux
`CAP_VALID_MASK`) before validation, so bits above `CAP_LAST_CAP` (40) are never
installed or reported as held even though `CapSet` can represent them. Ambient
capabilities are explicitly stored and always kept within permitted and
inheritable; raising one additionally requires `SECURE_NO_CAP_AMBIENT_RAISE` to
be clear. File capabilities are modelled by `FileCapabilities`: the
`security.capability` attribute is decoded (revisions 1–3, undefined bits
clipped, version 3 limited to this kernel's only namespace root) and applied at
exec. `execve(2)` samples the executable's set-ID bits and file capability set;
`apply_exec_with_security` installs the sampled identities, bounds file
permitted capabilities by the capability bounding set, intersects the file
inheritable set with the old one, and reports `bprm->secureexec`. Without file
capabilities the privileged-root reconstruction is bounded by the existing
bounding and inheritable sets; Linux's set-ID condition (the post-fill effective UID/GID differs from
the real UID/GID) or a file capability set clears ambient, and ambient is merged into permitted after the growth clamp and becomes
effective when the other rules do not. `no_new_privs` declines only the set-ID identity sample; file capabilities are
still sampled and applied, and the transition prevents permitted from growing
beyond the pre-exec set.
All policy checks below use the pre-transition state. Checked setters returning
an error do not partially modify fields; publishing or discarding a prepared
object remains the caller's job.

| Operation | Acceptance condition | Rejection/result |
| --- | --- | --- |
| `set_uid`, `set_gid` | Valid ID, and `CAP_SETUID`/`CAP_SETGID` or target equals the corresponding real or saved ID | `InvalidInput` for the invalid ID sentinel or `OperationNotPermitted` without mutation otherwise |
| `set_reuid`, `set_regid` | `CAP_SETUID`/`CAP_SETGID`, or requested real ID is old real/effective and requested effective ID is old real/effective/saved; absent options are permitted | `OperationNotPermitted` before any field updates |
| `set_resuid`, `set_resgid` | `CAP_SETUID`/`CAP_SETGID`, or every supplied ID is in the corresponding old real/effective/saved set | `OperationNotPermitted` before mutation; allowed no-ops can preserve distinct filesystem IDs |
| `set_fsuid`, `set_fsgid` | `CAP_SETUID`/`CAP_SETGID`, or target is old real/effective/saved/filesystem ID | Old filesystem ID returned in all cases; rejected requests leave state unchanged |
| `set_capabilities` | Replacement satisfies all Linux `capset` rules: inheritable within old inheritable union old permitted unless effective `CAP_SETPCAP`, inheritable within old inheritable union bounding, permitted never grows, effective within new permitted | `OperationNotPermitted` before mutation; all three sets change together and ambient is intersected with the new permitted and inheritable sets |
| `raise_cap_ambient` | Capability at most `CAP_LAST_CAP`; member of both permitted and inheritable; `SECURE_NO_CAP_AMBIENT_RAISE` clear | `InvalidInput` above `CAP_LAST_CAP`; `OperationNotPermitted` before mutation otherwise |
| `lower_cap_ambient` | Capability at most `CAP_LAST_CAP` | `InvalidInput` above `CAP_LAST_CAP`; lowering an absent capability succeeds |
| `drop_bounding_cap` | Effective `CAP_SETPCAP`; full-width capability argument at most `CAP_LAST_CAP` | `OperationNotPermitted` before range validation, then `InvalidInput`; bounding set only |
| `set_securebits` | Only `SECURE_ALL_BITS`/`SECURE_ALL_LOCKS`; no locked bit changes and no lock is cleared; with effective `CAP_SETPCAP` any such change is accepted, without it only a real change confined to the modern exec-check bits and their locks | `OperationNotPermitted` before mutation for unsupported bits, lock changes, or unprivileged changes outside the exec-check mask |
| `keep_caps_enable`, `keep_caps_disable` | `KEEP_CAPS_LOCKED` is clear | `OperationNotPermitted` even for an otherwise redundant requested value |
| `set_supplementary_groups` | All typed group vectors accepted locally | Sorts, preserves duplicates and replaces the array; no authorization/count rejection |
| `apply_exec_with_security` (and the `apply_exec` shorthand) | Trusted caller invokes after successful exec with the thread's `no_new_privs` state, whether another process shares its filesystem context, and the `ExecSecurity` sample (`ids` from the executable's set-ID bits, absent under `no_new_privs`; `file_caps` from `security.capability`, still sampled under `no_new_privs`; both empty on a `nosuid` mount) | Installs the sampled identities, then brings saved/filesystem IDs to the final effective IDs and computes the sets. Linux `handle_privileged_root()` classifies by the post-fill IDs: a non-real caller elevated to effective UID 0 by a set-UID file that carries capabilities (`__is_suid`) gets the file formula (file permitted bounded by the bounding set plus the old/file inheritable intersection) with effective following the file's flag; any other real/effective root caller gets the reconstruction from bounding/inheritable, forced effective only with effective UID 0, and keeps the file's effective flag when a set-UID bit moves that UID off zero; otherwise permitted starts empty. An effective file set the file formula cannot deliver is `OperationNotPermitted`. Ambient is merged after the growth clamp, supplies effective when the other rules do not, and is cleared when Linux's set-ID condition holds (the post-fill effective UID or GID differs from the pre-exec real UID or GID) or when the file carries capabilities. When permitted would grow or the set-ID condition holds under `no_new_privs` or unsafe `CLONE_FS` sharing, the prospective permitted set is intersected with the pre-exec set and effective/group IDs reset to real IDs when `no_new_privs` was set or the pre-exec credential lacked effective `CAP_SETUID`, independent of growth. It clears `KEEP_CAPS` even if locked, preserves every other securebit plus the inheritable/bounding sets, and returns `bprm->secureexec`, which is true when the set-ID condition holds, or for a non-real-root caller whose exec grants an effective set or leaves permitted wider than the new ambient set |

UID-changing setters snapshot the old credential before mutation and apply
Linux's `cap_emulate_setxuid` rules. If any old real/effective/saved UID was zero
and all three new UIDs are nonzero, effective and permitted are cleared unless
`KEEP_CAPS` is set; inheritable is unchanged. Independently, leaving effective
UID zero clears effective, and entering effective UID zero sets effective equal
to permitted. `SECURE_NO_SETUID_FIXUP` suppresses both the setuid and setfsuid
fixups. The bounding set never changes. `set_fsuid` changes only the
filesystem-capability subset of effective: `CAP_FS_MASK` is removed when fsuid
leaves zero and re-added from permitted when it returns to zero; `KEEP_CAPS` has
no effect on that transition. GID changes do not modify capabilities.

Private group fields and sorting at the replacement entry establish the order
required by `in_group`'s binary search. `prepare` shares the immutable array but
copies scalar state, so changing a prepared value does not mutate its source.
`for_access` sets filesystem IDs to the real IDs in a copy. Unless
`SECURE_NO_SETUID_FIXUP` is set, it then applies Linux's access-override
effective-set rule: a nonzero real UID clears effective, while real UID zero sets
effective equal to permitted. Permitted, inheritable, ambient, bounding and
securebits are preserved. Its caller must still perform DAC and decide whether
real-ID access semantics are wanted.

`can_read_ptrace` selects the caller's real or filesystem IDs according to
`PtraceAccessMode`, compares all target real/effective/saved IDs with that one
chosen identity, and then requires target-permitted containment in the caller's
permitted or effective set. An effective `CAP_SYS_PTRACE` overrides both
conditions. `has_scheduler_capability_over` requires effective `CAP_SYS_NICE` or
target-permitted containment in caller-permitted. These predicates exclude
filesystem IDs in the real-credentials case, groups and securebits; a true result
is only one input to a caller-owned access decision.

## Concurrency and lifetime

Ordinary values use Rust borrowing and automatically derived thread-safety;
there are no manual `Send`/`Sync` implementations. `&mut Cred` ensures exclusive
mutation, but does not prove that the value is uncommitted or authorized.
External owners must enforce that policy. A malicious kernel caller can use
public constructors to create privileged values; this crate is not a sandbox
against arbitrary kernel code.

`INITIAL_CRED` and `INIT_USER_NS` use `klazy::Once` to publish shared `Arc`s.
The namespace ID counter uses relaxed `AtomicU64::fetch_add`, which provides
atomic count allocation but no synchronization of unrelated state. `Once` can
spin during initialization and panics after poisoned initialization. Reentry
from an interrupt that preempts an initializer can hang. Initial allocation and
final group/namespace deallocation inherit allocator context requirements.

Task credential lock ownership and operation-wide snapshot reuse belong to
`kprocess` and its callers. Keeping one `Arc<Cred>` stabilizes one value; separate
reads of current credentials are not made a transaction by this crate.

## Threat analysis

| ID | Threat and affected asset | Severity | Trigger | Existing controls and residual responsibility |
| --- | --- | --- | --- | --- |
| T-01 | Unauthorized UID/GID escalation | High | Unprivileged value requests an unrelated ID | Checked setters restrict old-ID target sets and reject before mutation. Constructors and group replacement remain trusted; caller must prevent unauthorized publication. |
| T-02 | Mixed identity during an access check | High | Consumer repeatedly snapshots current credentials or publishes intermediate state | `prepare` copies state and immutable `Arc` snapshots remain stable. `kprocess` locks publication; the consumer must retain one snapshot and commit only after success. |
| T-03 | Incorrect group membership or group-based escalation | High | Unsorted groups or unauthorized/unbounded replacement | Private array and `sort_unstable` establish lookup order. `ksyscall::sys_setgroups` owns privilege, count, and invalid-ID checks; direct kernel callers must provide equivalent policy. |
| T-04 | Incorrect real-ID or cross-task authorization | High | Caller uses effective IDs for real-ID access or mistakes the predicate for complete permission | `for_access` selects the access identity; `Cred::can_read_ptrace` applies the mode-specific identity, capability-subset and `CAP_SYS_PTRACE` rules; `Cred::has_scheduler_capability_over` requires effective `CAP_SYS_NICE` or target-permitted containment. Consumer still chooses snapshot, access mode and additional task/resource policy. |
| T-05 | Capability predicate treated as namespace isolation or complete permission | High | Consumer assumes `has_cap` alone authorizes a resource operation | `has_cap` is the global `capable()` form (initial namespace only); `has_cap_in(target_ns, cap)` implements the namespace ancestry/owner rules. Neither is a DAC decision by itself. Consumers must combine it with the correct resource metadata, ID namespace and operation-specific Linux rule. File capabilities are decoded here but user-ID mapping is absent, so a set owned by a foreign user namespace is rejected. |
| T-06 | Stale saved IDs, capabilities, or keep-capabilities after exec | High | Exec owner omits the transition or bypasses the intended lock semantics | `apply_exec_with_security` installs the sampled set-ID identities, then applies Linux's exec transition: a non-real caller elevated to effective UID 0 by a file with capabilities keeps the file formula and the file's effective flag; other real/effective root callers reconstruct permitted from bounding/inheritable, force effective only with effective UID 0, and keep the file's effective flag after a set-UID move off zero; ordinary non-root exec clears effective/permitted. Ambient is merged after the clamp, supplies effective when the other rules do not, and is cleared on Linux's set-ID condition or a file capability set. With `no_new_privs` or unsafe cross-process `CLONE_FS` sharing, a growing set or the set-ID condition enters the downgrade: the prospective permitted set is intersected with the pre-exec permitted set, and effective/group IDs reset to real IDs when `no_new_privs` is set or the pre-exec credential lacked effective `CAP_SETUID`, even without permitted growth. It clears `KEEP_CAPS` even when locked, preserves the inheritable/bounding sets and every other securebit, and set-ID/setfsuid setters apply the Linux fixups unless `NO_SETUID_FIXUP` is set. Leaving root via set-ID always clears ambient. Securebits replacement rejects locked changes and unprivileged non-exec-check changes; ordinary flag setters reject locked state. Correct placement of exec mutation/publication is external. |
| T-07 | Allocation or initialization prevents progress | Medium | Large direct group input, first-use allocation, poisoning, or interrupt reentry | External syscall count bound limits one source of input; `Once` coordinates normal initialization. No fallible allocation conversion, timeout or poison recovery exists here. Initialize in a suitable context. |
| T-08 | Namespace identity collision | Medium | `NamespaceId` counter exhausts its `u64` range | Atomic increment prevents concurrent lost updates before wrap. Exhaustion is not checked; the practical lifetime bound is an accepted limitation, not guaranteed eternal uniqueness. |

## Failure modes and handling

Severity: 1 fatal, 2 serious, 3 moderate, 4 minor.

| ID | Failure mode | Cause | Local effect | System effect | Severity | Handling |
| --- | --- | --- | --- | --- | --- | --- |
| F-01 | Checked transition refused | Target outside allowed set or keep-capabilities locked | Unchanged value, `KError::OperationNotPermitted` | Requested identity change fails | 3 | Propagate rejection and do not publish a failed update |
| F-02 | Filesystem-ID change refused | Target not permitted | Old ID returned, no mutation | Caller can misinterpret unchanged identity | 3 | Inspect resulting ID when confirmation is needed |
| F-03 | Allocation failure | Construction or group replacement under memory pressure | Allocator failure policy; no `NoMemory` return | May terminate kernel operation or kernel | 1 | Bound caller-controlled input; no local recovery path |
| F-04 | Singleton initialization cannot complete | Reentry or preemption of initializer by waiting context | Spin without progress | Execution path may hang | 2 | Establish initial objects before interrupt use; honor allocator/Once context requirements |
| F-05 | Singleton remains poisoned | Initializer unwinds | Later access panics | Shared initial identity unavailable | 2 | No recovery/reset API here; external failure policy applies |
| F-06 | Snapshot/policy misuse | Wrong identity source or premature publication | Semantically valid but unauthorized credential | Incorrect access decisions | 2 | Preserve owner-controlled prepare/commit and snapshot discipline |

There is no retry or fallback in checked transitions. Allocation and singleton
failures are not returned as `KResult` errors. UID/GID aliases accept every `u32`
bit pattern; no-change decoding is performed at the syscall boundary, not by
interpreting a magic integer in these setters.

## Privacy and limitations

The crate handles user/group identities and membership relations but no passwords,
keys or raw user buffers. It emits no production logs. Public queries and namespace
`Display` intentionally expose identity data to callers; namespace `Debug` can
expose identity/parentage. Consumers control whether such data reaches diagnostics
or users. Ordinary drop does not erase credential memory or group arrays.

User-namespace mapping, dumpability and publication policies are listed in
`design.md`; file capabilities and set-ID elevation are implemented here but
carry the namespace limitations recorded there. `UserNamespace` is not a `Cred`
field. The initial root credential is a global shared identity, never an implicit
substitute for a user's current credential. Supplementary-group allocation remains
infallible, and the namespace counter has no wrap rejection.

## Verification points

Existing `ut-tests/kcred_unittest.rs` cases cover rejected set-ID changes, saved-ID rules,
filesystem-ID no-ops, keep-capabilities locking/exec reset, capability bitmap
conversion, transition validation, bounding-set drops, ambient raise/lower/clear,
ambient masking, ambient preservation and group-change clearing across exec,
set-ID and setfsuid capability fixups, sorted groups with duplicates, stable
prepared snapshots,
access identity and asymmetric matching.
`ut-tests/namespace_unittest.rs` tests singleton identity, ID ordering and decimal display.
These tests do not validate all external task publication paths or counter wrap.
Use the platform defconfig and `make unittest UNITTEST_CRATE=kcred` to run the
registered kernel tests. Crate rustdoc also contains a runnable prepare/transition
example and is checked with the configured `make doc_check_missing` workflow.

Review changes against these concrete conditions:

- All checked rejections occur before mutation; allowed target sets use old IDs.
- Capability transitions and set-ID fixups use the old credential snapshot and
  mutate only the sets Linux specifies.
- Group replacement remains sorted and caller-authorized, with input bounded externally.
- Keep-capabilities locking and exec clearing retain their different semantics.
- Snapshot consistency and commit authorization stay with the owning task layer.
- Namespace identities are not presented as mappings or capability authority.
- Singleton use respects initialization, allocation and interrupt constraints.

`Cred::has_sys_tty_config` is the current-credential VT authorization predicate
and is backed by effective `CAP_SYS_TTY_CONFIG`. Consumers must pass the current
subjective credential and perform terminal-owner checks separately.
