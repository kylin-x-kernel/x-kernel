// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `sync_file`: a fence exported as a file descriptor (`sync_file.c`).
//!
//! A sync_file owns exactly one kernel fence — which may itself be a
//! [`FenceArray`](super::fence_array::FenceArray) or a chain node — and exposes
//! it as a file:
//!
//! * `poll()`: readable once the fence reached a terminal state. Like Linux,
//!   the first `poll()` subscribes a callback that wakes the wait queue, and a
//!   fence that is already signaled wakes the queue at once;
//! * `read()`: `sync_file.c` has no read operation, so this driver's file has
//!   none either; `SYNC_IOC_FILE_INFO` is the way to describe the fence;
//! * `SYNC_IOC_MERGE`, `SYNC_IOC_FILE_INFO` and `SYNC_IOC_SET_DEADLINE` live
//!   in [`super::fence_access`'s sibling](crate::model::sync_ioctl).
//!
//! The file is an anonymous-inode file, so it has no path and is created in one
//! step by [`new_sync_file`]. Its lifetime is the descriptor's lifetime: the
//! fence object stays alive exactly while a sync_file, a reservation or a
//! waiting structure still references it.

use alloc::sync::Arc;

use anon_inodefs::AnonInodeFs;
use kcred::Cred;
use kpoll::{IoEvents, PollContext, PollRegisterError};
use ktime_types::MonotonicInstant;
use kvfs::{FMode, FileOperations, OpenFlags, VfsError, VfsFile, VfsResult};

use super::{
    dma_fence::{DmaFence, FenceCallback},
    wakeable::Wakeable,
};
use crate::abi::{DrmSyncFileInfo, DrmSyncMergeData, DrmSyncSetDeadline, ioc, iow};

/// `SYNC_IOC_MAGIC`.
pub(crate) const SYNC_IOC_MAGIC: u8 = b'>';

/// `SYNC_IOC_MERGE`: merge another sync_file into a new one.
pub(crate) const SYNC_IOC_MERGE: u32 = ioc(
    crate::abi::IOC_READ | crate::abi::IOC_WRITE,
    SYNC_IOC_MAGIC,
    3,
    core::mem::size_of::<DrmSyncMergeData>() as u16,
);

/// `SYNC_IOC_FILE_INFO`: report the fences of one sync_file.
pub(crate) const SYNC_IOC_FILE_INFO: u32 = ioc(
    crate::abi::IOC_READ | crate::abi::IOC_WRITE,
    SYNC_IOC_MAGIC,
    4,
    core::mem::size_of::<DrmSyncFileInfo>() as u16,
);

/// `SYNC_IOC_SET_DEADLINE`: publish a deadline on the file's fence.
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    unitless_time_integer,
    reason = "CG-011-time-unit-names: Linux UAPI name `SYNC_IOC_SET_DEADLINE` from \
              `include/uapi/linux/sync_file.h`"
)]
pub(crate) const SYNC_IOC_SET_DEADLINE: u32 = iow::<DrmSyncSetDeadline>(SYNC_IOC_MAGIC, 5);

/// The fence a sync_file carries, with the bookkeeping the ABI reports.
pub(crate) struct SyncFile {
    /// The exported fence.
    pub(crate) fence: Arc<DmaFence>,
    /// `sync_file->user_name`: the caller's name, or empty when the file was
    /// created around a fence (the name is then synthesized on demand).
    pub(crate) user_name: alloc::string::String,
    /// Instance instant used to resolve the fence's relative timestamp.
    pub(crate) instantiated: MonotonicInstant,
    /// The poll readiness source.
    wake: Arc<Wakeable>,
    /// Whether the readiness callback was subscribed (`POLL_ENABLED`).
    pub(crate) poll_enabled: core::sync::atomic::AtomicBool,
}

impl SyncFile {
    /// Wraps `fence` in a sync_file object.
    ///
    /// The poll callback is not subscribed here: Linux subscribes it on the
    /// first `poll()`, which is what makes a client that never polls pay
    /// nothing. A fence that completed before anyone polls is observed by the
    /// poll itself (`sync_file_poll` checks the fence after `poll_wait`).
    pub(crate) fn new(fence: Arc<DmaFence>, user_name: Option<alloc::string::String>) -> Arc<Self> {
        Arc::new(Self {
            fence,
            user_name: user_name.unwrap_or_default(),
            instantiated: khal::time::monotonic_time(),
            wake: Arc::new(Wakeable::new()),
            poll_enabled: core::sync::atomic::AtomicBool::new(false),
        })
    }

