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

PELT is advisory history, not admission or scheduling authority. Its entity
cells use the existing owning-RQ token; `task_pelt` must not inspect an entity
under another CPU's scheduler lock. `detach_blocked_pelt` accepts only blocked
PELT residency and is called under the source owner lock before its owner CPU
changes. Runnable removals detach through dequeue/steal, and migration/exit
of the current task detach through the leave path. Class changes, including
PI, remove Fair history before another class can accumulate execution.

`steal_fair` is a bounded source-RQ operation, independent of higher-class
execution. Its predicate receives refreshed source/entity history under the
same lock and must validate host task state, `!on_cpu` and destination affinity
without acquiring another RQ lock. Parked Fair entities are skipped inside the
class. Only an actual queued owner can be removed, so a lone off-tree runner
is never migrated by this API. The numeric scan cursor retains no owner or raw
pointer and resumes by tree range lookup, even when its old key was removed.
The destination must publish only after the source lock has been released.

Blocked aggregates retain numeric history only, so neither sleeping-task scans
nor additional task-reference lifetimes are introduced. Time jumps backwards
are handled without unsigned underflow; forward gaps use bounded table lookup.
Non-zero nice weights and an internal non-zero divider make divisions total.
Saturating sums/subtraction and minimum-divider floors protect aggregation
against overflow and independent entity/queue rounding. No unsafe operation
or allocation is added. Capacity/frequency invariance and IRQ-time exclusion
are not provided; the resulting samples must not be treated as normalized
hardware utilization or a hard capacity guarantee.

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
`DeferredArc` payload header. Each owning tree/cache and `current` retains its own count. Fair uses one
library-owned tree with a borrowed root mirror and no secondary index. A non-empty queue destructor unlinks one
by one. It does not `fast_clear`, and it does not destroy a Deferred payload
in place. Existing owner probes cover the earlier queue implementations; the new
Fair tree still needs its own prepared probes executed.

RT removal by task must use `&Owner`'s raw pointer. A pointer derived from
`&SchedTask` must not cross the payload header. Miri reported a borrow-stack
violation on that path. After the fix to `Owner::as_ptr`, the RT and core raw
cases pass strict-provenance Miri. Deadline's intrusive-collections 0.10.2 RB trees encode parent/color as
integer pointers; their existing checks used default provenance mode and
are not a strict-provenance pass. Fair supplies typed parent pointers and a separate color byte through
custom RBTreeOps; the library-backed revision has not run under Miri.

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
`FairAdapter`'s field-projection pattern. `get_link` / `get_value` project fields forward and back on
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
library's atomic link operations. No link or identity field is public. The ready-list adapter adds no hand-written `Send` or `Sync`.

`ReadyList::steal` borrows the task through the cursor `get`. The predicate
receives only `&T`. It does not restore a temporary `Arc` and does not
increment or decrement a strong reference. The cursor borrow covers the
predicate. On a hit the owner mark is cleared first, the task borrow ends,
and `remove` then transfers the list's existing owning reference. A rejection
or a predicate panic does not move list ownership.

`FairAdapter` projects `fair.ready_link`, a `FairLink` embedded in
`SchedTask<T>`. Both directions preserve the original PointerOps provenance.
`BoundAdapter` delegates exactly that projection, pointer ownership and key
lookup, replacing only link operations. Adapter instances are retained in one
private Arc context; no clone-equivalence assumption about adapters is needed.
`KeyAdapter::get_key` and `FairTreeAdapter::vruntime` require the owning RQ;
exclusive clear/drop never invokes those scheduling accessors.
`FairTreeAdapter` is unsafe: its vruntime accessor must not panic or reenter
the tree while pending removed-node references are being repaired. Production
uses one RqCell read; the probe adapters read immutable scalar fields.

`fair_tree.rs`'s custom RBTreeOps stores the exact children, parent and color
requested by intrusive-collections. Structural setters cannot panic or
allocate; child edits record dirty nodes without following transient parents.
After the library completes balancing, the wrapper repairs summaries through
final ancestry. Root changes are mirrored by parent-to-None and root release;
the mirror has no independent owner and is never read during an in-flight edit.
All insertion/deletion balancing and owner recovery stay in the library.

Unsafe pointer access is confined to live embedded links and the original
adapter inverse projection in `Context::value`. Linked nodes are retained by
the library tree. A removed node remains RETIRED until the wrapper consumes
its dirty-list entry while holding the returned owner. Atomic membership
remains occupied during this interval, preventing another tree from acquiring
and overwriting pending metadata. The final UNLINKED Release follows repair
and pairs with Acquire on reuse. Color changes never expose an unlinked state.

FairLink's Cell metadata and MutationState are shared only inside one wrapper
and its bound adapter, serialized by RQ/exclusive mutation. Their Send/Sync
arguments depend on those private context handles never escaping. Moving a
wrapper moves both handles and all owners together; shared wrapper access
requires Sync adapter/value/owner bounds. Atomic membership alone is safe to
observe remotely and does not publish scheduling metadata.

Drop enables clearing mode, drains the library tree, and discards pending
pointers without dereferencing them after owner destruction. Clearing mode
releases links immediately because no summary repair follows; normal removal
instead retains the node reservation through repair. The library tree is
empty before either private context handle can become the final owner.

Prepared invariant/oracle/ownership probes have not been executed for this
revision. Type checking alone does not establish augmentation or lifetime
correctness; prior AVL/hand-balanced-tree results do not validate this wrapper.

