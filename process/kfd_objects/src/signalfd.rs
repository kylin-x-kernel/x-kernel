// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux `signalfd` object support.

use alloc::sync::Arc;
use core::mem;

use anon_inodefs::AnonInodeFs;
use bitflags::bitflags;
use kcred::Cred;
use kerrno::{KError, KResult};
use kpoll::{IoEvents, PollContext, PollRegisterError, PollSet, Pollable};
use kprocess::AsThread;
use ksignal::{SignalInfo, SignalSet, api::ThreadSignalManager};
use ksync::RwLock;
use ktask::{
    current,
    future::{block_on, poll_io},
};
use kvfs::{FMode, FileOperations, OpenFlags, VfsFile, VfsInode};
use linux_raw_sys::general::{O_CLOEXEC, O_NONBLOCK};
use zerocopy::{Immutable, IntoBytes};

const SIGNALFD_SIGINFO_SIZE: usize = 128;
const SFD_CLOEXEC: u32 = O_CLOEXEC;
const SFD_NONBLOCK: u32 = O_NONBLOCK;

bitflags! {
    /// Flags for the `signalfd4` syscall.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct SignalfdFlags: u32 {
        /// Creates a file descriptor that is closed on `exec`.
        const CLOEXEC = SFD_CLOEXEC;
        /// Creates a non-blocking signalfd.
        const NONBLOCK = SFD_NONBLOCK;
    }
}

/// The Linux `signalfd_siginfo` payload layout.
#[repr(C)]
#[derive(Immutable, IntoBytes)]
struct SignalfdSiginfo {
    ssi_signo: u32,
    ssi_errno: i32,
    ssi_code: i32,
    ssi_pid: u32,
    ssi_uid: u32,
    ssi_fd: i32,
    ssi_tid: u32,
    ssi_band: u32,
    ssi_overrun: u32,
    ssi_trapno: u32,
    ssi_status: i32,
    ssi_int: i32,
    ssi_ptr: u64,
    ssi_utime: u64,
    ssi_stime: u64,
    ssi_addr: u64,
    ssi_addr_lsb: u16,
    _pad: [u8; 46],
}

const _: [(); SIGNALFD_SIGINFO_SIZE] = [(); mem::size_of::<SignalfdSiginfo>()];

impl SignalfdSiginfo {
    fn from_signal_info(sig_info: &SignalInfo) -> Self {
        let errno = sig_info.errno();
        let child_exit = sig_info.child_exit();
        let (ssi_tid, ssi_overrun) = match (sig_info.timer_id(), sig_info.timer_overrun()) {
            (Some(timer_id), Some(overrun)) => (timer_id as u32, overrun.max(0) as u32),
            _ => (0, 0),
        };
        let (ssi_int, ssi_ptr) = sig_info
            .sigval()
            .map(|value| {
                // SAFETY: sigval_t is a public C union whose sival_int and
                // sival_ptr members occupy the same storage; reading either is
                // valid regardless of which variant was written.
                unsafe { (value.sival_int, value.sival_ptr as u64) }
            })
            .unwrap_or((0, 0));

        Self {
            ssi_signo: sig_info.signo() as u32,
            ssi_errno: errno,
            ssi_code: sig_info.code(),
            ssi_pid: child_exit.map(|child| child.pid()).unwrap_or(0),
            ssi_uid: child_exit.map(|child| child.uid()).unwrap_or(0),
            ssi_fd: -1,
            ssi_tid,
            ssi_band: 0,
            ssi_overrun,
            ssi_trapno: 0,
            ssi_status: child_exit.map(|child| child.status()).unwrap_or(0),
            ssi_int,
            ssi_ptr,
            ssi_utime: child_exit
                .map(|child| posix_types::PosixClockTicks::from_time_span(child.utime()).as_raw())
                .unwrap_or(0),
            ssi_stime: child_exit
                .map(|child| posix_types::PosixClockTicks::from_time_span(child.stime()).as_raw())
                .unwrap_or(0),
            ssi_addr: 0,
            ssi_addr_lsb: 0,
            _pad: [0; 46],
        }
    }
}

/// Signal file private data.
pub struct Signalfd {
    mask: RwLock<SignalSet>,
    poll_rx: PollSet,
}

impl Signalfd {
    pub fn new(mask: SignalSet) -> Arc<Self> {
        Arc::new(Self {
            mask: RwLock::new(mask),
            poll_rx: PollSet::new(),
        })
    }

    /// Creates a signalfd file and captures `cred` as its open credential.
    pub fn new_file(mask: SignalSet, open_flags: u32, cred: Arc<Cred>) -> KResult<Arc<VfsFile>> {
        let open_flags = OpenFlags::from_bits(open_flags).ok_or(KError::InvalidInput)?;
        AnonInodeFs::global().get_file(
            "[signalfd]",
            Arc::new(SignalfdFops),
            Self::new(mask),
            FMode::READ | FMode::STREAM,
            open_flags,
            cred,
        )
    }

    /// Returns the signalfd object attached to a signalfd file.
    pub fn from_file(file: &VfsFile) -> KResult<Arc<Self>> {
        file.private_data_get::<Self>()
            .ok_or(KError::BadFileDescriptor)
    }

    pub fn update_mask(&self, mask: SignalSet) {
        *self.mask.write() = mask;
        self.poll_rx.wake();
    }

    fn mask(&self) -> SignalSet {
        *self.mask.read()
    }
}

struct SignalfdAccess {
    signalfd: Arc<Signalfd>,
    signal: Arc<ThreadSignalManager>,
}

