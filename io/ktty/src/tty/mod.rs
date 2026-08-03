// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::{Arc, Weak};
use core::{ops::Deref, sync::atomic::Ordering};

use kerrno::{KError, KResult, LinuxError};
use kpoll::{IoEvents, PollContext, PollRegisterError, Pollable};
use kprocess::{AsThread, ControllingTerminal, Pid, Process, ProcessGroup, SetTerminalResult};
use ksync::Mutex;
use ktask::future::{block_on, poll_io};
use kvfs::{DeviceFileOps, NodeFlags, VfsFile, VfsFileBuilder, VfsInode};
use osvm::{VirtMutPtr, VirtPtr};

use crate::terminal::{
    Terminal, WindowSize,
    ldisc::{LineDiscipline, ProcessMode, TtyConfig, TtyRead, TtyWrite},
    termios::{Termios, Termios2},
};

mod ntty;
mod pty;

pub use ntty::{N_TTY, NTtyDriver, try_handoff_console};
pub use pty::{PtyDriver, create_pty_pair};

fn current_tty_ops() -> KResult<Arc<dyn DeviceFileOps>> {
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
    match term.downcast::<PtyDriver>() {
        Ok(term) => Ok(term),
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
    ldisc: Mutex<LineDiscipline<R, W>>,
    writer: W,
    is_ptm: bool,
}

impl<R: TtyRead, W: TtyWrite + Clone> Tty<R, W> {
    fn new(terminal: Arc<Terminal>, config: TtyConfig<R, W>) -> Arc<Self> {
        let writer = config.writer.clone();
        let is_ptm = matches!(&config.process_mode, ProcessMode::None(_));
        let ldisc = Mutex::new(LineDiscipline::new(terminal.clone(), config));
        Arc::new_cyclic(|this| Self {
            this: this.clone(),
            terminal,
            ldisc,
            writer,
            is_ptm,
        })
    }
}

impl<R: TtyRead, W: TtyWrite> Tty<R, W> {
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
        if self.is_ptm {
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
    pub fn set_pty_number(&self, n: u32) {
        self.terminal.pty_number.store(n, Ordering::Release);
    }
}

impl<R: TtyRead, W: TtyWrite> ControllingTerminal for Tty<R, W> {
    fn into_any(self: Arc<Self>) -> Arc<dyn core::any::Any + Send + Sync> {
        self
    }
}

impl<R: TtyRead, W: TtyWrite> DeviceFileOps for Tty<R, W> {
    fn open(&self, _inode: &VfsInode, file: &mut VfsFileBuilder) -> KResult<()> {
        if self.is_ptm || file.requests_no_controlling_tty() {
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
        block_on(poll_io(self, IoEvents::IN, file.is_nonblocking(), || {
            if self.is_ptm || self.terminal.job_control.current_in_foreground() {
                self.ldisc.lock().read(buf)
            } else {
                // TODO: a background process group reading the controlling
                // terminal should receive SIGTTIN (or `EIO` when SIGTTIN is
                // ignored/blocked or the group is orphaned), per POSIX. For now
                // we silently block until the reader reaches the foreground,
                // which can stall job control.
                Err(KError::WouldBlock)
            }
        }))
    }

    fn write(&self, _file: &VfsFile, buf: &[u8], _offset: u64) -> KResult<usize> {
        self.writer.write(buf);
        Ok(buf.len())
    }

    fn ioctl(&self, _file: &VfsFile, cmd: u32, arg: usize) -> KResult<usize> {
        use linux_raw_sys::ioctl::*;
        match cmd {
            TCGETS => {
                (arg as *mut Termios).write_vm(*self.terminal.termios.lock().as_ref().deref())?;
            }
            TCGETS2 => {
                (arg as *mut Termios2).write_vm(*self.terminal.termios.lock().as_ref())?;
            }
            TCSETS | TCSETSF | TCSETSW => {
                *self.terminal.termios.lock() =
                    Arc::new(Termios2::new((arg as *const Termios).read_vm()?));
                if cmd == TCSETSF {
                    self.ldisc.lock().drain_input();
                }
            }
            TCSETS2 | TCSETSF2 | TCSETSW2 => {
                *self.terminal.termios.lock() = Arc::new((arg as *const Termios2).read_vm()?);
                if cmd == TCSETSF2 {
                    self.ldisc.lock().drain_input();
                }
            }
            // TCSBRK: send break / drain output. This virtual terminal has no
            // physical serial line and no kernel output buffer, so it is an
            // intentional no-op (including the `arg == 0` drain-output case).
            TCSBRK => {}
            TCFLSH => match arg as u32 {
                linux_raw_sys::general::TCIFLUSH => self.ldisc.lock().drain_input(),
                linux_raw_sys::general::TCIOFLUSH => {
                    // TCIOFLUSH drains input and output; with no kernel output
                    // buffer (writes go straight to hardware) only input is
                    // flushed here.
                    self.ldisc.lock().drain_input();
                }
                // No kernel output buffer to flush (writes go to hardware).
                linux_raw_sys::general::TCOFLUSH => {}
                _ => return Err(KError::InvalidInput),
            },
            TIOCGPGRP => {
                let foreground = if self.is_ptm {
                    self.terminal
                        .job_control
                        .foreground()
                        .ok_or(KError::NotATty)?
                } else {
                    let caller_session = kprocess::current_user_process().group().session();
                    self.terminal.job_control.foreground_for(&caller_session)?
                };
                (arg as *mut i32).write_vm(foreground.pgid() as i32)?;
            }
            TIOCGSID => {
                let session = if self.is_ptm {
                    self.terminal.job_control.session().ok_or(KError::NotATty)?
                } else {
                    let caller_session = kprocess::current_user_process().group().session();
                    self.terminal.job_control.session_for(&caller_session)?
                };
                (arg as *mut u32).write_vm(session.sid())?;
            }
            TIOCSPGRP => {
                let pgid = (arg as *const i32).read_vm()?;
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
                (arg as *mut WindowSize).write_vm(*self.terminal.window_size.lock())?;
            }
            TIOCSWINSZ => {
                *self.terminal.window_size.lock() = (arg as *const WindowSize).read_vm()?;
            }
            TIOCSPTLCK => {}
            TIOCGPTN => {
                (arg as *mut u32).write_vm(self.pty_number())?;
            }
            TIOCSCTTY => {
                if arg != 0 {
                    // Linux supports arg == 1 for privileged terminal stealing.
                    // X-Kernel does not yet implement reassignment across sessions.
                    return Err(if arg == 1 {
                        KError::OperationNotPermitted
                    } else {
                        KError::InvalidInput
                    });
                }
                let tty = self.this.upgrade().ok_or(KError::NoSuchDevice)?;
                let current_process = kprocess::current_user_thread().process().clone();
                tty.bind_to(&current_process)?;
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

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
    }

    fn poll(&self, _file: &VfsFile) -> IoEvents {
        Pollable::poll(self)
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
        let mut events = IoEvents::OUT | self.terminal.job_control.poll();
        if self.is_ptm || events.contains(IoEvents::IN) {
            events.set(IoEvents::IN, self.ldisc.lock().poll_read());
        }
        events
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if !self.is_ptm {
            self.terminal.job_control.register(context, events)?;
        }
        if events.contains(IoEvents::IN) {
            self.ldisc.lock().register_rx(context)?;
        }
        Ok(())
    }
}

/// /dev/tty device - refers to the calling process's controlling terminal
pub struct CurrentTty;
impl DeviceFileOps for CurrentTty {
    fn open(&self, _inode: &VfsInode, file: &mut VfsFileBuilder) -> KResult<()> {
        let tty = current_tty_ops()?;
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

    fn ioctl(&self, file: &VfsFile, cmd: u32, arg: usize) -> KResult<usize> {
        let tty = tty_file_private(file)?;
        tty.tty.ioctl(file, cmd, arg)
    }

    fn poll(&self, file: &VfsFile) -> IoEvents {
        match tty_file_private(file) {
            Ok(tty) => tty.tty.poll(file),
            Err(_) => IoEvents::empty(),
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

#[cfg(unittest)]
mod tests {
    use kerrno::KError;
    use unittest::def_test;

    use super::pty::create_pty_pair;

    #[def_test(user, serial)]
    fn pty_master_cannot_become_controlling_terminal() {
        let (master, _) = create_pty_pair();
        let process = kprocess::current_user_process();

        assert_eq!(master.bind_to(&process), Err(KError::NotATty));
    }
}
