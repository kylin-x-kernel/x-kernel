// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Hardware entropy devices and the host capabilities needed to read them.
//!
//! Drivers implement [`HwrngDevice`]; the entropy subsystem consumes it.
//! The host implements [`completion::RngCompletion`] and injects it into drivers.
//! This crate owns neither tasks nor DMA buffers and makes
//! no claim about the quality or trustworthiness of returned bytes.

#![no_std]

extern crate alloc;

pub mod completion;

pub use driver_base::{Device, DeviceKind, DriverError, DriverResult};

/// A raw entropy source, implemented by a concrete device driver.
///
/// Report [`DeviceKind::Hwrng`]. Concurrent reads must not consume the same
/// bytes twice. Empty reads return zero without I/O or waiting; a nonempty success
/// is in `1..=buf.len()` and may be short. Errors
/// deliver no bytes. Returning from a read must not leave the caller's buffer
/// accessible to DMA. Keep in-flight storage driver-owned.
pub trait HwrngDevice: Device {
    /// Read in sleepable task context, sleeping until the device reports completion.
    ///
    /// No fixed completion timeout. No device, IRQ or preemption-disabling
    /// lock may be held across the wait. A short success need not fill `buf`.
    ///
    /// # Errors
    /// Returns [`DriverError::InvalidInput`] if sleeping is forbidden.
    /// Other errors may come from wait preparation or the device.
    /// A read error alone does not establish device failure; consult [`Self::is_failed`].
    /// An error does not cancel a request submitted by an earlier readiness
    /// check. Task interruption does not end the read.
    fn read(&self, buf: &mut [u8]) -> DriverResult<usize>;

    /// Whether this instance has entered a terminal device failure state.
    ///
    /// Non-sleeping. Once true, remains true for the lifetime of the instance.
    /// False means no terminal failure has been established, not that a read
    /// will succeed or hardware will make progress. Wait preparation failures
    /// and invalid caller contexts must not set this state. Consumers may retry
    /// failed reads in valid task context while this remains false.
    /// This is not permission to release outstanding DMA or reset the device.
    fn is_failed(&self) -> bool;
}

#[cfg(unittest)]
#[path = "../ut-tests/hwrng_unittest.rs"]
mod hwrng_unittest_tests;
