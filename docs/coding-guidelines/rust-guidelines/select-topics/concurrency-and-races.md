# Concurrency and Races

Concurrency code is reviewed with extreme rigor.
Lock ordering, atomic correctness, memory ordering,
and race condition analysis are all demanded explicitly.

### Establish and enforce a consistent lock order (`lock-ordering`) {#lock-ordering}

Acquiring two locks in different orders
from different code paths
is a potential deadlock.
Hierarchical lock order must be established and documented.

```rust
pub(super) fn set_control(
    self: Arc<Self>,
    process: &Process,
) -> Result<()> {
    // Lock order: group of process -> session inner -> job control
    let process_group_mut = process.process_group.lock();
    // ...
}
```

See also:
PR [#2942](https://github.com/asterinas/asterinas/pull/2942).

### Never do I/O or blocking operations while holding a spinlock (`no-io-under-spinlock`) {#no-io-under-spinlock}

Holding a spinlock while performing I/O
or blocking operations is a deadlock hazard.
Use a sleeping mutex or restructure
to drop the lock first.

```rust
// Good — spinlock dropped before I/O
let data = {
    let guard = self.state.lock(); // state: SpinLock<...>
    guard.pending_data.clone()
};
self.device.write(&data)?;

// Bad — I/O while holding spinlock
let guard = self.state.lock(); // state: SpinLock<...>
self.device.write(&guard.pending_data)?;
```

See also:
PR [#925](https://github.com/asterinas/asterinas/pull/925).

### Choose the lock type from the execution context (`lock-by-context`) {#lock-by-context}

Pick the weakest lock
that matches the real concurrency boundary.
Do not default to IRQ-disabling spinlocks.

- Use `ksync::Mutex` or other sleepable primitives
  in ordinary task context
  when the path may block, wait, sleep,
  or call into poll/wake machinery.
- Use `SpinNoPreempt`
  for short non-blocking critical sections
  in task context
  when IRQ handlers do not touch the same state.
- Use `SpinNoIrq`
  only when local IRQ handlers
  or other interrupt-context code
  can race on the same state,
  or when the API must be callable from IRQ context.
- Use `SpinRaw`
  only when the caller already guarantees
  that IRQs and preemption are disabled,
  and document that precondition explicitly.

If there is no interrupt-context contender,
do not use `SpinNoIrq`.
The wider critical-section semantics
make later blocking bugs much easier to introduce.

```rust
// Good — sleepable mutex in task context
fn update_connection(&self) {
    let mut conn = self.conn.lock(); // ksync::Mutex<_>
    conn.apply_update();
    conn.wait_queue.notify_all(true);
}

// Good — short non-blocking task-context spinlock
fn push_local_stat(&self, delta: usize) {
    let mut stats = self.stats.lock(); // SpinNoPreempt<_>
    stats.rx_packets += 1;
    stats.rx_bytes += delta;
}

// Bad — no IRQ competitor, but IRQ-disabling lock used anyway
fn update_state(&self) {
    let mut state = self.state.lock(); // SpinNoIrq<_>
    state.advance();
}
```

### Never acquire sleepable locks while holding a spinlock (`no-sleepable-lock-under-spinlock`) {#no-sleepable-lock-under-spinlock}

Once a spinlock is held,
the remaining path must stay non-blocking.
Do not enter `ksync::Mutex`,
`RwLock`, `Semaphore`, `WaitQueue`,
`block_on`, `sleep`, or similar paths
until the spinlock is dropped.

This includes framework-provided wrapper locks.
If a driver or runtime callback is entered
under a spinlock,
limit that lock scope
to the actual register or queue access,
then drop it before touching sleepable subsystem state.

```rust
// Good — extract device event under the spinlock, then drop it
let event = self.device.with_mut(|dev| dev.poll_event())?;
match event {
    Some(event) => {
        let mut conn = self.conn.lock(); // sleepable lock after spinlock scope
        conn.handle(event);
    }
    None => {}
}

// Bad — sleepable mutex reached while still under framework spinlock
self.device.with_mut(|dev| {
    let mut conn = self.conn.lock();
    conn.handle(dev.poll_event()?);
    Ok::<_, Error>(())
})?;
```

### Do not use atomics casually (`careful-atomics`) {#careful-atomics}

When multiple atomic fields
must be updated in concert, use a lock.
Only use atomics when a single value
is genuinely independent.

```rust
// Good — a lock protects correlated fields
struct Stats {
    inner: SpinLock<StatsInner>,
}
struct StatsInner {
    total_bytes: u64,
    total_packets: u64,
}

// Bad — two atomics that must be consistent
// but can be observed in an inconsistent state
struct Stats {
    total_bytes: AtomicU64,
    total_packets: AtomicU64,
}
```

### Critical sections must not be split across lock boundaries (`atomic-critical-sections`) {#atomic-critical-sections}

Operations that must be atomic
(check + conditional action)
must happen under the same lock acquisition.
Moving a comparison outside the critical region
is a correctness bug.

```rust
// Good — check and action under the same lock
let mut inner = self.inner.lock();
if inner.state == State::Ready {
    inner.state = State::Running;
    inner.start();
}

// Bad — TOCTOU race: state can change
// between the check and the action
let is_ready = self.inner.lock().state == State::Ready;
if is_ready {
    self.inner.lock().state = State::Running;
    self.inner.lock().start();
}
```

See also:
PR [#2277](https://github.com/asterinas/asterinas/pull/2277).
