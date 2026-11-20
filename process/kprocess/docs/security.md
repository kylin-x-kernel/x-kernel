# kprocess — Security and reliability

## Ordinary Signal Admission Errors

send_signal errors propagate before task interruption. Ordinary realtime owner
exhaustion returns WouldBlock instead of pretending delivery or invoking fatal
OOM. Only audited current-kernel sends explicitly permit bit-only fallback;
ABI SI_KERNEL does not grant that policy to userspace. Realtime child-exit
notifications use ordinary CLD_* admission: owner exhaustion is logged without
publishing a signal, while the independent child-exit wait notification still runs.
This does not make process construction, nested allocations or user accounting
memory-pressure safe. Kernel-generated fallback can lose sender/status details.

## Requeued Timer Wake Boundary

record_requeued accepts no numeric target authority. The timer manager verifies
the supplied reservation identity before returning its captured TimerSignalTarget.
The runtime releases the timer mutex before checking current disposition, pending state and
live slot identity, looking up a task, and validating matches_thread. Deletion,
ID reuse, expired managers and retired slots cannot redirect this wake to a new
owner. Concurrent consumption may make a wake redundant; it cannot create a
notification or acknowledge a timer. Ready callbacks execute outside all signal
and record locks. The CPU-timer integration test covers ignored retention,
accumulated overruns and one completion, not exhaustive wake/exit interleavings.

## Prepared Task Entry Ordering

Callers prepare the user-task entry before process/thread clone preparation.
`PreparedUserClone::into_parts` is therefore followed only by transferring the
entry owner, not a new fallible callback allocation. This avoids adding an error
path after the child-relation rollback guard has committed. The crate example
documents this order; syscall tests inject entry allocation failure and verify
unchanged child and thread relationships. They do not force global allocator OOM.

## Scope and trust model

Timer dispatch does not infer queue admission from a wake hint. Rejected
notifications return through TimerSignalSendOutcome and are released outside
timer, signal and handler ownership locks. Their receipts are not acknowledged
as delivered. Prepared receipt release queues record availability, never a
direct timer callback. The alarm task upgrades the captured runtime, verifies
exact storage identity under its timer mutex, and dispatches deferred demand
after unlocking. Neither a reused PID nor a reused timer ID authorizes progress.

TEE session-slot access exposes an optional KBox under the existing mutex.
The installing API accepts an already allocated owner; rejected redundant owners
are dropped after unlocking. Mutable slot callbacks retain the lock and must
respect its reclaim and non-reentrancy constraints. Taking an owner transfers
cleanup responsibility to the caller; allocation flags do not constrain its drop.

Signal-slot allocation failure unwinds thread preparation: the committed cgroup
membership is dropped and detaches, and owned files, credentials, runtime and
identity references are released. The slot itself is not registered on failure.
If the outer thread-owner allocation fails after slot registration, dropping
the supplied Thread retires the slot and releases its cgroup membership and
other resource owners. These destructors run in the preparing caller's context;
NOWAIT constrains owner allocation, not nested preparation or cleanup.
NOWAIT avoids new reclaim dependencies while the process cgroup transaction
mutex is held; it does not make nested Arc/Vec allocations fallible.
The clone publication regression checks that transferring a prepared `KBox`
into the task preserves the Thread address. Existing lifecycle tests cover
unpublished rollback and deferred runtime destruction, but do not force global
allocator OOM at the outer thread-owner allocation.
The fork runtime's final-stage callback runs before its attached-child guard is
committed. A deterministic NoMemory result from that stage tests process-tree
rollback, runtime and identity release, and shared-handler ownership cleanup
without requiring global allocator exhaustion. The parent remains usable.

Task publication slots retain `WeakKtaskRef` (DeferredWeak), not a destructor
completion token. Final strong release prevents resurrection immediately while
the normal worker may still retain the Thread runtime and its exit MM snapshot.
Visibility retirement and process-exit cleanup remain synchronous at their
explicit boundaries; neither is delegated to payload Drop. Task pointer identity
checks use KtaskRef/WeakKtaskRef, while Process and slot identities still use Arc.

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

Protected assets are stable PID/TID bindings, each task's immutable identity
token, parent/child membership, exit visibility, runtime mm/fs/ns and per-thread
files ownership, objective/subjective credentials, cgroup charge, signal
recipients, timer generations and controlling-terminal identity. Possession of a
`Process` reference is not by itself syscall authority.

