// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::{Arc, Weak};
use core::{ops::Deref, sync::atomic::Ordering};

use kerrno::{KError, KResult, LinuxError};
use kpoll::{IoEvents, PollContext, PollRegisterError, Pollable};
use kprocess::{AsThread, ControllingTerminal, Pid, Process, ProcessGroup, SetTerminalResult};
use ksync::Mutex;
use ktask::future::block_on;
use kvfs::{DeviceFileOps, NodeFlags, VfsFile, VfsFileBuilder, VfsInode, poll_file_io};
use posix_types::{UserConstPtr, UserPtr};

use crate::{
    port::{InputIngress, TtyConfig, TtyPort, TtyRead, TtyWrite},
    terminal::{
        Terminal, WindowSize,
        ldisc::OutputLineDiscipline,
        termios::{Termios, Termios2},
    },
};

mod ntty;
mod pty;
mod pty_pair;
mod vt;

pub use ntty::{NTtyDriver, native_tty, try_handoff_console};
pub use pty::PtyDriver;
#[cfg(unittest)]
use pty::create_pty_pair;
pub use pty_pair::{
    PtyPair, open_ptmx, pty_allocated_count, pty_limit, pty_slave_inode_for, pty_slave_numbers_for,
    register_pty_drivers,
};
pub use vt::{VtConsole, register_fixed_vt_devices, vt_console};

fn current_tty_ops(file: &mut VfsFileBuilder) -> KResult<Arc<dyn DeviceFileOps>> {
    let term = kprocess::current_user_thread()
        .process()
        .group()
        .session()
        .terminal()
        .ok_or_else(|| KError::from(LinuxError::ENXIO))?;
    let term = term.into_any();

    let term = match term.downcast::<NTtyDriver>() {
        Ok(term) => return Ok(term),
        Err(term) => term,
    };
    let term = match term.downcast::<VtConsole>() {
        Ok(term) => return Ok(term),
        Err(term) => term,
    };
    match term.downcast::<PtyDriver>() {
        Ok(term) => {
            term.hold_pty_open(file)?;
            Ok(term)
        }
        Err(_) => Err(KError::OperationNotSupported),
    }
}

struct TtyFilePrivate {
    tty: Arc<dyn DeviceFileOps>,
}

impl TtyFilePrivate {
    fn new(tty: Arc<dyn DeviceFileOps>) -> Self {
        Self { tty }
    }
}

fn tty_file_private(file: &VfsFile) -> KResult<Arc<TtyFilePrivate>> {
    file.private_data_get::<TtyFilePrivate>()
        .ok_or(KError::InvalidInput)
}

/// TTY device combining terminal and line discipline
pub struct Tty<R, W> {
    this: Weak<Self>,
    terminal: Arc<Terminal>,
    input: Mutex<TtyPort<R, W>>,
    writer: W,
    output: Arc<Mutex<OutputLineDiscipline<W>>>,
    ingress: Arc<InputIngress>,
    kind: TtyKind,
    pty_pair: Option<Weak<PtyPair>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TtyKind {
    Terminal,
    PtyMaster,
    VirtualConsole,
}

/// `<linux/tty.h>` `N_TTY`: the only line discipline this kernel implements.
const N_TTY: i32 = 0;

impl<R: TtyRead, W: TtyWrite + Clone> Tty<R, W> {
    fn new(terminal: Arc<Terminal>, kind: TtyKind, config: TtyConfig<R, W>) -> KResult<Arc<Self>> {
        Self::new_with_pair(terminal, kind, config, None)
    }

    fn new_with_pair(
        terminal: Arc<Terminal>,
        kind: TtyKind,
        config: TtyConfig<R, W>,
        pty_pair: Option<Weak<PtyPair>>,
    ) -> KResult<Arc<Self>> {
        Self::new_with_flags(
            terminal,
            kind,
            config,
            pty_pair,
            kalloc_types::AllocFlags::KERNEL,
        )
    }

