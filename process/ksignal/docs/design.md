# ksignal design

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
        ProcessSignalManager           ThreadSignalManager
        ├─ pending: PendingSignals     ├─ pending: PendingSignals
        ├─ handlers: Option<Arc<      ├─ blocked / saved_sigmask
        │           SignalHandlers>>   ├─ stack: SignalStack
        ├─ children: Vec<(tid, Weak)>  └─ possibly_has_signal
        └─ has_pending: AtomicBool              |
                  \                            /
                   `Weak` registry, process fallback dequeue
                                |
              handle_signal -> SignalFrame on user stack
                                |
              arch::UContext / MContext + signal trampoline page
```

`ProcessSignalManager` holds process-shared actions and a process-directed
pending queue; each `ThreadSignalManager` registers itself with the process
manager through a `Weak` reference at creation. Thread dequeue first drains
the thread queue, then falls back to the process queue. `SignalActions` is
shared by `Arc` so `sigaction` updates are immediately visible to every
thread. Its shared reference stays inside the signal managers: constructors
take an owned `SignalActions` value, and `fork_shared` / `fork_private` create
new process managers with shared configuration or an independent snapshot.
Both create empty pending queues and separate thread lists. Callers query or
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
non-blocking thread while pruning dead `Weak` entries. The thread-level
`send_signal` follows the same ignore/blocked policy.

`check_signals` uses two fast-path atomics (`possibly_has_signal`,
process `has_pending`) to avoid locks on the common no-signal return. The
slow path dequeues against `!blocked`, consults dequeue observers (used by
`kprocess` timer delivery to drop stale POSIX timer signals), then dispatches
through `handle_signal`.

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

- Lock order is `ProcessSignalManager.handlers` -> `SignalHandlers.actions` ->
  children/blocked during delivery classification. Fork/exec use the same
  ownership-before-content order; they do not lock another process's ownership.
- Pending queues and each thread's saved mask/stack have separate locks.
  Send retains the ownership lock through pending publication, excluding
  ownership release. Actions/children/blocked locks are released before pending
  insertion; notification happens after all signal locks are released.
  Dequeue observers, user-memory access and wakes run outside configuration locks.
- `send_signal` reads actions and scans children inside one actions-lock
  generation to keep the disposition decision and target scan coherent; the
  doc comments deliberately do not claim Linux's global `sighand->siglock`
  equivalence for every field.
- `has_pending`/`possibly_has_signal` are Release/Acquire hints only; the
  authoritative check happens under the queue locks.
- Dead thread entries are pruned opportunistically during target scans.
- `SignalInfo` is `Send + Sync` through an explicit `unsafe impl` because
  the payload is treated as opaque ABI bits (see `security.md`).

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