Pidfd exit notifications use a `PollSet`, not stored completion tokens. Exit
state is published before waking observers; readiness comes from
`Process::is_exited()`, including after reaping. Poll callers must register and
recheck readiness before sleeping. Registration failure does not establish exit
or permit reclamation. Pidfds retain the process and notification source through
their existing Arc ownership.

## External boundaries and validation responsibilities

| Boundary / entry | Inputs and direction | Local check / responsibility |
|---|---|---|
| `publish_user_task` / `publish_user_task_with` | Kernel task carrying a user runtime into global visibility | Preparation asserts pointer-identical task/thread identity; cgroup reconciliation may fail; publish precedes activation. `publish_user_task_with` runs its initializer before the task is visible. Caller must prepare a valid runtime and parent writeback. |
| Thread fork/clone | Typed clone policy, namespace flags and credential snapshot into runtime owners | Checks parent relation stability and Running state, delegates namespace/fs/mm/fd validation and cgroup charge; namespace InvalidFlagCombination/Unimplemented map to InvalidInput/Unsupported. |
| Signal facade | PID/TID/group and optional `SignalInfo` into ksignal/task interrupt | Unpublished targets return NoSuchProcess, while published zombies accept the send and discard delivery (Linux `kill` semantics); thread runtime mismatch returns OperationNotPermitted; optional TGID must match. Syscall signal permission checks remain external. |
| `capability::target_capabilities` | Caller-provided PID plus the caller's explicit observer PID namespace; ksyscall `capget` consumer into kprocess credential provider | PID zero selects the caller; a positive PID is resolved through the observer namespace projection into the root-keyed live task index, then to the target's objective `real_cred` capability sets, including an unreaped exited leader's retained credentials. An invisible or unresolvable PID returns NoSuchProcess (`ESRCH`). Any caller may read these objective sets without ptrace authorization, matching Linux `capget`; subjective credentials are never reported. The syscall layer owns ABI/version validation and supplies its active namespace. |
| `PidFd::fdinfo_pid` | Captured thread identity token and PID namespace into fdinfo `Pid:` reporting | A thread pidfd reports its tid only while `lookup::task` resolves that tid to a task whose immutable identity token equals the captured one; exit, exec leader takeover or tid reuse yields `-1`. The token is never derived from user input and cannot be forged, so a recycled tid is never reported as the original thread. The value is additionally namespace-scoped: only an observer in the thread's captured namespace or an ancestor receives the root tid, and a child/sibling observer receives `0` as `pid_nr_ns` would; process pidfds keep their per-namespace projection lookup. The printed number is a snapshot of the pid handle the pidfd retains; it cannot be recycled to another task while the pidfd lives. |
| `ptrace::check_read_real_creds_access` | Caller/target threads into access decision | Same-process access succeeds; otherwise caller real IDs must match target real/effective/saved IDs and target permitted capabilities must be contained in the caller's permitted set. Effective `CAP_SYS_PTRACE` overrides both checks. Failure returns OperationNotPermitted. |
| `ptrace::check_read_fs_creds_access` | Caller/target threads into `/proc/<pid>/fdinfo/<fd>` read authorization | Same-thread-group access succeeds; otherwise caller fsuid/fsgid must match all target real/effective/saved UID/GID values and target permitted capabilities must be contained in the caller's effective set. Effective `CAP_SYS_PTRACE` overrides both checks. Failure returns OperationNotPermitted, which procfs maps to `EACCES`. |
| Cgroup migration | Target group and authorization callback into membership transaction | Requires published members/source, runs callback under stable process cgroup gate, then delegates atomic group migration. Callback decides filesystem-owned authorization. |
| Current-thread and credential helpers | Current TaskInner into a Thread/credential view | Dereference/downcast requires a user runtime. Read-only helpers (`with_current_user_thread`, PID/TID accessors) borrow the current task instead of cloning a task handle, so hot syscall paths add no reference-count traffic. `current_fs_context` alone falls back to initial fs for kernel tasks. Commit asserts no subjective override before replacing both credential pointers. |
| `Process::address_space` / mapping closure | Runtime active-mm handle into MM operations | Refuses missing runtime/mm user with NoSuchProcess. `LiveAddressSpace` retains an active user; MM owns address/range checks. |
| Thread robust/clear-child pointer setters | User-originated addresses stored as integers | No dereference/validation here; `posix-process` performs fault-aware access and bounded robust cleanup on exit. |
| Exec metadata and timer APIs | Kernel-prepared path/argv/heap/timer configuration into runtime | Caller/loader validates image metadata; ktimer validates timer requests. Missing runtime returns NoSuchProcess; close-on-exec errors propagate. |
| Exec credential transition | `no_new_privs`, filesystem-context ownership and prospective credential changes into `Cred::apply_exec` | `Process::begin_exec` admits the transition atomically: exclusive admission sets `in_exec`, while a concurrent `CLONE_FS` fork makes it shared. `ExecFsGuard::is_shared_with_other_process` reports the cross-process `FsStruct::process_user_count > 1` case; same-thread-group sharing is not unsafe. When exec changes identity or grows the permitted set and either `no_new_privs` or cross-process sharing is present, the `LSM_UNSAFE_SHARE` downgrade suppresses privilege gain: a growing set is clamped to the pre-exec permitted set, and euid/egid are reset to the real IDs when `no_new_privs` is set or the old credential lacked effective `CAP_SETUID`. `ksyscall` hands the loader an `ExecContext` (`no_new_privs` and cross-process filesystem sharing); the loader samples set-ID identities only when `no_new_privs` is clear, still reads the file capability set under it, and suppresses both only on a `nosuid` mount. The admission path itself synthesizes neither. |
| Pidfd construction | Stable process and explicit credentials into anonymous file | OpenFlags rejects unknown bits; typed private data checks reject other file kinds. Pidfd live queries reject exited identity while poll retains exit visibility. |
| TEE/TIPC callbacks (feature-gated) | Typed context/handle access across provider boundary | Owner locks protect contexts; closure callers must preserve provider invariants and avoid recursive locking. |

