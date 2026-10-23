# ksignal design

The process-handler ownership regressions live in `ut-tests/process_handlers.rs`,
mounted under `api::process::handler_tests` with `unittest::path_mod_test!`.
Only their module location changes; handler lifecycle and assertions do not.

## Purpose

`ksignal` owns the Linux signal data model and the per-process/per-thread
signal state of X-Kernel: signal numbers and sets, `siginfo_t` payloads,
dispositions, pending queues, blocking state, alternate stacks, and the
construction/restoration of user handler frames. `kprocess` embeds its
managers into process/thread runtimes; `kexec` maps the signal trampoline
during exec; `ktimer` produces timer `siginfo` payloads and depends on
`ksignal` types; `ksyscall` and the posix process runtime own ABI decoding,
permission checks, and the teardown actions behind `SignalOsAction`.

## Scope

```text
process/ksignal/src/
├── lib.rs               crate root, CurrentSignalDispatch provider contract
├── types.rs             Signo, SignalSet, SignalInfo, ChildExitInfo, SignalStack
├── action.rs            SignalAction, SignalActionFlags, dispositions, OS actions
├── pending.rs           PendingSignals (standard + real-time queues)
├── siglock.rs           SigLock: signal-state ownership, transactions, projections
├── trampoline.rs        map_signal_trampoline
├── api/
│   ├── mod.rs           re-exports
│   ├── process.rs       ProcessSignalManager, child-exit notification
│   ├── thread.rs        ThreadSignalManager, frame dispatch, sigreturn restore
│   └── dequeue_observer.rs  per-signal dequeue observers
├── arch/                per-architecture MContext/UContext and trampoline
│   ├── mod.rs, x86_64.rs, aarch64.rs, riscv.rs, loongarch64.rs
└── ../ut-tests/         kernel unit tests
```

## Architecture

```text
            ksyscall (ABI decode, permissions)
                       |
posix/process runtime (check_signals -> process teardown)
                       |
        ProcessSignalManager              ThreadSignalManager
        ├─ siglock: SigLock ────────┐     ├─ slot: SlotId (generation-checked)
        │    ├─ shared: PendingSignals    ├─ stack: SignalStack
        │    └─ slots: per-thread    <----┴─ hint: Arc<AtomicBool>
        │         pending / blocked /
        │         saved_sigmask / hint
        ├─ handlers: Option<Arc<SignalHandlers>>
        └─ fatal_signal: AtomicU64
                  \                            /
                   transaction: select + queue or take + republish
                                |
              handle_signal -> SignalFrame on user stack
                                |
              arch::UContext / MContext + signal trampoline page
```

`ProcessSignalManager` holds process-shared actions and the `SigLock` that
owns every pending queue and blocked mask; each `ThreadSignalManager` holds a
generation-checked slot handle into it rather than a registered `Weak`
reference. One dequeue takes the thread queue first, then the process queue,
from the same transaction. `SignalActions` is
shared by `Arc` so `sigaction` updates are immediately visible to every
thread. Its shared reference stays inside the signal managers: constructors
take an owned `SignalActions` value, and `fork_shared` / `fork_private` create
new process managers with shared configuration or an independent snapshot.
Both create empty pending queues and separate slot tables. Callers query or
replace actions through manager methods, not by retaining the underlying lock
or cloning its reference.

`SignalHandlers` owns the shared action table, its content lock, and the
`pending_changed` poll source shared by CLONE_SIGHAND peers.
`ProcessSignalManager.handlers` owns this process's reference, with a separate
lock for changing that reference. The shared object embeds its action table,
so no separate table allocation is required. Access takes ownership then table
locks; only shared fork clones the shared object's reference.
`reset_actions_for_exec` resets a sole owner's table in place. With other owners
it allocates a replacement outside the locks, rechecks ownership, then copies and
resets the current snapshot before publishing the replacement. The old reference
is dropped outside the ownership lock. Temporary manager references do not count
as table owners. Concurrent fork sees either the pre-exec or post-exec table.
Reset preserves only ignore dispositions; all action flags, masks and restorers
are cleared. Pending and blocked masks are unaffected. Last-thread cleanup
takes the optional configuration reference under the ownership lock and drops
it outside that lock, before parent-visible process completion. Runtime teardown
does the same for unpublished clone rollback; manager Drop is an idempotent
fallback. Retained manager references cannot retain or recreate configuration
ownership. Configuration access and fork/exec return `NoSuchProcess` after
release; signal send and prepared child-exit commit discard late delivery.
Sole-owner exec retains the arrival source; shared-owner exec creates a fresh
source. Neither change moves pending signals or rebinds captured weak sources.
Final configuration release drops its source outside signal locks, waking any
remaining registrations even if manager references are still retained.

