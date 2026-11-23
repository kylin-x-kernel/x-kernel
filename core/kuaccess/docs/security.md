# kuaccess security and reliability

## Trust model

- User addresses, lengths, and string contents are untrusted.
- `kuaccess` constrains those inputs to the current process's user address space and returns failures explicitly to the caller.

## External boundaries and attack surface

- Every `vm_load_string*()` call site.
- Every read or write that reaches `Vm` through `osvm::VirtMemIo`.
- Page-fault handling raised inside the access window.
- `fault_in_user_writeable(addr)`, a public entry that takes a user-space address and runs explicit page-fault handling outside the access window, without going through `Vm` or `osvm::VirtMemIo`.

## Unsafe inventory

- `user_copy(...)`:
  Depends on the user-address range check before the call, per-page copy bounds, and the exception-table fixup. An ordinary copy does not fill the page inside the exception. It handles the fault after returning to task context and retries the current page.
- `user_atomic_load_u32(...)`:
  Depends on 4-byte alignment and the user-address range check. It only reads the user word and reports a fault through the exception-table fixup.
- `user_atomic_cmpxchg_u32(...)`:
  Depends on the 4-byte alignment check, the user-address range check, and the exception-table fixup. The faultable operation resolves faults in task context before retrying the nofault primitive.

## Memory-safety invariants

- `check_access()` allows only the valid user-space interval.
- `atomic_cmpxchg_u32()`, `atomic_load_u32()`, `atomic_u32_eq()`, and `fault_in_user_writeable()` also require the address to be 4-byte aligned.
- `fault_in_user_writeable()` uses the live MM's task-context resolver and does not modify the user word. Admission waits run outside the MM mutex and repeat mapping checks. The caller must be allowed to sleep and must not hold a non-sleepable lock. `Resolved` and retry-class outcomes return success; the caller repeats its store after a retry-class outcome. `OutOfMemory` returns `ENOMEM`. Permission denial, an unmapped address, and every other failure return `NoAccess` immediately. This path must not bypass a read-only mapping.
- `handle_page_fault()` handles a fault only while the current thread has explicitly entered the user-memory access window and holds no fault-suppression depth.
- An ordinary copy must not wait for block-device completion on the exception path with IRQs disabled. The caller must be allowed to sleep and must not hold a non-sleepable lock. The requirement is not met by relaxing block-driver context checks.
- `kuaccess` must preserve the typed outcome classification of `MmSpace::handle_page_fault()`. Only `Resolved` and retry-class outcomes may become a handled trap. Unmapped, permission denied, bus error, OOM, no-progress, and generic failure must become a user-copy or atomic-user failure, not another retry.
- An ordinary copy may fault the same page again after `Resolved`, so staged mapping and write-permission preparation can finish. Completed/COW attempts for one page share a budget of 16. Admission retries wait on the backing provider's existing locks outside the MM mutex and do not consume this budget. An explicit `NoProgress` fails immediately. If the budget is exhausted and the copy still cannot finish, the result is `NoAccess`. A partially initialized buffer must not be reported as a complete success. Reads and writes use `READ` and `WRITE` fault permissions respectively and must not bypass a read-only mapping.
- String helpers interpret bytes as UTF-8 only after the complete byte stream has been read successfully.

## Thread safety

- No mutable state is shared across threads.
- The thread-local access-window flag is maintained by the current thread object and must not be reused across threads.

## Threat analysis

- Out-of-range address: rejected by `check_access()`.
- Fault or unmapped page: handed to the address space's typed fault handling. If it cannot be resolved as resolved or retry, the failure propagates upward.
- Non-UTF-8 input: mapped to `IllegalBytes`, so dirty data is not processed further as a path or argument.

## Failure modes and effects (FMEA)

- The current task is not a thread: `access_user_memory()` panics immediately, exposing a caller context error.
- The user address becomes invalid during the access: returns `MemError::NoAccess` or the equivalent `KError`.
- A page fault is taken over by the wrong thread: prevented by the access-window flag.
- A typed fault outcome is incorrectly collapsed to true: a user copy could retry forever on an unrecoverable fault. `fault_outcome_to_trap_result()` is the single mapping from an outcome to the trap bool.

## Failure handling

- Address and mapping problems return through `Result`.
- The only panic is an internal-invariant failure: the thread-context assumption was broken.

## Known limitations

- The helpers cover byte copies, string loads and `u32` atomic load/compare-exchange. Callers assemble parsing of more complex structures themselves.
- Atomic primitives currently provide only 32-bit load and compare-exchange. Wider operations or other read-modify-write operations are added when needed.


## Page-cache destination boundary

The nofault write provider validates the entire user range before entering the
architecture exception-table copy. It first attempts one architecture copy over
the whole range, which may span user pages. If that attempt reports a remaining
byte count, the provider resumes from the fault point and falls back to
per-page copies: each retry stays within one user page and completed fragments
are accumulated before any short fragment is returned. This prevents a bulk
fixup from discarding progress before a missing next page and repeatedly
preparing an already writable page. It creates no user references
and exposes only the successfully copied prefix. Raising the thread's no-fault
depth prevents nested callers from accidentally resolving a fault under a source
folio lock.
Prefaulting is separate, checks both VMA policy and PTE permissions, and must run
outside backing-provider locks. A successful preparation grants no lasting
accessibility guarantee; later copies remain exception protected.
