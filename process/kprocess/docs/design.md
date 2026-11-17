# kprocess — Design

Unit-test wake counters and exit observers use fallible `KBox` allocation in
sleepable fixture setup. `KBox::leak` retains their existing static lifetime:
raw wakers borrow the observers and never reclaim them.

## Fallible Ordinary Signal Admission

Process/thread signal helpers propagate ksignal WouldBlock before interrupting
any task. Existing syscall callers already propagate KResult, so ordinary
non-SI_USER realtime owner exhaustion reaches userspace as EAGAIN. Target,
permission and signal-zero behavior remain unchanged. Group delivery retains
its existing partial-delivery semantics; no rollback of prior recipients is added.

Current-kernel signal delivery explicitly uses SignalNotification::kernel_fallback.
Non-SIGCHLD child-exit notifications use ordinary admission: realtime CLD_* owner
exhaustion is logged without publishing a pending bit or interrupting the parent.
The independent notify_child_exit call still wakes wait* waiters after exit
publication. Userspace send helpers never opt in based on a siginfo code.
Prepared POSIX timers still submit reserved ownership without allocating.
Tests adapt existing delivery calls to Result; fault injection remains at the
ksignal production allocator seam, not syscall dispatch or global allocation.
The realtime child-exit regression injects ordinary owner failure for CLD_EXITED,
CLD_KILLED and CLD_DUMPED payloads and checks that pending state, fatal epochs
and signal wake observers remain unchanged. It does not inject failure through
the complete process-exit and wait* path.

## Independent Standard Timer Notifications

The POSIX dispatch bridge transfers the reserved lease to ksignal even for
standard signal numbers. Distinct timers targeting the same signo remain
independent; ordinary notifications still coalesce while that signo is pending.
The process regression queues two CPU one-shot timers on SIGUSR1 and verifies
their separate IDs and completion, including the pending bit between dequeues.
This adds no dispatch allocation. Ignored replay uses the same stable target.

## Purpose and ownership

POSIX timer dispatch consumes the signal layer's typed admission outcome after
all signal locks are released. It treats the wake TID only as an identity-checked
wake hint, then drops any rejected notification in this unlocked bridge scope.
Coalescing can still wake the existing pending signal; blocked admission needs
no task wake. Releasing a prepared receipt queues its reserved record for the
alarm task, which calls the captured runtime outside locks. That runtime checks
exact record identity, polls one timer under its mutex and dispatches after
unlocking. This supports deferred one-shot demand without a remaining alarm.
Ignored periodic records are retained by ksignal. Its requeue request uses the
same ready worker: record_requeued validates exact timer reservation identity,
captures its stable target under the timer mutex, then queries pending state and
wakes an identity-checked task after unlocking. No new notification or receipt
is created. Retention/replay and rejected sends never acknowledge delivery.
The ready-progress regression holds an old CPU-timer notification, rearms an
already-expired one-shot while storage is busy, and releases the old lease.
It waits for actual alarm-task dispatch without polling the timer again, then
checks the single new notification. This tests progress with no remaining
deadline or armed CPU-timer gate, not just a helper's return value.

The per-thread TEE session slot also holds a preallocated `KBox` DST. Installing
an owner or borrowing the slot does not allocate in kprocess. TEE's lazy initializer
uses NOWAIT only for a missing slot under the mutex; existing-session access and
cleanup do not allocate redundant owners. Session callbacks retain the slot lock.

Thread and signal-slot ownership use `KBox`. Initial thread construction through
`build_process_thread` returns `KResult<KBox<Thread>>` and uses KERNEL, requiring
sleepable task context without reclaim-dependent locks, IRQ/preemption exclusion
or non-sleepable RCU scopes. Fork and sibling-clone paths instead pass NOWAIT for
both owners: they hold the process cgroup transaction mutex and must not add
reclaim dependencies under that serialization lock. Slot allocation still
precedes the signal lock. The thread owner is allocated after its fields have
been prepared, outside the signal lock but still within the clone transaction.

