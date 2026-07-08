// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux `signalfd` object support.

use alloc::{borrow::Cow, sync::Arc};
use core::{
    mem,
    sync::atomic::{AtomicBool, Ordering},
    task::Context,
};

use bitflags::bitflags;
use kerrno::{KError, KResult};
use kfd::{FileLike, IoDst, IoSrc};
use kpoll::{IoEvents, PollSet, Pollable};
use ksignal::{SignalInfo, SignalSet};
use ksync::RwLock;
use ktask::future::{block_on, poll_io};
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
            ssi_pid: 0,
            ssi_uid: 0,
            ssi_fd: -1,
            ssi_tid,
            ssi_band: 0,
            ssi_overrun,
            ssi_trapno: 0,
            ssi_status: 0,
            ssi_int,
            ssi_ptr,
            ssi_utime: 0,
            ssi_stime: 0,
            ssi_addr: 0,
            ssi_addr_lsb: 0,
            _pad: [0; 46],
        }
    }
}

/// A file-like adapter that exposes pending signals through `read`.
pub struct Signalfd {
    mask: RwLock<SignalSet>,
    non_blocking: AtomicBool,
    poll_rx: PollSet,
}

impl Signalfd {
    pub fn new(mask: SignalSet) -> Arc<Self> {
        Arc::new(Self {
            mask: RwLock::new(mask),
            non_blocking: AtomicBool::new(false),
            poll_rx: PollSet::new(),
        })
    }

    pub fn update_mask(&self, mask: SignalSet) {
        *self.mask.write() = mask;
        self.poll_rx.wake();
    }

    fn mask(&self) -> SignalSet {
        *self.mask.read()
    }

    fn has_pending_signals(&self) -> bool {
        let mask = self.mask();
        let current_thread = kprocess::current_user_thread();
        let signal = current_thread.signal_manager();
        let pending = signal.pending();
        !(pending & mask).is_empty()
    }

    fn dequeue_signal(&self) -> Option<SignalInfo> {
        let mask = self.mask();
        let current_thread = kprocess::current_user_thread();
        let signal = current_thread.signal_manager();
        signal.dequeue_signal(&mask)
    }
}

impl FileLike for Signalfd {
    fn read(&self, dst: &mut IoDst) -> KResult<usize> {
        if dst.remaining_mut() < SIGNALFD_SIGINFO_SIZE {
            return Err(KError::InvalidInput);
        }

        block_on(poll_io(self, IoEvents::IN, self.nonblocking(), || {
            if let Some(sig_info) = self.dequeue_signal() {
                let sfd_info = SignalfdSiginfo::from_signal_info(&sig_info);
                dst.write(sfd_info.as_bytes())?;

                if self.has_pending_signals() {
                    self.poll_rx.wake();
                }

                Ok(SIGNALFD_SIGINFO_SIZE)
            } else {
                Err(KError::WouldBlock)
            }
        }))
    }

    fn write(&self, _src: &mut IoSrc) -> KResult<usize> {
        Err(KError::BadFileDescriptor)
    }

    fn nonblocking(&self) -> bool {
        self.non_blocking.load(Ordering::Acquire)
    }

    fn set_nonblocking(&self, non_blocking: bool) -> KResult {
        self.non_blocking.store(non_blocking, Ordering::Release);
        Ok(())
    }

    fn path(&self) -> Cow<'_, str> {
        "anon_inode:[signalfd]".into()
    }
}

impl Pollable for Signalfd {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        events.set(IoEvents::IN, self.has_pending_signals());
        events
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        if events.contains(IoEvents::IN) {
            self.poll_rx.register(context.waker());
        }
    }
}

#[cfg(unittest)]
mod tests {
    use kio::Cursor;
    use ksignal::Signo;
    use unittest::{assert, assert_eq, def_test};

    use super::*;

    #[def_test]
    fn test_signalfd_path() {
        let signalfd = Signalfd::new(SignalSet::default());
        assert_eq!(signalfd.path(), "anon_inode:[signalfd]");
    }

    #[def_test]
    fn test_signalfd_siginfo_size() {
        assert_eq!(SIGNALFD_SIGINFO_SIZE, 128);
        assert_eq!(core::mem::size_of::<SignalfdSiginfo>(), 128);
    }

    #[def_test]
    fn test_signalfd_nonblocking() {
        let signalfd = Signalfd::new(SignalSet::default());
        assert!(!signalfd.nonblocking());
        signalfd.set_nonblocking(true).unwrap();
        assert!(signalfd.nonblocking());
        signalfd.set_nonblocking(false).unwrap();
        assert!(!signalfd.nonblocking());
    }

    #[def_test]
    fn test_signalfd_write_returns_error() {
        let signalfd = Signalfd::new(SignalSet::default());
        let mut src = Cursor::new(b"test".as_slice());
        assert!(signalfd.write(&mut src).is_err());
    }

    #[def_test]
    fn test_signalfd_siginfo_from_signal_info() {
        let sig = SignalInfo::new_kernel(Signo::SIGUSR1);
        let info = SignalfdSiginfo::from_signal_info(&sig);
        assert_eq!(info.ssi_signo, Signo::SIGUSR1 as u32);
        assert_eq!(info.ssi_fd, -1);
        assert_eq!(info.ssi_pid, 0);
    }
}
