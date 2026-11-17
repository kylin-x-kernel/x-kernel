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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

    /// Observes the read-only Remote IRR bit for a pin.
    ///
    /// Production reads the RTE, where the bit is not writable. Tests can
    /// override only this observation without fabricating a private RTE layout.
    fn remote_irr(&mut self, pin: u8) -> bool {
        self.read(pin).flags().contains(IrqFlags::REMOTE_IRR)
    }
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

fn entry_accepts_physical(entry: &RedirectionTableEntry) -> bool {
    matches!(entry.mode(), IrqMode::Fixed) && !entry.flags().contains(IrqFlags::LOGICAL_DEST)
}

/// Masks one valid physical RTE and returns the original mask state.
pub(super) fn prepare_retarget_mask(
    registers: &mut impl Registers,
    pin: IoApicPin,
) -> Option<bool> {
    if !valid_pin(registers, pin) {
        return None;
    }
    let entry = registers.read(pin.as_u8());
    if !entry_accepts_physical(&entry) {
        return None;
    }
    let was_masked = entry.flags().contains(IrqFlags::MASKED);
    if !was_masked {
        let mut masked = registers.read(pin.as_u8());
        masked.set_flags(masked.flags() | IrqFlags::MASKED);
        registers.write(pin.as_u8(), masked);
        if !registers
            .read(pin.as_u8())
            .flags()
            .contains(IrqFlags::MASKED)
        {
            return None;
        }
    }
    Some(was_masked)
}

fn restore_mask(
    registers: &mut impl Registers,
    pin: IoApicPin,
    was_masked: bool,
    keep_masked: bool,
) -> bool {
    if was_masked || keep_masked {
        return registers
            .read(pin.as_u8())
            .flags()
            .contains(IrqFlags::MASKED);
    }
    let mut entry = registers.read(pin.as_u8());
    entry.set_flags(entry.flags() - IrqFlags::MASKED);
    registers.write(pin.as_u8(), entry);
    !registers
        .read(pin.as_u8())
        .flags()
        .contains(IrqFlags::MASKED)
}

/// Completes an edge RTE destination transaction before EOI.
pub(super) fn retarget_edge(
    registers: &mut impl Registers,
    pin: IoApicPin,
    destination: u8,
    keep_masked: bool,
) -> bool {
    let Some(was_masked) = prepare_retarget_mask(registers, pin) else {
        return false;
    };
    let expected_vector = registers.read(pin.as_u8()).vector();
    let mut updated = registers.read(pin.as_u8());
    updated.set_dest(destination);
    registers.write(pin.as_u8(), updated);
    let observed = registers.read(pin.as_u8());
    if observed.dest() != destination
        || observed.vector() != expected_vector
        || !entry_accepts_physical(&observed)
    {
        return false;
    }
    restore_mask(registers, pin, was_masked, keep_masked)
}

/// Finishes a level RTE transaction after EOI.
pub(super) fn finish_level(
    registers: &mut impl Registers,
    pin: IoApicPin,
    destination: u8,
    was_masked: bool,
    keep_masked: bool,
) -> super::IoApicRetargetResult {
    if !valid_pin(registers, pin) {
        return super::IoApicRetargetResult::Failed;
    }
    let observed = registers.read(pin.as_u8());
    if registers.remote_irr(pin.as_u8()) {
        return if restore_mask(registers, pin, was_masked, keep_masked) {
            super::IoApicRetargetResult::Pending
        } else {
            super::IoApicRetargetResult::Failed
        };
    }
    let expected_vector = observed.vector();
    let mut updated = registers.read(pin.as_u8());
    updated.set_dest(destination);
    registers.write(pin.as_u8(), updated);
    let readback = registers.read(pin.as_u8());
    if readback.dest() != destination
        || readback.vector() != expected_vector
        || !entry_accepts_physical(&readback)
    {
        return super::IoApicRetargetResult::Failed;
    }
    if restore_mask(registers, pin, was_masked, keep_masked) {
        super::IoApicRetargetResult::Complete
    } else {
        super::IoApicRetargetResult::Failed
    }
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