impl SignalfdAccess {
    fn has_pending_signals(&self) -> bool {
        let mask = self.signalfd.mask();
        let pending = self.signal.pending();
        !(pending & mask).is_empty()
    }

    fn dequeue_signal(&self) -> Option<SignalInfo> {
        let mask = self.signalfd.mask();
        self.signal.dequeue_signal(&mask)
    }
}

impl Pollable for SignalfdAccess {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        events.set(IoEvents::IN, self.has_pending_signals());
        events
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if events.contains(IoEvents::IN) {
            context.register(&self.signalfd.poll_rx)?;
        }
        Ok(())
    }
}

struct SignalfdFops;

impl SignalfdFops {
    fn signalfd(file: &VfsFile) -> KResult<Arc<Signalfd>> {
        Signalfd::from_file(file)
    }

    fn current_signal() -> KResult<Arc<ThreadSignalManager>> {
        let task = current();
        let thread = task.try_as_thread().ok_or(KError::OperationNotPermitted)?;
        Ok(thread.signal_manager().clone())
    }

    fn access(file: &VfsFile) -> KResult<SignalfdAccess> {
        Ok(SignalfdAccess {
            signalfd: Self::signalfd(file)?,
            signal: Self::current_signal()?,
        })
    }
}

impl FileOperations for SignalfdFops {
    fn supports_read(&self) -> bool {
        true
    }

    fn read(&self, file: &VfsFile, buf: &mut [u8], _offset: u64) -> KResult<usize> {
        if buf.len() < SIGNALFD_SIGINFO_SIZE {
            return Err(KError::InvalidInput);
        }

        let access = Self::access(file)?;
        block_on(poll_io(
            &access,
            IoEvents::IN,
            file.is_nonblocking(),
            || {
                if let Some(sig_info) = access.dequeue_signal() {
                    let sfd_info = SignalfdSiginfo::from_signal_info(&sig_info);
                    buf[..SIGNALFD_SIGINFO_SIZE].copy_from_slice(sfd_info.as_bytes());

                    if access.has_pending_signals() {
                        access.signalfd.poll_rx.wake();
                    }

                    Ok(SIGNALFD_SIGINFO_SIZE)
                } else {
                    Err(KError::WouldBlock)
                }
            },
        ))
    }

    fn release(&self, _inode: &VfsInode, _file: &VfsFile) -> KResult<()> {
        Ok(())
    }

    fn poll(&self, file: &VfsFile) -> IoEvents {
        Self::access(file).map_or(IoEvents::ERR, |access| access.poll())
    }

    fn register_poll(
        &self,
        file: &VfsFile,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if let Ok(access) = Self::access(file) {
            access.register(context, events)?;
        }
        Ok(())
    }
}

#[cfg(unittest)]
mod tests {
    use ksignal::{ChildExitInfo, ChildExitSignalInfo, Signo};
    use unittest::{assert, assert_eq, def_test};

    use super::*;

    #[def_test]
    fn test_signalfd_poll_empty() {
        let file = Signalfd::new_file(SignalSet::default(), 0, kcred::initial_cred())
            .expect("signalfd file opens");
        assert!(file.poll().contains(IoEvents::ERR));
    }

    #[def_test]
    fn test_signalfd_siginfo_size() {
        assert_eq!(SIGNALFD_SIGINFO_SIZE, 128);
        assert_eq!(core::mem::size_of::<SignalfdSiginfo>(), 128);
    }

    #[def_test]
    fn test_signalfd_nonblocking() {
        let file = Signalfd::new_file(SignalSet::default(), 0, kcred::initial_cred())
            .expect("signalfd file opens");
        assert!(!file.is_nonblocking());
        file.set_nonblocking(true);
        assert!(file.is_nonblocking());
        file.set_nonblocking(false);
        assert!(!file.is_nonblocking());
    }

    #[def_test]
    fn test_signalfd_write_returns_error() {
        let file = Signalfd::new_file(SignalSet::default(), 0, kcred::initial_cred())
            .expect("signalfd file opens");
        let mut pos = 0;
        assert!(file.write_from(b"test", &mut pos).is_err());
    }

    #[def_test]
    fn test_signalfd_siginfo_from_signal_info() {
        let sig = SignalInfo::new_kernel(Signo::SIGUSR1);
        let info = SignalfdSiginfo::from_signal_info(&sig);
        assert_eq!(info.ssi_signo, Signo::SIGUSR1 as u32);
        assert_eq!(info.ssi_fd, -1);
        assert_eq!(info.ssi_pid, 0);
    }

    #[def_test]
    fn test_signalfd_siginfo_from_child_exit_signal_info() {
        let child = ChildExitInfo::from_wait_status(
            42,
            1000,
            9 << 8,
            ktime_types::TimeSpan::from_millis(20),
            ktime_types::TimeSpan::from_millis(30),
        );
        let sig = ChildExitSignalInfo::new_sigchld(child);
        let info = SignalfdSiginfo::from_signal_info(sig.as_child_exit_signal().as_signal_info());

        assert_eq!(info.ssi_signo, Signo::SIGCHLD as u32);
        assert_eq!(info.ssi_code, linux_raw_sys::general::CLD_EXITED as i32);
        assert_eq!(info.ssi_pid, 42);
        assert_eq!(info.ssi_uid, 1000);
        assert_eq!(info.ssi_status, 9);
        assert_eq!(
            info.ssi_utime,
            posix_types::PosixClockTicks::from_time_span(ktime_types::TimeSpan::from_millis(20))
                .as_raw()
        );
        assert_eq!(
            info.ssi_stime,
            posix_types::PosixClockTicks::from_time_span(ktime_types::TimeSpan::from_millis(30))
                .as_raw()
        );
    }
}