## Raw signal construction

`Signo` owns numeric validity. Its `from_raw` constructor checks the input
carrier before decoding the signal, so syscall and clone callers share one
range and width boundary. The constructor has no process-state or user-memory
access; `ksyscall` retains errno mapping and the distinct meanings of zero in
signal probes, timer notifications and child-exit notification requests.
The external `ut-tests/signal_number.rs` regressions cover signed, unsigned
and wide input carriers. Existing typed signal operations keep their contracts.

## Queuing and delivery algorithm

Each shared handler object owns a `pending_changed` poll source. Process send,
thread send, and prepared child-exit commit publish pending state and its hint
under the manager ownership lock, then clone only the selected PollSet. They
release all signal locks before waking and dropping that temporary source.
Child-exit preparation captures policy, not a source; commit selects the current
source. A delayed pre-detach notification therefore still reaches the old source.
Notifications include accepted signals blocked from handler delivery and may
repeat for an already-pending standard signal. Thread managers expose
`register_pending` for signalfd waits. A notification only requests a recheck of
the caller's own pending sets; it neither consumes a signal nor makes another
thread's private signal readable. Waiters register before their final readiness
check, and registration guards cancel subscriptions on drop. This follows Linux's
signal-side notification ownership without binding a signalfd to its creator.

Both managers expose `pending_source() -> KResult<WeakPollSet>` and
`notify_pending() -> KResult<()>`; inactive ownership returns `NoSuchProcess`.
Capture does not add a handler owner or keep the source alive. Explicit notify
clones the current PollSet under ownership, then wakes outside signal locks;
it does not publish pending data or imply matching readiness. Thread registration
captures a weak source, releases ownership, then calls `register_weak`.
Already-inactive capture maps to `PollRegisterError::InvalidState`; a captured
source that later dies registers successfully without installing a waiter or
rebinding. Ordinary fork has an independent source despite any shared file.

`PendingSignals` keeps one `Option<Box<SignalInfo>>` per standard signal
(1-31; at most one pending instance) and one `VecDeque` per real-time signal
(32-64; queued instances, FIFO). The `SignalSet` bitmap drives dequeue order:
`dequeue_signal` picks the lowest-numbered unblocked pending signal, and a
non-empty real-time queue re-marks its bit so the next instance stays
selectable.

`send_signal` (process) mirrors Linux `prepare_signal`/`complete_signal`:
an ignored disposition drops the signal only when no live thread currently
blocks it — a blocked signal stays pending because userspace may install a
handler before unblocking. Target-thread selection picks the first live
non-blocking thread that has not left the group, in the same transaction that
queues the signal. The thread-level `send_signal` follows the same
ignore/blocked policy.

`check_signals` reads the calling thread's projection to avoid locking on the
common no-signal return. The slow
path dequeues against the thread's own blocked mask, consults dequeue observers
(used by `kprocess` timer delivery to drop stale POSIX timer signals), then
dispatches through `handle_signal`.

Handler dispatch builds a `SignalFrame { ucontext, siginfo, saved }` on the
user stack (alternate stack when `SA_ONSTACK` and configured), rewrites
`UserContext` to enter the handler with `(signo, siginfo*, ucontext*)`
arguments, sets the restorer return path (x86_64 pushes it on the stack,
others use the return register), applies the handler mask plus the signal
itself unless `SA_NODEFER`, and resets the action to default on
`SA_RESETHAND`. Syscall-restart error codes in the trap context are folded
into either a rollback (restart) or `-EINTR` before the handler runs.
Default `SIGCONT` (`SignalOsAction::Continue`) does not fold those codes.
The posix return path rewrites them when no handler ran, and drops the
thread restart-block when a handler publishes `EINTR` for
`ERESTART_RESTARTBLOCK`. `restore` reads the frame back on `rt_sigreturn`, restores the
blocked set from `uc_sigmask`, and re-arms the pending flag.

