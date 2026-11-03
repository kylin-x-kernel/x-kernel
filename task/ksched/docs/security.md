# ksched — Security and reliability

## Trust model

```
ktask RunQueue (production caller)
   │  calls Scheduler while holding the RQ lock
   v
Stop (one slot per RQ) -> Deadline CBS+EDF -> RT FIFO/RR -> Fair EEVDF -> Idle
```

- The caller must serialize access on one RQ. A running task leaves only
  through `yield_current`, `preempt_current`, `block_current`,
  `migrate_current`, or `exit_current`.
- `Scheduler`'s public methods are safe. `RqLocked::assume()` appears in the
  private `token()` and in `init_attr_unpublished`. The latter requires the
  caller to still exclusively own a task that is not yet in the TID table and
  not yet on any run queue. `ktask::init_task_sched_attr` is that path's
  caller, used before clone publishes the task. This crate does not check the
  lock itself.
- The scheduler trusts that the caller has already validated the task state
  it passes in. This crate checks structure only through
  `SchedAttr::validate`: a FIFO/RR priority must be in `1..=99`. Credential
  checks are not done here. Before `set_attr` places a task in RT or
  Deadline, the caller must already have confirmed effective
  `CAP_SYS_NICE`. `sys_sched_setscheduler` / `sys_sched_setattr` submit through
  `ktask::sched_update_attr`. The owner check, and the privilege check for
  FIFO/RR/Deadline, finish before `Scheduler::set_attr` changes the
  attribute. If that check is skipped, FIFO 99 stays ahead of every Fair
  task.

## External boundary / attack surface

`SharedOwner<T>` is an unsafe implementation boundary. An implementation must
keep a stable address for the same value. `Clone` creates an independent
owner. A raw round trip neither releases nor increments the count. Recovering
a valid pointer must not panic. The caller must still make sure owner release
fits the RQ lock and the execution context. Default `Arc` behavior is
unchanged. Hand-off of a `DeferredArc`'s last reference follows the
`kfinalize` contract.

Ready, Fair, and Deadline `OwnerPointerOps` only transfer that responsibility.
They do not recover a control block from a borrowed reference. The intrusive
library may clone a node reference with a temporary `ManuallyDrop` owner. The
temporary view does not consume the count the queue retains. The real owner
covers every use. The adapter must keep the full pointer provenance of
`into_raw`. The inverse of the field projection is what recovers a
`DeferredArc` payload header. Each owner in the cache, the dual index, and
`current` is counted independently. A non-empty queue destructor unlinks one
by one. It does not `fast_clear`, and it does not destroy a Deferred payload
in place. Host, Miri, and guest tests of the three real queue kinds cover
this boundary.

RT removal by task must use `&Owner`'s raw pointer. A pointer derived from
`&SchedTask` must not cross the payload header. Miri reported a borrow-stack
violation on that path. After the fix to `Owner::as_ptr`, the RT and core raw
cases pass strict-provenance Miri. The intrusive-collections 0.10.2 used by
Fair and Deadline encodes parent and color as integer pointers. Those were
verified only in the default provenance mode, and the warning remains. They
are not a strict-provenance pass. Alias and leak checks stay enabled.

`ksched` does not touch user memory, devices, or firmware input directly. The
attack surface is mainly:

- the wrong scheduling API order (a missing `leave_current`, a double
  enqueue);
- parameter ranges such as priority (each implementation checks nice bounds);
- **the Deadline parameter triple.**
  `DlParams { runtime_ns, deadline_ns, period_ns }` ultimately comes from the
  `sched_setattr` user buffer. This crate's entry is `SchedAttr::validate`
  then `DlParams::new`. It checks non-zero, `runtime <= deadline <= period`,
  and `runtime >= SCHED_DL_MIN_RUNTIME_US`. An illegal combination returns
  `InvalidPolicy`;
- **total bandwidth.** The sum of `runtime/period` on one RQ (ready tree +
  throttled tree + runner) must not exceed `1 - SCHED_DL_BW_MARGIN_PCT`.
  This is a fairness constraint, not a safety boundary. Exceeding it returns
  `AdmissionFull` and does not cause an out-of-bounds access or corrupt
  state.

## Unsafe inventory

The Deadline class adds `DlAdapter<T, const READY: bool>`, matching
`FairAdapter`. `get_link` / `get_value` project fields forward and back on
the same `SchedTask<T>` with `core::mem::offset_of!`, onto
`deadline.ready_link` and `deadline.throttled_link`.

Invariants:

1. Each instantiation projects one real `RBTreeAtomicLink` field.
2. `link` must have been produced by the same adapter's `get_link`, and the
   entity must still be alive.
