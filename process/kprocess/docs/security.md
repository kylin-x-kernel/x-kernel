# kprocess — Security and reliability

## Scope and trust model

This analysis covers the entire crate, including `process/`, `process_runtime/`,
`thread/`, public facade files and tests. Syscall decoders are outside scope and
own ABI validation and operation-specific authorization. Trusted kernel callers
must pass matching task/thread `PidHandle` identities and obey publication,
current-thread, lock and cleanup contracts. `kidentity` supplies number identity;
`kcred` supplies immutable credential values; `memspace` supplies mm-user lifetime
and mapping invariants; `ksignal`, `kcgroup`, `kns`, `kresources` and filesystem
providers supply their respective state and validation.

Protected assets are stable PID/TID bindings, parent/child membership, exit
visibility, runtime mm/files/fs/ns ownership, objective/subjective credentials,
cgroup charge, signal recipients, timer generations and controlling-terminal
identity. Possession of a `Process` reference is not by itself syscall authority.

## External boundaries and validation responsibilities

| Boundary / entry | Inputs and direction | Local check / responsibility |
|---|---|---|
| `publish_user_task` | Kernel task carrying a user runtime into global visibility | Preparation asserts pointer-identical task/thread identity; cgroup reconciliation may fail; publish precedes activation. Caller must prepare a valid runtime and parent writeback. |
| Thread fork/clone | Typed clone policy, namespace flags and credential snapshot into runtime owners | Checks parent relation stability and Running state, delegates namespace/fs/mm/fd validation and cgroup charge; namespace InvalidFlagCombination/Unimplemented map to InvalidInput/Unsupported. |
| Signal facade | PID/TID/group and optional `SignalInfo` into ksignal/task interrupt | Missing/nonlive targets return NoSuchProcess; thread runtime mismatch returns OperationNotPermitted; optional TGID must match. Syscall signal permission checks remain external. |
| `ptrace::check_read_real_creds_access` | Caller/target threads into access decision | Same-process access succeeds; otherwise caller real IDs must match target real/effective/saved IDs or caller must be privileged. Failure returns OperationNotPermitted; effective UID zero is the current privilege approximation. |
| Cgroup migration | Target group and authorization callback into membership transaction | Requires published members/source, runs callback under stable process cgroup gate, then delegates atomic group migration. Callback decides filesystem-owned authorization. |
| Current-thread and credential helpers | Current TaskInner into a Thread/credential view | Dereference/downcast requires a user runtime. `current_fs_context` alone falls back to initial fs for kernel tasks. Commit asserts no subjective override before replacing both credential pointers. |
| `Process::address_space` / mapping closure | Runtime active-mm handle into MM operations | Refuses missing runtime/mm user with NoSuchProcess. `LiveAddressSpace` retains an active user; MM owns address/range checks. |
| Thread robust/clear-child pointer setters | User-originated addresses stored as integers | No dereference/validation here; `posix-process` performs fault-aware access and bounded robust cleanup on exit. |
| Exec metadata and timer APIs | Kernel-prepared path/argv/heap/timer configuration into runtime | Caller/loader validates image metadata; ktimer validates timer requests. Missing runtime returns NoSuchProcess; close-on-exec errors propagate. |
| Pidfd construction | Stable process and explicit credentials into anonymous file | OpenFlags rejects unknown bits; typed private data checks reject other file kinds. Pidfd live queries reject exited identity while poll retains exit visibility. |
| TEE/TIPC callbacks (feature-gated) | Typed context/handle access across provider boundary | Owner locks protect contexts; closure callers must preserve provider invariants and avoid recursive locking. |

No direct user-memory read/write, device MMIO/PIO, DMA, firmware parsing or
network-buffer parsing is implemented here. External data can still be present
in paths, argv, signals and stored user addresses. Absence of local raw accesses
is not an assertion that these values are trusted or that syscall authorization
is complete.

## Unsafe inventory

