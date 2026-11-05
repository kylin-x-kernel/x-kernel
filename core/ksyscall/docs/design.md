# ksyscall design

## Network Namespace Owner Preparation

Clone prepares an unpublished `knet::netns::PreparedStack` for NEWNET after
validation and before process-fork preparation creates child relationships.
Afterward installation performs no fallible device-owner allocation. Unshare
materializes the prepared proxy's network stack before applying that proxy;
both setns paths validate their replacement proxy before stack preparation and
install it only on success. Allocation uses KERNEL in reclaim-safe syscall task
context, outside process, filesystem-context and namespace-registry locks.
Existing fd-table detach ordering and other allocations are not changed.

## Clone Entry Preparation

Clone prepares its user entry with KERNEL after flag, namespace capability and
exit-signal validation, but before process or sibling-thread preparation. This
is sleepable syscall task context with no cgroup transaction or reclaim-dependent
lock held. The current-task/thread handles retain owners, not lock guards.
NoMemory propagates before child identities and relationships are created.
The prepared entry is transferred to `new_user_task` after clone preparation,
without adding another callback allocation beyond the rollback boundary.
Stack and other legacy allocations are unchanged.

## Test Layout

Guest cases and exec/exit-tail fixtures live in `ut-tests/`, mounted at their
original scopes through `unittest::path_mod_test!`. Module names, architecture
gates and user/serial registrations remain unchanged. The exit-tail handshake
itself is not changed by this directory migration.

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
│   │                  exit, ids, job, limits, namespace, pidfd, rusage,
│   │                  sched, seccomp, signal, thread, umask, wait)
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
    ├─ task adapter ───────────> kprocess / posix-process / kcred / knet
    ├─ io_mpx adapter ─────────> kfd_objects::Epoll
    ├─ sync adapter ───────────> kfutex / kprocess
    └─ misc adapter ───────────> posix-mm / posix-net / ...
