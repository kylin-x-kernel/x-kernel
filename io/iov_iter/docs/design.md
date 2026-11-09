# iov_iter design

## Purpose and scope

`iov_iter` provides direction-specific, borrowed I/O cursors and a remaining
transfer budget for file reads and writes. The complete implementation is in
`src/lib.rs`; `Cargo.toml` declares the sole direct dependency, `kerrno`, using
workspace versions. There are no crate-specific features or public submodules.

The crate neither implements user-pointer validation nor parses iovec arrays.
It delegates memory-access policy to `IovSource` and `IovSink` implementations.
It does not own file positions, perform filesystem authorization, allocate
buffers, or provide synchronization.

## Architecture and interactions

```text
POSIX I/O glue -- constructs adapters --> iov_iter_source / iov_iter_dest
                                              |
kernel slices --> iov_iter_kvec_source / iov_iter_kvec_dest
                                              |
                                   IovIterSource / IovIterDest
                                              |
                              VFS read_iter / write_iter consumers
                                              |
                           slice copy or adapter copy / revert
```

`IovIter<'a, Direction>` stores `inner`, a remaining byte `count`, and a
`PhantomData<Direction>` marker. `IovIterInner` contains either `Source` or
`Dest`; each direction contains a `Kvec` slice and `offset`, or a borrowed
`Reader` / `Writer` trait object. Public aliases select the direction marker.
Constructors keep the marker and inner variant consistent, allowing separate
source and destination method sets. These variants select storage and direction;
they do not represent a runtime state-transition protocol.

File-I/O consumers supply kernel slices directly or accept constructed iterators.
The VFS uses the source alias for writes and the destination alias for reads.
External implementations provide the source/sink traits; the wrapper invokes
those implementations. `IoSourceAdapter` in `posix/fs` adapts source access and
rewind operations. `osvm::VmBytesMut` and `posix_types::IoVectorBufIo` implement
`IovSink` directly, keeping user pointers, segment cursors and fault policy in
their existing owners. These are integration relationships, not direct Cargo
dependencies of this crate.

## Construction and transfer flow

The slice constructors borrow existing storage, initialize the offset to zero,
and use the slice length as the budget. Adapter constructors borrow the adapter
exclusively and snapshot its current remaining count without resetting its cursor.

A copy entry first limits the requested length by the wrapper budget. Zero work
returns immediately. The slice branch copies the bounded range and advances its
offset. The adapter branch receives the bounded slice and returns its own result.
On success, the wrapper subtracts the reported byte count with saturation. On
error, it returns immediately without changing its count. Adapter side effects
are not rolled back. Individual API error contracts and examples are in rustdoc.

`truncate` reduces only the wrapper budget. `revert` checks budget addition for
overflow, then rewinds the slice offset or calls the adapter. The count increases
only after success. Slice rewinds cannot cross the start of the original slice.
A rewind changes future access, not bytes already copied. Truncation does not
establish an immutable cap on subsequent rewinds.

## Execution context and concurrency

Slice-backed operations use ordinary borrowed memory, with no allocation, locks,
scheduler calls, CPU-local access, or explicit blocking. They require valid Rust
slice references, including their normal mapping and lifetime guarantees. There
is no intrinsic current-process or platform-initialization requirement. Early
boot and interrupt use are possible only when the caller can supply such memory.

Adapter constructors call `count`; copies and rewinds invoke adapter methods.
Their context, blocking, fault-handling, and initialization requirements are
inherited from the implementation. This crate cannot promise that arbitrary
adapters are suitable for interrupt context or early boot.

Each mutation requires `&mut self`; mutable slices and adapters are exclusively
borrowed. There are no internal locks or shared global mutable state. Independent
iterators may operate independently. Shared-memory slices may support concurrent
readers, subject to Rust's borrowing rules. Adapter trait objects have no
`Send` or `Sync` bound, so the public iterator type does not promise cross-thread
transfer or sharing, even for a value currently backed by a kernel slice.

## Design decisions and limitations

Direction markers prevent source and destination operations from being mixed by
safe callers. Borrowing retains the backing owner's lifecycle without allocation.
Trait objects allow different access policies behind the same VFS-facing type
without introducing dependencies on user-memory implementations.

Adapters are trusted to report actual transferred lengths and maintain their
cursors. Saturating budget subtraction prevents arithmetic underflow but does
not validate reports or repair inconsistent adapter state. Default trait rewinds
support only zero bytes. The wrapper provides neither transactional copies nor
automatic retries after partial effects or errors.

## Drop and resource lifecycle

There is no custom `Drop` implementation or acquisition/release pair. Dropping
an iterator ends its borrow; it neither frees the backing storage nor drops the
borrowed adapter. It does not restore the cursor, clear copied data, or perform
file cleanup. Owners retain responsibility for allocation, data disposal, and
any resources managed by their adapter.


## Locked copies and destination faults

`copy_to_iter_nofault` advances only by the transferred prefix and never invokes
user fault resolution. The separate `fault_in_writeable` callback may sleep and
must run outside folio and backing-provider locks. It prepares the next user
prefix without advancing the cursor; kernel-memory sinks return false because
there is no fault to resolve. A true result permits retry after releasing and
reacquiring the source. It does not pin user mappings. No iterator mode flag or
second cursor is needed; ordinary `copy_to_iter` remains available to consumers
that can tolerate fault resolution.
