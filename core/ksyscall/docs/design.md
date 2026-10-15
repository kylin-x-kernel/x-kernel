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
The empty-MM activation probe waits on an explicit peer acknowledgement queue
with a five-second deadline. It stays blocked until the parent verifies the
boundary and wakes it, avoiding two periodic polls missing the blocked interval.
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
  `getcpu` ignores its obsolete third cache argument without reading it.
  Null CPU/node outputs are allowed; non-null outputs use the existing user-copy wrappers.
  Scheduler policy/parameter getters and setters resolve positive published TIDs,
  including workers. Zero selects the caller; a process PID denotes its leader
  TID and never a representative worker. Missing or unpublished TIDs return ESRCH.
  Getters reject negative IDs with EINVAL; setters retain their existing ESRCH.
  Policy/parameter setters require the caller effective UID to match the target
  real/effective UID, or root (the current privilege approximation); otherwise
  they return EPERM before changing metadata.
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
namespace-creation permission with UID checks. `PR_SET_NO_NEW_PRIVS`
records the flag on the calling thread and `PR_GET_NO_NEW_PRIVS` reports
it back; the exec credential path consumes the flag via
`Cred::apply_exec`, and because exec never elevates identity today the
no-elevation contract holds unconditionally — future setuid/setgid or
file-capability support must consult the flag there. seccomp likewise has
no execution engine yet. `PR_SET_SECCOMP`
recognizes only the strict/filter mode identifiers (1/2), returning `ENOSYS`
instead of claiming installation; all other modes return `EINVAL`.
`seccomp(2)` recognizes the Linux strict/filter and action/notification-size
query operations (0/1/2/3), returning `ENOSYS`; unknown operations return
`EINVAL`. Neither rejection path accesses user memory or changes task state.
The prctl mode has `unsigned long` width; the syscall operation has
`unsigned int` width, as in Linux's syscall boundary.

Recognizing an identifier does not implement its operation. Flags, filter
contents, and query buffers are not validated. `ENOSYS` is this kernel's
explicit unimplemented-feature policy, not a claim of Linux errno parity
for recognized requests: Linux can reject an unsupported mode/configuration
with `EINVAL`. Callers requiring confinement must handle failure; this
rejection is not a sandbox. See Linux's
[seccomp entry points](https://github.com/torvalds/linux/blob/v6.12/kernel/seccomp.c#L2064-L2127)
and [PR_SET_SECCOMP(2const)](https://man7.org/linux/man-pages/man2/PR_SET_SECCOMP.2const.html).

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
