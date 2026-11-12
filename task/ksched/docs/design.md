# ksched — Design

## Role

`ksched`'s production scheduler is a class arbiter. `ktask` calls
`Scheduler` while holding the run-queue lock.
The arbiter walks Stop, Deadline, RT, Fair, and Idle in Linux rank order. This
layer implements Stop (at most one waiter per RQ), Deadline (CBS + EDF), RT
(FIFO/RR), Fair (EEVDF), and Idle. New tasks are still created Fair by
`SchedTask::new`.
`sched_setscheduler` / `sched_setattr` call `Scheduler::set_attr` through
`ktask::sched_update_attr`, moving FIFO/RR onto RT and `SCHED_DEADLINE` onto
Deadline. `SCHED_RESET_ON_FORK` is stored on `SchedAttr::reset_on_fork`. Clone
builds the child attribute with `SchedAttr::for_fork`. When the flag is set, an
RT or Deadline child returns to `SCHED_NORMAL` at nice 0, and a Fair child
zeros only a negative nice. A Deadline parent without the flag is refused
(`DeadlineFork`). When the task is already RT, `ktask` picks a CPU with
`find_lowest_rq` and keeps the throttle timer armed while idle.

## Background

### Reference ownership and destruction

`Scheduler<T, Owner = Arc<SchedTask<T>>>` keeps the original standard `Arc` by
default. A task migration may explicitly choose `DeferredArc<SchedTask<T>>`.
Production `ktask` has not switched yet. `SharedOwner` is the unsafe contract
for a stable address, `Clone`, and raw ownership transfer. It does not cover
construction, work queues, or scheduling policy. `owner.rs` implements the
contract for `Arc` and `DeferredArc`. `ksched` depends on `kfinalize` ownership
types, not on `kworkng` or `ktask`.

The owning Fair ready tree and its cache, both Deadline trees, the RT list,
Idle, and `current` all store the same owner type. There is no extra `Arc`
wrapper mixed in, and no runtime enum or virtual dispatch. `OwnerPointerOps`
delegates the library's raw transfer to `SharedOwner`. Entity field projection
still uses the existing `offset_of` and must not build an owning pointer from
`&SchedTask`. The first-deadline cache is refreshed through the owning tree's
`clone_first`. Fair wraps one intrusive-collections RBTree with one owner
per linked task; both `Arc` and `DeferredArc` use the existing
Adapter/PointerOps round trip. No auxiliary borrowed index remains.

Each scheduler uses one owner type. The type checker blocks a direct enqueue
across different owners. The outer caller decides when to construct a
`DeferredArc` and whether the release context is valid. Construction, provider
installation, and worker execution do not happen inside the scheduling
algorithm.

`dequeue` takes `&Owner`, not only `&SchedTask<T>`. RT's O(1) removal gets the
full allocation provenance from `Owner::as_ptr`. A narrow pointer derived from
`&SchedTask` cannot recover a `DeferredArc` header. A real RT queue reproduced
that under Miri. The fix keeps O(1) removal and adds neither a pointer field
nor an extra reference. Fair and Deadline find their owning node by key. Fair recovers only the count originally transferred through its adapter,
never an owner reconstructed from a borrowed task reference.

Fair now embeds one `FairLink` in place of the two original RB links.
It stores parent/children, subtree minimum vruntime, dirty-list metadata and
an atomic membership/color byte; it does not duplicate task keys or vruntime. The first-deadline
cache and buddy may retain extra owners, but there is no secondary tree or
`first_vruntime` owner cache. Layout is target-specific and not an ABI promise.
One private Arc context is allocated at queue construction and shared only
by the wrapper and its bound library adapter. Queue edits allocate nothing.
The library-backed revision has not been executed or performance-measured;
results for the earlier AVL or hand-balanced versions do not validate it.

A running task leaves its current RQ for more reasons than yield: preempt,
block, migrate, and exit. If each path cleaned scheduler state on its own, the
fair scheduler's lag and `curr` accounting would drift, and a running task's
strong reference could be trapped inside the algorithm. This crate closes that
contract with one state transition.

## Scope

