# kio — Security and reliability

## Scope, assets, and trust model

This analysis covers the entire `kio` crate: all production modules, both
`alloc` modes, compiler-selected borrowed-buffer branches, and the test/build
sources. No modules are excluded. API contracts are in rustdoc; architecture
and ownership flows are in `design.md`.

Protected assets are initialized byte ranges exposed as Rust slices, the valid
UTF-8 prefix of a `String`, vector allocation bounds and logical lengths,
exclusive access to mutable buffers, and the exactly-once ownership of wrapped
readers/writers. Buffered output also carries delivery state: acknowledging bytes
to a caller must be distinguished from flushing them to the inner sink.
There are no credentials, permission bits, resource handles, or authorization
checks in this crate.

The kernel caller supplies valid Rust references and concrete implementations
of `Read`, `Write`, `BufRead`, `Seek`, or closure adapters. Those implementations
own any OS/device access, permission checks, blocking policy and raw-memory
validation. `kio` owns its local buffer bounds and initialization bookkeeping.
A byte stream's contents are not assumed to be valid UTF-8 or a valid protocol.
The trust boundary here is the trait/callback contract, not the mere existence
of a dependency on another crate.

## External boundaries and input checks

| Boundary | Direction and inputs | Trust and validation responsibility |
| --- | --- | --- |
| `Read::read`, `Read::read_buf`, `BufRead::fill_buf` | Provider supplies bytes/counts into caller or adapter storage | Provider must respect the supplied extent and borrowed-buffer initialization rules; `kio` only exposes initialized filled bytes. A concrete file/network/device origin is not encoded by these signatures. |
| `read_fn` and `write_fn` | `kio` lends slices to an owned `FnMut` and receives `Result<usize>` | Callback follows the read/write count contract; wrappers forward results without validating counts or retrying. `WriteFn::flush` does not invoke a callback. |
| `Write::write` and buffering/copy helpers | Caller bytes flow to a provider, which returns an accepted count | Provider controls delivery and must not over-report; caller decides whether to retry, flush, or recover. No authorization occurs here. |
| Whole-stream/string and line helpers | Provider bytes are appended to a caller string/vector | `str::from_utf8` validates the appended range, or the complete temporary byte buffer, before string exposure. Invalid UTF-8 becomes `Error::IllegalBytes` when no earlier read error takes precedence. |
| `Seek` and `Cursor`/`Take` positioning | Caller supplies byte offsets | `Cursor::seek` rejects checked-add failure with `Error::InvalidInput`; `Take::seek`/`seek_relative` reject positions outside their logical window. Wrapped seek errors are forwarded. |

No public raw-pointer input, user-address dereference, MMIO/PIO, DMA buffer,
FFI declaration, inline assembly, interrupt handler, bootloader/firmware metadata,
or direct filesystem/network/IPC operation exists in this crate. Ordinary Rust
byte buffers may still contain sensitive or adversarial data received upstream;
these statements do not assert that all stream contents are trusted.

Local bounds checks have distinct outcomes. `default_read_buf` initializes its
cursor region and rejects an oversized callback count by panic (an explicit
assert in the compatibility branch, a checked cursor advance otherwise).
`Take::read` limits the supplied slice and asserts that the returned count does
not exceed the remaining budget. `BufReader::peek` asserts that lookahead fits
capacity. Fixed reader construction asserts capacity at most 2048 bytes.
These checks do not produce permission errors or sanitize protocol payloads.

## Unsafe inventory and invariants

There are no public unsafe functions/traits, unsafe trait implementations, FFI,
or assembly boundaries. Internal unsafe sites fall into uninitialized-storage,
fixed-buffer-copy, and raw-ownership scenarios. Existing `BorrowedBuf`,
`BorrowedCursor`, `MaybeUninit`, vectors, and exclusive borrows carry most of
the invariants; no new unsafe abstraction is required for this documentation.
Paths below are relative to this crate and use function names instead of unstable
line numbers. Multiple sites are grouped only where their invariants are shared.

