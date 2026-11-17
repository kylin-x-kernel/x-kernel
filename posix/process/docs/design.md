# posix-process — Design

## Page-fault OOM policy

`PageFaultOutcome::OutOfMemory` has a separate trap-loop path. With no reclaim
engine or system-wide OOM victim selector, the current allocating process is
selected for an uncatchable kernel `SIGKILL`. The existing fatal-signal path
performs group termination and resource teardown. Valid mappings with exhausted
backing are not reported as `SIGSEGV/SEGV_MAPERR`; unmapped/protection faults
retain their normal signals. There is no unconditional fault retry. This is a
minimal OOM victim policy, not Linux's reclaim, badness scoring, memcg OOM or
OOM reaper implementation.

## Synchronous signal-frame faults

`raise_current_thread_fault` queues kernel-generated faults on the current
thread's user-return path. No sibling can consume the notification. If blocked
or ignored, it terminates the group with signal status instead of resuming an
invalid sigreturn frame. Existing process-directed fatal delivery stays separate.

## x86_64 child FP inheritance

`prepare_user_entry` captures the creator's current x87/SSE/YMM state before
publication and retains it in the prepared entry closure. On first execution,
the child installs it before the user loop; subsequent task switches use kcpu's
eager save/restore. The snapshot carries the child's already-selected FS/GS
bases, including clone TLS setup. Exec resets FP state through UserContext.


Fatal trap delivery explicitly constructs a kernel_fallback notification before
calling the fallible signal manager. NOWAIT owner exhaustion may lose detailed
siginfo but must preserve the pending bit; it is not treated as an ordinary
user-queued realtime request. Failure to find an interruptible target retains
the existing direct-exit behavior. Other manager errors are propagated.

The x86 syscall-restart guest regressions live in `ut-tests/runtime/tests.rs`,
mounted at their original scope with `unittest::path_mod_test!`. Their
`target_arch = "x86_64"` gate is retained; no architecture API is widened.

## Purpose and scope

This crate orchestrates user-thread execution, PID 1 construction and exit.
The complete implementation is `src/lib.rs`, `src/init_process.rs` and
`src/runtime.rs`. `kprocess` owns process identities, runtime resources,
publication and process relationships; `kexec` owns executable loading;
`ksyscall` owns syscall decoding. This crate supplies the trap loop and orders
calls to those owners rather than duplicating their registries.

## Architecture and entry points

`prepare_user_entry` owns the saved user context and dispatcher in a fallibly
allocated `PreparedTaskEntry`. It uses KERNEL and requires sleepable task context
without reclaim-dependent locks. Clone calls it before process/thread preparation;
`new_user_task` now accepts this prepared owner instead of constructing a closure
from context and dispatcher arguments. It adds no fallible callback allocation
after prepared clone ownership is committed.

PID 1 prepares its entry in the late-init task after releasing setup locks.
Failure remains explicit at the existing fatal bootstrap boundary; this is not
NOFAIL allocation or an implicit retry policy.

`new_user_task` accepts a preallocated `KBox<Thread>` and transfers it directly
to `TaskInner::new_user` as the user-runtime DST without reboxing. Initial
`build_process_thread` errors are reported at PID 1's existing fatal bootstrap
boundary; ordinary clone preparation returns errors before publication. This
change does not make the remaining task-construction allocations fallible.

Boot code calls `spawn_init_process` with argv, environment, a syscall dispatcher
and an exit callback. It allocates root PID 1, resolves executable metadata,
calls `prepare_user_app_request` to own a validated image and initial stack,
then `load_prepared_user_app` to install into its unpublished, user-empty MM. It
creates the process/thread runtime, installs console stdio, seeds the task's page
table root, and publishes with `publish_user_task(...).commit(...)`. It creates a
fresh task and leaves the bootstrap thread intact. It does not acquire a
controlling terminal; session/TTY acquisition belongs to later userspace.
Neither preparation nor installation requires a current user thread or changes
the bootstrap task's active MM. Init never discards an existing user image.

Clone adapters use `new_user_task` with matching `PidHandle` and `Thread` values.
The returned task is still unpublished: the caller completes process publication
before activation. Its closure enters `run_user_thread_loop`, which calls the
provided dispatcher for syscalls and `MmSpace::handle_page_fault` for faults.
`BusError` maps to `SIGBUS/BUS_ADRERR`, `AccessDenied` to `SIGSEGV/SEGV_ACCERR`,
and other unresolved faults to `SIGSEGV/SEGV_MAPERR`; the faulting virtual
address is carried in `si_addr`. Fatal fault signals are queued on the faulting
thread, not the process-wide pending set, so synchronous handlers observe the
thread-local state they installed. Retryable faults retry user execution after
checking preemption. Signal delivery is delegated to `Thread::signal_manager`;
fatal and stop actions call `do_exit`, while continue actions leave the running
thread alive.

