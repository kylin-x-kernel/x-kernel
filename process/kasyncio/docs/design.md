# kasyncio design

## Purpose

`kasyncio` owns the Linux `io_uring`-compatible subset used for asynchronous
I/O (issue #IKJIGT, phase 1): the per-process context, submission/completion
rings, and in-process worker threads that execute submitted operations off the
submitting user thread. Syscall ABI decoding for `io_uring_setup` and
`io_uring_enter` stays in `ksyscall`; ring allocation, mmap, and file lifecycle
live here.

The synchronous syscall surface is untouched; async entry is additive.
Phase 1 executes `IORING_OP_NOP`; other Linux opcodes are consumed and
completed with `-EINVAL` until the operation dispatch (`ops.rs`) lands, so a
client never observes a fake success for I/O the kernel did not perform.

Supported: one shared SQ/CQ mmap (`IORING_FEAT_SINGLE_MMAP`), SQE submission,
CQE draining, and `IORING_ENTER_GETEVENTS`. Non-goals for this phase: SQPOLL,
registered files/buffers, linked operations, cancellation requests, full
io_uring feature coverage (`io_uring_register` stays unimplemented), and a
preemptive async runtime.

## Scope

```text
process/kasyncio/src/
├── abi.rs       Linux io_uring subset: SQE/CQE/params layouts and constants
├── context.rs   AsyncContext: ready/completed staging, worker accounting, wake sources
├── file.rs      anon-inode fops: enter, mmap, poll/register_poll, shutdown on release
├── ring.rs      SQ/CQ ring layout, SQE submission, CQE publication
├── setup.rs     context creation and io_uring_params outputs
├── worker.rs    spawn, idle park, retirement, signal/exit contracts
└── lib.rs       re-exports
```

`ops.rs` (operation decode/dispatch) remains the next step: only NOP is
accepted onto the ready queue, and `execute()` completes it with result `0`.
An entry with nonzero SQE flags or an unsupported opcode is consumed and
completed with `-EINVAL`. An out-of-range array index is dropped like Linux
`io_get_sqe`: `SQ_DROPPED` advances and the batch ends with no CQE, so an
invalid entry cannot wedge the submission ring.

## Execution model

Workers are **sibling threads of the submitting process**, spawned through
the ordinary clone path (`Thread::prepare_thread_clone` →
`TaskInner::new_user` → `publish_user_task().commit()`) with the process
page-table root installed and a pure kernel-mode entry loop. This is what
makes user-buffer operations sound without mm-adoption machinery: a worker
dereferences user VAs through the same guarded user-memory path as a
syscall, page faults resolve against the worker's own process, and the fd
table and credentials are shared by construction.

Growth is lazy: the submit path reserves a slot and spawns only when queued
work exceeds idle workers, bounded by `WORKER_CAP` (4) per context. The
eligibility check and the slot charge share one critical section
(`try_reserve_worker`), so concurrent submitters cannot oversubscribe the
cap; a failed spawn releases the reservation.

### Visibility and accounting (intended semantics)

Unlike io_uring's io-wq kernel threads, a kasyncio worker is a **real
published thread of the process**:

- its tid is globally published and `tgkill`/`kill` can target it;
- it appears in process task listings and PID directories;
- its CPU time is charged to the process;
- it participates in process thread membership (see the exit contract).

Phase 1 declares these as intended behavior, not incidental artifacts.
Hiding workers from signals/listings/accounting (a closer io_uring
analogue) is a design decision reserved for the syscall-integration step;
doing it later would change observable ABI surface, so it is called out
here explicitly to avoid rework under an ABI-compatibility pretext.

## Locking and wake contract

- `AsyncContext::inner` is a sleepable `ksync::Mutex` guarding short,
  non-blocking bookkeeping; nothing waits while holding it. Workers and
  submitters both run in sleepable task context and no IRQ handler touches
  this state, so no spinlock class is required.
- Two `PollSet` wake sources follow the detach-then-wake rule and are never
  woken under a lock: `worker_wake` (queued work or shutdown) and
  `cq_wake` (published completions).
- The ready queue is bounded by `READY_LIMIT` (1024). A full queue stops the
  submission batch at that SQE and returns a short count; the SQE remains at
  the shared SQ head for the next enter (Linux has no user-visible `EAGAIN`
  here).
- The ring lives in one physically contiguous block sized to the layout
  (power-of-two pages, at most 4 MiB; 32768 entries needs 4 MiB), and its pages
  are charged to the caller's memory cgroup. The block is The kernel accesses
  it through the direct map; userspace obtains the same pages through
  `mmap(fd, IORING_OFF_SQ_RING)` and `mmap(fd, IORING_OFF_SQES)`. The VMA
  holds an owning linear runtime over the block, so `close(fd)` before
  `munmap` keeps the pages alive exactly as Linux's ring page references do. The kernel
  keeps its own `sq_head` and `cq_tail`; user-owned `sq_tail`/`cq_head` are read
  from the shared header. Volatile field accesses plus acquire/release fences
  separate SQE/CQE payload publication from index updates.

## Completion contract

Every operation **accepted onto the ready queue** has exactly one of three
outcomes, and its user cookie is observable in exactly one completion:

- **Executed**: a worker dispatched it; the completion carries the natural
  result (non-negative count or negated errno).
- **Cancelled**: `shutdown` found it still queued (never dispatched); the
  completion carries a `-ECANCELED` CQE result.
- **In flight at shutdown**: already dispatched operations run to their
  natural completion even after shutdown; the context stays alive until
  the last worker retires.

A submission stopped by a full ready queue produces no completion for the
remaining SQEs: the batch returns a short count and the SQE stays at the
shared SQ head for retry. A shut-down context surfaces `BadFileDescriptor`
when no SQE was accepted. Neither case reports `EAGAIN` to userspace.

An SQE with nonzero fields or an unsupported opcode is different: it is consumed, the SQ head advances, and it
completes with `-EINVAL`, so later SQEs still run. An out-of-range SQ-array
index follows Linux `io_get_sqe` instead: the entry is consumed, `SQ_DROPPED`
advances, the batch ends, and no CQE is produced (the SQE was never read, so
no `user_data` exists to report).

`io_uring_enter` clamps `to_submit` to the published tail and `min_complete`
to the CQ depth, and counts CQEs already visible in the shared ring; it returns the accepted SQE count even when a
later worker-spawn or wait step fails, and the wait returns as soon as the
shared ring holds enough unconsumed CQEs.

## Worker exit contract

Workers retire — run the loop exit, then `posix_process::do_exit_worker()`
so membership, cgroup charge, and (when last) the process tail are handled
by the ordinary exit path without replacing the status published by the
ordinary threads — when any of the following holds:

1. **Context shutdown** (fd `release` or test): never blocks; the shutdown
   also cancels undispatched queue entries per the completion contract and
   wakes both `PollSet`s.
2. **Thread exit / process group exit**: the group-exit SIGKILL broadcast
   reaches workers through the same `interruptible` waits the synchronous
   syscalls use, so blocked operations unwind with `-EINTR`.
3. **Last ordinary thread gone**: `Process::has_only_in_process_workers`
   compares the worker count with the published-thread count under a worker
   epoch, so a concurrent attach/detach rejects the snapshot instead of
   letting a worker retire while an ordinary thread is still live. Workers
   re-evaluate at least once per `IDLE_PARK_BOUND` (200 ms); the last
   `do_exit_worker` then completes the process tail.
4. **Pending `SIGKILL`**: a worker never re-enters userspace, where
   fatal-signal delivery normally happens, so the idle path probes the
   thread's pending-signal set after every task-interrupt wake. `kill -9`
   on a worker tid therefore ends the worker. Handler-visible fatal
   signals other than `SIGKILL` still require a userspace return and are
   out of scope for phase 1.

## Known limitations

- Multiple contexts per process each park independently; the periodic
  park bound (not a cross-context wake) is what eventually retires their
  workers. Latency in that corner is bounded but not prompt.
- Completions are staged in a kernel-side deque and published to the CQ ring
  by `io_uring_enter` (or the next enter after a full ring). Only NOP executes
  until the `ops.rs` step; other opcodes complete with `-EINVAL` and are never
  reported as successful without doing the requested I/O.
- Handler-visible fatal signals other than `SIGKILL` still require a
  userspace return; workers only self-terminate on pending `SIGKILL`.