| Source and entity | Unsafe operations | Required invariant and guarding path |
| --- | --- | --- |
| `src/read/mod.rs`: `default_read_buf` compatibility branch | `BorrowedCursor::as_mut`, `advance` | Exclusively borrowed unfilled storage is zero-filled before read access; `n <= capacity` is asserted before committing the initialized prefix. No initialized bytes may be replaced with uninitialized bytes. |
| `src/read/mod.rs`: `default_read_to_end` | `BorrowedBuf::set_init`, `Vec::set_len` | Carried initialization describes the same spare prefix initialized on the prior iteration; the filled cursor count is initialized and within vector spare capacity before length increases. |
| `src/read/mod.rs`: `append_to_string` and its `Guard::drop` | `String::as_mut_vec`, `get_unchecked` on the appended suffix, restoring `Vec::set_len` | Callback must only append initialized bytes, never shorten or modify the original UTF-8 prefix, even on error/unwind. Original length therefore remains in bounds. The guard restores it unless suffix UTF-8 validation allows committing the new length. |
| `src/read/mod.rs`: `default_read_to_string`, `BufRead::read_line`; `src/read/impls.rs`: `Read for VecDeque<u8>::read_to_string`; `src/buffered/bufreader/mod.rs`: `Read for BufReader<R>::read_to_string` | Calls to `append_to_string` | Default vector read and `VecDeque` append only; `read_line` relies on `BufRead::read_until`'s append-only contract; the buffered fast path starts with an empty string so no existing prefix is exposed to its inner reader. The nonempty buffered path uses a separate vector. |
| `src/iobuf/ext.rs`: `read_from_vec_impl!`, expanded for `Vec<u8>` and `BufWriter<I>` | `set_len` after reading spare capacity | The borrowed cursor exclusively covers spare storage; only its initialized filled count is added to the old vector length, including on a returned error. |
| `src/buffered/bufreader/buffer.rs`: `Buffer::with_capacity`, `Buffer::buffer` | Fixed-vector `set_len`; `get_unchecked` and `assume_init_ref` | Constructor checks physical capacity before exposing `MaybeUninit<u8>` elements. A returned byte slice requires `pos <= filled <= capacity` and initialization of every byte below `filled`; its shared borrow prevents mutation. |
| `src/buffered/bufreader/buffer.rs`: `Buffer::read_more`, `Buffer::fill_buf` | `BorrowedBuf::set_init` | `initialized` must represent an actually initialized prefix in the current backing storage. `read_more` uses `initialized - filled` for the spare tail; `fill_buf` reuses the full prefix. Any compaction must preserve this meaning, not just the numeric bounds. |
| `src/buffered/bufwriter/mod.rs`: `BufWriter::write_to_buffer_unchecked` | Pointer addition, `copy_nonoverlapping`, `set_len` | Input length fits spare capacity, source/destination do not overlap, and exclusive ownership keeps storage live and stable. Byte alignment is one; copied bytes are initialized before exposure. |
| Same file: `write_to_buf`, `write_cold`, `write_all_cold`, `Write::write`, `Write::write_all` | Calls to `write_to_buffer_unchecked` | Minimum-with-spare bounds the partial helper; cold paths either already have space or successfully drain the buffer before handling input smaller than capacity; fast paths explicitly compare with spare capacity. Safe input/receiver borrows prevent overlap. |
| Same file: `BufWriter::into_parts` | `ptr::read` of `inner` | `ManuallyDrop` suppresses the old owner's destructor; pending storage is moved out and the inner value is read exactly once, avoiding a double drop. |
| `src/utils/cursor.rs`: `reserve_and_pad`, `vec_write_all_unchecked`, `vec_write_all` | Unchecked spare slicing, pointer copy, vector length extension | Reservation establishes storage through `pos + input length`; the gap is zero-filled. The copy range fits the live exclusive allocation, does not overlap the input, and its end is representable. Length is extended only after gap and payload initialization. |
| `src/utils/take.rs`: `Read for Take<T>::read_buf` | `as_mut`, `set_init`, `advance_unchecked` or compatibility `advance` | The subcursor is bounded by remaining budget and parent capacity, does not deinitialize existing bytes, and inherits only a known initialized prefix. After its borrow ends, filled and newly initialized counts refer to the same parent ranges before advancement. |
| `src/utils/repeat.rs`: `Read for Repeat::read_buf` | `as_mut`, `advance_unchecked` or compatibility `advance` | Entire exclusive unfilled region is initialized with the repeated byte before advancing by its capacity; no uninitialized data is written over initialized bytes. |
| `src/utils/copy.rs`: `BufferedWriterSpec for BufWriter<I>::copy_from` | `BorrowedBuf::set_init`, buffer `set_len` | Carried `init` describes the initialized spare prefix and is adjusted after appends/flushes. `set_len` adds only filled, initialized bytes within existing capacity. |

