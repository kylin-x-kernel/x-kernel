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

The crate contains three audited unsafe sites; all carry inline `SAFETY:`
notes. There are no hand-written unsafe traits, FFI, or inline assembly.

| Site | Operation | Invariant |
|---|---|---|
| `sys.rs` (`sethostname`) | stack `[u8; N]` reborrowed as `MaybeUninit<u8>` for copy-from-user | the array is live with trivially initializable bytes; only `buf[..len]` is read after the copy (same pattern as `devfs/nodes/loop.rs`) |
| `task/clone3.rs` | fully initialized `#[repr(C)]` integer struct viewed as `MaybeUninit<u8>` for a versioned in-place user copy | `kargs` is zero-initialized first, so bytes beyond the caller's struct version stay zero; the reinterpretation is size-preserving |
| `time/posix_timer.rs` | read of `sigevent._sigev_un._tid` | guarded by `sigev_notify == SIGEV_THREAD_ID`, the ABI-defined selector for that union arm; the tid is validated positive and owned by the process before use |

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

- `ioctl` first asks `posix-net::handle_net_ioctl` for exact commands, then
  falls back to file `ioctl`. Socket file vtables do not yet implement
  `ioctl`, so SIOC* still hangs on the syscall adapter instead of the Linux
  `sock_ioctl` shape.
- Capability, seccomp, and `no_new_privileges` security ABIs must not
  return success inconsistent with actual enforcement (currently `ENOSYS`).

## Audit checklist

- cgroup/namespace flags may leave the `ENOSYS` list only when fully
  implemented; adapters do not create a second membership or namespace-view
  state.
- Does a new syscall implementation only adapt the ABI instead of copying
  an owner state machine?
- Is a new adapter placed near its owner rather than in a historical
  catch-all directory?
- Do all user-pointer accesses go through the existing wrapper types?
- When merging similar syscall paths, are the per-ABI argument counts and
  flags respected separately?
- Is the calling context of current process/thread helpers explicit?
- Do architecture-specific syscalls keep hardware sources of truth in the
  architecture owner instead of ad-hoc probing in the adapter?
- When owner routing changes, are this crate's and the owner's documents
  updated together?
