# SMP freezing: audit and proposed protocol

Status: audit baseline and staged design. Task/worker freezing is now implemented
on the replacement branch using Frozen state, incoming-stack confirmation,
request identity capture and completed-exit registration. Reversible CPU parking
and physical SMP suspend remain proposals. The audit below describes the original
limitations, not defects in the newly revised paths. This document audits
local commit `4cc6237c` and the worker changes at `3f7f4547` (!1256). It does not
authorize removing the existing `MultipleCpus` checks. No runtime code changes
or successful QEMU execution are claimed by this proposal.

## Scope and boundaries

There are two separate deliverables:

1. Freeze an explicit task set, then all admitted ordinary tasks, across CPUs.
   Timers, interrupts and scheduler infrastructure remain available.
2. Suspend the system after those tasks and selected queues are quiescent.
   This additionally needs reversible CPU rendezvous, local-event handling,
   interrupt/controller exclusion and a real platform resume backend.

A successful task freeze does not certify that a CPU, IRQ handler, idle path,
exempt worker or device has stopped accessing shared resources. A four-vCPU
software integration test is not proof of physical S3 or guest platform sleep.

## Code audit

Paths below are relative to the repository root; function names are the stable
references for follow-up implementation reviews.

| Area | Existing mechanism | SMP conclusion |
| --- | --- | --- |
| `task/ktask/src/api.rs::wake_task` | Selects a wake run queue and unblocks a blocked task. Running/ready tasks are not kicked by this API alone. | Reuse scheduler wake paths; do not treat every target as a blocked waiter. |
| `task/ktask/src/run_queue/rq.rs::put_task_with_state`, `clear_prev_task_on_cpu` | Blocked-to-Ready transition, deferred enqueue, `wake_cpu` and `on_cpu` handshake with SeqCst operations/fences. | Preserve this protocol; no freezer-side direct state stores or remote enqueue. |
| `task/ktask/src/park.rs::park_task_unless` and `rq.rs::blocked_resched` | Wake latch plus predicate recheck after Blocked publication closes the running-to-blocked window. | Reuse for opt-in waits; a state sample alone is not a safe-point acknowledgment. |
| `rq.rs::kick_running_task`, `request_remote_resched` | Remote reschedule IPI support exists. | Candidate request-delivery mechanism. Must verify user-return progress under NOHZ, migration and IPI failure; do not inject an application signal as a freezer wake. |
| `posix/process/src/runtime.rs::run_user_thread_loop`, `task.rs::task_entry` | User-return and first-entry checkpoints exist. | Retain them. Arbitrary blocked syscalls or kernel tasks still require audited safe points or timeout. |
| `task/ktask/src/freezer.rs::checkpoint` | Publishes `is_confirmed` before notifying progress and waiting for thaw. | A remote coordinator could observe acknowledgment while this CPU still executes freezer/wait/scheduler code. Define a stronger scheduler boundary before using confirmation as physical task quiescence. |
| `task.rs::notify_exit`, `rq.rs::exit_current` | Exited publication, registry retirement and notification precede final switch-out. | Exited is not proof that the outgoing stack and cleanup have become inactive. |
| `rq.rs::finish_task_switch` | Runs on the incoming stack; performs exit cleanup before clearing the outgoing `on_cpu` marker. | Candidate integration boundary. Audit ordering with wake enqueue and outgoing reference destruction, not just the single marker store. |
| `task/ktask/src/freezer/system.rs` | Registry lock serializes first activation, snapshot closure and retirement; closed admission retains new activations. | Useful base. Retirement can remove a task before its final switch; admission closure also needs an in-flight activation/retirement audit. |
| !1256 `PreparedTaskWait::park_freezable` | Scoped opt-in, checkpoint and post-Blocked request recheck; request publication wakes opt-in waiters. | Retain the lost-wake structure. Its `wake_task(task, false)` is not sufficient evidence of prompt remote-idle progress; audit reschedule delivery. |
| `task/ktask/src/stop_machine.rs::stop_one_cpu[_nowait]` | Stop-class task runs a callback with IRQs disabled; completion follows callback return. | Possible execution substrate, not a suspend guard. Sequential synchronous calls cannot hold all other CPUs parked simultaneously. |
| `arch/khal/src/power.rs::SmpStopIf`, `arch/kipi/src/stop.rs::ack_and_park` | Terminal, best-effort stop; timeout may leave targets running, acknowledged targets never return. | Unsuitable for suspend/resume. Do not reuse terminal stop state or its non-returning CPU loop. |
| `task/ktask/src/timer_suspend.rs` | CPU-bound local event token; operations belong on the owning CPU with IRQs masked. | Never move tokens to the coordinator and restore another CPU's events there. |

