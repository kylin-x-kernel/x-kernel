# ktty — Design

## Native Console Input Work

Native consoles with an input IRQ use `ProcessMode::Work` and a prepared
`kworkng::WorkHandle`; they do not subscribe to generic IRQ notifications.
Preparation requires sleepable context after normal-worker initialization.
Late-init attaches an input Waker to the stdout port before userspace opens TTY.
Consoles without an IRQ retain Manual mode. PTY slaves use the same Work mode;
master writes notify slave input, while slave output notifies master readability.
Creation connects both directions before an initial input check. No dedicated
per-PTY input task is spawned.

The port callback owns `Mutex<InputReader>` (backend prefetch and its line
discipline) and a readable PollSet, but no strong reference back to `TtyPort`
or its work. Weak self-requeue shares
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

## Input Port And Line Discipline

`TtyPort<R, W>` owns the backend reader, one 80-byte prefetch batch, processing
mode, prepared work, and readable notifications.
Existing UART/PTY queues remain the raw-input storage; there is no additional
4 KiB queue at the TTY boundary. `InputReader` retains a partially consumed batch.

`LineDiscipline<W>` owns canonical editing and partial-line delivery state. Its
`receive(bytes, input_budget, output_budget)` consumes a prefix and reports both
consumed and published byte counts and the stopping reason. Pending canonical
output can be delivered with an empty input slice. The line discipline neither
reads a device nor selects a CPU, creates work or waits for backend readiness.
Terminal read policy (canonical/VMIN/VTIME interpretation) also stays in ldisc;
the port controls when to retry processing and notify waiters.

```text
UART / PTY raw queue -> port prefetch -> ldisc.receive -> cooked queue -> read
                             ^                              |
                             +------ output-space retry ----+
```

The current adapters remain pull-based: `TtyRead` returns immediately with at
most the supplied slice length. `TtyWrite` handles task-context output/echo.
Native console binding is host integration in `tty/ntty.rs`; `WorkHandle` is
not exposed to the serial driver. A private `InputWake` weakly references
`InputWork`, which owns the fixed CPU route and an optional prepared work handle.
Source notifications, initial kicks, output-space retries and budget requeues
submit through that owner's short IRQ-safe gate. Closing takes the handle out
under the same gate and drops it after unlocking. Stale notifications cannot
restart processing, even if the owner object remains referenced.

Dropping the port closes its input owner without waiting. A running batch may
finish against its original transport; kworkng drains and destroys captures in
task context. No synchronous wait occurs in port destruction. Master bypass
processing and existing PTY overflow limitations remain unchanged. Device-node
and open-file lifecycle must release or explicitly close processing owners;
work reclamation alone does not return an allocated PTY number.
No backend switching or hot-unplug is implemented.
TTY endpoint identity is selected separately at construction: a native terminal
or PTY slave has terminal semantics, while a PTY master has master semantics.
Job-control and controlling-terminal checks use that identity, not the input
processing mode; choosing bypass processing alone does not make a TTY a master.
Console, PTY endpoints and `/dev/tty` open as non-seekable streams. VFS does
not hold the file-position mutex across stream I/O, so a blocked read cannot
prevent writes through a duplicated descriptor of the same open file.
A future push adapter can feed the same receive/progress interface without
changing terminal parsing, but must own its queue and connection shutdown.

## Native Backend Connection

The native reader, writer and echo share `Arc<ConsoleConnection>`. It owns the
selected early SerialPort and an optional runtime character-device handle for
that same stdout identity. Notification attaches to this selected port. Per-I/O
global active-console lookup and the separate atomic handover-state enum are
removed; global console selection no longer redirects an established native TTY.

Late-init adopts the registered stdout runtime handle, not whichever console is
active. Missing runtime registration leaves boot access usable. Runtime errors
or unavailability clear the optional handle and fall back to the same port;
later explicit handoff can retry. Connection locking only snapshots/replaces
handles; device I/O, loss reporting and resource destruction occur outside it.
An in-flight snapshot may complete while another caller falls back, but both
access paths share the same port and receive queue. The no-port case retains HAL
fallback and manual processing. No dynamic rebinding or hot-unplug is added.

## Terminal Layer

