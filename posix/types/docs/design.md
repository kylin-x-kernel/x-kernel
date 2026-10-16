# posix-types — Design

## Purpose and scope

`posix-types` supplies the POSIX/Linux ABI carriers used by syscall adapters:
user-pointer wrappers with checked copying (`ptr.rs`), scatter-gather iovec
buffers (`io/`), multiplexing bitsets and sized argument wrappers (`io_mpx.rs`),
System V IPC structures (`ipc.rs`), signal ABI carriers (`signal.rs`), time
conversion traits and structures (`time/`), netlink/vsock socket addresses
and message-header trait attachments (`net/`), the robust futex list layout
(`sync.rs`), and `UserRead`/`UserWrite` trait attachments for
`linux_raw_sys` structures (`fs.rs`, `process.rs`,
`system.rs`, `task.rs`). `lib.rs` re-exports the domain modules and the derive
macros, and defines the `Pid`/`Tid` scalar carriers.

The crate owns temporary buffers and carriers only. It does not own processes,
file objects, namespaces, or timers; identity semantics for the scalar carriers
live in the process-domain crates, and syscall adapters remain responsible for
flag, length, identity, and reserved-field validation after copying. User
memory is accessed exclusively through `osvm`'s current-address-space provider.

## Architecture and interfaces

```text
syscall adapter ──UserPtr/UserConstPtr──> osvm checked copy ──> kernel value
        │                                                        │
        │  IoVec::load_from_user / IoVectorBuf::from_iovecs      │ semantic
        │  FdSet::read_from_user / write_to_user                 │ validation
        │  load_string[_with_max_len] / load_bytes_with_max_len  ▼
        └── TimeSpanLike / SystemTimeLike / PosixClockTicks <─ ABI time
```

`UserPtr<T>`/`UserConstPtr<T>` are `repr(transparent)` raw-address wrappers:
construction performs no access, `is_null`/`check_non_null`/`cast` stay pure,
and the `read_vm*`/`write_vm*`/`load_*` helpers route through `osvm` and may
fault or block. `UserRead` promises every copied bit pattern is a valid
initialized `T` (representation safety); `UserWrite` promises a fully
initialized byte representation including padding. Neither implies permission
or semantic validity. Scalars, arrays, raw pointers, `UserPtr` itself, and the
POD `linux_raw_sys` structures listed in the domain modules carry these traits
through audited `unsafe impl`s; derive macros from `macros` are re-exported for
external ABI structs. Linux structures whose `linux_raw_sys` definition has
implicit padding do not carry `UserWrite`; `k_epoll_event`, `k_sysinfo`, and
`k_flock64` replace them with explicit padding fields named `abi_pad*`, and
compile-time assertions pin their size, alignment, and field offsets to the
`linux_raw_sys` definition. They follow their domain module's export path:
`k_epoll_event` is re-exported at the crate root through `io`, while
`k_sysinfo` and `k_flock64` stay under `system` and `fs`.

`IoVec::load_from_user` copies descriptor arrays; `IoVectorBuf::from_iovecs`
enforces the Linux `IOV_MAX`-style bound (at most 1024 descriptors), rejects
negative segment lengths, and checks total-length overflow before any user
I/O. `IoVectorBuf::read_with`/`fill_with` drive per-segment callbacks that must
use checked user-memory access; short positive results advance to the next
segment, zero stops iteration, and side effects of earlier segments are not
rolled back on error. `IoVectorBufIo` is the kernel-side cursor with
`rewind_bytes` for handing back optimistically consumed bytes.

`FdSet` is a fixed `FD_SETSIZE` bitmap; `read_from_user` copies the whole
carrier and then clears bits at or above `nfds` (`nfds >= FD_SETSIZE` leaves it
unchanged), and a null pointer yields `Ok(None)`. `SignalSetWithSize` carries
the `pselect6` mask pointer plus its `sigsetsize`, checked by
`check_sigset_size` (exactly 8 bytes).

Time conversion is trait-based: `TimeSpanLike` (relative) and `SystemTimeLike`
(absolute) convert ABI `timespec`/`timeval` variants after `tv_nsec`/`tv_usec`
range validation, rejecting out-of-range subsecond fields with `InvalidInput`;
`try_into_realtime_deadline` additionally rejects pre-epoch deadlines.
`PosixClockTicks` translates between `TimeSpan` and `USER_HZ` (100) ticks;
`ITimerType` and `Tms` are the `setitimer`/`times()` carriers.

String loading offers three shapes: unbounded `load_string` (NUL-scan through
`osvm`, UTF-8 required), `load_string_with_max_len` (bounded, `InvalidInput`
when no NUL within the bound), and `load_bytes_with_max_len` (bounded, opaque
bytes, `OutOfRange` at the limit). Terminators are excluded; no helper provides
a stable snapshot of concurrently writable user memory.

## Execution context and concurrency

Pure carrier operations (construction, casts, bitset math, time arithmetic,
trait conversions) need no current thread, allocator, mappings, or platform
initialization and can run in early boot or interrupt context. Every helper
that names `vm` or `user` requires the `osvm` current-address-space provider
and an initialized, sleepable thread context; they may fault. Allocating
helpers (`load_vm_vec`, iovec loading, string loading) additionally require the
kernel heap; capacity overflow can panic and allocation failure follows
allocator policy.

There are no locks, atomics, or global mutable state. All values are plain
owned data; sharing and mutation discipline comes from Rust borrows and the
caller. Nothing here is reentrancy-sensitive beyond ordinary stack use.

## Ownership, drop and decisions

`IoVectorBuf` owns the descriptor vector it validated; dropping it drops the
descriptors, not any user data. Loaded vectors and strings are ordinary kernel
heap allocations owned by the caller. The `repr(transparent)` pointer wrappers
deliberately keep `usize`-like layout so they can be carried inside larger ABI
structs and passed by value without padding surprises. Representation safety is
split from semantic validation: the traits make copying sound, while the
adapters keep the judgment calls (fd bounds, identity selectors such as
negative `pid_t` values, permission checks) — this is the central design
decision of the crate, and it is what keeps `osvm` free of ABI knowledge.

## Kernel unit-test layout

Guest test sources live in `ut-tests/` and are mounted at their original
module boundaries with `unittest::path_mod_test!`. They use `#[def_test]`
and run in the configured kernel with `make unittest`, not host Cargo tests.
The migration preserves module visibility, feature gates and execution modes.

- `ut-tests/io_vector_buf.rs`: mounted by `src/io/vector_buf.rs` as `tests`.
- `ut-tests/io_iovec.rs`: mounted by `src/io/iovec.rs` as `tests`.
- `ut-tests/time_abi.rs`: mounted by `src/time/abi.rs` as `tests`.
- `ut-tests/time_itimer.rs`: mounted by `src/time/itimer.rs` as `tests`.
- `ut-tests/time_clock_ticks.rs`: mounted by `src/time/clock_ticks.rs` as `tests`.
