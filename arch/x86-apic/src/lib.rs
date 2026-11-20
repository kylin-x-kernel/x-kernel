// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! x86 APIC integration: Local APIC bring-up, IPIs, IO-APIC routing, and
//! the MSI-X vector backend for `kirq`.
//!
//! The crate is organized by hardware object and lifetime:
//!
//! - [`local_apic`] owns the per-CPU Local APIC handle, its bring-up, EOI,
//!   error acknowledgment, timer mask, and IPI senders.
//! - [`io_apic`] owns the bounded IO-APIC instance registry, the two-phase
//!   probe/install boot lifecycle, per-pin operations, and dispatch routing.
//! - [`msix`] owns the shared MSI-X vector allocator and the `kirq` backend
//!   provider.
//!
//! Start from `init_local_primary`, then probe and install every IO-APIC
//! instance (boot CPU, once during platform bring-up), and `init_secondary`
//! (each application CPU); after that, `send_ipi_self` / `send_ipi_raw` /
//! `send_ipi_all_but_self`, `configure_pin_checked`, `set_pin_enabled`, and
//! `end_of_interrupt` drive the hardware. Production boot goes through
//! `irq_driver::x86::init`, which probes the whole topology, validates ranges,
//! and only then installs any controller.
//!
//! # Example
//!
//! Kernel code only (x86_64 target; runs after the platform maps the
//! IO-APIC), so the example is `ignore`.
//!
//! ```ignore
//! use drivers::platform::x86_apic;
//!
//! // 1. Boot CPU bring-up: LAPIC vectors, then probe every IO-APIC without
//! //    programming it.
//! x86_apic::init_local_primary()?;
//! let first = x86_apic::probe_io_apic(
//!     /* address from the platform's declaration */,
//!     /* controller's first GSI from firmware */,
//! )?;
//! let instance = x86_apic::install_io_apic(
//!     first,
//!     x86_apic::IoApicInitPolicy::platform_legacy(),
//! )?;
//! x86_apic::init_secondary()?; // on each AP
//!
//! // 2. Route and enable a legacy line, then acknowledge it. The instance and
//! //    pin together name the source; a vector is assigned on first use.
//! let pin = x86_apic::IoApicPin::new(4).expect("pin 4 is addressable");
//! x86_apic::configure_pin_checked(
//!     instance,
//!     pin,
//!     x86_apic::IoApicTriggerPolicy::Program {
//!         trigger: x86_apic::IoApicTriggerMode::Edge,
//!         polarity: x86_apic::IoApicPolarity::High,
//!     },
//! );
//! x86_apic::set_pin_enabled(instance, pin, true);
//! // ... interrupt arrives on this CPU ...
//! x86_apic::end_of_interrupt();
//! ```
#![no_std]
#![cfg(target_arch = "x86_64")]

extern crate alloc;

#[macro_use]
extern crate log;

mod io_apic;
mod local_apic;
mod msix;

pub use io_apic::{
    IoApicRetargetResult, ProbedIoApic, SavedIoApics, configure_pin_checked, finish_level_retarget,
    install_io_apic, io_apic_route, pin_for_gsi_in_range, pin_trigger_mode, prepare_level_retarget,
    probe_io_apic, retarget_edge, retarget_supported, retrigger_pin, set_pin_enabled,
};
pub use local_apic::{
    end_of_interrupt, handle_error_interrupt, init_local_primary, init_secondary, raw_apic_id,
    send_ipi_all_but_self, send_ipi_raw, send_ipi_self, set_local_timer_enabled, with_local_apic,
};
pub use msix::{msi_route_matches, retrigger_msi};

