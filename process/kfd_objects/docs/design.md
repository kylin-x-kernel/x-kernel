# kfd_objects — Design

Kernel unit tests live in `ut-tests/` and are mounted from their original
source modules with `unittest::path_mod_test!` under `cfg(unittest)`.

## Purpose and scope

This crate owns epoll, eventfd, signalfd and timerfd object state and file
operations. `src/lib.rs` publicly exposes `epoll`, `eventfd`, `signalfd` and
`timerfd`, implemented in the corresponding source files. `ksyscall` decodes ABI
arguments, resolves descriptor tables and validates operation-specific flags;
`kfd_objects` accepts resolved `VfsFile` references and typed object inputs.
`anon_inodefs` creates anonymous files, `kvfs` dispatches their file operations,
`kpoll` owns poll registration, `ktask::future` supplies blocking, and
`ktask::Timer` supplies owned one-shot timer delivery and synchronous teardown.
Credentials are explicit constructor inputs captured by `VfsFile`, not duplicated
in object state. This crate does not own process descriptor-number allocation.

## Object architecture and direct flows

| Object | State and providers | Flow |
|---|---|---|
| `EventFd` | Atomic counter, semaphore flag, read/write `PollSet`s | `new_file` constructs anonymous private data; file read atomically decrements/reset-count, write conditionally adds, and each wakes the opposite side. |
| `TimerFd` | Owned `ktask::Timer<TimerFdHandler>` with embedded clock/count/poll state, programming `Mutex<()>` | `settime` validates and drains the old delivery before replacing state; callback/read/poll/gettime account elapsed periods. Read rearms the next periodic notification. |
| `Signalfd` | `RwLock<SignalSet>` | Each file read/poll resolves the current `ThreadSignalManager`, selects pending signals by the mask and serializes one 128-byte record per read. Signal configuration owns the shared notification source. |
| `Epoll` | `Arc<EpollInner>`, interest table, ready queue and poll set | `add`/`modify` register watched file poll sources; callbacks enqueue weak interest references; `poll_events` returns ready ABI events and rearms interests. |

`from_file` retrieves the typed private data and rejects an unrelated file.
The anonymous file operations implement `kvfs::FileOperations`; eventfd/timerfd
and epoll implement `kpoll::Pollable`, while signalfd uses a current-thread access
wrapper. These are provider-to-object callbacks, distinct from syscall entry
points. A constructor/use example is in crate rustdoc.

## Execution context

Eventfd read/write and timerfd read use `kvfs::poll_file_io`: an initial operation
attempt and one post-signal attempt preserve ready/nonblocking behavior. A still-blocked
signal wait returns the file-wait marker, not a Linux restart code. The syscall
boundary and signal delivery own restart policy; ordinary errors keep their
existing meaning. Other object waits retain their own documented paths.

Signalfd read/poll registration subscribes to the accessing thread's shared
signal-configuration source. Mask updates notify that same source. Blocked
signals therefore wake signalfd waits even though they do not interrupt handler
delivery. The file remains independent of its creator: each access resolves the
current thread, and readiness/dequeue retain the thread/process pending mask
checks. The poll loop's register-then-recheck closes arrival races.

Epoll ADD captures an optional weak file poll source before publishing an
interest, including when the file is already ready or IN is not requested.
Signalfd supplies its registering thread's signal source. Rearm and MOD retain
that source; only DEL followed by ADD captures a new one. Expiration of a source
does not redirect registration or synthesize HUP. Readiness still uses the
current caller's pending queues. Files without a captured source retain their
ordinary `register_poll` path.

File construction requires allocator and initialized anonymous-inode filesystem.
`read`/`write` may block through their polling futures unless the file carries
NONBLOCK. The authoritative nonblocking flag is on `VfsFile`. Epoll control uses
a sleepable mutex and may allocate; it must run in task context. Signalfd data
access requires a current user thread; a kernel task gets
`OperationNotPermitted` on read and ERR readiness. Object construction itself
requires no current credentials because the caller supplies them.

Timer accounting requires initialized clock services. REALTIME/BOOTTIME construction
requires task context and initialized normal-work services; construction propagates
service errors. Poll only checks readiness and never programs hardware.
Timer settime and read serialize through a sleepable programming mutex. Neither
may run in IRQ context or while holding a lock needed by a pending callback.
Final TimerFd owner release synchronously drains delivery and must not prevent
the servicing CPU from progressing or run in that timer's callback.
Timer callbacks use IRQ-safe inner state and wake pollers after unlocking it.
Registered poll wakers must obey that hard-IRQ context contract.
Epoll's ready queue also uses an
IRQ-masking lock because source wakers may enqueue an interest directly from
timer IRQ context. Its config and table locks only disable preemption. Do not
infer interrupt-context support for arbitrary watched-file callbacks from the
object API. No API is a general early-boot or reentrant-syscall guarantee;
callers must satisfy provider context contracts and avoid holding locks across
calls that can block.

## State machines and algorithms

