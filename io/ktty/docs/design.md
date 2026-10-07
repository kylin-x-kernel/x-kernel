# ktty — Design

## Native Console Input Work

Native consoles with an input IRQ use `ProcessMode::Work` and a prepared
`kworkng::WorkHandle`; they do not subscribe to generic IRQ notifications.
Preparation requires sleepable context after normal-worker initialization.
Late-init attaches the work to the stdout port even before userspace opens TTY.
Consoles without an IRQ retain Manual mode; PTY processing is unchanged.

The callback owns `Mutex<InputReader>` and a readable PollSet, but no strong
reference back to the line discipline or its work. Weak self-requeue shares
the producer's fixed CPU binding. Each round consumes at most 256 input bytes
or publishes at most 256 output bytes, including partial canonical-line delivery.
Incomplete canonical input consumes budget even without output. Input-empty and
output-full stop processing; budget exhaustion requeues. Reads and drain_input
resubmit when output capacity becomes available. kworkng owns pending/running
state; TTY does not duplicate it.

Echo and signal delivery run in task context under the reader mutex, without
port or TTY spinlocks. Wake and requeue occur after releasing that mutex.
Canonical input retains command spaces and tabs. Discarding a pending canonical
line also resets its delivery offset. The existing canonical-line Vec policy
is unchanged; a per-round budget is not a maximum line length.

## Terminal Layer

`ktty` implements the X-Kernel terminal layer: terminal devices, the line
discipline, pseudo-terminals, and the POSIX job-control interface. The
device-file layer forwards `/dev/console`, `/dev/tty`, and PTY requests to
this crate; process, process-group, and session identity stay owned by
`kprocess`.

## Scope

- `src/terminal/job.rs`: controlling-terminal session association, the
  foreground process group, and foreground-change notification.
- `src/terminal/ldisc.rs`: input processing, canonical mode, and signal
  character handling.
- `src/terminal/termios.rs`: the `Termios` terminal attribute layout and
  flag/character constants shared by the ioctl boundary.
- `src/tty/mod.rs`: TTY file operations and the `TC*`/`TIOC*` ioctl
  boundary.
- `src/tty/ntty.rs`, `src/tty/pty.rs`: the console TTY and PTY backends.

## Non-Responsibilities

- No terminal device drivers: UART hardware belongs to the console/serial
  drivers; `ktty` consumes byte streams through the TTY backend trait.
- No process, session, or credential ownership: sessions and process
  groups live in `kprocess`; `ktty` only holds weak references and asks
  it to resolve PGIDs.
- No pseudo-terminal allocation policy: `/dev/ptmx` open-path decisions
  belong to the devfs/pty layers; `ktty` implements the master/slave
  data path and job-control semantics.
- No signal delivery policy: control characters that generate signals
  are turned into `ksignal` requests by the line discipline; `ktty`
  does not decide delivery.
- No user-memory policy: user pointers appear only at the ioctl boundary
  and are copied through the checked user-access helpers.

## Controlling Terminal State

`TIOCSCTTY` only lets a session leader acquire the controlling terminal.
On a successful bind, `ktty` simultaneously:

1. installs the controlling-terminal object in `kprocess::Session`;
2. records the same session in `JobControl`;
3. makes the caller's process group the initial foreground process group.

This matches Linux's observable behavior after acquiring a controlling
terminal, so a shell started afterwards obtains a valid foreground PGID
through `TIOCGPGRP`.

Known limitation: Linux's privileged cross-session forced acquisition
(`TIOCSCTTY` with `arg == 1`) is not implemented; that request returns
`EPERM`, and other non-zero `arg` values return `EINVAL` (see the Known
Limitations section of `docs/security.md`). If any step fails, the newly installed
session/terminal state is rolled back. A per-TTY association-transaction
lock serializes bind, unbind, and rollback, preventing an older
transaction from undoing a newer successful binding on the same TTY.

`TIOCSPGRP` reads the target PGID from user space, resolves the target
process group in the process table, and requires:

- the caller to belong to the session associated with the controlling
  terminal;
- the target process group to belong to the same session.

`TIOCGPGRP` and `TIOCGSID` return state only to callers in the session of
the controlling terminal; cross-session queries get `ENOTTY`. As in
Linux, a PTY master may query the job-control state of its paired slave.
Explicit foreground changes and the initial controlling-terminal bind
share `JobControl::set_foreground`, which wakes readers waiting for
foreground state changes on success.

TTY `open` also implements Linux's implicit controlling-terminal
acquisition: when `O_NOCTTY` is not set, the current user process is a
session leader, and no controlling TTY exists yet, the opened terminal
becomes the controlling terminal through the same `bind_to` flow and
initializes the foreground. PTY masters never take part in implicit or
explicit controlling-terminal binding; only a slave can become a
controlling TTY. Opens issued by kernel threads have no user session and
therefore never trigger the transition; a boot script's fallback shell
re-opens `/dev/console` in user mode.

## Calling Constraints

TTY reads use `kvfs::poll_file_io`: available input is attempted before waiting
and rechecked after signal interruption. A still-blocked read returns the
file-wait marker; the read syscall boundary and signal delivery decide whether
to restart or report EINTR. Line-discipline consumption remains responsible for
the returned byte count. This does not add output backpressure, VTIME handling,
or partial reads below VMIN; those remain outside the wait contract.

TTY ioctl paths require the current task to be a user-process thread,
because permission and session checks depend on `current_user_thread()`.
User pointers are read or written only at the ioctl boundary; internal
job-control logic receives only kernel-held PGIDs, `Session`, and
`ProcessGroup` values. The ioctl path must not be called from interrupt
context. TTY `open` may be called from kernel threads; in that case only
the file open completes and no controlling-terminal assignment is
attempted.

## Concurrency Model

The controlling-terminal session and foreground are each protected by
`SpinNoIrq`; a per-TTY association-transaction lock protects the
multi-phase binds and unbinds that span `Session`, `JobControl`, and the
foreground. State updates never cross a potentially blocking operation;
after a foreground change, waiters are woken through `PollSet`.