The explicit unsafe operation is in `src/process/tree.rs`,
`Process::remove_child_slot_from_parent_locked`: `children.remove(slot)` operates
on an intrusive list. Its SAFETY comment requires the process-domain lock to
serialize every child-list mutation, a membership check proving this exact slot
is in this exact parent's list, and absence of concurrent unlink between check
and removal. The write-guard parameter plus pointer-identity search provide the
local guarded path; `parent.children` is also locked during removal.

There are no explicit unsafe functions/traits/impls, FFI declarations or inline
assembly elsewhere in this crate. The intrusive-list macro/provider and task/MM
integration still have external safety contracts; this inventory covers explicit
source boundaries rather than claiming generated/provider code has no unsafe.

## Memory and lifecycle invariants

- A published task must carry the same stable handle as its Thread. Numeric ID
  equality alone does not protect against reuse; retire/delete paths check object
  identity and current slot state.
- Parent link, child list, exit signal and reserved reparent slot form one
  relation. Mutate them under the domain write lock, never by independent updates.
- Running → Zombie/Dead and Zombie → Dead consumption are domain transactions.
  Acquire-load exited flags are advisory for compound decisions.
- Publication may retain strong Process identity after runnable threads vanish.
  Runtime availability and lifecycle liveness are separate; do not use Weak
  upgrade as authorization to target an exited process.
- `LiveAddressSpace` owns an active mm user. Pins/observers only retain metadata;
  final-user release clears user mappings. Runtime resource/fs/ns slots are
  detached before parent-visible exit by the lifecycle caller.
- Group and session publication must track installed group membership; rollback
  must retire only transaction-owned identities and not reused slots.
- Credential values are immutable snapshots. Per-thread commit takes real then
  subjective locks, verifies no override and replaces both with one new Arc.

## Concurrency and context

Publication takes cgroup gate, table write lock, domain write lock, then local
spin locks. Exit/reap release domain before structural table deletion. Readers
snapshot table slot references before taking domain read. Never acquire a
sleepable lock or call signal preparation/destruction under the domain spin lock.
Cgroup authorization and scheduler validation closures execute under their
sleepable owner locks; TEE callbacks do likewise. They must not reenter the same
lock. Current-user helpers are not IRQ/early-boot/kernel-task APIs; initialization
and allocator/scheduler/clock assumptions are explicit in design.md.

Per-thread CPU accounting uses `SpinNoPreempt` for non-sleeping scheduler updates
and synchronized remote samples; interrupt handlers must not access this state.
Aggregate counters are relaxed atomics.
Their snapshots are not transactionally tied to every directory/resource field.
`ExecMetadata` atomically replaces its pair, while separate getters can straddle
updates. Timer signal dequeue callbacks validate timer sequence through ktimer;
missing targets are ignored by delivery glue rather than reviving retired tasks.

## Threat analysis

| ID | Asset / threat | Severity | Trigger | Existing response and residual risk |
|---|---|---|---|---|
| T-01 | PID/TID reuse causes wrong-task removal or signal targeting | High | Old exit/rollback races a new numeric ID binding | Stable slots and Arc identity checks gate retirement/deletion; Reserved slots are not deleted as retired. Syscall target authorization remains external. |
| T-02 | Double reap or inconsistent parent relation | Medium | Multiple waiters, reparent and exit race | Domain write transaction scans, claims Zombie, detaches and retires identity once; callers rescan after failed claim. |
| T-03 | Intrusive-list removal corrupts memory | High | A foreign or concurrently unlinked slot is removed | Guard token, parent list lock and exact membership check enforce the documented unsafe preconditions. |
| T-04 | Runtime references retain resources through zombie lifetime | Medium | Parent observes completion before files/fs/ns/mm detach | Lifecycle caller must detach owners first; weak runtime identity avoids owning the runtime. Existing live capabilities can deliberately prolong active-mm lifetime. |
| T-05 | Credential confusion permits cross-task inspection | High | Caller/target credentials differ or subjective override exists | Ptrace checks real credentials and commit rejects override; UID-zero privilege approximation and absent full capability/dumpability model are residual limitations. |
| T-06 | Half-published fork becomes runnable | Medium | Parent writeback or cgroup migration fails | Staged publication and commit-before-activation; Drop rolls back. Already-exited/unusual rollback paths retain conservative state rather than deleting another identity. |
| T-07 | Exit signal exposes state before autoreap commits | Medium | Signal handler/waiter runs while parent contract changes | Prepare outside domain, revalidate parent/exit-signal and retry, commit state before signal queueing/wakeup. Failed notification is logged; UID fallback can reduce payload accuracy. |
| T-08 | Cgroup movement races child publication | Medium | Migration and clone operate concurrently | Process cgroup gate stabilizes authorization and reconciles unpublished membership; kcgroup owns group charge/migration invariants. |
| T-09 | Exec failure leaves partially updated runtime | Medium | Close-on-exec cleanup returns error | Error propagates before metadata pair publication; earlier timer/heap/signal changes are not rolled back. Caller must handle exec failure state appropriately. |

