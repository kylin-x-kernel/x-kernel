//! Device driver interfaces used by [1]. It provides common traits and
//! types for implementing a device driver.
//!
//! You have to use this crate with the following crates for corresponding
//! device types:
//!
//! - [`kdriver_block`][2]: Common traits for block storage drivers.
//! - [`kdriver_display`][3]: Common traits and types for graphics display drivers.
//! - [`net`][4]: Common traits and types for network (NIC) drivers.
//!
//! [1]:
//! [2]: ../kdriver_block/index.html
//! [3]: ../kdriver_display/index.html
//! [4]: ../net/index.html

#![no_std]
#![allow(rustdoc::broken_intra_doc_links)]

/// All supported device kinds.
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
}

/// The error type for driver operation failures.
#[derive(Debug)]
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
    /// Stable error message for display/logging.
    pub const fn message(&self) -> &'static str {
        match self {
            DriverError::AlreadyExists => "Entity already exists",
            DriverError::WouldBlock => "Try again",
            DriverError::BadState => "Bad state",
            DriverError::InvalidInput => "Invalid parameter",
            DriverError::Io => "Input/output error",
            DriverError::NoMemory => "Not enough memory",
            DriverError::ResourceBusy => "Resource is busy",
            DriverError::Unsupported => "Unsupported operation",
        }
    }
}

impl core::fmt::Display for DriverError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.message())
    }
}

/// A specialized `Result` type for device operations.
pub type DriverResult<T = ()> = Result<T, DriverError>;

/// Common operations that require all device drivers to implement.
pub trait DriverOps: Send + Sync {
    /// The name of the device.
    fn name(&self) -> &str;

    /// The kind of the device.
    fn device_kind(&self) -> DeviceKind;

    /// The IRQ number of the device, if applicable.
    fn irq(&self) -> Option<usize> {
        None
    }
}