    fn new_with_flags(
        terminal: Arc<Terminal>,
        kind: TtyKind,
        config: TtyConfig<R, W>,
        pty_pair: Option<Weak<PtyPair>>,
        flags: kalloc_types::AllocFlags,
    ) -> KResult<Arc<Self>> {
        let writer = config.writer.clone();
        let port = TtyPort::new_with_flags(terminal.clone(), config, flags)?;
        let output = port.output.clone();
        let ingress = port.ingress.clone();
        let input = Mutex::new(port);
        Ok(Arc::new_cyclic(|this| Self {
            this: this.clone(),
            terminal,
            input,
            writer,
            output,
            ingress,
            kind,
            pty_pair,
        }))
    }
}

impl<R: TtyRead, W: TtyWrite> Tty<R, W> {
    async fn write_output(&self, is_nonblocking: bool, buf: &[u8]) -> KResult<usize> {
        let mut written = 0;
        while written < buf.len() {
            match poll_file_io(self, IoEvents::OUT, is_nonblocking, || {
                self.output.lock().write(&buf[written..])
            })
            .await
            {
                Ok(0) => break,
                Ok(count) => written += count,
                Err(_) if written != 0 => break,
                Err(error) => return Err(error),
            }
        }
        Ok(written)
    }

    fn flush_queues(&self, selector: u32) -> KResult<()> {
        match selector {
            linux_raw_sys::general::TCIFLUSH => self.input.lock().drain_input(),
            linux_raw_sys::general::TCOFLUSH => self.writer.flush_output(),
            linux_raw_sys::general::TCIOFLUSH => {
                self.input.lock().drain_input();
                self.writer.flush_output();
            }
            _ => return Err(KError::InvalidInput),
        }
        Ok(())
    }