```
task/ksched/
├── src/
│   ├── lib.rs          # Scheduler / SchedTask
│   ├── config.rs       # Fair request, RR slice, RT bandwidth, DL budget and admission margin
│   ├── policy.rs       # SchedClass rank, user policy, RtPrio, DlParams
│   ├── entity.rs       # SchedTask, FairEntity, RtEntity, DeadlineEntity, RQ token
│   ├── fair_tree.rs    # RBTreeOps augmentation over intrusive-collections
│   ├── owner.rs        # SharedOwner and intrusive PointerOps adapter
│   ├── ready_list.rs   # Intrusive FIFO for RT and O(1) removal
│   ├── class.rs        # ClassOps
│   ├── classes/        # stop.rs, deadline.rs (CBS+EDF), rt.rs, fair.rs (EEVDF), idle.rs
│   ├── scheduler.rs    # Five-class arbitration
│   └── clock.rs
├── ut-tests/           # Stop / Deadline / RT / Fair / Idle arbiter tests
└── docs/
```

## Architecture

```
Ready --pick_next_task--> Running
Ready --steal_ready_task--> Migrating (off source RQ; PLACE_LAG armed)
Running --leave_current(Yield|Preempt)--> Ready
Running --leave_current(Block)--> Blocked (off RQ)
Running --leave_current(Migrate)--> Migrating (off source RQ)
Running --leave_current(Exit)--> Exited (off RQ)
Blocked|Migrating --enqueue_task--> Ready
```

| Component | Responsibility |
|------|------|
| `LeaveReason` | Closed enum of why the current task leaves |
| `Scheduler::leave` / yield / preempt / block / migrate / exit | The only transition that takes a running task off the execution slot |
| `Scheduler::enqueue` | Enqueue a non-current Ready task (wake / migrate-in) |
| `Scheduler::steal` | Remove one ready waiter matching a predicate (idle-pull); does not `pick_next` or install `curr` |
| `Scheduler::curr` | The running task's `Arc`, returned on `leave` |

## Class arbiter

`ktask` calls `Scheduler` while holding the run-queue lock.

```
pick_next: Stop -> Deadline -> RT -> Fair -> Idle
           one slot  CBS+EDF  FIFO/RR  EEVDF  always returns the pinned idle
```

| Component | Responsibility |
|------|------|
| `SchedTask` | Caller payload, class, seat, user attr, `pi_prio`, and one `FairEntity`, `RtEntity`, and `DeadlineEntity` per task |
| `StopClass` | At most one waiter per RQ; no time slice; not stealable |
| `DeadlineClass` | Two intrusive RB trees (ready / throttled); CBS budget; EDF selection; per-RQ bandwidth admission |
| `RtClass` | Intrusive FIFO at priorities `1..=99`; RR slice; RT bandwidth throttle |
| `FairClass` | One owning augmented RB tree, lag, NEXT_BUDDY, request |
| `IdleClass` | One pinned idle task per RQ; not queued |
| `Scheduler::curr` | The running task's `Arc`, returned on `leave` |

Public methods are safe. The caller must already hold that RQ lock. This crate
does not check the lock, sample the clock, take a lock, or switch stacks.
`now_ns` is passed in by the caller.

Fair placement matches the plugin EEVDF: `vruntime = V - lag`. An off-tree
runner counts toward `min_vruntime` and the average **V**. Block and Migrate
record lag. Exit does not. Yield and preempt enqueue again. `migrate_current`
is the migrate-out entry. The destination RQ enqueues with
`EnqueueKind::MigrateIn`. Idle's `has_runnable` is always true, so
`pick_next` falls through to idle when Fair is empty and never returns empty.

When a queued Fair task changes nice, `Scheduler::set_attr` has already written
the new weight. `attr_changed` must subtract the enqueued weight of the old
nice from `total_weight` / `sum_wv`, then re-enqueue at the new weight. The
whole update stays under one RQ lock. A running task is off the tree and does
not take this charge path.