Child-exit notification is split into `prepare_child_exit_signal` (decision)
and `commit_child_exit_signal` (queueing) so `kprocess` can publish exit and
autoreap state before `SIGCHLD` becomes observable; the default-ignored
disposition of `SIGCHLD` does not suppress queuing, while explicit `SIG_IGN`
and `SA_NOCLDWAIT` request autoreap.

## Execution context

- All managers are lock-based (`SpinNoIrq`) and involve no sleeping within
  this crate; they are safe on syscall and trap return paths.
- `handle_signal`, `check_signals`, and `restore` run on the
  user-return path of the owning thread; `handle_signal` writes to
  user memory through `osvm` checked access (`write_vm`), so it may fail on
  an unmapped stack, which is reported as `CoreDump`.
- Handler invocation itself never happens here: the caller observing
  `SignalOsAction::Terminate/CoreDump/Stop/Continue` performs teardown in
  the posix process runtime, which may sleep.
- Dequeue observers run synchronously inside the dequeue path; observers
  must only rely on properties guaranteed by every path that can dequeue
  their signal (in practice: current user thread with its process runtime
  reachable).
- `register_signal_observer` mutates a global table; registration is
  expected during subsystem init, not on hot dequeue paths.
- `map_signal_trampoline` requires a mutable `MmSpace` and runs during exec
  address-space setup.

## State

Pending-queue state is queue/bitmap bookkeeping without an explicit state
machine. The `SignalDisposition` of each action is `Default | Ignore |
Handler(fn)`, replaced wholesale by `sigaction` and reset to `Default` by
`SA_RESETHAND` during dispatch. `saved_sigmask` is an `Option<SignalSet>`
set by `rt_sigsuspend` and consumed by the next caught-signal frame build
(Linux `saved_sigmask` semantics).

## Concurrency

- Lock order is `ProcessSignalManager.handlers` -> `SignalHandlers.actions`
  and -> `SigLock`. Fork/exec use the same ownership-before-content order; they
  do not lock another process's ownership. `SigLock` is never taken before
  `handlers`.
- `SigLock` owns the whole deliverability predicate: the process-directed
  queue plus each member thread's private queue, blocked set and saved sigmask.
  Deliverability is `pending ∩ ¬blocked`, so observing its factors from
  different critical sections is the bug class this layout removes. Target
  selection and queueing of a process-directed signal are one transaction, so a
  selected thread cannot change its mask in between.
- The fast-path projection is per thread and counts both queues under that
  thread's own blocked mask, the same predicate Linux keeps in `TIF_SIGPENDING`
  (`recalc_sigpending_tsk`). It may over-report (one extra slow-path pass) and
  may never under-report. It is written only by `SigLock::republish`, inside the
  transaction that changed the state, and derived from that state; `republish`
  writes only real transitions. Callers cannot publish an availability answer,
  so a receiver can no longer overwrite a sender's notification with a stale
  observation taken before unlocking. A signal every thread blocks therefore
  keeps the fast path quiet instead of forcing a slow-path pass per user
  return.
- `SigTxn` is the only mutation handle; `SigView` is the read-only capability
  used by query paths. `SigLock::transact` and `SigLock::inspect` construct them,
  they borrow the guarded state for the critical section, and they cannot
  outlive it. Verbs take their inputs from the guarded state — `take` derives
  the delivery mask from the thread's blocked set — instead of from a caller
  snapshot.
- Transactions record the slots and queues they touched in an allocation-free
  dirty set: `republish` recomputes only those, so mask changes, enqueues and
  dequeues stay O(1) inside the lock, and read-only queries recompute nothing.
  A process-directed change marks all slots, because the answer differs per
  thread; that cost is paid once per process-directed signal, as in
  `complete_signal`, not per syscall return. The dirty set is inline (256 slots)
  and degrades to a full recompute above it rather than allocating in interrupt
  context. Set bits are walked directly, and a transaction that touched nothing
  skips the recompute entirely.
- Dequeue observers, user-memory frame writes, arrival wakes and
  `task.interrupt()` run outside `SigLock`. A signal consumed by an observer
  costs one further transaction instead of a critical section held across a
  callback. Wakes stay outside the `handlers` guard because the arrival source
  is `Arc`-shared with CLONE_SIGHAND peers and cannot move into `SigLock`.
