// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux `signalfd` object support.
//!
//! Blocking reads interrupted before consuming a record return `ERESTARTSYS`.
//! Signal delivery decides whether to retry after the handler (`SA_RESTART`)
//! or expose `EINTR`. Reads try to consume a record before waiting; nonblocking
//! reads never wait and return `EAGAIN` when no matching record is pending.

use alloc::sync::Arc;
use core::mem;

use anon_inodefs::AnonInodeFs;
use bitflags::bitflags;
use kcred::Cred;
use kerrno::{KError, KResult, LinuxError};
use kpoll::{IoEvents, PollContext, PollRegisterError, Pollable};
use kprocess::AsThread;
use ksignal::{SignalInfo, SignalSet, api::ThreadSignalManager};
use ksync::RwLock;
use ktask::{
    current,
    future::{block_on, poll_io},
};
use kvfs::{FMode, FileOperations, FilePollSource, OpenFlags, VfsFile, VfsInode};
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
                let integer = unsafe { value.sival_int };
                // SAFETY: sigval_t's pointer member overlays the same initialized
                // C-union storage; this copies its bits without dereferencing it.
                let pointer = unsafe { value.sival_ptr };
                (integer, pointer as u64)
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
}

impl Signalfd {
    /// Creates a mask-filtering object without binding it to the constructing thread.
    pub fn new(mask: SignalSet) -> Arc<Self> {
        Arc::new(Self {
            mask: RwLock::new(mask),
        })
    }

    /// Creates a signalfd file and captures `cred` as its open credential.
    ///
    /// # Errors
    ///
    /// Returns `KError::InvalidInput` for unknown open-flag bits and forwards
    /// errors from anonymous-inode file creation.
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
    ///
    /// # Errors
    ///
    /// Returns `KError::BadFileDescriptor` when the file has no matching private data.
    pub fn from_file(file: &VfsFile) -> KResult<Arc<Self>> {
        file.private_data_get::<Self>()
            .ok_or(KError::BadFileDescriptor)
    }

    /// Replaces the file mask and notifies the updating thread's shared signal source.
    ///
    /// The syscall adapter is responsible for removing uncatchable signals.
    /// File aliases share the mask, but unrelated signal sources are not notified.
    /// Returns `NoSuchProcess` if the updater has released its signal configuration.
    pub fn update_mask(&self, mask: SignalSet, signal: &ThreadSignalManager) -> KResult<()> {
        *self.mask.write() = mask;
        signal.notify_pending()
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
    async fn read(&self, buf: &mut [u8], is_nonblocking: bool) -> KResult<usize> {
        let mut read_record_fn = || {
            let sig_info = self.dequeue_signal().ok_or(KError::WouldBlock)?;
            let sfd_info = SignalfdSiginfo::from_signal_info(&sig_info);
            buf[..SIGNALFD_SIGINFO_SIZE].copy_from_slice(sfd_info.as_bytes());
            Ok(SIGNALFD_SIGINFO_SIZE)
        };

        // Available records and nonblocking EAGAIN take precedence over a task
        // interrupt. Only a read that actually needs to wait is interruptible.
        match read_record_fn() {
            Err(KError::WouldBlock) if !is_nonblocking => {}
            result => return result,
        }
        match poll_io(self, IoEvents::IN, false, &mut read_record_fn).await {
            // poll_io checks interruption before readiness. Recheck records so
            // a simultaneous arrival still wins, as on the initial attempt.
            Err(KError::Interrupted) => read_record_fn().map_err(|error| match error {
                // No record was consumed. Signal delivery decides whether the
                // handler permits retry (SA_RESTART) or the caller gets EINTR.
                KError::WouldBlock => KError::from(LinuxError::ERESTARTSYS),
                error => error,
            }),
            result => result,
        }
    }

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
            self.signal.register_pending(context)?;
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

        block_on(Self::access(file)?.read(buf, file.is_nonblocking()))
    }

    fn release(&self, _inode: &VfsInode, _file: &VfsFile) -> KResult<()> {
        Ok(())
    }

    fn poll(&self, file: &VfsFile) -> Option<IoEvents> {
        Some(Self::access(file).map_or(IoEvents::ERR, |access| access.poll()))
    }

    fn register_poll(
        &self,
        file: &VfsFile,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        Self::access(file)
            .map_err(|_| PollRegisterError::InvalidState)?
            .register(context, events)
    }

    fn capture_poll_source(
        &self,
        _file: &VfsFile,
    ) -> Result<Option<FilePollSource>, PollRegisterError> {
        let signal = Self::current_signal().map_err(|_| PollRegisterError::InvalidState)?;
        let source = signal
            .pending_source()
            .map_err(|_| PollRegisterError::InvalidState)?;
        Ok(Some(FilePollSource::new(source, IoEvents::IN)))
    }
}

unittest::path_mod_test!(tests, "../ut-tests/signalfd.rs");