`SchedAttr::validate` checks that the per-class fields agree with `policy`.
Deadline must carry a valid `DlParams`. FIFO/RR require a priority in
`1..=99`. A Fair policy must carry `RtPrio::NONE`. Any non-Deadline policy
must not carry `dl_params`. Stop and Idle are kernel classes. `set_attr`
returns `KernelClassImmutable` for them. The user policy word has no Stop.

`SchedAttr` stays the user policy. `pi_prio` is the inheritance boost, and
`RtPrio::NONE` means there is none. Dispatch uses both. A Fair policy with a
boost runs as RT at `pi_prio` and is not an RR task, so it has no RR slice.
An RT policy runs at the greater of its own priority and `pi_prio`. Deadline
keeps its class; a stored boost is applied only after `set_attr` leaves
Deadline. `publish_dispatch` stores `rt_priority_snapshot` and then
`class`, both with Release, so a remote reader that observes the new class
also observes the new priority. `pi_donor_priority_snapshot` publishes one
complete donation byte for PI readers: Deadline 99, effective RT priority, or
zero for other classes, without taking an RQ lock.
`Scheduler::attr` and `SchedAttr::for_fork` read the user attribute, so
`sched_getattr` and fork do not observe the boost. `init_attr_unpublished`
clears it.

`set_attr` validates, rejects Stop and Idle, charges the runner, and runs
`can_admit` on the user class. Failure returns before either stored value
changes. It then keeps the current boost and calls `reseat`. `set_pi_boost`
writes only `pi_prio` and calls `reseat`; it does not admit bandwidth. Stop
and Idle ignore it. A same-class update calls `attr_changed` with the
previous effective RT priority as the queue key and the user policy for the
RR slice. A class change dequeues, switches, and enqueues.

### Stop class

Each RQ has at most one Stop task. `ktask` creates and wakes one
`migration/%u` per run queue and runs the stop-machine on it. It may be a
waiter or the runner in `Scheduler::curr`, but those cannot be two
different tasks. This layer only implements the queue. Creation and wake
already satisfy the one-Stop-task rule.

- The caller guarantees one Stop task per RQ. `enqueue` (a waiter is already
  present, or the current runner is already Stop) and `install_current` (via
  `set_curr`, while a waiter is present) `assert` on a violation. That is a
  programming error, not a `SchedError`. `ClassOps::enqueue` has no error
  channel, and a second Stop task is not a user-recoverable failure. The
  runner is recorded in `Scheduler::curr` and does not occupy the `waiter`
  slot.
- `pick_next` looks at this class first. Yield and Preempt put the runner
  back in the only slot, so a wake from a lower class reports `resched` false
  and the tick does not preempt it.
- Block, Migrate, and Exit clear the slot and do not enqueue. `steal` is
  always `None`: Stop stays on the RQ it entered.
- There is no time slice. `curr_timer_ns` and `class_timer_ns` are `None`.

### Deadline class (CBS + EDF)

Each task reserves bandwidth with
`DlParams { runtime_ns, deadline_ns, period_ns }`. The ready state is ordered
by **absolute deadline** (`deadline_ns`), so the earliest deadline runs first.
`account` subtracts from `budget_left_ns`. At zero the task becomes throttled
and moves to the throttled tree by `period_start_ns`. The next period's
`advance` refills the budget and returns it to the ready tree.

- Both trees key on `(instant_ns, tree_id)`. The class assigns `tree_id` on
  each enqueue. It exists only to break ties at the same instant, so FIFO
  order does not need an id lookup table.
- `enqueue` depends on `EnqueueKind`. `New` opens a new request.
  `ClassChange` is handled by `switched_to` and does not refill the budget
  again. `MigrateIn` and `Wake` stay on the throttled tree when
  `budget_left_ns == 0`; otherwise they continue the current request. A
  same-class `attr_changed` clears `is_throttled` / `is_active` without
  refilling, so migrate-in does not consult those flags.
