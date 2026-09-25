# Prepared Work Execution Services

## Purpose And Scope

`kworkerpool_ng` supplies CPU-bound execution services for prepared work.
It owns source dispatch, normal worker capacity, worker startup/retirement,
and bounded bottom-half (BH) execution. It does not own concrete work objects,
queue admission, cancellation, flush, global registration or system readiness.
A management adapter implements `PartitionWorkSource` and `AcquiredExecution`.
System integration retains the services and installs BH actions before publication.
This stage leaves the existing production workqueue unchanged.

All modules are private; the crate root re-exports the dispatcher/source types,
acquisition/execution traits, normal services, BH services and their error types.
Unit tests live under their owning modules using standard Rust module paths.
Under `cfg(unittest)`, `test_support` exports two controlled fixtures consumed by
queue-core integration tests: a held source inspection and a local BH service
binding. Production constructors retain CPU validation and notification setup.

| Source | Responsibility |
| --- | --- |
| `src/dispatch.rs`, `src/source.rs` | FIFO membership and retained source demand |
| `src/source_detachment.rs` | Source detachment completion after inspection-reference retirement |
| `src/execution.rs` | Management-to-execution contracts |
| `src/pool.rs`, `src/worker.rs` | Fixed worker slots and lifecycle states |
| `src/normal.rs`, `src/normal/wake.rs` | Worker acquisition, parking and prepared notification |
| `src/normal/manager.rs` | Task creation/reaping, idle retirement and deadline wake |
| `src/normal/runtime.rs`, `src/normal/services.rs` | Joined startup/stop and complete boot-CPU coverage |
| `src/bottom_half.rs`, `src/bottom_half/wake.rs`, `src/bottom_half/services.rs` | CPU/priority-bound BH dispatch and detached coverage |

## Architecture And Dependencies

`PartitionWorkSource::acquire` returns an optional execution and a
`SourceRemainder` snapshot. `Empty` means no further eligible demand was
observed; `More` retains the source for another inspection. Either value can
accompany an acquired execution or an empty attempt. The remainder stays typed
through inspection completion and source requeueing; worker occupancy is derived
only from the optional execution. Concurrent notifications remain independently
retained, so `Empty` is not a queue-drained guarantee.

```text
management adapter -> PartitionDispatch -> source inspection -> AcquiredExecution
                            ^                                  |
                            |                                  v
                    prepared notification             callback and cleanup
                            |
                  normal worker / BH pass
```

The adapter prepares a source, publishes demand with `notify`, then invokes the
matching service Waker. The worker/BH pass calls `acquire` without dispatch locks,
finishes source inspection, and consumes any acquired execution through `run`.
An execution must remain valid independently of the temporary source reference.

The crate uses `intrusive-collections` for Arc-owned intrusive nodes, `kspin`
for locking and CPU pinning, and `kpoll` for completion receipts. `ktask`
provides task creation, affinity, durable waits and prepared timers;
`ktime_types` supplies monotonic time and `kbuild_config` supplies stack size.
`kcpu_id_map` and `khal` supply boot CPU identity and time.
`kirq` supplies effective softirq context and local notification;
kipi's built-in BH APIs supply allocation-free remote notification without
requiring scheduler or timer notification providers.
`unittest` supplies kernel test registration.

## Source State And Retirement

`PartitionDispatchState` is protected together with list membership by the
owning dispatcher's `SpinNoIrq` lock:

| State/event | Result |
| --- | --- |
| Idle + notify | Queued; insert once |
| Queued + notify | Remain Queued; coalesce |
| Queued + selection | Inspecting; remove from list |
| Inspecting + notify | InspectingNotified |
| InspectingNotified + notify | Remain InspectingNotified |
| Inspecting + finish | Queued if more demand, otherwise Idle |
| InspectingNotified + finish | Queued even after an empty acquisition |
| Idle/Queued + detach | Detached; remove queued node and close accounting once |
| Inspecting/InspectingNotified + detach | InspectingDetached; close accounting once |
| InspectingDetached + finish | Detached; never requeue |
| Detached/InspectingDetached + notify or repeated detach | No new demand or closure |

Each queued source owns an Arc transferred to its inspection. Finishing returns
remaining demand to the FIFO tail before invoking the execution. Dropping an
unfinished inspection requests another turn unless detached.
A Weak owner identity prevents foreign insertion and strong reference cycles.

`SourceDetachment` counts outstanding inspections, including references being
released outside the dispatch lock. Detachment completion requires closure and
zero outstanding inspections. It does not wait for acquired executions or
publisher-held references. Callers may observe/register the receipt but must not
complete or reset it. Queue shutdown must drain accepted work separately.

## Normal Worker Lifecycle

