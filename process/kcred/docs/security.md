# kcred — Security and reliability

## Scope and trust model

This analysis covers the entire crate: `src/lib.rs`, `src/credentials/` including
`securebits.rs`, `src/namespace.rs`, and their registered tests. No modules are
excluded. The crate has no unsafe blocks, unsafe functions/traits/impls, raw
pointer operations, FFI, assembly, hardware registers, DMA, or boot metadata.
Its risks are authorization and identity consistency rather than an internal
raw-memory boundary. Dependency internals remain the responsibility of `alloc`,
`klazy`, `bitflags`, and `kerrno`.

Protected assets are `Cred`'s real/effective/saved/filesystem UID/GID fields,
its sorted `Arc<[Gid]>` supplementary groups, `SecureBits::KEEP_CAPS` and
`KEEP_CAPS_LOCKED`, shared initial root credentials, and namespace ID identity.
Checked transitions restrict changes to those fields; identity queries are inputs
to external access-control decisions, not authorization tokens by themselves.

The direct API boundary accepts typed integers, `Option<Uid/Gid>`, an owned group
vector and credential references. There is no direct user pointer or concrete
file/network/device payload entry. The syscall integration can nevertheless pass
untrusted numeric identity requests into these APIs:

| Boundary | Data direction | Responsibility |
| --- | --- | --- |
| `ksyscall` credential adapters -> `kcred` | Decoded ID requests and copied supplementary groups | `ksyscall` decodes its no-change sentinel, copies user memory, checks group count and group-change privilege. `kcred` enforces the checked setter rules below. |
| Trusted constructors and group setter -> credential owner | New `Cred` or replacement group state | Caller authorizes construction/publication. `Cred::new`, `root`, and `set_supplementary_groups` are intentionally not privilege gates. |
| `kprocess` -> `prepare`/transitions -> `kprocess` | Snapshot copy, mutation result, replacement value | `kprocess` owns current-task lookup and synchronized publication; the syscall adapter commits only after successful updates. |
| Explicit `&Cred` -> `kvfs` or cross-task access policy | Filesystem IDs, membership or identity-predicate result | Consumer chooses the correct snapshot and combines identity with resource metadata and any additional policy. `kcred` does not open files or grant ptrace access. |
| Namespace allocation -> namespace consumer | `NamespaceId`, raw/displayed ID, parent reference | Consumers must not treat a numeric ID as proof of authority or assume this crate supplies ID mapping. |

`ksyscall` currently checks `sys_setgroups` size against 65536, rejects negative
sizes with `KError::InvalidInput`, checks privilege, and validates/copies the user
list before calling the unchecked-for-policy group setter. These are external
controls in `core/ksyscall/src/task/credentials.rs`, not local `kcred` guarantees.
Likewise, validation of the numeric `PR_SET_KEEPCAPS` argument is outside this
crate; its two boolean-style methods only check the lock bit.

## Authorization and state invariants

`Cred::is_privileged` tests effective UID zero for the set-ID policy.
`Cred::has_sys_admin` is the separate CAP_SYS_ADMIN decision and currently also
uses effective UID zero; real, saved and filesystem IDs cannot grant it.
Consumers supply the current subjective snapshot. Capability sets and namespace
capability rules are not implemented; the two predicates must evolve independently. All policy checks below
use the pre-transition state. Checked setters returning an error do not partially
modify fields; publishing or discarding a prepared object remains the caller's job.