These are the safety conditions recorded by the implementation, not a proof
that arbitrary third-party trait implementations uphold them. In particular,
`append_to_string` assumes an append-only callback and the default `read_line`
passes an overridable safe trait method. A violating implementation can break
that assumption. The initialized-prefix condition after `Buffer::backshift`
also needs special scrutiny: it copies `MaybeUninit` storage and leaves the
`initialized` counter unchanged. This audit does not certify those paths against
malicious implementations or all partial-initialization sequences.

## Thread safety

There is no shared mutable global state, lock ordering, poisoning, manual
`Send`/`Sync`, or custom memory ordering. Automatic traits follow owned stream,
closure, and buffer types. Mutable operations require `&mut self`; sharing a
stream across threads requires external synchronization appropriate for that
stream and context. This does not make a wrapped device or callback thread-safe.

## Threat analysis

| ID | Threat and asset | Severity | Trigger | Existing response and residual risk |
| --- | --- | --- | --- | --- |
| T-01 | Invalid byte counts corrupt progress or make safe indexing panic | Medium | A trait/closure provider returns a count larger than the supplied slice | `default_read_buf` and `Take::read` check relevant bounds; slice-based paths also bounds-check. Providers remain responsible for contracts; not every boundary has a uniform error conversion. |
| T-02 | Uninitialized bytes become visible through a slice or string | High | Filled/initialized metadata exceeds real initialization, or an append callback modifies old bytes | Borrowed-buffer counts, initialized-prefix bookkeeping, UTF-8 checks and append guard are implemented controls. Correct metadata and append-only providers remain necessary; see the inventory's explicit audit limitations. |
| T-03 | Unbounded input exhausts memory or occupies an execution path | Medium | Endless input to whole-stream/delimiter reads or copy, or perpetual interruptions | `Take` can impose a byte budget; `Repeat` rejects its own unbounded allocation APIs. Selected reservations return `NoMemory`. Caller must impose stream/deadline policy; infallible allocations and retry loops remain. |
| T-04 | Output is lost or duplicated | Medium | Drop ignores a write error, recovery retries already-delivered data, or a one-chunk transfer short-writes | Explicit `flush` exposes errors; `IntoInnerError` retains recovery state; `into_parts` avoids I/O and `WriterPanicked` marks uncertainty. Generic one-chunk transfers can consume more than they write; use `copy`/`write_all` where appropriate, without assuming rollback after errors. |
| T-05 | Sensitive payload remains in memory or reaches diagnostics | Medium | Freed/reused buffers or caller formatting of values/errors | Safe views restrict access to filled data, but no secure erase, encryption, or redaction is implemented. Callers control exposure and must use suitable storage/diagnostic policies. |
| T-06 | Buffered position and remaining counts become inconsistent | Medium | Caller mutates an inner stream directly, seeks beyond a capacity-query range, or adds overflowing remaining counts | Checked seek arithmetic rejects invalid `Cursor`/`Take` offsets; reader stream-position subtraction can panic on inconsistency. Optional remaining-count arithmetic is not universally checked; callers must keep it representable and coordinate inner access. |

## Failure mode and effects analysis

Severity: 1 fatal, 2 serious, 3 moderate, 4 minor.

