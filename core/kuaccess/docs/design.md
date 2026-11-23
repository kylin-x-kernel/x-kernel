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
- The trap handler takes over a fault only while the current thread's `accessing_user_memory` window is open and its fault-suppression depth is zero. Atomic nofault access raises that depth for the instruction, matching Linux `pagefault_disable()`.

## Algorithms

### User memory access

1. The caller starts an access through an `osvm` pointer wrapper or `vm_load_string*()`.
2. `Vm` first checks that the user address is in range.
3. `copy_user_pages()` can first attempt one architecture copy over the whole range (`read_mem` and `write_mem` pass `whole_first: true`; writability probes do not). When that bulk attempt reports a remaining byte count, it resumes from the fault point and splits the rest on 4 KiB user-page boundaries, calling `user_copy` for each piece.
4. Ordinary copies do not open the in-trap fill window. On a fault, the exception-table fixup returns, and the calling task then calls the current process's `MmSpace::handle_page_fault()`. This avoids waiting for IRQ-driven disk completion while IRQs are masked in exception context.
5. One progress loop tries the current page fragment first. Success advances the offset and resets that page's fault count. Failure invokes the captured live MM's task-context resolver. On admission `Retry`, that resolver retains the current runtime, drops the MM mutex and waits for its resources. Each subsequent attempt performs fresh VMA lookup and permission checks. These waits do not spend the copy's materialization budget.
6. After `Resolved`, the copy may fault again so shared-file materialization and write permission can finish in stages. Completed/COW attempts that still do not enable a copy retain the per-page bound of 16. Explicit no-progress and permanent faults fail immediately. This protects against a backing provider repeatedly claiming progress without turning temporary lock contention into `NoAccess`. A successful return still means the entire output was initialized.

### User atomic access

1. Check 4-byte alignment and the user address range.
2. Faultable loads use the `access_user_memory()` window. Compare-exchange uses the nofault primitive and, after a fault, resolves the write in task context before retrying.
3. Load uses a naturally aligned read-only 32-bit load. The nofault compare-exchange exclusive/atomic sequence runs under `IrqSave`.
4. Success returns the observed value or `(exchanged, observed)`. A fault maps to `MemError::NoAccess`.

These primitives give futex and robust-list paths a TOCTOU-free read or update of a user futex word. A read-only load does not require a writable mapping and does not trigger COW.

`fault_in_user_writeable()` invokes the same task-context resolver without storing a user word. Admission waits release MM serialization, retain the captured active MM user and revalidate the current mapping afterwards. COW conflicts are returned to the caller's operation retry. OOM retains its distinct result; unmapped or denied addresses fail rather than being treated as progress. `FUTEX_WAKE_OP` uses this outside bucket locks before resolving keys again and retrying its nofault CAS.

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


### Locked copies and prefaulting

`Vm::write_mem_nofault` validates the range and first attempts one architecture
copy over the whole range, which may span user pages. If that attempt reports a
remaining byte count, it resumes from there and splits the remainder at
user-page boundaries, accumulating completed fragments and stopping on the
first short copy, so a bulk fixup cannot hide an earlier writable page's
progress.
The returned prefix leaves the destination cursor on the page needing fault
preparation. The copy raises the thread's no-fault depth for its duration, so a
fault returns through the fixup instead of being resolved; the depth is nested
and travels with the thread across a preemption.
It never invokes MM or waits for an inode lock. `Vm::fault_in_writeable` instead
checks VMA permissions and actual writable user PTEs under MM serialization, then
uses the existing task-context fault driver for missing permissions. The driver
waits for admission outside MM and retains its materialization progress bound.
Prefaulting never writes a probe value into the user's buffer and does not pin
the mapping. The caller must release source folio and backing-provider locks
before invoking it, then retry through the nofault copy operation.
