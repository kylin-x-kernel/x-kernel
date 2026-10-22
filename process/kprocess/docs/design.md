# kprocess — Design

## Purpose and ownership

`kprocess` owns the process domain: stable process/thread identity bindings,
parent-child and group/session relations, task publication, exit/wait state,
process-runtime capabilities, signal targeting and timer-delivery integration.
`Process` is a stable identity that can outlive runnable threads; its weak
`ProcessRuntime` reference is separate from the externally visible exited state.
`Thread` owns a strong runtime reference and its own objective/subjective
credential pointers and `TaskFiles` binding. Threads share the underlying table
only with `CLONE_FILES`; unshare and exec replace only the caller's binding.
`current_files()` pins the current thread and applies its process's live limit,
without storing a second binding or limit. `ProcessRuntime` is crate-private; external users operate
through `Process`, `Thread` and the semantic facade modules.

Syscall ABI decoding belongs to `ksyscall`; image loading belongs to `kexec`;
trap-loop and cleanup orchestration belong to `posix-process`; MM/VMA state
belongs to `memspace`; fd/resources, filesystem context and namespaces belong
to `kfd`/`kresources`, `fs_context` and `kns`. `kidentity` owns `PidHandle`
allocation and namespace number projection; kernel-internal PID/TID values here
stay root/global, while the user-visible getpid/getppid/fork views project into
the caller's active PID namespace. Concrete controlling-terminal behavior belongs to TTY
implementations of `ControllingTerminal`.

## Source scope and architecture

The analysis covers the entire crate, including feature-gated code and tests.

| Source | Role |
|---|---|
| `src/lib.rs` | Public facade, task construction/publication and current-resource helpers. |
| `src/process.rs`, `src/process/exit.rs`, `src/process/tree.rs`, `src/process/thread_membership.rs`, `src/process/runtime_access.rs` | Stable identity, exit/wait, tree/group membership, thread membership and runtime capabilities. |
| `src/process_runtime/mod.rs`, `src/process_runtime/posix_state.rs`, `src/process_runtime/runtime_state.rs` | Runtime construction/fork, exec metadata, mm-user ownership, heap and timers. |
| `src/thread/mod.rs`, `src/thread/core.rs`, `src/thread/current.rs`, `src/thread/cpu_time.rs`, `src/thread/restart.rs`, `src/thread/task_ext.rs` | Task-bound thread state, credentials, restart-block slot, current-context access and task-runtime hooks. |
| `src/publication.rs`, `src/process_domain.rs`, `src/lookup.rs`, `src/lifecycle.rs` | Directory slots, relation transaction lock, lookup and exit events/CPU totals. |
| `src/process_group.rs`, `src/session.rs` | Group member slots, session groups and controlling terminal. |
| `src/capability.rs`, `src/cgroup.rs`, `src/credentials.rs`, `src/job_control.rs`, `src/pidfd.rs`, `src/process_exit.rs`, `src/process_signals.rs`, `src/procfs.rs`, `src/ptrace.rs`, `src/resource_limits.rs`, `src/scheduler.rs`, `src/stat.rs`, `src/system_view.rs`, `src/timer_delivery.rs`, `src/wait_reap.rs` | Domain-specific public queries, updates, callbacks and snapshots. |
| `ut-tests/lib.rs`, `ut-tests/process_runtime_runtime_state.rs` | Process-domain and captured-MM regression cases; the remaining regression modules also live in `ut-tests/`. |

```text
TaskInner --owns user runtime--> Thread --owns--> ProcessRuntime
                                   |                 |
                                   +--owns Process <-+
                                              |
                                weak runtime reference
Process --group membership--> ProcessGroup --owns--> Session
Process --parent/child relation slots--> Process
ProcessPublication --slots--> process / task / group / session views
```

`ProcessPublication` stores strong published process identities and weak task,
group and session values inside stable slots. Per-process thread/group slots
retain stable bindings with weak target references. Child relation slots are
intrusive list entries prepared before relation updates. The global `INIT_PROC`
retains init identity; orphan reparenting reserves an init relation slot.

