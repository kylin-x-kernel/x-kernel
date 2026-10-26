# Concurrency

Use this file when the change touches locks,
atomics, interrupts, wait/wake paths, or shared mutable state.

## Mandatory Rules

- establish and preserve a consistent lock order;
- use atomics only for genuinely independent state;
- if multiple fields must change consistently,
  prefer a lock over scattered atomics;

## Lock Selection By Context

- use `ksync::Mutex` or other sleepable primitives
  in ordinary task context when the protected path
  may block, wait, call into poll/wake code,
  or hold the lock across non-trivial work;
- use `SpinNoPreempt` only for short non-blocking
  critical sections in task context when preemption
  must not interleave updates on the current CPU,
  but IRQ handlers do not touch the same state;

## X-Kernel Lock Guidance

- if state is shared between task and IRQ context,
  split the design into:
  a tiny IRQ-safe data path under `SpinNoIrq`
  and a larger sleepable path outside it;

## Lock Construction (`KFEAT_LOCK_STAT`)

`KFEAT_LOCK_STAT` is off by default on release configs, including QEMU
virt defconfigs. Enable it only when linting or collecting `/proc/lock_stat`:

```bash
cp platforms/kplat-aarch64/qemu_defconfig .config
make defconfig
# Task Scheduler → Task Diagnostics → Lock contention statistics
# or append: KFEAT_LOCK_STAT=y
make olddefconfig
make clippy   # or make build / make run
```

`make clippy` follows `.config`; do not special-case the feature
in the build tool.

### Heap and lazy runtime init

Use `Mutex::new` / `RwLock::new` (and `Arc::new(Mutex::new(...))`,
`Lazy::new(|| Mutex::new(...))`). With `stats` they bind a per-init-site
class via `#[track_caller]` and are **not** `const`.

## Workerqueue Usage

- use `kworkng` when hardirq, softirq, timer, or other non-sleepable paths need
  to defer sleepable work into task context;
- use `NormalQueue::new()` when a subsystem needs independent routing,
  flush/closure or per-CPU active limits; system defaults share normal workers;
- use safe static work publication for kernel-lifetime Work; borrowed Work and
  BH submission remain unsafe because the caller must pin and retain storage;
- synchronous flush, cancel, shutdown and queue drain require sleepable context;
- stop external producers and shut down owned handles before subsystem teardown;
  drain delayed timers explicitly before closing their queue. Queue closure alone
  does not cancel caller-owned timers or remove callback dependency cycles.

Example:

```text
let queue = kworkng::NormalQueue::new("net-reset", config)?;
let reset_work = kworkng::WorkHandle::new(move || reset_device())?;

irq_handler_or_softirq_path() {
    reset_work.queue(&queue, None)?;
}

device_teardown() {
    stop_irq_or_other_producers();
    reset_work.shutdown()?;
    queue.close_and_drain()?;
}
```

## Design Guidance

- keep lock scope narrow;
- make wait/wake behavior explicit in code review;
- treat IRQ context, task context, and sleepability
  as first-class parts of the contract;
- prefer simple synchronization stories over clever ones.

## When Reviewing

Check specifically for:

- lock inversion risk;
- atomics used where a lock would be clearer and safer;
- unstated assumptions about IRQ state, scheduler availability,
  or wakeup ordering;
