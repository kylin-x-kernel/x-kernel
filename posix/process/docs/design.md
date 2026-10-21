# posix-process — Design

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
`BusError` maps to SIGBUS; other unresolved faults map to SIGSEGV. Retryable faults
retry user execution after checking preemption. Signal delivery is delegated to
`Thread::signal_manager`; fatal and stop actions call `do_exit`, while continue
actions leave the running thread alive.

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
finishes user-time accounting, then calls `krcu::enter_user_quiescent`; its
return callback calls `krcu::exit_user_quiescent` before kernel-time accounting.
This follows Linux's accounting-before-EQS / watching-before-accounting order.
The return callback precedes architecture IRQ dispatch as well as syscall/fault
processing in this loop. No RCU read scope or protected borrow may span user
execution; retained task/thread references use independent ownership. RCU
system state, GP service and private read-admission policy belong to `krcu`,
with algorithms in `krcu-core`, not POSIX. `ktask` retains task reader storage
and scheduler boundaries. No public consumer read interface is opened here.

The loop alternates user execution, trap handling, kernel CPU accounting and
signal checks. The loop clears the previous trap's interrupt flag before the
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
Go runtimes always have helper threads alive at exec time, so without this
every Go program that execs (runc init, containerd-shim) fails.

`do_exit` performs the following order:

1. Traverse the registered robust-futex list, then clear `clear_child_tid` and
   wake its futex if the write and key resolution succeed.
2. Release optional per-thread TEE state and detach cgroup membership.
3. Capture the thread's exit MM pin and scheduling context in `kprocess` before
   removing membership/publication. Mark the thread exiting, close its
   CPU-accounting interval and accumulate final thread CPU time.
4. For the last thread, detach the mm owner, clear SysV shm accounting, release
   optional TEE private state, then detach files, filesystem context and namespaces.
   With TIPC enabled, close process-local handles before publishing process exit.
5. Complete process exit/parent notification. If group exit was requested and
   not already marked, mark it and send SIGKILL to surviving sibling threads.
6. Return to the kernel continuation; the trap loop stops before user reentry.
   `check_signals` rejects an already exiting thread before touching its signal
   queue or constructing a user frame, including the next signal-loop iteration
   after a fatal signal itself called `do_exit`.

## Concurrency and resource release

This crate creates no global process registry or additional lifecycle lock.
`kprocess` serializes membership, parent notification and runtime owner slots.
Objects taken out of owner slots are destroyed outside the slot locks.
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
128 plus the signal number and does not write a core image. PI-tagged robust
entries are skipped. Cleanup errors are generally logged or ignored so teardown
can continue; these are current limitations, not complete Linux compatibility.