NoMemory propagates through preparation before runnable-task publication.
The attached-child rollback guard now remains armed through final thread
preparation, not only runtime construction. A signal-slot or outer owner
failure therefore also removes the unpublished child relation from its parent.
`PreparedUserClone::into_parts` transfers the same owner to the POSIX trap-loop
builder and `TaskInner::new_user`; coercion to `KBox<dyn UserTaskRuntime>` does
not allocate or move the pointee. PID 1's caller retains the existing fatal
initialization boundary. Callers prepare a `PreparedTaskEntry` before starting
clone preparation, so consuming the prepared clone adds no fallible callback
allocation. Arc/Vec and kernel-stack allocations are
not made fallible by this migration.

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
the caller's active PID namespace. The `getppid(2)` projection is cached per
process (`visible_parent`, `u32::MAX` = none) and refreshed under the
`process_domain` write lock whenever the parent relation is attached or
reparented, so the syscall reads one atomic instead of the domain and relation
locks. Concrete controlling-terminal behavior belongs to TTY
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
| `src/capability.rs`, `src/cgroup.rs`, `src/credentials.rs`, `src/job_control.rs`, `src/process_exit.rs`, `src/process_signals.rs`, `src/procfs.rs`, `src/ptrace.rs`, `src/resource_limits.rs`, `src/scheduler.rs`, `src/stat.rs`, `src/system_view.rs`, `src/timer_delivery.rs`, `src/wait_reap.rs` | Domain-specific public queries, updates, callbacks and snapshots. |
| `src/pidfd.rs` | Pidfd file identity and lifetime: process-anchored lifecycle/notification, fdinfo `Pid:` resolution, and thread-pidfd identity-token verification (`new_file_for_task`, `fdinfo_pid`). |
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
  identity, then call `publish_user_task`. `publish_user_task_with` runs a
  caller initializer while the task is still private, then publishes it.
  `PublishedUserTask::commit` runs parent-side writeback before activation;
  abort/drop rolls back publication.
