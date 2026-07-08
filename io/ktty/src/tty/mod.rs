// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::{Arc, Weak};
use core::{ops::Deref, sync::atomic::Ordering, task::Context};

use kerrno::{KError, KResult, LinuxError};
use kpoll::{IoEvents, Pollable};
use kprocess::Process;
use ksync::Mutex;
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
    /// Bind this TTY to a process group as the controlling terminal
    pub fn bind_to(self: &Arc<Self>, proc: &Process) -> KResult<()> {
        let pg = proc.group();
        if pg.session().sid() != proc.pid() {
            return Err(KError::OperationNotPermitted);
        }
        assert!(pg.session().set_terminal_with(|| {
            self.terminal.job_control.set_session(&pg.session());
            self.clone()
        }));

        self.terminal.job_control.set_foreground(&pg)?;
        Ok(())
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

impl<R: TtyRead, W: TtyWrite> DeviceFileOps for Tty<R, W> {
    fn supports_read(&self) -> bool {
        true
    }

    fn supports_write(&self) -> bool {
        true
    }

    fn read(&self, _file: &VfsFile, buf: &mut [u8], _offset: u64) -> KResult<usize> {
        if self.is_ptm || self.terminal.job_control.current_in_foreground() {
            self.ldisc.lock().read(buf)
        } else {
            Err(KError::WouldBlock)
        }
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
            TIOCGPGRP => {
                let foreground = self
                    .terminal
                    .job_control
                    .foreground()
                    .ok_or(KError::NoSuchProcess)?;
                (arg as *mut u32).write_vm(foreground.pgid())?;
            }
            TIOCSPGRP => {
                let current_process = kprocess::current_user_thread().process().clone();
                self.terminal
                    .job_control
                    .set_foreground(&current_process.group())?;
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
                let tty = self.this.upgrade().ok_or(KError::NoSuchDevice)?;
                let current_process = kprocess::current_user_thread().process().clone();
                tty.bind_to(&current_process)?;
            }
            TIOCNOTTY => {
                let tty = self.this.upgrade().ok_or(KError::NoSuchDevice)?;
                let current_process = kprocess::current_user_thread().process().clone();
                if current_process
                    .group()
                    .session()
                    .unset_terminal(&(tty as _))
                {
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

    fn register_poll(&self, _file: &VfsFile, context: &mut Context<'_>, events: IoEvents) {
        Pollable::register(self, context, events);
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

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        if !self.is_ptm {
            self.terminal.job_control.register(context, events);
        }
        if events.contains(IoEvents::IN) {
            self.ldisc.lock().register_rx_waker(context.waker());
        }
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

    fn register_poll(&self, file: &VfsFile, context: &mut Context<'_>, events: IoEvents) {
        if let Ok(tty) = tty_file_private(file) {
            tty.tty.register_poll(file, context, events);
        }
    }
}