    /// Console/keyboard/VT ioctls for the native serial console.
    ///
    /// The native console (`TtyKind::Terminal`) is a serial TTY with no real VT
    /// keyboard, LEDs, graphics mode, or modem lines, so these requests are
    /// accepted with sane defaults (text mode, XLATE keyboard, no packet or
    /// exclusive mode) instead of failing `ENOTTY`. This lets userspace console
    /// setup (e.g. systemd-executor) proceed.
    ///
    /// Returns `None` for any command it does not handle; the caller then passes
    /// those to the generic terminal path. The line-discipline pair is handled
    /// here rather than there because the console has exactly one discipline
    /// (`N_TTY`) to report, so it needs no discipline table.
    fn console_ioctl(&self, cmd: u32, arg: usize) -> Option<KResult<usize>> {
        use linux_raw_sys::ioctl::*;
        // Mode/keyboard values from <linux/kd.h> and <linux/vt.h> that
        // linux_raw_sys does not expose.
        const KD_TEXT: i32 = 0x00;
        // <linux/kd.h>: K_RAW=0x00, K_XLATE=0x01. The console reports XLATE.
        const K_XLATE: i32 = 0x01;
        const KB_101: i32 = 0x02;
        const N_TTY: i32 = 0;
        const TIOCSER_TEMT: i32 = 0x01;
        const KDSKBDIACR: u32 = 0x4B4B;
        if !matches!(
            cmd,
            FIONREAD
                | TIOCOUTQ
                | KDGETMODE
                | KDSETMODE
                | KDGKBMODE
                | KDSKBMODE
                | KDGKBMETA
                | KDSKBMETA
                | KDGKBLED
                | KDSKBLED
                | KDGKBTYPE
                | KDSIGACCEPT
                | KDGKBDIACR
                | KDSKBDIACR
                | KDGKBENT
                | KDSKBENT
                | KDGKBSENT
                | KDSKBSENT
                | KDGETKEYCODE
                | KDSETKEYCODE
                | KDMKTONE
                | KDENABIO
                | KDDISABIO
                | KDADDIO
                | KDDELIO
                | TIOCGPKT
                | TIOCPKT
                | TIOCEXCL
                | TIOCNXCL
                | TIOCGEXCL
                | TIOCVHANGUP
                | TIOCGLCKTRMIOS
                | TIOCSLCKTRMIOS
                | TIOCGSOFTCAR
                | TIOCSSOFTCAR
                | TIOCMGET
                | TIOCMSET
                | TIOCMBIS
                | TIOCMBIC
                | TIOCGETD
                | TIOCSETD
                | TIOCCONS
                | TIOCSERGETLSR
                | TIOCGICOUNT
                | TIOCMIWAIT
                | VT_OPENQRY
                | VT_GETSTATE
                | VT_GETMODE
                | VT_SETMODE
                | VT_ACTIVATE
                | VT_WAITACTIVE
                | VT_RELDISP
                | VT_DISALLOCATE
                | VT_RESIZE
                | VT_RESIZEX
        ) {
            return None;
        }
        Some((|| {
            match cmd {
                FIONREAD => {
                    UserPtr::<i32>::from(arg).write_vm(self.ingress.readable_bytes() as i32)?
                }
                TIOCOUTQ => UserPtr::<i32>::from(arg).write_vm(0)?,
                KDGETMODE => UserPtr::<i32>::from(arg).write_vm(KD_TEXT)?,
                KDGKBMODE => UserPtr::<i32>::from(arg).write_vm(K_XLATE)?,
                TIOCGETD => UserPtr::<i32>::from(arg).write_vm(N_TTY)?,
                // Linux stores these as a single char/unsigned char: writing an
                // `int` would corrupt the three bytes after the caller's buffer
                // (and fault when the char ends a writable mapping).
                KDGKBTYPE => UserPtr::<u8>::from(arg).write_vm(KB_101 as u8)?,
                KDGKBLED => UserPtr::<u8>::from(arg).write_vm(0)?,
                TIOCGSOFTCAR => UserPtr::<i32>::from(arg).write_vm(1)?,
                TIOCSERGETLSR => UserPtr::<i32>::from(arg).write_vm(TIOCSER_TEMT)?,
                KDGKBMETA | TIOCGPKT | TIOCGEXCL | TIOCMGET => {
                    UserPtr::<i32>::from(arg).write_vm(0)?
                }
                TIOCGLCKTRMIOS => UserPtr::<Termios>::from(arg)
                    .write_vm(*self.terminal.termios.lock().as_ref().deref())?,
                // struct serial_icounter_struct is 20 ints; all counters zero.
                TIOCGICOUNT => UserPtr::<[i32; 20]>::from(arg).write_vm([0; 20])?,
                VT_OPENQRY => UserPtr::<i32>::from(arg).write_vm(1)?,
                // struct vt_stat { active, signal, state }; report VT 1 active.
                VT_GETSTATE => UserPtr::<[u16; 3]>::from(arg).write_vm([1, 0, 0])?,
                // struct vt_mode zeroed == VT_AUTO with no switch signals.
                VT_GETMODE => UserPtr::<[u8; 8]>::from(arg).write_vm([0; 8])?,
                // VT keyboard-table getters have no serial-console backing.
                // Linux returns ENOTTY for them on a non-VT console rather than
                // reporting success with an untouched output buffer, so do the
                // same instead of falling into the no-op setter arm below.
                KDGKBDIACR | KDGKBENT | KDGKBSENT | KDGETKEYCODE => {
                    return Err(KError::NotATty);
                }
                // Remaining requests are setters or stateful operations with no
                // serial-console effect (keyboard mode/LEDs/tables, graphics
                // mode, packet/exclusive mode, modem lines, vhangup). Accept
                // them so console setup/teardown proceeds; full vhangup revoke
                // and real VT keyboard state remain follow-ups.
                _ => {}
            }
            Ok(0)
        })())
    }

    fn hold_pty_open(&self, file: &mut VfsFileBuilder) -> KResult<()> {
        if let Some(pair) = &self.pty_pair {
            let pair = pair.upgrade().ok_or(KError::Io)?;
            file.set_private_data(Arc::new(pair.acquire_open(self.kind)?));
        }
        Ok(())
    }

