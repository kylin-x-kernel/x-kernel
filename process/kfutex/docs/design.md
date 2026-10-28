# kfutex design

## Purpose

`kfutex` owns the concurrency semantics of non-PI futexes: the canonical
key, the global fixed bucket table, the waiter state machine, and the
linearization rules of wait/wake/requeue/wake-op. Syscall ABI decoding,
timeout-flag parsing, count clamping, and robust-list walking stay in
`ksyscall` (`core/ksyscall/src/sync/futex.rs`) and `posix/process`; the
latter drives robust owner-death notification through this crate's key
resolution and wake primitives.

PI futexes are out of scope for the current stage; until the scheduler
provides real priorities and a donation graph, PI state must not be mixed
into the non-PI buckets (PI commands return `ENOSYS` at the syscall layer).

## Scope

```text
process/kfutex/src/
├── key.rs       canonical private/shared key
├── table.rs     static buckets, compound operations, GlobalFutexTable handle
├── waiter.rs    Arc waiter, route, and checked terminal-state transitions
├── wake_op.rs   FUTEX_WAKE_OP decode/arithmetic
└── lib.rs       re-exports (FutexKey, global_table, FutexWakeOp)
```

```text
MmSpace::resolve_futex_backing
          |
          v
      FutexKey
          |
          v
global FutexTable[256]  (lazy, hash-seeded, permanent)
          |
          v
SpinNoPreempt<VecDeque<Arc<FutexWaiter>>>
```

## Key contract

- `FUTEX_PRIVATE_FLAG` yields `(mm_id, virtual_address)` without consulting
  VMA metadata (`FutexKey::resolve_private`).
- A non-private operation on a private VMA still yields
  `(mm_id, virtual_address)`.
- Shared anonymous/file VMAs yield
  `(VmObjectId, backing_page_index, byte_offset_in_page)`.
- The backing offset is computed by `memspace` from post-split/trim VMA
  metadata; `kfutex` never derives VMA-relative offsets itself.
- Addresses must be `u32`-aligned and inside the user range
  (`check_access` before any queue operation).

## Waiter lifecycle

```text
Init -> Queued -> Woken
               -> Cancelled
```

A waiter is held by both the waiting future and its bucket `Arc`. Dropping
the future removes itself only through `Arc::ptr_eq`, never by storing raw
pointers to buckets or queue entries.

Requeue updates the waiter's key, bucket id, and generation while both
source and target bucket locks are held. The cancellation path first reads
a route snapshot, then takes the corresponding bucket lock and re-checks
the generation; if the route changed it retries.

Signal and timeout may only return as errors after winning the
`Queued -> Cancelled` transition under the bucket lock. If WAKE already
performed `Queued -> Woken`, the waiter must return success so a single
wake cannot be both counted and consumed by an `EINTR`/`ETIMEDOUT` return.

## Linearization

- WAIT: the user word is faulted in outside the bucket lock
  (`atomic_u32_eq`), compared again under the lock with a nofault load, and
  enqueued only when equal; a transient nofault failure inside the lock
  re-arms the waker and retries the poll instead of returning `EFAULT`
  while the mapping is still valid. The wait comparison is a read-only
  atomic load, so it never requires a writable user page or causes a
  pointless COW.
- WAKE: under the bucket lock, marks at most N matching waiters
  `Queued -> Woken`.
- CMP_REQUEUE: locks both buckets in index order, performs the nofault
  compare (mismatch is `EAGAIN`), then completes the wake and route move.
- WAKE_OP: under the same two-lock order, performs one nofault user-word
  CAS, then selects waiters on both keys. A changed word returns `EAGAIN`
  without marking waiters, so the syscall can drop the bucket locks,
  honor a pending preemption, and retry. A fault returns `EFAULT` the same
  way. The syscall calls `kuaccess::fault_in_user_writeable` (which makes
  the word writable without storing the operation result) only for that
  fault, then retries. A non-private operation also write-faults `uaddr2`
  before the first attempt and resolves both addresses again after either
  retry. Private keys stay `{mm_id, address}`. A mapping that does not
  allow writes fails the fault-in; the word is unchanged and no waiter is
  woken. Out of memory from the fault is `ENOMEM`. As in Linux
  `futex_wake_op`, the retry has no attempt cap: it stops when fault-in or
  key resolution fails.

Faulting user accesses happen only outside bucket locks. Inside the locks
only kuaccess nofault atomics run, so page-fault handling cannot happen
inside a non-preemptible bucket critical section. Waking the scheduler
happens after the bucket lock is released: waiters marked `Woken` stay in
the bucket until `drain_inactive` removes them and calls their wakers under
`ktask::with_wake_sync` (Linux WF_SYNC hint: the waker usually sleeps
next).

## Bucket lifetime and lock ordering

The 256 buckets are created once on first futex use (lazily seeded from the
monotonic clock) and live forever. There is no dynamic key-to-entry cache,
so empty keys need no reclamation and no entry-drop-vs-wake use-after-free
window exists.

Two-bucket operations always lock the lower index first. Waiter route
metadata is mutated only while holding the covering bucket lock(s); the
cancellation path must re-validate its snapshot inside the lock.

## Public handle

The concrete `FutexTable` is crate-private; `global_table()` returns the
zero-sized `GlobalFutexTable` handle exposing `wait`, `wake`, `wake_op`,
and `requeue`. This keeps the table type out of the public API while
method chaining stays unchanged.

## Robust mutex interaction

Robust owner death is encoded only into the user futex word: preserve
`FUTEX_WAITERS`, clear the TID, set `FUTEX_OWNER_DIED`, then optionally
wake one waiter based on the WAITERS bit — the walk lives in
`posix/process` (`exit_robust_list`) and `ksyscall`. `kfutex` keeps no
kernel-side `owner_dead` flag, and plain `FUTEX_WAIT` never returns
`EOWNERDEAD`; pthread mutex protocols produce that error in user space.

## Testing

`ut-tests/table.rs`/`ut-tests/waiter.rs` cover same-key requeue, wake+move,
cancellation of requeued waiters, wake-wins-over-late-cancellation, route
generation bumps, and single-terminal-transition invariants.

## Kernel unit-test layout

Guest test sources live in `ut-tests/` and are mounted at their original
module boundaries with `unittest::path_mod_test!`. They use `#[def_test]`
and run in the configured kernel with `make unittest`, not host Cargo tests.
The migration preserves module visibility, feature gates and execution modes.

- `ut-tests/wake_op.rs`: mounted by `src/wake_op.rs` as `tests`.
- `ut-tests/waiter.rs`: mounted by `src/waiter.rs` as `tests`.
- `ut-tests/table.rs`: mounted by `src/table.rs` as `tests`.
