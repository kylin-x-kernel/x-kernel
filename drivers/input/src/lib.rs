// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Common traits and types for graphics display device drivers.

//! Common traits and types for input device drivers.

#![no_std]

#[doc(no_inline)]
pub use driver_base::{DeviceKind, DriverError, DriverOps, DriverResult};
use strum::FromRepr;

/// Input event categories defined by the Linux input subsystem.
#[repr(u8)]
#[derive(Debug, Clone, Copy, Eq, PartialEq, FromRepr)]
pub enum EventType {
    Synchronization = 0x00,
    Key             = 0x01,
    Relative        = 0x02,
    Absolute        = 0x03,
    Misc            = 0x04,
    Switch          = 0x05,
    Led             = 0x11,
    Sound           = 0x12,
    ForceFeedback   = 0x15,
}

impl EventType {
    /// Total number of event type slots.
    pub const COUNT: u8 = Self::MAX + 1;
    /// Maximum event type value.
    pub const MAX: u8 = 0x1f;

    /// Return the bitset length for the given event type.
    pub const fn bits_count(&self) -> usize {
        match self {
            EventType::Synchronization => 0x10,
            EventType::Key => 0x300,
            EventType::Relative => 0x10,
            EventType::Absolute => 0x40,
            EventType::Misc => 0x08,
            EventType::Switch => 0x12,
            EventType::Led => 0x10,
            EventType::Sound => 0x08,
            EventType::ForceFeedback => 0x80,
        }
    }
}

/// An input event, as defined by the Linux input subsystem.
#[repr(C)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct Event {
    /// Event category (matches `EventType`).
    pub event_type: u16,
    /// Event code within the category.
    pub code: u16,
    /// Event value/payload.
    pub value: u32,
}

/// Identification tuple for an input device.
#[repr(C)]
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct InputDeviceId {
    /// The bustype identifier.
    pub bus_type: u16,
    /// The vendor identifier.
    pub vendor: u16,
    /// The product identifier.
    pub product: u16,
    /// The version identifier.
    pub version: u16,
}

/// Axis information for absolute input devices.
#[repr(C)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct AbsInfo {
    /// The minimum value for the axis.
    pub min: u32,
    /// The maximum value for the axis.
    pub max: u32,
    /// The fuzz value used to filter noise from the event stream.
    pub fuzz: u32,
    /// The size of the dead zone; values less than this will be reported as 0.
    pub flat: u32,
    /// The resolution for values reported for the axis.
    pub res: u32,
}

/// Operations that require an input device driver to implement.
pub trait InputDriverOps: DriverOps {
    /// Returns the device ID of the input device.
    fn device_id(&self) -> InputDeviceId;

    /// Returns the physical location of the input device.
    fn physical_location(&self) -> &str;

    /// Returns a unique ID of the input device.
    fn unique_id(&self) -> &str;

    /// Fetches the bitmap of supported event codes for the specified event
    /// type.
    ///
    /// Returns true if the event type is supported and the bitmap is written to
    /// `out`.
    fn get_event_bits(&mut self, ty: EventType, out: &mut [u8]) -> DriverResult<bool>;

    /// Reads an input event from the device.
    ///
    /// If no events are available, `Err(DriverError::WouldBlock)` is returned.
    fn read_event(&mut self) -> DriverResult<Event>;
}