- **Admission is per RQ.** `can_admit` sums `runtime/period` over the ready
  tree, the throttled tree, and the runner. `pick_next` removes the runner
  from both trees, so the class remembers it in the private `running` field
  (`tree_id` and reserved ppm). It is set in `pick_next`, `set_curr`, and
  `switched_to(is_curr)`, cleared in `put_prev` and `switched_from(is_curr)`,
  and refreshed by `attr_changed(is_curr)` when parameters change inside the
  class. The new request replaces that task's old reservation in the sum.
  The result must not exceed `1 - SCHED_DL_BW_MARGIN_PCT`. A task that is
  neither on a tree nor the runner after Block or Migrate is not counted.
  Over the cap returns `SchedError::AdmissionFull`, mapped to `EBUSY` above.
  An illegal parameter still uses `InvalidPolicy` (`EINVAL`).
- `set_attr` already calls `validate()`, so validation happens before any
  state change. Admission runs in `set_attr`'s `can_admit` phase. Failure
  does not change the task attribute.
- Deadline does not participate in steal. Bandwidth is charged to the home
  RQ. Moving the task would leave the reservation that admitted it.
- `curr_timer_ns` reports the earlier of "budget exhausted" and "request
  deadline", and the period start while throttled. `class_timer_ns` is the
  earliest key of the two trees. Events are reported as relative nanoseconds
  and handed to `ktask` through `Decision::next_event_ns` to program the
  existing scheduling slot. **No dedicated timer is added.** The floor shares
  `MIN_SCHED_TIMER_NS` with Fair and is below any legal `runtime_ns`.

Known tradeoff: `can_admit` walks both resident trees, and `nr_queued` walks
the ready tree (O(n)). This step prefers correctness. If the deadline
population grows, an incremental sum or a cached earliest entity can replace
the walk.

When RT and Fair switch at the same rank, a queued task is dequeued from the
old class before entering the new one. A running task only changes class and
is not enqueued. A higher-priority RT waiter preempts Fair through
`higher_class_runnable`. Inside one RT priority, a higher-priority waiter
uses `wakeup_preempts`. A voluntary `yield` (POSIX `sched_yield`, Linux
`yield_task_rt`) puts both FIFO and RR at the tail of that priority. A
preempted task is inserted at the head. Only when a `SCHED_RR` slice is
exhausted (`rr_rotate`) does the preempted task go to the tail instead.
Once `used_ns` reaches `SCHED_RT_RUNTIME_US` inside an RT period, the whole
class throttles. `pick_next` skips RT and lets Fair run until the period
boundary `advance` clears the counter.

`rt_priority_snapshot` is the lock-free priority published with
Release/Acquire, for `ktask` CPU selection. `Scheduler::rt_preempt_rank`
reads this CPU under the RQ lock. It is 0 when the runner is Fair or Idle and
the RT class can still be picked, the runner's effective RT priority
(the user priority raised by `pi_prio` when that is greater) when RT is
current,
and 255 for Stop or Deadline. Bandwidth throttle also records 255
(`is_throttled_at`) while this RQ's clock has not passed
`period_start_ns + rt_period_ns`, because `pick_next` will not select RT
during that interval. CPU selection skips this CPU and falls back to the hint
only when no other preemptible CPU exists. After the queue is emptied the
period-boundary timer stays armed. When it fires it clears the flag and
rewrites the snapshot. This byte describes only the runner. A higher-priority
waiter already queued on RT but not yet on CPU does not raise it, so a
medium-priority RT task may be placed on this CPU behind that waiter. This
step does not store a separate "highest queued RT priority". `ktask` writes
the byte to a per-CPU atomic before dropping the RQ lock on operations that
change it (`charge`, replacing `curr`, `set_attr`). CPU selection does not
take the remote lock. A read-only `next_event_ns` does not republish the
snapshot.

## Calling constraints / execution context

- Every `Scheduler` method is called by `ktask` while holding the
  **target** run queue's scheduler lock, with IRQ and preemption disabled as
  that path requires. Idle-pull's `steal_ready_task` holds the **source** RQ
  lock and must not also hold the destination scheduler lock.
- `leave_current` must finish before `pick_next_task`. EEVDF asserts if
  `curr` is still present.
- The plugin EEVDF must not own a task's lifetime. `EevdfScheduler::curr`
  caches only scheduling numbers. `Scheduler::curr` holds the running `Arc`
  and releases it on leave. See "Class arbiter".
