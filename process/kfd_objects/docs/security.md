# kfd_objects — Security and reliability

## Scope, trust and assets

The entire crate is covered, including unit-test unsafe sites. Assets include
counter/timer readiness, signal payloads, watched-file identity, poll registrations
and file-opening credentials. Syscall adapters supply resolved files, flags,
masks, clock IDs, descriptor numbers and user data; those values can originate
in userspace. `kvfs` supplies valid kernel byte slices to file operations and owns
file flags/credentials; `ksignal` owns pending signals and `ktask::future` owns
timer/blocking runtime. This crate does not dereference raw user pointers or
perform descriptor-table permission checks.

## External boundaries and validation

`new_file` passes explicit `Arc<Cred>` into `AnonInodeFs::get_file`. Eventfd,
timerfd and signalfd reject bits unknown to `OpenFlags`; detailed syscall flag
restrictions remain adapter duties. `from_file` checks typed private data and
returns `BadFileDescriptor` on mismatch. Eventfd/timerfd reads require at least
eight bytes; signalfd requires 128. Eventfd write rejects `u64::MAX` and a short
buffer; a full counter/empty read becomes `WouldBlock` through the polling helper.
Timer `settime` rejects unrepresentable deadlines with `InvalidInput` before
mutating the old setting. Signalfd access checks the current task for a `Thread`
and returns `OperationNotPermitted` if absent.

Epoll control receives a resolved file and fd; duplicate ADD returns
`AlreadyExists`, absent MOD/DEL returns `NotFound`, and source registration
failures are mapped to `KError`. Backend APIs do not establish user authorization
or independently validate all epoll nesting rules. The syscall owner must supply
those controls where required.

## Unsafe inventory

| Source and function | Operation / invariant | Enforcing context |
|---|---|---|
| `src/signalfd.rs`, `SignalfdSiginfo::from_signal_info` | Reads both integer/pointer views of a C `sigval_t` union sharing storage. | `SignalInfo::sigval` supplies the value; adjacent SAFETY states both views may be read regardless of the written member. No pointer is dereferenced. The provider must supply initialized union storage. |
| `src/epoll.rs`, test helper `assert_epoll_event` | Reads packed `epoll_event.events` and `.data` with `read_unaligned`. | Test supplies an initialized event; the SAFETY comments explicitly account for Linux ABI packed fields. |
| `src/eventfd.rs`, `test_eventfd_register` | Constructs a `Waker` from `RawWaker`. | Test-only no-op callbacks never dereference the null data pointer, as the adjacent SAFETY comment requires. |

There is no explicit FFI declaration or inline assembly. Signal output has a
compile-time 128-byte size assertion and initializes all output fields/padding;
serialization uses `zerocopy::IntoBytes`. Unsafe descriptions above do not replace
provider contracts or prove that all upstream union construction is correct.

## Thread safety and lifetime invariants

Counter updates must use one atomic transition. Timer expiration/deadline/count
updates share the inner lock; handle cancellation uses its own lock. Signalfd
mask mutation uses its RwLock while signal access is through the current signal
manager. Epoll table, ready queue, queue bit, waker generation and registration
ownership must retain their coordinated protocol; MOD modifies existing interest
identity and serializes rollback with the control lock. The ready queue uses an
IRQ-masking spin lock because timer callbacks can run its source waker directly
in IRQ context; that callback must not acquire task-context-only config locks.

Registered timer wakers hold strong object references, so the final `Drop` and
its cancellation may occur later than file close. Weak epoll target references
must be upgraded before access. Dropping poll-registration owners outside their
mutex avoids unregister callbacks nesting under that lock.

## Threat analysis

| ID | Threat / asset | Severity | Trigger | Existing response and residual risk |
|---|---|---|---|---|
| T-01 | Counter overflow or lost update corrupts readiness | Medium | Concurrent writes at the counter limit | Atomic conditional updates reject overflow and share readiness limits; constructor accepts raw `u64`, and semaphore reads currently expose pre-update count. |
| T-02 | Invalid timer programming destroys prior state | Medium | Deadline cannot be represented | Validate before cancellation; return InvalidInput. Multi-step concurrent reprogramming still requires scrutiny beyond field locks. |
| T-03 | Timer callback retains resources after close | Medium | A registered waker outlives the file reference | Waker Arc prevents use-after-free and final Drop cancels a remaining handle; prompt reclamation is not guaranteed. |
| T-04 | Signal record leaks uninitialized bytes | High | Kernel serializes a pending signal | Explicit fields and zero padding, size assertion and IntoBytes; union storage validity depends on SignalInfo construction. |
| T-05 | Stale epoll wake/config causes duplicate or wrong events | Medium | MOD/DEL or source wake races consumption | Stable interest identity, generation checks, queue bit and bounded/deduplicated consumption; preserve registration rollback protocol. |
| T-06 | Wrong file type reaches backend state | Medium | Adapter passes unrelated file | Typed private-data lookup returns BadFileDescriptor; fd authorization remains outside this crate. |
| T-07 | Kernel task accesses signalfd signal state | Medium | Read/poll without a user Thread | Read returns OperationNotPermitted; poll reports ERR; constructors require explicit credentials. |
| T-08 | Timer IRQ recursively acquires an epoll lock held by the interrupted task | High | A source waker runs while task context owns the ready queue or config lock | Ready queue uses `SpinNoIrq`; IRQ-reachable wake and trace paths avoid the config lock. Keep IRQ-masked critical sections short. |

## Failure modes and management

| ID | Failure mode | Cause | Local effect | System effect | Severity (1–4) | Controls |
|---|---|---|---|---|---|---|
| F-01 | Anonymous file allocation fails | Anonymous-inode provider failure | No file returned | Descriptor creation fails | 3 | Propagate provider errors; references release normally. |
| F-02 | Poll registration fails | Watched source rejects registration | Interest cannot be armed | Readiness wait may need retry | 3 | ADD rollback, MOD configuration restoration; poll preserves already-emitted partial results and requeues for retry. |
| F-03 | Timer interval advance overflows | Deadline exceeds clock representation | Next deadline can disappear | Periodic notification stops | 3 | Saturating expiration accounting and checked deadline addition; document finite representational range. |
| F-04 | Short I/O buffer | Caller supplies insufficient bytes | No record transfer | Caller must supply valid size | 4 | InvalidInput before copying/consuming. |

## Privacy, limitations and verification

Signals carry PID/UID, exit status, timer values and CPU time. Epoll user data is
returned verbatim; it is not a kernel pointer to dereference. Trace logs may
expose watched fd/event state. No local log redaction is provided. Constructor
credentials belong to the opened VfsFile; this crate does not maintain a second
credential snapshot or independently enforce syscall access policies.

Existing tests cover eventfd readiness/accounting, timer validation, signalfd
layout/payload and epoll registration/trigger races. Their presence is not a
claim they were executed by this documentation edit.

## Audit checklist

- Preserve explicit constructor credentials and typed private-data checks.
- Preserve exact buffer-size checks and initialized signal output padding.
- Check timer callback retention as well as final Drop cancellation.
- Keep epoll MOD identity, queue deduplication and generation checks together.
- Drop registration owners outside their mutex; verify callback lock context.
- Keep IRQ-reachable epoll wake paths on IRQ-safe state and avoid task-only locks.
- Keep limitations aligned with implementation rather than assumed Linux behavior.