    /// Binds this TTY as `proc`'s controlling terminal.
    ///
    /// The process must be a session leader. A successful bind also initializes
    /// the terminal's foreground process group to the process's current group.
    ///
    /// # Errors
    ///
    /// Returns `EPERM` when `proc` is not a session leader, `EBUSY` when either
    /// the terminal or the session is already associated elsewhere, or the
    /// foreground-group error if the final state transition cannot complete.
    pub fn bind_to(self: &Arc<Self>, proc: &Process) -> KResult<()> {
        if self.kind == TtyKind::PtyMaster {
            return Err(KError::NotATty);
        }
        let _association = self.terminal.association.lock();
        let pg = proc.group();
        let session = pg.session();
        if session.sid() != proc.pid() {
            return Err(KError::OperationNotPermitted);
        }
        let terminal: Arc<dyn ControllingTerminal> = self.clone();
        let installed_job_session = self.terminal.job_control.ensure_session(&session)?;
        let installed_terminal = match session.set_terminal(&terminal) {
            SetTerminalResult::Installed => true,
            SetTerminalResult::AlreadySetToSame => false,
            SetTerminalResult::Occupied => {
                if installed_job_session {
                    self.terminal.job_control.clear_session_if_matches(&session);
                }
                return Err(KError::ResourceBusy);
            }
        };

        if let Err(err) = self.terminal.job_control.set_foreground(&pg) {
            if installed_terminal {
                session.unset_terminal(&terminal);
            }
            if installed_job_session {
                self.terminal.job_control.clear_session_if_matches(&session);
            }
            return Err(err);
        }
        Ok(())
    }

    fn unbind_from(self: &Arc<Self>, proc: &Process) -> bool {
        let _association = self.terminal.association.lock();
        let session = proc.group().session();
        let terminal: Arc<dyn ControllingTerminal> = self.clone();
        if !session.unset_terminal(&terminal) {
            return false;
        }
        self.terminal.job_control.clear_session_if_matches(&session);
        true
    }

    /// Steals this TTY as `proc`'s controlling terminal (`TIOCSCTTY` arg 1).
    ///
    /// Like [`Self::bind_to`] but forcibly reassigns a terminal that already
    /// belongs to another session. Linux restricts this to a privileged
    /// session leader; the previous session simply loses the terminal.
    ///
    /// # Errors
    ///
    /// Returns `EPERM` when `proc` is not a session leader or lacks
    /// `CAP_SYS_ADMIN`, and the foreground-group error if the final state
    /// transition cannot complete.
    pub fn steal_for(self: &Arc<Self>, proc: &Process) -> KResult<()> {
        if self.kind == TtyKind::PtyMaster {
            return Err(KError::NotATty);
        }
        let _association = self.terminal.association.lock();
        let pg = proc.group();
        let session = pg.session();
        // `tiocsctty`: a caller that is its session's leader and whose session
        // already owns this terminal succeeds without changing anything. The
        // leader test is part of the condition, not an extra one - a mere
        // member of the owning session must fall through to the `signal->tty`
        // check below and be refused.
        let tty_session = self.terminal.job_control.session();
        if session.sid() == proc.pid()
            && tty_session
                .as_ref()
                .is_some_and(|owner| Arc::ptr_eq(owner, &session))
        {
            return Ok(());
        }
        // "The process must be a session leader and not have a controlling tty
        // already."
        if session.sid() != proc.pid() || session.terminal().is_some() {
            return Err(KError::OperationNotPermitted);
        }
        // Stealing from a live session needs the capability; taking a terminal
        // that no session owns does not, as in Linux where only the
        // `tty->ctrl.session` branch checks `capable(CAP_SYS_ADMIN)`.
        if tty_session.is_some() && !kprocess::current_cred().has_sys_admin() {
            return Err(KError::OperationNotPermitted);
        }
        let terminal: Arc<dyn ControllingTerminal> = self.clone();
        // A steal *replaces* the current owner. `ensure_session` refuses with
        // EBUSY when a different session already owns the terminal, which is
        // exactly the case this path exists for, so the previous owner is
        // released first and restored if the rest of the acquisition fails.
        //
        // The alternative -- verifying the new occupant before releasing the
        // old one -- cannot work: the two claims are mutually exclusive by
        // construction, so the new one always sees the old one as an
        // occupant. What must be preserved instead is that a *failed* steal
        // never leaves the terminal with no session at all.
        let previous = self.terminal.job_control.session();
        let restored_previous = previous.clone();
        // `clear_session_if_matches` drops the foreground group together with
        // the session, so the previous owner's group is captured first: a
        // rollback that restored only the session would leave the terminal
        // owned but with no foreground group, which is the half-state this
        // whole ordering exists to prevent.
        let previous_foreground = self.terminal.job_control.foreground();
        if let Some(previous) = previous.as_ref()
            && !Arc::ptr_eq(previous, &session)
        {
            previous.unset_terminal(&terminal);
            self.terminal.job_control.clear_session_if_matches(previous);
        }
        // Undo everything this call installs and put the previous owner back,
        // so a rejected steal is a no-op rather than a terminal with neither.
        let restore = || {
            session.unset_terminal(&terminal);
            self.terminal.job_control.clear_session_if_matches(&session);
            if let Some(previous) = restored_previous.as_ref()
                && self.terminal.job_control.ensure_session(previous).is_ok()
            {
                let _ = previous.set_terminal(&terminal);
                if let Some(pg) = previous_foreground.as_ref() {
                    let _ = self.terminal.job_control.set_foreground(pg);
                }
            }
        };
        if let Err(err) = self.terminal.job_control.ensure_session(&session) {
            restore();
            return Err(err);
        }
        match session.set_terminal(&terminal) {
            SetTerminalResult::Installed | SetTerminalResult::AlreadySetToSame => {}
            SetTerminalResult::Occupied => {
                restore();
                return Err(KError::ResourceBusy);
            }
        }
        if let Err(err) = self.terminal.job_control.set_foreground(&pg) {
            restore();
            return Err(err);
        }
        Ok(())
    }