    /// The fence carried by this file.
    pub(crate) fn fence(&self) -> &Arc<DmaFence> {
        &self.fence
    }

    /// Readiness as `poll()` reports it.
    pub(crate) fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        events.set(IoEvents::IN, self.fence.is_signaled());
        events
    }

    /// Registers `context` for read readiness.
    ///
    /// Subscribes the file's wake callback the first time, as
    /// `sync_file_poll` does: without it a fence that completes from interrupt
    /// context would never wake the waiter. The callback only wakes the poll
    /// set, so it is valid in interrupt context.
    pub(crate) fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        self.wake.register(context, events)?;
        if self.fence.is_signaled() {
            // `dma_fence_add_callback` failing wakes the queue at once.
            self.wake.notify();
            return Ok(());
        }
        self.ensure_callback();
        Ok(())
    }

    /// Subscribes the readiness callback once (`test_and_set_bit(POLL_ENABLED)`).
    fn ensure_callback(&self) {
        use core::sync::atomic::Ordering;
        if self.poll_enabled.swap(true, Ordering::AcqRel) {
            return;
        }
        self.fence
            .add_callback(FenceCallback::wakeable(&Arc::downgrade(&self.wake)));
    }
}

/// The short name a sync_file reports in `sync_file_info`.
///
/// This is `sync_file_get_name`: the user-supplied name when there is one,
/// otherwise `"<driver>-<timeline>-<context>-<seqno>"`.
pub(crate) fn reported_name(file: &SyncFile) -> alloc::string::String {
    if !file.user_name.is_empty() {
        return file.user_name.clone();
    }
    let fence = &file.fence;
    alloc::format!(
        "{}-{}-{}",
        driver_name_of(fence),
        fence.context().value(),
        fence.seqno()
    )
}

/// The driver name a fence reports to userspace.
///
/// A chain node and a fence array are infrastructure, so they report the name
/// of the work they carry, exactly as their ops do
/// (`dma_fence_chain_get_driver_name` returns `dma_fence_chain`).
fn driver_name_of(fence: &DmaFence) -> &'static str {
    if let Some(contained) = fence.contained() {
        return driver_name_of(&contained);
    }
    if fence.array().is_some() {
        return "dma_fence_array";
    }
    fence.driver_name().unwrap_or("unknown")
}

/// `sync_file_get_name`, truncated to the wire field.
pub(crate) fn reported_name_bytes(file: &SyncFile) -> [u8; SYNC_FILE_NAME_LEN] {
    let name = reported_name(file);
    let bytes = name.as_bytes();
    let mut out = [0; SYNC_FILE_NAME_LEN];
    let len = bytes.len().min(SYNC_FILE_NAME_LEN - 1);
    out[..len].copy_from_slice(&bytes[..len]);
    out
}

/// `struct sync_fence_info`, one record per fence of the file.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::AnyBitPattern, bytemuck::NoUninit)]
pub(crate) struct SyncFenceInfo {
    /// Timeline name of the fence.
    pub(crate) obj_name: [u8; SYNC_FENCE_NAME_LEN],
    /// Driver name of the fence.
    pub(crate) driver_name: [u8; SYNC_FENCE_NAME_LEN],
    /// `dma_fence_get_status`: 0 active, 1 signaled, negative error.
    pub(crate) status: i32,
    /// Fence info flags; no exporter of this card defines one.
    pub(crate) flags: u32,
    /// Completion instant in nanoseconds of CLOCK_MONOTONIC, or zero.
    pub(crate) timestamp_ns: u64,
}

/// Length of the name fields in the ABI structures.
pub(crate) const SYNC_FILE_NAME_LEN: usize = 32;
/// Length of the name fields in the ABI fence records.
pub(crate) const SYNC_FENCE_NAME_LEN: usize = 32;

const _: [(); 80] = [(); core::mem::size_of::<SyncFenceInfo>()];

/// `dma_fence_get_status`: the status one fence reports.
///
/// This is what `sync_fence_info.status` and `sync_file_info.status` both
/// carry: `0` while pending, `1` for success, and the fence's negative errno
/// for a failure. A successful completion is positive, not zero, so a status
/// of `0` alone means "not done yet".
pub(crate) fn fence_status(fence: &DmaFence) -> i32 {
    if !fence.is_signaled() {
        return 0;
    }
    match fence.error() {
        0 => 1,
        error => -error,
    }
}