No direct user-memory read/write, device MMIO/PIO, DMA, firmware parsing or
network-buffer parsing is implemented here. External data can still be present
in paths, argv, signals and stored user addresses. Absence of local raw accesses
is not an assertion that these values are trusted or that syscall authorization
is complete.

## Unsafe inventory

Signal delivery wakes the selected task only after releasing signal-state locks.
`Thread` exposes the existing pending SIGKILL query through `UserTaskRuntime`;
the scheduler callback cannot sleep or acquire scheduler locks. Direct signal
wakeup respects the published sleep policy: ordinary signals select interruptible
waits, SIGKILL also selects killable waits. Event-only waits still require a
source notification. A signal wake never transfers a source resource or cancels I/O.

Guest fixtures in `ut-tests/lib.rs` retain the raw-waker callbacks formerly
in `src/tests.rs`. Their data pointers refer to leaked counters or observers;
the vtables preserve that lifetime and never reclaim the pointed-to storage.
These test-only callbacks remain outside production builds.

The intrusive-list unsafe operation is in `src/process/tree.rs`,
`Process::remove_child_slot_from_parent_locked`: `cursor_mut_from_ptr(slot)`
on the parent's `intrusive_collections::LinkedList` is undefined unless the slot
is linked in that exact list. Its SAFETY comment requires the process-domain
lock to serialize every child-list mutation, a membership check proving this
exact slot is in this exact parent's list, and absence of concurrent unlink
between check and removal. The write-guard parameter plus pointer-identity
search provide the local guarded path; `parent.children` is also locked during
removal. `is_linked` alone is not sufficient: it cannot tell which parent's list
holds the slot. Linking an already linked slot panics; attachment only links a
fresh slot, the current slot just popped by reparenting, or the init slot,
which is linked only while init is the parent.

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

- A task's identity is the task object; the numeric TID is a movable binding it
  owns. `TaskIdentityToken` is allocated once per thread and never reused. The
  number directory (`ProcessPublication::task_table`) is the only
  number-indexed table; thread and cgroup membership key on the token. Retire
  and delete paths still check slot object identity, so a reused number cannot
  retire an unrelated task.
- A number binding moves only in the sole-survivor `exec` takeover, in the
  sleepable `exec_de_thread` phase after the sibling sweep and before the image
  swap: the directory link moves once, the survivor adopts the group number,
  and its signal-registry entry is re-keyed. No other path changes a live
  task's number.