    /// Set `pg` as the foreground process group of this terminal.
    ///
    /// This supports explicit foreground-group changes after
    /// [`bind_to`](Self::bind_to) initializes the terminal to the session
    /// leader's process group.
    pub fn set_foreground(self: &Arc<Self>, pg: &Arc<ProcessGroup>) -> KResult<()> {
        self.terminal.job_control.set_foreground(pg)
    }

    /// Get the pseudo-terminal slave number
    pub fn pty_number(&self) -> u32 {
        self.terminal.pty_number.load(Ordering::Acquire)
    }

    /// Set the pseudo-terminal slave number.
    fn set_pty_number(&self, n: u32) {
        self.terminal.pty_number.store(n, Ordering::Release);
    }

    /// Termios/line-discipline/PTY requests shared by every tty kind.
    ///
    /// Runs in the caller's task context, like `Tty::ioctl`: the job-control
    /// requests (`TIOCSPGRP`, `TIOCSCTTY`, `TIOCNOTTY`) read the current user
    /// thread and process and therefore require a user task — a kernel task
    /// must not reach this path. May sleep while reading or writing user
    /// memory, so it must not run in IRQ context, and callers must not hold tty
    /// or job-control locks across the call (the handlers take them
    /// themselves).
    fn terminal_ioctl(&self, cmd: u32, arg: usize) -> KResult<usize> {
        use linux_raw_sys::ioctl::*;
        match cmd {
            // Generic N_TTY requests serve every tty kind (Linux
            // n_tty_ioctl_helper): every endpoint owns an ingress, has no
            // pending output buffer of its own and runs the only discipline.
            FIONREAD => UserPtr::<i32>::from(arg).write_vm(self.ingress.readable_bytes() as i32)?,
            TIOCOUTQ => UserPtr::<i32>::from(arg).write_vm(0)?,
            TIOCGETD => UserPtr::<i32>::from(arg).write_vm(N_TTY)?,
            // Linux's `tty_set_ldisc` succeeds when the requested discipline is
            // already N_TTY and rejects anything this kernel does not
            // implement, so console setup that explicitly restores N_TTY works.
            TIOCSETD => {
                if UserConstPtr::<i32>::from(arg).read_vm()? != N_TTY {
                    return Err(KError::InvalidInput);
                }
            }
            TCGETS => UserPtr::<Termios>::from(arg)
                .write_vm(*self.terminal.termios.lock().as_ref().deref())?,
            TCGETS2 => {
                UserPtr::<Termios2>::from(arg).write_vm(*self.terminal.termios.lock().as_ref())?
            }
            TCSETS | TCSETSF | TCSETSW => {
                *self.terminal.termios.lock() =
                    Arc::new(Termios2::new(UserConstPtr::<Termios>::from(arg).read_vm()?));
                if cmd == TCSETSF {
                    self.input.lock().drain_input();
                }
                self.writer.notify_tx();
                // Shared PTY termios also changes the peer's output requirements.
                self.ingress.notify_space();
            }
            TCSETS2 | TCSETSF2 | TCSETSW2 => {
                *self.terminal.termios.lock() =
                    Arc::new(UserConstPtr::<Termios2>::from(arg).read_vm()?);
                if cmd == TCSETSF2 {
                    self.input.lock().drain_input();
                }
                self.writer.notify_tx();
                self.ingress.notify_space();
            }
            // TCSBRK: send break / drain output. This virtual terminal has no
            // physical serial line and no kernel output buffer, so it is an
            // intentional no-op (including the `arg == 0` drain-output case).
            TCSBRK => {}
            TCFLSH => self.flush_queues(arg as u32)?,
            TIOCGPGRP => {
                let foreground = if self.kind == TtyKind::PtyMaster {
                    self.terminal
                        .job_control
                        .foreground()
                        .ok_or(KError::NotATty)?
                } else {
                    let caller_session = kprocess::current_user_process().group().session();
                    self.terminal.job_control.foreground_for(&caller_session)?
                };
                UserPtr::<i32>::from(arg).write_vm(foreground.pgid() as i32)?;
            }
            TIOCGSID => {
                let session = if self.kind == TtyKind::PtyMaster {
                    self.terminal.job_control.session().ok_or(KError::NotATty)?
                } else {
                    let caller_session = kprocess::current_user_process().group().session();
                    self.terminal.job_control.session_for(&caller_session)?
                };
                UserPtr::<u32>::from(arg).write_vm(session.sid())?;
            }
            TIOCSPGRP => {
                let pgid = UserConstPtr::<i32>::from(arg).read_vm()?;
                if pgid <= 0 {
                    return Err(KError::InvalidInput);
                }
                let current_process = kprocess::current_user_thread().process().clone();
                let current_session = current_process.group().session();
                let target_group = kprocess::job_control::target_group(pgid as Pid)?;
                self.terminal
                    .job_control
                    .set_foreground_for(&current_session, &target_group)?;
            }
            TIOCGWINSZ => {
                UserPtr::<WindowSize>::from(arg).write_vm(*self.terminal.window_size.lock())?
            }
            TIOCSWINSZ => {
                *self.terminal.window_size.lock() =
                    UserConstPtr::<WindowSize>::from(arg).read_vm()?;
            }
            TIOCSPTLCK => {}
            TIOCGPTN => UserPtr::<u32>::from(arg).write_vm(self.pty_number())?,
            TIOCSCTTY => {
                let tty = self.this.upgrade().ok_or(KError::NoSuchDevice)?;
                let current_process = kprocess::current_user_thread().process().clone();
                if arg == 0 {
                    tty.bind_to(&current_process)?;
                } else if arg == 1 {
                    // Linux privileged terminal stealing: greetd reassigns a VT
                    // that a getty already holds as its controlling terminal.
                    tty.steal_for(&current_process)?;
                } else {
                    return Err(KError::InvalidInput);
                }
            }
            TIOCNOTTY => {
                let tty = self.this.upgrade().ok_or(KError::NoSuchDevice)?;
                let current_process = kprocess::current_user_thread().process().clone();
                if tty.unbind_from(&current_process) {
                    // TODO: If the process was session leader, send SIGHUP and
                    // SIGCONT to the foreground process group and all processes
                    // in the current session lose their controlling terminal.
                } else {
                    warn!("Failed to unset terminal");
                }
            }
            _ => return Err(KError::NotATty),
        }
        Ok(0)
    }
}

impl<R: TtyRead, W: TtyWrite> ControllingTerminal for Tty<R, W> {
    fn into_any(self: Arc<Self>) -> Arc<dyn core::any::Any + Send + Sync> {
        self
    }
}

impl<R: TtyRead, W: TtyWrite> DeviceFileOps for Tty<R, W> {
    fn open(&self, _inode: &VfsInode, file: &mut VfsFileBuilder) -> KResult<()> {
        self.hold_pty_open(file)?;
        // A blocked reader must not serialize writers sharing this open file.
        file.stream_open();
        if self.kind == TtyKind::PtyMaster || file.requests_no_controlling_tty() {
            return Ok(());
        }

        let current = ktask::current();
        let Some(thread) = current.try_as_thread() else {
            // Kernel-side opens, including PID 1 stdio construction, do not
            // have a user process whose session could acquire this terminal.
            return Ok(());
        };
        let process = thread.process();
        let session = process.group().session();
        if session.sid() != process.pid() || session.terminal().is_some() {
            return Ok(());
        }

        let tty = self.this.upgrade().ok_or(KError::NoSuchDevice)?;
        // Linux controlling-terminal assignment during open is best-effort:
        // losing a race to another terminal/session must not fail the open.
        if let Err(err) = tty.bind_to(process) {
            match err {
                KError::ResourceBusy | KError::OperationNotPermitted => debug!(
                    "tty: best-effort controlling-terminal assignment lost a race: pid={} sid={} \
                     error={:?}",
                    process.pid(),
                    session.sid(),
                    err
                ),
                _ => warn!(
                    "tty: unexpected controlling-terminal assignment failure: pid={} sid={} \
                     error={:?}",
                    process.pid(),
                    session.sid(),
                    err
                ),
            }
        }
        Ok(())
    }

