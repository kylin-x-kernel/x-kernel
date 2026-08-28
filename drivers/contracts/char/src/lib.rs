// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Common traits and types for character device drivers.
//!
//! # Scope
//!
//! This crate is intended for character devices that participate in the
//! unified discover → bind → activate driver pipeline — for example `hwrng`,
//! secondary serial / UART expansion cards, and runtime handoff wrappers for
//! boot-time devices.
//!
//! The early-boot console (`drivers/platform/console`) still comes up on its dedicated
//! boot path before the bus manager and driver registry exist. Runtime code
//! may later *reuse* that already-initialized console by exposing a thin
//! `CharDevice` wrapper through the generic pipeline; the boot sequence itself
//! remains separate.

#![no_std]

#[doc(no_inline)]
pub use driver_base::{Device, DeviceKind, DriverError, DriverResult};

/// Operations a character device driver must implement.
///
/// Character devices expose a byte-stream view to user space; the trait stays
/// intentionally minimal so that a wide range of devices (random sources,
/// expansion UARTs, ...) can implement it without taking on policy decisions
/// that belong to higher layers.
pub trait CharDevice: Device {
    /// Read up to `buf.len()` bytes from the device into `buf`.
    ///
    /// Returns the number of bytes actually read. A return value of `Ok(0)`
    /// indicates end-of-stream when the device has a finite source; for
    /// open-ended sources, drivers should return [`DriverError::WouldBlock`]
    /// instead so the caller can decide whether to retry or sleep.
    fn read(&self, buf: &mut [u8]) -> DriverResult<usize>;

    /// Write up to `buf.len()` bytes from `buf` to the device.
    ///
    /// Returns the number of bytes actually accepted. Drivers that cannot
    /// make progress without blocking should return
    /// [`DriverError::WouldBlock`] rather than spinning internally.
    fn write(&self, buf: &[u8]) -> DriverResult<usize>;

    /// Flush any buffered output to the underlying hardware.
    ///
    /// Default implementation is a no-op for stateless devices.
    fn flush(&self) -> DriverResult {
        Ok(())
    }
}
