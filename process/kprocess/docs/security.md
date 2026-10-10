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

Signal handler ownership is detached during last-thread cleanup before notifying
the parent, outside process-domain and membership locks. Runtime destruction
also detaches it for failed or abandoned unpublished children. Retaining a
manager for observation or prepared notification does not retain that ownership;
configuration operations reject inactive owners rather than recreating defaults.

Protected assets are stable PID/TID bindings, parent/child membership, exit
visibility, runtime mm/files/fs/ns ownership, objective/subjective credentials,
cgroup charge, signal recipients, timer generations and controlling-terminal
identity. Possession of a `Process` reference is not by itself syscall authority.

## External boundaries and validation responsibilities

| Boundary / entry | Inputs and direction | Local check / responsibility |
|---|---|---|
| `publish_user_task` | Kernel task carrying a user runtime into global visibility | Preparation asserts pointer-identical task/thread identity; cgroup reconciliation may fail; publish precedes activation. Caller must prepare a valid runtime and parent writeback. |
| Thread fork/clone | Typed clone policy, namespace flags and credential snapshot into runtime owners | Checks parent relation stability and Running state, delegates namespace/fs/mm/fd validation and cgroup charge; namespace InvalidFlagCombination/Unimplemented map to InvalidInput/Unsupported. |
| Signal facade | PID/TID/group and optional `SignalInfo` into ksignal/task interrupt | Unpublished targets return NoSuchProcess, while published zombies accept the send and discard delivery (Linux `kill` semantics); thread runtime mismatch returns OperationNotPermitted; optional TGID must match. Syscall signal permission checks remain external. |
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

The intrusive-list unsafe operation is in `src/process/tree.rs`,
`Process::remove_child_slot_from_parent_locked`: `children.remove(slot)` operates
on an intrusive list. Its SAFETY comment requires the process-domain lock to
serialize every child-list mutation, a membership check proving this exact slot
is in this exact parent's list, and absence of concurrent unlink between check
and removal. The write-guard parameter plus pointer-identity search provide the
local guarded path; `parent.children` is also locked during removal.

`src/process_runtime/runtime_state.rs::replace_current_address_space` is an
unsafe CPU-state handoff. Its caller guarantees a fresh user-empty MM preserving
kernel mappings, sleepable entry without locks or old-user access windows, and
no return to the old user context after success. Current live single-thread
membership and the live binding are checked before exchange. The complete new
binding retains its root; the displaced binding retains the old root through
activation. IRQ/preemption exclusion covers binding exchange, residency, ASID
preparation and ktask's saved/hardware root update. No MM mutex, allocation or
callback occurs inside that section; all displaced/candidate drops are outside.
The caller must reset old user registrations immediately after success, before
fallible new-image work or fatal cleanup. TEE reset stays at successful exec tail.
Signal delivery during sleepable old-MM teardown would violate this contract.

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

The process-MM binding uses `SpinNoIrq` because scheduling reads it with IRQs
disabled. `ProcessAddressSpace::new` acquires the MM mutex and allocates the heap
marker before the binding is published; it must run in sleepable context outside
spinlocks. Snapshot capture does not call this constructor. Binding critical
sections only copy identity, clone an active user or
structural observer, take the user slot, or update residency/ASID metadata.
Heap marker selection is protected by the same binding lock; numeric heap
updates use the existing release/acquire atomic ordering. `LiveAddressSpace`
captures user and heap together, preventing a retained MM view from consulting
another binding's heap marker. This is not an atomic snapshot of all VMAs or
all process resources. Its heap accessor takes no binding lock under the MM lock.
Never acquire the MM mutex, allocate, invoke callbacks, or drop an active user
under this lock. Last-user release and runtime destruction require sleepable
context outside the MM mutex. Scheduling may nest runqueue -> thread exit-snapshot
lock -> binding -> residency/ASID locks, never the reverse. A retained pin preserves object identity
after exit, but does not grant permission to access torn-down user mappings.

Membership retirement is preceded by one-way installation of `ExitAddressSpace`
in the exiting Thread. Pin, residency and AArch64 ASID context are captured from
one locked binding, preventing later binding changes from retargeting its root
or residency updates. Capture clones `MmPin` and metadata references, without
allocation, MM locking, user acquisition or owner release.
Snapshot presence, not the later exiting flag, selects scheduler metadata.
AArch64 prepares the retained context on each switch, preserving rollover.
No path clears or overwrites the snapshot before Thread destruction; TaskGC's
final drop occurs outside scheduler/queue locks after the task can no longer run.
Runtime lifetime alone is not evidence of old-root retention. The POSIX exit
caller must complete user cleanup before retirement and prevent later signal
frame writes/user return. The structural pin does not authorize either operation.
ASID root preparation changes CPU-local allocator state and must accompany a
matching hardware activation; read-only observations must not prepare a root.

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

Clone preparation initializes the child's blocked mask from the creating thread
before task publication or activation. `ksignal::ThreadSignalManager::set_blocked`
continues to remove SIGKILL/SIGSTOP. The copy does not transfer pending signals
or grant access to another thread's private queue, and adds no fallible step to
the existing preparation/publication rollback protocol.
Initial action configuration is passed by value; shared action ownership is
created only through the signal manager's fork operations. `kprocess` no longer
returns an independently retainable action-table lock to its callers.
Exec resets only this process's configuration, preventing a CLONE_SIGHAND peer
from losing its handlers or action metadata when another process execs.

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
- A restart-block must stay on the current `Thread`, start empty on clone,
  and be cleared on exec. Arm and take it only through `CurrentThread`.
  The payload type stays in the syscall adapter. Do not store it in the futex
  waiter table.

## Procfs Thread-count Exposure

The procfs accessor accepts a valid reference to the selected `Process` and reads
kernel-owned membership through the existing table and member-slot locks. It
accepts no user-provided count and does not depend on current process identity.
The per-slot scan neither authorizes access nor guarantees stable future
membership; publication can interleave with it. Callers must not treat it as an
atomic group snapshot or synchronization against clone/exit.

## Unreleased thread membership

The procfs lifetime count uses member-slot state, not the schedulable-task count.
ExitedLeader retains no strong task/runtime owner and grants no TID lookup or
signal delivery capability. Exact Weak task identity gates the exit transition
so an old task cannot retire a replacement binding. Reserved/rolled-back slots
remain Vacant; nonleader exit is released; reap/autoreap clears the retained
leader under the existing process-domain transaction. Exec does not change
member accounting, canonical TID, or publication in this patch. The existing
nonleader-exec identity gap and its post-exec count consequence are documented
in design.md; metadata must not be presented as proof of Linux-exact takeover.
Current runtime TIDs are checked monotonic allocations and are not recycled on
drop; exhaustion fails rather than wrapping. Fixed-number test identities do not
provide a production reuse path. Any future recycling policy must preserve leader
retention separately from replacement publication before changing that invariant.
The count is a slot scan, not a global counter or atomic snapshot. No new unsafe code or sleepable lock
is introduced, and directory metadata locks are not held across the query.