- After changing a running entity's `vruntime`, `deadline`, or `weight`, the
  `curr` snapshot must be refreshed. The production path does this in
  `update_current` and `set_priority`.
- A running task that reached the CPU without `pick_next_task` calls
  `sync_running_curr` on entry to `switch_to_local`. Otherwise an empty
  `curr` treats any non-empty ready queue as preemptible. Idle never syncs.
  An empty `curr` is the correct meaning for it.

## Algorithm

1. `add_task`: a new task enters the ready queue at the algorithm's initial
   placement.
2. `pick_next_task`: select the next running task. EEVDF also writes the
   `curr` snapshot.
3. `steal_ready_task`: remove one ready waiter matching a predicate, using
   the same lag snapshot as `remove_task`, without installing `curr`.
   FIFO/RR stop at the first match and leave the rest of the intrusive list
   in place (O(match index)). CFS/EEVDF find and then `remove_task`. `ktask`
   idle-pull uses this to reject a task that is still `on_cpu`.
4. `update_current(elapsed_ns)`: advance the running task's accounting. EEVDF
   refreshes the snapshot.
5. `next_preemption_ns`: relative nanoseconds until preemption must be
   reevaluated. `None` for a lone task.
6. Preemption is probed in two steps, matching Linux `check_preempt_tick`
   and `__schedule`. Only the schedule tick (`account_sched_tick`) calls
   `check_preempt_tick` without consuming the mark. The IRQ tail
   `peer_preempts_curr` consumes WF_SYNC and decides whether to
   `leave_current(Preempt)` and pick. `update_current` requests resched only
   when this request is exhausted. It does not mark because a peer has an
   earlier deadline (otherwise a WF_SYNC switch onto a later-deadline wakee
   would be preempted back immediately). An off-tree helper calls
   `sync_running_curr` on entry to `switch_to_local`. Idle keeps `curr`
   empty.
7. `leave_current`:
   - Yield: reset request/slice and enqueue again;
   - Preempt: keep the remaining request and enqueue again;
   - Block/Migrate: the fair scheduler saves lag and marks PLACE_LAG, and
     does not enqueue;
   - Exit: clear current accounting and do not set PLACE_LAG.
8. `enqueue_task`: consume PLACE_LAG if present, then enqueue. EEVDF
   nominates NEXT_BUDDY (Linux `set_next_buddy`). Nominate even when `curr`
   is empty: a remote WF_SYNC often lands in the leave-to-pick window,
   otherwise the following pick would leave a runner with an earlier
   deadline.
9. EEVDF `min_vruntime` follows Linux `update_min_vruntime`. A `curr` that is
   off the tree but still runnable participates in the watermark (the min of
   that value and the ready minimum vruntime, then a monotonic raise).
   `leave_current` must update **before** clearing `curr` (Linux `put_prev`
   / `update_curr`). After `curr` is cleared, do not update the watermark
   from the ready-only tree on the following enqueue. `place_entity` is
   `vruntime = V - lag`. It does not clamp the wakee further to V or
   `min_vruntime`.
10. `update_current` follows Linux `update_deadline`. After a request
    completes, assign a new `vd = ve + r/w`, and request resched only when
    the ready queue is non-empty.

## Design decisions

- The production path uses `Scheduler`. The Fair default request comes from
  Kconfig `SCHED_FAIR_REQUEST_US`. The RR slice is `SCHED_RR_SLICE_US`
  (default 100 ms). RT bandwidth is `SCHED_RT_RUNTIME_US` /
  `SCHED_RT_PERIOD_US` (default 0.95 s / 1 s). Deadline's minimum budget and
  admission margin are `SCHED_DL_MIN_RUNTIME_US` (default 100 µs) and
  `SCHED_DL_BW_MARGIN_PCT` (default 5). Stop is one slot per RQ and has no
  slice.
