# kfd_objects — Design

## Purpose and scope

This crate owns epoll, eventfd, signalfd and timerfd object state and file
operations. `src/lib.rs` publicly exposes `epoll`, `eventfd`, `signalfd` and
`timerfd`, implemented in the corresponding source files. `ksyscall` decodes ABI
arguments, resolves descriptor tables and validates operation-specific flags;
`kfd_objects` accepts resolved `VfsFile` references and typed object inputs.
`anon_inodefs` creates anonymous files, `kvfs` dispatches their file operations,
`kpoll` owns poll registration and `ktask::future` supplies blocking/timer runtime.
Credentials are explicit constructor inputs captured by `VfsFile`, not duplicated
in object state. This crate does not own process descriptor-number allocation.

## Object architecture and direct flows

| Object | State and providers | Flow |
|---|---|---|
| `EventFd` | Atomic counter, semaphore flag, read/write `PollSet`s | `new_file` constructs anonymous private data; file read atomically decrements/reset-count, write conditionally adds, and each wakes the opposite side. |
| `TimerFd` | Clock ID, `SpinNoIrq<TimerFdInner>`, timer-handle lock and `PollSet` | `settime` validates a deadline, cancels the old handle, updates state and arms a `ktask::future` timer; callback/read/poll tick expiration state. |
| `Signalfd` | `RwLock<SignalSet>` and read `PollSet` | Each file read/poll resolves the current `ThreadSignalManager`, selects pending signals by the mask and serializes one 128-byte record per read. |
| `Epoll` | `Arc<EpollInner>`, interest table, ready queue and poll set | `add`/`modify` register watched file poll sources; callbacks enqueue weak interest references; `poll_events` returns ready ABI events and rearms interests. |

`from_file` retrieves the typed private data and rejects an unrelated file.
The anonymous file operations implement `kvfs::FileOperations`; eventfd/timerfd
and epoll implement `kpoll::Pollable`, while signalfd uses a current-thread access
wrapper. These are provider-to-object callbacks, distinct from syscall entry
points. A constructor/use example is in crate rustdoc.

## Execution context

File construction requires allocator and initialized anonymous-inode filesystem.
`read`/`write` may block through `block_on(poll_io(...))` unless the file carries
NONBLOCK. The authoritative nonblocking flag is on `VfsFile`. Epoll control uses
a sleepable mutex and may allocate; it must run in task context. Signalfd data
access requires a current user thread; a kernel task gets
`OperationNotPermitted` on read and ERR readiness. Object construction itself
requires no current credentials because the caller supplies them.

Timer polling/rearming requires initialized clock/timer/scheduler services.
Timer callbacks use IRQ-safe inner state. Epoll's ready queue also uses an
IRQ-masking lock because source wakers may enqueue an interest directly from
timer IRQ context. Its config and table locks only disable preemption. Do not
infer interrupt-context support for arbitrary watched-file callbacks from the
object API. No API is a general early-boot or reentrant-syscall guarantee;
callers must satisfy provider context contracts and avoid holding locks across
calls that can block.

## State machines and algorithms

Timer state is disarmed (`deadline=None`) or armed with a monotonic/realtime
deadline. Expiration of a one-shot timer increments the saturated count and
clears the deadline. A periodic timer counts overdue periods and advances its
deadline by that many intervals; representational overflow can clear the next
deadline. Read consumes accumulated expirations. `settime` with zero value clears
the interval/count/deadline. Validation happens before cancellation, preserving
the previous timer when a new deadline is unrepresentable.

Eventfd read decrements by one in semaphore mode and by the whole counter
otherwise. The current implementation copies the pre-update counter to the
caller in both modes; it does not claim Linux's semaphore-mode return value of
one. Write rejects `u64::MAX` and only commits when the new counter is below it.

Epoll `TriggerMode` has Level, Edge and OneShot `{ fired }` states. OneShot
consumption sets `fired=true`; `modify` rearms by replacing configuration on the
same interest identity. Level-ready interests are deferred for the next poll;
Edge/OneShot consumption removes the current ready entry and handles rearming.
The ready scan is bounded by the queue length at entry and deduplicates keys,
so synchronous requeue cannot keep one call running indefinitely. A key contains
both fd number and weak file identity. Generation checks reject stale wakers.

## Concurrency and resource lifecycle

Eventfd uses acquire loads and release/acquire fetch-update loops for its counter.
Timer inner state and handle slots have separate `SpinNoIrq` locks. These protect
their fields; they do not by themselves make every cancel/register/reprogram
sequence one atomic transaction. Registered timer wakers own `Arc<TimerFd>`;
`Drop` cancels a retained handle when final destruction actually occurs. Closing
a file alone must not be described as proving immediate destruction, since a
registered callback can retain the object.

Signalfd mask changes take the RwLock and wake readers. Signal queues belong to
`ksignal`, not this object. Epoll's `ctl_lock` serializes ADD/MOD/DEL including
registration failure rollback. `SpinNoPreempt` locks protect the table and
per-interest configuration; `SpinNoIrq` protects the IRQ-reachable ready queue.
Atomics track queue membership and waker generation. Registration owners are
replaced under their lock and dropped outside it to avoid nested unregister
locks. The table owns interests; ready entries, watched files and callback owner
references use weak links where specified by their structures, so expired
targets can be removed while polling.

## Decisions and limitations

Keeping backend state here allows syscall adapters to handle descriptor lookup
without creating process dependencies for each object. Stable MOD identities
prevent queued events from referring to an abandoned configuration object.
Current timer clock selection treats MONOTONIC/BOOTTIME as monotonic and other
IDs as realtime; the adapter must restrict accepted clock IDs. Signalfd mask
filtering (including uncatchable signals) belongs to its syscall adapter. This
backend does not promise complete Linux epoll graph/cycle validation, timer
clock-change cancellation, semaphore-mode return compatibility or synchronous
file-close cancellation of retained timer callbacks.
