# kmutex security and reliability

This analysis covers the entire crate: `src/lib.rs`, `src/mutex.rs` and
`src/spin.rs`.

## Trust boundaries

Trusted kernel callers supply protected data and spin configuration. The
statically linked `kiface_task_api::TaskExecutionIf` provider supplies `NonZeroU64`
task keys, waker-driven waiting and scheduling. The separate
`kiface_task_api::TaskLockDiagnosticsIf` provider supplies diagnostic bookkeeping.
The execution provider must retain
identity across migration and must not reuse a key while mutexes still record
that owner. The return type mechanically excludes the unlocked zero sentinel;
key uniqueness and stability remain provider obligations. No task pointer
crosses the interface; hook addresses are opaque
identities and must not be dereferenced by the provider.

There is no direct user-memory, MMIO/PIO, DMA, firmware, filesystem, network,
assembly or device-input boundary. Lock contention can indirectly reflect
external workload. The crate cannot establish scheduler progress on its own.

## Unsafe inventory and invariants

`src/mutex.rs` implements unsafe `lock_api::RawMutex`: Acquire CAS from zero
admits exactly one owner; Release unlock publishes protected writes. The raw
unlock contract requires one successful acquisition and exactly one release
on the same task. `GuardNoSend` enforces task-local guard transfer restrictions
without preventing the scheduler from migrating that task across CPUs.

The two wrapper owner-query methods call unsafe `lock_api::Mutex::raw` only
to inspect metadata. They do not mutate protected data or bypass locking.
Stack listener pinning uses the existing `event_listener::listener!` macro and
safe `core::pin::pin!`; the borrowed future cannot escape synchronous `TaskExecutionIf::block_on`.
No new manual Send/Sync implementations or raw future lifetime extensions exist.

## Threats and failure modes

| Trigger | Effect | Existing control and residual risk |
| --- | --- | --- |
| Unstable or colliding provider keys | Broken exclusion or wrong-owner diagnostics | NonZeroU64 excludes zero; the trusted provider must ensure stability and uniqueness. |
| Unlock racing waiter registration | Lost progress if admission were reordered | Listener registration precedes Acquire recheck; release notifies one. |
| Recursive blocking acquisition | Deadlock | Owner comparison panics before waiting; use try-lock only where failure is handled. |
| Cross-task guard transfer | Wrong-owner unlock | GuardNoSend rejects safe transfer; unsafe raw-lock users retain the obligation. |
| Invalid blocking context | Scheduler failure or deadlock | Caller context contract and host yield/wait checks; no local context detector. |
| Provider hook unwind/reentry | Leaked exclusion or lost notification | Hooks must not unwind, block or recursively acquire this mutex. |
| Excessive spin configuration | Excessive latency or shift overflow | Preserve existing configuration semantics; kernel callers must bound values. |
| Abandoned owner or leaked guard | Permanent contention | No poisoning, recovery or fairness guarantee. |

The Release swap precedes the wrong-owner assertion as in the original
implementation. This assertion is diagnostic, not a recovery mechanism for
unsafe misuse. A broken raw unlock contract cannot be made safe by catching it.

## Privacy and audit checks

The crate reports numeric owner keys and mutex addresses to trusted kernel
diagnostics; it does not log protected payloads or retain task handles. Hosts
must keep such identifiers within their normal kernel diagnostic boundary.

- Preserve listener registration, owner recheck, wait, and retry order.
- Preserve Acquire admission, Release publication, and notify-one behavior.
- Keep all scheduling and time access behind the task provider.
- Keep watchdog declarations unconditional and invocations feature-gated.
- Keep guards non-Send and owner identity independent of CPU placement.
- Validate real scheduler behavior in the existing integration suites; the
  compile-fail doctest alone proves only the guard's type-level restriction.
