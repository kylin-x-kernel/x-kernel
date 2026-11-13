# kfutex design

## Purpose

`kfutex` owns ordinary futex concurrency and kernel PI ownership: the canonical
key, the global fixed bucket table, the waiter state machine, and the
linearization rules of wait/wake/requeue/wake-op. Syscall ABI decoding,
timeout-flag parsing, count clamping, and robust-list walking stay in
`ksyscall` (`core/ksyscall/src/sync/futex.rs`) and `posix/process`; the
latter drives robust owner-death notification through this crate's key
resolution and wake primitives.

The three PI commands use `krtmutex::RawRtMutex`. Internal calls accept a
canonical key and validated owner; user calls also synchronize TID, WAITERS and
OWNER_DIED through nofault atomics. `PiFutexContext` supplies process lookup,
exit-state and mapping services without making `kfutex` depend on `kprocess`.
Syscall decoding and robust-list traversal remain in their existing owners.

## Scope

```text
process/kfutex/src/
├── key.rs       canonical private/shared key
├── table.rs     static buckets, compound operations, GlobalFutexTable handle
├── pi.rs        PI state definitions, internal commands and retirement
├── pi_user.rs   user-word PI transactions and exit handoff
├── waiter.rs    Arc waiter, route, and checked terminal-state transitions
├── wake_op.rs   FUTEX_WAKE_OP decode/arithmetic
└── lib.rs       re-exports (FutexKey, global_table, PiFutexContext, FutexWakeOp)
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
SpinNoPreempt<BucketState>
  ├── ordinary VecDeque<Arc<FutexWaiter>>
  └── PI BTreeMap<key projection, PiState>
```

## PI state and common table

`table.rs` owns bucket storage and ordinary operations; `pi.rs` owns the PI
index/value types, ownership and reclamation. `pi_user.rs` implements the user
protocol. Ordinary and PI state are separate, but share the bucket lock so a
key cannot acquire both kinds of waiter. Ordinary enrollment, checked wake,
requeue and wake-op reject active PI keys; ordinary wake never grants PI ownership.

An uncontended user acquisition CASes the word without creating kernel state.
A contender looks up the owner outside locks, then under the bucket lock
rechecks the word and owner's exit flag, sets WAITERS and publishes an adopted
raw lock. Enrollment takes bucket -> PI graph; donation and wake effects run
only after bucket unlock. Unlock commits the next TID before graph handoff.
`PiEffects` retains preemption exclusion until donation and wake delivery finish.
Timeout/signal returns an error only when cancellation wins over handoff.

Fault-in runs outside locks and retries with a newly resolved key. Acquired
waiters revalidate the key and repair their user word before returning; failed
repair passes ownership on. Exit first publishes the task-exiting flag, then
sweeps buckets and hands owned locks on with owner-death metadata. A waiter
repairs through its own mapping, including a shared alias in another process.

Internal commands pin entries until free and unpinned. User entries retire when
no call/waiter remains; their word retains ownership for later adoption. Free
user entries may also be detached with lookups in flight: every subsequent user mutation rechecks Arc
identity under the bucket lock. This permits userspace fast acquisition after
an unlock publishes zero without splitting a live kernel owner.

This separation follows Linux's shared hash-bucket synchronization and distinct
[PI implementation](https://github.com/torvalds/linux/blob/master/kernel/futex/pi.c).
Our separate per-bucket PI index avoids changing the ordinary waiter type.

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

`drain_inactive` retains the first woken waiter inline and collects any
remaining woken waiters in a temporary vector. A drain batch containing zero
or one woken waiter needs no temporary heap allocation. Larger batches keep
their bucket traversal order, and all scheduler callbacks still run after
unlock. The batch can include waiters selected by concurrent wake operations,
so its size is not necessarily this operation's requested wake count.

## Bucket lifetime and lock ordering

The 256 buckets are created once on first futex use (lazily seeded from the
monotonic clock) and live forever. Ordinary keys need no reclamation; PI entry retirement follows the pin and
identity-validation rules above.

Two-bucket operations always lock the lower index first. Waiter route
metadata is mutated only while holding the covering bucket lock(s); the
cancellation path must re-validate its snapshot inside the lock.

## Public handle

The concrete `FutexTable` is crate-private; `global_table()` returns the
zero-sized `GlobalFutexTable` handle exposing `wait`, `wake`, `wake_op`,
`requeue`, checked wake, internal PI operations, user PI operations and exit
cleanup. The concrete table stays private.

## Robust mutex interaction

The robust-list walker preserves WAITERS, clears TID and sets OWNER_DIED in
the word. Ordinary entries may wake one waiter; PI entries use the subsequent
PI exit sweep instead. Plain WAIT never returns EOWNERDEAD; pthread protocols
interpret the word and produce that result in userspace. The PI metadata
retains owner death until the next waiter has repaired its own mapping.

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

- `ut-tests/pi.rs`: internal PI ownership, donation, cancellation and pin lifetime.
- `ksyscall/ut-tests/sync/futex/tests.rs`: user PI word, errors, contention,
  timeout, interruption, faults and owner exit.