`RqCell`'s `get` and `set` read and write an `UnsafeCell`. A read requires a
valid pointer, an initialized value, and no concurrent write. A write also
requires exclusive access. `RqLocked` plus a complete `Copy` value from
`new` and `set` provide all three. `seat`, `attr`, Fair virtual time, and
`rr_left_ns` use the same token.

`class` and `rt_prio` are `AtomicU8`. `publish_dispatch` stores `rt_prio`
with Release and then `class` with Release. `sched_class()` and
`rt_priority_snapshot()` load with Acquire, so a CPU-selection read that
observes the new class also observes the priority stored before it. The
reverse order would let `select_run_queue` observe `Rt` and then a stale
`0`. `seat` stays in an `RqCell` and is read only under the lock.

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

Steal borrows the task through the cursor `get`. The predicate receives only
`&T`. On a hit the owner word is cleared first, and `cursor.remove` then
transfers the list's existing owning reference.

## Memory-safety invariants

1. The library retains one owner per Fair node. Normal removal holds its
   returned owner and RETIRED reservation through dirty repair. Drop uses
   clearing mode and never follows pending references after releasing owners.
2. EEVDF `curr` must not hold an `Arc`. It stores only a scheduling snapshot.
3. `curr` must be empty before `pick_next_task` (`leave_current` has run).
4. Exit must not set PLACE_LAG. Block and Migrate must set it for the later
   `enqueue_task`.
5. After a running entity's scheduling fields change, the `curr` snapshot
   must be refreshed.
6. Fair has one library-owned tree. Linked keys/vruntime remain immutable;
   all completed edits drain the dirty list and repair final ancestor paths.
   The root mirror and summary agree with the library topology.
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

`Scheduler` is not a concurrently shareable type. Concurrency safety
comes from `ktask`'s per-CPU RQ lock and the IRQ/preemption constraints.

## Threat analysis

| ID | Threat | Impact | Trigger | Mitigation |
|------|----------|----------|----------|----------|
| T-01 | A missing `leave_current` leaves a stale `curr` or skewed statistics | High | A new leave path is not wired to the shared API | One `leave_current`; EEVDF `pick_next` asserts |
| T-02 | The scheduler holds a running task's `Arc` and pins its lifetime | High | The `curr` strong reference is not released | `Scheduler::curr` owns the runner and `leave` takes and returns that reference |
| T-03 | Exit sets PLACE_LAG by mistake | Medium | Exit and Block share sleep accounting | A separate `LeaveReason::Exit` branch |
| T-04 | The source RQ still counts a task's weight after migration | Medium | Migrate does not clear current accounting | Migrate goes through `leave_current` and clears the snapshot |
| T-05 | A stale `curr` snapshot causes the wrong preemption or placement | Medium | A running entity's fields change without a refresh | `update_current` / `set_priority` refresh; test helpers sync explicitly |
| T-06 | Idle-pull steals an entity that is still `on_cpu` | High | A remote `steal_ready_task` after yield/preempt enqueued the task | The scheduler removes only a ready waiter; the `ktask` predicate rejects `on_cpu` and does not `pick_next` |
| T-07 | A stale Fair link, summary or cache selects or holds the wrong task | High | Removal, key changes, balancing or Drop violate tree invariants | Library balancing; RQ-serialized final-path repair; RETIRED reuse barrier; prepared invariant, oracle, ownership and reuse probes |
| T-08 | `Scheduler` writes an `RqCell` without the RQ lock | High | The caller breaks the lock contract | Documented; the token is built at a `Scheduler` entry, at unpublished init, and inside tree `get_key` adapters that only run from class methods under a `Scheduler` entry |
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

- `ktask` creates one `migration/%u` Stop task per run queue and runs the
  stop-machine. `SCHED_DEADLINE` must carry a valid
  `DlParams`, or `SchedAttr::validate` rejects it with `InvalidPolicy`. A
  per-RQ admission failure is `AdmissionFull`, not a permission check.
- `sched_setscheduler` / `sched_setattr` move FIFO/RR into `RtClass` and
  `SCHED_DEADLINE` into Deadline. Realtime and Deadline policies require
  effective `CAP_SYS_NICE`. `RLIMIT_RTPRIO` is still not consulted.
- `set_pi_boost` changes the effective class and RT priority without
  changing `Scheduler::attr`. Fork copies that user attribute and clears the
  boost. This crate does not authenticate the caller; a boost is trusted the
  same way a user attribute is trusted after the syscall layer has checked
  it. A boosted Fair task is charged to this RQ's RT bandwidth and does not
  rotate as `SCHED_RR`. Deadline dispatch ignores the boost.
  `ktask::TaskPiHost` submits boosts through the migration-aware owner-RQ lock
  and reads the atomic donation snapshot under the PI graph lock.
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
- [ ] Every linked key/vruntime change removes the Fair node first; final
      path repair drains all dirty entries and agrees with the mirrored root.
- [ ] Normal unlink retains the owner and RETIRED reservation through repair;
      release never permits early reuse of pending metadata.
- [ ] Owner transfer/cache cloning stay in the library and delegate the original
      adapter projection. No tree balancing is duplicated in ksched.
- [ ] Nonempty Drop uses clearing mode, reads no scheduling fields, and never
      dereferences discarded pending pointers after owners are released.
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
