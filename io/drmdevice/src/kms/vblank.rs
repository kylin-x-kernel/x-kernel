// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `DRM_IOCTL_WAIT_VBLANK` (`drm_vblank.c`): the handler and the wait it parks
//! on.
//!
//! The semantics are Linux's `drm_wait_vblank_ioctl`: a past target returns
//! the current blank immediately; a later target is answered by the clock's
//! tick, so the answer carries the blank's own sequence and timestamp rather
//! than the moment the task ran; `DRM_VBLANK_NONBLOCK` answers the current
//! blank instead of sleeping; a signal interrupts the wait with `EINTR`.
//!
//! [`wait_vblank_flags_valid`] is the request's validation, kept as a pure
//! function of the flag word so the accepted set is one place.
//!
//! Nothing here writes to user memory, so the ioctl never fails with `EFAULT`
//! after its task slept.

use core::{
    future::poll_fn,
    task::{Poll, Waker},
};

use crate::{
    model::vblank::{VblankClockState, VblankRequest},
    prelude::*,
};

/// Whether `request_type` is a request this card can answer.
///
/// Linux validates the same bits: the sequence type must be one of
/// `_ABSOLUTE` (zero), `RELATIVE`, `ABSOLUTE_BLANKED` or `RELATIVE_BLANKED`,
/// the high bits of the sequence are CRTC address bits (`HIGH_CRTC`), and
/// `NONBLOCK` is a scheduling flag. A bit outside those sets is `EINVAL`.
pub(crate) fn wait_vblank_flags_valid(request_type: u32) -> bool {
    let known = DRM_VBLANK_RELATIVE
        | DRM_VBLANK_HIGH_CRTC
        | DRM_VBLANK_NONBLOCK
        | DRM_VBLANK_ABSOLUTE_BLANKED
        | DRM_VBLANK_RELATIVE_BLANKED;
    request_type & !known == 0
}

/// One `WAIT_VBLANK` request against one card.
pub(crate) struct VblankWait<'a> {
    device: &'a DrmDevice,
    request: VblankRequest,
}

/// Whether a request addresses a CRTC this card does not have.
///
/// A request that selects a CRTC (`HIGH_CRTC`, and the same address encoding for
/// the blanked forms) carries its address in the sequence; this card has one
/// CRTC, addressed as zero. `sequence` is also the target for requests without
/// an address, where only its low byte is meaningful, so only the address bits
/// of an addressed request are checked.
pub(crate) fn wait_vblank_crtc_valid(request_type: u32, sequence: u32) -> bool {
    let selects_crtc = request_type
        & (DRM_VBLANK_HIGH_CRTC | DRM_VBLANK_ABSOLUTE_BLANKED | DRM_VBLANK_RELATIVE_BLANKED)
        != 0;
    !selects_crtc || sequence & !u32::from(u8::MAX) == 0
}

impl<'a> VblankWait<'a> {
    /// Prepares a request resolved by [`DrmDevice::vblank_request`].
    ///
    /// The caller has already answered past targets and started the clock;
    /// this object only parks the task until the requested blank arrives.
    pub(crate) const fn new(device: &'a DrmDevice, request: VblankRequest) -> Self {
        Self { device, request }
    }

    /// Parks until the requested blank, returning it.
    ///
    /// # Errors
    ///
    /// `EINVAL` when the calling context cannot sleep, `ENODEV` when the
    /// display disappeared and `EINTR` when a signal interrupted the wait.
    pub(crate) fn run(self) -> DrmResult<(u32, TimeSpan)> {
        // Validate the sleeping context before registering with the clock.
        let _wait = CompletionWait::prepare().map_err(|_| DrmError::InvalidInput)?;
        let clock = self.device.vblank_clock();
        let target = self.request.target(clock.tick().sequence);
        wait_until(self.device, clock, target)
    }