- **Deadline uses two independent trees, not one tree or one link shared by
  ready and throttled.** A single tree with one link (saving one
  `RBTreeAtomicLink`) was considered and rejected for two reasons:
  1. **It would pollute `nr_queued`.** `ktask`'s `load_count` computes
     `nr_running` as `Scheduler::nr_queued() + non-idle runner`. CPU
     selection (`find_idlest_cpu` / SIS / idle-pull) reads that directly. If
     both states shared one tree, a throttled task would still be in the
     tree, `nr_queued` would count a task that cannot run this period as
     runnable, and placement would put a new task on a CPU that has no
     runnable work. Split, `nr_queued == ready.len()`, matching Fair.
  2. **The two states sort on different keys.** Ready state sorts by the
     request's absolute deadline (`deadline_ns`). Throttled state sorts by
     the next period's start (`period_start_ns`), which is the wake instant.
     One tree would mix both keys and make `pick_next` skip throttled nodes
     one by one. Selection would fall from "take the minimum key" to O(number
     of throttled nodes), and the earliest event of `class_timer_ns` would no
     longer be the tree's `front()`.
  The cost is one extra link per entity (`ready_link` / `throttled_link`),
  initialized statically at construction, with no runtime allocation. Dequeue
  does not pick a tree from `is_throttled`. A same-class `attr_changed` only
  clears the flag and does not unlink, so the flag and the link can diverge.
  While the seat is `QueueSeat::Deadline`, each tree is searched once by its
  own key.
- `SchedAttr` stays a flat `Copy` struct. Per-class fields coexist, and
  `policy` decides which ones mean something. `validate()` is the only gate.
  Adding a class means adding a field and a branch, without changing the
  existing field layout. Parameters that do not belong to the class use a
  sentinel (`RtPrio::NONE` / `Option::None`).
- `Scheduler::curr` holds the running task's `Arc`. That is not the same
  ownership as the plugin EEVDF numeric snapshot. `leave` releases the
  arbiter's reference. The Block path still requires the caller to hold
  another one.
- `LeaveReason` replaces a boolean `preempt` plus a scattered
  `account_sleep`, so callers do not have to remember a hidden invariant.
- The unwired metadata-split scheduler is gone. Production and unit tests
  both use the entity embedded in the task.
- Fair wraps intrusive-collections 0.10.2's owning `RBTree`. `BoundAdapter`
  delegates projection, key access and PointerOps to the original Fair adapter;
  its custom `RBTreeOps` supplies typed links and records structural changes.
  The library performs all balancing, rotations, recoloring and raw-owner
  recovery. There is no local copy of those algorithms or dependency fork.
- Child setters append a node once to an embedded dirty list in O(1). They do
  not follow parent paths, since rotations/transplants temporarily expose
  incomplete topology. After one complete insert/remove, repair each dirty
  node's final ancestor path. In the pinned library, insertion/deletion use a
  constant number of rotations and child-pointer edits, so these repairs cost
  O(log N). Ancestor updates do not stop early: a different pending child may
  still have a stale summary. Changes to library mutation behavior require
  re-auditing this complexity argument.
- The private context mirrors the root through `set_parent(node, None)` and
  root release. It is a borrowed pointer into the same owning RBTree, not a
  second index. The root summary supplies minimum ready vruntime in O(1).
  The existing earliest-deadline cache preserves the O(1) eligible-head path;
  fallback prunes by subtree minima along one O(log N) root-to-leaf path.
- Keys and vruntime remain immutable while linked. Nice/placement changes
  remove and reinsert the node. No full-tree summary rebuild, temporary vector
  or per-edit heap allocation is used. Dirty-list storage is embedded in each
  link; the one per-queue context is allocated before scheduling operations.
- Deadline/enqueue-id order, lag, weight sums, current-task accounting,
  buddy/WF_SYNC and request semantics remain. Steal walks the same tree in
  deadline order and transfers the selected owner's library-held count;
  rejecting a candidate does not clone an owner.
- EEVDF virtual time (`vruntime` / `deadline` / `vlag`) uses a fixed-width
  `i64`. Wall time (`slice_ns` / `request_ns` / `elapsed_ns`) uses `u64`.
  The two sides convert only through `vruntime_delta` /
  `vruntime_to_wall_ns`. `Σ w·v` multiplies and divides in `i128`. `isize`
  is not used (vruntime would overflow on 32-bit). Nice still goes through
  `SchedAttr::nice` (`Nice`, applied by `Scheduler::set_attr`).