    fn supports_read(&self) -> bool {
        true
    }

    fn supports_write(&self) -> bool {
        true
    }

    fn read(&self, file: &VfsFile, buf: &mut [u8], _offset: u64) -> KResult<usize> {
        if self.kind == TtyKind::VirtualConsole {
            return if buf.is_empty() {
                Ok(0)
            } else {
                Err(KError::Io)
            };
        }
        block_on(poll_file_io(
            self,
            IoEvents::IN,
            file.is_nonblocking(),
            || {
                if self.kind == TtyKind::PtyMaster
                    || self.terminal.job_control.current_in_foreground()
                {
                    let mut input = self.input.lock();
                    if self.writer.is_closed() {
                        input.read_after_peer_close(buf)
                    } else {
                        input.read(buf)
                    }
                } else if self.writer.is_closed() {
                    Err(KError::Io)
                } else {
                    // TODO: a background process group reading the controlling
                    // terminal should receive SIGTTIN (or `EIO` when SIGTTIN is
                    // ignored/blocked or the group is orphaned), per POSIX. For now
                    // while the peer remains open, we silently block until the
                    // reader reaches the foreground, which can stall job control.
                    Err(KError::WouldBlock)
                }
            },
        ))
    }

    fn write(&self, file: &VfsFile, buf: &[u8], _offset: u64) -> KResult<usize> {
        if self.kind == TtyKind::VirtualConsole {
            // Share the producer lock with echo. VtOutput owns processing,
            // so this discipline bypasses a second newline conversion.
            return self.output.lock().write(buf);
        }
        block_on(self.write_output(file.is_nonblocking(), buf))
    }