    /// Answers without sleeping, for `DRM_VBLANK_NONBLOCK`.
    ///
    /// The probe reports the current blank; a client that wanted to pace itself
    /// retries later instead of being parked. No source is started and no
    /// waiter is registered, so a probe never changes the timeline.
    pub(crate) fn probe(self) -> (u32, TimeSpan) {
        self.device.vblank_clock().tick_report()
    }
}

/// Parks until the clock reaches `target` or the task is interrupted.
///
/// Each poll registers the block-on waker with the clock before the final
/// `reached` check and withdraws it when the wait returns. A blank that lands
/// while a poll runs is therefore either observed by that check or wakes the
/// pending future.
fn wait_until(
    device: &DrmDevice,
    clock: &Arc<VblankClockState>,
    target: u64,
) -> DrmResult<(u32, TimeSpan)> {
    let mut parked: Option<Waker> = None;
    loop {
        if device.backend().is_err() {
            return Err(DrmError::NoSuchDevice);
        }
        if clock.reached(target) {
            return Ok(clock.tick_report());
        }
        let outcome = block_on_interruptible(poll_fn(|cx| {
            // Register before the final check: a blank that lands while this
            // poll runs is then either observed here or wakes the pending
            // future, so the wait never misses the blank it asked for.
            clock.park(target, cx.waker());
            parked = Some(cx.waker().clone());
            if clock.reached(target) {
                return Poll::Ready(());
            }
            Poll::Pending
        }));
        if let Some(waker) = parked.take() {
            clock.unpark(&waker);
        }
        outcome.map_err(|_| DrmError::Interrupted)?;
    }
}

impl Ioctl for DrmWaitVblank {
    const NAME: &'static str = "WAIT_VBLANK";

    fn handle(
        dev: &DrmDevice,
        file: &Arc<DrmFile>,
        _unit: u32,
        request: &mut Self,
    ) -> DrmResult<usize> {
        wait_vblank(dev, file, request)
    }
}

/// Answers one `DRM_IOCTL_WAIT_VBLANK`.
///
/// Linux validates the flag word, refuses a device whose CRTC cannot enable
/// vblank (`EOPNOTSUPP`), resolves the target against the file's cursor, and
/// only then parks. A past target is answered at once; a wait never advances
/// the sequence, so one client cannot move the timeline another reads.
pub(crate) fn wait_vblank(
    dev: &DrmDevice,
    file: &Arc<DrmFile>,
    request: &mut DrmWaitVblank,
) -> DrmResult<usize> {
    if !wait_vblank_flags_valid(request.rep_type)
        || !wait_vblank_crtc_valid(request.rep_type, request.sequence)
    {
        return Err(DrmError::InvalidInput);
    }
    if !dev.vblank_supported() {
        return Err(DrmError::OpNotSupported);
    }
    let is_relative = request.rep_type & DRM_VBLANK_RELATIVE != 0;
    let is_nonblocking = request.rep_type & DRM_VBLANK_NONBLOCK != 0;
    let resolved = dev.vblank_request(file, is_relative, request.sequence);
    if resolved == VblankRequest::Past {
        let (sequence, timestamp) = dev.vblank_clock().tick_report();
        *request = wait_answer(sequence, timestamp);
        return Ok(0);
    }
    if is_nonblocking {
        let (sequence, timestamp) = VblankWait::new(dev, resolved).probe();
        *request = wait_answer(sequence, timestamp);
        return Ok(0);
    }
    let (sequence, timestamp) = VblankWait::new(dev, resolved).run()?;
    *request = wait_answer(sequence, timestamp);
    Ok(0)
}

/// The reply of one answered wait: the blank and its boundary.
fn wait_answer(sequence: u32, timestamp: TimeSpan) -> DrmWaitVblank {
    DrmWaitVblank {
        rep_type: 0,
        sequence,
        tv_sec: timestamp.as_secs() as i64,
        tv_usec: timestamp.subsec_micros() as i64,
    }
}
