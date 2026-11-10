# kmutex security and reliability

This analysis covers the entire crate: `src/lib.rs`, `src/mutex.rs`,
`src/wait.rs` and `src/spin.rs`.

## Trust boundaries

Trusted kernel callers supply protected data and spin configuration. The
statically linked `kiface_task_api::TaskExecutionIf` provider supplies
`ktask_types::OwnerKey` task keys, waker-driven waiting and scheduling. The
separate `kiface_task_api::TaskLockDiagnosticsIf` provider supplies diagnostic
bookkeeping. The execution provider must retain identity across migration and
must not reuse a key while mutexes still record that owner. The `OwnerKey`
return type mechanically excludes the unlocked zero sentinel and keeps the low
flag bits clear; key uniqueness and stability remain provider obligations. No
task pointer crosses the interface; hook addresses are opaque identities and
must not be dereferenced by the provider.

There is no direct user-memory, MMIO/PIO, DMA, firmware, filesystem, network,
assembly or device-input boundary. Lock contention can indirectly reflect
external workload. The crate cannot establish scheduler progress on its own.

The typed `Mutex::with_config` constructor accepts the same trusted spin
bounds as the raw constructor and preserves runtime lock-class registration
with statistics enabled. A pause-only budget still ends in the existing
blocking path; callers must remain in sleepable context even when the normal
critical section is short. It does not disable preemption or recover a
descheduled or abandoned owner.

## Unsafe inventory and invariants

`src/mutex.rs` implements unsafe `lock_api::RawMutex`: Acquire CAS from zero
owner part admits exactly one owner; the `Release` owner-clear publishes
protected writes. The raw unlock contract requires one successful acquisition
and exactly one release on the same task. `GuardNoSend` enforces task-local
guard transfer restrictions without preventing the scheduler from migrating
that task across CPUs. The wrong-owner assertion is debug-only diagnostics,
not a recovery mechanism for unsafe misuse.

`src/wait.rs` contributes the waiter-queue unsafe surface:

- `UnsafeRef::from_raw` and `cursor_mut_from_ptr` link and unlink stack-pinned
  nodes. The `Cell`-based links are manipulated only while holding
  `wait_state`; a node is unlinked before its latch is published, and the
  unlocking task performs no node access after `latch` returns, so the waiter
  may drop its frame as soon as it observes the latch.
- `unsafe impl Send for WaitState` records that the intrusive links are never
  touched outside `wait_state`. Cross-task access to a node is limited to the
  atomic `state` and the waker slot it guards: the slot is an `UnsafeCell`
  written on `Empty -> Registered` (poll) and read only after `latch` wins
  `Registered -> Claimed`, and the `AcqRel` transitions on `state` order the two
  accesses so they never run concurrently. `Done` is published after that read,
  so a poll frees the frame only after the unlocker is done with it. The link
  itself stays confined to the guarded critical section.
- `WaitFuture` is pinned with `core::pin::pin!` on the waiter's own stack. It
  borrows the node and cannot escape synchronous `TaskExecutionIf::block_on`.
- The latch/waker handshake is one `state` atomic (`Empty -> Registered ->
  Claimed -> Done`) with no lock: the unlocking task's `latch` claims the waker
  (`Registered -> Claimed`), takes it, then publishes `Done`; a poll stores its
  waker and tries `Empty -> Registered`, and resolves only on `Done`. The
  transitions exclude each other, so a wake published before the first poll
  still resolves the future, and a wake racing registration either claims the
  waker or is observed by the failed poll; no wake is lost. `Done` is published
  after the take, so a poll that resolves and frees the frame cannot overtake
  the unlocker's last node access.

The two wrapper owner-query methods call unsafe `lock_api::Mutex::raw` only
to inspect metadata. They do not mutate protected data or bypass locking.

## Threats and failure modes

| Trigger | Effect | Existing control and residual risk |
| --- | --- | --- |
| Unstable or colliding provider keys | Broken exclusion or wrong-owner diagnostics | `OwnerKey` excludes zero and misaligned keys; the trusted provider must ensure stability and uniqueness so future flag packing cannot alias another task. |
| Unlock racing waiter admission (flag publish vs owner re-read) | Lost progress | The waiter publishes `WAITERS_FLAG` and re-reads the same word; an unlock that clears the owner part either precedes the publish and is seen by the re-read, so the waiter retries, or follows it and returns a word whose preserved `WAITERS_FLAG` routes the unlock to the slow path. Single-location coherence closes the window. |
| Wake published before the waiter stores its waker | Missed wake | Registration and `latch` advance one `state` atomic; a wake that beats the first poll claims straight to `Done`, so the next poll resolves immediately, and a wake racing registration either claims that waker or is observed by the poll's failed `Empty -> Registered` CAS. |
| Waiter observes completion while the unlocker still touches its node | Use-after-free of the pinned frame | The node is unlinked under `wait_state` first, and `latch` is the unlocker's last node access: it claims the waker (`Registered -> Claimed`), takes it, and only then publishes `Done`. A poll frees the frame only on `Done`, so external/spurious re-polls during the take see `Claimed` and stay parked. |
| Recursive blocking acquisition | Deadlock | Owner comparison panics before waiting; use try-lock only where failure is handled. |
| Cross-task guard transfer | Wrong-owner unlock | GuardNoSend rejects safe transfer; unsafe raw-lock users retain the obligation. The owner assertion is debug-only. |
| Non-unwinding task exit while queued | Dangling wait-list link | The provider must unwind task exit through the blocking call or retain the waiter stack; this is the retention obligation any stack-registered waiter requires. |
| Invalid blocking context | Scheduler failure or deadlock | Caller context contract and host yield/wait checks; no local context detector. |
| Provider hook unwind/reentry | Leaked exclusion or lost notification | Hooks must not unwind, block or recursively acquire this mutex. Diagnostic hooks run while `wait_state` is not held. |
| Excessive spin configuration | Excessive latency or shift overflow | Preserve existing configuration semantics; kernel callers must bound values. |
| Abandoned owner or leaked guard | Permanent contention | No poisoning, recovery or fairness guarantee. Wake order is FIFO, but barging by spinners and try-lock callers can still starve waiters. |

The `Release` owner-clear precedes the wrong-owner assertion. The assertion is
diagnostic, not a recovery mechanism for unsafe misuse. A broken raw unlock
contract cannot be made safe by catching it.

## Privacy and audit checks

The crate reports numeric owner keys and mutex addresses to trusted kernel
diagnostics; it does not log protected payloads or retain task handles. Hosts
must keep such identifiers within their normal kernel diagnostic boundary.

- Preserve the admission order: push under `wait_state`, publish `WAITERS_FLAG`,
  then re-read the owner part of the same word before sleeping.
- Preserve the release order: `Release` owner-clear (flag bits kept),
  `WAITERS_FLAG` check, `wait_state` pop, flag restore-or-clear, then drop
  `wait_state`, lock-free `latch`, then wake outside the lock.
- Keep `latch` the unlocking task's last access to the node: it takes the waker
  before publishing `Done`, and a poll frees the pinned frame only on `Done`.
- Keep every flag transition under `wait_state` and every owner test masked by
  `!OwnerKey::FLAG_MASK`; the single word couples the admission against the
  release.
- Keep all scheduling and time access behind the task provider.
- Keep watchdog declarations unconditional and invocations feature-gated.
- Keep guards non-Send and owner identity independent of CPU placement.
- Validate real scheduler behavior in the existing integration suites; the
  compile-fail doctest alone proves only the guard's type-level restriction.