## Public interface groups and users

- Boot/clone owners call `build_process_thread`, `Thread::prepare_process_fork`
  or `prepare_thread_clone`, construct `TaskInner::new_user` using the matching
  identity, then call `publish_user_task`. `PublishedUserTask::commit` runs
  parent-side writeback before activation; abort/drop rolls back publication.
- `Process` runtime capabilities return filesystem/namespace/mm/resource/signal
  references and expose timer/exec/teardown operations. `LiveAddressSpace` owns
  an active mm-user handle; its mapping closure supplies both the locked space
  and observer/invalidation handles required by mapping runtimes.
- `scheduler`, `resource_limits`, `capability` and job-control mutation queries
  resolve non-exited processes. Query/pidfd views may retain published zombies.
  `procfs` additionally requires a representative published task for listing.
- `process_signals` resolves process/group/thread targets and calls `ksignal`,
  interrupting a chosen task. Process-directed sends target published identities:
  a still-published zombie accepts no delivery but reports success, so `kill(2)`
  does not observe `ESRCH` before the parent reaps it. `None` probes a target
  without queuing a signal. These helpers do not perform all syscall permission
  checks.
- `process_exit` and `wait_reap` are used by lifecycle/syscall owners to sequence
  last-thread cleanup, stable exit publication, child scanning and final identity
  removal. `PidFd` implements `Pollable` and anonymous-file operations against
  stable identity and exit completion.
- `migrate_cgroup_process` adapts a PID and filesystem authorization callback to
  process-wide migration; `scheduler` coordinates published task memberships.
  `CurrentThread` credential preparation/commit replaces only the calling
  thread's pointers; `ptrace` checks cross-task read credentials.
- `timer_delivery` registers `ktimer` expiry and `ksignal` dequeue observers;
  CPU timers run on current-user-thread return and wall timers on the alarm task.
  `UserTaskRuntime for Thread` supplies CPU residency and AArch64 switch roots to
  `ktask`; `CurrentSignalDispatch` adapts current-task signal injection.
- `TaskStat`, `system_view`, nice/scheduler and CPU-accounting APIs provide
  procfs/scheduler snapshots rather than owning hardware scheduling policy.

CPU accounting uses `Inactive`, `Kernel`, and `User` states, following Linux
generic vtime. Switch-out settles the interval; switch-in starts kernel time.
User transitions are marked at `UserContext::run` boundaries before IRQ dispatch.
Sampling settles the elapsed interval under `SpinNoPreempt`, advancing
`last_wall` so later samples cannot count the same interval again.
The existing exit flag prevents restarting finalized accounting during cleanup.
Separate IRQ/steal/guest accounting remains outside this model.

## Execution context and calling constraints

Construction, fork, runtime capability acquisition, publication, cgroup migration
and most facade operations allocate or acquire sleepable locks. They require
initialized allocator/scheduler and relevant MM/filesystem/namespace services;
they are not general interrupt-context or early-boot APIs. Current-user helpers
require a `Thread` installed in the current task. `current_fs_context` alone
falls back to `init_fs` for kernel tasks; it still requires initialized filesystem
state. A `CurrentThread` handle is created without validating its payload, but
its dereference requires a matching user runtime.

The process-domain spin rwlock is IRQ-safe and non-sleeping. Relation snapshots
use its read side; fork/exit/reparent/reap/publication mutations use its write
side. No sleepable lock, resource destructor or external signal callback may be
introduced under that transaction. Callbacks passed to cgroup authorization,
scheduler validation, TEE access or mapping access execute while the documented
owner lock is held and must avoid recursive acquisition/incompatible lock order.
Timer initialization is once-only and requires timer/signal/task runtime ready.
CPU accounting depends on monotonic time; task runtime hooks depend on MM CPU
residency and architecture initialization.

## State transitions

