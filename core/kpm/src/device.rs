// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Ordered, caller-owned device participation in a system PM transaction.

use kerrno::{KError, KResult};

/// A device participant whose ordinary I/O is excluded during system suspend.
///
/// Callbacks run in sleepable task context with IRQs enabled and MONOTONIC
/// advancing. Devices suspend in supplied order and resume in reverse order.
/// Supply consumers before their providers; PM-critical RTC, timer and recovery
/// resources must remain available independently of this list.
///
/// # Safety
/// On successful suspend, drain or suppress IRQs, DMA, timers, clock observers
/// and other accesses that could touch unavailable resources, including while
/// events and IRQs resume before this device's resume callback. Retain those
/// exclusions until resume has restored safe access. On suspend error undo this
/// callback's partial changes: the coordinator resumes only successful entries.
/// Resume must restore safe access on both aborted and actual platform entry,
/// without failure. Both callbacks must return normally without unwinding and
/// must not reenter PM or wait for frozen tasks or work on PM-paused queues.
/// Dependencies must agree with
/// the supplied ordering. The platform remains responsible for unlisted users
/// and for restoring shared infrastructure before ordinary IRQs are enabled.
pub unsafe trait DeviceSuspendOps {
    /// Quiesces the device after task freezing, before RTC/time suspension.
    ///
    /// # Errors
    /// Returns an error after undoing this device's own partial preparation.
    fn suspend(&mut self) -> KResult<()>;

    /// Restores the device after time/events recover, before platform finish
    /// and task thaw. Must also handle an abort that never entered sleep.
    fn resume(&mut self);
}

/// Device suspension failure, preserving the caller's supplied list position.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceSuspendError {
    /// Zero-based index of the device whose suspend callback failed.
    pub index: usize,
    /// Error returned after the failing device undid its own partial changes.
    pub error: KError,
}

pub(crate) struct DeviceSet<'a, 'device> {
    devices: &'a mut [&'device mut dyn DeviceSuspendOps],
    suspended: usize,
}

impl<'a, 'device> DeviceSet<'a, 'device> {
    pub(crate) fn new(devices: &'a mut [&'device mut dyn DeviceSuspendOps]) -> Self {
        Self {
            devices,
            suspended: 0,
        }
    }

    pub(crate) fn suspend(&mut self) -> Result<(), DeviceSuspendError> {
        for (index, device) in self.devices.iter_mut().enumerate() {
            device
                .suspend()
                .map_err(|error| DeviceSuspendError { index, error })?;
            self.suspended += 1;
        }
        Ok(())
    }

    pub(crate) fn resume(&mut self) {
        while self.suspended != 0 {
            self.suspended -= 1;
            self.devices[self.suspended].resume();
        }
    }
}
