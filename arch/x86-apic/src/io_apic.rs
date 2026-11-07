// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Checked IO-APIC programming and fixed-route edge retrigger.

use x2apic::ioapic::{IoApic, IrqFlags, IrqMode, RedirectionTableEntry};

use super::{IO_APIC, IO_APIC_VECTOR_BASE, IoApicPin, IoApicPolarity, IoApicTriggerMode};

pub(super) trait Registers {
    fn max_pin(&mut self) -> u8;
    fn read(&mut self, pin: u8) -> RedirectionTableEntry;
    fn write(&mut self, pin: u8, entry: RedirectionTableEntry);
}

impl Registers for IoApic {
    fn max_pin(&mut self) -> u8 {
        // SAFETY: Only the permanently mapped IO_APIC implements this private
        // adapter in production. Before publication the initializer owns it
        // exclusively; afterwards its register lock serializes access.
        unsafe { self.max_table_entry() }
    }

    fn read(&mut self, pin: u8) -> RedirectionTableEntry {
        // SAFETY: `IoApicPin` bounds the fixed vector window and the helpers
        // below bound it by max_pin. Before publication the initializer owns
        // the controller exclusively; afterwards the caller retains the
        // IO-APIC register-select/window lock.
        unsafe { self.table_entry(pin) }
    }

    fn write(&mut self, pin: u8, entry: RedirectionTableEntry) {
        // SAFETY: The checked pin and permanent mapping remain valid. Before
        // publication the initializer owns the controller exclusively; at
        // runtime the IO-APIC register lock protects each access and the
        // surrounding read/modify/write transaction.
        unsafe { self.set_table_entry(pin, entry) };
    }
}

fn valid_pin(registers: &mut impl Registers, pin: IoApicPin) -> bool {
    pin.as_usize() <= usize::from(registers.max_pin())
}

/// Programs trigger/polarity while masked, checks readback, then restores the
/// old mask. Failure never intentionally unmasks, but a failed mask readback
/// cannot guarantee hardware suppression.
/// Does not change routing or prove that already delivered interrupts drained.
/// Requires initialized controller state; call in task/ordinary IRQ context,
/// never NMI, while the source owner excludes competing configuration.
pub fn configure_pin_checked(
    pin: IoApicPin,
    trigger: IoApicTriggerMode,
    polarity: IoApicPolarity,
) -> bool {
    configure(&mut *IO_APIC.lock(), pin, trigger, polarity)
}

fn configure(
    registers: &mut impl Registers,
    pin: IoApicPin,
    trigger: IoApicTriggerMode,
    polarity: IoApicPolarity,
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
    // Changing an active level route would lose its remote-IRR completion
    // relationship. Refuse it instead of interpreting software drain as EOI.
    let mut flags = entry.flags();
    flags.set(
        IrqFlags::LEVEL_TRIGGERED,
        trigger == IoApicTriggerMode::Level,
    );
    flags.set(IrqFlags::LOW_ACTIVE, polarity == IoApicPolarity::Low);
    let configuration = IrqFlags::LEVEL_TRIGGERED | IrqFlags::LOW_ACTIVE;
    if entry.flags().contains(IrqFlags::REMOTE_IRR)
        && (entry.flags() & configuration) != (flags & configuration)
    {
        return false;
    }
    entry.set_flags(flags);
    registers.write(pin, entry);
    let mut observed = registers.read(pin);
    if observed.flags() & (configuration | IrqFlags::MASKED)
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

/// Issues edge retrigger to the physical destination/vector read under the
/// IO-APIC lock. Rejects unsupported or changed routing without sending an IPI.
/// Fixed IO-APIC vectors are never allocated from the MSI vector pool.
/// Call in task/ordinary IRQ context after local APIC and IO-APIC preparation,
/// with source ownership retained. This is not NMI-safe or a delivery drain.
pub fn retrigger_pin(pin: IoApicPin) -> bool {
    retrigger(&mut *IO_APIC.lock(), pin, |vector, destination| {
        super::send_ipi_raw(
            usize::from(vector),
            kcpu_id_map::RawCpuId::new(usize::from(destination)),
        );
    })
}

fn retrigger(registers: &mut impl Registers, pin: IoApicPin, send: impl FnOnce(u8, u8)) -> bool {
    if !valid_pin(registers, pin) {
        return false;
    }
    let entry = registers.read(pin.as_u8());
    if entry.vector() as usize != IO_APIC_VECTOR_BASE + pin.as_usize()
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

unittest::path_mod_test!(tests, "../ut-tests/io_apic.rs");
