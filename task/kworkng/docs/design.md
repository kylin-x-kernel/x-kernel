# Prepared Workqueue API

## Purpose And Scope

`kworkng` provides service initialization, default queue selectors, independent
queues and safe ownership wrappers above `kworkqueue_ng` and `kworkerpool_ng`.
Prepared submission reuses stable storage, avoiding per-submission allocation
from task, BH and hard IRQ context. NMI is unsupported. Preparation and
synchronous waits require sleepable task context.

This crate does not activate itself. `kruntime` initializes it once after all CPU
services are ready and before driver initialization. Network polling uses a
prepared static Work on a private NormalQueue. There is no legacy selection
switch or GC extension. The legacy workqueue crates
and their task-accounting hooks have been removed.

The default-off `bench` feature includes `benches/kernel.rs` in release kernels.
The boot integration invokes it after runtime initialization instead of launching
userspace, then powers off. It exercises the same published services without
replacing actions or registries. See `benches/README.md` for scope and commands.
The runner's optional `klogger` dependency supplies unfiltered console output
for results and progress, so boot log-level overrides cannot hide acceptance
records. It does not change the system-wide log filter.

Scope: `services.rs`, `defaults.rs`, `queue.rs`, `normal_queue.rs`, `owned.rs`,
`work_handle.rs` and `delayed_work_handle.rs`. Queue state, timer transfer, color
accounting and execution scheduling belong to the two subordinate crates.

## Architecture

```text
runtime::init
  -> Services: NormalServices + BottomHalfServices + three default Queues
  -> atomic Tasklet/High action registration
  -> SERVICES: Once<Services> publishes complete readiness

DefaultQueue ----------------> submission only; no default shutdown authority
create_queue ----------------> Arc<WorkQueue>: independent flush/close authority
NormalQueue -----------------> normal-task-only queue authority
WorkHandle / DelayedWorkHandle
  -> Arc<Owner<W>>: shared closure gate + retained default reclamation queue
     -> Arc<Storage<W>>: pinned W + prepared one-shot reclamation Work
```

`Work`, `DelayedWork` and configuration/error types are re-exported without a
second management model. `DefaultQueue` does not expose its underlying Queue.
`NormalQueue` cannot be constructed from an arbitrary BH queue. Safe static and
owned callbacks therefore run only in normal tasks, never inline or in BH context.
Borrowed publication and all BH publication retain the core unsafe contract.
Its queue field is module-private; sibling modules can only borrow it through a
read-only accessor. Owner's reclamation queue is also private and selected only
by its constructor from the permanent normal default, never a caller-supplied queue.

## Dependencies

- `kworkqueue_ng`: configuration, borrowed Work, delayed timer lifecycle, queue
  admission/drain, optional diagnostic directory and terminal retirement hook.
- `kworkerpool_ng`: prepared per-CPU normal/BH execution services.
- `kirq`: group action registration and CPU-pinned softirq invocation.
- `ktask`, `khal`, `kcpu_id_map`, `ktime_types`: context checks, CPU identity,
  boot map, scheduling and time; real task/timer fixtures also use these providers.
- `klazy`: one-time service publication and nonblocking Acquire reads.
- `kspin`: IRQ-compatible handle closure gates and isolated test fixture selection.
- `kbuild_config`: independent default normal/BH release limits; neither starts services.
- Optional `watchdog`/`kiface`: task-context warning contract, provided by this
  crate when `stall-check` is enabled. The per-CPU watchdog task invokes it.
- `log` and `unittest`: diagnostics and kernel test registration.

## Initialization And Failure

Call `runtime::init` after every boot CPU has scheduler, timer and softirq support,
in sleepable context before exposing producers. CPU hotplug and production
unregistration are unsupported. Legacy Tasklet/High actions must not already be
installed when selecting this implementation.

Preparation validates all default configurations before starting workers, prepares
BH services, starts normal managers and binds all three default queues. A candidate
is private until installation. `SERVICES.try_call_once` serializes installation,
registers both BH actions atomically, then publishes the complete service set with
Release ordering. Readers use nonblocking `get()` with Acquire ordering; they
neither take a registry lock nor clone a service/queue Arc just to look it up.
A conflicting action leaves both slots unchanged and Once uninitialized, allowing
retry. Duplicate initialization leaves the published set untouched. Both failures
join candidate workers after leaving Once initialization, without a spinlock.
Only the short action-registration/publication section disables preemption, so a
same-CPU initializer cannot preempt and spin on its unfinished predecessor. This
protection does not cover preparation, rollback joins or normal read paths.

`kirq::open_softirqs` validates length, duplicate vectors and occupied slots under
one action-table lock before writing anything. It neither invokes callbacks nor
changes pending bits. An action invoked between registration and service publication
returns immediately when `get()` is empty. Default submission and private queue
creation are still unready, so no candidate work has been accepted in that window.
After publication, each accepted BH submission supplies its normal notification.
The softirq runner already pins the CPU; the action adds no preemption guard.