The actual `ProcessExitState` variants are Running, Zombie and Dead. New
identities start Running even before publication; “prepared” or “exiting” are
control-flow phases, not extra enum variants. Exit changes Running to Zombie
for waitable policy or Dead for autoreap, reparents children to init and resets
their exit signal to SIGCHLD. Init is excluded from this normal exit transition.
A consuming wait changes Zombie to Dead and removes parent membership;
`WaitReapMode::Peek` leaves it intact. A scan returns Ready, NoMatchingChild or
NoWaitableChild according to selector, exit-signal class and actual state.

Publication slots transition Vacant/Retired → Reserved → Published; retirement
clears the value to Retired before structural directory deletion. Reservation
of an already Published identity leaves it published. Cleanup removes only the
same slot when it is Vacant/Retired, protecting reused PID/TID bindings. A
`PublishedUserTask` owns a rollback record until activation consumes it.

Thread accounting uses None/User/Kernel: each state change charges elapsed time
to the prior state before selecting the next. Thread exit and no-new-privileges
are separately published atomic flags. Group/session changes create or select a
group and publish group/session identity together; existing-group movement
requires the same session. The syscall owner checks additional leader/ID rules.

Each `Thread` also carries a seccomp `SeccompContext` (mode + classic-BPF
filter chain). It is installed only through `seccomp(2)` on the owning thread
(or a `TSYNC` broadcast to published siblings, which first requires every
sibling's chain to be a prefix of the caller's and re-validates that prefix
under each sibling's own lock before replacing its chain, so a synchronization
can never remove a restriction) and evaluated at syscall entry by `ksyscall`. A mode
transition Linux refuses (`STRICT` ↔ `FILTER`) fails with `EINVAL` rather than
replacing the chain, and the whole chain is bounded by Linux
`MAX_INSNS_PER_PATH`. `fork`/`clone` copy the parent chain so children inherit
the restriction; seccomp state survives exec (filters only ever remove allowlist
membership).

## Critical flows

Fork takes the process cgroup gate, reserves a child charge, clones/shares fs and
namespaces, allocates a root identity and validates/attaches the child relation.
Shared fs while its parent is in exec returns WouldBlock; namespace errors map
to InvalidInput/Unsupported or a mount error. With `CLONE_NEWPID` the child
leader is allocated inside the freshly created child PID namespace (visible as 1
there) while still carrying a root projection for global lookup, matching Linux
`copy_pid_ns`. `CallerParent` inherits the
caller's current parent and exit-signal contract, rechecking it under the domain
lock. Thread preparation clones/shares the caller's fd table; runtime preparation
then clones/shares mm and signal actions;
`AttachedForkProcess` rolls back the attached relation if preparation fails.
The new thread starts from the caller's subjective credential snapshot and
inherits no-new-privileges. Its restart-block starts empty; clone does not
copy a parent's armed continuation. `exec` clears the slot because the payload
addresses the previous image. The payload type belongs to the syscall adapter.
Only `CurrentThread` may arm or take it, so a sibling `Thread` cannot read
another thread's wait. A sibling clone shares the existing runtime.
Both clone preparation paths copy the calling thread's blocked signal mask
before returning the child for publication, including when the caller is not
the process leader. Subsequent mask changes remain thread-local. New private
pending queues are empty; a new process also starts with empty process pending,
while a sibling retains access to the existing process pending queue. Sharing
signal actions does not share blocked masks or copy pending signals.
Initial runtime construction passes owned action values to `ksignal`. Fork
asks the parent signal manager to create the new process manager; `kprocess`
does not hold or expose the shared action-table reference. Exec delegates the
action reset to that manager, without changing pending or blocked ownership.
Exec detaches signal configuration when another process shares it; otherwise
it resets in place. Ignore dispositions survive, but all flags, masks and
restorers are cleared. Other processes retain their full original configuration.