3. The reverse projection only subtracts the field offset. It does not build
   an exclusive reference and does not change ownership.
4. Tree keys are immutable while linked. `deadline_ns`, `period_start_ns`,
   and `tree_id` change only after removal.
5. `RqLocked::assume()` inside `KeyAdapter::get_key` is called by the library
   only from class methods that hold the RQ lock (insert, find, cursor
   walk). Tree destruction does not call `get_key`.

The algorithm implementations mostly use safe collections and `Arc`. FIFO/RR
unsafe code is concentrated in `ready_list`. `ReadyItem` declares the
concrete field offset. `ReadyAdapter` projects the pointer forward and back.
Removal by entity first checks this `ReadyList`'s address, then uses
`cursor_mut_from_ptr`. `is_linked` alone does not mean the task belongs to
this queue. The source queue's exclusive borrow covers the window from the
check to the unlink.

A task that is already linked does not rewrite its owner. An unlinked task
records this list's address before linking, and the word is cleared before
the library releases the link. Deleting from the wrong queue returns `None`.
After the run queue is published, a list object that contains nodes must not
move, or the address would no longer equal the owner. `Drop` clears every
owner word and the owned `Arc`s.
The owner word is not the list's synchronization lock. `Relaxed` is enough
for the identity check. Operations on the same queue already have exclusive
access. Another queue can write only its own address or zero, and cannot
forge this queue's address. Taking a link across queues depends on the
library's atomic link operations. No link or identity field is public. No
hand-written `Send` or `Sync` is added.

`ReadyList::steal` borrows the task through the cursor `get`. The predicate
receives only `&T`. It does not restore a temporary `Arc` and does not
increment or decrement a strong reference. The cursor borrow covers the
predicate. On a hit the owner mark is cleared first, the task borrow ends,
and `remove` then transfers the list's existing owning reference. A rejection
or a predicate panic does not move list ownership.

EEVDF adds two raw pointer projections on the private `EntityAdapter`.
`get_link` adds the `offset_of` of the field this adapter selected to a live
entity pointer. `get_value` inverts a link pointer previously produced by
that adapter. The offset comes from the same concrete `EevdfEntity<T, S>`
type. It does not assume Rust field order or rebuild an address from an
integer. The tree manages ownership with the library's
`DefaultPointerOps<Arc<_>>`. There is no raw borrowed index.
The adapter locates fields. It is not the tree's balancing algorithm. The
macro does not support const generics, so the adapter is implemented
explicitly.
EEVDF steal has a separate raw-ownership audit point. It takes the tree-owned
`Arc` data pointer from the cursor's `get_ptr` (not from `&T`).
`ManuallyDrop` only borrows that strong reference. The predicate receives
only `&Arc`. The tree borrow covers the whole predicate call and blocks
removal and destruction. A panic does not release the borrowed strong
reference. Only a hit clones a truly owned `Arc`. After the temporary view
ends, the path is `remove_task`. The temporary view is not used after
removal, and the borrowed reference is not handed to the caller.

`FairClass` has its own `FairAdapter`. It projects `fair.deadline_link` and
`fair.vruntime_link` inside `SchedTask<T>`, not the plugin `EevdfEntity`.
`get_link` and `get_value` invert the same field `offset_of` and do not
change `Arc` ownership. Each tree holds one `Arc`.
`KeyAdapter::get_key` calls `RqLocked::assume()` from a safe function.
Insert, lookup, and cursors enter only from `FairClass` methods that already
hold the token. `RBTree`'s `Drop` uses `clear()` and does not call
`get_key`, so destroying a non-empty `Scheduler` does not read an `RqCell`
without the lock.

`RqCell`'s `get` and `set` read and write an `UnsafeCell`. A read requires a
valid pointer, an initialized value, and no concurrent write. A write also
requires exclusive access. `RqLocked` plus a complete `Copy` value from
`new` and `set` provide all three. `seat`, `attr`, Fair virtual time, and
`rr_left_ns` use the same token.

`class` and `rt_prio` are `AtomicU8`. `sched_class()` and
`rt_priority_snapshot()` load with Acquire, paired with the Release of
`set_class` and `store_rt_prio`, so CPU selection can read them without the
RQ lock. `seat` stays in an `RqCell` and is read only under the lock.

RT removal by address lives in `ready_list`. `ReadyItem` gives the field
offset of `ReadyLinks`. `ReadyAdapter` projects both ways with that offset.
Removal checks that the owner word equals this list's address, then calls
`cursor_mut_from_ptr`. `is_linked` alone does not mean the task belongs to
this queue. The queue's exclusive borrow covers the window from the check to
the unlink.

