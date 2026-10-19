# ktty — Security And Reliability

## Scope

This analysis covers the entire `ktty` crate: `src/lib.rs`, `src/port.rs`,
`src/tty/` (`mod.rs` ioctl boundary, `ntty.rs` console backend,
`pty.rs` PTY backend, `pty_pair.rs` pair lifetime), and `src/terminal/` (`job.rs` job control,
`ldisc.rs` line discipline, `termios.rs` attribute layout). No modules
are excluded; guest fixtures in `ut-tests/` are included in this scope.
The fixtures retain private parent modules, `cfg(unittest)`, and the original
user/serial execution flags. The three former `include!` modules and the PTY
lifecycle module remain registered. This extraction does not widen production
visibility or change ioctl, worker, or device-lifetime contracts.
The trust boundary toward `kprocess` and the user ABI is
described in the sections below.

User space can submit ioctl commands, user pointers, and PGIDs through
TTY file descriptors. `ktty` does not trust those ABI inputs; process,
process-group, session, and controlling-terminal identity is decided by
kernel objects in `kprocess`.

## External Boundaries And Attack Surface

The fixed VT owns a separate terminal and real session binding. Configuration
checks use current ioctl Cred and exact controlling-terminal object equality,
with kcred's documented EUID 0 approximation for CAP_SYS_TTY_CONFIG. O_NOCTTY
and node DAC alone do not grant configuration authority.

PROCESS captures a weak actual Process identity before commit; fd close does
not clear it. Exited/disappeared controllers reset mode/display/keyboard on
subsequent use, so no reused PID receives ownership. The single-VT boundary
creates no fake release/acquire signals: PROCESS ACKACQ is accepted when no
switch is pending, release responses and unsupported VT targets are rejected.

Program writes and echo share the output-line-discipline mutex, acquired
before the VT mode-state mutex. Backend-owned processing keeps this path in
bypass without skipping zero-progress normalization. GRAPHICS suppresses this
VT's output and TEXT forwards to the native byte connection. No user copy
occurs under this mutex. Native serial terminal/session/input remain separate.
Keyboard Unicode/off are declared policy with no connected VT keycode consumer;
other translation modes are rejected. There is no claim of GPU rendering,
hardware blanking or complete VT keyboard translation. Registry-owned named
identities are projected by devfs/procfs/sysfs, with failed/retired registrations
excluded as described in KVFS's registration contract.

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

The port retains each backend batch until ldisc reports the consumed prefix.
Output-full is not permission to discard the suffix. Canonical pending output
and read policy remain in ldisc; worker budget, wake and retry remain in the
port. Both source and output budgets are preserved across this interface.
Backend readers must return a count no greater than the supplied buffer length
and must not wait for input. The interface is internal kernel code, not a
user-controlled length boundary.

Every port owns a bounded 4096-byte ingress queue. PTY writers return only the
accepted prefix; a full queue reports `WouldBlock` instead of discarding bytes.
Queue locks cover only insertion, consumption and state snapshots. Reader and
space notifications occur after releasing the queue lock. Master bypass reads
the same ingress directly and poll cannot consume, transform, or hide input.

Program output and echo serialize through the endpoint's output mutex. This is
the sole PTY producer, so a room check can reserve a complete CRLF unit while
the consumer only increases free room. OPOST/ONLCR conversion happens at write
time, and original input counts are separate from expanded output counts.
No CRLF suffix survives outside input ownership or flush semantics.
This relies on the TtyWrite room contract: fitting units are accepted in full
under producer serialization, or fail before publishing any bytes. Backends
that can short-write must report only capacity that satisfies this guarantee.

Input flush locks the work/manual reader before clearing ingress, processing
prefetch, canonical state and cooked bytes. An old in-flight processing batch
cannot republish input after the flush. Source-owned UART queues are not flushed.
Space notifications also cover master close and output-policy changes so writers
recheck capacity or return a transport error rather than staying asleep.
Poll registration always subscribes to transport space/closure notifications,
including when OUT was not requested, so peer closure also wakes IN and HUP
waiters. Shared PTY termios changes notify both ingress space sources after
releasing termios and input locks; neither endpoint's output waiter depends on
an unrelated read or signal to recheck changed newline requirements.
PTY output flush discards only the peer ingress under its queue lock, then wakes
space waiters. It does not acquire the peer processing mutex or erase bytes
already accepted by the peer line discipline. Combined flush first discards
local input and then peer ingress without nesting the two endpoint locks.
Native console output processing stays backend-owned, avoiding double CRLF
expansion; PTY output groups ordinary bytes into bounded batch writes.