Publication reconciles the thread's cgroup under the gate, reserves table/member
slots under the publication table write lock, then publishes all visible slots
under one domain write transaction. Parent writeback runs after visibility and
before activation. Failure/drop retires owned bindings and removes an inserted,
still-running unpublished child relation; it does not retire an unrelated
existing process identity. Retired slot deletion checks pointer identity and
state to tolerate numeric ID reuse.

Every exiting thread detaches its files before retiring thread membership, so
last-thread completion cannot overtake a sibling's descriptor cleanup.
Last-thread cleanup is orchestrated by `posix-process`: remove thread visibility,
record final CPU time, detach mm/fs/ns and optional resources, then call
`complete_process_exit`. SIGCHLD preparation occurs outside the domain lock;
parent/exit-signal changes cause retry. The domain transaction commits exit,
autoreap and reparent; only then are signal queueing and parent wakeup performed.
Default SIGCHLD, explicit ignore and no-child-wait policy come from `ksignal`.
Child-exit payloads contain wait status, PID/UID and CPU time. If no published
thread credential snapshot exists at payload construction, the current code
uses UID zero. Non-SIGCHLD exit signals use the generic process-send path.

Wait scans/matches/consumes under the domain write lock and returns a stable
`WaitedChild`. The wrapper adds child CPU totals and removes the retired directory
entry outside that lock. `finish_thread_exit` retires TID visibility before later
resource teardown, so external thread lookup stops targeting exit-tail tasks.

Exec initializes the new image's heap marker when constructing its MM binding.
The caller unshares its files and closes close-on-exec descriptors. The subsequent
process update handles optional TA context, signal dispositions, POSIX timers
and optional IPC/private state, then publishes path/cmdline
under one `ExecMetadata` lock. A failure closing descriptors can occur after
earlier changes: the operation is not an all-state rollback transaction. The
combined metadata update is atomic, but separate path and cmdline calls need not
observe the same generation across a concurrent exec.

`de_thread`: `execve` serializes on the process-wide `exec` gate
(`Process::acquire_exec_gate`, Linux `cred_guard_mutex`), so two sibling execs
cannot SIGKILL and join each other in a mutual deadlock; the loser waits
interruptibly and is killed by the winner's sweep. A multi-threaded process (Go
runtimes, OCI container init) then SIGKILLs every sibling thread and joins them,
so only the execing thread reaches the shared address-space swap. Each swept
target is marked (`Thread::mark_exec_sweep_target`) so its fatal death skips the
group-exit marking and group-kill broadcast, and the sweep runs whenever the
execing thread is not the group leader — including when the leader already
exited and this is the sole surviving non-leader, so the group pid moves onto
the survivor. A genuine concurrent `exit_group()` still marks the group exited,
and a delivered fatal `kill`/`tgkill` bumps the process signal manager's
fatal-signal epoch (Linux `complete_signal` marking `SIGNAL_GROUP_EXIT`); a
fatal signal every thread blocks stays pending and does not count. The sweep's
own kill bypasses the counter, and `exec_de_thread` aborts the exec when either
changed, terminating with the racing signal so the parent observes
`WIFSIGNALED` instead of a clean exit or the sweep's SIGKILL.
`execve` also installs
the opened backing executable file (`with_exe_file`), replacing/inheriting the
display path in `ExecMetadata` so `/proc/<pid>/exe` can follow to the real file
across exec, fork, and the init process created via `fork` + `execve`.

## Task identity and the number binding

A task's *identity* is the task object itself; the numeric TID/PID is a
*binding* it owns. `kidentity::TaskIdentityToken` is allocated once per thread
and never reused, and `Thread` stores its current number in
`SpinNoIrq<Arc<PidHandle>>` with an `AtomicU32` snapshot, so `tid()` is a
lock-free read while the binding can change in one critical section.

The single number-indexed table is `ProcessPublication::task_table` (TID →
task). Registries that must not be re-indexed when a number moves key on the
stable token instead: `ThreadMembership` (per-process thread membership) and
`kcgroup`'s `member_tasks`. `ProcessPublication` also keeps a token → task
directory so those registries resolve a live task and read the current number
on demand.