| Operation | Acceptance condition | Rejection/result |
| --- | --- | --- |
| `set_uid`, `set_gid` | Privileged, or target equals the corresponding real or saved ID | `OperationNotPermitted` without mutation otherwise |
| `set_reuid`, `set_regid` | Privileged, or requested real ID is old real/effective and requested effective ID is old real/effective/saved; absent options are permitted | `OperationNotPermitted` before any field updates |
| `set_resuid`, `set_resgid` | Privileged, or every supplied ID is in the corresponding old real/effective/saved set | `OperationNotPermitted` before mutation; allowed no-ops can preserve distinct filesystem IDs |
| `set_fsuid`, `set_fsgid` | Privileged, or target is old real/effective/saved/filesystem ID | Old filesystem ID returned in all cases; rejected requests leave state unchanged |
| `keep_caps_enable`, `keep_caps_disable` | `KEEP_CAPS_LOCKED` is clear | `OperationNotPermitted` even for an otherwise redundant requested value |
| `set_supplementary_groups` | All typed group vectors accepted locally | Sorts, preserves duplicates and replaces the array; no authorization/count rejection |
| `apply_exec` | Trusted caller invokes after successful exec | Saved/filesystem IDs become effective IDs; `KEEP_CAPS` cleared even if locked; no executable inspection |

Private group fields and sorting at the replacement entry establish the order
required by `in_group`'s binary search. `prepare` shares the immutable array but
copies scalar state, so changing a prepared value does not mutate its source.
`for_access` changes filesystem IDs only in a copy. Its caller must still perform
DAC and decide whether real-ID access semantics are wanted.

`matches_real_credential_ids` compares target real/effective/saved IDs with the
caller's real IDs, not corresponding fields in two credentials. Filesystem IDs,
groups and securebits do not participate. A true result is only one predicate
for a caller-owned access decision.

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
| T-03 | Incorrect group membership or group-based escalation | High | Unsorted groups or unauthorized/unbounded replacement | Private array and `sort_unstable` establish lookup order. `ksyscall::sys_setgroups` owns privilege/count checks; direct kernel callers must provide equivalent policy. |
| T-04 | Incorrect real-ID or cross-task authorization | High | Caller uses effective IDs for real-ID access or mistakes the predicate for complete permission | `for_access` and `matches_real_credential_ids` encode their specific identity rules. Consumer still chooses snapshot, access mode and additional task/resource policy. |
| T-05 | Root approximation treated as capability or namespace isolation | High | Consumer assumes `euid == 0` is scoped authority | No such isolation is implemented. Current policy is explicitly limited to root-based checks; consumers requiring capabilities or ID mapping cannot obtain them from these APIs. |
| T-06 | Stale saved IDs or keep-capabilities after exec | High | Exec owner omits the transition or bypasses the intended lock semantics | `apply_exec` resets saved/filesystem IDs and clears KEEP_CAPS; ordinary flag setters reject locked state. Correct placement of exec mutation/publication is external. Capability-set side effects are absent. |
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

Unsupported capabilities, namespace mapping, executable privilege bits and
publication policies are listed in `design.md`. `UserNamespace` is not a `Cred`
field. The initial root credential is a global shared identity, never an implicit
substitute for a user's current credential. Supplementary-group allocation remains
infallible, and the namespace counter has no wrap rejection.

## Verification points

Existing `src/tests.rs` cases cover rejected set-ID changes, saved-ID rules,
filesystem-ID no-ops, keep-capabilities locking/exec reset, sorted groups with
duplicates, stable prepared snapshots, access identity and asymmetric matching.
`src/namespace.rs` tests singleton identity, ID ordering and decimal display.
These tests do not validate all external task publication paths or counter wrap.
Use the platform defconfig and `make unittest UNITTEST_CRATE=kcred` to run the
registered kernel tests. Crate rustdoc also contains a runnable prepare/transition
example and is checked with the configured `make doc_check_missing` workflow.

Review changes against these concrete conditions:

- All checked rejections occur before mutation; allowed target sets use old IDs.
- Group replacement remains sorted and caller-authorized, with input bounded externally.
- Keep-capabilities locking and exec clearing retain their different semantics.
- Snapshot consistency and commit authorization stay with the owning task layer.
- Namespace identities are not presented as mappings or capability authority.
- Singleton use respects initialization, allocation and interrupt constraints.