A task that is already linked does not rewrite its owner. An unlinked task
records this list's address before linking, and the word is cleared before
the library releases the link. Deleting from the wrong queue returns `None`.
A repeated enqueue is rejected by `AtomicLink` and does not overwrite the
owner. `Drop` unlinks one by one and clears the owner word. The owner word
is not the list lock. `Relaxed` is enough, because operations on the same
list are already exclusive, and another list can write only its own address
or zero.

Steal builds a `ManuallyDrop<Arc<_>>` from the cursor's `get_ptr`. It does
not walk forward from `&T` to recover an `Arc` header. The list still holds
the original strong reference. The predicate receives only `&Arc`. On a hit,
`cursor.remove` transfers that `Arc`, and the temporary view is not used
again.

## Memory-safety invariants

1. Each of an EEVDF Ready task's two trees holds one `Arc`. The two earliest
   caches and `next_buddy` may hold more. After the trees unlink, the
   returned holding covers release of the extra references. The entity must
   not be finally destroyed early.
2. EEVDF `curr` must not hold an `Arc`. It stores only a scheduling snapshot.
3. `curr` must be empty before `pick_next_task` (`leave_current` has run).
4. Exit must not set PLACE_LAG. Block and Migrate must set it for the later
   `enqueue_task`.
5. After a running entity's scheduling fields change, the `curr` snapshot
   must be refreshed.
6. The two EEVDF trees have the same members. Sort fields do not change
   while linked. A priority change removes the task from both trees first.
7. Removal by entity checks the key and the pointer identity first. It does
   not infer membership from `is_linked`. `set_priority` on the wrong queue
   must not modify an entity linked elsewhere. A double enqueue is a caller
   contract error. The link assertion rejects it.
8. An RT task is on at most one priority list at a time. The owner word
   equals that list's identity. A failed removal must not decrement
   `queued`. While throttled, tasks may stay on the list, but
   `has_runnable` is false and `pick_next` must not select them.
9. Each RQ has at most one Stop task. The waiter slot holds at most one
   `Arc`, and only while that task is queued. The runner is in
   `Scheduler::curr`, not in the slot. A second `enqueue`, or
   `install_current` of another Stop task while a waiter is present, breaks
   the contract and must assert. It must not overwrite the previous `Arc`.
   The caller guarantees one Stop task per RQ at create and wake.

## Thread safety

`BaseScheduler` is not a concurrently shareable type. Concurrency safety
comes from `ktask`'s per-CPU RQ lock and the IRQ/preemption constraints.

## Threat analysis

| ID | Threat | Impact | Trigger | Mitigation |
|------|----------|----------|----------|----------|
| T-01 | A missing `leave_current` leaves a stale `curr` or skewed statistics | High | A new leave path is not wired to the shared API | One `leave_current`; EEVDF `pick_next` asserts |
| T-02 | The scheduler holds a running task's `Arc` and pins its lifetime | High | The `curr` strong reference is not released | `curr` is a non-owning snapshot |
| T-03 | Exit sets PLACE_LAG by mistake | Medium | Exit and Block share sleep accounting | A separate `LeaveReason::Exit` branch |
| T-04 | The source RQ still counts a task's weight after migration | Medium | Migrate does not clear current accounting | Migrate goes through `leave_current` and clears the snapshot |
| T-05 | A stale `curr` snapshot causes the wrong preemption or placement | Medium | A running entity's fields change without a refresh | `update_current` / `set_priority` refresh; test helpers sync explicitly |
| T-06 | Idle-pull steals an entity that is still `on_cpu` | High | A remote `steal_ready_task` after yield/preempt enqueued the task | The scheduler removes only a ready waiter; the `ktask` predicate rejects `on_cpu` and does not `pick_next` |
| T-07 | A leftover link in either tree or the cache selects or holds the wrong task | High | Removal, a key change, or `Drop` misses one link | Both trees unlink under the same lock; the cursor successor updates the cache; normal tree `Drop`; membership, reference, and re-entry tests |
| T-08 | `Scheduler` writes an `RqCell` without the RQ lock | High | The caller breaks the lock contract | Documented; the token is built only at a `Scheduler` entry and at unpublished init |
| T-09 | The arbiter `curr` `Arc` is the only reference after Block | High | The caller does not hold the task separately | `leave` releases the arbiter reference; the caller must hold the task |
| T-10 | A second Stop task appears on the same RQ | High | A repeated activate, a mistaken enqueue, or `install_current` while a waiter exists | `enqueue` and `set_curr` assert when the slot is already occupied |

## Failure modes and effects (FMEA)