These are reasons to retain the UP guard, not demonstrated bugs in its supported
single-present-CPU contract. In particular, task safe-point acknowledgment and
complete CPU quiescence must not acquire interchangeable meanings.

## Proposed task-freeze protocol

### Request identity and admission

Keep one owned request object per task per round and strong task references for
the round. The existing round/request Arc identities are a usable generation
boundary; a separate numeric generation is not required merely for naming.

Before system snapshot, close first-activation admission. A task admitted before
closure must either be in the snapshot or represented by an in-flight record
that the snapshot joins. A task retired before closure must either have completed
its outgoing cleanup or remain represented until that cleanup completes. Do not
wait while holding REGISTRY or a run-queue lock. If retirement unlinking moves
to a switch-completion boundary, review its IRQ context and lock order first.

Explicit-set freezing retains its narrower contract: its caller owns set
stability. System freezing is responsible for the complete admitted set.

### Delivery and safe-point entry

Publish the request under the task request lock, then release that lock before
any scheduler operation. Wake only waits explicitly enrolled as freezer-safe.
Use an audited reschedule kick for a running remote target, and ensure a ready
target on an idle CPU receives a scheduling notification. Account for affinity
changes and switch-in races; an IPI is a hint, never an acknowledgment.

Do not force ordinary I/O waits, lock waits or callback bodies to acknowledge.
The next audited checkpoint, or completed exit, supplies progress. Failure to
reach it is a bounded preparation failure. The coordinator must not retain a
lock or resource that the target needs to reach that point.

### Confirmation and retained hold

Proposed logical states, owned by the request identity:

```text
Requested -> EnteringSafePoint -> Held -> Released
     \              \             /
      +--------------+-----------+----> Released (abort)

Requested -> ExitPending -> ExitComplete
```

`Held` means the outgoing execution has completed the required scheduler handoff
and cannot return to callback/user execution before release. It is stronger than
the current early `is_confirmed` store. Commit it from an audited switch boundary
or an equivalent scheduler handshake; do not publish it before blocking and
later hope that another CPU has observed `on_cpu=false`.

The first implementation must select and prove a wake policy. A dedicated
freezer hold should retain ordinary wake hints without granting ordinary
execution. If generic wakeups can schedule a frozen waiter to recheck release,
the contract must permit that control execution and the later CPU rendezvous
must exclude it before hardware transitions. A one-time `!on_cpu` observation
does not prove that it stays off CPU. No acknowledgment notification, reference
destructor or deferred wake may secretly occur after a boundary claiming that
the entire CPU has stopped touching shared state.

Release publication and confirmation commit must serialize for the same request.
A release that wins prevents late entry into Held. A target that sampled an old
request may only observe/release that old round; it must not clear a newer slot.
Use the existing scheduler ownership and lock ordering rather than independent
atomic booleans for a multi-field transition. Avoid new allocation or blocking
notifications under run-queue locks.

`ExitComplete` must be published after the audited terminal cleanup boundary.
Review registry retirement together with this state so already-unlinked exiting
tasks cannot disappear from the system quiescence proof.

### Abort, thaw and deadlines

Before hardware changes, a timeout or cancellation releases this round's holds,
withdraws its requests and drains outstanding commit/release activity before a
new round can reuse slots or guards. Late arrivals must observe release without
parking indefinitely. Return semantics must state whether targets merely become
runnable or have completed their thaw control path; do not wait for ordinary
application work to finish as part of thaw.

Use advancing MONOTONIC time and available timer IRQs during preparation. Keep
timeout infrastructure and progress notifications outside the frozen set. Report
the target/phase that failed without turning diagnostics into a dependency of
release. Successful guards remain held despite a later cancellation until PM
cleanup explicitly releases them.

## Proposed CPU rendezvous: a later deliverable

Keep the existing UP-only PM entry until this separate protocol is implemented.
Define a ready participant mask, coordinator CPU ownership and exclusion of CPU
topology changes. A raw present-CPU count does not establish helper readiness.

Reserve all request slots, lifetime owners and local-event storage before
parking CPUs. Audit RCU progress, IRQ/BH activity, exempt tasks, DMA, watchdogs
and platform callback dependencies before choosing the final rendezvous point.
Starting a generic stop callback while another CPU can hold a lock needed by
the coordinator is not an acceptable quiescence protocol.

