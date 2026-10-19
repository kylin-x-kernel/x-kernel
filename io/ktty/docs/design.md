# ktty — Design

## Guest Test Layout

Guest tests live in crate-root `ut-tests/`, mounted with
`unittest::path_mod_test!` under their original private module names and
`cfg(unittest)`. Parent access and registration are unchanged, including
the previously included port, connection and PTY fixtures.

| Module | File under `ut-tests/` | Cases |
| --- | --- | --- |
| `port::tests` | `port.rs` | 20 |
| `terminal::terminal_tests` | `terminal.rs` | 3 |
| `terminal::termios::termios_tests` | `terminal_termios.rs` | 7 |
| `tty::tests` | `tty.rs` | 2 |
| `tty::tests_pty` | `tty_pty.rs` | 7 |
| `tty::ntty::tests` | `tty_ntty.rs` | 5 |
| `tty::pty_pair::tests` | `pty_lifecycle.rs` | 7 |
| `tty::vt::tests` | `tty_vt.rs` | 5 |

The eight modules contain 56 cases, with no additional architecture/feature
gates. Nine cases retain `serial`, four retain `user`, and three carry both.
The two native-connection cases still return Ignored when runtime stdout is
unavailable. Test-only production hooks stay at their original owners.
Run through the configured kernel guest unittest workflow, not host Cargo
integration-test discovery.

## Fixed Virtual Console

`tty/vt.rs` creates an independent `Tty<VtInput, VtOutput>` with its own
`Terminal`, session/job-control, ingress and generic line discipline. tty0 and
tty1 alias this VT; `CurrentTty` preserves its identity and ioctl dispatch.
It shares only the native console's physical byte output connection, never
N_TTY's terminal/session or serial input. There is no VT keyboard byte source:
nonempty reads return EIO; output is writable and poll also reports input ERR.

VT_SETMODE accepts AUTO/PROCESS, retains waitv/relsig/acqsig, and sets ignored
frsig to zero, as Linux v6.12 vt_ioctl.c does. PROCESS records the actual current
process as a Weak<Process> after authorization and user-copy validation. Closing
an fd does not release this ownership. VT_GETMODE reads the registered mode;
on subsequent ioctl/output use a disappeared or exited controller resets the
console to AUTO/TEXT/default Unicode policy. Numeric PID reuse cannot address
another process because ownership is an object identity.

Only VT 1 exists, so VT_ACTIVATE/VT_WAITACTIVE accept 1 and reject other targets.
Linux change_console returns immediately for the already active VT, without
relsig/acqsig. Accordingly this implementation invents no switch or signals.
VT_RELDISP implements Linux vt_reldisp's no-pending branch: PROCESS+VT_ACKACQ
succeeds, AUTO or release 0/1 without a pending switch returns EINVAL. Cross-VT
release/acquire and signal-driven switching are not implemented. kprocess has a
real process signal channel; absence of a second VT, not absence of PID lookup,
is the boundary. VT_GETSTATE reports active=1 and v_state=3: Linux vt_ioctl.c
initializes state=1 for tty0's current-VT alias, then mask=2 for VT 1. Bits 0
and 1 therefore describe the same console here, not two distinct VTs. Bit 2
is clear: VT 2 does not exist and cannot be activated.
VT_OPENQRY succeeds with output -1 for no free VT; callers must inspect that
output instead of treating it as an activatable number.

KD_TEXT sends this VT's text to the native output connection; KD_GRAPHICS
consumes writes without emitting that text. The state mutex serializes mode
changes with byte output. KDGETMODE reflects this live policy. This is a
headless text-plane behavior, not a GPU text renderer, framebuffer save/restore
or hardware blanking implementation. It does not suppress N_TTY's independent
serial output or kernel logging.

Program writes pass through the same OutputLineDiscipline mutex as echo.
VtOutput declares backend-owned processing, so that discipline bypasses its
newline conversion and still calls the same TEXT/GRAPHICS policy. Nonempty
backend writes returning zero are normalized to WouldBlock; empty writes
return zero without calling the backend.

