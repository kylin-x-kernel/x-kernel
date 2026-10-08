# ksyscall design

## Purpose

`ksyscall` is the X-Kernel syscall adapter crate. It:

- dispatches on syscall number;
- decodes Linux ABI arguments;
- performs user-pointer `copyin`/`copyout`;
- validates flags, scalars, and struct shape at the syscall boundary;
- forwards calls to the real resource owners.

`ksyscall` owns no long-lived state, invariants, or lifetimes behind the
syscalls; those semantics belong to the respective owner crates.

## Background

Historically some syscall implementations were grouped by API name — for
example `fs/` collected VFS path operations together with `timerfd`,
`eventfd`, and `pidfd`, which do not share a resource boundary. The current
refactoring principles are:

- `ksyscall` keeps adapters only;
- resource semantics return to their owners;
- directory organization follows owner semantics, not historical API
  categories.

## Scope

```text
core/ksyscall/
├── Cargo.toml
├── src/
│   ├── lib.rs
│   ├── dispatch.rs
│   ├── ipc/          (eventfd, pipe)
│   ├── io_mpx/       (epoll, poll, select)
│   ├── sync/         (futex, membarrier)
│   ├── sys.rs
│   ├── arch/         (mod, riscv_hwprobe)
│   ├── task/         (clone, clone3, cpu_time, credentials, ctl, execve,
│   │                  exit, ids, job, limits, pidfd, rusage, sched,
│   │                  signal, thread, umask, wait)
│   ├── time/         (itimer, posix_timer, queries, sleep, timerfd)
│   └── vfs/
└── docs/             (design.md, security.md)
```

## Architecture

```text
user trap / arch syscall entry
    │
    v
ksyscall::handle_syscall
    │ decode sysno + ABI arguments
    ├─ vfs adapter  ───────────> posix-fs / kvfs
    ├─ ipc adapter  ───────────> kfd_objects::{EventFd, PipeObject}
    ├─ time adapter ───────────> khal time sources / kprocess CPU-time state / kfd_objects::TimerFd
    ├─ task adapter ───────────> kprocess / posix-process / kcred
    ├─ io_mpx adapter ─────────> kfd_objects::Epoll
    ├─ sync adapter ───────────> kfutex / kprocess
    └─ misc adapter ───────────> posix-mm / posix-net / ...
```

## Design principles

1. `ksyscall` owns ABI adaptation only, never resource state.
2. A syscall's directory placement follows the resource owner it finally
   routes to.
3. `copyin`/`copyout`, flag validation, and compat branches belong to the
   adapter layer.
4. State machines, invariants, and lifetimes of resource objects stay in
   owner crates.
5. Syscalls with similar names but different owners are not merged into one
   directory.
6. Each syscall number decodes exactly the argument count its ABI defines;
   registers left unused by older ABIs must not be read as extension flags.
7. When a process-control syscall replaces the user execution context, it
   must build a fresh register state for the target architecture's ELF
   entry ABI instead of inheriting the old syscall argument registers.
   After `execve` loads the new image it calls
   `UserContext::reset_for_exec()` before rebuilding IP/SP/TLS, zeroing all
   general registers and syscall-restart state; otherwise the x86_64 static
   glibc entry would see a stale `rdx` (old `envp`, interpreted as
   `rtld_fini` → SIGSEGV on exit), and aarch64 `x2` / riscv/loongarch64
   `a2` would keep the old `envp`.

## Exec image commitment

`task/execve.rs` copies pathname, argv and envp before entering the private
`exec_owned` body. Both the syscall and focused kernel tests use this body;
eligibility, path lookup and credential-aware loading are not duplicated.
Preparation owns the request and process metadata without locking or clearing
the old MM. Every exec creates a fresh user-empty MM. The current-only kprocess
replacement helper publishes a coherent binding and asks ktask to activate its
saved and hardware roots; independent CLONE_VM peers keep their old MM.
Preparation and replacement precheck errors return normally. Successful exchange
is the commitment boundary: old registrations reset immediately, then a fallible
block installs the image and applies metadata. All its temporary MM owners/locks
drop before fatal SIGSEGV cleanup. There is no rollback to the old user image.
Credential, TEE-session, IPC and context setup retain their success-tail roles.

Regression coverage lives in `src/task/tests/exec_lifecycle.rs` and
`src/task/tests/exit_tail.rs`. It checks the commitment boundary, independent
VM-sharing peer survival, retained-image identity, and exit-tail root lifetime
across scheduling and CPU migration. Request-local unittest probes exercise
post-commit failures without exposing a production fault-injection interface.
Synthetic ELF fixtures verify installation, not execution of user instructions.
The external harness `exec-mm-semantics` suite covers runnable-image behavior:
successful exec, ordinary preparation errors, and VM-sharing peer isolation.

## Owner alignment

