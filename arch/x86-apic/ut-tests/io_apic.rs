// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec::Vec;

use unittest::def_test;

use super::*;

fn pin(pin: usize) -> IoApicPin {
    IoApicPin::try_from_usize(pin).expect("test pin within the vector window")
}

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
            pin(4),
            IoApicTriggerPolicy::Program {
                trigger: IoApicTriggerMode::Level,
                polarity: IoApicPolarity::Low,
            },
            0x24
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
fn keeping_hardware_trigger_still_installs_a_verified_vector() {
    let hardware = IrqFlags::MASKED | IrqFlags::LEVEL_TRIGGERED | IrqFlags::LOW_ACTIVE;
    let mut registers = Model::new(hardware);
    assert!(configure(
        &mut registers,
        pin(4),
        IoApicTriggerPolicy::KeepHardware,
        0x24
    ));
    assert_eq!(registers.writes.len(), 2);
    assert!(registers.writes.iter().all(|flags| *flags == hardware));
    assert_eq!(registers.flags, hardware);
}

#[def_test]
fn failed_readback_never_restores_delivery() {
    let mut registers = Model::new(IrqFlags::empty());
    registers.fault = Fault::Configuration;
    assert!(!configure(
        &mut registers,
        pin(4),
        IoApicTriggerPolicy::Program {
            trigger: IoApicTriggerMode::Level,
            polarity: IoApicPolarity::Low,
        },
        0x24
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
        pin(4),
        IoApicTriggerPolicy::Program {
            trigger: IoApicTriggerMode::Level,
            polarity: IoApicPolarity::Low,
        },
        0x24
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
            pin(4),
            IoApicTriggerPolicy::Program {
                trigger: IoApicTriggerMode::Level,
                polarity: IoApicPolarity::Low,
            },
            0x24
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
    assert!(retrigger(&mut registers, pin(4), 0x24, |vector, destination| sent =
        Some((vector, destination))));
    assert_eq!(sent, Some((0x24, 7)));
    assert!(registers.writes.is_empty());
    for flags in [IrqFlags::LEVEL_TRIGGERED, IrqFlags::LOGICAL_DEST] {
        registers.flags = flags;
        assert!(!retrigger(&mut registers, pin(4), 0x24, |_, _| panic!(
            "unsupported route"
        )));
    }
    registers.flags = IrqFlags::empty();
    registers.vector = 0x40;
    assert!(!retrigger(&mut registers, pin(4), 0x24, |_, _| panic!(
        "wrong vector"
    )));
    for pin in [pin(24), pin(31)] {
        assert!(!retrigger(&mut registers, pin, 0x24, |_, _| panic!(
            "pin beyond hardware table"
        )));
        assert!(!configure(
            &mut registers,
            pin,
            IoApicTriggerPolicy::Program {
                trigger: IoApicTriggerMode::Edge,
                polarity: IoApicPolarity::High,
            },
            0x24
        ));
    }
    for pin in [32, usize::MAX] {
        assert!(IoApicPin::try_from_usize(pin).is_none());
    }
}

struct InitModel {
    max_pin: u8,
    writes: Vec<(u8, RedirectionTableEntry)>,
}

impl Registers for InitModel {
    fn max_pin(&mut self) -> u8 {
        self.max_pin
    }

    fn read(&mut self, _pin: u8) -> RedirectionTableEntry {
        panic!("initial programming must not read redirection entries")
    }

    fn write(&mut self, pin: u8, entry: RedirectionTableEntry) {
        self.writes.push((pin, entry));
    }
}

#[def_test]
fn initialization_masks_every_hardware_input_and_leaves_vectors_unassigned() {
    let mut model = InitModel {
        max_pin: 47,
        writes: Vec::new(),
    };
    crate::io_apic::program_initial_redirection_table(
        &mut model,
        47,
        crate::IoApicInitPolicy::platform_legacy(),
    );

    assert_eq!(model.writes.len(), 48);
    for (expected_pin, write) in model.writes.iter().enumerate() {
        let pin = write.0;
        let entry = &write.1;
        assert_eq!(usize::from(pin), expected_pin);
        assert!(
            entry.flags().contains(IrqFlags::MASKED),
            "pin {pin} must be masked during initialization"
        );
        assert_eq!(
            entry.vector(),
            0,
            "pin {pin} must wait for a route-assigned vector"
        );
        assert_eq!(entry.dest(), 0);
        assert!(matches!(entry.mode(), IrqMode::Fixed));
        let level_low = expected_pin >= 10;
        assert_eq!(
            entry.flags().contains(IrqFlags::LEVEL_TRIGGERED),
            level_low
        );
        assert_eq!(entry.flags().contains(IrqFlags::LOW_ACTIVE), level_low);
    }
}

#[def_test]
fn vector_table_separates_equal_pins_on_different_instances() {
    let first = IoApicInstanceId::try_from_usize(0).expect("first instance");
    let second = IoApicInstanceId::try_from_usize(1).expect("second instance");
    let mut table = VectorTable::new();

    let first_vector = table.allocate(first, pin(4)).expect("free vector");
    let second_vector = table.allocate(second, pin(4)).expect("free vector");
    assert_ne!(first_vector, second_vector);
    assert_eq!(first_vector as usize, IO_APIC_VECTOR_BASE);
    assert_eq!(second_vector as usize, IO_APIC_VECTOR_BASE + 1);
    assert_eq!(
        table.route(first_vector as usize),
        Some((first, pin(4)))
    );
    assert_eq!(
        table.route(second_vector as usize),
        Some((second, pin(4)))
    );
    assert_eq!(table.route(IO_APIC_VECTOR_BASE + 2), None);
}

#[def_test]
fn vector_table_exhaustion_is_reported() {
    let instance = IoApicInstanceId::try_from_usize(0).expect("instance");
    let mut table = VectorTable::new();
    for offset in 0..MAX_IO_APIC_PINS {
        let assigned = table
            .allocate(instance, pin(offset))
            .expect("pool has one slot per pin");
        assert_eq!(assigned as usize, IO_APIC_VECTOR_BASE + offset);
    }
    assert_eq!(table.allocate(instance, pin(0)), None);
}