## Drop / resource release

The library RBTree owns one PointerOps count per Fair node. Insert, cursor
removal, minimum-cache cloning and ordered iteration use the library APIs;
`fair_tree.rs` implements no raw-owner recovery or reference-count cloning.
The bound adapter delegates the original full-provenance field projection,
so ownership does not pass through a narrowed `&SchedTask` pointer.

Normal library unlink marks the node `RETIRED`, which still reports linked.
The wrapper holds the returned owner while draining dirty references; retired
nodes are skipped during ancestor repair. Only after repair finishes does
membership become UNLINKED with Release, allowing another tree's Acquire to
reuse storage. This closes the unlink-to-repair early-reuse window even when
another holder observes the atomic membership flag. The removed owner's
backing allocation remains live throughout.

Nonempty wrapper Drop switches link operations to clearing mode before
calling library `clear()`. Clear saves children, releases each link directly
and drops its count; it never queries task keys/vruntime. The wrapper discards
pending-list pointers without following them after clear, then the empty
library tree and private context drop normally. No retired/dirty node pointer
may be followed after its backing owner is released.

## Augmentation validation

The [native/Miri fixture](../tests/fair-tree/README.md) compiles the wrapper
against the pinned library with an Arc adapter. Prepared checks cover library
red-black invariants, parent/root consistency, dirty-list draining, summaries,
successor transplants, reuse, owner release and a linear selection oracle.
Guest regressions cover actual Scheduler picks, Arc/DeferredArc, nice,
preemption, wake and migration.

Only type and formatting checks have run for this library-backed revision.
The prepared tests, guest execution and Miri have not run. Earlier AVL or
hand-balanced-tree results are not validation of this implementation, and no
RQ hold-time, throughput or wake-latency improvement has been measured.

## RT queues

`RtClass` places one `ready_list::ReadyList` at each priority `1..=99`.
`occupied` is the bitmap of non-empty levels. The highest level is
`leading_zeros`. Enqueue order within one priority is FIFO. A voluntary yield
puts the current task at the tail of that priority, for both FIFO and RR. A
preempted task is inserted at the head. Preemption after a `SCHED_RR` slice
is exhausted inserts at the tail.

The owner word is set to this `ReadyList`'s address, and the task is linked,
only when the task is not already on another list. A task that is already
linked does not change owner and is not enqueued. The word is cleared before
removal. Another CPU or another priority has a different list address. A
mismatch returns `None` and does not unlink by pointer. `is_linked` alone
does not mean the task belongs to this queue. Queue objects do not move after
the run queue is published, or the address would no longer match. `Drop` pops
front one by one, clears ownership, and releases the `Arc` held by the list.

RT steal's filter only borrows the task inside the cursor. It does not create
a temporary `Arc` or clone a reference. A hit transfers the existing owner
through the list `remove`. A miss leaves the reference count unchanged.

Throttle is per RQ. `account` adds the runner's wall time. After runtime is
reached, `has_runnable` is false and `pick_next` skips this queue's RT. The
flag is cleared only in `advance`. `class_timer_ns` points at the end of this
period while the flag is still set, including after block, migrate, exit, or
steal has emptied the queue. An idle CPU has no tick. Dropping this event
would leave the throttle flag and the published 255 past the period boundary.
When the timer fires, `advance` clears the flag and republishes the rank, and
does not arm again. The 255 that CPU selection reads is decided by
`is_throttled_at(now)`. Once `now` has reached
`period_start_ns + rt_period_ns`, it is no longer treated as throttled.
`steal` ignores throttle: idle-pull may move these waiters onto another CPU
that still has runtime, and the usage is charged to the destination RQ.
Period advance uses the absolute `RqClock` inside `advance` and does not
sample the clock again.

Actual execution time is stored in `SchedTask::sum_exec_runtime_ns`. The
shared `Scheduler::charge` consumes the delta from `RqClock::advance`. A
non-Idle task is accumulated once before the class algorithm runs, independent
of nice weight. Gaps spent blocked or migrating are not accumulated. A read
returns only the committed value.
