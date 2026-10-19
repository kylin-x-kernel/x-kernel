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
use osvm::{VirtMutPtr, VirtPtr};

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

pub use ntty::{N_TTY, NTtyDriver, try_handoff_console};
pub use pty::PtyDriver;
#[cfg(unittest)]
use pty::create_pty_pair;
pub use pty_pair::{
    PtyPair, open_ptmx, pty_allocated_count, pty_limit, pty_slave_inode, pty_slave_numbers,
    register_pty_drivers,
};
pub use vt::{VT_CONSOLE, VtConsole, register_fixed_vt_devices};

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

impl<R: TtyRead, W: TtyWrite + Clone> Tty<R, W> {
    fn new(terminal: Arc<Terminal>, kind: TtyKind, config: TtyConfig<R, W>) -> Arc<Self> {
        Self::new_with_pair(terminal, kind, config, None)
    }

    fn new_with_pair(
        terminal: Arc<Terminal>,
        kind: TtyKind,
        config: TtyConfig<R, W>,
        pty_pair: Option<Weak<PtyPair>>,
    ) -> Arc<Self> {
        let writer = config.writer.clone();
        let port = TtyPort::new(terminal.clone(), config);
        let output = port.output.clone();
        let ingress = port.ingress.clone();
        let input = Mutex::new(port);
        Arc::new_cyclic(|this| Self {
            this: this.clone(),
            terminal,
            input,
            writer,
            output,
            ingress,
            kind,
            pty_pair,
        })
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
                    self.input.lock().drain_input();
                }
                self.writer.notify_tx();
                // Shared PTY termios also changes the peer's output requirements.
                self.ingress.notify_space();
            }
            TCSETS2 | TCSETSF2 | TCSETSW2 => {
                *self.terminal.termios.lock() = Arc::new((arg as *const Termios2).read_vm()?);
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
                (arg as *mut i32).write_vm(foreground.pgid() as i32)?;
            }
            TIOCGSID => {
                let session = if self.kind == TtyKind::PtyMaster {
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

unittest::path_mod_test!(tests_pty, "../../ut-tests/tty_pty.rs");

unittest::path_mod_test!(tests, "../../ut-tests/tty.rs");
