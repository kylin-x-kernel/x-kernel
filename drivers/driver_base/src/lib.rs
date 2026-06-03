// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device driver base interfaces for x-kernel.
//!
//! This crate provides common traits and types for implementing a device driver.
//! It is the shared dependency of all driver sub-crates and defines the unified
//! type contract for device classification, error handling, and device identity.
//!
//! # Core types
//!
//! - [`DeviceKind`] — enumeration of all supported device categories.
//! - [`DriverError`] / [`DriverResult`] — unified error type and `Result` alias.
//! - [`Device`] — the minimal trait every device implementation must expose.
//!
//! # Companion crates
//!
//! Use the following crates for device-type-specific traits:
//!
//! - `kdriver_block`: block storage drivers.
//! - `kdriver_display`: graphics display drivers.
//! - `net`: network (NIC) drivers.
//!
//! # Example
//!
//! ```
//! use driver_base::{Device, DeviceKind, DriverError, DriverResult};
//!
//! struct MyDevice;
//!
//! impl Device for MyDevice {
//!     fn name(&self) -> &str {
//!         "my-device"
//!     }
//!
//!     fn device_kind(&self) -> DeviceKind {
//!         DeviceKind::Char
//!     }
//! }
//!
//! let dev = MyDevice;
//! assert_eq!(dev.name(), "my-device");
//! assert_eq!(dev.device_kind(), DeviceKind::Char);
//! assert_eq!(dev.irq(), None);
//! ```

#![no_std]
#![allow(rustdoc::broken_intra_doc_links)]

/// All supported device kinds.
///
/// Each variant corresponds to a device category in x-kernel. The `#[repr(u8)]`
/// layout ensures a compact, `Copy`-friendly representation suitable for hot
/// paths and FFI boundaries.
///
/// # Example
///
/// ```
/// use driver_base::DeviceKind;
///
/// let kind = DeviceKind::Net;
/// assert_eq!(kind.as_str(), "net");
/// ```
#[repr(u8)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum DeviceKind {
    /// Block storage device (e.g., disk).
    Block,
    /// Character device (e.g., serial port).
    Char,
    /// Network device (e.g., ethernet card).
    Net,
    /// Graphic display device (e.g., GPU).
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
    /// Returns a stable short name for the device category.
    ///
    /// The returned string is suitable for logging and display purposes.
    /// It remains stable across crate versions.
    ///
    /// # Returns
    ///
    /// A `&'static str` identifying the category (e.g., `"block"`, `"net"`).
    ///
    /// # Example
    ///
    /// ```
    /// use driver_base::DeviceKind;
    ///
    /// assert_eq!(DeviceKind::Block.as_str(), "block");
    /// assert_eq!(DeviceKind::Fs9p.as_str(), "fs9p");
    /// assert_eq!(DeviceKind::Bus.as_str(), "bus");
    /// ```
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
///
/// Covers common failure modes shared across all driver sub-crates. Each variant
/// maps to a distinct category; callers use [`should_retry`](DriverError::should_retry)
/// to decide whether to re-attempt the operation.
///
/// # Example
///
/// ```
/// use driver_base::DriverError;
///
/// let err = DriverError::WouldBlock;
/// assert!(err.should_retry());
/// assert_eq!(err.message(), "Try again");
/// ```
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
    /// Returns whether the caller may retry the operation later.
    ///
    /// Only [`WouldBlock`](DriverError::WouldBlock) and
    /// [`ResourceBusy`](DriverError::ResourceBusy) are considered retryable.
    /// All other variants indicate permanent failures for the current request.
    ///
    /// # Returns
    ///
    /// `true` if the error is transient and the operation may succeed on retry.
    ///
    /// # Example
    ///
    /// ```
    /// use driver_base::DriverError;
    ///
    /// assert!(DriverError::WouldBlock.should_retry());
    /// assert!(DriverError::ResourceBusy.should_retry());
    /// assert!(!DriverError::Io.should_retry());
    /// ```
    pub const fn should_retry(self) -> bool {
        matches!(self, Self::WouldBlock | Self::ResourceBusy)
    }

    /// Returns a stable human-readable message for the error.
    ///
    /// Suitable for logging and diagnostics. The message is guaranteed to
    /// remain stable across crate versions.
    ///
    /// # Returns
    ///
    /// A `&'static str` describing the error.
    ///
    /// # Example
    ///
    /// ```
    /// use driver_base::DriverError;
    ///
    /// assert_eq!(DriverError::NoMemory.message(), "Not enough memory");
    /// ```
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
///
/// This alias eliminates the need to write `Result<T, DriverError>` throughout
/// the driver subsystem. The default success type is `()`.
///
/// # Example
///
/// ```
/// use driver_base::{DriverError, DriverResult};
///
/// fn try_read() -> DriverResult<Vec<u8>> {
///     Err(DriverError::WouldBlock)
/// }
///
/// assert!(try_read().is_err());
/// ```
pub type DriverResult<T = ()> = core::result::Result<T, DriverError>;

/// Common metadata that every device implementation must expose.
///
/// This trait describes the *identity* of a device instance — its name, its
/// category, and (optionally) its IRQ. Concrete device-class operations
/// (block read/write, net send/recv, ...) live in the per-category
/// sub-traits in their respective crates (`block::BlockDevice`,
/// `net::NetDevice`, etc.) and require this trait as a super-trait.
///
/// # Requirements
///
/// Implementors must be `Send + Sync` so that trait objects can be shared
/// across threads.
///
/// # Example
///
/// ```
/// use driver_base::{Device, DeviceKind};
///
/// struct SerialPort;
///
/// impl Device for SerialPort {
///     fn name(&self) -> &str {
///         "serial0"
///     }
///
///     fn device_kind(&self) -> DeviceKind {
///         DeviceKind::Char
///     }
///
///     fn irq(&self) -> Option<usize> {
///         Some(4)
///     }
/// }
///
/// let dev = SerialPort;
/// assert_eq!(dev.name(), "serial0");
/// assert_eq!(dev.device_kind().as_str(), "char");
/// assert_eq!(dev.irq(), Some(4));
/// ```
pub trait Device: Send + Sync {
    /// The name of the device.
    ///
    /// The name should be unique within the system and stable across reboots
    /// for the same hardware configuration.
    fn name(&self) -> &str;

    /// Returns the kind (category) of the device.
    ///
    /// Used by the driver framework to route the device to the appropriate
    /// subsystem for management.
    fn device_kind(&self) -> DeviceKind;

    /// Returns the IRQ number of the device, if applicable.
    ///
    /// Devices that do not use interrupts (e.g., ramdisk) should return
    /// `None`, which is the default.
    ///
    /// # Returns
    ///
    /// - `Some(irq)` if the device uses an interrupt.
    /// - `None` if the device is interrupt-free (default).
    fn irq(&self) -> Option<usize> {
        None
    }
}
