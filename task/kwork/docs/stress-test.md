# kwork stress test subsystem

This document fixes the design for an optional runtime stress-test subsystem
used to exercise `kwork` with real tasks, timers, softirq drain contexts, and
SMP scheduling.

## Build gate

The stress subsystem is compiled only behind the explicit development switch
`KFEAT_KWORK_STRESS`, which maps to the Cargo features `kfeat/kwork_stress`,
`procfs/kwork_stress`, and `kwork/stress_test`. It must be disabled in normal
production builds and in ordinary unit-test images unless explicitly
requested.

Stress cases run only through `/proc/kwork_stress`, including the short
`smoke` command. Boot does not run stress automatically.

## Boundary

The stress subsystem is a `kwork` runtime user. It must use public product
APIs such as `ScheduledWork`, `DelayedScheduledWork`, `WorkQueueHandle`,
`system_wq`, `system_bh_wq`, `flush`, `cancel`, and `cancel_sync`.

It must not reach into `kworkqueue` binding internals or `kworkerpool` worker
state to force progress. Those crates are validated separately by unit tests.

## User trigger

The trigger is `/proc/kwork_stress`, compiled with the feature above. The file
accepts a small command payload:

```text
queue-flush [rounds] [works]
static-queue [rounds] [works]
balanced-fanout [rounds] [works]
percpu-fanout [rounds] [works]
yield-cpu [rounds]
bh-drain [rounds]
bh-highpri [rounds]
budgeted-poller [rounds] [works]
cancel-race [rounds] [works]
disable-race [rounds] [works]
destroy-race [rounds] [works]
delayed-cancel [rounds] [works]
cancel-nonblocking [rounds] [works]
wait-deadlock [rounds]
sleep-block [rounds]
all [rounds] [works]
smoke
soak [seconds] [works]
bench [seconds] [works]
```

The read side returns supported commands. The write side logs one summary per
case:

```text
case=<name> rounds=<n> queued=<n> completed=<n> [cancel=<n>] [cancel_sync=<n>] [would_deadlock=<n>] [disabled=<n>] [failures=<n>] active_cpus=<n>
```

The first implementation runs synchronously in the writing task. A later async
runner can add a persistent last-result file without changing the case
semantics.

`bench` reports one line per benchmarked product path:

```text
bench=<name> seconds=<n> batches=<n> ops=<n> elapsed_ns=<n> ns_per_op=<n> ops_per_sec=<n> active_cpus=<n>
```

The benchmark command is a comparison tool, not a correctness proof. It uses
the same public APIs as stress cases and is intended to preserve a performance
baseline before scheduler, pool, or queue policy changes.

## First cases

- `queue-flush`: enqueue many works into a dynamic queue with `max_active=1`,
  repeatedly forcing inactive promotion and queue flush completion. The case
  rotates explicit targets across ready pools when more than one pool is ready.
- `static-queue`: enqueue many works into a static `WorkQueue::new` queue and
  flush it while rotating explicit CPU targets.
- `balanced-fanout`: enqueue a batch through default `system_wq` APIs without
  an explicit CPU target, validating the product-layer default binding
  selection used by interrupt-like producers.
- `percpu-fanout`: enqueue from producers pinned to different CPUs through
  `system_percpu_wq`, validating current-CPU binding selection.
- `yield-cpu`: one work item yields in a loop while other work waits
  behind it, validating CPU-intensive worker accounting and dynamic worker
  creation. The case rotates explicit targets across ready normal pools.
- `bh-drain`: producers enqueue bottom-half work while the BH softirq drain
  budget forces repeated restart. BH work remains CPU-local because the current
  softirq raise path is local to the caller CPU.
- `bh-highpri`: enqueue work through `system_bh_highpri_wq`.
- `budgeted-poller`: start a `BudgetedPoller`, repeatedly notify it, mix in
  foreground assist, and destroy it after all published units complete.
- `cancel-race`: ktask producers and cancellers pinned to different CPUs race
  system work enqueue with `cancel_sync`, covering pending/running cancellation
  and cross-CPU flush wait.
- `disable-race`: one task toggles per-work disable depth while another CPU
  repeatedly tries to queue and flush the same work, covering disabled enqueue
  rejection and enable recovery.
- `destroy-race`: a dynamic queue is destroyed while a producer is still
  enqueueing to it, covering disabled queue publication, flush-on-destroy, and
  producer-side disabled returns.
- `delayed-cancel`: delayed work is scheduled, modified to immediate, canceled
  before expiry, and canceled after timer expiry windows to exercise stale
  timer generation handling.
- `cancel-nonblocking`: pending normal work and delayed work are canceled with
  the non-waiting public `cancel` API while a running blocker keeps the queue
  serial. The case also verifies that canceling a running work returns the
  running result without blocking.
- `wait-deadlock`: worker callbacks call queue-wide waiting APIs that target
  their own bounded pool. The case verifies that these public APIs reject the
  wait with `WouldDeadlock` and do not poison later queue use.
- `sleep-block`: one normal worker blocks in a sleepable wait while another
  work item is queued to the same CPU pool. The case verifies that the blocked
  worker releases bounded-pool concurrency and a replacement worker can run the
  queued work before the blocker is released.
- `smoke`: short fixed suite that runs every case with small parameters.
- `soak`: time-driven mixed suite. It repeatedly runs the concurrency-heavy
  cases with small fixed round batches and the requested work count until the
  requested duration expires. Dynamic queue cases are included periodically to
  keep the mix focused on long-running state-machine races instead of queue
  allocation throughput.
- `bench`: time-driven product-path benchmark. It measures default system
  enqueue/flush, CPU-local system enqueue/flush, static queues, dynamic
  serial queues, bottom-half dispatch, and `BudgetedPoller` notification
  throughput.

Future cases:

- long-duration mixed producer tasks that keep running until an explicit stop
  command records a persistent result snapshot.

## Failure rules

Every case must use bounded timeouts. A timeout is a failure and must report the
last observed counters.

Stress waits may yield or sleep to avoid the test harness itself starving
worker, manager, or watchdog tasks. Such waits are test infrastructure only:
product worker progress must come from queue state transitions, worker-pool
actions, and registered wake events, not from periodic rescue timers.

Stress failures must not panic by default after the user-visible result has
been stored; panic mode may be added later as a separate option for CI
amplification.