A non-leader `exec` therefore performs an O(1) takeover in the sleepable
`exec_de_thread` phase: the survivor retires the dead leader's directory link
and republishes its own under the group number
(`ProcessPublication::move_task_number`), adopts the group number
(`Thread::adopt_task_number`), and re-keys its process signal registry entry
(routing still selects threads by number). Membership and cgroup need no
change, and the address-space exchange stays a pure pointer swap in
`replace_current_address_space`, with no identity work in its callback.

## Concurrency model

Last-thread POSIX cleanup calls `exit_signal_handlers` before process completion,
outside membership and process-domain locks. It releases this process's handler
ownership even if a prepared notification or observer retains its signal manager.
`ProcessRuntime::drop` also releases it for unpublished fork rollback. Both paths
are idempotent; surviving CLONE_SIGHAND peers retain their own ownership.

Lock order for publication is process cgroup gate → publication tables → process
domain → small spin-locked slots. Exit/reap retire visibility under the domain
lock, release it, then remove table entries. Lookup first collects table slot
references and then observes them under the domain read lock. Tree/member/group
locks protect local containers; cross-object invariants require the domain token.
Session terminal and group lists have their own IRQ-safe spin locks.

Runtime fs/ns slots and exec metadata use RwLock; the process-MM binding uses
SpinNoIrq. CPU accounting, scheduler state and timers retain their own locks.
Thread credential commit takes objective
then subjective credential write locks and asserts the pointers were not
independently overridden. CPU totals use relaxed nanosecond atomics and are
observational counters, not publication barriers. Heap and thread control fields
use their documented atomic orderings; no single atomic read proves an entire
process snapshot is current. `Process::resources` can retain the resource object
after its fd-table slot was detached, and `mm_id` remains available while the
runtime shell exists even after active mm ownership is released.

## Resource lifecycle, decisions and limitations

Threads strongly retain runtime, while Process retains it weakly to avoid an
identity/runtime cycle. `MmUserHandle` counts active users; `MmPin`/observers
retain metadata without keeping user mappings alive. Releasing the final active
user clears mappings; `LiveAddressSpace` delays that event while held. Exit takes
owner slots once and drops objects outside their locks; repeated detach is safe
while the runtime shell remains reachable. Zombies/pidfds retain stable identity
without requiring attached files, filesystem paths or namespaces.

`ProcessRuntimeState` directly owns a `ProcessAddressSpace` under `SpinNoIrq`:
active user, structural pin, MM identity, CPU residency, heap marker and existing
AArch64 ASID context all belong to one binding. Private `ProcessAddressSpace::new`
collects that tuple in sleepable context before publication. The IRQ-safe lock
serves scheduler callbacks, including IRQ-tail scheduling. Accessors return owned
capabilities or scalar values, not references into the binding. Residency updates
and ASID preparation remain non-sleeping; lock order is runqueue, thread exit-snapshot
lock, process-MM binding, then residency/ASID lock. Paths may omit intermediate
locks, but no reverse acquisition is allowed.

User acquisition under the binding lock only increments reference counts. Exit
takes the user slot under the lock, then releases it in sleepable context outside
both the binding lock and the MM mutex. The pin and activation metadata survive
that release for scheduling tails. Runtime destruction also releases the user
before dropping the pin, without holding the binding lock. Timers remain separate.

`replace_current_address_space` constructs a complete candidate in sleepable
context through the same constructor as initial setup. It requires a live current
user task which is the sole published thread. Under the binding lock and local
exclusion it exchanges owners, marks new residency, prepares the AArch64 ASID (or
uses the cached non-AArch64 root), then activates saved and hardware roots through
ktask. Old residency bits remain conservative. All displaced/candidate destruction
occurs outside guards; no error follows exchange.

