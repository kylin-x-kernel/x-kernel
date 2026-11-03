// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec::Vec;

use unittest::def_test;

use super::*;

#[derive(Clone, Copy)]
enum Fault {
    None,
    Configuration,
    Mask,
    Restore,
}

struct Model {
    flags: IrqFlags,
    vector: u8,
    destination: u8,
    writes: Vec<IrqFlags>,
    fault: Fault,
}

impl Model {
    fn new(flags: IrqFlags) -> Self {
        Self {
            flags,
            vector: 0x24,
            destination: 7,
            writes: Vec::new(),
            fault: Fault::None,
        }
    }
}

impl Registers for Model {
    fn max_pin(&mut self) -> u8 {
        23
    }

    fn read(&mut self, pin: u8) -> RedirectionTableEntry {
        assert_eq!(pin, 4);
        let mut entry = RedirectionTableEntry::default();
        entry.set_vector(self.vector);
        entry.set_dest(self.destination);
        entry.set_flags(self.flags);
        entry
    }

    fn write(&mut self, pin: u8, entry: RedirectionTableEntry) {
        assert_eq!(pin, 4);
        assert_eq!(entry.vector(), self.vector);
        assert_eq!(entry.dest(), self.destination);
        self.writes.push(entry.flags());
        match self.fault {
            Fault::Configuration => self
                .flags
                .set(IrqFlags::MASKED, entry.flags().contains(IrqFlags::MASKED)),
            Fault::Mask if entry.flags().contains(IrqFlags::MASKED) => {}
            Fault::Restore if !entry.flags().contains(IrqFlags::MASKED) => {}
            _ => self.flags = entry.flags(),
        }
    }
}

#[def_test]
fn checked_configuration_masks_before_changes_and_preserves_routing() {
    for masked in [false, true] {
        let mut registers = Model::new(if masked {
            IrqFlags::MASKED
        } else {
            IrqFlags::empty()
        });
        assert!(configure(
            &mut registers,
            4,
            IoApicTriggerMode::Level,
            IoApicPolarity::Low
        ));
        assert_eq!(registers.writes.len(), if masked { 2 } else { 3 });
        assert_eq!(registers.writes[0], IrqFlags::MASKED);
        assert!(
            registers.writes[1]
                .contains(IrqFlags::MASKED | IrqFlags::LEVEL_TRIGGERED | IrqFlags::LOW_ACTIVE)
        );
        assert_eq!(registers.flags.contains(IrqFlags::MASKED), masked);
        assert_eq!(registers.destination, 7);
    }
}

#[def_test]
fn failed_readback_never_restores_delivery() {
    let mut registers = Model::new(IrqFlags::empty());
    registers.fault = Fault::Configuration;
    assert!(!configure(
        &mut registers,
        4,
        IoApicTriggerMode::Level,
        IoApicPolarity::Low
    ));
    assert_eq!(registers.writes.len(), 2);
    assert!(registers.flags.contains(IrqFlags::MASKED));
    assert!(
        registers
            .writes
            .iter()
            .all(|flags| flags.contains(IrqFlags::MASKED))
    );
    registers.fault = Fault::None;
    assert!(configure(
        &mut registers,
        4,
        IoApicTriggerMode::Level,
        IoApicPolarity::Low
    ));
    assert!(registers.flags.contains(IrqFlags::MASKED));
}

#[def_test]
fn failed_mask_or_restore_readback_reports_failure() {
    for (fault, expected_writes) in [(Fault::Mask, 1), (Fault::Restore, 3)] {
        let mut registers = Model::new(IrqFlags::empty());
        registers.fault = fault;
        assert!(!configure(
            &mut registers,
            4,
            IoApicTriggerMode::Level,
            IoApicPolarity::Low
        ));
        assert_eq!(registers.writes.len(), expected_writes);
        if matches!(fault, Fault::Mask) {
            // No trigger write or unmask is attempted after a rejected mask.
            assert_eq!(registers.flags, IrqFlags::empty());
        } else {
            assert!(registers.flags.contains(IrqFlags::MASKED));
        }
    }
}

#[def_test]
fn retrigger_uses_programmed_destination_without_changing_mask() {
    let mut registers = Model::new(IrqFlags::MASKED);
    let mut sent = None;
    assert!(retrigger(&mut registers, 4, |vector, destination| sent =
        Some((vector, destination))));
    assert_eq!(sent, Some((0x24, 7)));
    assert!(registers.writes.is_empty());
    for flags in [IrqFlags::LEVEL_TRIGGERED, IrqFlags::LOGICAL_DEST] {
        registers.flags = flags;
        assert!(!retrigger(&mut registers, 4, |_, _| panic!(
            "unsupported route"
        )));
    }
    registers.flags = IrqFlags::empty();
    registers.vector = 0x40;
    assert!(!retrigger(&mut registers, 4, |_, _| panic!("wrong vector")));
    for pin in [24, 32, usize::MAX] {
        assert!(!retrigger(&mut registers, pin, |_, _| panic!(
            "invalid pin"
        )));
        assert!(!configure(
            &mut registers,
            pin,
            IoApicTriggerMode::Edge,
            IoApicPolarity::High
        ));
    }
}