/// The status a whole sync_file reports (`sync_file_info.status`).
///
/// A file whose fence is a fence array (a merge) reports the first member that
/// is not successful while it is still waiting, and `1` whenever every member
/// is, which is exactly `sync_file_ioctl_fence_info` folding each member's
/// status into `info.status`.
pub(crate) fn sync_file_status(file: &SyncFile) -> i32 {
    let mut status = 1;
    for member in super::fence_access::flatten(&file.fence) {
        let member = fence_status(&member);
        status = if status <= 0 { status } else { member };
    }
    status
}

/// The `sync_fence_info` records of one sync_file.
///
/// A fence array expands into its members, as `SYNC_IOC_FILE_INFO` walks it;
/// everything else is a single record.
pub(crate) fn sync_fence_info(file: &SyncFile) -> alloc::vec::Vec<SyncFenceInfo> {
    super::fence_access::flatten(&file.fence)
        .into_iter()
        .map(|member| SyncFenceInfo {
            obj_name: {
                let mut name = [0; SYNC_FENCE_NAME_LEN];
                let bytes = timeline_name_of(&member).as_bytes();
                let len = bytes.len().min(SYNC_FENCE_NAME_LEN - 1);
                name[..len].copy_from_slice(&bytes[..len]);
                name
            },
            driver_name: {
                let mut name = [0; SYNC_FENCE_NAME_LEN];
                let bytes = driver_name_of(&member).as_bytes();
                let len = bytes.len().min(SYNC_FENCE_NAME_LEN - 1);
                name[..len].copy_from_slice(&bytes[..len]);
                name
            },
            status: fence_status(&member),
            flags: 0,
            timestamp_ns: if member.is_signaled() {
                member
                    .timestamp()
                    .map_or(0, |stamp| stamp.nanos(file.instantiated))
            } else {
                0
            },
        })
        .collect()
}

/// The timeline name of a fence (`get_timeline_name`).
fn timeline_name_of(fence: &DmaFence) -> &'static str {
    if let Some(contained) = fence.contained() {
        return timeline_name_of(&contained);
    }
    if fence.array().is_some() {
        return "dma_fence_array";
    }
    "unbound"
}

/// Operations of one sync_file descriptor.
struct SyncFileFops;

impl FileOperations for SyncFileFops {
    fn ioctl(&self, file: &VfsFile, _cred: &kcred::Cred, cmd: u32, arg: usize) -> VfsResult<usize> {
        crate::model::sync_ioctl::sync_file_ioctl(file, cmd, arg)
            .map_err(crate::model::error::DrmError::to_vfs)
    }

    fn poll(&self, file: &VfsFile) -> Option<IoEvents> {
        sync_file_of(file).ok().map(|state| state.poll())
    }

    fn register_poll(
        &self,
        file: &VfsFile,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        match sync_file_of(file) {
            Ok(state) => state.register(context, events),
            Err(_) => Ok(()),
        }
    }
}

fn sync_file_of(file: &VfsFile) -> VfsResult<Arc<SyncFile>> {
    file.private_data_get::<SyncFile>()
        .ok_or(VfsError::InvalidInput)
}

/// Creates the descriptor a fence is exported through.
///
/// This is Linux's `sync_file_create` + `get_unused_fd_flags(O_CLOEXEC)` +
/// `fd_install` in one step, but without an fd number: the caller decides
/// whether the descriptor is installed into the calling task (an ioctl) or a
/// remote client (an out-fence written into shared memory).
pub(crate) fn new_sync_file(
    fence: &Arc<DmaFence>,
    user_name: Option<alloc::string::String>,
    cred: &Arc<Cred>,
) -> VfsResult<Arc<VfsFile>> {
    let state = SyncFile::new(fence.clone(), user_name);
    let file = AnonInodeFs::global().get_file(
        "sync_file",
        Arc::new(SyncFileFops),
        state,
        FMode::READ,
        OpenFlags::empty(),
        cred.clone(),
    )?;
    Ok(file)
}

/// Returns the sync_file state of `file`, if it is one.
pub(crate) fn state_of(file: &VfsFile) -> Option<Arc<SyncFile>> {
    file.private_data_get::<SyncFile>()
}

/// Copies a C string field of a request without reading past its end.
pub(crate) fn c_string(field: &[u8]) -> alloc::string::String {
    let bytes = match field.iter().position(|&byte| byte == 0) {
        Some(end) => &field[..end],
        None => field,
    };
    alloc::string::String::from_utf8_lossy(bytes).into_owned()
}