- Slot handles carry a generation, so `exit()`, `Drop` and `rebind_after_exec`
  cannot address a slot that was released and reused. Because `Drop` releases
  the slot, the last manager reference must not be dropped while `SigLock` is
  held (`SigLock` is a non-reentrant `SpinNoIrq`); no path does that, and
  transactions never touch a manager reference. `exit()` only clears the
  slot's target flag: the thread's own pending and blocked state stays
  observable while it unwinds, and `Drop` releases the slot.
- `fatal_signal` remains a packed `AtomicU64` read by the exec/de_thread
  sweep. It is not part of the deliverability predicate, but it is published
  inside the same transaction as the queue so the sweep cannot absorb a fatal
  signal that was already queued.
- `SignalInfo` is `Send + Sync` through an explicit `unsafe impl` because
  the payload is treated as opaque ABI bits (see `security.md`).

## Signal state ownership

`SigLock` (`src/siglock.rs`) is the only critical section that answers signal
deliverability. `ThreadSignalManager` keeps only a slot handle, its alternate
stack and a projection cell; the pending queue, blocked mask and saved sigmask
it used to own per field live in the shared state, so a mask change and an
enqueue are serialized without a second lock. `handlers` and the action table
stay outside it: dispositions are `Arc`-shared with CLONE_SIGHAND peers and are
not part of the deliverability predicate, so they keep their own locks and the
ownership-before-content order.

Slots hold their payload out of line (`Box<ThreadSlot>`): `PendingSignals` is
about 1.3 KiB, so an inlined table would keep a thread-count peak resident for
the process lifetime. `retire` clears the slot and pops trailing empty slots,
so the table shrinks back after a thread-count peak. Registration allocates the
boxed slot and its projection cell *before* taking the lock and only links the
prepared slot inside the transaction, so no `Box`/`Arc` allocation or slab
refill runs in the critical section other threads' sends and slow-path dequeues
spin on. The table is pre-sized at construction (before the manager is shared,
so nothing spins on the lock yet); a process that exceeds that capacity pays an
amortized doubling of 8-byte entries, never of the slot payload.

The one deliberate exception is the signal payload: `put_thread`/`put_shared`
reach `PendingSignals::put_signal`, which boxes a standard signal only after the
queue accepts it, so a send may allocate inside the lock. That matches Linux,
where `__send_signal_locked` asserts it already holds `sighand->siglock`, skips
allocation for a coalesced signal or `SIGKILL`, and otherwise calls
`__sigqueue_alloc` with `GFP_ATOMIC`. Moving that allocation to the caller was
considered and rejected: it would allocate on the coalescing path that Linux
specifically avoids, and would break the `SIGKILL` case that never allocates.

## Design decisions

- Handler functions are typed `unsafe extern "C" fn(i32)` and stored as an
  address; the kernel never calls them directly — it only writes a user
  frame — so no kernel-side indirect call risk is introduced.
- The signal frame embeds both the ABI `ucontext_t`/`siginfo_t` (user-visible
  layout, musl-compatible register numbering) and a private
  `UserRestorableContext` carrying the state `mcontext_t` does not cover:
  segment bases plus, on x86_64, an `FXSAVE` snapshot of the interrupted
  x87/MMX/XMM/MXCSR state. Each frame owns its snapshot, so nested signals
  restore the FP/SIMD state layer by layer. The region is written into the
  user-writable frame, so `restore` treats it as untrusted input (x86_64
  clears unsupported `MXCSR` bits before `FXRSTOR`).
- x86_64 pushes the restorer address on the user stack to match the
  kernel-only `SA_RESTORER` ABI; other architectures use the return-address
  register.
- A trampoline page is mapped at a fixed address per address space so
  binaries without a vdso still get a working `sigreturn`.
- Kernel-originated `sigval` reads expose `None` unless `si_code` implies a
  user or timer payload, keeping union-arm reads (see `security.md`)
  guarded by the code that selected the arm.

## Resource lifecycle

Managers are created by `kprocess` when a process/thread runtime is built
(`ThreadSignalManager::new` self-registers with the process manager). Shared
signal configuration, pending queues and poll sources are kernel-owned; frame
memory lives in user address space and alternate stacks are user-owned.

The last thread's exit releases its process's shared-configuration ownership.
`ProcessSignalManager::drop` repeats that idempotent release as a fallback.
The removed configuration reference is dropped outside the ownership lock;
other processes may still own the same configuration. Final poll-source
destruction detaches registrations and can synchronously invoke their wakers.
Configuration release must therefore occur outside process-domain, membership
and signal locks, as required by `release_handlers`.
