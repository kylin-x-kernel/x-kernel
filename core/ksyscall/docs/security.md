# ksyscall security and reliability

## Overview

`ksyscall` is the first adaptation boundary between the user-space syscall
ABI and kernel resource owners. The main risk sources are:

- user-provided syscall numbers, flags, fds, PIDs, and scalar arguments;
- user-pointer `copyin`/`copyout`;
- wrong owner routing that moves permission or semantic checks to the wrong
  place;
- adapters accidentally holding resource state, blurring the boundary.

## Trust model

```text
userspace syscall arguments
   │ untrusted
   v
ksyscall
   │ validates ABI shape and dispatches
   v
resource owners
   ├─ posix-fs / kvfs
   ├─ kfd_objects
   ├─ kprocess
   ├─ posix-process / kprocess
   └─ other subsystem owners
```

- User-space syscall arguments are untrusted.
- `ksyscall` trusts each owner crate to maintain real resource semantics
  once the call crosses its boundary.
- `ksyscall` must complete ABI-level baseline validation at the syscall
  boundary without duplicating owner-internal invariant checks.

## Unsafe inventory

The production code contains four audited unsafe sites; all carry inline `SAFETY:`
notes. There are no hand-written unsafe traits, FFI, or inline assembly.

| Site | Operation | Invariant |
|---|---|---|
| `sys.rs` (`sethostname`) | stack `[u8; N]` reborrowed as `MaybeUninit<u8>` for copy-from-user | the array is live with trivially initializable bytes; only `buf[..len]` is read after the copy (same pattern as `devfs/nodes/loop.rs`) |
| `task/clone3.rs` | fully initialized `#[repr(C)]` integer struct viewed as `MaybeUninit<u8>` for a versioned in-place user copy | `kargs` is zero-initialized first, so bytes beyond the caller's struct version stay zero; the reinterpretation is size-preserving |
| `time/posix_timer.rs` | read of `sigevent._sigev_un._tid` | guarded by `sigev_notify == SIGEV_THREAD_ID`, the ABI-defined selector for that union arm; the tid is validated positive and owned by the process before use |
| `task/execve.rs` | replace current MM and activate its page table | candidate comes from `new_user_empty`, all old-user inputs are owned, no old-MM lock or user-copy window crosses activation; post-exchange errors are fatal and user entry requires complete image/context setup |

The unittest exit-tail fixture additionally calls the replacement helper with
a fresh kernel-valid MM while two real threads are published, verifying rejection
without activation. It holds no MM guard or old-user access window at that call.

All other user-memory access goes through the `posix-types`/`osvm` checked
copy wrappers.

## Core invariants

1. `ksyscall` keeps no long-term resource state.
2. User pointers are accessed only through the existing safe wrapper types.
3. Syscall adapters own ABI-level error-code branches and do not implement
   owner logic beyond their mandate.
4. Adapter directory structure reflects owner boundaries, not historical
   API categories.
5. Helpers involving the current process/thread are called only in
   contexts that guarantee them.
6. Syscall decoding reads only the registers its ABI defines; stale values
   in registers unused by an older ABI must not be trusted as extension
   flags.

The obsolete `getcpu` cache argument is ignored and never dereferenced. CPU
and NUMA-node output pointers still go through `UserPtr` and retain copyout errors.

## Threat analysis

