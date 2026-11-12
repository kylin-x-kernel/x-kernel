# kseccomp design

## Positioning

`kseccomp` is the classic-BPF filter execution engine and the `struct seccomp_data`
snapshot model for seccomp(2), and a necessary part of being able to "run the dockerd default seccomp profile". It is responsible for only
four things: cBPF program interpretation, program validation, decision priority, and data layout. It does **not** hold thread/process state,
does not read user memory, and does not take part in syscall dispatch.

Responsibility boundaries of the related crates:

```text
ksyscall (sys_seccomp)  ->  decode the user sock_fprog field by field, call kseccomp to build the filter
                          ->  store in kprocess::Thread.seccomp()
kseccomp                 ->  pure interpreter run(SockFilter[], SeccompData) -> u32
                          ->  SeccompFilter validation (length / instruction encoding / jump range)
                          ->  SeccompFilter::strict_allowlist() generates the strict allowlist
kprocess::Thread         ->  holds SeccompContext (mode + filter chain)
handle_syscall           ->  syscall entry runs evaluate_seccomp first, then dispatches
```

## Architecture

```text
User-space sock_fprog
   │  (ksyscall decodes the len/filter pointer by field, then decodes each sock_filter)
   ▼
SeccompFilter::new()  ── validates empty program/BPF_MAXINSNS/instruction encoding/jump range
   │
   └─ SeccompFilter::strict_allowlist(&[nr])  ── strict-mode allowlist
   ▼
Thread.seccomp()   (set_strict / append_filter / sync_from)
   ▼
The syscall entry builds SeccompData{nr,arch,ip,args[6]}
   ▼
run(program, data.bytes)  ── cBPF machine (A/X/M[0..16]), pure arithmetic, no unsafe
   ▼
Action::from_ret ── highest_priority ── Decision{Allow,Errno,KillProcess,KillThread,UserNotif,Trace,Unsupported}
```

## Scope

- `SeccompData`: a 64-byte snapshot laid out per Linux `include/uapi/linux/seccomp.h`,
  constructed through the single entry point `SeccompData::new`, which guarantees that the `nr/arch/ip/args` offsets are self-consistent, and which provides
  the `AUDIT_ARCH_*` constants for x86_64/aarch64/riscv64/loongarch64.
- `run`: the classic-BPF interpreter, supporting IMM/ABS/IND/MEM/LEN for `LD`/`LDX`, ST/STX,
  ALU (ADD/SUB/MUL/DIV/OR/AND/LSH/RSH/NEG/MOD/XOR), JMP
  (JA/JEQ/JGT/JGE/JSET, with source K or X), RET (K or A), and MISC (TAX/TXA).
- `SeccompFilter::new`: the empty-program / `BPF_MAXINSNS` limit checks, plus instruction encoding and jump range validation aligned with Linux
  `seccomp_check_filter`.
- `Action::from_ret` / `highest_priority`: chained filters are merged by the priority of Linux
  `seccomp_run_filters`; an unrecognized action takes the strictest priority.
- strict mode: `SeccompFilter::strict_allowlist()` takes architecture-correct syscall numbers via `linux_sysno::Sysno`
  and generates an allowlist program for read/write/_exit/rt_sigreturn,
  reusing the same `evaluate` engine.
- The `opcode` module centralizes the classic-BPF encoding constants and bit-field extraction functions, shared by the interpreter and the validator.

## Design decisions

- **Pure-function interpreter**: all illegal instructions, out-of-bounds loads, and division by zero fall back to
  `SECCOMP_RET_KILL_THREAD`, and are never "leniently allowed", matching the Linux behavior that "an illegal filter must not
  silently allow a syscall".
- **Illegal programs are rejected at install time**: `SeccompFilter::new` validates the instruction encoding and all forward jump
  targets; an illegal program returns `SeccompFilterError::InvalidInstruction`, which the syscall layer
  maps to `EINVAL`. This exposes the error at the installation point, rather than letting it kill the task on the first syscall.
- **RET preserves all 32 bits**: the action and the low 16 bits of errno/data are returned together, and are split with `SECCOMP_RET_ACTION_FULL`/`DATA` only when the decision is decoded (
  `Action::from_ret`).
- **KILL_PROCESS uses a full-width comparison**: 0x80000000 exceeds the action mask, so `from_ret` matches all actions in one pass with
  `SECCOMP_RET_ACTION_FULL`; `TRAP` and unknown values are both classified as
  `Action::Unknown`, whose `precedence()` is the lowest (strictest), so they are never ignored.
- **The strict allowlist is generated per arch**: this avoids hard-coding x86 numbers; the caller passes in the numbers of
  `Sysno::read/write/exit/rt_sigreturn`, so aarch64/riscv64 and x86_64 are all correct.
- **Evaluation does not allocate**: `SeccompContext::evaluate` uses a single fold to find the minimum priority,
  and allocates no memory on the syscall hot path while holding `SpinNoIrq`.

## Calling constraints / execution context

- `run`/`SeccompFilter::new`/`highest_priority` are pure computation and may be called from any context;
  this crate is `#![forbid(unsafe_code)]`.
- `SeccompContext` protects the mode and the filter chain with `SpinNoIrq`: on every syscall entry the
  evaluation takes the lock and walks the chain, so the chain length equals the number of filters, which makes it a syscall hot path; the total length of the chain is bounded by Linux
  `MAX_INSNS_PER_PATH` (see `kprocess`), and the evaluation path does not allocate.
- Filters are installed only in `sys_seccomp`, never in interrupt context; `inherit_from`/
  `is_syncable_with`/`sync_from` take a snapshot first and only then take the lock, so no path ever holds the locks of two
  contexts at the same time.

## Boundaries and non-goals

- `SECCOMP_RET_USER_NOTIF` / `SECCOMP_GET_NOTIF_SIZES` /
  `SECCOMP_RET_TRACE` are not implemented (they are enumerated as `UserNotif`/`Trace`, and the syscall layer maps them to
  `ENOSYS`, failing closed).
- `TRAP` cannot deliver a user callback, so, like an unknown action, it is handled with the strictest priority,
  and dispatch terminates the thread with `SIGSYS` (fail closed).
- There is no cBPF-to-kernel-eBPF translation and no eBPF verifier; per-instruction interpretation plus install-time validation is enough to satisfy
  the container default profile and the security boundary.