Each participant needs explicit preparation, parked, restore-ready and released
states tied to one round. Local clock events and controller state remain
CPU-owned. Publish a parked acknowledgment only after the agreed final shared
accesses; the parked control loop may access only its retained protocol state
and a platform-approved wake mechanism. An IRQ-off loop cannot rely on ordinary
reschedule IPIs to exit. Busy polling is a software test backend, not a physical
power-loss resume mechanism.

On partial-entry timeout, publish abort, prevent unentered/late callbacks from
parking, resume every participant that entered, and drain every queued callback
before releasing its storage. A timeout is not permission to proceed to sleep
with an unacknowledged CPU or free memory still referenced by its callback.

After platform entry, recovery must have a defined infallible or terminal-failure
policy; it cannot return to ordinary execution with a CPU indefinitely parked.
Shared clocks/controller resources must be restored before dependent CPU-local
operations or ordinary dispatch. Keep queue/task guards and admission closed
through all required restoration. The exact device-versus-CPU ordering remains
platform-dependent and must be resolved against real callback dependencies.

## Implementation sequence and acceptance tests

1. Scheduler-backed SMP explicit task freezer: request lifetime, notification,
   safe-point commit, completed-exit proof and rollback. Preserve UP behavior.
2. System admission/retirement closure and audited worker participation from
   !1256. Keep !1255's PM topology restriction at this stage.
3. Four-vCPU software integration of task and queue preparation with device
   callback probes. All vCPUs stay online; no clock freeze or platform sleep is
   claimed. Audit worker exemptions: do not freeze a queue's required workers
   before waiting for its claimed callbacks to finish.
4. Reversible CPU rendezvous with local-event restoration, followed by an actual
   platform suspend backend and its own end-to-end evidence.

Required QEMU assertions for stages 1–3:

| Case | Required observation |
| --- | --- |
| Cross-CPU execution | Pin real targets to distinct logical CPUs and record execution CPU IDs; fail if four CPUs were requested but targets ran only locally. |
| Idle cooperative worker | Freeze without a work notification; enqueue work during Held; callback counts stay unchanged until release and advance afterwards. |
| Executing callback | Hold a callback at an explicit gate; freeze cannot succeed early; opening the gate permits cleanup and then confirmation. |
| Publication-before-switch window | Deterministic test hooks hold EnteringSafePoint before switch completion; the remote coordinator must not return success. |
| Exit and registry retirement | Gate the outgoing exit tail; explicit/system freeze cannot claim complete quiescence before the required terminal boundary. |
| Wake/block races | Exercise requests before park, after Blocked publication and across remote wake enqueue; retain work hints without double execution. |
| Running user target | Under NOHZ, request delivery brings a real user task to its user-return checkpoint without fabricated signals. Separate guest userspace fixture required. |
| Ordinary blocked target | No safe point means timeout, not false confirmation; rollback leaves unrelated targets runnable. |
| New activation | Construct/activate remotely during closed admission; entry cannot execute until gate release. Include the admitted-before-enqueue window. |
| Cancellation and retry | Gate late confirmation and thaw, cancel the round, then start a fresh round; old requests cannot freeze or release new ownership. |
| Affinity/migration | Request and cancel during allowed migration; each task executes on at most one CPU and no acknowledgment is lost. |
| Default policy | KeepRunning services do not implicitly acknowledge; PM dependencies remain usable on separate services. |

Use event handshakes and bounded test-run timeouts rather than timing sleeps as
the oracle. Multi-CPU cases must execute, not silently return Ignored. Preserve
the exact commit, resolved config, emulator command, per-case output and final
guest pass marker. Run through the Makefile/Kconfig flow; a successful build or
four-vCPU boot alone is not evidence of freeze functionality.

For stage 4 additionally test partial CPU arrival, late arrival after abort,
remote local-event restoration, repeated rounds and resume failure policy. Real
sleep testing needs a suitable platform backend and must separately establish
timekeeping and device recovery behavior.

## Decisions still required before runtime edits

- Choose the concrete scheduler hold/wake representation and switch-completion
  hook; audit every added lock edge and deferred destruction site.
- Define how in-flight activation and retirement remain covered by system freeze.
- Prove remote running/ready/idle notification progress, including IPI failure.
- Select the supported platform and its CPU retention/resume mechanism before
  defining final suspend rendezvous APIs.

No remote PR, CI job or review state was changed for this audit. Existing unit
tests and the earlier failed root-disk-less QEMU boot do not satisfy the SMP
acceptance matrix above.