A concrete integration example is `spawn_init_process` in `src/init_process.rs`:
it shows identity/runtime preparation, task construction, publication and the
observable effect (the initial image enters userspace on its own kernel stack).

## Execution context

`spawn_init_process` runs from a PID-less late-init kernel thread after the
allocator, scheduler, root filesystem, initial filesystem context and stdio
providers are available. The first root PID allocation must yield 1. It keeps
the task's default all-online-CPU affinity. Its `after_init_exit` callback runs
on the init task when the loop ends and normally shuts the system down.

`do_exit`, `raise_signal_fatal` and `check_signals` are current-user-thread paths;
`check_signals` must receive that thread and its saved context. The loop uses
current-task state, mapped userspace and scheduler services. These paths may
allocate, lock, yield or block and are unsuitable for interrupt context or early
boot. Do not reenter teardown for the same thread or hold a TEE session context
across its exit cleanup.

## Runtime and teardown flow

`UserContext::run` owns the IRQ-masked user-transfer boundary. Its entry callback
publishes user mode, then calls `krcu::enter_user_quiescent`; its return callback
calls `krcu::exit_user_quiescent` with no accounting work. The entry callback
publishes user mode as a single lock-free store, not a settlement, so the
boundary never reads the clock or takes a lock. Kernel mode is published only
after `run` returns, once the trap has actually been taken, so a timer tick that
arrives from userspace settles the interrupted interval as user time. The return callback precedes architecture IRQ
dispatch as well as syscall/fault processing in this loop. No RCU read scope or protected borrow may span user
execution; retained task/thread references use independent ownership. RCU
system state, GP service and private read-admission policy belong to `krcu`,
with algorithms in `krcu-core`, not POSIX. `ktask` retains task reader storage
and scheduler boundaries. No public consumer read interface is opened here.

The loop alternates user execution, trap handling, kernel CPU accounting and
signal checks. After the trap, one lock-free probe packs the independent
return-work projections — deliverable signal, interrupted task, armed CPU
timer, pending reschedule, signal-return action and syscall restart errno. When
it is clear, the loop skips signal scanning, timer polling, the scheduling
checkpoint and restart handling and goes straight back through `uctx.run`;
`enter_user_quiescent` still finishes deferred RCU reader cleanup before the
eret. The loop clears the previous trap's interrupt flag before the
normal pending-signal check. A later clear happens only when another scan
will follow it. Clearing after the last scan would let a concurrent group-exit
sender queue SIGKILL and set the flag in between, then lose that notification
before the target enters a futex wait.
Signal return still runs the pending-signal check. The handled signal was
dequeued before the handler ran, so the check delivers only a signal queued
while that handler blocked it and made deliverable when the previous mask is
restored. Restart of the interrupted syscall is not applied again on this path.
For other traps, the return records the restart code and drains pending
signals before it polls CPU timers and checks preemption. An interrupt raised
by that poll or by preemption causes one more signal scan only if the first
drain did not install a handler; it never loops on the interrupt flag. If a
handler is already installed, it runs first and the new signal remains pending
with its interrupt notification intact for a later return, including sigreturn.
CPU-timer polling runs only when a periodic tick requested work for this thread;
an armed timer alone no longer forces a process-wide scan on each syscall.
Delivery inside a long-running handler is not promised by this return path.
A peer that keeps posting signals must not pin the thread in
the kernel: the first handler ends the drain so userspace runs it, and
non-handler signals are capped, with one yield when the cap is hit while the
task is still interrupted. Signals left pending keep their notification.
The no-handler rewrite runs only after those scans. A handler installed by
either of them turns `ERESTART_RESTARTBLOCK` into `EINTR` and drops that
restart-block; rewriting the code first would hide it from the later handler,
so the timed wait would resume after the handler returned. Consuming a default
continue action does not count as a handler. Internal restart codes must not
become user-visible errno values. `ERESTARTSYS` rolls the syscall back, and
`ERESTART_RESTARTBLOCK` (timed futex wait) becomes `restart_syscall` so the
stored deadline is kept.
Signals that arrive after that single rescan keep their interrupt notification
for the next interruptible wait or trap.