`ktty` implements the X-Kernel terminal layer: terminal devices, the line
discipline, pseudo-terminals, and the POSIX job-control interface. The
device-file layer forwards `/dev/console`, `/dev/tty`, and PTY requests to
this crate; process, process-group, and session identity stay owned by
`kprocess`.

## PTY Ownership

`open_ptmx` owns allocation, master open and slave publication as one operation.
The filesystem supplies only an unpublished inode constructor, called without
PTY locks held; it must retain the pair weakly. Allocation, master open and
publication helpers are private so callers cannot reorder the lifecycle.
`register_pty_drivers` owns the Unix98 device ranges and rejects numeric aliases
with EIO: masters require PTMX, and slaves require an original-pair node.
devfs invokes registration at initialization but does not implement PTY policy.

`PtyPair` owns the original master/slave endpoints, a persistent slave inode,
input submission owner and a reserved number. The registry uses an intrusive
tree for published pairs and a bitmap for all reservations, including pairs
under construction. The system-wide capacity is fixed at 16 pairs; no build-time
configuration or runtime setter is provided. Exhaustion returns ENOSPC without
waiting for another close. Allocation and node construction happen outside the
registry lock. A fixed bitmap records reservations; usage is counted directly
from that bitmap under the same lock, without a separate accounting state.

`pty_limit` and `pty_allocated_count` supply procfs read-only decimal queries at
`/proc/sys/kernel/pty/max` and `nr`. Occupancy includes unpublished reservations
and master-closed pairs with surviving slave files. It is a snapshot, not a promise
that a later allocation will succeed. Unlike Linux's writable max, both X-Kernel
nodes are read-only, including writes by root.

One `PtyOpen` in file-private data owns each open-file use. Builder failure and
final VfsFile destruction release that use; dup/fork share the same VfsFile.
`/dev/tty` captures an endpoint but must independently acquire a use through the
same pair admission check. Neither session references nor inode references are
open-file uses. Endpoints and slave-node openers weakly refer to their original
pair, never selecting a new pair by a reused number.

Final master release hides the slave from directory lookup/enumeration and
rejects subsequent opens with EIO, including opens through retained nodes.
Existing slave files retain the number. Final endpoint-use release detaches the
pair from the registry, closes input submissions and clears its matching session
association; detached endpoints/inode are released before returning the number.
Keeping a snapshot of the old pair does not restore admission or reserve its
old number. A running input round may finish against old buffers; work storage
reclamation is delegated to kworkng rather than delaying future allocation.

Pair state uses a sleeping mutex. Its only nested registry operation takes the
short SpinNoPreempt registry lock; lookup drops the registry lock before taking
pair state. Backend callbacks, wakeups and final resource destruction occur
outside both locks. Open, lookup and final file-use release require sleepable
task context, consistent with VFS inode destruction. The nonblocking input-owner
close operation does not make destruction of a retained inode IRQ-safe.

## Scope

- `src/terminal/job.rs`: controlling-terminal session association, the
  foreground process group, and foreground-change notification.
- `src/port.rs`: backend contracts, prefetch, processing modes, work and wakeup.
- `src/terminal/ldisc.rs`: supplied-byte processing, canonical mode, terminal
  read policy, echo and signal character handling.
- `src/tests/port.rs`: terminal semantics and input transport regression tests.
- `src/terminal/termios.rs`: the `Termios` terminal attribute layout and
  flag/character constants shared by the ioctl boundary.
- `src/tty/mod.rs`: TTY file operations and the `TC*`/`TIOC*` ioctl
  boundary.
- `src/tty/ntty.rs`, `src/tty/pty.rs`: the console TTY and PTY backends.
- `src/tty/pty_pair.rs`: PTY admission, open-file uses, registry and number ownership.

## Non-Responsibilities

- No terminal device drivers: UART hardware belongs to the console/serial
  drivers; `ktty` consumes byte streams through the TTY backend trait.
- No process, session, or credential ownership: sessions and process
  groups live in `kprocess`; `ktty` only holds weak references and asks
  it to resolve PGIDs.
- No devfs node construction: devfs projects each PTY as a persistent slave inode;
  ktty owns allocation/admission and retains the inode without constructing it.
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
