// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Register-level IO-APIC programming and the shared route-vector pool.
//!
//! This module knows nothing about which controller instances exist: callers
//! pass a [`Registers`] implementation or an explicit route. The registry in
//! the parent module supplies both under its register lock.

use x2apic::ioapic::{IoApic, IrqFlags, IrqMode, RedirectionTableEntry};

use super::IoApicController;
use crate::{
    IO_APIC_VECTOR_BASE, IoApicInstanceId, IoApicPin, IoApicPolarity, IoApicTriggerMode,
    IoApicTriggerPolicy, MAX_IO_APIC_PINS,
};

/// One IO-APIC input pin and the controller instance that owns it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VectorRoute {
    instance: IoApicInstanceId,
    pin: IoApicPin,
}

/// Fixed pool of vectors shared by every IO-APIC instance.
///
/// A route occupies one slot for its lifetime. The pool is deliberately not
/// indexed by pin number: equal pin numbers on different instances are
/// distinct routes and receive distinct vectors.
pub(super) struct VectorTable {
    routes: [Option<VectorRoute>; MAX_IO_APIC_PINS],
}

impl VectorTable {
    pub(super) const fn new() -> Self {
        Self {
            routes: [None; MAX_IO_APIC_PINS],
        }
    }

    /// Assigns the lowest free vector to a newly configured route.
    pub(super) fn allocate(&mut self, instance: IoApicInstanceId, pin: IoApicPin) -> Option<u8> {
        let index = self.routes.iter().position(Option::is_none)?;
        self.routes[index] = Some(VectorRoute { instance, pin });
        u8::try_from(IO_APIC_VECTOR_BASE + index).ok()
    }

    /// Resolves a delivery vector to the route that was assigned it.
    pub(super) fn route(&self, vector: usize) -> Option<(IoApicInstanceId, IoApicPin)> {
        let index = vector.checked_sub(IO_APIC_VECTOR_BASE)?;
        self.routes
            .get(index)
            .copied()
            .flatten()
            .map(|route| (route.instance, route.pin))
    }
}

pub(super) trait Registers {
    fn max_pin(&mut self) -> u8;
    fn read(&mut self, pin: u8) -> RedirectionTableEntry;
    fn write(&mut self, pin: u8, entry: RedirectionTableEntry);
}

impl Registers for IoApic {
    fn max_pin(&mut self) -> u8 {
        // SAFETY: Only permanently mapped IO-APICs implement this private
        // adapter. Before publication the initializer owns its controller
        // exclusively; afterwards the registry register lock serializes access.
        unsafe { self.max_table_entry() }
    }

    fn read(&mut self, pin: u8) -> RedirectionTableEntry {
        // SAFETY: `IoApicPin` bounds the vector window and the helpers below
        // bound it by max_pin. Before publication the initializer owns the
        // controller exclusively; afterwards the caller retains the registry
        // register-select/window lock.
        unsafe { self.table_entry(pin) }
    }

    fn write(&mut self, pin: u8, entry: RedirectionTableEntry) {
        // SAFETY: The checked pin and permanent mapping remain valid. Before
        // publication the initializer owns the controller exclusively; at
        // runtime the registry lock protects each access and the surrounding
        // read/modify/write transaction.
        unsafe { self.set_table_entry(pin, entry) };
    }
}

impl Registers for IoApicController {
    fn max_pin(&mut self) -> u8 {
        self.io_apic.max_pin()
    }

    fn read(&mut self, pin: u8) -> RedirectionTableEntry {
        self.io_apic.read(pin)
    }

    fn write(&mut self, pin: u8, entry: RedirectionTableEntry) {
        self.io_apic.write(pin, entry)
    }
}

fn valid_pin(registers: &mut impl Registers, pin: IoApicPin) -> bool {
    pin.as_usize() <= usize::from(registers.max_pin())
}

/// Assigns the route's delivery vector while masked, optionally programs
/// trigger/polarity, checks readback, then restores the old mask.
///
/// A descriptor that does not state trigger or polarity still needs a delivery
/// route; [`IoApicTriggerPolicy::KeepHardware`] installs the vector without
/// rewriting those bits. Failure never intentionally unmasks, but a failed
/// mask readback cannot guarantee hardware suppression.
pub(super) fn configure(
    registers: &mut impl Registers,
    pin: IoApicPin,
    policy: IoApicTriggerPolicy,
    vector: u8,
) -> bool {
    if !valid_pin(registers, pin) {
        return false;
    }
    let pin = pin.as_u8();
    let mut entry = registers.read(pin);
    let was_masked = entry.flags().contains(IrqFlags::MASKED);
    entry.set_flags(entry.flags() | IrqFlags::MASKED);
    registers.write(pin, entry);
    let mut entry = registers.read(pin);
    if !entry.flags().contains(IrqFlags::MASKED) {
        return false;
    }
    let configuration = IrqFlags::LEVEL_TRIGGERED | IrqFlags::LOW_ACTIVE;
    let mut flags = entry.flags();
    match policy {
        IoApicTriggerPolicy::KeepHardware => {}
        IoApicTriggerPolicy::Program { trigger, polarity } => {
            flags.set(
                IrqFlags::LEVEL_TRIGGERED,
                trigger == IoApicTriggerMode::Level,
            );
            flags.set(IrqFlags::LOW_ACTIVE, polarity == IoApicPolarity::Low);
            // Changing an active level route would lose its remote-IRR
            // completion relationship. Refuse it instead of interpreting
            // software drain as EOI.
            if entry.flags().contains(IrqFlags::REMOTE_IRR)
                && (entry.flags() & configuration) != (flags & configuration)
            {
                return false;
            }
        }
    }
    entry.set_flags(flags);
    entry.set_vector(vector);
    registers.write(pin, entry);
    let mut observed = registers.read(pin);
    if observed.vector() != vector
        || observed.flags() & (configuration | IrqFlags::MASKED)
            != flags & (configuration | IrqFlags::MASKED)
    {
        return false;
    }
    if !was_masked {
        observed.set_flags(observed.flags() - IrqFlags::MASKED);
        registers.write(pin, observed);
        if registers.read(pin).flags().contains(IrqFlags::MASKED) {
            return false;
        }
    }
    true
}

/// Issues edge retrigger to the physical destination/vector read from the
/// entry. Rejects unsupported, changed or reassigned routing without sending
/// an IPI.
pub(super) fn retrigger(
    registers: &mut impl Registers,
    pin: IoApicPin,
    expected_vector: u8,
    send: impl FnOnce(u8, u8),
) -> bool {
    if !valid_pin(registers, pin) {
        return false;
    }
    let entry = registers.read(pin.as_u8());
    if entry.vector() != expected_vector
        || entry
            .flags()
            .intersects(IrqFlags::LOGICAL_DEST | IrqFlags::LEVEL_TRIGGERED)
        || !matches!(entry.mode(), IrqMode::Fixed)
    {
        return false;
    }
    send(entry.vector(), entry.dest());
    true
}