| ID | Failure mode | Cause | Local effect | System effect | Severity | Response |
| --- | --- | --- | --- | --- | --- | --- |
| F-01 | Exact transfer stops early | EOF or zero-progress writer | `UnexpectedEof` or `WriteZero`; partial progress remains | Caller operation fails | 3 | Propagate error and recover at protocol level, not by assuming rollback |
| F-02 | Allocation fails | Large stream or requested capacity | `NoMemory` for fallible reserves; infallible allocation may terminate/panic | Possible kernel unavailability | 2 | Bound input and use fixed storage when suitable; no universal OOM recovery |
| F-03 | Inner writer panics | Provider failure during output | Delivery becomes uncertain; tracked writes leave `panicked` true | Abort or unwind according to kernel policy | 1 | Tracked `BufWriter` drop avoids another write; `into_parts` permits recovery if execution resumes |
| F-04 | Destructor drain fails | Inner I/O error | Remaining bytes discarded with buffer | Lost output | 3 | Call and check `flush` before drop; retain an `into_inner` error for recovery |
| F-05 | Invalid text | Non-UTF-8 appended bytes | Invalid suffix removed; `IllegalBytes` or existing read error | Parsing operation fails | 3 | Do not interpret raw stream data as text without successful validation |
| F-06 | Capacity or position precondition fails | Oversized fixed buffer/lookahead, inconsistent inner position | Panic | May terminate the kernel | 2 | Validate lookahead/capacity and coordinate inner access |
| F-07 | No completion | Infinite stream or repeated interruptions | Synchronous loop never returns | Execution path unavailable | 2 | Caller-selected budget and provider cancellation/timeout policy |

## Failure management and privacy

Errors use `kerrno::KError` (root `Error`); there is no local error enum or
permission check. Retry behavior is method-specific: exact reads/writes and
buffer draining retry canonicalized `Interrupted`, while default delimiter
methods and closure adapters forward it. `Result` does not guarantee that no
bytes were read, written, or appended on failure. Error recovery does not rewind
streams or provide durability. Formatting can panic when a `fmt` implementation
reports failure without a corresponding I/O error.

Buffers process arbitrary caller/provider payloads, potentially including user
data. This crate does not emit production logs or transmit data independently,
but writing forwards payloads to the supplied sink. Derived debug output for
some adapters includes inner state; callers should not log secret-bearing
adapters. Buffer deallocation and `into_inner` do not erase payloads.

## Known limitations and verification

This is a documentation and contract audit, not a memory-safety proof.
Compiler-probed compatibility branches require separate build coverage.
There is no secure clearing, authorization, rate limiting, universal checked
count arithmetic, or reliable destructor error reporting. Some initialization
and append-only assumptions still depend on local bookkeeping or provider
behavior as described above; they must not be described as validated isolation.

Existing kernel tests in `ut-tests/cursor.rs` cover seek boundaries, gap padding,
fixed-write overflow and UTF-8 reads; `ut-tests/read_write.rs` covers buffered
I/O, `Take`, chaining, copying and synthetic streams; `ut-tests/iobuf.rs` covers
remaining counts; `ut-tests/seek.rs` covers offset handling. Run them through
`make unittest UNITTEST_CRATE=kio` after the platform configuration workflow.
These tests are not exhaustive checks of malicious providers or every unsafe
initialization path. Rustdoc examples exercise bounded memory I/O and explicit
buffer flushing.

## Audit checklist

- Check every `set_len` against both physical capacity and actual initialization.
- Recheck initialized prefixes after compaction, partial reads, and errors.
- Check `append_to_string` callers cannot shrink or overwrite the old prefix,
  including through trait overrides and unwind paths.
- Preserve single ownership when changing `into_parts` or panic recovery.
- Keep read/write count and `consume` bounds explicit in provider contracts.
- Bound untrusted whole-stream reads; do not equate a size hint with a limit.
- Check partial-transfer advancement before replacing copy loops with extensions.
- Flush explicitly where output errors matter; do not claim durability from drop.
- Validate wrapped-stream context before IRQ/early-boot use or drop under a lock.
- Do not log or reuse sensitive buffers under an assumption of automatic erasure.
