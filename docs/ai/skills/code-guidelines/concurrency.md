# Concurrency

Use this file when the change touches locks,
atomics, interrupts, wait/wake paths, or shared mutable state.

## Mandatory Rules

- establish and preserve a consistent lock order;
- do not perform I/O or blocking work while holding a spinlock;
- do not acquire blocking synchronization primitives
  such as `ksync::Mutex`, `RwLock`, `Semaphore`,
  or `WaitQueue` while holding a spinlock;
- do not split atomic critical sections across separate lock acquisitions;
- use atomics only for genuinely independent state;
- if multiple fields must change consistently,
  prefer a lock over scattered atomics;
- when memory ordering matters,
  explain it in code comments or surrounding docs.

## Lock Selection By Context

- use `ksync::Mutex` or other sleepable primitives
  in ordinary task context when the protected path
  may block, wait, call into poll/wake code,
  or hold the lock across non-trivial work;
- use `SpinNoPreempt` only for short non-blocking
  critical sections in task context when preemption
  must not interleave updates on the current CPU,
  but IRQ handlers do not touch the same state;
- use `SpinNoIrq` only when the same state is accessed
  from local IRQ or similarly IRQ-like contexts,
  or when the caller must be valid in interrupt context;
- use `SpinRaw` only when the caller already guarantees
  IRQs and preemption are disabled for the whole critical section,
  and document that precondition explicitly.

## X-Kernel Lock Guidance

- do not use `SpinNoIrq` as the default "safe" choice;
  if there is no interrupt-context contender for the same state,
  prefer a sleepable lock or `SpinNoPreempt`;
- if a path may reach `block_on`, `wait`, `sleep`,
  event listeners, scheduler operations, or code that can return
  `WouldBlock` and then wait, the path must not hold a spinlock;
- if driver callbacks are wrapped by a class or runtime spinlock,
  keep that lock scope tightly around the actual driver register
  or queue access, then drop it before touching sleepable subsystem locks;
- if state is shared between task and IRQ context,
  split the design into:
  a tiny IRQ-safe data path under `SpinNoIrq`
  and a larger sleepable path outside it;
- document context assumptions in the API contract:
  whether the function may run in IRQ context,
  whether it may sleep,
  and which lock families callers may hold.

## Workerqueue Usage

- use `kwork` when hardirq, softirq, timer, or other non-sleepable paths need
  to defer sleepable work into task context;
- use `kwork::schedule_work()` / `kwork::schedule_work_on()` for small shared jobs that do not
  need subsystem ordering or teardown isolation;
- use `WorkQueueHandle::alloc()` when a driver or subsystem needs its own ordered
  work stream, bounded worker pool, or explicit `WorkQueueHandle::destroy()`
  lifecycle;
- use delayed work for timer-triggered task-context work, not as a replacement
  for ordinary softirq polling or immediate IRQ bottom halves;
- queueing APIs may be used from IRQ-like context, but `ScheduledWork::flush()`,
  `ScheduledWork::cancel_sync()`, `DelayedScheduledWork::flush()`,
  `DelayedScheduledWork::cancel_sync()`, `WorkQueue::flush()`,
  `WorkQueueHandle::flush()`, and `WorkQueueHandle::destroy()` must run from
  sleepable task context;
- before freeing state captured by a work callback, stop new producers first,
  then cancel or flush the work, and finally destroy any dynamic workqueue.

Example:

```text
let queue = kwork::WorkQueueHandle::alloc("net-reset", kwork::WorkQueueAttrs::new())?;
let reset_work = kwork::ScheduledWork::new(|work| {
    // Runs in kwork task context; sleeping operations are allowed here.
});

irq_handler_or_softirq_path() {
    let _ = queue.queue_work(&reset_work);
}

device_teardown() {
    stop_irq_or_other_producers();
    reset_work.cancel_sync()?;
    queue.destroy()?;
}
```

## Design Guidance

- keep lock scope narrow but not so fragmented
  that invariants leak across reacquisitions;
- make wait/wake behavior explicit in code review;
- treat IRQ context, task context, and sleepability
  as first-class parts of the contract;
- prefer simple synchronization stories over clever ones.

## When Reviewing

Check specifically for:

- lock inversion risk;
- blocking operations under spinlocks;
- `SpinNoIrq` used where no IRQ-context competitor exists;
- sleepable locks acquired while a spinlock,
  IRQ-disable guard, or preempt-disable guard is held;
- TOCTOU bugs caused by split critical sections;
- atomics used where a lock would be clearer and safer;
- unstated assumptions about IRQ state, scheduler availability,
  or wakeup ordering.