pub const APIC_TIMER_VECTOR: u8 = 0xf0;
pub const APIC_SPURIOUS_VECTOR: u8 = 0xf1;
pub const APIC_ERROR_VECTOR: u8 = 0xf2;
pub const MSIX_VECTOR_BASE: u8 = 0x40;
pub const IO_APIC_VECTOR_BASE: usize = 0x20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoApicTriggerMode {
    Edge,
    Level,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoApicPolarity {
    High,
    Low,
}

/// Number of delivery vectors reserved for IO-APIC routes below the MSI
/// window.
///
/// Vectors are allocated per configured `(instance, pin)` route, so this is
/// also the maximum number of simultaneously configured IO-APIC pins across
/// all instances. Each instance's addressable local pin number is bounded by
/// the same value, independent of how many pins the hardware implements.
pub const MAX_IO_APIC_PINS: usize = MSIX_VECTOR_BASE as usize - IO_APIC_VECTOR_BASE;

/// Maximum number of IO-APIC controller instances retained by the backend.
pub const MAX_IO_APICS: usize = 32;

/// Checked index of one initialized IO-APIC controller instance.
///
/// A controller instance owns one redirection table and one GSI range. Local
/// pins from different instances are distinct sources even when their pin
/// numbers are equal; the instance identifier, not the pin number, selects the
/// controller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoApicInstanceId(u8);

impl IoApicInstanceId {
    /// Creates an instance identifier from its bounded array index.
    pub const fn try_from_usize(index: usize) -> Option<Self> {
        if index < MAX_IO_APICS {
            Some(Self(index as u8))
        } else {
            None
        }
    }

    /// Returns the instance identifier as an array index.
    pub const fn as_usize(self) -> usize {
        self.0 as usize
    }
}

/// A checked IO-APIC input pin.
///
/// A pin selects one redirection-table entry on one IO-APIC. It is neither a
/// firmware GSI nor a CPU vector; the owning instance assigns a delivery
/// vector when the route is configured. Pins above [`Self::MAX`] remain
/// masked because the backend has no vector capacity to describe them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoApicPin(u8);

impl IoApicPin {
    /// Largest pin the backend can assign a delivery vector to.
    pub const MAX: usize = MAX_IO_APIC_PINS - 1;

    /// Creates a pin from its 8-bit hardware number.
    ///
    /// Returns `None` when the backend cannot assign a delivery vector to the
    /// value.
    pub const fn new(pin: u8) -> Option<Self> {
        if (pin as usize) < MAX_IO_APIC_PINS {
            Some(Self(pin))
        } else {
            None
        }
    }

    /// Creates a pin from a register-index-sized number.
    ///
    /// Returns `None` when the backend cannot assign a delivery vector to the
    /// value.
    pub const fn try_from_usize(pin: usize) -> Option<Self> {
        if pin < MAX_IO_APIC_PINS {
            Some(Self(pin as u8))
        } else {
            None
        }
    }

    /// Returns the hardware pin number.
    pub const fn as_u8(self) -> u8 {
        self.0
    }

    /// Returns the hardware pin number as a register-index-sized value.
    pub const fn as_usize(self) -> usize {
        self.0 as usize
    }
}

/// Hardware capability reported by a successfully initialized IO-APIC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoApicCapabilities {
    /// Number of input pins implemented by the controller, in `1..=256`.
    pub pin_count: u16,
}

impl IoApicCapabilities {
    fn from_max_table_entry(max_entry: u8) -> Self {
        Self {
            pin_count: u16::from(max_entry) + 1,
        }
    }
}

/// Initial redirection policy applied while bringing up an IO-APIC.
///
/// The backend does not infer ISA or PCI semantics from pin numbers. The
/// caller supplies that policy; [`Self::platform_legacy`] preserves the
/// historical PC rule until firmware interrupt-source overrides replace it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoApicInitPolicy {
    /// Pins at or above this number start as level-triggered and low-active.
    pub default_level_low_from: Option<u8>,
}

impl IoApicInitPolicy {
    /// Preserves the pre-existing PC compatibility default.
    pub const fn platform_legacy() -> Self {
        Self {
            default_level_low_from: Some(10),
        }
    }
}

/// How configuring a route treats the redirection entry's trigger and polarity.
///
/// Assigning a delivery vector is independent of trigger metadata. A
/// descriptor that does not state trigger or polarity must still be routed
/// through the controller; [`Self::KeepHardware`] installs the vector while
/// retaining the bits the boot policy already programmed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoApicTriggerPolicy {
    /// Keep the trigger and polarity currently programmed in the entry.
    KeepHardware,
    /// Program the given trigger and polarity together with the vector.
    Program {
        trigger: IoApicTriggerMode,
        polarity: IoApicPolarity,
    },
}