| ID | Threat | Severity | Trigger | Response |
|---|---|---|---|---|
| T-01 | Bad user pointer fails copyin/copyout | Medium | any wrapped access | All access goes through `UserPtr`/`UserConstPtr` and existing wrappers, propagating `KResult` |
| T-02 | Adapter lands under the wrong owner, blurring boundary responsibilities again | Medium | new syscalls placed by habit | Directories and docs are organized by owner semantics; reviews check routing |
| T-03 | Adapter duplicates an owner state machine, creating a second source of semantics | High | convenience copying | Docs state that `ksyscall` owns no long-term state; ABI adaptation only |
| T-04 | Current-thread/process helpers called in the wrong context | Medium | helper misuse | Reuse `kprocess` constraints; syscall entry maintains the task-context assumption |
| T-05 | Historically misleading directories keep collecting syscalls of the wrong owner | Medium | new additions | This crate-local design doc fixes the `vfs`/`ipc`/`time`/`task` adapter semantics |
| T-06 | `setpriority` authorizes only a process representative and misses target identity | High | per-thread credentials | `PRIO_*` selection and scanning both resolve to concrete tasks; per task, caller euid is compared with target real/effective UID, and priority-raising permission is checked separately |
| T-06a | Unprivileged process rewrites arbitrary task affinity | High | `sched_setaffinity` calling `set_cpumask` on non-current targets | After target resolution, caller euid is compared with target ruid/euid under `check_same_owner` semantics; root (approximating `CAP_SYS_NICE`) may bypass, otherwise `EPERM` |
| T-07 | Unprivileged process renames the host | High | `sethostname` writing the UTS namespace directly | The syscall boundary checks a privileged credential and bounds nodename length and the user buffer access |
| T-08 | Unprivileged process changes power state | High | `reboot` reaching platform power interfaces directly | Privileged credential, Linux magic, and the supported command set are checked |
| T-09 | Unprivileged process shifts the wall clock | High | wall-clock setters updating the realtime association directly | `settimeofday` and `clock_settime` check a privileged credential in the shared setter and reject moving the wall clock before `CLOCK_MONOTONIC` |
| T-10 | `PR_SET_KEEPCAPS` receives an invalid value or bypasses the lock bit | Medium | `ctl.rs` | Values greater than 1 are rejected; `kcred::Cred::keep_caps_enable()` / `keep_caps_disable()` validate the lock bit and commit once through a prepared credential |
| T-11 | `riscv_hwprobe` consumes unvalidated user cpusets/pairs | Medium | bad pointers, oversized masks or pair counts causing huge kernel allocations (DoS) or corrupt ABI results | Pairs stream one by one (`read_vm`/`write_vm`, no bulk `Vec` allocation; an oversized `pair_count` only walks into unmapped pages and returns `EFAULT`); `cpusetsize` is clamped to `cpumask_size()` on both load and write; non-`0`/`WHICH_CPUS` `flags` return `EINVAL`; in value mode an empty intersection of the user cpuset with online CPUs returns `EINVAL`; `cpus == NULL && cpusetsize != 0` returns `EFAULT`; in WHICH_CPUS mode an empty cpuset means all online CPUs, unknown keys write `key=-1,value=0` and clear the output cpuset; key semantics live in the `kcpu` hwprobe helper |
| T-12 | Syscall hot path reads unreliable RISC-V hardware state | Medium | S-mode reads of M-mode CSRs faulting, or capability divergence across CPUs | The source of truth is the FDT-initialized snapshot in `kcpu`; `ksyscall` only aggregates over the selected CPU mask |
| T-13 | `get_robust_list` leaks another process's user addresses | High | target resolution | After resolving the target thread, `kprocess::ptrace::check_read_real_creds_access()` applies the uniform policy: same-thread-group exemption, asymmetric matching of caller real UID/GID against target real/effective/saved IDs, and the euid-0 approximation of `CAP_SYS_PTRACE`; otherwise `EPERM` |
| T-14 | `waitid` copies signals across a raw `siginfo_t`-shaped buffer | Medium | `waitid(P_PID, ...)` status reporting | `siginfo_t` is registered as `posix_types::UserWrite` and written via `write_vm`; the payload is built by `ksignal::ChildExitSignalInfo`, the same source used for real SIGCHLD delivery |
| T-15 | `execveat(AT_EMPTY_PATH)` execs an fd the caller cannot name | Medium | runc/crun self-reexec from a sealed memfd | The fd must reference an already-open file the caller owns; the image still goes through `kexec`'s execute-permission and ELF validation, and the resulting `/proc/self/exe` follows to the same opened backing file |

## Signal-wait reliability

`pause` and `rt_sigsuspend` use one interruptible wait in sleepable current-user-
thread context, with no spinlock held across blocking. Checking pending signals
before waiter registration and retrying when an interrupt wins registration
prevents lost wakeups. Blocked signals do not finish the wait; signal disposition
is resolved by the existing user-return path, which restarts `ERESTARTNOHAND`
when no handler is delivered. Neither syscall reports a successful return.

`pause` does not copy user memory or change the active/saved signal masks.
`rt_sigsuspend` validates and copies its mask before installing it, and retains
its existing saved-mask restoration on handler-frame setup. Registration failure
cannot leak `ENOMEM` through either wait: it yields and retries. Persistent
allocation failure may therefore consume scheduler time; normal waiting uses
registered wakeups rather than polling or a timer.

