// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Common traits and types for input device drivers.

#![no_std]

extern crate alloc;

use alloc::sync::Arc;

#[doc(no_inline)]
pub use driver_base::{Device, DeviceKind, DriverError, DriverResult};
use strum::FromRepr;

/// Input event categories defined by the Linux input subsystem.
#[repr(u8)]
#[derive(Debug, Clone, Copy, Eq, PartialEq, FromRepr)]
pub enum EventType {
    /// Marker event used to separate input event epochs (value 0x00).
    Synchronization = 0x00,
    /// Key press/release events (value 0x01).
    Key             = 0x01,
    /// Relative pointer motion events (value 0x02).
    Relative        = 0x02,
    /// Absolute pointer/tablet positioning events (value 0x03).
    Absolute        = 0x03,
    /// Miscellaneous event codes (value 0x04).
    Misc            = 0x04,
    /// Switch state events (value 0x05).
    Switch          = 0x05,
    /// LED state events (value 0x11).
    Led             = 0x11,
    /// Sound output events (value 0x12).
    Sound           = 0x12,
    /// Force feedback effect events (value 0x15).
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
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
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
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
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

/// Number of absolute-axis codes addressable by `EVIOCGABS`.
pub const ABS_CNT: usize = 0x40;

/// Length of an `EV_ABS` capability bitmap in bytes.
pub const ABS_BITMAP_LEN: usize = ABS_CNT.div_ceil(u8::BITS as usize);

/// Selects the multi-touch slot used by the following `ABS_MT_*` payload.
pub const ABS_MT_SLOT: u16 = 0x2f;

/// First multi-touch payload axis. Linux stores these values per slot.
pub const ABS_MT_FIRST: u16 = 0x30;

/// Last multi-touch payload axis (`ABS_MT_TOOL_Y`).
///
/// Codes above this value, up to `ABS_MAX`, are scalar axes in Linux.
pub const ABS_MT_LAST: u16 = 0x3d;

/// Highest `INPUT_PROP_*` code defined by Linux.
pub const INPUT_PROP_MAX: usize = 0x1f;

/// Full length of the `EVIOCGPROP` bitmap before clipping to the caller.
///
/// Linux rounds the `INPUT_PROP_MAX` bit range up to a full `usize` word.
pub const PROP_BITMAP_LEN: usize =
    INPUT_PROP_MAX.div_ceil(usize::BITS as usize) * core::mem::size_of::<usize>();

/// Static metadata for one absolute axis.
///
/// This is the driver-owned half of Linux `struct input_absinfo`: the five
/// calibration and resolution fields of an axis that exists. The live value is
/// deliberately not part of the driver contract because the input core owns it,
/// just as it owns key state. The metadata and the live value are signed Linux
/// `__s32` values; [`Event::value`] carries the same bit pattern in an unsigned
/// transport word.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AbsInfo {
    /// The minimum value for the axis.
    pub min: i32,
    /// The maximum value for the axis.
    pub max: i32,
    /// The fuzz value used to filter noise from the event stream.
    pub fuzz: i32,
    /// The size of the dead zone; values less than this are reported as 0.
    pub flat: i32,
    /// The resolution for values reported for the axis.
    pub res: i32,
}

impl AbsInfo {
    /// Creates absolute-axis metadata.
    pub const fn new(min: i32, max: i32, fuzz: i32, flat: i32, res: i32) -> Self {
        Self {
            min,
            max,
            fuzz,
            flat,
            res,
        }
    }
}

/// IRQ-safe notification that an input device may have completed events.
///
/// Implementations must only schedule input processing. They must not sleep,
/// allocate, or call back into the driver from interrupt context.
pub trait InputEventNotifier: Send + Sync {
    /// Schedule task-context processing of completed input events.
    fn notify_events(&self);
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
    ///
    /// `out` may be empty when the caller only probes support. The return value
    /// must reflect the device capability and must not depend on `out.len()`.
    /// When supported, implementations write at most `out.len()` leading bitmap
    /// bytes and leave the remainder untouched.
    fn get_event_bits(&self, ty: EventType, out: &mut [u8]) -> DriverResult<bool>;

    /// Returns the static metadata of one absolute axis.
    ///
    /// `code` is an `ABS_*` event code. `Some` means the device advertises that
    /// axis and returned its calibration metadata. `None` means the axis is not
    /// advertised; this covers both a device without `EV_ABS` and an `EV_ABS`
    /// device that does not expose this particular axis. Linux exposes a
    /// zero-filled `input_absinfo` in the latter case.
    ///
    /// Whether the device supports `EV_ABS` at all is a capability question and
    /// is answered by [`Self::get_event_bits`]; a device without `EV_ABS` never
    /// reaches this method through evdev. Drivers must not include the live axis
    /// position here: the input core tracks it from the `EV_ABS` event stream.
    ///
    /// Drivers that have no absolute axes may use the default implementation.
    fn abs_info(&self, _code: u16) -> DriverResult<Option<AbsInfo>> {
        Ok(None)
    }

    /// Fetches the `INPUT_PROP_*` capability bitmap.
    ///
    /// The bitmap is a little-endian bit array indexed by property code, using
    /// the same layout as `get_event_bits`. Implementations fill the leading
    /// bytes they report and leave the rest of `out` untouched. Returns true
    /// when the device returned a property bitmap (which may be all zeros)
    /// and false when it reports none.
    /// Callers that return a fixed ABI bitmap length must zero `out` first;
    /// implementations are not required to clear the bytes they do not fill.
    ///
    /// Drivers that report no properties may use the default implementation.
    fn get_prop_bits(&self, _out: &mut [u8]) -> DriverResult<bool> {
        Ok(false)
    }

    /// Reads an input event from the device.
    ///
    /// If no events are available, `Err(DriverError::WouldBlock)` is returned.
    fn read_event(&self) -> DriverResult<Event>;

    /// Returns a bounded, IRQ-safe snapshot of whether an event may be ready.
    ///
    /// This is an optimization hint for timer-driven poll fallbacks, not an
    /// event source: it must not consume events, allocate, sleep or sleep on a
    /// lock. `None` means unsupported and preserves the caller's ordinary retry
    /// wake. `Some(false)` may suppress an idle wake; `Some(true)` still requires
    /// the caller to recheck through normal polling/reading. False negatives are
    /// not permitted: uncertain state must return `Some(true)` or `None`.
    fn poll_event_ready(&self) -> Option<bool> {
        None
    }

    /// Attach or detach the input layer's event notification capability.
    ///
    /// Registration and removal run in task context, but an installed notifier
    /// may be called from hard IRQ context. The driver must synchronize changes
    /// with its IRQ handler so a completed event is either observed by the
    /// input layer's initial drain or triggers a notification. The callback
    /// must run without driver locks that an event read might acquire.
    ///
    /// Drivers without event notifications may use the default implementation.
    /// The current input layer requires notifications for activation and treats
    /// `Unsupported` as an activation failure.
    ///
    /// # Errors
    ///
    /// Returns `Unsupported` if the driver does not provide notifications.
    fn set_event_notifier(&self, _notifier: Option<Arc<dyn InputEventNotifier>>) -> DriverResult {
        Err(DriverError::Unsupported)
    }
}
