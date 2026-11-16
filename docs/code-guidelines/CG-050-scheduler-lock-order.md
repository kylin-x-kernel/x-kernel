---
id: CG-050-scheduler-lock-order
level: MUST
paths: ["task/ktask/src/**/*.rs", "task/kspin/src/**/*.rs"]
checks:
  - backend: ai
triggers:
  - "SpinNoIrq"
  - "SpinNoPreempt"
  - "static_lock!"
  - "blocked_resched"
  - "park_task"
---

# CG-050-scheduler-lock-order: Keep scheduler lock nesting acyclic

- **Rule**: Code MUST keep the scheduler lock order one-way and acyclic:
  `run-queue lock -> bandwidth registry (TRACKED, MEMBERS) -> task state
  atomics`. Code MUST NOT (a) take a run-queue lock while holding a bandwidth
  registry lock, (b) hold `TRACKED` and `MEMBERS` at the same time, (c) block on
  a global lock from an accounting point that already holds the run-queue lock,
  or (d) skip a bandwidth registration because its lock was contended.
- **Scope**: `ktask` scheduler paths (`run_queue/**`, `park.rs`, `future/**`,
  `completion.rs`, `freezer.rs`) and any `kspin` primitive they use. Wakers may
  be called from IRQ context on a remote CPU, so a blocking acquisition inside
  these paths can deadlock two CPUs that hold each other's lock.
- **Why**: `RunQueue::blocked_resched` runs with the run-queue lock held and
  IRQs disabled. It reaches cgroup bandwidth accounting, which uses process-wide
  `static_lock!` registries. Holding those locks across a scheduling point
  means a remote waker spells in the parker's run-queue lock, and a CPU that
  takes a registry lock first can then wait for the run queue: an AB/BA cycle
  that wedges every CPU with interrupts disabled. An observed instance on
  x86_64 had three CPUs spinning on `bandwidth::TRACKED` while a fourth spun on
  a `KWaker` wake latch.
- **Do**: keep registry locks as leaves — collect rollover work into a local
  buffer, release the registry lock, and only then update task states. Skipping
  a registration is never acceptable: it would let a group escape `cpu.max`.
- **Don't**: take `MEMBERS` while holding `TRACKED`; allocate under a registry
  lock; hold one registry lock while acquiring the other; update task state
  under a registry lock; add a new `static_lock!` to `run_queue/**` without
  appending it to the order chain above.
- **Not this rule**: [CG-013](CG-013-no-sleep-under-spinlock.md) covers sleeping
  or performing I/O under a spinlock, and
  [CG-015](CG-015-no-split-critical-section.md) covers splitting one critical
  section. This rule covers *ordering* between two holders that never sleep.
