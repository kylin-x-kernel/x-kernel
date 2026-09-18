# posix-process — Design

## Purpose and scope

This crate orchestrates user-thread execution, PID 1 construction and exit.
The complete implementation is `src/lib.rs`, `src/init_process.rs` and
`src/runtime.rs`. `kprocess` owns process identities, runtime resources,
publication and process relationships; `kexec` owns executable loading;
`ksyscall` owns syscall decoding. This crate supplies the trap loop and orders
calls to those owners rather than duplicating their registries.

## Architecture and entry points

Boot code calls `spawn_init_process` with argv, environment, a syscall dispatcher
and an exit callback. It allocates root PID 1, prepares and loads an `ExecRequest`,
creates the process/thread runtime, installs console stdio, seeds the task's page
table root, and publishes with `publish_user_task(...).commit(...)`. It creates a
fresh task and leaves the bootstrap thread intact. It does not acquire a
controlling terminal; session/TTY acquisition belongs to later userspace.

Clone adapters use `new_user_task` with matching `PidHandle` and `Thread` values.
The returned task is still unpublished: the caller completes process publication
before activation. Its closure enters `run_user_thread_loop`, which calls the
provided dispatcher for syscalls and `MmSpace::handle_page_fault` for faults.
`BusError` maps to SIGBUS; other unresolved faults map to SIGSEGV. Retryable faults
retry user execution after checking preemption. Signal delivery is delegated to
`Thread::signal_manager`; default actions call `do_exit`.

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

The loop alternates user execution, trap handling, kernel CPU accounting and
signal checks. The loop clears the previous trap's interrupt flag before the
normal pending-signal check. It must not clear that flag after checking signals:
a concurrent group-exit sender could otherwise queue SIGKILL and set the flag
in between, then lose its notification before the target enters a futex wait.
`SkipSignalCheckOnce` suppresses the normal post-syscall signal check after
signal return. Before reentering userspace, the loop polls CPU timers and checks
preemption; an interrupt raised since the clear causes another signal check.
Signals arriving after that final check retain their interrupt notification for
the next interruptible wait or trap.

`do_exit` performs the following order:

1. Traverse the registered robust-futex list, then clear `clear_child_tid` and
   wake its futex if the write and key resolution succeed.
2. Release optional per-thread TEE state and detach cgroup membership.
3. Remove the thread from process membership/publication, close its CPU-accounting
   interval and accumulate final thread CPU time.
4. For the last thread, detach the mm owner, clear SysV shm accounting, release
   optional TEE private state, then detach files, filesystem context and namespaces.
   With TIPC enabled, close process-local handles before publishing process exit.
5. Complete process exit/parent notification. If group exit was requested and
   not already marked, mark it and send SIGKILL to surviving sibling threads.
6. Mark the current thread exited; the trap loop subsequently stops.

## Concurrency and resource release

This crate creates no global process registry or additional lifecycle lock.
`kprocess` serializes membership, parent notification and runtime owner slots.
Objects taken out of owner slots are destroyed outside the slot locks.
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