Successful services live for the kernel lifetime. Duplicate initialization fails
without replacing them. Allocation failure follows kernel allocator policy.
Unready submissions return `NotReady` without logging, including from IRQ context;
initialization failures are logged by `runtime::init`, not by each producer retry.
`runtime` also re-exports the provider error types carried by `InitError`, so
integrators can match startup failures without importing the provider crate.

## Public Lifetime Choices

| Choice | Storage responsibility | Execution context | Teardown |
| --- | --- | --- | --- |
| Borrowed pinned Work/DelayedWork | External caller | Normal or BH | Stop/join producers and API calls, then synchronized ordinary/physical drain |
| Static Work | Kernel-lifetime shared reference | Normal only | No storage release; still obey callback dependency rules |
| WorkHandle | Shared Owner and stable Storage | Normal only | Explicit shutdown closes all clones; final Drop schedules reclamation |
| DelayedWorkHandle | Same Owner plus prepared timer in W | Normal only | Shutdown/reclamation drains Work and physical timer delivery |

Preparation may allocate. Handle clone, prepared submission, modification and
nonwaiting cancellation do not allocate. Synchronous waits may prepare receipts
and park. Final handle release queues its already prepared reclamation Work; it
does not synchronously wait or invoke capture destructors in the caller's context.
It may revoke pending work and does not promise callback execution.

Only the completion primitive and task-wait machinery are allocation-free.
Synchronous drain still allocates completion-observation receipts and containers.
Per ordinary Work preparation attempt, N captured-slot candidates (0–2) require N
receipt Arcs plus two Vec buffers for N > 0: zero, three or four allocations.
Capture-growth retries may repeat them. Delayed cancellation uses the same Work
drain before joining its timer. The final reclamation hook's Drain::drain can
therefore allocate these wait resources even though enqueueing reclamation does
not allocate. Allocator failure remains fatal under kernel allocator policy;
allocation-free entire drain or final reclamation is explicitly not guaranteed.
The core queue design documents the additional queue/delayed-flush allocation counts.

## Ownership And Closure

All clones share `Owner::is_closed`. Submission holds that IRQ-safe gate until the
underlying admission returns. Shutdown validates sleepability, permanently closes
the gate and releases it before draining W. Thus no pre-closure submitter can
publish after drain begins. Other drain errors leave the gate closed for retry;
invalid context leaves it open. Nonclosing cancel/flush retain the core's scoped
semantics and do not prevent later submissions.

The final Owner destructor transfers exactly one strong Storage reference as a
raw Arc into its private one-shot reclamation Work. Its own Storage reference also
survives until publication returns, including when remote retirement wins the race.
After that Work retires, the terminal hook reconstructs the transferred Arc, drains
W, then destroys W in place. The last allocation release may occur on the publishing
CPU, but business captures are destroyed in the normal reclamation task.

The `UnsafeCell<ManuallyDrop<W>>` allows this field's final destruction while a
publisher still borrows the separate reclamation field. No mutable W access or
reclamation Work address is exported. The terminal action is outside queue flush
accounting: flushing the default queue is not a reclamation-completion API.

Forgotten handles and strong callback/handle cycles can leak, but cannot release
storage early. Shutdown is not a promise that captures are already destroyed;
remaining handles retain them until final release. Progress requires callbacks to
return and normal providers to remain available. No panic/unwind recovery exists.

## Queue Shutdown And Delayed Work

Private queue closure drains ordinary accepted work and source inspections but
does not stop shared services or other queues. Before closure, callers must stop
producers and drain every delayed object, including timer-only arrangements.
`DelayedWorkHandle::shutdown` provides that drain for owned objects. Dropping an
Arc<WorkQueue> or NormalQueue is not implicit synchronous shutdown.

## Optional Diagnostics

`KFEAT_KWORK_STALL_CHECK` selects `kfeat/kwork_stall_check` and requires
`KFEAT_WATCHDOG`. The Cargo feature also enables `kfeat/watchdog`, keeping the
inspection provider and watchdog boot initialization together. The unit-test
entry does not force this integration feature on. Core queue diagnostic tests
can still enable `kworkqueue_ng/stall-check` without linking watchdog.

`self-wait-check` forwards core callback-identity checks. It does not diagnose
arbitrary dependency cycles or destructor-originated waits. `stall-check` attaches
named records before queue exposure and implements `watchdog::WorkqueueStallInspectorIf`.
Inspection borrows the published services and delegates to the core directory.
Unready services and a busy diagnostic directory can be skipped; output is warning-only.
There is no reporting deadline, automatic recovery or GC work. The watchdog task
inspects between its soft-lockup timestamp update and its four-second sleep;
timer IRQ and NMI handlers never inspect queue state.

## Validation