Timer state is disarmed (`deadline=None`) or armed with a monotonic/realtime/boottime
deadline. Expiration of a one-shot timer increments the saturated count and
clears the deadline. A periodic timer counts overdue periods and advances its
deadline by that many intervals; representational overflow can clear the next
deadline. Read consumes accumulated expirations. `settime` with zero value clears
the interval/count/deadline. Validation happens before cancellation, preserving
the previous timer when a new deadline is unrepresentable.

Periodic notification is one-shot: after expiration the core node stays idle
until read consumes the count and arms the next deadline. Read, poll, gettime
and replacement account missed intervals arithmetically, anchored to the prior
deadline, without one callback per missed interval. Tiny intervals therefore
cannot keep IRQ dispatch busy merely because the file remains unread.
The callback still wakes waiters when poll has already accounted the expiration.
If realtime moved backwards and there is no unread expiration, the callback
rearms the converted deadline instead. This is not clock-change cancellation.
Business deadlines retain their full clock representation; only core scheduling
is capped at its u64-nanosecond runtime horizon, never truncated to an earlier
business expiration.

Eventfd reads return one and decrement the counter by one in semaphore mode.
Ordinary reads return the entire pre-update counter and reset it to zero.
A successful transfer is one native-endian `u64` in either mode; nonblocking
reads of an empty counter return WouldBlock without copying data. Write rejects
`u64::MAX` and only commits when the new counter is below it.

Epoll `TriggerMode` has Level, Edge and OneShot `{ fired }` states. OneShot
consumption sets `fired=true`; `modify` rearms by replacing configuration on the
same interest identity. Level-ready interests are deferred for the next poll;
Edge/OneShot consumption removes the current ready entry and handles rearming.
The ready scan is bounded by the queue length at entry and deduplicates keys,
so synchronous requeue cannot keep one call running indefinitely. A key contains
both fd number and weak file identity. Generation checks reject stale wakers.

A failed MOD restores the previous configuration, but source registration has
already advanced the waker generation. Enabled interests are therefore queued
for a readiness check and rearm retry, waking epoll waiters without relying on
the invalidated old callback. This is a retry hint, not a fabricated file event:
consumption still checks the file. Persistent registration failure uses the
existing bounded poll-error/retry path. A previously fired OneShot stays disabled.

## Concurrency and resource lifecycle

Eventfd uses acquire loads and release/acquire fetch-update loops for its counter.
Timer settime/read take the programming mutex before the IRQ-safe inner lock.
Settime synchronously cancels before taking the inner lock, so it never waits
for a callback while owning the callback's state lock. Read/start may take a
core queue lock while holding inner state; core dispatch releases queue locks
before invoking the handler, so that direction is not inverted. The callback,
poll and gettime never acquire the programming mutex. Wake calls are outside
the inner lock; settime also drops the programming mutex before waking.

The FD owns an Arc runtime containing the timer and programming mutex.
REALTIME/BOOTTIME objects also own a persistent clock subscription and prepared
normal work. The observer accounts committed clock samples without an I/O waiter;
normal work serializes cancellation and reprogramming. Observer callbacks release
inner state before queueing work and never acquire the programming mutex.
Final FD release marks the runtime closed, unregisters, then drains delivery.
Late callbacks/work cannot rearm a closed runtime. The prepared work captures a
strong runtime reference: observer destruction in IRQ context releases its direct
runtime field before WorkHandle defers capture reclamation to task context.
This prevents final timer destruction in the clock notification IRQ path.
Registration and work storage are allocated at construction, not on clock writes.

Signalfd mask changes release the RwLock before notifying the updater's shared
signal source. Reads consume pending without a file-level broadcast. Signal queues belong to
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
Timer deadlines preserve MONOTONIC, BOOTTIME and absolute REALTIME; relative
REALTIME uses MONOTONIC. The adapter validates signed ABI IDs and passes a `TimerFdClock` enum. Signalfd mask
filtering (including uncatchable signals) belongs to its syscall adapter. This
backend does not promise complete Linux epoll graph/cycle validation, timer
clock-change cancellation. Timerfd
teardown applies to final object ownership, not every descriptor close.

### timerfd clock changes

BOOTTIME uses `BoottimeInstant`; relative REALTIME selects MONOTONIC when armed,
while absolute REALTIME keeps `SystemTime`. Old settings are accounted in their
original domain before replacement. REALTIME/BOOTTIME files hold runtime
subscriptions from creation, independent of read/poll/epoll registration.
Observers immediately account expirations using the committed sample, then normal
work rearms the timer. A forward step past a deadline followed by a rollback does
not erase accumulated expirations. Arming identities reject delayed updates from
before the current arming. `settime` wakes only for actual expiration, without an
extra broadcast for domain changes.

### Typed timerfd clock selection

`TimerFd::new` and `new_file` accept `TimerFdClock` (Realtime, Monotonic or
Boottime). Handler state, current-time selection and absolute-deadline conversion
use this enum with exhaustive matches. The backend no longer defaults unknown
integer IDs to REALTIME. Relative REALTIME still selects a monotonic deadline.