Exec immediately resets old user registrations after replacement, before fallible
new-image work. Sleep/preemption during old-owner release must not deliver user
signals. TEE session cleanup stays at the successful exec tail, preserving fatal
cleanup state until then.

Before `Process::exit_thread` retires membership or the TID, the exiting Thread
installs one immutable `ExitAddressSpace` containing a clone of the binding's
`MmPin`, matching CPU residency and, on AArch64, its ASID context. Pin cloning
only increments structural reference counts; capture clones the matching tuple
under thread and binding `SpinNoIrq` locks. It takes no MM mutex and allocates
nothing. The thread lock is released before membership retirement and directory
callbacks. Repeated exit never replaces or drops the installed snapshot.

Snapshot presence immediately selects the thread's scheduler residency/root
callbacks, independently of `is_exiting`. Live threads still select the runtime.
AArch64 prepares a root through the captured ASID context on every switch so
rollover remains supported; no frozen ASID scalar is stored. Other architectures
retain their saved-root switch path. Old residency bits remain conservative.
The snapshot has no active MM user and cannot delay last-user mapping clear.
It survives runtime binding changes, process reap and remaining exit work until
Thread destruction. Task ownership uses DeferredArc: after the final switch
and final strong release, a normal sleepable worker destroys the task and its
Thread runtime. Weak expiration and handle Drop do not wait for snapshot/MM
storage release; explicit process cleanup and publication rollback still run at
their existing boundaries. Extra task owners delay retirement. User cleanup must precede
`finish_thread_exit`; the POSIX continuation owns skipping subsequent user signal
delivery and user return.

Structural ownership and captured-view regressions live in
`ut-tests/process_runtime_runtime_state.rs`. Cross-crate scheduling and exec
coverage lives in `core/ksyscall/ut-tests/task/tests/exit_tail.rs`.

`LiveAddressSpace` captures an active MM user and the binding's heap marker under
that same lock. Its `heap_top()` reads the captured atomic without reacquiring the
binding lock, including while holding the MM mutex. A procfs maps iterator uses
this paired capability rather than separately looking up the process heap.
The marker is live within that image, not a frozen numeric snapshot, but a later
binding cannot retarget an existing capability. Final capability release still
requires sleepable context outside the MM mutex.

Process heap getters/setters act on the bound marker. Fork continues to copy the
parent's current heap value into a new per-process marker, including CLONE_VM;
this does not introduce shared-brk semantics.

Preallocated intrusive child slots keep commit-time relations within the
non-sleeping domain boundary; stable publication slots separate visibility from
allocating/deleting BTreeMap entries. Dedicated semantic query modules prevent
callers from reconstructing lifecycle policy from weak runtime references.
`ProcessRuntime` holds the process namespace bundle in an `RwLock<Option<NsProxy>>`.
`apply_prepared_unshare` installs a validated bundle (optionally swapping the
filesystem context for `NEWNS`/`CLONE_FS`) and unshares the fd table for
`CLONE_FILES`; `apply_setns` swaps one namespace slot and retargets the fs
root/pwd into a mount namespace for `CLONE_NEWNS`. Both paths only install
state validated by `kns`, so the commit surface is small. `Process::nsproxy` and
the per-namespace accessors expose the bundle to `setns` and `/proc/<pid>/ns`.
Known limits include the fs-sharing check covering only the caller's published
threads (cross-process `CLONE_FS` sharing is not counted), signal PID arguments
carrying only caller-namespace translation with a root fallback while lookup
tables stay root-keyed, no child-subreaper implementation, representative-thread
credential snapshots, simplified ptrace privilege checks (effective UID zero
substitutes for CAP_SYS_PTRACE), and no full authorization policy in
signal/job-control facade helpers. `PR_SET_PDEATHSIG` records the
signal on the identity but delivery at parent-death reparenting is not yet
wired. The init process is bootstrapped through the low-level
`build_process_thread` path, so its backing `exe_file` is not stashed for
`/proc/1/exe` (a documented follow-up). Optional TEE/TIPC behavior exists
only with its Cargo feature enabled.

