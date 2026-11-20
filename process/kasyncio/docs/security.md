# kasyncio security

## Trust boundary

Everything that crosses the user/kernel boundary here is untrusted until
validated:

- **SQE contents**: opcode, flags, fds, lengths, offsets, and user pointers
  arrive from userspace. The submit path validates the index array, SQE flags,
  and opcode before queueing. An entry with nonzero flags or an unsupported
  opcode is consumed and completed with `-EINVAL` (preserving its `user_data`),
  so it cannot block later entries and the client still learns which request
  failed. An out-of-range array index is dropped like Linux `io_get_sqe`:
  `SQ_DROPPED` advances, the batch ends, and no CQE is fabricated for an SQE
  that was never read.
- **User buffers**: operations carrying user VAs execute on a worker that
  is a thread of the owning process, so every access goes through the
  standard guarded user-memory path (`osvm`/`kuaccess`) with fault
  resolution against the worker's own address space. No worker ever
  dereferences a raw user pointer outside that path, and no other
  process's memory is reachable.
- **Ring memory**: ring pages are user-mappable and writable by design. The
  kernel keeps its own `sq_head`/`cq_tail`, masks every user-published index,
  validates every SQE (supported opcode, zero `flags`, bounded array index),
  and only publishes CQEs it generated. The pages are owned by the file while
  it is open and by the VMA's owning linear runtime while a mapping exists, so
  closing the last descriptor before `munmap` frees nothing that user PTEs
  still reference; closing it stops workers and drains staged completions.
  The raw zero-fill in `RingPages::new` is bounded by that allocation: the byte
  length is rounded to pages and `num_pages * PAGE_SIZE_4K >= len_bytes` is
  checked before the write, matching the SAFETY comment.
- **`io_uring_enter` arguments**: unknown enter flags are rejected; a non-NULL
  signal mask is rejected only when `IORING_ENTER_GETEVENTS` requests the wait
  this subset cannot service (Linux never reads the mask otherwise).
  `to_submit` is clamped to the published entries and `min_complete` to the
  CQ depth, as Linux does. Rejection staging is bounded by the completion-ring
  depth, so an invalid-SQE flood cannot grow the kernel queue without bound. A non-io_uring fd
  returns `EOPNOTSUPP`, matching Linux. The setup descriptor is installed
  close-on-exec unconditionally.

## Worker credentials and reach

A worker is a sibling thread of the submitting process, so it inherits —
by ordinary thread semantics, not by any kasyncio grant — the process
address space, fd table, and credentials. This is the same trust
relationship as any pthread the process could create: kasyncio does not
elevate, and cannot be used to escalate, what the process can already do
to itself. Workers never enter userspace, so they never execute attacker
-supplied code; they only execute the kernel's own operation dispatch.

Worker tids are globally published (see `design.md`, visibility and
accounting). `kill`/`tgkill` targeting a worker tid therefore requires the
signal permission checks that already govern inter-process signaling —
unchanged from any other thread.

## Resource bounds

- `WORKER_CAP` (4) bounds kernel stacks per context; the reservation is
  charged in the same critical section as the eligibility check, so
  concurrent submitters cannot oversubscribe it.
- `READY_LIMIT` (1024) bounds the per-context queue. A full queue stops the
  submission batch at the queued SQE and returns a short count; the SQE stays
  at the shared SQ head for the next `io_uring_enter`. It is not reported to
  userspace as `EAGAIN`, matching Linux.
- Ring blocks are charged to the caller's memory cgroup with
  `memacct::charge_frame`, and their size follows the requested layout (at most
  4 MiB); tiny rings no longer reserve a fixed multi-MiB block.
- Idle-park registration failures back off exponentially (10 ms doubled to
  a 320 ms ceiling) so a persistently failing wake source cannot spin the
  CPU.
- Workers retire on shutdown, process/group exit, pending `SIGKILL`, or
  the last ordinary thread leaving, so contexts cannot strand threads or
  pin process resources after their user goes away.

## Failure posture

No kasyncio path panics on user-controlled input: an invalid opcode or flags
become an error completion (the cookie is preserved), an invalid SQ-array index
is dropped with `SQ_DROPPED`, a full ready queue returns a short submission
count, idle-park registration failures back off, and teardown paths never block
their callers.