- Parent link, child list, exit signal and reserved reparent slot form one
  relation. Mutate them under the domain write lock, never by independent updates.
  An exec commits the exit signal to `SIGCHLD` only once the old thread group can
  no longer be resumed (`exec_de_thread` for a takeover, the address-space
  exchange for a single thread); failures before that point preserve the old
  contract, while failures after it are fatal and cannot roll it back.
- Running → Zombie/Dead and Zombie → Dead consumption are domain transactions.
  Acquire-load exited flags are advisory for compound decisions.
- Publication may retain strong Process identity after runnable threads vanish.
  Runtime availability and lifecycle liveness are separate; do not use Weak
  upgrade as authorization to target an exited process.
- `LiveAddressSpace` owns an active mm user. Pins/observers only retain metadata;
  final-user release clears user mappings. Runtime resource/fs/ns slots are
  detached before parent-visible exit by the lifecycle caller.
- `finish_thread_exit` reserves last-thread cleanup atomically with successful
  removal of the final published member. Repeated exits cannot qualify again,
  and the reservation rejects all later task publication. The entry releases
  the membership lock and retires the global TID before returning; it accepts
  no subsystem callback. POSIX performs named cleanup stages after retirement
  and publishes process completion only after those stages and resource detach.
  User-memory cleanup must finish before retirement; current runtime access
  remains distinct from external TID lookup during the exit continuation.
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

The per-thread CPU accounting lock serializes mode changes, context switches
and cross-CPU samples. Its IRQ-safe critical section covers only the monotonic
clock and accounting fields. The architecture transfer callbacks defer
preemption checks until their normal runtime checkpoints. Aggregate counters
remain relaxed atomics and are independent of directory and resource fields.

The scheduler CPU clock uses the membership lock for its exited/live handoff;
a sequence check retries if membership changes between allocation-free batches,
and task references acquired under that lock are released after unlocking.
Periodic IRQs only set a coalescing per-thread CPU-timer work bit. The
user-return path clears it before sampling and dispatches outside the IRQ;
a tick racing with the clear leaves a fresh request for the next return.
`ExecMetadata` atomically replaces its pair, while separate getters can straddle
updates. Typed timer dequeue receipts validate timer sequence through ktimer;
ordinary userspace SI_TIMER fields do not select that completion path. The
trusted timer bridge supplies provenance, and ksignal consumes it outside its
locks before handing the signal to a consumer. Completion compares the full
non-wrapping u64 generation, not the truncated u32 ABI field. Exhausted creation
and settime return InvalidInput without publication or timer mutation; deletion
and exec cleanup remain available. Creation captures a weak ProcessRuntime
origin, and dequeue upgrades it rather than resolving the receiving task. The
pinned runtime directly samples its process CPU times and mutates its own timer
manager, refreshing the same CPU-timer gate. Its manager lock is released before
the upgraded owner is dropped. Expired origins reject delivery; exec's timer
clear rejects old IDs in the still-live origin. A reusable record lease retains
occupancy through completion. Cancellation removes only still-queued old
ownership, never releasing an in-flight dequeue lease.
POSIX destinations are now separately
captured as weak signal-manager identity plus an optional registered slot.
Thread capture checks exact Process identity, not just TGID equality. Sends
validate slot liveness under SigLock. Numeric dispatch fields cannot redirect
the notification, and wake lookup checks manager/slot identity while retaining
the task. Legacy itimers and alarm-owner polling still use their existing
numeric routes; this is not a global PID/TID routing redesign.
Missing targets are ignored by delivery glue rather than reviving retired tasks.
Delete and exec clearing publish timer-map removal and refresh the CPU gate
under the manager mutex, then drop returned retired ownership after unlocking.
Late receipts fail lookup even when retired storage is still retained. This
provides the release boundary for captured destination references and
reserved records. Retired-wrapper destruction cancels old pending ownership.
Rearm similarly finishes its update after the timer mutex is released. Exact
record identity and captured manager/slot prevent cancellation of successors;
no timer/signal lock nesting is introduced.
Timer creation pins the runtime before validating PosixTimerConfig outside the
manager mutex. The same strong runtime owner spans admission; no preparation
step can dereference a retired runtime. Configuration contains validated
clock/notification values, a weak origin, a weak destination/optional slot and,
for notifying timers, fallibly reserved record storage. Reservation uses KERNEL
outside the manager lock and requires reclaim-safe sleepable context.
Admission errors retain the configuration until the explicit manager lock scope
has ended. Only then does the Process adapter discard it and return KError;
the runtime stays pinned through this unlocked cleanup as well.

