# kio — Design

## Purpose and scope

`kio` supplies synchronous byte-stream traits, memory adapters, buffering,
and transfer helpers for X-Kernel code that cannot use `std::io`.
Consumers implement `Read`, `Write`, `BufRead`, or `Seek` for their streams,
then call the generic adapters. `IoBuf` and `IoBufMut` add remaining-byte
queries without requiring every stream to have a known length.

This document covers the entire crate, including both `alloc` configurations.
The source layout is:

| Source | Responsibility |
| --- | --- |
| `src/lib.rs`, `README.md`, `src/prelude.rs` | Crate overview, root re-exports, `PollState`, trait imports |
| `src/read/mod.rs`, `src/read/impls.rs` | Read contracts, defaults, UTF-8 append guard, line/split iterators, slice and collection implementations |
| `src/write/mod.rs`, `src/write/impls.rs` | Write contracts, exact writes, formatting adapter, memory sinks |
| `src/seek/mod.rs`, `src/seek/impls.rs` | `SeekFrom`, seek defaults, reference/box forwarding |
| `src/iobuf/mod.rs`, `src/iobuf/ext.rs`, `src/iobuf/impls.rs` | Remaining space traits and specialized one-chunk transfers |
| `src/buffered/mod.rs` | Buffered type exports and `IntoInnerError<W>` recovery |
| `src/buffered/bufreader/mod.rs`, `src/buffered/bufreader/buffer.rs` | Reader buffering, seeking, lookahead, initialized storage |
| `src/buffered/bufwriter/mod.rs` | Buffered output, flush progress, panic tracking, `WriterPanicked` |
| `src/buffered/linewriter/mod.rs`, `src/buffered/linewriter/shim.rs` | Newline-aware output layered over `BufWriter<W>` |
| `src/utils/mod.rs`, `src/utils/copy.rs`, `src/utils/cursor.rs` | Utility exports, copying, position-based memory I/O |
| `src/utils/chain.rs`, `src/utils/take.rs` | Sequential and byte-limited readers |
| `src/utils/empty.rs`, `src/utils/repeat.rs`, `src/utils/sink.rs`, `src/utils/iofn.rs` | Synthetic streams and closure adapters |
| `ut-tests/cursor.rs`, `ut-tests/iobuf.rs`, `ut-tests/read_write.rs`, `ut-tests/seek.rs` | Registered kernel unit tests |
| `build.rs`, `Cargo.toml` | Compiler API probes, feature and dependency selection |

There are no file descriptors, filesystem lookup, permissions, user-pointer
validation, device registers, DMA ownership, or asynchronous polling operations
here. Those responsibilities belong to the implementing stream and its caller.
`PollState` only carries readable/writable booleans; it does not register waiters.

## Architecture and interaction

```text
consumer / stream implementer
        | root traits, adapters, default helpers
        v
read / write / seek <--- buffered and utils
        ^                     |
        |                     v
  iobuf extensions ----> concrete Read / Write / Seek / closure
        |
        +-- core BorrowedBuf / BorrowedCursor, slices
        +-- heapless storage or alloc collections
```

Only `prelude` is a public named module. `read`, `write`, `seek`, `iobuf`,
`buffered`, and `utils` are private modules whose public exports are flattened
at the crate root. Their nested implementation modules remain private.
`Read`, `Write`, `Seek`, and `IoBuf` have no supertraits; `BufRead: Read`,
`IoBufExt: Read + IoBuf`, and `IoBufMutExt: Write + IoBufMut` express the
additional requirements. Blanket extension implementations provide the latter
methods automatically.

The crate depends on `kerrno` for the root aliases `Error`, `ErrorKind`, and
`Result`, `memchr` for delimiter searches, `heapless` for fixed storage, and
`unittest` for kernel test registration. Build dependency `autocfg` probes
`BorrowedBuf::init_len` and `MaybeUninit` slice APIs, emitting `borrowedbuf_init`
and `maybe_uninit_slice`; these are compiler cfgs, not Cargo features.
The only crate feature is `alloc`, disabled by default. It adds collection
implementations and whole-stream/string APIs, makes reader storage a boxed slice,
and makes writer storage an allocation-backed vector. Without it, buffers use
`heapless::Vec` with `DEFAULT_BUF_SIZE` (2048 bytes) as the capacity ceiling.

Interfaces are grouped by provider and caller direction:

- Consumers call core traits on concrete streams; adapters call those same
  traits on their inner values. `&mut T` and allocation-backed `Box<T>` forward
  supported operations. Slices, collections, borrowed cursors and `Cursor<T>`
  provide in-memory implementations.
- Consumers call `read_fn`/`write_fn` to own a closure; subsequent stream calls
  invoke that closure synchronously. `WriteFn` supplies a successful no-op flush.
- Consumers call `copy` and `IoBuf` extensions; specialization chooses buffer
  reuse or a stack transfer and calls the supplied reader/writer directly.
- `Split<B>` and `Lines<B>` implement `Iterator`, yielding `Result<Vec<u8>>`
  and `Result<String>` respectively. They allocate per item, remove the delimiter
  or line ending, and propagate errors. `Cursor<T>` supplies conditional `Clone`,
  while buffered errors implement formatting and `IntoInnerError<W>` converts
  to `Error`, consuming its retained writer.

## Execution context and concurrency

Helpers documented to retry interruption use the error's interruption category,
including opted-in file-wait interruption, without changing their accumulated
progress. They do not decide Linux syscall restart or inspect signal handlers.