KDSKBMODE admits K_UNICODE/K_OFF, and KDGKBMODE reflects the registered policy.
No VT keycode translator or keyboard input consumer is connected; serial input
is not redirected. RAW/XLATE/MEDIUMRAW return EOPNOTSUPP, invalid values EINVAL.
Unicode is intentional: Linux v6.12 vt.c declares default_utf8=true and
keyboard.c::kbd_init selects VC_UNICODE when that setting is true. Other Linux
configurations can select XLATE; this headless interface does not implement it.

All VT/KD setters, activation/wait and VT_RELDISP share authorization:
current controlling-terminal object equality OR has_sys_tty_config authority,
following Linux vt_ioctl.c. kcred currently approximates that capability with
EUID 0; full capability sets/user-namespace semantics remain outside this PR.
User memory copies occur outside the mode-state mutex. Lock order for output
is output discipline -> VT state -> native byte connection; no path acquires
the output or terminal association locks under VT state. Native initialization is deferred
until actual TEXT output, so registering VT nodes does not initialize N_TTY.

TTY owns 4:0/4:1 registration through named KVFS cdev identities. Partial failure
rolls back the first alias; devfs projects handles, procfs snapshots and sysfs
subscribes. Registration failure never creates a hardcoded filesystem identity.

seatd 0.9.1 uses PROCESS -> K_OFF -> KD_GRAPHICS on opening a client; it closes
the fd but keeps process ownership, and restores Unicode/TEXT on cleanup.
Initialization does not wait for VT signals. Its terminal helpers return errors,
but seat.c::vt_open ignores those three helper results; success of seatd alone
would therefore not prove all three ioctls succeeded. Validation must also
check their actual return values and resulting state. Multi-VT behavior is out
of scope; current runtime validation results are recorded in PR material.

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
output-full stop processing; budget exhaustion requeues. Reads resubmit when
cooked capacity becomes available; input flush serializes with the worker and
wakes blocked ingress producers. kworkng owns pending/running
state; TTY does not duplicate it.

Echo and signal delivery run in task context under the reader mutex, without
port or TTY spinlocks. Wake and requeue occur after releasing that mutex.
Canonical input retains command spaces and tabs. Discarding a pending canonical
line also resets its delivery offset. The existing canonical-line Vec policy
is unchanged; a per-round budget is not a maximum line length.

## Input Port And Line Discipline

`TtyPort<R, W>` owns bounded ingress, processing mode, prepared work, and
readable notifications. Every port uses a fixed 4096-byte `InputIngress`.
This is a port-owned receive queue analogous to a Linux TTY flip buffer,
not a line-discipline buffer or a writer-private cache. Its readable PollSet
serves raw master reads; its space PollSet serves peer writers; its input Waker
schedules the receiving port's worker. Cooked readability and canonical state
remain separate and belong to the line discipline and processing port.
PTY writers push directly into the peer port's shared ingress and return the
accepted prefix. Native pull adapters stage source bytes into their port's
ingress before processing; upstream UART storage remains driver-owned.
`InputReader` retains at most one partially consumed 80-byte processing batch.
Master bypass reads ingress directly, without prefetch, CRLF expansion, or a
second readable queue. PTY master poll only observes queued bytes.

`LineDiscipline<W>` owns canonical editing and partial-line delivery state. Its
`receive(bytes, input_budget, output_budget)` consumes a prefix and reports both
consumed and published byte counts and the stopping reason. Pending canonical
output can be delivered with an empty input slice. The line discipline neither
reads a device nor selects a CPU, creates work or waits for backend readiness.
Terminal read policy (canonical/VMIN/VTIME interpretation) also stays in ldisc;
the port controls when to retry processing and notify waiters.

```text
native pull / PTY push -> port ingress -> ldisc.receive -> cooked queue -> read
                              |              ^                 |
                              |              +-- work retry ---+
                              +-> master raw read
ingress consumption / flush -> space notification -> peer writer retry
```

`TtyRead::input_ingress` shares a push source's receiver-owned queue; otherwise
the port uses the adapter's immediate, slice-bounded `read` method.
`TtyWrite::write` returns accepted bytes, `write_room` reports byte capacity,
and `register_tx` registers space notifications. Native console output is
explicitly best effort, unlike the PTY transport.
Native console binding is host integration in `tty/ntty.rs`; `WorkHandle` is
not exposed to the serial driver. A private `InputWake` weakly references
`InputWork`, which owns the fixed CPU route and an optional prepared work handle.
Source notifications, initial kicks, output-space retries and budget requeues
submit through that owner's short IRQ-safe gate. Closing takes the handle out
under the same gate and drops it after unlocking. Stale notifications cannot
restart processing, even if the owner object remains referenced.