## Threat analysis

| ID | Asset / threat | Severity | Trigger | Existing response and residual risk |
|---|---|---|---|---|
| T-01 | PID/TID reuse causes wrong-task removal or signal targeting | High | Old exit/rollback races a new numeric ID binding | Stable slots and Arc identity checks gate retirement/deletion; Reserved slots are not deleted as retired. Syscall target authorization remains external. |
| T-02 | Double reap or inconsistent parent relation | Medium | Multiple waiters, reparent and exit race | Domain write transaction scans, claims Zombie, detaches and retires identity once; callers rescan after failed claim. |
| T-03 | Intrusive-list removal corrupts memory | High | A foreign or concurrently unlinked slot is removed; duplicate linking panics | Guard token, parent list lock and exact membership check enforce the documented unsafe preconditions; attachment links only unlinked slots. |
| T-04 | Runtime references retain resources through zombie lifetime | Medium | Parent observes completion before task files and runtime fs/ns/mm detach | Lifecycle caller must detach owners first; weak runtime identity avoids owning the runtime. Existing live capabilities can deliberately prolong active-mm lifetime. |
| T-05 | Credential confusion permits cross-task inspection | High | Caller/target credentials differ or subjective override exists | Ptrace checks real credentials and commit rejects override, with effective `CAP_SYS_PTRACE` as the privilege override; the absent full dumpability model remains a residual limitation. |
| T-06 | Half-published fork becomes runnable | Medium | Parent writeback or cgroup migration fails | Staged publication and commit-before-activation; Drop rolls back. Already-exited/unusual rollback paths retain conservative state rather than deleting another identity. |
| T-07 | Exit signal exposes state before autoreap commits | Medium | Signal handler/waiter runs while parent contract changes | Prepare outside domain, revalidate parent/exit-signal and retry, commit state before signal queueing/wakeup. Failed notification is logged; UID fallback can reduce payload accuracy. |
| T-08 | Cgroup movement races child publication | Medium | Migration and clone operate concurrently | Process cgroup gate stabilizes authorization and reconciles unpublished membership; kcgroup owns group charge/migration invariants. |
| T-09 | Exec failure leaves partially updated runtime | Medium | Close-on-exec cleanup returns error | Error propagates before metadata pair publication; earlier timer/heap/signal changes are not rolled back. Caller must handle exec failure state appropriately. |
| T-10 | `de_thread` sweep kills sibling threads while another executor also sends signals | Medium | Exec races a second `kill`/`tgkill` | Concurrent `exec` serializes on the process `exec` gate, so only one thread sweeps; a spurious non-fatal wake cannot be mistaken for the winner's kill. The sweep owns each sibling and joins it before the address-space swap. Swept targets skip the group-exit marking and never record the process exit code, but a delivered fatal signal bumps the process fatal-signal epoch (the sweep's own kill bypasses it) and a concurrent `exit_group()` marks the group exited, so either aborts the exec with the racing signal instead of being absorbed or undone. |
| T-11 | `/proc/<pid>/exe` follows to a backing file that outlives the process | Medium | Magic-link open races final exit | The backing `Arc<VfsFile>` is read from a snapshot; a concurrent teardown may detach owners, so the follow returns NotFound when the backing file is gone, never a stale path string alone. |
| T-12 | Last-thread cleanup runs twice or completion becomes visible before cleanup | Medium | Concurrent exits or thread publication race the final membership removal | Reserve cleanup only on actual final-member removal, reject later publication, retire the TID after unlocking, and let POSIX publish completion only after its explicit cleanup stages. |

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

`apply_prepared_unshare` installs an already-validated namespace bundle and
optionally replaces the fs context; the syscall adapter then detaches the calling
thread's fd table for `CLONE_FILES`. That detach is the only fallible commit step;
a failure after the bundle install leaves the process with the new namespaces
and the old fd sharing, so callers must treat it as a process-fatal condition.
`apply_setns` swaps the bundle and, for `CLONE_NEWNS`, retargets the fs
root/pwd to the target mount namespace's visible root; the caller has already
authorized the switch and rejected pseudo-isolation kinds.

