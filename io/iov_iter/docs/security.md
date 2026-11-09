# iov_iter security and reliability

## Scope and trust model

This analysis covers the entire crate, implemented in `src/lib.rs`, with no
excluded modules. The crate handles byte buffers that may contain file or user
data. It provides cursor and transfer-budget accounting, not access authorization.

Safe callers supply valid borrowed kernel slices or exclusively borrowed
`IovSource` / `IovSink` implementations. Those interfaces own the access policy
for other memory. In particular, the wrapper does not validate user addresses,
parse iovec descriptors, or enforce file permissions. Concrete adapters and their
callers must enforce those policies.

## Assets and boundaries

The source bytes, writable destination bytes, slice offsets, and remaining
transfer budget are the relevant data and accounting state. Rust slice bounds
and borrowing constrain local memory access. The source interface moves bytes
from an adapter into a caller-provided slice; the sink interface moves bytes
from a caller-provided slice into an adapter. Implementations are trusted kernel
components for transfer counts, cursor changes, and memory-access correctness;
the byte contents themselves are not interpreted or trusted as control data.

The public constructors and copy methods accept slices and trait references,
not raw user pointers. File read/write roles are explicit in the trait contracts;
the ultimate source of arbitrary slice contents cannot be inferred locally.
There are no direct MMIO, PIO, DMA, device interrupt, network-protocol, firmware,
or boot-metadata interfaces. An adapter may mediate such storage, but its
validation and execution-context requirements remain external obligations.

## Unsafe inventory and memory invariants

There are no `unsafe` blocks, unsafe functions or implementations, FFI declarations,
or assembly in this crate. No local `SAFETY` annotations need reconciliation.
This statement does not extend to external adapter implementations.

For a kernel slice, constructors establish offset zero and a budget equal to
length. Copies cap their ranges by the budget and bytes remaining in the slice;
rewinds reject movement before offset zero and check budget addition for overflow.
`truncate` only reduces the budget. These operations preserve in-bounds slice
access. Private fields and public constructors preserve the agreement between
the direction marker and `IovIterInner` variant.

Adapter reports are not range-checked after copying. Saturating subtraction
protects the wrapper's count from underflow, but cannot establish that the adapter
copied the reported bytes. Because the traits are safe, their implementations
must preserve Rust memory safety independently; their accounting contract is
also needed for correct I/O results.

## Thread safety and execution context

There are no locks, atomics, or shared global state. Mutation uses exclusive
references. The stored trait objects do not require `Send` or `Sync`; the iterator
type therefore does not offer those guarantees even for its slice-backed variant.
Slice operations introduce no explicit sleep or scheduling point. Adapter calls
inherit external blocking, fault, reentrancy, and context requirements. Callers
must not assume that a generic iterator is interrupt-safe merely because the
wrapper itself contains no blocking primitive.

## Threat analysis

| ID | Threat and impact | Severity | Trigger | Response and residual risk |
| --- | --- | --- | --- | --- |
| T-01 | Out-of-range cursor movement could target bytes outside a kernel slice. | High | A caller requests an oversized copy or rewind, or overflowing budget addition. | Copy lengths are bounded; `revert` uses `checked_add` and rejects `count > offset` with `KError::InvalidInput`. Review these branches and the private-constructor invariant when changing cursor logic. |
| T-02 | Incorrect adapter reports corrupt visible I/O accounting. | Medium | An adapter reports more bytes than requested or changes its cursor inconsistently. | The supplied copy slice is bounded and count subtraction saturates. Reports are otherwise trusted; adapter correctness is an external dependency. The wrapper does not reject dishonest reports. |
| T-03 | Partial effects can make retry duplicate or overwrite data. | Medium | An adapter modifies a buffer or cursor and then returns an error. | The error propagates and wrapper count remains unchanged, but no rollback occurs. Callers must use the adapter's failure contract; unconditional retry is not guaranteed safe. |
| T-04 | Adapter execution can block or panic in an unsuitable context. | Medium | A caller uses a faulting or blocking adapter from an interrupt or other restricted context. | Context suitability is delegated to the adapter contract and caller. The wrapper does not check execution context or catch panics. |
| T-05 | Copied bytes remain observable after rewind or drop. | Medium | A caller treats rewind or destruction as erasure of sensitive contents. | Rustdoc explicitly describes cursor-only rewind and borrowed ownership. Storage owners must implement any required erasure; this crate does not clear data or provide confidentiality policy. |

## Failure modes and effects (FMEA)

Severity: 1 = fatal, 2 = serious, 3 = moderate, 4 = minor.

| ID | Failure mode | Cause | Local effect | System effect | Severity | Handling |
| --- | --- | --- | --- | --- | --- | --- |
| F-01 | Rewind rejected | Overflow, movement before slice start, or adapter rejection | Wrapper count remains unchanged | Caller must handle failure to restore progress | 3 | Propagate `KError::InvalidInput` or the adapter error; do not assume rollback of adapter state. |
| F-02 | Short or zero transfer | Empty budget/input or adapter short copy | Fewer bytes transferred | I/O caller must decide whether to stop or continue | 3 | Return the count; the wrapper does not retry. |
| F-03 | Adapter access error | Implementation-specific failure | Possible partial buffer/cursor effects | I/O failure or inconsistent retry if ignored | 2 | Propagate the error and retain implementation-specific recovery responsibility. |
| F-04 | Panic during adapter call | Adapter implementation panics | Call does not return normally | Depends on kernel panic policy | 2 | No catch or recovery in this crate; use a suitable adapter. |

## Failure handling and privacy

`KError::InvalidInput` is the only error constructed locally. Other errors are
forwarded through `KResult`; no retry, fallback, poisoning, or recovery service is
implemented. Direction mismatches hit `unreachable!`, but cannot be constructed
through the current safe public constructors. This is an internal invariant,
not a caller permission check.

The crate does not log or persist payloads. Copying intentionally transfers data
to the supplied destination, and dropping an iterator does not erase either
buffer. Data classification, authorization, and disposal belong to callers and
storage owners.

## Audit and verification checklist

- Recheck both direction constructors and private fields when changing variants.
- Check bounded ranges and offset updates in both slice copy branches.
- Check overflow and before-start rejection in both `revert` implementations.
- Keep adapter trust and error-side-effect descriptions consistent with delegation.
- Preserve the distinction between cursor rewind and data rollback or erasure.
- Run the crate-level rustdoc example for bounded copy, rewind, and sink output.
- Generate platform rustdoc with broken-link and syntax lints enabled; these
  checks validate documentation, not arbitrary adapter behavior.

The example does not establish fault recovery, concurrency, or adapter correctness.
Those properties require validation of each external implementation.


## Copy and fault separation

Locked page-cache reads use the mandatory sink nofault operation. Implementers
must neither invoke backing-provider fault handling nor lose progress: successful
short copies advance by exactly their prefix, while errors leave the cursor
unchanged and report no transferred bytes. Fault preparation is a separate,
sleepable callback with no source slice borrowed. Callers reacquire and validate
source residency and EOF after that callback; prefaulting never authorizes an
unchecked later user access.