    fn ioctl(&self, _file: &VfsFile, cred: &kcred::Cred, cmd: u32, arg: usize) -> KResult<usize> {
        if self.kind == TtyKind::VirtualConsole
            && let Some(result) = vt::ioctl(self, cred, cmd, arg)
        {
            return result;
        }
        // Only the native serial console accepts the console/KB/VT class; a
        // pty slave is also `TtyKind::Terminal`, but Linux keeps KD*/VT*/
        // TIOCPKT off ptys (packet mode on a master is real framing there), so
        // both pty endpoints must keep returning ENOTTY for it.
        if self.kind == TtyKind::Terminal
            && self.pty_pair.is_none()
            && let Some(result) = self.console_ioctl(cmd, arg)
        {
            return result;
        }
        self.terminal_ioctl(cmd, arg)
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
    }

    fn poll(&self, _file: &VfsFile) -> Option<IoEvents> {
        Some(Pollable::poll(self))
    }

    fn register_poll(
        &self,
        _file: &VfsFile,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        Pollable::register(self, context, events)
    }
}

impl<R: TtyRead, W: TtyWrite> Pollable for Tty<R, W> {
    fn poll(&self) -> IoEvents {
        let mut events = self.terminal.job_control.poll();
        if self.kind == TtyKind::PtyMaster || events.contains(IoEvents::IN) {
            events.set(IoEvents::IN, self.input.lock().poll_read());
        }
        events.set(IoEvents::OUT, self.output.lock().write_room() != 0);
        if self.writer.is_closed() {
            events |= IoEvents::ERR | IoEvents::HUP;
        }
        if self.kind == TtyKind::VirtualConsole {
            // The text output remains usable, but no VT input source is wired.
            events |= IoEvents::ERR;
        }
        events
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if self.kind != TtyKind::PtyMaster {
            self.terminal.job_control.register(context, events)?;
        }
        if events.contains(IoEvents::IN) {
            self.input.lock().register_rx(context)?;
        }
        // Peer closure reports ERR/HUP regardless of the requested event mask.
        self.writer.register_tx(context)?;
        Ok(())
    }
}

/// /dev/tty device - refers to the calling process's controlling terminal
pub struct CurrentTty;
impl DeviceFileOps for CurrentTty {
    fn open(&self, _inode: &VfsInode, file: &mut VfsFileBuilder) -> KResult<()> {
        let tty = current_tty_ops(file)?;
        file.stream_open();
        file.set_nonblocking(true);
        file.set_private_data(Arc::new(TtyFilePrivate::new(tty)));
        Ok(())
    }