Dropping the port closes its input owner without waiting. A running batch may
finish against its original transport; kworkng drains and destroys captures in
task context. No synchronous wait occurs in port destruction. Closing the master
closes its ingress and wakes blocked slave writers with a transport error.
Device-node
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
Push adapters share ingress ownership without changing terminal parsing.

## Output Processing And Backpressure

Program output and echo share one output-line-discipline mutex per endpoint.
PTY slave output expands LF to CRLF only when both OPOST and ONLCR are set
at write time; master-to-slave input bypasses this output processing. Room is
checked for each complete output unit before publishing, so CRLF never needs
a retained suffix. Write results count original input bytes, not expanded bytes.
No other producer writes the PTY transport outside this output mutex.
The TtyWrite contract guarantees full acceptance for a unit fitting write_room
when producers are serialized. A closure race may return an error before any
publication; it cannot produce a short CRLF followed by a duplicate retry.
Plain-byte runs are submitted in batches; only LF requires a separate CRLF unit.
Native console writers declare backend-owned output processing, so the TTY
passes their output through in a batch and does not expand LF a second time.
The existing serial backend policy also serves kernel logs and remains unchanged.

Nonblocking writes return the accepted prefix, or `WouldBlock` if nothing fits.
Blocking writes retry remaining input through `poll_file_io`, waiting on peer
ingress space; interruption or error preserves already accepted progress.
Ingress reads and input flush wake writers after releasing the queue lock.
OUT readiness reserves a complete two-byte unit when CRLF conversion is enabled;
otherwise any free byte suffices. Termios changes wake output waiters to recheck.
PTY endpoints share termios, so TCSETS* notifies both the writer's peer ingress
space and the local ingress space used by the opposite endpoint's writer.
Transport space/closure notifications are registered independently of the
requested poll mask, allowing input-only and hangup-only waiters to observe
ERR/HUP when the peer receive queue closes.
After peer closure, reads acquire the input-reader mutex and synchronously
process ingress, prefetch and pending canonical delivery until input is exhausted
or the cooked queue is full. They return cooked bytes without waiting for VMIN;
subsequent reads continue processing retained input before returning EIO.
Incomplete canonical lines retain the existing requirement for a line delimiter;
closure does not synthesize one or turn terminal control characters into data.
Background readers report EIO immediately after peer closure without consuming
foreground input; while the peer remains open, they retain foreground waiting.
Echo remains best effort because input processing cannot block on its peer's
output capacity. It is not part of the program-write delivery guarantee.

TCIFLUSH, the input side of TCIOFLUSH and input-flushing termios updates clear ingress, processing
prefetch, canonical state and cooked bytes while holding the input-reader mutex.
Newly pushed bytes after the ingress clear belong to the next input epoch.
TCOFLUSH clears the peer port's unconsumed ingress, in either direction,
matching Linux PTY driver flushing of the peer flip buffer. TCIOFLUSH performs
both local input flush and peer ingress flush. Bytes already transferred into
the peer's processing batch, canonical state or cooked queue are past the
output-flush boundary; clearing those requires a peer input flush. The two
directions are cleared independently, without nesting endpoint reader locks.
Clearing ingress releases its queue lock before waking blocked producers.
Concurrent bytes pushed after each clear belong to the next input epoch.
Console output flush remains a no-op; native hardware queues remain source-owned.

The PTY regressions use manual slave processing to make saturation deterministic
while exercising the production write loop and poll registrations. They cover
multi-capacity delivery, partial nonblocking retry, reader-triggered writer wake,
atomic CRLF publication, bidirectional output flush wake and input/combined flush.
The existing Work-mode case covers deferred slave input and echo. Console and
port regressions cover backend-owned conversion and batching of plain bytes.

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
- `ut-tests/port.rs`: terminal semantics and input transport regression tests.
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
the returned byte count. This does not add VTIME handling or partial reads
below VMIN; those remain outside the wait contract.

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