| ID | Failure mode | Cause | Local effect | System effect | Severity | Mitigation |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | `curr` is non-empty at pick | Call order is wrong | Assertion failure | Scheduling stops (exposes the bug) | 2 | The leave contract is mandatory |
| F-02 | A wake has no PLACE_LAG | Block/Migrate did not record lag | Placement is off | Latency and fairness degrade | 3 | Dispositions differ, covered by tests |
| F-03 | No extra strong reference after Block | The caller did not pin the task | `switch_to` assertion failure | Scheduling stops | 2 | `blocked_resched` requires `strong_count > 1` |
| F-04 | Idle-pull migrate-in has no PLACE_LAG | Removal does not go through `remove_task` | Destination RQ placement is off | The migrated task's fairness drifts | 3 | `steal_ready_task` uses `remove_task`; dest `enqueue_task` applies PLACE_LAG |
| F-05 | A second Stop task | Another enqueue on the same RQ, or a Stop runner is installed while a waiter exists | Assertion failure | Scheduling stops (exposes the bug) | 2 | `enqueue` and `set_curr` assert; `steal` does not move Stop |

## Failure handling

- A broken contract prefers `assert!`. It is a programming error and must not
  be recovered silently.
- An illegal user policy makes `SchedAttr::validate` return `SchedError`,
  which `ktask` passes to the caller.

## Privacy

No user data. The crate operates only on kernel task scheduling metadata.

## Known limitations

- The Stop queue is in the arbiter, but `ktask` does not create Stop tasks
  yet and has no stop-machine. `SCHED_DEADLINE` must carry a valid
  `DlParams`, or `SchedAttr::validate` rejects it with `InvalidPolicy`. A
  per-RQ admission failure is `AdmissionFull`, not a permission check.
- `sched_setscheduler` / `sched_setattr` move FIFO/RR into `RtClass` and
  `SCHED_DEADLINE` into Deadline. Realtime and Deadline policies require
  effective `CAP_SYS_NICE`. `RLIMIT_RTPRIO` is still not consulted.
- `SCHED_RESET_ON_FORK` is stored on `SchedAttr`. On clone, an RT or Deadline
  child returns to `SCHED_NORMAL` at nice 0. A Fair child zeros only a
  negative nice. A Deadline parent without the flag cannot fork (`EAGAIN`).
- RT bandwidth is computed per RQ. There is no cross-CPU root-domain
  bandwidth pool. After this RQ throttles, `pick_next` no longer selects
  those tasks, but idle-pull can still move them to another CPU and charge
  that CPU's runtime. CPU selection reads `rt_preempt_rank`. An RQ that is
  still throttled in this period is recorded as 255 and, like Stop and
  Deadline, is not treated as preemptible. It falls back to the hint only
  when no other preemptible CPU exists. The decision uses
  `is_throttled_at` (`now >= period_start_ns + rt_period_ns` means no longer
  throttled). After the queue is emptied the period boundary stays armed.
  An idle CPU clears the flag when it fires and rewrites the published 255,
  so this CPU is not skipped by selection forever under low load. There is
  no Linux `cpupri` push/pull. `sched_setscheduler` moves FIFO/RR into this
  class.

## Audit checklist

- [ ] A new leave path calls only `leave_current`.
- [ ] EEVDF `curr` stays non-owning.
- [ ] Exit avoids PLACE_LAG.
- [ ] After migration the source RQ's V/weight no longer includes the task
      that left.
- [ ] The Block-path caller holds another strong reference.
- [ ] Tests cover the five dispositions and pick-without-leave.
- [ ] `steal_ready_task` goes through `remove_task` and does not install
      `curr`.
- [ ] The EEVDF pair of trees and the earliest caches agree, and every key
      change unlinks first.
- [ ] A queued task that changes nice subtracts the old weight contribution
      before enqueueing at the new weight, and removal leaves no residual
      charge.
- [ ] An adapter's forward and reverse projections use the same field offset,
      and the tree holds a real `Arc`.
- [ ] `Scheduler` calls still run under the RQ lock, and `curr`'s `Arc` is
      released on leave.
- [ ] Stop and Idle `set_attr` still return `KernelClassImmutable`, and a
      second Stop `enqueue`, or `install_current` while a waiter exists,
      asserts.
- [ ] A zero, reversed, or below-minimum `DlParams` is rejected (not clamped
      or normalized).
- [ ] An admission failure returns before the task attribute changes (`attr`
      is unchanged after rejection).
- [ ] On `advance`, `dequeue`, and `put_prev`, at most one of Deadline's two
      links is linked (never both the ready tree and the throttled tree).
- [ ] RT removal checks queue identity before `cursor_mut_from_ptr`.

Writes of actual runtime are serialized by the owning RQ lock. The atomic
field supports a cross-CPU read only. It does not publish other task state.
A migration must hand off RQ ownership before updating. Accumulation overflow
saturates.
