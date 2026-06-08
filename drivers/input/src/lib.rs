// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Common traits and types for input device drivers.

#![no_std]

#[doc(no_inline)]
pub use driver_base::{Device, DeviceKind, DriverError, DriverResult};
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

    const fn bit_len_of(kind: Self) -> usize {
        use EventType::*;

        match kind {
            Synchronization | Relative | Led => 0x10,
            Key => 0x300,
            Absolute => 0x40,
            Misc | Sound => 0x08,
            Switch => 0x12,
            ForceFeedback => 0x80,
        }
    }

    /// Return the bitset length for the given event type.
    pub const fn bits_count(self) -> usize {
        Self::bit_len_of(self)
    }
}

/// An input event, as defined by the Linux input subsystem.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct Event {
    /// Event category (matches `EventType`).
    pub event_type: u16,
    /// Event code within the category.
    pub code: u16,
    /// Event value/payload.
    pub value: u32,
}

impl Event {
    /// Builds an input event from raw evdev-compatible fields.
    pub const fn new(event_type: u16, code: u16, value: u32) -> Self {
        Self {
            event_type,
            code,
            value,
        }
    }

    /// Returns whether the event belongs to the given category.
    pub const fn is_type(self, kind: EventType) -> bool {
        self.event_type == kind as u16
    }
}

/// Identification tuple for an input device.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
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

impl InputDeviceId {
    /// Empty/default identifier used when the device does not expose IDs.
    pub const UNKNOWN: Self = Self::new(0, 0, 0, 0);

    /// Creates a device identifier tuple.
    pub const fn new(bus_type: u16, vendor: u16, product: u16, version: u16) -> Self {
        Self {
            bus_type,
            vendor,
            product,
            version,
        }
    }
}

/// Axis information for absolute input devices.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
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

impl AbsInfo {
    /// Creates an absolute-axis descriptor.
    pub const fn new(min: u32, max: u32, fuzz: u32, flat: u32, res: u32) -> Self {
        Self {
            min,
            max,
            fuzz,
            flat,
            res,
        }
    }
}

/// Operations that require an input device driver to implement.
pub trait InputDevice: Device {
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
    fn get_event_bits(&self, ty: EventType, out: &mut [u8]) -> DriverResult<bool>;

    /// Reads an input event from the device.
    ///
    /// If no events are available, `Err(DriverError::WouldBlock)` is returned.
    fn read_event(&self) -> DriverResult<Event>;
}