## Privacy, limitations and verification

Executable paths, command lines, IDs, user addresses, credentials and CPU stats
can be exposed through procfs/signal/pidfd consumers. Those consumers own access
policy; this crate supplies snapshots and does not redact them, with one
namespace-scoped exception: a thread pidfd's fdinfo `Pid:` value is withheld
from observers that would not name the thread (see the next paragraph). Debug/
signal logs include process IDs and signal names. Credential and payload memory
is not explicitly scrubbed on drop here.

Thread-pidfd `Pid:` disclosure is namespace-scoped. `PidFd::fdinfo_pid`
reports a thread's root tid only when the reading task's active PID namespace is
that thread's captured namespace or one of its ancestors; a reader in a child or
sibling namespace receives `0`, the value Linux's `pid_nr_ns` returns when no
projection exists. The decision is not a capability check — procfs still gates
the read itself (`ptrace::check_read_fs_creds_access`) — and it discloses no
number the reader's namespace could name. Process pidfds are unaffected: their
leader handles carry real per-namespace projections.

Root/global PID lookup projection, fs-sharing detection limited to published
threads, missing subreaper support, representative-thread credentials, simplified
ptrace privilege semantics, UID-zero child-exit fallback and external syscall
authorization are current limits. Resource-object or mm-ID availability does not
prove a live fd table or active mappings. `de_thread`
relies on the sibling threads being in the same process and reachable through
the scheduler; an uncancellable sibling in uninterruptible kernel work delays
the execing thread indefinitely (Linux has the same property). Optional
TEE/TIPC paths require feature-specific validation.

`ut-tests/lib.rs` exercises publication, PID reuse, rollback, parent/wait/exit,
resource detach and related invariants. Other modules in `ut-tests/` cover ptrace checks, pidfd
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
`TaskStat::num_threads` and the `status` `Threads` field consume that same
unreleased count, so a zombie leader keeps the group at one thread until
reap/autoreap clears the retained leader. ExitedLeader retains no strong
task/runtime owner and grants no TID lookup or signal delivery capability; it
retains only the leader's objective credentials so `capget(tgid)` reports the
unreaped zombie leader's frozen capability sets rather than a surviving sibling's.
Exact Weak task identity gates the exit transition so an old task cannot retire
a replacement binding. Reserved/rolled-back slots remain Vacant; nonleader exit
is released; reap/autoreap clears the retained leader under the existing
process-domain transaction. A successful non-leader `exec` takeover also clears
the retained leader, but only after the survivor has moved the global number
binding, adopted the group number and re-keyed signal routing. A failed
precondition or move returns first, leaving the leader slot authoritative. This
is an identity transition, not an accounting rewrite, and the procfs count never
synthesizes a leader alias. A nonleader slot still retires at its own exit, so
only a leader has Linux-style unreaped retention; the takeover case is the sole
path that transfers that retained identity to a survivor. Current runtime TIDs
are checked monotonic allocations and are not recycled on drop; exhaustion fails
rather than wrapping. Fixed-number test identities do not provide a production
reuse path. Any future recycling policy must preserve leader retention separately
from replacement publication before changing that invariant. The count is a slot
scan, not a global counter or atomic snapshot. No new unsafe code or sleepable
lock is introduced, and directory metadata locks are not held across the query.

The sleepable live-MM fault entry must be called outside MM, provider and
non-sleepable locks with IRQs/preemption enabled. It keeps the captured active
MM user alive while admission waits run, never trusts the pre-wait VMA for an
access afterwards, and preserves permanent failure classes. Exception handlers
continue to use raw locked fault handling rather than sleeping in this entry.

The live mapping guard forwards relocation's target-replacement policy while
permissions stay owned by the source snapshot. MM removes a fixed target and
discarded source tail before preparing the retained source; these destructive
steps are not enclosed in a transient admission retry.

Shared-mapping sync retains the captured `MmUserHandle` and must enter without
MM/provider locks from sleepable task context. File persistence runs outside the
MM mutex; process exit/exec cannot retarget or tear down that captured MM user.