## Published tasks and unreleased thread-group members

`scheduler::process_task_count(&Process)` retains the published-task view:
exit unpublishes a TID immediately, so external targeting cannot reach teardown.
`procfs::thread_group_member_count(&Process)` has a different lifetime contract.
The member-slot state is Vacant, Published(Weak task), or ExitedLeader.
A published numeric group leader exits into ExitedLeader; nonleaders release
their slot at exit. Last-task detection, task enumeration and lookup ignore
ExitedLeader. The retained state
owns no task, runtime or mm and cannot be scheduled. Wait consumption/autoreap
clears it when the process identity is retired, even if other Arc<Process>
observers remain. Failed publication uses Vacant and contributes no membership.

This change does not implement exec thread-group identity takeover. In particular,
a sole nonleader may currently exec after the original leader exits while retaining
its original TID; kprocess has no unified de_thread transition that changes that
identity and its global publication. The original ExitedLeader therefore remains
until reap, and the task-directory count can include that retained member after
nonleader exec. Linux-exact post-exec counts are not claimed for this path. A
count-only leader alias would conceal the identity mismatch, so exec neither
promotes a member for accounting nor removes the original leader slot here.
A follow-up must coordinate canonical task identity, global/member publication,
signal/cgroup bindings, old-leader release and the successful/fatal exec boundary
before procfs can derive the corresponding Linux count from final membership.

This retention scheme relies on the current runtime identity allocator, not on
the probability of TID reuse. `Thread::prepare_thread_clone` calls
`allocate_thread_task_number`, which delegates to `kidentity`'s root namespace.
Its checked monotonic `fetch_update` returns `WouldBlock` at exhaustion; dropping
a handle does not recycle a number and the counter never wraps. The fixed-number
replacement fixture deliberately bypasses allocation to test stale Weak identity
guards; it does not demonstrate runtime numeric reuse. Introducing recycling
would require revisiting retained-leader ownership before allowing a new task to
overwrite an `ExitedLeader` slot.

Both queries scan stored slots under the existing membership/slot locks and
allocate no list or maintained counter. Publication may interleave with a scan;
neither result promises an atomic thread-group snapshot. Reap clears retained
membership under process-domain -> membership -> slot order. Task exit releases
membership locks before global TID removal and acquires no process-domain lock
while holding them. There is no procfs max(1) or zombie-state compensation.

Regression definitions extend the real exit/reap test to distinguish live count
0 from lifetime count 1 before reap and 0 after reap with strong observers still
held. Another test explicitly exits the leader before a sibling and checks that
only the leader remains until reap. These definitions exercise real member exit
and reap with never-run tasks; they do not execute an exec syscall or prove
identity takeover. Their execution must be checked against this head's CI.

## Kernel unit-test layout

TIPC handle teardown coverage in `ut-tests/lib.rs` requires `KFEAT_TIPC=y`.
The captured-MM cases retain their AArch64-specific ASID assertions.

Guest test sources live in `ut-tests/` and are mounted at their original
module boundaries with `unittest::path_mod_test!`. They use `#[def_test]`
and run in the configured kernel with `make unittest`, not host Cargo tests.
The migration preserves module visibility, feature gates and execution modes.

- `ut-tests/lib.rs`: mounted by `src/lib.rs` as `tests`.
- `ut-tests/pidfd.rs`: mounted by `src/pidfd.rs` as `pidfd_tests`.
- `ut-tests/ptrace.rs`: mounted by `src/ptrace.rs` as `tests`.
- `ut-tests/stat.rs`: mounted by `src/stat.rs` as `tests_stat`.
- `ut-tests/thread_cpu_time.rs`: mounted by `src/thread/cpu_time.rs` as `tests`.
- `ut-tests/process_runtime_runtime_state.rs`: mounted by `src/process_runtime/runtime_state.rs` as `tests`.