- `ksyscall`'s clone path calls `PidFd::new_file` (process pidfd, reporting the
  thread-group leader) or `PidFd::new_file_for_task` (thread pidfd for
  `CLONE_THREAD | CLONE_PIDFD`, reporting the thread's own tid). Both anchor
  lifecycle, poll readiness and file lookup on the owning `Process`; only the
  reported fdinfo number differs.
- `procfs` calls `PidFd::fdinfo_pid(observer)` with the reading task's active
  PID namespace to render `/proc/<pid>/fdinfo/<fd>`'s `Pid:` line. Resolution
  is a snapshot: the process identity is reported while a task is attached
  (a waitable zombie included) and `-1` after reaping, while a thread pidfd
  reports its root tid only while `lookup::task` still resolves that tid to the
  same immutable identity token. A thread that exited, moved its number through
  an exec leader takeover, or lost its tid to reuse therefore reports `-1`
  instead of a stale or recycled number. Process identities translate through
  their per-namespace projections (`0` when the observer has none). Thread
  identities are allocated root-only, so the kernel names a thread by its root
  tid (`gettid(2)`, the `clone3` return value); that number is reported to a
  reader in the thread's namespace or an ancestor of it, while a reader below
  or beside that branch gets `0`, matching `pid_nr_ns`.
- `Process` runtime capabilities return filesystem/namespace/mm/resource/signal
  references and expose timer/exec/teardown operations. `LiveAddressSpace` owns
  an active mm-user handle; its mapping closure supplies both the locked space
  and observer/invalidation handles required by mapping runtimes.
- `scheduler`, `resource_limits` and job-control mutation queries resolve
  non-exited processes. `capability::target_capabilities` is a read query: it
  resolves live tasks but deliberately reports an unreaped exited leader's
  retained real credentials. Query/pidfd views may retain published zombies.
  `procfs` additionally requires a representative published task for listing.
- `process_signals` resolves process/group/thread targets and calls `ksignal`,
  then calls `ktask::signal_task` after signal-state locks are released.
  Direct wakeup selects interruptible waits, and also killable waits when
  SIGKILL is pending. Registered Future notifications and running-task kicks
  remain active. A wake grants no resource; each waiter rechecks its predicate.
  Process-directed sends target published identities:
  a still-published zombie accepts no delivery but reports success, so `kill(2)`
  does not observe `ESRCH` before the parent reaps it. `None` probes a target
  without queuing a signal. These helpers do not perform all syscall permission
  checks.
  Timer delivery uses a separate internal notification route: POSIX emissions
  carry a non-Clone ksignal receipt, while legacy timers and all ordinary sends
  carry only siginfo. Creation captures a weak ProcessRuntime as TimerSignalOrigin;
  emission forwards it to the receipt. Completion upgrades this exact runtime
  after signal locks are released and validates its existing ID/sequence, never
  the receiver's current runtime. It applies to every timer signo without
  installing global observers or allocating a callback owner.
  Emissions now transfer the unique lease of a KArc-backed record reserved at
  creation; the dispatch bridge no longer builds a receipt from copied fields.
  Timer creation pins the runtime, builds a validated PosixTimerConfig outside
  its timer-manager mutex, then publishes the disarmed timer under the mutex.
  Runtime absence still precedes invalid-clock errors. Notifying configurations
  reserve storage with KERNEL before taking that mutex; NoMemory publishes no ID.
  SIGEV_NONE skips reservation. The caller must be sleepable and reclaim-safe.
  A rejected admission returns its configuration; the Process adapter leaves
  the manager lock scope before releasing that configuration and exposing the
  original KError to its caller.
  The creation regression checks absent-runtime precedence, invalid-clock
  rejection without consuming the first timer ID, and successful disarmed
  admission through the production Process interface.
  A second regression creates equal timer IDs/sequences in two real runtimes,
  verifies the captured origin and updates overrun only in that runtime. Clearing
  its POSIX timers (the exec path) makes the old completion fail. These tests do
  not yet establish stable signal destination membership or non-wrapping leases.
  The manager now issues non-wrapping u64 generations independently of siginfo.
  Exhausted create/settime returns InvalidInput before publication or mutation;
  deletion and exec clearing remain possible without resetting that source.
  The runtime completion bridge preserves all 64 bits when checking the timer.
  POSIX timer creation also captures a weak signal-manager target; SIGEV_THREAD_ID
  resolves a task, verifies exact Process identity, and captures its live slot
  before admission. Dispatch ignores the POSIX numeric envelope and sends through
  this target. TID lookup is only a wake hint: the resolved task's manager/slot
  must still match, and the task is retained through signal_task. Legacy itimers
  keep numeric routing. Regression tests substitute another process/TID in the
  envelope and verify both shared and private notifications reach only the
  captured owner. Rearm finishes its `PosixTimerUpdate` after unlocking, cancelling
  old pending ownership before returning the syscall result.
  Delete and exec clearing now transfer retired timer ownership out of the
  manager mutex. The CPU-timer gate is updated before unlocking, and the removed
  timer or detached map is dropped, cancelling invalidated pending ownership.
  Process/syscall return types are
  unchanged; the lower-level ktimer removal APIs now return opaque owners.
- `process_exit` and `wait_reap` are used by lifecycle/syscall owners to sequence
  last-thread cleanup, stable exit publication, child scanning and final identity
  removal. `PidFd` implements `Pollable` and anonymous-file operations against
  stable identity and process exit state. `Process::exit_event()` exposes a
  `PollSet` notification source, while `Process::is_exited()` is the persistent
  readiness predicate. Poll users register then recheck that predicate to cover
  exit racing registration; late observers need no replayed notification.
- `current_cgroup()` resolves the running thread's cgroup (`None` for kernel
  tasks and tasks that never joined one); external callers include the bpffs
  device-access policy, direction kprocess -> consumer.
- `migrate_cgroup_process` adapts a PID and filesystem authorization callback to
  process-wide migration; `scheduler` coordinates published task memberships.
  `CurrentThread` credential preparation/commit replaces only the calling
  thread's pointers; `ptrace::check_read_real_creds_access` checks cross-task
  real-credential reads, including permitted-set containment.
- `ptrace::check_read_fs_creds_access` supplies the `/proc/<pid>/fdinfo/<fd>`
  access decision (`PTRACE_MODE_READ_FSCREDS` semantics): provider kprocess,
  consumer procfs, direction kprocess -> consumer. It passes same-thread-group
  callers, otherwise compares the caller's filesystem UID/GID with all target
  real/effective/saved IDs and the target's permitted capabilities with the
  caller's effective set, and allows effective `CAP_SYS_PTRACE` to override
  both checks.
- `timer_delivery` registers `ktimer` expiry and attaches typed dequeue receipts;
  a periodic tick coalesces CPU-timer work on the current thread while its
  process has armed timers. User return consumes that request and polls the
  manager; wall timers run on the alarm task. The per-thread pending bit plays
  the role of Linux `posix_cputimers_work.scheduled`.
  `UserTaskRuntime for Thread` supplies CPU residency, pending-fatal-signal
  queries and AArch64 switch roots to `ktask`; `CurrentThreadSignalIf` adapts
  current-task signal injection.
