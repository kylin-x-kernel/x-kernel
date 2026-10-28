# kuaccess design

## Purpose

`kuaccess` is the kernel-side glue for accessing user memory:

- connect `osvm`'s generic virtual-memory access entry points to the current thread's address space;
- handle the trap path that lets the kernel resolve page faults while it is accessing a user address;
- provide a few hot user-string load helpers;
- provide user 32-bit atomic load and compare-exchange primitives for paths such as futex.

Callers include syscall implementations, the user-thread runtime, and other crates that use `osvm` pointer wrappers.

## Scope

The current scope is:

- `src/lib.rs`
- `ut-tests/copy_tests.rs` (user-copy regression tests)

## Architecture

```text
syscall / runtime
      |
      v
  kuaccess
   |     \
   |      \-- vm_load_string* -> osvm load helpers
   |      \-- atomic_load/cmpxchg_u32 -> architecture user atomics
   |
   \-- VirtMemIo(Vm) -> user_copy + current thread address space
```

## Calling constraints and execution context

- Must run in a context where a current task exists.
- `access_user_memory()` requires the current task to resolve to a thread.
- User-string loads access user addresses, so they may sleep and resolve page faults.
- Ordinary `osvm` copy entry points require a sleepable thread context and do not disable IRQs themselves. A page fault returns through the exception-table fixup, then the calling task fills the page back in. Filling a file page may wait for block I/O.
- The trap handler for atomic user access depends on the current thread's `accessing_user_memory` flag and takes over page faults only inside that window.

## Algorithms

### User memory access

1. The caller starts an access through an `osvm` pointer wrapper or `vm_load_string*()`.
2. `Vm` first checks that the user address is in range.
3. `copy_user_pages()` splits the copy on 4 KiB user-page boundaries and calls `user_copy` for each piece.
4. Ordinary copies do not open the in-trap fill window. On a fault, the exception-table fixup returns, and the calling task then calls the current process's `MmSpace::handle_page_fault()`. This avoids waiting for IRQ-driven disk completion while IRQs are masked in exception context.
5. One progress loop tries the current page fragment first. Success advances the offset and resets that page's fault count. Failure handles at most one fault, then tries the copy again. `Resolved`, `Retry`, and `CowConflictRetry` all allow the copy to be retried. Unmapped, permission, bus, OOM, no-progress, and generic failure return `NoAccess`.
6. If the copy still fails after `Resolved`, fault handling for the current page continues. That allows a shared-file write to finish in stages: not mapped, then a read-only PTE, then a writable PTE. Each user page may call fault handling at most 16 times. `Resolved` and retry-class outcomes share that budget, and the next page starts again from zero. If the budget is exhausted and the copy still fails, the result is `NoAccess`, so a backend that keeps reporting success or retry without making progress cannot loop forever. The cap stops unbounded retries; it does not measure PTE progress by counting attempts. Continued concurrent changes to the mapping can also exhaust the budget and fail the copy. An already mapped page does not need the address-space lock. A successful return means the whole output has been initialized.

### User atomic access

1. Check 4-byte alignment and the user address range.
2. The faultable form calls the architecture primitive inside the `access_user_memory()` window so MM can resolve a recoverable fault. The nofault form relies on the exception-table fixup to return failure directly.
3. Load uses a naturally aligned read-only 32-bit load. The nofault compare-exchange exclusive/atomic sequence runs under `IrqSave`.
4. Success returns the observed value or `(exchanged, observed)`. A fault maps to `MemError::NoAccess`.

These primitives give futex and robust-list paths a TOCTOU-free read or update of a user futex word. A read-only load does not require a writable mapping and does not trigger COW.

`fault_in_user_writeable()` requests a write fault and does not store a new user word. The caller must be in a sleepable task context and must not hold a non-sleepable lock. `Resolved` means the address can now be written. `Retry` and `CowConflictRetry` keep handling the same write fault, with no attempt cap. Every other result returns `NoAccess`. `FUTEX_WAKE_OP` uses it for a COW page after `fork`: the nofault CAS fails, drops the bucket locks, and returns; the syscall calls this function, resolves both keys again, and retries the CAS. The new value is still stored only by the CAS while those locks are held.

### String loads

1. Read a byte vector or a NUL-terminated byte stream from the user address.
2. Validate UTF-8.
3. Failure returns `IllegalBytes`.

## Concurrency

- `Vm` does not use `IrqSave`. The slow fault path of an ordinary copy, like a disk read, requires a sleepable context. Nofault atomic access keeps its local `IrqSave`.
- There is no global shared state. Concurrency control belongs to thread state and the address-space lock.

## Design decisions

- User-string loads stay in `kuaccess` rather than a syscall crate because they are part of "how to access user memory safely", not of one syscall family.
- Only string-level helpers are kept. This crate does not grow into a general user-argument parsing layer.
- The trap handler's external ABI is still `bool`, but that bool is only the adapter result of architecture trap dispatch. MM semantics come from `PageFaultOutcome`, so `kuaccess` does not define its own fault classification.

## Regression tests

`ut-tests/copy_tests.rs` uses an on-demand mapping backend that requires `PreparedTaskWait`. It checks that ordinary-copy fault handling runs in the sleepable task context outside the exception, and covers cross-page reads, a first-touch write fault, read-only protection, and revoking a mapping. The staged write-fault test spans multiple pages and exceeds the per-page recovery budget, checking that the copy is complete after repeated faults on the same page. The no-progress tests cover a bounded exit when the backend keeps reporting success or a COW retry, and an immediate failure on an explicit `NoProgress`. The tests do not depend on real disk or IRQ timing.