The constructor assigns terminal or PTY-master identity independently of input
processing. Controlling-terminal and foreground checks use this identity, so
changing the processing policy does not change those access rules.

Native console reads collect and reset pending software-drop accounting and
report loss from task context, after releasing the UART lock. Reports count
lost bytes only; input contents are not logged. This is not a hardware-overrun
counter or a lossless-input guarantee.

Native and PTY input work is prepared after kworkng startup in sleepable context.
IRQ producers collect UART data and wake a prepared notification that submits
existing work; they never parse,
echo or signal inline. Reader mutation is serialized by a sleepable mutex;
reader locks are released before notification or self-requeue. Prepared submission
does not wait or execute the reader inline, including output-space retries issued
while the caller owns the port lock.
Bounded input and output work prevents a single round from draining indefinitely.
Full output waits for reader consumption instead of busy requeueing.

Self-requeue and source Wakers weakly reference a shared input owner, avoiding
a cycle through the reader's backend. The owner serializes submission and removal
of its optional work handle. No production caller receives a strong handle clone
that could bypass the gate. Port destruction closes the gate and releases the
handle outside it, without waiting; kworkng drains work before destroying captures
in task context. Already executing input may finish, so closure is not a
synchronous signal/echo completion barrier. Stale Wakers cannot resurrect work.
The native console remains kernel-lifetime. PTY callbacks retain original buffers,
never resolve peers by number, and do not own their TTY endpoints. PTY allocation
retirement is separate from safe input-work destruction.

Native read/write/echo use one connection, including its selected source for
loss accounting. Runtime adoption is limited to the registered early stdout
identity. Handle availability is checked before I/O, but this check is not a
general hot-unplug guarantee. Failure falls back only to the retained boot port;
connection locks are released before I/O, logging, or dropping removed handles.

termios, window size, session, and foreground state are each protected by
spin locks. No user-memory access or process-table lookup happens inside
a lock, keeping the critical sections small. Poll waiters are woken only
after a foreground update completes.

## Threat Analysis

The filesystem callback passed to `open_ptmx` constructs an unpublished
inode outside PTY locks and must hold only a weak pair reference. ktty alone
orders master opening and publication; callback errors release the reservation.
Numeric-alias rejection is enforced by ktty's registered Unix98 operations.

The fixed capacity is 16 pairs. Reservation rejects numbers beyond that limit;
allocation and release update the bitmap under the registry lock. Read-only
usage snapshots expose no reservation capability. Endpoint allocation retains
the kernel's existing OOM policy.

PTY numbers remain reserved from construction through final open-file-use
release. The bitmap and published tree are protected by a short registry lock;
removed pair Arcs and concrete resources are dropped outside it. Pair admission
is serialized with close under a sleeping mutex. The registry never acquires
the pair mutex while locked, avoiding inversion with publication/retirement.
`PtyOpen` is not duplicated for dup/fork, and no release callback also decrements
its count. Construction failure drops the reservation; failure after publication
closes the master use without discarding independently opened slave uses.

An old inode has a weak original-pair reference; retirement makes that identity
permanently unopenable even while snapshots survive or its number is reused.
Session-held endpoint references cannot bypass `/dev/tty` admission. Final
file-use release may destroy an inode and must run in sleepable VFS context,
not in IRQ context or under caller spinlocks. Input closure is nonblocking;
it is not a synchronous barrier against an already running signal/echo batch.

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

PTY capacity is fixed; no per-mount quota or runtime resizing is supplied.
Native console output and echo remain best effort; neither promises lossless
delivery under saturation. Canonical-line storage retains its existing growable
Vec policy; bounded ingress is not a bound on an entire canonical line.
Master close makes slave poll report ERR/HUP. Slave reads serialize with input
work and process retained ingress, prefetch and canonical delivery before
returning cooked bytes without VMIN waits. EIO is returned only when this
processing cannot publish further bytes; incomplete canonical lines still need
a delimiter and are not finalized by closure. Writes report EIO through
the closed peer ingress. Complete Linux hangup, SIGHUP delivery and symmetric
EOF/EIO semantics in the opposite direction remain outside this implementation.
In particular, closing the last slave file does not close its receive ingress
while the master remains open. A master blocked on full ingress still needs a
reader, flush or signal to resume; this change does not introduce slave-close
hangup detection. Echo follows OPOST/ONLCR for LF; a CR with ICRNL disabled
remains CR, as input processing and output newline conversion are separate.

When a background process group reads the controlling terminal, the
current implementation waits for it to reach the foreground while the peer is
open. After peer closure, background reads return EIO without consuming input
reserved for foreground readers. Linux's full
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
