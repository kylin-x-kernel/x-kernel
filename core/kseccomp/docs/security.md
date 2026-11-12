# kseccomp security and reliability

## Scope, assets, and boundaries

seccomp is a voluntary sandbox that user code inside the guest imposes on the kernel: **the trust boundary lies between the kernel and
user code**, and the goal of a filter is to reduce the syscall surface available to a malicious or compromised user process.
The interpreter in this crate sits on the kernel side of that boundary, so it must satisfy the following:

1. Never amplify privileges: any interpretation exception (unknown instruction, out-of-bounds access, division by zero) must fail closed,
   rather than falling back to an "unfiltered" allow.
2. Never leak kernel data: `run` may only read the `SeccompData` snapshot it was given,
   with no other memory reads or writes (this crate contains no `unsafe`).
3. The deny path must hand the syscall layer a deterministic, executable result, and the decision precedence must match Linux
   (otherwise a malicious program could bypass the intended constraints by having "two filters return different actions").

## Invariants and commitments

- **fail-closed fallback**: `run` returns `SECCOMP_RET_KILL_THREAD` by default; an out-of-bounds
  load returns 0, and division by zero / illegal instructions / illegal jumps terminate immediately with deny, never going beyond the program boundary
  or accessing memory outside the snapshot.
- **Unrecognized actions are not allowed**: `TRAP` and any unrecognized return value are classified as
  `Action::Unknown`, which has the lowest precedence value, so `highest_priority` yields
  `Decision::Unsupported`, and dispatch terminates the thread with `SIGSYS`.
- **Determinism**: a program is a stateless straight-line machine (A/X/M + pc); the same program on the same snapshot always yields the same result;
  there is no entropy, no shared concurrency, and no global state.
- **Data minimization**: the `SeccompData` snapshot contains only the 64 bytes of fields the seccomp ABI needs;
  the interpreter does not touch any other kernel memory.
- **Length limit**: `SeccompFilter::new` enforces `BPF_MAXINSNS=4096`, preventing a malformed
  user program from dragging down the kernel or triggering excessive allocation (the ksyscall side validates before allocating).
- **Install-time validation**: instruction encodings and all forward jump targets are validated before installation, and an illegal program returns
  `EINVAL`; there is no ambiguous failure of the form "installs successfully but is certain to kill when run".
- **Cooperation with PR_SET_NO_NEW_PRIVS / CAP_SYS_ADMIN**: the seccomp installation gate is on the
  `sys_seccomp` side (privileged or no_new_privileges); this crate only guarantees that an installed
  filter is interpreted faithfully, and does not perform authorization on its own.

## Defenses against sandbox escape

| Attack vector | Defense |
| --- | --- |
| A filter returns ALLOW to bypass | Chained merging takes the highest priority; ALLOW has the lowest priority in `Action::precedence` |
| Bypass using TRAP / an unknown return code | `Action::Unknown` has the highest priority (strictest) → `Decision::Unsupported` → dispatch `do_exit(SIGSYS)` |
| Bypass using TRACE/USER_NOTIF | The enumerators are non-executable → `GET_ACTION_AVAIL` returns `ENOSYS`, and dispatch returns `ENOSYS` for the syscall, never letting it through |
| Out-of-bounds `BPF_ABS/IND` reads kernel memory | All loads are bounds-checked against the 64-byte snapshot, and out-of-bounds reads are zero-filled |
| Division-by-zero / shift crash | ALU division/modulo by zero gives `EINVAL` at install time, and at run time still falls back to deny; shifts follow `wrapping_*` semantics |
| An oversized `sock_fprog.len` or an overlong chain drags down the kernel | `SeccompFilter::new` and ksyscall both enforce `BPF_MAXINSNS`; the total chain length is bounded by Linux `MAX_INSNS_PER_PATH` |
| An illegal program turns the kernel into an amplifier of "certainly kill the process" | Install-time validation rejects it first, and the caller sees `EINVAL` at the install point |
| The syscall entry does nothing (misses evaluation) | `handle_syscall` uniformly calls `evaluate_seccomp` before dispatch, a single-point entry |
| The strict allowlist is generated wrongly (lets a wrong syscall through) | `SeccompFilter::strict_allowlist` is pinned by unit tests to the semantics "allow only four numbers, kill the rest"; the numbers come from `linux_sysno::Sysno` |

## Audit points

- `bpf::run`: all branches for illegal instructions / out-of-bounds / division by zero resolve to `DEFAULT_RETURN`
  (deny); there should be no implicit path that can allow.
- `filter::validate`: class / size / mode / operator and forward jump targets are all validated; any newly added
  executable encoding must extend this function in sync.
- `Action::from_ret` / `highest_priority`: the precedence table Unknown > KILL_PROCESS >
  KILL_THREAD > ERRNO > USER_NOTIF > TRACE > LOG > ALLOW is consistent with
  `seccomp_run_filters` (including the "unknown values are strictest" policy).
- `SeccompFilter::strict_allowlist`: the layout must guarantee that the `jt` of each `JEQ` lands exactly on the
  trailing `ALLOW`, and does not clobber the accumulator (a load instruction must not be used as a trampoline).
- The ksyscall installation gate: the `flags` allowlist (TSYNC/TSYNC_ESRCH/LOG/SPEC_ALLOW);
  any unknown flag yields `EINVAL`, avoiding silent ignoring.

## Known boundaries

- `USER_NOTIF`/`TRACE` are not implemented: `GET_ACTION_AVAIL` returns `ENOSYS` for both,
  and dispatch fails closed for both → `ENOSYS`. They are not silently allowed.
- `TRAP` does not deliver a `SIGSYS` callback frame (Linux builds the
  `siginfo` from the data of `SECCOMP_RET_TRAP`); it is currently handled in the strictest way by terminating the thread directly; a process that depends on a `SIGSYS` handler will
  observe a difference.
- `SECCOMP_FILTER_FLAG_LOG` is accepted but produces no audit log; `SPEC_ALLOW` has no side effects.
- TSYNC only broadcasts to sibling threads that are "already published"; a thread that is under construction and not yet published will, after publication,
  inherit from a thread that is still running (see `Thread::prepare_*_fork/thread_clone` for the inheritance points).