## Failure modes and handling

| ID | Failure mode | Cause | Local effect | System effect | Severity (1–4) | Controls |
|---|---|---|---|---|---|---|
| F-01 | Runtime/target disappears | Concurrent exit/detach or invalid numeric target | Capability/lookup returns NoSuchProcess | Syscall fails or delivery is skipped | 3 | Typed Result and current-context invariant panics where required. |
| F-02 | Fork preparation fails | Cgroup, namespace, MM, fd or identity preparation error | No runnable child | Caller sees clone error | 3 | AttachedForkProcess and publication rollback release prepared relations/charges. |
| F-03 | Parent contract changes during exit | Concurrent parent exit/reparent | Prepared signal is obsolete | Notification must be retried | 3 | Discard retry-safe preparation and resample before commit. |
| F-04 | Internal task identity or slot assertion fails | Trusted caller or publication invariant violation | Publication stops | Kernel panic | 1 | Assertions report violated trusted-caller invariants rather than accepting inconsistent identity. |
| F-05 | Signal delivery fails after exit commit | Parent runtime/target becomes unavailable | Notification may be absent | Parent relies on wait-event path | 3 | Log preparation/send failures and finish parent wait wakeup. |
| F-06 | Deferred slot cleanup remains | Conservative abort or retirement cleanup path | Directory storage persists | Extra retained metadata | 3 | Identity-matched hot cleanup plus explicit directory sweep; no broad PID-only deletion. |

Errors from MM/fd/namespace/cgroup/timer providers are propagated or explicitly
mapped. Timer delivery deliberately tolerates missing recipients. Current-helper,
initialization and publication assertions are invariant checks, not recoverable
user-input validation. No crash-recovery mechanism is provided.

## Privacy, limitations and verification

Executable paths, command lines, IDs, user addresses, credentials and CPU stats
can be exposed through procfs/signal/pidfd consumers. Those consumers own access
policy; this crate supplies snapshots and does not redact them. Debug/signal
logs include process IDs and signal names. Credential and payload memory is not
explicitly scrubbed on drop here.

Root/global PID projection, missing subreaper support, representative-thread
credentials, simplified ptrace privilege semantics, UID-zero child-exit fallback
and external syscall authorization are current limits. Resource-object or mm-ID
availability does not prove a live fd table or active mappings. Optional TEE/TIPC
paths require feature-specific validation.

`src/tests.rs` exercises publication, PID reuse, rollback, parent/wait/exit,
resource detach and related invariants. Inline tests cover ptrace checks, pidfd
exit readiness and CPU accounting. Test source existence is not execution proof.

## Audit checklist

- Check stable-handle identity at task construction/publication and reuse-safe cleanup.
- Keep every parent/list/state transition inside the process-domain transaction.
- Match intrusive-list unsafe assumptions to the exact checked list and slot.
- Preserve cgroup/table/domain lock order and defer callbacks/destructors.
- Keep resource detach before parent completion; distinguish live from published.
- Preserve objective/subjective credential lock order and override assertion.
- Check signal preparation retry safety and commit-before-wakeup ordering.
- Document and validate optional TEE/TIPC callbacks and architecture MM hooks.