```

The dispatch entry runs `evaluate_seccomp` before the `match` on `Sysno`: the
current thread's seccomp chain (owned by `kprocess::Thread.seccomp()`) either
allows the syscall, forces an errno, kills the process/thread with `SIGSYS`
(`KILL_PROCESS`/`KILL_THREAD`, and fail-closed `TRAP`/unrecognised actions),
or returns `ENOSYS` for the unimplemented `USER_NOTIF`/`TRACE` actions.
The `sys_seccomp` adapter reads the user `sock_fprog` field-wise, builds a
`kseccomp::SeccompFilter`, and installs it on the thread; `TSYNC` first checks
every sibling can adopt the caller's chain without becoming more permissive.

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

Regression coverage lives in `ut-tests/task/tests/exec_lifecycle.rs` and
`ut-tests/task/tests/exit_tail.rs`. It checks the commitment boundary, independent
VM-sharing peer survival, retained-image identity, and exit-tail root lifetime
across scheduling and CPU migration. Request-local unittest probes exercise
post-commit failures without exposing a production fault-injection interface.
The empty-MM activation and partial-install probes each wait on a dedicated peer
acknowledgement queue with a five-second deadline. Each stays blocked until the
parent verifies the boundary, publishes its acknowledgement and wakes the child,
avoiding two periodic polls missing the blocked interval. The partial-install
observer captures request-local handshake state without coupling the loader to
the scheduler. Migration and memory checks still run after acknowledgement.
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
- `task/namespace.rs`: `unshare` and `setns`. `unshare` decodes the flag set
  (namespace bits plus `CLONE_FS`/`CLONE_FILES`), rejects unknown bits with
  `EINVAL`, then runs `NsProxy::prepare_unshare` (privilege, shared-fs, and
  unsupported-kind checks) and commits via `Process::apply_prepared_unshare`.
  `setns` splits into the `NsFd` channel (nstype must be zero or match the
  fd type) and the `PidFd` channel (nstype is an OR mask resolved against one
  snapshot of the target bundle, Linux 5.8+ semantics), and commits via
  `Process::apply_setns`. User/Cgroup swaps are `EOPNOTSUPP` (no
  pseudo-isolation); a Net swap is accepted and the syscall path materializes
  the target namespace's protocol stack through `knet::netns::ensure_stack`.
  Both channels sit behind the `has_sys_admin` gate.
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
- `task/ctl.rs`: `capget` / `capset`, `prctl` `PR_GET_KEEPCAPS` /
  `PR_SET_KEEPCAPS`, `PR_CAPBSET_READ` / `PR_CAPBSET_DROP`, and the
  `PR_CAP_AMBIENT` subcommands; owners in the `kprocess` credential
  publication path, `kprocess::capability` target resolution, and `kcred`
  capability sets, bounding set, transition validation and securebits.
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
  signal model, and `kfd_objects::Signalfd`. `signalfd4` validates the flag
  bits but ignores `SFD_CLOEXEC`/`SFD_NONBLOCK` on the descriptor-update form
  (`fd != -1`), as Linux only consults them when it allocates a new descriptor;
  sd-event widens an existing signalfd with the same flags. The queue variants use Linux's
  exact argument count and do not validate a trailing `sigsetsize`. `pause` (x86_64 syscall 34) and
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
  `getcpu` ignores its obsolete third cache argument without reading it.
  Null CPU/node outputs are allowed; non-null outputs use the existing user-copy wrappers.
  Scheduler policy/parameter getters and setters resolve positive published TIDs,
  including workers, and read or write the live `SchedAttr`. Getters use
  `ktask::sched_getattr`. Zero selects the caller; a process
  PID denotes its leader TID and never a representative worker. Missing or
  unpublished TIDs return ESRCH. A negative pid, a null `sched_param` /
  `sched_attr`, nonzero `sched_getattr` flags, or a negative policy in
  `sched_setscheduler` or `sched_setattr` is EINVAL before lookup.
  `sched_setscheduler` also copies `sched_param` before lookup, so a bad
  pointer is EFAULT rather than ESRCH.
  `sched_setscheduler`, `sched_setparam`, and `sched_setattr` copy user memory
  and credential snapshots before the run-queue lock, then apply the live
  attribute through `ktask::sched_update_attr` so the read and the store share
  one critical section.
  `sched_setscheduler` admits Fair, FIFO, and RR. `sched_setattr` also admits
  `SCHED_DEADLINE` (a zero period is stored as the deadline). Realtime and
  Deadline policies require effective `CAP_SYS_NICE` in addition to the owner
  match. Every setter applies Linux `cap_safe_nice()`: unless the caller has
  effective `CAP_SYS_NICE`, the target's permitted capability set must be a
  subset of the caller's permitted set. Otherwise the call returns EPERM before
  changing the task. An illegal priority or deadline triple is
  EINVAL before that EPERM. `SCHED_RESET_ON_FORK` is stored on the live
  attribute and returned in `sched_getscheduler` / `sched_getattr`. An
  unprivileged owner may set the flag; clearing it requires effective
  `CAP_SYS_NICE` and otherwise returns EPERM without changing the attribute.
  `sched_setparam` does not carry the flag and leaves it unchanged. Clone
  copies that attribute with `SchedAttr::for_fork` and installs it before the
  child is published. An RT or Deadline parent with the flag produces a
  `SCHED_NORMAL` child at nice 0; a Fair parent drops only a negative nice.
  `sched_getattr` reports `sched_runtime` as 0 except for Deadline, and
  `sched_setattr` ignores that field for every other policy. Bytes past the
  kernel `sched_attr` are cleared.
  `sched_get_priority_min` / `max` and `sched_rr_get_interval` (including the
  time64 number where the architecture has one) follow the same policy table:
  FIFO/RR priorities are 1 and 99, and only `SCHED_RR` has a finite quantum.
  `PRIO_PROCESS` selects one task by TID; `PRIO_PGRP` and `PRIO_USER`
  iterate every published task — a process representative thread must not
  stand in for per-thread nice or real UID. `setpriority` compares the
  caller's effective UID with the target's real/effective UIDs; effective
  `CAP_SYS_NICE` bypasses the owner match, and callers without it cannot
  lower nice (raise priority). After that EACCES check, unprivileged callers
  must also satisfy `cap_safe_nice()` against the target's permitted set.
  `sched_setaffinity` requires the same caller-euid vs target ruid/euid match
  or effective `CAP_SYS_NICE`, plus the same permitted-set containment;
  the error order is ESRCH → EPERM → EINVAL (empty mask);
  `ktask::set_task_affinity` then migrates a queued task off its CPU,
  only changes the mask when no CPU is occupied, and returns EBUSY when
  the task cannot migrate.
- `sync/futex.rs`: `futex` / `get_robust_list` / `set_robust_list`;
  only non-PI operations are implemented. `FUTEX_LOCK_PI` is rejected with
  `EOPNOTSUPP` before the realtime gate and address resolution; the other PI
  opcodes (`UNLOCK_PI`, `TRYLOCK_PI`, `WAIT_REQUEUE_PI`, `CMP_REQUEUE_PI`,
  `LOCK_PI2`) remain `ENOSYS`. Linux's `CONFIG_FUTEX_PI=n` returns `ENOSYS`
  for all of them. musl's `pthread_mutexattr_setprotocol` returns the
  `LOCK_PI` probe errno to PulseAudio, which accepts only `EOPNOTSUPP` as
  "PI unavailable" and otherwise aborts. Only `LOCK_PI` may carry that
  errno: glibc probes `UNLOCK_PI` and treats only `ENOSYS` as missing PI
  support, aborting on an unexpected errno. Unknown non-PI opcodes remain
  `ENOSYS`. Compound operations (`REQUEUE` / `CMP_REQUEUE` / `WAKE_OP`)
  resolve both keys under a single `address_space` lock before `WAKE_OP`
  decodes its operation. Key resolution still reports a misaligned address
  as `EINVAL` before that write fault. A non-private call then write-faults
  `uaddr2`, so an out-of-range address with an illegal encoding returns
  `EFAULT` or `ENOMEM`. After a
  nofault fault the syscall write-faults, honors a pending preemption, and
  resolves shared keys again. `EAGAIN` retries the same way without another
  write fault. The robust-list walk lives in
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

`unshare`/`setns` implement the namespace system calls over `kns` two-phase
validation and `kprocess` commit (see above). `clone`/`clone3` now accept
`CLONE_NEWPID` (the child is allocated in a fresh child PID namespace) and
`CLONE_NEWNS`/`CLONE_NEWUTS`/`CLONE_NEWIPC`/`CLONE_NEWNET` (a fresh network
namespace whose protocol stack is materialized by `knet::netns::ensure_stack`),
while `CLONE_NEWCGROUP`, `CLONE_NEWUSER`, and `CLONE_NEWTIME` still return
`ENOSYS` from the namespace-flag validator instead of improvising namespace
creation without isolation. A thread clone cannot switch namespaces — the namespace
bundle hangs off the process — so `CLONE_THREAD` combined with any namespace
flag is rejected with `EINVAL` (Linux `copy_process()`), and every namespace
flag on any clone is gated on the caller's privilege before the thread or
process branch is chosen. User namespaces are not implemented, so both syscalls
require effective `CAP_SYS_ADMIN`: `unshare` through
`UnshareContext::privileged`, and `setns` before either channel dispatches —
joining any namespace without it is `EPERM`, standing in for Linux's per-type
`ns_install` capability checks. The unshare flag gate follows
`check_unshare_flags`: `CLONE_THREAD`/`CLONE_SIGHAND`/`CLONE_VM` are accepted
as no-ops for a single-threaded caller and rejected with `EINVAL` only for a
multithreaded one, and `CLONE_NEWNS` implies `CLONE_FS` as in
`ksys_unshare`, so callers never need the explicit bit. The pidfd `setns`
channel treats `nstype` as Linux's OR-combination mask: every requested slot
is validated against the target process and committed together, unsupported
kinds answer `EOPNOTSUPP`. `getpid`/`getppid` project the identity into the
PID namespace the task was created in (`task_active_pid_ns` semantics, so a
`CLONE_NEWPID` child reads 1 and its parent reads 0), `clone`/`fork` return
the child's number in the forking process's namespace, and signal PID
arguments (`kill`/`tgkill`/`sigqueue`) are translated through the caller's
namespace with a root-number fallback for root-keyed sources such as
/proc. Kernel-internal lookup stays root-keyed (see the known-limits note
in `process/kns/docs/design.md`).
`PR_SET_NO_NEW_PRIVS`
records the flag on the calling thread and `PR_GET_NO_NEW_PRIVS` reports it
back; seccomp install consults it. Exec models only the no-file-capability
credential transition: ordinary non-root exec clears effective/permitted, while
privileged root reconstructs permitted from the bounding/inheritable sets. With
`no_new_privs` or a filesystem context shared with another process, a growing
reconstruction or a group identity change enters the downgrade: a growing set is
intersected with the pre-exec permitted set and effective/group IDs are reset to
the real IDs (the latter requires `no_new_privs` or no pre-exec effective
`CAP_SETUID`). Exec admission samples
the cross-process `CLONE_FS` owner count and, when exclusive, sets `in_exec`
under the same filesystem lock; runtime adoption checks that flag atomically and
returns `EAGAIN`, so a child cannot attach between the sample and the credential
transition. There is no setuid/setgid or file-capability elevation; adding one
requires extending both the credential transition and this flag handling.

`seccomp(2)` is implemented: `SECCOMP_SET_MODE_STRICT` installs the fixed
`read`/`write`/`_exit`/`rt_sigreturn` allowlist, `SECCOMP_SET_MODE_FILTER`
validates and appends a classic-BPF filter (with `TSYNC` applying it to the
thread group only when that cannot remove a sibling's restriction), and
`SECCOMP_GET_ACTION_AVAIL` answers action-constant queries. The filter is
evaluated at syscall entry before dispatch. `PR_SET_SECCOMP` remains a
`prctl` alias for the same install path. `USER_NOTIF`, `TRACE`, and
`GET_NOTIF_SIZES` are not implemented: `GET_ACTION_AVAIL` reports them as
`ENOSYS`, and a filter selecting `USER_NOTIF`/`TRACE` makes the syscall fail
closed with `ENOSYS`.

Install-time validation mirrors Linux `seccomp_check_filter`: the length bound,
instruction encoding, and every jump target are checked before the filter is
installed, so a program the interpreter cannot run is rejected with `EINVAL`
instead of killing the task at its next syscall. The install gate returns
`EACCES` when the caller has neither `CAP_SYS_ADMIN` nor `PR_SET_NO_NEW_PRIVS`,
`PR_SET_SECCOMP` is the same install path with the mode as its argument, and a
strict-mode violation terminates the thread with `SIGKILL` (a filter kill stays
`SIGSYS`). The `arch` field reported to filters is the target's `AUDIT_ARCH_*`
(x86_64/aarch64/riscv64/loongarch64). See Linux's
[seccomp entry points](https://github.com/torvalds/linux/blob/v6.12/kernel/seccomp.c#L2064-L2127)
and [PR_SET_SECCOMP(2const)](https://man7.org/linux/man-pages/man2/PR_SET_SECCOMP.2const.html).

`PR_SET_PDEATHSIG`/`PR_GET_PDEATHSIG` store the parent-death signal on the
process identity.

`waitid` and `execveat` share the wait and exec machinery with `wait4` and
`execve`:

- `task/wait.rs` implements `wait4`/`waitid` over one scan (the
  `wait_reap::WaitChildSelector` built from `WaitPid`). `waitid` accepts
  `P_ALL`/`P_PID`/`P_PGID`/`P_PIDFD`; `P_PIDFD` resolves `id` as a pidfd
  file descriptor and waits on its target process. `WNOWAIT`/`WNOHANG`/
  WEXITED is honored by the shared machinery. wait4 copies its optional
  rusage from the waited child's stable CPU-time snapshot, including reaped
  descendants. Only ru_utime and ru_stime are supported; the remaining
  counters are zero, matching the current getrusage implementation. Status
  is copied before usage, after the shared scanner has consumed the child.
  Copy-out failure therefore does not undo reaping. A WNOHANG result of
  zero leaves both outputs untouched. The raw waitid usage output remains
  unimplemented.
- `task/execve.rs` implements `execve`/`execveat` through a single
  `run_exec`. `execveat` accepts `AT_EMPTY_PATH` (exec the file already
  open on `dirfd`, e.g. a memfd) and `AT_SYMLINK_NOFOLLOW` (currently
  treated as follow). A multi-threaded `exec`, or one whose thread is not the
  group leader, runs `de_thread` (see `posix-process`) to replace the whole
  thread group instead of failing; concurrent `exec` serializes on the process
  `exec` gate.

## Non-goals

`ksyscall` does not:

- keep internal state of fd-backed objects;
- hold shared objects such as `ProcessRuntime`, address spaces, or VFS
  nodes;
- implement path resolution, signal state machines, timer state machines,
  or pipe buffer behavior;
- provide a convenient catch-all owner abstraction.

## Scheduler syscall thread targets

Scheduler parameter queries and updates resolve published thread IDs. This allows pthread APIs to address non-leader threads; zero selects the caller, and positive process IDs select only the leader TID. There is no process representative fallback.

Scheduler ownership checks use the caller thread's subjective credentials and
the target thread's objective credentials. Raw `setuid`/`setresuid` update only
the calling thread through `update_current_cred` and `commit_creds`; they do not
broadcast changes to a thread group. Linux has the same per-thread kernel
model; POSIX-wide credential changes are synchronized by libc/NPTL wrappers
using signals, as documented by
[setuid(2), C library/kernel differences](https://man7.org/linux/man-pages/man2/setuid.2.html)
and [nptl(7)](https://man7.org/linux/man-pages/man7/nptl.7.html).
A raw syscall can intentionally leave same-group threads with different UIDs.
The scheduler gate must then use the concrete target credentials, without a
same-group exemption. The published-worker regression invokes actual
`sys_setuid`, verifies the caller changed while its peer retained its IDs, and
checks both setters return EPERM without modifying the peer's metadata.

## Capability syscall ABI and ownership

`capget` and `capset` accept Linux capability ABI versions 1 (`0x19980330`),
2 (`0x20071026`) and 3 (`0x20080522`). Versions 2 and 3 use two consecutive
`__user_cap_data_struct` objects: the low word covers capabilities 0–31 and the
high word covers 32–63. Version 1 predates the upper word and copies exactly one
12-byte `__user_cap_data_struct`; capabilities above 31 are invisible to a v1
caller. `posix-types` supplies the fixed `CapUserData` representation and its
`UserRead`/`UserWrite` implementations; `ksyscall` converts each word through
`CapSet::from_words`/`low_word`/`high_word` without reimplementing bit ranges
and narrows only the user copy for version 1.

The version word is read and validated before the data pointer is touched. An
unknown version rewrites only the version word with version 3 and returns
`EINVAL`; the header PID and data array are not accessed. `capget` alone keeps
Linux's legacy null-data probe: after version validation it returns 0 without
reading the PID, and a null-data request with an unknown version still rewrites
the version and returns 0 rather than `EINVAL`.

For a non-null `capget` request, a negative PID returns `EINVAL`. PID 0 and
every positive PID, including the caller's own thread ID, are resolved by
`kprocess::capability::target_capabilities` through the caller's PID namespace.
The reported state is always the target task's objective real credential, which
matches Linux `cap_capget()` reading `__task_cred()` even when the target is
`current`. A positive PID may be a thread ID or thread-group ID, matching Linux
`find_task_by_vpid()`; a missing target returns `ESRCH`. If a thread-group
leader has exited while siblings remain, the group ID still reports the
leader's retained objective credentials, matching Linux's unreaped zombie leader,
rather than a sibling thread's per-thread credentials.

For `capset`, only PID 0 or the caller's current thread ID is accepted; every
other target, including a negative PID, returns `EPERM` before the data pointer
is read. "Current thread ID" covers both numbers that identify the caller in
this kernel's PID model: the active-namespace number reported by `getpid(2)` for
a namespace leader and the root-projected number currently reported by
`gettid(2)`. Member-thread identities are allocated with only a root projection,
so accepting both spellings of the caller is required until those identities
carry a full namespace chain; neither spelling can select another task. The
data words are masked with Linux `CAP_VALID_MASK` (represented by
`CapSet::FULL`) before transition validation, so unknown bits are dropped rather
than installed or used to reject an otherwise valid request. A successful
`capset` always updates only the current task and relies on
`Cred::set_capabilities` for the Linux transition rules: inheritable growth is
bounded by old inheritable union old permitted unless effective `CAP_SETPCAP`,
and always by old inheritable union the bounding set; permitted never grows; and
effective must remain within permitted.

`PR_CAPBSET_READ` validates the capability number and queries the same
`Cred::cap_bounding` set returned by the capability APIs. `PR_CAPBSET_DROP`
passes the full-width argument to `Cred::drop_bounding_cap`, which applies the
`CAP_SETPCAP`-before-range policy so an oversized value cannot truncate to a
valid capability. `PR_CAP_AMBIENT` validates the subcommand and reserved
arguments, then delegates to the credential model: `IS_SET` reports membership,
`RAISE` requires the capability in both permitted and inheritable and
`SECURE_NO_CAP_AMBIENT_RAISE` clear, and `LOWER`/`CLEAR_ALL` remove membership.
A successful `capset` masks ambient with the new permitted and inheritable sets.
File capabilities and setuid/setgid executable transitions are not implemented,
so `capget` never invents executable-derived privilege. Exec still applies
Linux's no-file-capability rule: permitted starts empty, privileged root
reconstructs it from the bounding and inheritable sets when `SECURE_NOROOT` is
clear and the real or effective UID is 0; ambient is merged into permitted after
the growth clamp and supplies effective when privileged root does not. An
effective GID outside the old filesystem/supplementary groups clears ambient.
When `no_new_privs` is set or another process shares the caller's filesystem
context and reconstruction would grow permitted or the group identity changed,
a growing permitted set is intersected with the pre-exec set; effective/group
IDs reset to real IDs when `no_new_privs` is set or the pre-exec credential
lacks effective `CAP_SETUID`.

## Resource-limit syscall copy ordering

`prlimit64` reads the optional new-limit object before publishing any old-limit
output, so aliased input and output buffers retain the caller's requested
update. Resource validation also occurs when both pointers are NULL. A rejected
input/update does not overwrite the output buffer; after a successful update,
an output copy fault returns EFAULT without rolling back the applied limit,
matching Linux copy ordering. Legacy getrlimit/setrlimit still require their
respective output/input buffers. The adapter converts the copied input to a
semantic `krlimit::Rlimit` and calls the owner's `update_rlimit_and_get_old` once.
Query-only requests use the owner's checked read snapshot. For an update, one
limits write guard covers validation against the current hard cap, old-value
capture and replacement. The returned pair is therefore the one this update
actually replaced despite concurrent updates. User copying remains outside
either guard, and hard-limit authorization policy is unchanged.

Linux `kernel/sys.c` copies a non-NULL `prlimit64` input before looking up the
PID, so EFAULT takes priority over ESRCH. Legacy `getrlimit` checks the resource
before its output copy; `setrlimit` copies its input before resource validation.
Their differing error priority is intentional, rather than a shared-helper
symmetry rule. See [Linux v6.12 kernel/sys.c](https://github.com/torvalds/linux/blob/v6.12/kernel/sys.c).
The user-task regressions exercise a genuinely aliased `TestUserValue`, verify
the returned old pair and applied new pair, and restore the original soft limit
after framework assertion failures. A separate user test proves an unreadable
input before checking PID/resource error priority.

The `prlimit_rejected_update_preserves_aliased_buffer` user/serial regression
requests cur=2, max=1 with the same input/output buffer and checks InvalidInput
without replacing that invalid pair with the old limit. This distinguishes
update-before-output from the incorrect input/output/update ordering.

The output-fault regression proves the destination is unwritable, submits a
valid soft-limit change, and verifies EFAULT leaves the change applied before
restoring the original pair. A separate user/serial fixture captures the current
resource owner and starts eight kernel tasks against that same owner. Distinct
soft values at a fixed hard cap must form one complete transition chain from
the returned old values to the final query. All tasks join and the original
soft limit is restored before assertions; completion order and timing are not
used as the oracle. This tests owner concurrency, not user-thread syscall setup.

## Time-query argument compatibility

`gettimeofday` accepts a NULL timeval pointer and returns zero without reading
the wall clock or copying output. A non-NULL pointer keeps the existing timeval
conversion and user-copy path. `clock_settime` accepts only CLOCK_REALTIME;
CLOCK_REALTIME_COARSE remains query-only, as in Linux. Unsupported clock IDs
return EINVAL before reading the supplied timespec or reaching the privileged
wall-clock setter. CLOCK_REALTIME continues to use the existing pointer, range
and privilege checks. This does not change coarse-query resolution or timezone
handling.

The query-only clock regression runs in a fresh user task, first demonstrates
that its non-NULL input fails the real user-copy path, then checks EINVAL for
both that address and NULL. Test assertions use the unittest framework so a
failed check reports a test failure instead of panicking the kernel.

## Poll result-array compatibility

The requested event mask ignores unrecognized bits, matching Linux's
`do_pollfd` filtering rather than rejecting the entire call. `POLLERR` and
`POLLHUP` are reported without being requested; `POLLRDHUP` is returned only
when the caller requested it. A stream peer closing only its write half must
therefore not make an `events = 0` entry ready. The poll result regressions
cover unknown bits both alone and alongside `POLLIN`, requested and unrequested
half-close readiness, and unconditional full-hangup reporting.

`poll` and `ppoll` clear each copied entry's `revents` before descriptor lookup.
All negative descriptors are ignored. Invalid nonnegative entries contribute
`POLLNVAL` and one ready entry, while valid entries are still polled during the
same call. This prevents stale ignored events and invalid entries from hiding
valid readiness. Duplicate descriptor entries are counted independently.
An invalid entry makes the call ready without waiting for an unready valid
entry; timeout, signal-mask and valid-event masking paths otherwise stay intact.
Five user-task regressions cover ignored negatives, mixed ready/invalid entries,
unready/invalid entries, duplicate ready entries and a zero-timeout control.

### Epoll millisecond timeout normalization

`sys_epoll_pwait` maps every negative millisecond timeout to an unlimited wait,
matching Linux's `ep_timeout_to_timespec`. Zero remains a poll and nonnegative
values keep the existing millisecond conversion. The shared wait path, signal
mask handling, copy-to-user behavior and `epoll_pwait2` timespec rules are unchanged.

The registered user-context regression uses an invalid descriptor to ensure
`INT_MIN`, `-65536`, `-2` and `-1` reach descriptor validation without blocking.

## pidfd_getfd reserved flags

`pidfd_getfd` accepts only flags equal to zero. Reject any other value with
`EINVAL` before looking up the pidfd or target descriptor, acquiring the target
FD-table read guard, or allocating a new descriptor. The flags-zero duplication
path still shares the original open file description and sets close-on-exec.

## System uptime

`sysinfo.uptime` reads the existing elapsed boot-time source used by
`CLOCK_BOOTTIME`, rounds up to whole seconds, and saturates at the signed
ABI maximum. Rounding matches Linux [`do_sysinfo()`](https://github.com/torvalds/linux/blob/v6.6/kernel/sys.c#L2601-L2611).
Unlike `clock_gettime(CLOCK_BOOTTIME)`, `sysinfo` has no nanosecond field:
for the same instant, its rounded value can be one greater than `timespec.tv_sec`.
The boot clock currently aliases monotonic time. This syscall
maintains no separate clock state and does not change clock or suspend semantics.

The uptime regression brackets `sysinfo` with actual `clock_gettime(CLOCK_BOOTTIME)`
reads and compares the rounded ABI values, without using the implementation
helper as the expected-value source.

### Signal number width validation

Signal syscalls, POSIX timer notifications and clone exit-signal requests use
`ksignal::Signo::from_raw` for the shared checked numeric boundary. The signal
owner accepts valid signals 1..=64 and rejects primitive negative or wide
values without truncation; these syscall frontends map rejection to `EINVAL`.
Clone retains its full-width `u64` exit-signal input until this conversion.

Zero keeps each frontend's existing meaning: kill-style probes bypass signal
construction, a clone exit signal of zero means no notification, and explicit
timer signal zero is invalid. A null timer event still defaults to `SIGALRM`
with its timer ID; `SIGEV_NONE` and `SIGEV_THREAD_ID` keep their existing
branches and validation order.

Numeric boundary regressions live with `Signo`. Caller regressions exercise
actual zero-probe, invalid-number rejection and nullable timer-event paths
without signal delivery, user-memory access or timer creation.

### POSIX timer creation output

`timer_create` parses the optional notification and delegates creation to the
current process owner before copying the timer ID to user memory. If that copy
fails, the adapter deletes the newly created timer through the same owner and
returns the original copy error. Notification parsing, clock validation and
successful ID output retain their existing order. The timer manager lock is
released before user copying; the allocator's monotonically advancing ID is
not reset by rollback.

This follows Linux's output-fault cleanup in
[`do_timer_create`](https://github.com/torvalds/linux/blob/v6.6/kernel/time/posix-timers.c#L469-L495).
Fresh user-task regressions exercise the actual create/gettime/delete syscalls
with NULL and inaccessible ID outputs, a successful unarmed timer, and rejected
clock/notification inputs. The failure cases check the freshly initialized
owner's first ID before and after creation; they do not assume Linux assigns
that number or that deleting a timer immediately reuses its ID.


### sched_getaffinity native-word ABI regression

The raw syscall decodes `len` as Linux's 32-bit `unsigned int`, validates its
native `unsigned long` alignment and minimum mask size before PID lookup,
and encodes CPU bits as zero-initialized native words. On the supported 64-bit
platforms with NR_CPUS <= 8, it writes and returns 8 bytes; the internal compact
KCpuMask representation is not the userspace ABI. A larger valid caller buffer
is not filled beyond those 8 bytes. User-buffer addresses may be unaligned.

The user/serial regressions traverse `handle_syscall` with actual argument
registers. They reject lengths 0, 1, 7, 9 and 15 without modifying output,
check EINVAL before ESRCH before EFAULT, and verify native-endian output,
zero padding and surrounding sentinels. High-register-bit cases prove dispatch
truncates len to u32: `1 << 61` and `usize::MAX` are invalid, while
`(1 << 32) | 8` and `(1 << 61) | 8` act like length 8. These are raw-syscall
return values; libc's sched_getaffinity wrapper returns zero on success.
See [Linux v6.18 scheduler syscalls](https://github.com/torvalds/linux/blob/v6.18/kernel/sched/syscalls.c).
This boundary fix does not establish a cause for earlier Chromium failures.


### Shared native CPU-set representation

The private `cpuset::NativeCpuSet` owns unsigned-long word count, byte length,
native-endian encoding, and conversion to/from the compact `KCpuMask`.
`sched_getaffinity` validates length against this type before target lookup,
then copies the type's zero-padded output. Register-width decoding and error
ordering remain in the syscall adapter. Conversion tests cover empty, sparse,
alternating and full sets plus independently assembled native-word bytes.

This change migrates only `sched_getaffinity`. As requested in review,
`sched_setaffinity` and `riscv_hwprobe` retain their existing input-length policy
and implementation; their later migration must add a byte-decoding entry to
the shared type, keeping byte-count acceptance in each syscall. This change
does not claim runtime coverage for configurations beyond the current
NR_CPUS range or for a new architecture.

### Generic clock consumers

`CLOCK_BOOTTIME` queries and `sysinfo.uptime` use `ktime::boottime`.
`clock_nanosleep` waits in the original domain for absolute REALTIME and
relative/absolute BOOTTIME requests. Relative REALTIME and MONOTONIC keep
monotonic waits. Interrupted relative BOOTTIME waits calculate remaining time
in BOOTTIME; absolute waits do not write remaining time.

### Typed clock-ID boundary

`time/clock_id.rs` parses signed ABI clock IDs into the shared `ktime_types::ClockId` once for queries,
wall-clock setting, clock sleeps and timerfd creation. Unsupported values,
including negative FD-encoded clocks, remain rejected. Each operation restricts
the parsed enum to its existing supported set; MONOTONIC_RAW sleep still returns
OperationNotSupported. Raw signed values remain available for diagnostics only.
Timerfd creation maps the accepted enum to the backend's `TimerFdClock`, which
is retained through construction and arming; raw ABI IDs do not enter the backend.
`ut-tests/time_clock_id.rs` checks ABI identity, negative/unknown rejection before
user-memory access and operation-specific restrictions.

### Kernel log read-back (`syslog`)

`sys_syslog` is the adapter over `klogger`'s log ring. The ring, the filter and
the level vocabulary all belong to `klogger`; `ksyscall` only validates the
arguments, reads a window of the ring and performs the user copy.

Implemented actions, with Linux's numbering:

| action | name | behaviour |
|--------|------|-----------|
| 3 | `READ_ALL` | return the newest whole records that fit, keep them buffered |
| 4 | `READ_CLEAR` | return the newest whole records that fit, then drop them and everything older (a clear watermark, not "the ring as it is now") |
| 5 | `CLEAR` | drop the whole buffer, return 0 |
| 9 | `SIZE_UNREAD` | return the number of buffered bytes |
| 10 | `SIZE_BUFFER` | return the ring's total capacity |

Order of operations and failure handling:

1. Privilege is decided **per action**, before the action is validated — the
   order Linux's `check_syslog_permissions` uses. `READ_ALL` (3) and
   `SIZE_BUFFER` (10) are open to every caller, matching Linux's default
   (`dmesg_restrict` off); `READ_CLEAR` (4), `CLEAR` (5) and `SIZE_UNREAD` (9)
   require privilege, and an unknown action counts as restricted so an
   unprivileged caller sees `EPERM` where Linux also returns `EPERM` before its
   `EINVAL` default. The predicate is `CAP_SYSLOG`, as in Linux's
   `check_syslog_permissions`: capability sets exist, so a non-root caller that
   holds the capability may clear the log, and root is trusted only while its
   capability set retains it.
2. The `SIZE_*` actions return `klogger`'s count and capacity.
3. The read actions decode `len` at its ABI width (`i32`) and reject a negative
   value with `InvalidInput` before touching the ring: Linux declares the
   argument as a signed `int`, so a caller's `-1` must fail like Linux's
   `len < 0` instead of being clamped to the buffer size and copied past the
   length the caller declared. Truncating to `i32` first normalizes both the
   sign-extended and the zero-extended register encodings to the same value.
4. They then rebuild a typed `UserPtr<u8>` and reject a null destination with
   `InvalidInput`.
5. A kernel read buffer is allocated, sized `len.min(LOGBUF_SIZE)` — bounded by
   the caller's length and the ring, never by a separate `logbuf_len()` sample (a
   record published between that sample and the read would make the buffer too
   small for the newest record) — and filled
   by `klogger::logbuf_read_newest_seq`, which copies the newest records that fit,
   oldest first, starting at a record boundary and returning `(copied, newest)`.
   `newest` is the sequence number of the window's last record and is what a
   `READ_CLEAR` later hands to `klogger::logbuf_clear_through` as its clear
   watermark. That is the shape Linux's `syslog_print_all` produces (it walks
   back from the newest record until the size fits and starts the copy at a
   record boundary), so a short destination receives the most recent records and
   never a partial line. The ring takes no lock, so a concurrent push cannot split
   the window and cannot stall the reader; **the user copy never runs with any
   ring state held**, so a page fault on the destination cannot hold up a writer.
6. A zero length returns 0 before the ring is touched: Linux returns ahead of
   `syslog_print_all` for `len == 0`, and nothing was delivered, so nothing may be
   consumed.
7. `READ_CLEAR` calls `logbuf_clear_through(watermark)` after a successful copy
   that delivered at least one byte, where the watermark is the sequence number of
   the last record the read copied. Records logged after the snapshot — during
   the user copy, say — therefore stay buffered, as with Linux's `clear_seq`; the
   records older than the returned window are dropped too, because the cursor
   also skips past everything it did not return.
   so `dmesg -c` clears what it drained and does not re-deliver it. A copy failure
   keeps everything, because the clear runs only after the copy succeeded.
8. Every other action, including Linux's `CONSOLE_*` controls, returns
   `InvalidInput`: console verbosity belongs to `/proc/sys/kernel/printk`, whose
   value domain is inverted relative to Linux's, and mixing the two would let a
   caller believe it changed something else.

The return value is the number of bytes copied (`isize`), so `dmesg`,
`dmesg -c` and `dmesg -r` work unchanged.