Without the `system` feature, serial kernel tests select separate Arc-owned Once instances and restore legacy BH
actions. Selection uses a test-only lock; every read retains the chosen instance
for its closure, so no borrowed service/queue reference escapes fixture lifetime.
Production Once is never reset. Installation and reads exercise the same Once
protocol in both builds; only test fixture selection adds a lock and Arc clone.
They cover readiness rejection, atomic action registration, rollback, every boot
CPU/category, independent queue closure, static publication, shared handle closure,
racing producers/retiming, last release in IRQ-masked context and from the callback,
and delayed capture destruction after timer/callback drain. Tests also dispatch
both actions before publication, verify rejection then successful later submission,
and observe real diagnostic backlog before and after cancellation. Scoped service teardown
joins normal workers so terminal reclamation tails finish before the next fixture.
The runtime enables `system`, which selects live-service tests instead of these
replaceable fixtures. This feature changes test selection only, not production
implementation selection. Live tests leave the permanent Once and both BH
actions installed, covering all CPU/categories, safe ownership, deferred release
and delayed work. `kruntime` additionally probes real hard IRQ publication,
caller-CPU allocation counters, timer cancellation races and payload visibility.

## Normal finalization provider

The permanent executor can be prepared before worker services. Successful
runtime initialization binds its normal default queue through a Once. It owns
one borrowed Work and an IRQ-safe intrusive pending list from
`intrusive-collections`. Its Arc-owning adapter uses the Carrier's embedded
`LinkedListAtomicLink`; enqueue/dequeue allocates no separate list node.
Per-object Carrier also contains a prepared NMI irq_work node, a static executor
reference and the one-shot Finalization slot. No per-object Work, thread or queue
is allocated. The pending lock still serializes all list operations; atomic links
do not make the queue lock-free or permit NMI to access it directly.

Submission writes the token, transfers a cloned Arc into the list, releases its
lock, then notifies the shared Work if bound. Every bound enqueue notifies: no additional
scheduled-bit protocol is needed. AlreadyQueued means an unclaimed occurrence
already exists, not a dropped node; other rejection is fatal while the permanent
list retains the obligation. Work accepts a successor while running and never
executes two occurrences of that same Work concurrently. Implicit routing retains
unfinished CPU ownership rather than requesting a conflicting CPU.

Each callback removes and destroys at most 64 nodes, taking the list lock only
around removal. Destructors, node frees and notifications run outside it.
Remaining backlog schedules another occurrence. If an enqueue races an empty
observation or the final backlog check, its own notification supplies the next
occurrence; if it sees AlreadyQueued that occurrence observes the published node.
The batch bound limits node count, not duration or a single destructor's runtime.

Payload destruction runs only on normal workers, including when submission
originates in hardirq or softirq. A sleeping Normal destructor serializes this
entire Normal finalizer: it MUST NOT wait for another finalization on the same
executor (including indirect dependencies). It can enqueue more nodes without
waiting. Shared worker-pool dependency restrictions still apply.

Payload Box and standard Arc/Weak counting semantics remain unchanged. The publishing Arc
protects the lightweight node while a consumer may finish early; list ownership
keeps it live independently of the public identity. Only the private consumer
takes the token. No user cancellation, flush or executor teardown is exposed.

NMI DeferredArc retirement is the narrow exception to the public work-submission
NMI prohibition. Each carrier has a first-field one-shot IrqWork. Before taking
any ordinary lock, submit detects NMI and transfers its Arc::into_raw owner to
local ordinary IRQ; the callback restores it and runs the existing publish path.
Identity storage has a separate prefix containing IrqWork and the global layout.
Its original allocation token survives separately in the Arc allocator copies;
ordinary release frees directly, while NMI release queues the prefix for ordinary
IRQ free. Neither IRQ callback executes payload destruction. Unready/unsupported
NMI routing is fatal after relinquishing the raw token, not inline fallback.
No new worker or queue was added; the existing shared-finalizer dependencies remain.

Unittest seams check completion before publisher return and enqueue after the
consumer observed empty but before callback return. Seams are absent in production.
The former explicit remote CPU override is removed: shared unfinished Work owns
its CPU, so the publication test no longer claims forced remote completion.
The 193-node normal backlog spans the finalizer's 64-node batch; existing real IRQ,
Weak, and busy-pool regressions remain applicable, subject to shared routing.

### Early startup handoff

The unsafe runtime::init_finalization_preparation installs the provider during
serialized primary startup after allocator/per-CPU/IRQ-lock setup. The hidden
kfinalize::service::new_integration entry prepares fresh carriers concurrently on
independently initialized CPUs, including audited startup NoPreempt paths. It
requires neither current Task nor installed workers and never creates tasks or
waits for execution. Consumers need no kworkng dependency.
Ordinary DeferredArc::new keeps its ready-provider/sleepable-task checks.

The same permanent list is the startup inbox and the running backlog; there is
no detached batch or second list to migrate. Before binding, notify returns
without touching the token. After successful service publication, install first
publishes the normal preparation entry, then the queue, and then checks pending
under its lock, notifying outside it. Preparation must precede any notification:
an early destructor may immediately prepare another DeferredArc.
A producer that observes no queue has already linked its node, so the binder's
subsequent lock check sees that node. A later producer sees the bound queue and
notifies itself. Duplicate notifications coalesce; ownership never moves during
binding. If initialization fails, services::init does not bind, and obligations
remain in permanent storage for a later successful attempt. Startup/rollback
must never wait for that physical reclamation.