## Known limitations

- `sched_setscheduler` and `sched_setparam` now accept published worker TIDs.
  Policy/parameter setters enforce caller-euid vs target ruid/euid ownership
  or privileged euid 0 before mutating metadata. Full capability/RT resource-limit
  authorization remains unimplemented; these metadata writes do not establish
  real RT execution.
- Exec supports only a live single-threaded process. Independent CLONE_VM peers
  retain their old MM; this is not a de_thread or vfork implementation.
- `ioctl` first asks `posix-net::handle_net_ioctl` for exact commands, then
  falls back to file `ioctl`. Socket file vtables do not yet implement
  `ioctl`, so SIOC* still hangs on the syscall adapter instead of the Linux
  `sock_ioctl` shape.
- Capability, seccomp, and `no_new_privileges` security ABIs must not
  return success inconsistent with actual enforcement. `PR_SET_SECCOMP`
  rejects unknown modes with `EINVAL` and strict/filter installation with
  `ENOSYS`. `seccomp(2)` rejects unknown operations with `EINVAL` and
  recognized operations with `ENOSYS`. Neither path accesses the supplied
  pointer or changes task state. This is explicit rejection, not seccomp
  enforcement; flags, filters, and query buffers remain unimplemented.
  `PR_SET_NO_NEW_PRIVS` records the flag on the calling thread and returns
  success: the exec credential path consumes the flag
  (`Cred::apply_exec`), and since exec never elevates identity today the
  promised "exec grants no privileges" holds unconditionally. Future
  setuid/setgid or file-capability support must consult the flag at the
  credential reset and refuse elevation instead of silently dropping the
  guarantee.

- `PR_SET_PDEATHSIG` records the signal on the process identity but does not
  yet deliver it at parent-death reparenting, and — unlike Linux — does not
  clear it when an exec changes the credentials.
- `waitid` validates the option set and reports exits only: `WSTOPPED`/
  `WCONTINUED` are accepted but stop/continue reports are not produced,
  `rusage` is not populated, a `P_PIDFD` wait keys on the pid recorded when
  the descriptor was created (pid reuse after close is not detected), and
  pidfd `O_NONBLOCK` is not honored as `WNOHANG`.

## Audit checklist

- After MM exchange, keep all fallible installation and process-update work
  inside the fatal boundary; never resume the old user context.
- Release MM guards and temporary active users before fatal exit. Old-MM
  retirement may sleep, but must not dispatch user signals against the new MM.
- Reset old user registrations before fallible image installation. Preserve the
  TEE session for fatal cleanup until the successful exec tail.
- Keep exit-tail root observations tied to the captured MM, not the process's
  newer binding. A structural pin does not authorize user-memory access.
- Keep unittest injection request-local. Borrowed-MM observers must not reenter
  the same MM mutex or wait for a peer requiring it. Do not hold IRQ/preemption
  exclusion across blocking or migration.
- Use noncreating accessors when checking TEE cleanup, and release observers
  before testing reclamation. Tests must not supply the cleanup they verify.

- cgroup/namespace flags may leave the `ENOSYS` list only when fully
  implemented; adapters do not create a second membership or namespace-view
  state.
- Does a new syscall implementation only adapt the ABI instead of copying
  an owner state machine?
- Is a new adapter placed near its owner rather than in a historical
  catch-all directory?
- Is `waitid`'s `P_PIDFD` path resolving the fd through
  `PidFd::from_file` (a downcast, not a raw `fd as pid`) before waiting?
- Does `execveat(AT_EMPTY_PATH)` reuse an fd the caller already holds, and
  does the resulting `exe_file` stay identical to the opened file?
- Do all user-pointer accesses go through the existing wrapper types?
- When merging similar syscall paths, are the per-ABI argument counts and
  flags respected separately?
- Is the calling context of current process/thread helpers explicit?
- Do architecture-specific syscalls keep hardware sources of truth in the
  architecture owner instead of ad-hoc probing in the adapter?
- When owner routing changes, are this crate's and the owner's documents
  updated together?
- A new `ERESTART_RESTARTBLOCK` path must arm the current thread's
  restart-block and route `restart_syscall`, not only change the errno.
  Default `SIGCONT` is not a handler and must not leave `-516` in the user
  return value. Publishing `EINTR` for a handler must clear the slot in that
  same return.

