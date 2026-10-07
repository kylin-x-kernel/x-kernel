# kerrno — Design

## Purpose and scope

`kerrno` supplies the shared kernel error vocabulary and translates it to Linux
errno. All implementation, macros and in-source unit tests are in `src/lib.rs`.
`KErrorKind` models semantic failures, `KError` stores either that domain or a
Linux error, and `KResult<T>` is their common result alias. The crate does not
perform recovery, validation of syscall arguments, permission checks or I/O.

## Architecture and representation

```text
KErrorKind (positive internal code) ──> KError ──> LinuxError
LinuxError (positive errno) ──negate──> KError ──canonicalize──> mapped kernel kind
serialized signed code ──try_from_i32──> KError or original rejected integer
```

`LinuxError` is a re-export of `linux_sysno::Errno`, not another error enum.
`KError` is a transparent `i32` carrier: positive values identify kernel kinds,
and negative values retain Linux errno. Zero is not a valid error. `code()`
returns that tagged internal value; syscall code must convert to `LinuxError`
and then negate its raw value. Negating a positive kernel-kind code directly
would produce the wrong Linux error.

`KErrorKind::from_code` and its `TryFrom<i32>` recognize the internal table.
`KError::try_from_i32` accepts one of those positive codes or the negation of a
named Linux errno. `checked_neg` rejects `i32::MIN`; other unknown values and
zero return the original integer as the error.

Conversion from a kernel kind to Linux errno is total and sometimes many-to-one:
`BadState` and `BadAddress` share `EFAULT`; `InvalidData` and `InvalidInput` share
`EINVAL`; `UnexpectedEof`, `WriteZero`, and `Io` share `EIO`. Reverse conversion
chooses the canonical kind or returns the unmapped Linux errno. `canonicalize`
normalizes mapped values and preserves unmapped ones; it cannot recover lost
semantic distinctions.

## Interfaces and caller obligations

Interruption category checks preserve source identity: `is_interrupted()`
recognizes ordinary interruption, Linux EINTR and opted-in file-wait interruption.
The latter defaults to EINTR; only a caller that has already preserved the
enclosing transfer's progress may promote it to syscall restart. Canonicalization
does not discard kernel-kind distinctions and internal restart codes are not
ordinary interruption-category errors.

Subsystems construct `KError` from semantic kinds; Linux-facing adapters use
`LinuxError` at the ABI edge. `Display` presents descriptions and `Debug`
identifies the stored domain. `k_err_type!(Kind [, message])` constructs an error
and logs a warning; `k_err!(Kind [, message])` returns `Err` with the same behavior.
The hidden `__priv` module exports logging support for macro expansion.

`From<LinuxError>` trusts a positive errno but `LinuxError::new` itself is
unchecked. Unknown positive errno can be preserved. Zero or negative errno can
instead create invalid or misclassified internal carriers; `i32::MIN` negation
can overflow. Use `try_from_i32` when decoding arbitrary internal serialized
codes. The library does not turn `is_valid()` into a stronger validation guarantee.

## Context, concurrency and lifecycle

Value construction, comparisons and checked conversions need no current thread,
CPU-local state, memory mappings, allocator, scheduler or platform initialization.
They do not block or allocate and can run in early boot or IRQ context. Formatting
depends on the supplied sink; warning macros depend on the logger's initialization,
locking, allocation and execution-context contract. Do not assume a logging macro
has the same context freedom as a pure constructor.

There are no production locks, atomics or global mutable errors. The small `Copy`
values carry no owned resources and need no custom drop/cleanup; an error does
not roll back the failed operation. Borrowing and external log-sink synchronization
provide concurrency discipline. `alloc` is imported for code/tests, but error
representation itself is allocation-free.

## Decisions and limitations

A signed carrier preserves unmapped Linux codes while keeping common kernel
kinds independent of Linux numbering. This makes ABI conversion explicit and
compact, at the cost of a sign invariant and lossy semantic mappings. Codes are
internal table identifiers, not a promised stable external persistence format.
There is no configurable recovery state machine or retry engine. Tests behind
`cfg(unittest)` cover tables, conversion failures, canonicalization and formatting;
rustdoc examples demonstrate the public macros and canonicalization.
