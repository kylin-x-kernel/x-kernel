# kresources security and reliability

## Scope, trust model, and external boundaries

This analysis covers all of `src/lib.rs`. Assets are attached file handles,
open-file references, and soft/hard resource-limit pairs. Trusted kernel callers
supply already resolved `Arc<VfsFile>` objects and integer descriptors; POSIX
adapters own user-pointer copying, process-access authorization, and ABI parsing.
No user pointer, MMIO, DMA, firmware, assembly, or FFI is accessed here. File
close crosses into `kvfs` and may execute filesystem callbacks.

## Unsafe inventory and memory invariants

There is no local unsafe code. `Arc` keeps snapshots and shared tables alive;
`Option` models irreversible detachment. `rlimit` checks the index before array
access. `update_rlimit_and_get_old` checks soft <= hard and the currently
stored hard cap before changing a pair. Public `rlimits` access is a trusted
escape from those checks and must preserve them.

## Thread safety

Owner, descriptor table, and limits use `ksync::RwLock`. Their nesting order is
owner -> table -> limits on insertion. Close callbacks run after releasing these
locks. External callbacks must not be invoked while manually holding a table
lock if they can acquire that table again. Detached snapshots are valid objects,
but no longer prove membership in the process's table.

For `Some` updates, limit validation, old-value capture and replacement use one
limits write guard. Concurrent updating callers therefore receive the pair
actually replaced, not a snapshot taken before another caller's update.
`None` delegates to the checked `rlimit` read snapshot without mutation or an
exclusive guard. Neither branch acquires another lock or copies user memory;
both release their guard before returning, and callers must preserve that
boundary. Failure returns no old value and leaves the stored pair intact.

## Threat analysis

| ID | Threat and asset | Severity | Trigger | Response and residual risk |
|---|---|---|---|---|
| T-01 | Unauthorized hard-limit increase | High | A caller supplies a larger maximum | `set_rlimit` returns `OperationNotPermitted`; direct public lock access remains trusted and capability-based increases are unsupported. |
| T-02 | Descriptor exhaustion | Medium | Repeated file insertion | `add_file` and `duplicate_file_from` pass the soft limit to the same lowest-free slot allocator in `FdTable`. Both bound the new FD number by that limit and capacity. Fixed-slot duplication only checks capacity, so syscall-level policy remains a caller obligation. |
| T-03 | Access after process files teardown | Medium | A new accessor races `exit_files` | Owner locking and `NoSuchProcess` reject detached access; temporary table references preserve storage but do not delay descriptor cleanup; snapshots of individual files remain usable. |
| T-04 | Reentrant filesystem deadlock | Medium | A close callback reacquires process resources | Removal precedes close and releases owner/table locks; callers of raw table APIs must preserve this ordering. |
| T-05 | Stale old limit returned after concurrent replacement | Medium | A caller composes separate snapshot and setter operations | `update_rlimit_and_get_old` performs validation, snapshot and replacement under one write guard; standalone reads followed by setters remain separate operations. |

## Failure modes and effects (FMEA)

| ID | Failure mode | Cause | Local effect | System effect | Severity (1-4) | Handling |
|---|---|---|---|---|---|---|
| F-01 | Invalid limit update | Out-of-range index or soft > hard | `InvalidInput`, unchanged pair | Syscall fails | 3 | Validate before mutation. |
| F-02 | Missing file | Invalid descriptor or detached owner | `BadFileDescriptor` or `NoSuchProcess` | Operation fails | 3 | Propagate `kfd` or owner error. |
| F-03 | Flush failure | Filesystem close returns an error | Slot already removed | Data writeback may fail | 2 | Single close propagates; batch close continues without reporting individual failures. |
| F-04 | Allocation exhaustion | Table clone or object allocation | Construction cannot complete | Allocator failure policy applies | 2 | No recoverable allocation API is provided here. |

## Failure handling

Typed-private-data lookup returns `InvalidInput` for absent/wrong-type data.
`unshare_fd_table` contains an `expect` after checking `Some` under the same write
lock; failure means the owner invariant has been broken. No recovery or retry is
implemented for that invariant violation. Cleanup never reattaches an exited
owner and is idempotent at the owner level.

## Privacy and limitations

This crate does not copy file contents or log them. Strong references can retain
open-file credentials and paths beyond process detachment. Resource limits are
not all enforced here; hard-limit increases lack a capability path. Batch flush
errors are intentionally discarded. Existing external references delay final
file release.

## Audit checklist

- Preserve index and soft/hard validation on checked limit access.
- Keep current-hard validation, old snapshot and replacement in one transaction.
- Keep user copying and task coordination outside the limits guard.
- Keep table -> limits ordering consistent with insertion.
- Detach slots before executing close callbacks.
- Reject attempts to replace a detached owner.
- Distinguish retained snapshots from current descriptor membership.

Descriptor close retains the selected table until POSIX-lock cleanup completes.
This prevents owner-address reuse after concurrent replacement. A blocking
record-lock caller must revalidate the binding and fd under `with_fd_table`;
a retained table reference alone does not prove current ownership.

Binding references use `Arc<FdTableOwner>`; temporary table references cannot
force unshare or postpone final descriptor cleanup. Only a live binding may be
shared or used to insert descriptors. Temporary handles provide only read-only
access, making repopulation of a retired table unavailable through safe APIs. Teardown removes slots under the table write lock
and closes them after unlocking. Genuine remaining bindings retain their locks.