- `vfs/`: path and VFS syscalls; owners in `posix-fs` / `kvfs`.
- `ioctl`: `dispatch.rs` first asks `posix-net::handle_net_ioctl` for its
  exact SIOC* list, then falls back to `posix-fs::sys_ioctl`. It does not
  filter by the `0x89xx` ioctl type space; socket file vtables do not yet
  override `ioctl`.
- `ipc/pipe.rs`: `pipe2`; owner `kfd_objects::PipeObject`.
- `ipc/eventfd.rs`: `eventfd2`; owner `kfd_objects::EventFd`.
- `sys.rs`: `sethostname` routes to the current UTS namespace. `reboot`
  validates the Linux magic/command then routes to `khal::power` endpoints:
  `HALT` takes `halt()` (stop all CPUs, keep power), `POWER_OFF` takes
  `power_off()` (platform power removal); both stop other CPUs through the
  SMP-stop hook first, while fs-sync/device-shutdown housekeeping is left
  to a future orderly-shutdown supervisor; `SW_SUSPEND` takes
  `suspend_to_ram()` (non-terminal: S3 via the platform sleep proxy, or a
  platform error back to the caller when absent/refused).
- `arch/`: architecture-specific system-info/control adapters (`arch/mod.rs`
  organizes per-architecture submodules via `cfg`, `lib.rs` declares the
  top-level `mod arch;`, and `sys.rs` only re-exports `crate::arch::*`),
  avoiding stray top-level per-architecture files.
  - riscv64 `riscv_hwprobe` parses the Linux `struct riscv_hwprobe`, user
    cpusets, and `RISCV_HWPROBE_WHICH_CPUS`, then queries `kcpu` for each
    present logical CPU's RISC-V capability snapshot; `ksyscall` only does
    ABI copyin/copyout and cpuset boundary handling, while key semantics,
    aggregation, and matching live in `kcpu`'s hwprobe helper.
  - riscv64 `riscv_flush_icache` reads the third argument `flags` (Linux
    ABI is 64-bit: kernel `uintptr_t`, libc `unsigned long`, so it is
    handled as `usize` with all 64 bits reserved-checked — note the
    difference from `riscv_hwprobe`'s 32-bit `unsigned int flags`):
    `SYS_RISCV_FLUSH_ICACHE_LOCAL` flushes only this hart
    (`karch::flush_icache_all_local()`, a single `fence.i`), other reserved
    bits return `EINVAL`; without LOCAL, `karch::flush_icache_all()`
    broadcasts an IPI to all harts so self-modifying code stays visible
    after task migration.
- `time/timerfd.rs`: `timerfd_*`; owner `kfd_objects::TimerFd`.
- `time/queries.rs`: `time` / `clock_gettime` / `gettimeofday` /
  `clock_getres` / `clock_settime` / `settimeofday`; owners in `khal` clock
  sources, `ktime` realtime association (including `set_realtime`), and
  `kprocess` CPU-time queries.
- `time/sleep.rs`: `nanosleep` / `clock_nanosleep`; owners in the `ktask`
  sleep runtime and `khal` clock queries.
- `time/itimer.rs`: `getitimer` / `setitimer` / `alarm` (x86_64 ABI); owner
  `ProcessTimerManager` legacy interval-timer state. `alarm` is a one-shot
  `ITIMER_REAL` adapter that reports the previous remaining seconds.
- `time/posix_timer.rs`: `timer_create` / `timer_gettime` / `timer_settime`
  / `timer_delete` / `timer_getoverrun`; owners in `ProcessTimerManager`
  POSIX timer state and the `kprocess` timer delivery runtime.
- `io_mpx/`: `select` / `pselect6` / `poll` / `ppoll` / `epoll_*`; owners
  in `kfd_objects::Epoll` and the generic `FileLike` poll interface.
- `task/pidfd.rs`: `pidfd_*`; owner `kprocess::PidFd`.
- `task/credentials.rs`: `get*id` / `set*id` / `getgroups` / `setgroups`;
  owners in `kprocess` current-credential helpers and the `kcred` model.
- `task/ctl.rs`: `prctl` `PR_GET_KEEPCAPS` / `PR_SET_KEEPCAPS`; owners in
  the `kprocess` credential publication path and `kcred` securebits.
- `task/ids.rs`: `getpid` / `getppid`; owner `kprocess` current thread and
  parent relations.
- `task/job.rs`: `getsid` / `setsid` / `getpgid` / `getpgrp` / `setpgid`;
  owner `kprocess` process-group and session state.
- `task/thread.rs`: `gettid` / `set_tid_address` / `arch_prctl`; owners in
  `kprocess` current-thread state and architecture thread context.
- `task/signal.rs`: `rt_sigprocmask` / `rt_sigaction` / `rt_sigpending` /
  `kill` / `tkill` / `tgkill` / `rt_sigqueueinfo` / `rt_tgsigqueueinfo` /
  `rt_sigreturn` / `rt_sigtimedwait` / `rt_sigsuspend` / `pause` / `sigaltstack` /
  `signalfd4`; owners in `kprocess` per-thread signal state, the `ksignal`
  signal model, and `kfd_objects::Signalfd`. `pause` (x86_64 syscall 34) and
  `rt_sigsuspend` share the internal `wait_for_deliverable_signal` helper.
  Only `rt_sigsuspend` replaces the mask and saves the old mask for signal-frame
  restoration; `pause` changes neither mask. The helper checks unblocked pending
  signals before registering an interrupt waiter, rechecks when registration
  races an interrupt, and blocks through `block_on` while waiting. Registration
  failure yields and retries without exposing `ENOMEM`. Both return
  `ERESTARTNOHAND`: handler delivery changes this to `EINTR` regardless of
  `SA_RESTART`, while no handler delivery restarts the syscall. The supported
  aarch64, riscv64, and loongarch64 syscall tables have no `pause` entry.
- `task/cpu_time.rs`: `times`; owners in `kprocess` process CPU-time
  accounting and `khal` clock queries.
- `task/rusage.rs`: `getrusage`; owner `kprocess` CPU-time sampling state.
- `task/limits.rs`: `getrlimit` / `setrlimit` / `prlimit64`; owner the
  process resource/rlimit state held by `kprocess::ProcessRuntime`.
- `task/umask.rs`: `umask`; owner the `kprocess::ProcessRuntime` file
  creation mask.
- `task/sched.rs`: `sched_yield` / `sched_*affinity` / `sched_*scheduler` /
  `getcpu` / `getpriority` / `setpriority`; owners in `ktask` scheduling
  interfaces, `kprocess` process/thread state, and `khal` CPU queries.
  `sched_getscheduler` and `sched_getparam` resolve positive published TIDs
  through a separate read-only helper; zero selects the current thread and
  negative IDs return EINVAL. The two policy/parameter setters retain
  `kprocess::scheduler::target_task`: zero and the caller's process PID select
  the caller; other positive process PIDs select a representative task;
  non-process worker TIDs and negative IDs remain ESRCH.
  `PRIO_PROCESS` selects one task by TID; `PRIO_PGRP` and `PRIO_USER`
  iterate every published task — a process representative thread must not
  stand in for per-thread nice or real UID. `setpriority` compares the
  caller's effective UID with the target's real/effective UIDs; root
  currently approximates `CAP_SYS_NICE`, and unprivileged callers cannot
  lower nice (raise priority). `sched_setaffinity` requires the same
  caller-euid vs target ruid/euid match or root (`CAP_SYS_NICE`
  approximation); the error order is ESRCH → EPERM → EINVAL (empty mask);
  `ktask::set_task_affinity` then migrates a queued task off its CPU,
  only changes the mask when no CPU is occupied, and returns EBUSY when
  the task cannot migrate.
- `sync/futex.rs`: `futex` / `get_robust_list` / `set_robust_list`;
  compound operations (`REQUEUE` / `CMP_REQUEUE` / `WAKE_OP`) resolve both
  keys under a single `address_space` lock; the robust-list walk lives in
  `posix/process`. An untimed wait interrupted by a signal returns
  `ERESTARTSYS`. A timed wait returns `ERESTART_RESTARTBLOCK` and records
  the monotonic deadline in a futex payload owned by this crate; `kprocess`
  only stores that payload in an opaque current-thread slot. A delivered
  handler turns that code into `EINTR` and drops the slot (`SA_RESTART` does
  not restart timed futex wait). No handler, including default `SIGCONT`,
  rewrites the syscall to `restart_syscall` and leaves the slot armed.
- `task/restart.rs`: `restart_syscall` takes the futex payload from the
  current thread. An empty or unrecognized slot returns `EINTR`.

## Container-related adapters

Until unified user-namespace capability authorization exists, `clone` /
`clone3` return `ENOSYS` for `CLONE_NEWCGROUP` instead of improvising
namespace-creation permission with UID checks. The exec credential path
does not yet enforce `no_new_privileges` promotion constraints, so
`PR_SET_NO_NEW_PRIVS` returns `ENOSYS` and `PR_GET_NO_NEW_PRIVS` reports
unset. seccomp likewise has no execution engine yet: `seccomp(2)` and
`PR_SET_SECCOMP` return `ENOSYS` rather than faking success.

## Non-goals

`ksyscall` does not:

- keep internal state of fd-backed objects;
- hold shared objects such as `ProcessRuntime`, address spaces, or VFS
  nodes;
- implement path resolution, signal state machines, timer state machines,
  or pipe buffer behavior;
- provide a convenient catch-all owner abstraction.