`exec_de_thread` (Linux `de_thread()`): `execve` serializes on the process
`exec` gate, then SIGKILLs every sibling thread and joins them, so only the
execing thread swaps the address space. Swept targets are marked so their fatal
death does not mark the group exited or broadcast a group kill, and a genuine
concurrent `exit_group()`/fatal signal aborts the exec instead of being undone.
On success it commits the surviving leader's parent contract as `SIGCHLD`, as
Linux does before `begin_new_exec()` proceeds to file and MM replacement; a
single-threaded exec commits the same contract after its address-space exchange.
Go runtimes always have helper threads alive at exec time, so without this
every Go program that execs (runc init, containerd-shim) fails.

`do_exit` performs the following order:

1. Traverse the registered robust-futex list, then clear `clear_child_tid` and
   wake its futex if the write and key resolution succeed.
2. Release optional per-thread TEE state, close this thread's files and detach
   cgroup membership.
3. Call `finish_thread_exit`. `kprocess` removes membership and reserves the
   unique last-thread qualification under its membership lock only if this call
   actually removes the final published member. It releases the lock and retires
   the global TID publication before returning. Repeated exits cannot qualify
   again, and later task publication into the reserved process is rejected.
4. Close the CPU-accounting interval and accumulate final thread CPU time.
   The exit flag is published earlier, before the PI sweep; switch-in keeps
   charging cleanup until this close.
5. For the last thread, run the optional named `exit_tee` stage. It calls
   `tee_ta_release_sessions` after TID retirement, with the current user runtime
   still installed and the filesystem context available for the manager path.
   It may sleep without holding the membership lock, touches no user memory or
   process descriptors, and shares a one-second exchange deadline. Failure is
   logged and exit continues. Only this side effect is feature-gated; TEE and
   non-TEE builds use the same membership and reservation algorithm.
6. For the last thread, detach signal handlers and the mm owner, clear SysV shm
   accounting and optional TEE runtime-private state, then detach the filesystem
   context and namespaces. With TIPC enabled, close process-local handles before
   publishing process exit.
7. Complete process exit/parent notification only after the last-thread cleanup
   and resource detach. If group exit was requested and not already marked,
   mark it and send SIGKILL to surviving sibling threads.
8. Return to the kernel continuation; the trap loop stops before user reentry.
   `check_signals` rejects an already exiting thread before touching its signal
   queue or constructing a user frame, including the next signal-loop iteration
   after a fatal signal itself called `do_exit`.

## Concurrency and resource release

This crate creates no global process registry or additional lifecycle lock.
`kprocess` serializes membership, parent notification and runtime owner slots.
Objects taken out of owner slots are destroyed outside the slot locks.
Last-thread qualification blocks later task publication before TID retirement.
`do_exit` explicitly runs `exit_tee` and the remaining cleanup stages after
retirement, with no membership lock held. The current user runtime keeps the
exiting continuation usable even though global TID lookup no longer finds it.
`complete_process_exit` remains after every cleanup stage and process-resource
detach, so a parent cannot reap the process while TEE cleanup is still running.
The exiting task's structural MM pin survives through task destruction; it is
not an active MM user and does not keep old user mappings accessible. Scheduler
residency and AArch64 ASID selection follow its captured MM, not a survivor's
later runtime binding. Membership count alone does not imply a finished CPU tail.
`SHM_MANAGER` serializes shared-memory exit accounting. Robust-futex updates use
`kuaccess` atomic load/compare-exchange and `kfutex` keys/wake queues; the list
walk yields between nodes and has a fixed traversal limit.

## Decisions and limitations

Keeping IPC cleanup here avoids a `kprocess` dependency on `posix-ipc` while
preserving release-before-parent-notification ordering. Page-fault classification
comes from MM so architecture trap code need not understand file-backed EOF.
Stop currently terminates the group with exit code 1; CoreDump terminates with
128 plus the signal number and does not write a core image. Cleanup errors
are generally logged or ignored so teardown
can continue; these are current limitations, not complete Linux compatibility.

## PM cooperative safe point

The user-thread loop calls `ktask::freezer::checkpoint` before each user-mode
entry, after syscall/signal handling has released its scoped kernel resources.
It is outside both IRQ-off architecture transfer callbacks. Only explicitly
requested tasks block; a task stuck inside a syscall is not falsely counted as
frozen. Freezing does not set the process interruption bit or fabricate EINTR.

User execution faults use the captured live MM's sleepable resolver. Backing
admission waits release the MM mutex and block on existing provider resources
before lookup and permissions are checked again. This is the same wait contract
used by kernel user-memory copies, without sleeping in architecture trap code.

## PI futex exit

After robust-list marking, `do_exit` publishes the Thread exit flag and calls
`kfutex::exit_pi` before retiring its TID. PI-tagged robust entries are marked
but never passed to ordinary wake. Kernel ownership is handed off independently
of user mappings; each waiter repairs its word before returning to userspace.
The exit flag and bucket-locked adoption check prevent attachment after sweep.