- `TaskStat`, `system_view`, nice/scheduler and CPU-accounting APIs provide
  procfs/scheduler snapshots rather than owning hardware scheduling policy.
- PID-namespace identity resolution is public for cross-crate consumers:
  `PidIdentity` (Linux `struct pid`), `Process::pid_identity` (the leader
  identity `task_tgid` names), `Process::pid_identity_in_namespace` (lookup
  across leaders and member threads, like `find_get_pid`) and
  `Thread::task_number` (a single task's identity). Provider: kprocess;
  user: posix-net control-data handling, which resolves an
  `SCM_CREDENTIALS` claim to an identity and lets each receiver evaluate it
  in its own namespace (direction kprocess -> posix-net). Member-thread
  identities carry only a root projection and `gettid(2)` reports that root
  number, so `pid_identity_in_namespace` also matches a thread's root number
  when the observer namespace has no exact projection for it.

`TaskStat::from_thread` samples the thread's private pending, shared pending and
blocked masks through one `ThreadSignalManager::masks()` call, then builds the
`stat` view. A caller that renders `status` uses
`sample_thread()` together with the stat snapshot so `SigPnd`, `ShdPnd` and
`SigBlk` are not independently sampled. `vsize` is `Some(sum of VMA lengths)`
when a live address space is attached and `None` when it is absent (`mm == NULL`
in Linux terms); `None` is not an error and is distinct from an empty address
space. `num_threads` uses the unreleased membership count, so an
exited-but-unreaped leader remains visible until reap. `exit_signal` is the
group leader's process-wide wait contract; every non-leader thread renders `-1`
because a `CLONE_THREAD` task does not notify a parent independently, matching
Linux `do_task_stat`. A successful exec commits that contract as `SIGCHLD`:
`exec_de_thread` commits it for a takeover, and `execve` commits it immediately
after the address-space exchange for a single-threaded exec. A failure before
that point leaves the contract unchanged; a failure after it is fatal and does
not return to the old image. The `stat`
signal fields are a compatibility view: they expose only the low 31 bits, as
Linux's obsolete `do_task_stat` fields do.

CPU accounting uses `Inactive`, `Kernel`, and `User` states. The user/kernel
boundary only publishes the current mode with one
relaxed atomic store; it takes no lock and reads no clock, so the syscall fast
path stays lock-free. Settlement instead happens at the local timer tick, at
context switch (switch-in starts a fresh interval, switch-out settles it) and on
demand when sampling. A settlement charges the unsettled interval to the mode
that is current when it runs, so this is not Linux generic vtime or Linux's
raw tick accounting; long kernel work still settles as kernel time because the trap path
publishes `Kernel` for it. Settlement is a compare-exchange on `last_wall_ns`
followed by an accumulator update. These operations are not a single snapshot:
remote settlement may race finalization and thread-group transfer. The final
accounting flag prevents restarting finalized accounting during cleanup;
it is separate from the earlier exit flag. Separate IRQ/steal/guest
accounting remains outside this model.

The process CPU clock instead sums the scheduler runtime of its published and
exiting members with the group's exited-runtime total. The membership lock
serializes live-to-exited transfers; a sequence check retries a snapshot if
membership changes between fixed-size batches. The caller refreshes
its own scheduler runtime before taking that lock; remote running tasks supply
their last committed values. An exiting last thread remains in the accounting
view through process cleanup, despite losing TID visibility, and transfers its
runtime before process-exit publication. Task references acquired for the
snapshot are retained until after each batch releases the membership lock;
the snapshot uses only a fixed-size stack buffer and does not allocate.

The thread CPU clock samples the same scheduler ledger directly. Reading
advances it: the running interval is charged before the value returns, so the
caller never observes its own elapsed on-CPU time as missing. The sampler
takes no thread handle because only the owning run queue may advance a
ledger; remote tasks contribute committed values only. This is the split
Linux draws with `CPUCLOCK_SCHED` versus `CPUCLOCK_PROF`/`CPUCLOCK_VIRT`:
the named CPU clocks read scheduler runtime, while the mode-settled
user/system accounting above still feeds views that need the split.

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
The mode-settled CPU time state machine depends on monotonic time. The named
CPU clocks depend on the scheduler runtime ledger instead:
`current_thread_cpu_clock` requires an initialized current task and run queue
(any task, user thread or kernel task), never sleeps, and must be called
without the local run-queue lock; `Process::process_cpu_time` requires task
context with neither the run-queue nor the membership lock held; it samples
in bounded, allocation-free batches and retries on membership changes. A periodic CPU
timer poll inherits both constraints through its sampling of these clocks.
Task runtime hooks depend on MM CPU residency and architecture initialization.

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

Thread accounting uses Inactive/User/Kernel. A mode change only publishes the
next state; elapsed time is charged to the mode current at the next tick,
switch, or sample. Thread exit and no-new-privileges
are separately published atomic flags. Group/session changes create or select a
group and publish group/session identity together; existing-group movement
requires the same session. The syscall owner checks additional leader/ID rules.

Each `Thread` also carries a seccomp `SeccompContext` (mode + classic-BPF
filter chain). It is installed only through `seccomp(2)` on the owning thread
(or a `TSYNC` broadcast to published siblings, which first requires every
sibling's chain to be a prefix of the caller's and re-validates that prefix
under each sibling's own lock before replacing its chain, so a synchronization
can never remove a restriction) and evaluated at syscall entry by `ksyscall`. A
lock-free mode cache mirrors the state mode so a disabled thread returns
`Allow` from one atomic load and skips building the `seccomp_data` snapshot
entirely; the cache is republished under the same lock whenever the mode
changes, so a non-disabled reader always re-locks and sees a consistent chain. A mode transition Linux refuses (`STRICT` ↔ `FILTER`) fails with `EINVAL`
rather than replacing the chain, and the whole chain is bounded by Linux
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
Last-thread cleanup is orchestrated by `posix-process`. The single
`finish_thread_exit` entry removes membership and reserves the last-thread
qualification under the membership lock only when this call actually removes
the final published task. Repeated exits return false; every later task
publication into the reserved process is rejected. After releasing the lock,
this entry retires the exiting task's global TID publication and returns whether
it reserved cleanup. It accepts no callback and performs no subsystem cleanup.
The caller marks the thread exiting before PI cleanup, keeps CPU accounting
open across later blocking cleanup, records final CPU time after membership
retirement, then performs explicit last-thread stages: optional `exit_tee`,
signal-handler and mm teardown,
optional runtime cleanup, fs/ns detach, and finally `complete_process_exit`.
The TID is no longer an external lookup or signal target during those stages;
current-thread runtime access and the process identity remain available until
their respective detach/completion boundaries. `exit_tee` runs while the current
user thread can still sleep and before its filesystem context is detached.
The retirement API requires schedulable task context without spinlock or
IRQ/preemption-disable guards. The last-thread state machine is identical with
and without TEE; only the named TEE stage is feature-gated.
SIGCHLD preparation occurs outside the domain lock; parent/exit-signal changes
cause retry. The domain transaction commits exit, autoreap and reparent; only
then are signal queueing and parent wakeup performed.
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
After `exec_de_thread` succeeds it commits the surviving leader's parent contract
as `SIGCHLD` (`Process::commit_exec_parent_contract`), before `execve` replaces
the executable file/table and image. A single-threaded exec does the same
immediately after its address-space exchange. This mirrors Linux `de_thread()`,
which establishes `SIGCHLD` before `unshare_files()` and `exec_mmap()`; a failure
after that point is fatal, so the parent must see the new contract even though
the old image cannot be resumed.

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
(`Thread::adopt_task_number`), re-keys its process signal registry entry
(routing still selects threads by number), and drops the retired leader's
`ExitedLeader` membership slot so the lifetime thread count describes the new
identity rather than both the old placeholder and its survivor. Membership and
cgroup still key on the stable token; the address-space exchange stays a pure
pointer swap in `replace_current_address_space`, with no identity work in its
callback.

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
`ProcessRuntime` holds the process namespace bundle in an `RwLock<Option<Arc<NsProxy>>>`.
`apply_prepared_unshare` installs a validated bundle (optionally swapping the
filesystem context for `NEWNS`/`CLONE_FS`); `apply_setns` swaps one namespace slot and retargets the fs
root/pwd into a mount namespace for `CLONE_NEWNS`. Both paths only install
state validated by `kns`, so the commit surface is small. `Process::nsproxy` and
the per-namespace accessors expose the bundle to `setns` and `/proc/<pid>/ns`.
Known limits include signal PID arguments carrying only caller-namespace
translation with a root fallback while lookup tables stay root-keyed, no
child-subreaper implementation, representative-thread credential snapshots,
read-credential ptrace checks backed by effective `CAP_SYS_PTRACE` but not a
complete ptrace access policy, and no full authorization policy in
signal/job-control facade helpers. The filesystem context counts process
owners: a `CLONE_FS` fork adds an owner and process exit releases it, so the
sharing check distinguishes cross-process sharing from the normal sharing
among threads of one thread group. Runtime construction adopts the owner
through `try_add_process_user`, which fails with `EAGAIN` while another process
is in an exclusive exec transition. The returned `ProcessUserGuard` is stored in
the same runtime slot as the context and released by a lock-free atomic
decrement, including during deferred runtime destruction. `Process::begin_exec`
performs the sharing test and sets `in_exec` atomically, and its `ExecFsGuard`
clears the flag on every exit path. `unshare` and `setns` install private fs
clones from
`clone_for_process`, so replacing a context never carries an exclusive-exec
marker into the replacement. `PR_SET_PDEATHSIG` records the
signal on the identity but delivery at parent-death reparenting is not yet
wired. The init process is bootstrapped through the low-level
`build_process_thread` path, so its backing `exe_file` is not stashed for
`/proc/1/exe` (a documented follow-up). Optional TEE/TIPC behavior exists
only with its Cargo feature enabled.

## Published tasks and unreleased thread-group members

`scheduler::process_task_count(&Process)` retains the published-task view:
exit unpublishes a TID immediately, so external targeting cannot reach teardown.
`procfs::thread_group_member_count(&Process)` has a different lifetime contract.
The member-slot state is Vacant, Published(Weak task), or
ExitedLeader(Arc<Cred>).
A published numeric group leader exits into ExitedLeader; nonleaders release
their slot at exit. Last-task detection, task enumeration and lookup ignore
ExitedLeader. The retained state
owns no task, runtime or mm and cannot be scheduled; it keeps the leader's
objective credential snapshot so `capget` on the thread-group number observes
the unreaped zombie leader's frozen capability sets instead of a surviving sibling's
per-thread sets. Wait consumption/autoreap clears it when the process identity
is retired, even if other Arc<Process> observers remain. Failed publication uses Vacant and contributes no membership.

A successful non-leader `exec` takeover is a real identity transition, not an
accounting-only alias. Once the survivor has moved the number directory, adopted
the group number and re-keyed its signal registry entry, it releases the old
leader's `ExitedLeader` slot. The lifetime count then remains one while the
survivor owns the process identity, instead of retaining both the placeholder
and its successor until reap. A failed precondition or publication move returns
before release, so the original leader slot remains authoritative. Member
enumeration and `/proc/<pid>/task` continue to list only published tasks and do
not expose the retired leader.

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
neither result promises an atomic thread-group snapshot. `TaskStat::num_threads`
and procfs `status` `Threads` consume the unreleased count, so a zombie leader
counts as one until reap rather than being zeroed by task publication. Reap
clears retained membership under process-domain -> membership -> slot order.
Task exit releases membership locks before global TID removal and acquires no
process-domain lock while holding them. There is no procfs max(1) or
zombie-state compensation.

Regression definitions extend the real exit/reap test to distinguish live count
0 from lifetime count 1 before reap and 0 after reap with strong observers still
held. Another test explicitly exits the leader before a sibling and checks that
only the leader remains until reap. A takeover test drives the transition
directly through `Process::assume_leader_identity`: it starts from an exited
leader plus one published survivor, verifies both the publication move and the
post-takeover lifetime count of one, including the procfs accessors that consume
it. These definitions exercise real member exit and reap with never-run tasks;
the takeover fixture invokes the identity transition directly rather than a full
`execve` syscall. The surrounding exec metadata and signal transitions have
their own unit fixtures.

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

`LiveAddressSpace::resolve_page_fault` delegates sleepable fault resolution to
its existing active MM user. The capability pins the captured image across a
backing-admission wait. The MM owner drops serialization before the runtime waits,
then repeats lookup and access checks. User execution faults, ordinary user
copies and explicit user-word fault-in use this common entry.

The live mapping guard forwards relocation's target-replacement policy while
permissions stay owned by the source snapshot. MM removes a fixed target and
discarded source tail before preparing the retained source; these destructive
steps are not enclosed in a transient admission retry.

`LiveAddressSpace::msync_range()` delegates through its existing `MmUserHandle`.
The captured image stays live across file persistence, while memspace drops its
mutex for the runtime hook and repeats VMA lookup after each synchronous range.
No process-binding lock or new sync request state spans that wait.
