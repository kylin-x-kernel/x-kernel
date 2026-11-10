// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `SYNCOBJ_EVENTFD`: a timeline point that signals an eventfd.
//!
//! `drm_syncobj_eventfd_ioctl` ties one eventfd to one timeline point: every
//! time that point becomes available, the eventfd is incremented. A signal is
//! not a one-shot: the eventfd keeps counting points as they arrive, which is
//! what lets a client sleep on one descriptor for a whole timeline.
//!
//! Linux keeps a per-`drm_device` `defer_list` of `drm_syncobj_eventfd`
//! records and signals them from `drm_syncobj_signal_ioctl`. This driver keeps
//! the same relationship on the object it watches: the record arms a chain
//! callback on the object's value, so the eventfd is incremented on the exact
//! fence completion that made the point available, and each later signal arms
//! the relation again on the then-current value.
//!
//! Accounting is per `EVENT` unit — the node number the command arrived
//! through — so the same relation registered through a render node and through
//! the card node counts from one unit, as Linux's `drmDeviceEvent` does.

use alloc::sync::Arc;

use kfd_objects::eventfd::EventFd;
use kspin::SpinNoIrq;
use kvfs::VfsFile;

use super::error::{DrmError, DrmResult};
use crate::{
    abi::DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE,
    sync::dma_fence::{DmaFence, FenceCallback},
};

/// One eventfd tied to one timeline point of one `EVENT` unit.
pub(crate) struct EventFdSignal {
    /// The eventfd object behind the registered descriptor.
    eventfd: Arc<EventFd>,
    /// The node the registration arrived through.
    pub(crate) unit: u32,
    /// The point that triggers a signal.
    pub(crate) point: u64,
    /// The request's flags; only `WAIT_AVAILABLE` is defined.
    flags: u32,
    /// The fence instance the event is armed on.
    watched: SpinNoIrq<Option<Arc<DmaFence>>>,
}

impl EventFdSignal {
    /// Creates the relation; it is armed by [`Self::arm`] on the object.
    pub(crate) fn new(eventfd: Arc<EventFd>, unit: u32, point: u64, flags: u32) -> Arc<Self> {
        Arc::new(Self {
            eventfd,
            unit,
            point,
            flags,
            watched: SpinNoIrq::new(None),
        })
    }

    /// Whether the relation fires as soon as the point is available.
    pub(crate) const fn is_available_now(&self) -> bool {
        self.flags & DRM_SYNCOBJ_EVENTFD_FLAGS != 0
    }

    /// Arms the event on `value`, the object's current value.
    ///
    /// A value that is already available has nothing left to wait for: the
    /// point it made available was signaled when it was installed, and a
    /// later value arms a fresh callback. Every other value is armed once, so
    /// a re-signal at the same point does not accumulate callbacks.
    pub(crate) fn arm(self: &Arc<Self>, value: &Arc<DmaFence>) {
        if value.is_signaled() {
            *self.watched.lock() = None;
            return;
        }
        {
            let watched = self.watched.lock();
            if watched
                .as_ref()
                .is_some_and(|known| Arc::ptr_eq(known, value))
            {
                return;
            }
        }
        *self.watched.lock() = Some(value.clone());
        value.add_callback(FenceCallback::eventfd(&Arc::downgrade(self)));
    }

    /// Increments the eventfd once (`eventfd_signal`).
    pub(crate) fn signal(&self) {
        self.eventfd.signal(1);
    }

    /// Fires as soon as the point is available (`WAIT_AVAILABLE`).
    ///
    /// `syncobj_eventfd_entry_func` signals the eventfd without waiting for
    /// the fence when the registration asked for availability; the plain form
    /// waits for the fence that makes the point available.
    pub(crate) fn signal_on_install(&self) {
        if self.is_available_now() {
            self.signal();
        }
    }
}

impl core::fmt::Debug for EventFdSignal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("EventFdSignal")
            .field("unit", &self.unit)
            .field("point", &self.point)
            .finish_non_exhaustive()
    }
}

/// The eventfd object behind a descriptor of the calling task.
///
/// The descriptor must be an eventfd; any other file is `EINVAL`, as Linux's
/// `drm_syncobj_eventfd_ioctl` reports a failed `eventfd_ctx_fdget`.
pub(crate) fn eventfd_from_fd(fd: i32) -> DrmResult<Arc<EventFd>> {
    if fd < 0 {
        return Err(DrmError::InvalidInput);
    }
    let file: Arc<VfsFile> = kprocess::current_files()
        .get_file(fd)
        .map_err(|_| DrmError::InvalidInput)?;
    EventFd::from_file(&file).map_err(|_| DrmError::InvalidInput)
}

/// Checks the request of `SYNCOBJ_EVENTFD`.
///
/// `drm_syncobj_eventfd_ioctl` accepts only `WAIT_AVAILABLE` as a flag: a
/// plain registration signals the eventfd when the point's fence completes,
/// and `WAIT_AVAILABLE` signals it as soon as the point is available, without
/// waiting for the fence.
///
/// # Errors
///
/// `EINVAL` for undefined flags and for a non-zero pad.
pub(crate) fn validate_request(flags: u32, pad: u32) -> DrmResult<()> {
    if flags & !DRM_SYNCOBJ_EVENTFD_FLAGS != 0 {
        return Err(DrmError::InvalidInput);
    }
    if pad != 0 {
        return Err(DrmError::InvalidInput);
    }
    Ok(())
}

/// The one flag `SYNCOBJ_EVENTFD` defines (`DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE`).
const DRM_SYNCOBJ_EVENTFD_FLAGS: u32 = DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE;