No production path uses locks, atomics, CPU-local state, current-process lookup,
or the scheduler directly. Mutation is controlled by exclusive Rust borrows;
`Send` and `Sync` are automatic and depend on contained values and closures.
Separate instances can be used independently. Re-entering a single instance
must still respect exclusive access; no internal synchronization is provided.

Memory-only, non-allocating operations do not require a process, initialized
platform, or scheduler. They do require valid mapped Rust storage and enough
stack (generic transfers use 2048-byte scratch space, and fixed buffers are inline).
Heap-backed construction/growth requires a working allocator.
Every generic I/O entry and buffered-writer drop inherits the wrapped stream's
blocking, interrupt, early-boot, and reentrancy constraints. Callers must establish
those constraints before invoking it in an interrupt or while holding a spinlock.
There are no timeouts, cancellation points, or bounded retry counts.

## State and main flows

There is no state enum. State is held by private counters and booleans:

- `Cursor<T>::pos: u64` starts at zero, advances on I/O, and is changed by
  seeking or `set_position`. Slice reads/writes clamp at EOF; vector writes
  reserve and zero-pad gaps. Relative/end seeks use checked signed arithmetic
  and return `Error::InvalidInput` on underflow/overflow.
- `Chain<T, U>::done_first` starts false. A nonempty read with no progress,
  an empty `fill_buf`, or completion of the first whole-stream read switches
  to the second reader permanently. Empty destination reads do not switch.
- `Take<T>` stores `len` and remaining `limit` as `u64`. Construction and
  `set_limit` set both; reads reduce the budget. A zero budget avoids invoking
  the inner reader. Seeking validates the adapter-relative range and moves the
  inner stream using relative seeks, updating the budget after successful steps.
- Reader `Buffer` stores `pos`, `filled`, and (under `borrowedbuf_init`)
  `initialized`. Its visible bytes are `pos..filled`. `fill_buf` refills only
  when exhausted; `consume` advances the visible position. Large reads bypass
  empty buffering. `peek` compacts via `backshift` and calls `read_more` until
  enough bytes or EOF. Ordinary seek accounts for unread bytes and discards
  buffering; `seek_relative` can reuse an in-buffer position.
- `BufWriter<W>` stores initialized pending bytes and `panicked: bool`.
  Small writes append; larger writes flush or bypass storage. Before tracked
  inner writes `panicked` becomes true and after normal return becomes false.
  `flush_buf` retries interruptions and drains only acknowledged prefixes via
  `BufGuard`; a zero write becomes `Error::WriteZero`. `flush` additionally
  invokes the inner writer's flush. `LineWriterShim` finds the last newline,
  emits complete lines, and buffers an eligible suffix using the same writer.

Default exact reads repeatedly call `Read::read` or `Read::read_buf` and retry
interruptions; EOF before completion becomes `Error::UnexpectedEof`.
Default whole-stream reads append to a vector, track filled spare capacity, and
use size hints only as allocation/read-size heuristics. String helpers validate
UTF-8 before exposing appended bytes, restoring the old length for invalid data.
Delimiter defaults call `fill_buf`, search with `memchr`, then append/consume;
unlike exact reads, they propagate interruptions without retry.

`copy` compares source/destination buffer sizes. It can reuse a slice,
`VecDeque`, `BufReader`, `Vec`, or `BufWriter`; otherwise `stack_buffer_copy`
loops with scratch storage. Destination acceptance is counted without a final
flush. `IoBufExt::write_to` and `IoBufMutExt::read_from` instead perform one
transfer. Their specialized advancement and short-write behavior is documented
on the methods; callers must not substitute them for a lossless copy loop.

## Decisions and limitations

Trait-based synchronous I/O separates byte movement from kernel object policy.
Buffer reuse and specialization avoid redundant copies; fixed storage makes
basic buffering available without an allocator, at the cost of a 2048-byte ceiling.
Borrowed-buffer initialization tracking avoids repeatedly zeroing scratch memory,
but makes metadata maintenance an unsafe invariant (see `security.md`).

This is a subset of `std::io`: there are no vectored I/O or `IoSlice` APIs.
`Repeat` rejects unbounded `read_to_end`/`read_to_string` with `Error::NoMemory`;
a `Take` adapter provides a finite alternative. Zero-capacity readers cannot
supply buffered data. Allocation failure is not uniformly fallible: some paths
use `try_reserve`, others use infallible collection growth. No API promises
transactional rollback of stream progress.

`Cursor` remaining-count implementations subtract position without saturation;
positions beyond the inner remaining count can underflow despite being valid
seek positions. `Chain` adds remaining counts without checked overflow.
Callers of these optional queries must keep their counts representable.
`BufReader::seek_relative(i64::MIN)` also overflows the negative-offset
negation when overflow checks are enabled; its rustdoc records that panic.

## Ownership, drop, and recovery

Adapters own their inner values unless constructed around references.
`into_inner` on a reader discards unread buffered bytes; mutable inner access
can desynchronize buffering and seek accounting. `Cursor` and reader adapters
otherwise use ordinary field destruction without implicit seek or I/O.

`BufWriter` drop attempts `flush_buf` unless `panicked` is set, discards errors,
and does not call the inner flush method. This is best-effort cleanup, not
confirmed delivery; the inner write can itself panic. `LineWriter` inherits it.
`into_inner` drains pending bytes and returns the inner writer on success;
on error `IntoInnerError<W>` retains the writer and pending data for recovery.
`into_parts` performs no I/O and suppresses normal drop to move the inner value
exactly once. `WriterPanicked` marks buffered bytes whose delivery is uncertain.
No buffer is securely erased on drop.