The exec and exit-tail regressions in `src/task/tests/` cover the exec
boundaries above. Synthetic-image and injected-error cases do not establish
real user-program execution or allocator-exhaustion behavior.

Scheduler ownership does not equate shared PID/thread-group membership with
matching credentials. Raw credential changes affect only the calling thread;
libc-wide synchronization is a separate userspace responsibility. A caller
that dropped its UID cannot update a peer retaining another UID solely because
they share a process. The same-group raw-setuid regression checks peer IDs,
EPERM and unchanged scheduler metadata, then restores caller credentials.

## Resource-limit input/output aliasing

The prlimit64 adapter copies the optional input into a kernel-owned value before
writing any userspace output. Invalid resource IDs, input faults and rejected
updates cannot overwrite the old-limit output object. An output fault after an
applied update leaves that update in effect, following Linux behavior. No raw
user references or new unsafe code are introduced. Existing target-process
authorization and hard-limit policy are unchanged. When an update is requested,
the owner validates, captures the old pair and replaces it under one limits
write guard, so concurrent updates cannot make this syscall return a stale
pre-update pair. Query-only calls take a checked snapshot under a read guard.
Either guard is released before writing user memory; an output EFAULT still
does not undo a successful update. The atomic contract covers one resource
pair, not user copying, process liveness or authorization outside that owner lock.

## Optional time-query output and query-only clocks

A NULL gettimeofday timeval skips all user access and clock sampling. A
non-NULL timeval still uses the checked user-copy boundary. clock_settime
rejects query-only clocks before user memory is read or the privileged setter
is entered; rejecting CLOCK_REALTIME_COARSE does not relax any authorization
for CLOCK_REALTIME. No unsafe operations, lock changes or additional privileges
are introduced.

## Poll result-array handling

Ignored negative descriptors perform no file lookup and expose no retained
input `revents` as an output event. Invalid nonnegative descriptors report
`POLLNVAL` without skipping valid entries. Counting invalid entries as ready
prevents waiting on unrelated unready files. The existing user-copy, temporary
signal-mask and registered-wakeup boundaries remain unchanged; this adds no
unsafe access, new locks or descriptor-table mutations.

### Negative epoll timeouts

All negative millisecond timeouts represent unlimited waits rather than invalid
input. Normalization still delegates to the existing shared path: signal-set
size, event count, descriptor and user-memory checks remain in place. The
regression uses an invalid descriptor and performs no wait, allocation or
user-memory access. It does not change `epoll_pwait2` timespec validation.

## pidfd_getfd reserved-argument rejection

Unsupported `pidfd_getfd` flags fail before target-resource access or descriptor
publication. This argument check does not add or establish ptrace credential
authorization for the existing flags-zero path. Target lifetime, permission
policy and FD-table locking remain separate concerns.

`sysinfo` saturates uptime conversion rather than wrapping into a negative ABI
value; the existing zero-initialized carrier and checked user-memory write
remain unchanged.

### Signal number truncation

Signal-number construction is centralized in `ksignal::Signo::from_raw`.
Its checked primitive conversion rejects negative and out-of-width inputs
before enum validation, so attacker-controlled high bits cannot alias a valid
low-byte signal, including full-width clone exit-signal values. Syscall-specific
zero handling and `EINVAL` mapping stay in these frontends. Existing credentials,
target lookup, queue handling, timer ownership and user-memory checks are
unchanged. Owner and caller regressions avoid signal delivery, usercopy and
timer side effects; nullable and zero paths retain their original ordering.

### POSIX timer creation failure cleanup

A failed timer-ID output copy does not leave the just-created timer attached
to the process. Cleanup uses the existing process timer owner after the copy
fault, outside its manager lock. A peer may already have deleted the timer, or
runtime teardown may have removed it; a cleanup error does not replace the
original user-memory error. This adds no user-pointer access under a lock,
new owner API or unsafe code. The change covers failure cleanup, not a new
two-phase timer-publication protocol for concurrent callers.

The user-task regressions keep timers unarmed and use no waits or signal
delivery. Each starts in a fresh process and checks owner lookup before and
after the actual failed syscall. Assertion closures are followed by cleanup,
so an expected failure against the old implementation also removes its leaked
test timer before returning.