    fn supports_read(&self) -> bool {
        true
    }

    fn supports_write(&self) -> bool {
        true
    }

    fn read(&self, file: &VfsFile, buf: &mut [u8], offset: u64) -> KResult<usize> {
        let tty = tty_file_private(file)?;
        tty.tty.read(file, buf, offset)
    }

    fn write(&self, file: &VfsFile, buf: &[u8], offset: u64) -> KResult<usize> {
        let tty = tty_file_private(file)?;
        tty.tty.write(file, buf, offset)
    }

    fn ioctl(&self, file: &VfsFile, cred: &kcred::Cred, cmd: u32, arg: usize) -> KResult<usize> {
        let tty = tty_file_private(file)?;
        tty.tty.ioctl(file, cred, cmd, arg)
    }

    fn poll(&self, file: &VfsFile) -> Option<IoEvents> {
        // `/dev/tty` is a tty and its file type always provides a poll handler,
        // exactly as `tty_fops` does in Linux. The error arm models a file that
        // never went through `CurrentTty::open`, i.e. one that is not a
        // `/dev/tty` at all; reporting it as "not ready" keeps `epoll_ctl`
        // accepting the node and leaves the `poll`/`select` fallback alone,
        // whereas `None` would mean "no handler" and turn that fallback into
        // always-ready.
        match tty_file_private(file) {
            Ok(tty) => tty.tty.poll(file),
            Err(_) => Some(IoEvents::empty()),
        }
    }

    fn register_poll(
        &self,
        file: &VfsFile,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if let Ok(tty) = tty_file_private(file) {
            tty.tty.register_poll(file, context, events)
        } else {
            Ok(())
        }
    }
}

unittest::path_mod_test!(tests_pty, "../../ut-tests/tty_pty.rs");

unittest::path_mod_test!(tests, "../../ut-tests/tty.rs");
