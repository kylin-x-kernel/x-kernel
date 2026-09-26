# ktty — Security And Reliability

## Scope

This analysis covers the entire `ktty` crate: `src/lib.rs`,
`src/tty/` (`mod.rs` ioctl boundary, `ntty.rs` console backend,
`pty.rs` PTY backend), and `src/terminal/` (`job.rs` job control,
`ldisc.rs` line discipline, `termios.rs` attribute layout). No modules
are excluded; the trust boundary toward `kprocess` and the user ABI is
described in the sections below.

User space can submit ioctl commands, user pointers, and PGIDs through
TTY file descriptors. `ktty` does not trust those ABI inputs; process,
process-group, session, and controlling-terminal identity is decided by
kernel objects in `kprocess`.

## External Boundaries And Attack Surface

- `TC*`/`TIOC*` ioctls carry user addresses and must be copied through
  the `osvm` user-memory access interface.
- The PGID passed to `TIOCSPGRP` may be invalid, exited, or belong to
  another session.
- `O_NOCTTY` controls whether opening a TTY may implicitly acquire the
  controlling terminal.
- Holding an open TTY fd does not mean the caller owns the terminal's
  controlling session.
- Console input and PTY peer input are external data processed by the
  line discipline.

## Memory-Safety Invariants

ioctl user pointers are dereferenced only at the ABI boundary in
`src/tty/mod.rs`, through `read_vm`/`write_vm` with user-address
validation. The job-control layer stores no user pointers — only weak
references to `Session` and `ProcessGroup` objects.

## Thread Safety

Native input work is prepared after kworkng startup in sleepable context.
IRQ producers only collect UART data and submit prepared work; they never parse,
echo or signal inline. Reader mutation is serialized by a sleepable mutex;
port locks are released before submission and reader locks before notification.
Bounded input and output work prevents a single round from draining indefinitely.
Full output waits for reader consumption instead of busy requeueing.

Self-requeue uses Weak ownership. Teardown must detach the producer then call
shutdown outside port/TTY locks, preserving callback resources until successful
drain. Shutdown rejects stale producer handles. Dropping one clone is not a
completion barrier. The native console remains kernel-lifetime.

termios, window size, session, and foreground state are each protected by
spin locks. No user-memory access or process-table lookup happens inside
a lock, keeping the critical sections small. Poll waiters are woken only
after a foreground update completes.

## Threat Analysis

| ID | Threat | Impact | Trigger | Existing control |
|------|----------|----------|----------|----------|
| T-01 | Cross-session modification of a terminal's foreground group | Medium | Another session holds an accessible TTY fd | `set_foreground_for` compares caller session and terminal session by object identity |
| T-02 | Setting another session's group as foreground | Medium | User submits a valid but cross-session PGID | `set_foreground` rejects session-mismatched target groups with `EPERM` |
| T-03 | Invalid user pointer causes illegal kernel access | High | ioctl argument targets unmapped or inaccessible memory | All access goes through `read_vm`/`write_vm`, returning user-visible errors |
| T-04 | `O_NOCTTY` open unexpectedly changes session state | Medium | Device open drops the transient open flag | `VfsFileBuilder::requests_no_controlling_tty` blocks implicit binding before the flag can be cleared |
| T-05 | PTY master wrongly used as controlling terminal | High | Session leader opens `/dev/ptmx` | Both implicit open and `TIOCSCTTY` binding return without changing session state for a master |
| T-06 | Cross-session query of foreground or SID | Medium | Another session inherits or receives the TTY fd | `foreground_for`/`session_for` compare caller session object identity |

## Failure Modes And Effects Analysis (FMEA)

| ID | Failure mode | Cause | Local effect | System effect | Severity | Handling |
|------|----------|----------|--------|------|----------|----------|
| F-01 | Controlling terminal bound without a foreground group | Bind flow skipped foreground initialization | Shell cannot enable job control | Interactive terminal degraded | 3 | `bind_to` installs session, terminal, and foreground in one transaction |
| F-02 | Partial state left after bind | Foreground set failed, or bind/unbind raced | Later getty cannot re-acquire the terminal | Console login unavailable | 2 | The per-TTY association-transaction lock serializes install, rollback, and unbind |
| F-03 | Target PGID for foreground does not exist | Process group exited or typo'd input | `TIOCSPGRP` fails | Current foreground unchanged | 4 | Target resolved through `kprocess::job_control::target_group` before the set |
| F-04 | Fallback shell inherits stdio without a controlling TTY | stdio pre-opened by a PID-less kernel thread | Bash disables job control | Interactive shell degraded | 3 | The fallback path re-opens the console in the PID 1 user context, triggering standard TTY-open binding |

## Known Limitations

When a background process group reads the controlling terminal, the
current implementation waits for it to reach the foreground; Linux's full
`SIGTTIN`/orphaned-process-group semantics are not implemented yet.

`TIOCSCTTY` does not yet support Linux's privileged `arg == 1`
cross-session forced acquisition; that request returns `EPERM`, and other
non-zero arguments return `EINVAL`.

## Audit Checklist

- New ioctls copy user pointers and validate values at the boundary.
- The controlling terminal, caller, and target process group all belong
  to the same session.
- A failed multi-phase update rolls back only the state installed by this
  attempt.
- Foreground changes wake waiters, and no blocking work runs beyond the
  wake while holding a lock.
