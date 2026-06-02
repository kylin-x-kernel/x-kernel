// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device driver interfaces used by x-kernel. It provides common traits and
//! types for implementing a device driver.
//!
//! You have to use this crate with the following crates for corresponding
//! device types:
//!
//! - [`kdriver_block`][2]: Common traits for block storage drivers.
//! - [`kdriver_display`][3]: Common traits and types for graphics display drivers.
//! - [`net`][4]: Common traits and types for network (NIC) drivers.
//!
//! [2]: ../kdriver_block/index.html
//! [3]: ../kdriver_display/index.html
//! [4]: ../net/index.html

#![no_std]
#![allow(rustdoc::broken_intra_doc_links)]

/// All supported device kinds.
#[repr(u8)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum DeviceKind {
    /// Block storage device (e.g., disk).
    Block,
    /// Character device (e.g., serial port).
    Char,
    /// Network device (e.g., ethernet card).
    Net,
    /// Graphic display device (e.g., GPU)
    Display,
    /// Input device (e.g., keyboard, mouse).
    Input,
    /// Vsock device (e.g., virtio-vsock).
    Vsock,
    /// 9P filesystem transport device.
    Fs9p,
    /// Bus controller / bridge (e.g., PCI host bridge).
    Bus,
}

impl DeviceKind {
    /// Stable short name for the device category.
    pub const fn as_str(self) -> &'static str {
        use DeviceKind::*;

        match self {
            Block => "block",
            Char => "char",
            Net => "net",
            Display => "display",
            Input => "input",
            Vsock => "vsock",
            Fs9p => "fs9p",
            Bus => "bus",
        }
    }
}

/// The error type for driver operation failures.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum DriverError {
    /// An entity already exists.
    AlreadyExists,
    /// Try again, for non-blocking APIs.
    WouldBlock,
    /// Bad internal state.
    BadState,
    /// Invalid parameter/argument.
    InvalidInput,
    /// Input/output error.
    Io,
    /// Not enough space/cannot allocate memory (DMA).
    NoMemory,
    /// Device or resource is busy.
    ResourceBusy,
    /// This operation is unsupported or unimplemented.
    Unsupported,
}

impl DriverError {
    /// Whether the caller may retry the operation later.
    pub const fn should_retry(self) -> bool {
        matches!(self, Self::WouldBlock | Self::ResourceBusy)
    }

    /// Stable error message for display/logging.
    pub const fn message(self) -> &'static str {
        use DriverError::*;

        match self {
            AlreadyExists => "Entity already exists",
            WouldBlock => "Try again",
            BadState => "Bad state",
            InvalidInput => "Invalid parameter",
            Io => "Input/output error",
            NoMemory => "Not enough memory",
            ResourceBusy => "Resource is busy",
            Unsupported => "Unsupported operation",
        }
    }
}

impl core::fmt::Display for DriverError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str((*self).message())
    }
}

/// A specialized `Result` type for device operations.
pub type DriverResult<T = ()> = core::result::Result<T, DriverError>;

/// Common metadata that every device implementation must expose.
///
/// This trait describes the *identity* of a device instance — its name, its
/// category, and (optionally) its IRQ. Concrete device-class operations
/// (block read/write, net send/recv, ...) live in the per-category
/// sub-traits in their respective crates (`block::BlockDevice`,
/// `net::NetDevice`, etc.) and require this trait as a super-trait.
pub trait Device: Send + Sync {
    /// The name of the device.
    fn name(&self) -> &str;

    /// The kind of the device.
    fn device_kind(&self) -> DeviceKind;

    /// The IRQ number of the device, if applicable.
    fn irq(&self) -> Option<usize> {
        None
    }
}