`NormalServices::start` requires initialized task/timer facilities on every boot
CPU. It validates context and idle interval, starts one private runtime per CPU,
and joins the successful prefix on a recoverable startup failure. Success returns
complete coverage only. `binding` returns a matching dispatcher and prepared Waker.

Each runtime retains its manager task. The manager exclusively owns worker task
references; the service owns capacity and weak-task Wakers. Worker closures retain
the service, not manager ownership, avoiding a strong reference cycle.

`WorkerState` transitions are:
- Empty -> Creating reserves capacity before task allocation.
- Creating -> Prepared precedes activation; the started worker publishes Available.
- Available -> Selecting -> Occupied or Available follows acquisition.
- Occupied -> Available occurs after execution and cleanup, including callback sleep.
- Available -> Parked records idle time after a protected empty-demand observation.
- Parked -> Available reserves a wake and discards the old idle timestamp.
- Parked -> Retiring follows idle policy or quiescent service stop.
- Retiring -> Empty follows published task exit and manager reference release.
- An unactivated Creating/Prepared attempt can return to Empty after disposal.

The fixed positive slot limit includes all nonempty states. The manager grows
capacity only when demand has no usable worker and a slot is available.
Saturation retains source demand. Excess idle workers retire while one idle
baseline remains; occupied workers never retire. Checked idle deadlines outside
the timer range are omitted, leaving the worker usable.

The manager prepares one task wait and one timer. Its timer callback only wakes
the manager. Park notifications reschedule idle policy; retiring tasks use a
positive 1 ms exit recheck. A quiet baseline needs no periodic timer.
A manager-local flag records a cancellation obligation after a successful start.
When no deadline is needed, synchronous cancellation runs only if that obligation
exists, then clears it. Timer expiry does not clear the flag; final timer Drop
still drains delivery unconditionally. Repeated no-deadline passes avoid timer
owner-lock acquisition.
Published task exit permits slot reuse, not a claim that scheduler GC has already
released the task stack.

## BH Execution

Single-service and collection preparation report the exported
`BottomHalfPrepareError::CpuUnavailable` for absent boot CPUs. Transport errors
remain internal to notification delivery, not the preparation API.

`BottomHalfServices::prepare` creates both priorities on every boot CPU without
registering actions. Failure releases all detached storage. Normal priority maps
to Tasklet and high priority to High; a system integrator owns vector registration.

A prepared BH Waker stores only CPU/priority. With preemption disabled, local
delivery raises softirq and remote delivery sets a fixed IPI reason. Neither path
allocates a closure or executes work inline. Remote delivery requires initialized,
enabled destination IPIs and installed actions; CPU-map presence is not readiness.

`run_pass` validates effective softirq context and CPU before selection, so a
hardirq interrupting softirq is rejected. The provider serializes passes.
The kirq softirq runner holds preemption protection across the action, so
`run_pass` needs no additional guard to keep the validated CPU stable.
The budget counts source turns, including empty acquisitions, not callback time.
Remaining demand requests another pass; a producer racing an empty exit supplies
its own notification. BH callbacks must not sleep.

## Concurrency And Context

Lock order is dispatch -> workers, or dispatch -> source completion, never reversed.
The empty-demand observation and Parked publication share dispatch/worker protection;
`PreparedTaskWait` preserves a wake between unlocking and actual sleep.
Callbacks, adapter inspection, source payload destruction and Waker invocation
occur outside service locks. Pure state helpers require exclusive mutable access.

Source/service preparation allocates in task context. After preparation, framework
notify/select/finish/detach and notification delivery allocate no new storage.
Worker growth allocates only in manager task context. Adapter/callback behavior
is a separate caller responsibility. Normal callbacks may sleep; BH callbacks may not.
Early boot without initialized providers, NMI and CPU hotplug are unsupported.

## Shutdown And Limitations

Stop producers, drain acquired executions, detach sources and wait for their
receipts before `NormalServices::stop`. It joins managers and workers, drains
manager timers, and supports repeated stop. Do not call it from an owned worker
or manager. Invalid context rejects before stop effects. Dropping the collection
does not stop live tasks: explicit joined stop is required.
BH owners must likewise outlive all producers and active passes.

Source operations are O(1); source inspection is round-robin by FIFO turn.
Worker scans are bounded by the configured slot count H; cold idle maintenance
can be O(H squared). Fixed slots avoid producer-side growth, at the cost of a
bounded scan. No hard callback latency, forced callback termination, panic
recovery or reclaim-rescuer guarantee is provided.

Local tests cover state conservation, coalescing, reference retirement, reentrant
destruction, capacity, real worker growth/parking/retirement, startup rollback,
and BH preparation. Concrete workqueue semantics and system migration are
separate integration responsibilities, not established by these service tests.
