// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec::Vec;
use core::cell::Cell;

use acpi::MadtEntryIter;
use fwnode::FwSource;
use kirq::IrqTrigger;
use unittest::{assert, assert_eq, def_test};

use super::{
    MAX_IO_APICS, X86ApicDeclarations, X86DiscoveryError, X86DiscoveryIssue, X86IrqInitConfig,
    from_entries,
};
use crate::x86::IoApicDeclaration;

const BASE_OFFSET: usize = 44;

fn push_entry(entries: &mut Vec<u8>, entry_type: u8, body: &[u8]) {
    entries.push(entry_type);
    entries.push((body.len() + 2) as u8);
    entries.extend_from_slice(body);
}

fn push_io_apic(entries: &mut Vec<u8>, id: u8, address: u32, gsi_base: u32) {
    let mut body = Vec::new();
    body.push(id);
    body.push(0);
    body.extend_from_slice(&address.to_le_bytes());
    body.extend_from_slice(&gsi_base.to_le_bytes());
    push_entry(entries, 1, &body);
}

fn push_iso(entries: &mut Vec<u8>, bus: u8, source: u8, gsi: u32, flags: u16) {
    let mut body = Vec::new();
    body.push(bus);
    body.push(source);
    body.extend_from_slice(&gsi.to_le_bytes());
    body.extend_from_slice(&flags.to_le_bytes());
    push_entry(entries, 2, &body);
}

fn discover(entries: &[u8]) -> Result<X86ApicDeclarations, X86DiscoveryError> {
    from_entries(MadtEntryIter::new(entries, BASE_OFFSET))
}

#[def_test]
fn discovers_controllers_and_overrides_in_any_order() {
    let mut entries = Vec::new();
    // Overrides precede the controllers they resolve against.
    push_iso(&mut entries, 0, 2, 9, 0);
    push_io_apic(&mut entries, 0, 0xFEC0_0000, 0);
    push_iso(&mut entries, 0, 5, 10, 0b1111);
    push_io_apic(&mut entries, 1, 0xFEC0_1000, 24);

    let discovered = discover(&entries).expect("usable declarations");
    assert_eq!(
        discovered.io_apics(),
        &[
            IoApicDeclaration {
                id: 0,
                address: 0xFEC0_0000,
                gsi_base: 0,
            },
            IoApicDeclaration {
                id: 1,
                address: 0xFEC0_1000,
                gsi_base: 24,
            },
        ]
    );
    assert_eq!(
        discovered.overrides(),
        &[
            super::IrqOverride {
                isa_irq: 2,
                gsi: 9,
                trigger: IrqTrigger::EdgeRising,
            },
            super::IrqOverride {
                isa_irq: 5,
                gsi: 10,
                trigger: IrqTrigger::LevelLow,
            },
        ]
    );
    assert_eq!(discovered.source(), FwSource::Acpi);
    assert!(discovered.issues().is_empty());
    assert!(!discovered.issues_truncated());
}

#[def_test]
fn absent_when_madt_has_no_usable_controller() {
    assert_eq!(discover(&[]), Err(X86DiscoveryError::Absent));

    let mut entries = Vec::new();
    push_entry(&mut entries, 0, &[1, 2, 1, 0, 0, 0]);
    assert_eq!(discover(&entries), Err(X86DiscoveryError::Absent));
}

#[def_test]
fn malformed_entry_fails_the_walk_at_its_table_offset() {
    let mut entries = Vec::new();
    push_io_apic(&mut entries, 0, 0xFEC0_0000, 0);
    // IO-APIC entry header with only four of its twelve bytes present.
    entries.extend_from_slice(&[1, 12, 0, 0]);

    assert_eq!(
        discover(&entries),
        Err(X86DiscoveryError::Malformed(acpi::MadtParseError {
            offset: BASE_OFFSET + 12,
            kind: acpi::MadtParseErrorKind::TruncatedEntry,
        }))
    );
}

#[def_test]
fn zero_address_is_skipped_but_other_controllers_survive() {
    let mut entries = Vec::new();
    push_io_apic(&mut entries, 7, 0, 24);
    push_io_apic(&mut entries, 0, 0xFEC0_0000, 0);

    let discovered = discover(&entries).expect("one usable controller");
    assert_eq!(discovered.io_apics().len(), 1);
    assert_eq!(
        discovered.issues(),
        &[X86DiscoveryIssue::ZeroIoApicAddress { id: 7 }]
    );
}

#[def_test]
fn duplicate_address_and_gsi_base_keep_the_first_controller() {
    let mut entries = Vec::new();
    push_io_apic(&mut entries, 0, 0xFEC0_0000, 0);
    push_io_apic(&mut entries, 1, 0xFEC0_0000, 24);
    push_io_apic(&mut entries, 2, 0xFEC0_1000, 0);

    let discovered = discover(&entries).expect("one retained controller");
    assert_eq!(discovered.io_apics().len(), 1);
    assert_eq!(discovered.io_apics()[0].id, 0);
    assert_eq!(
        discovered.issues(),
        &[
            X86DiscoveryIssue::DuplicateIoApicAddress {
                id: 1,
                address: 0xFEC0_0000,
            },
            X86DiscoveryIssue::GsiBaseConflict {
                id: 2,
                gsi_base: 0,
            },
        ]
    );
}

#[def_test]
fn overrides_require_a_controller_that_owns_the_gsi() {
    let mut entries = Vec::new();
    push_io_apic(&mut entries, 0, 0xFEC0_0000, 24);
    push_iso(&mut entries, 0, 3, 20, 0);
    push_iso(&mut entries, 0, 4, 24, 0);

    let discovered = discover(&entries).expect("controller with a base");
    assert_eq!(discovered.overrides().len(), 1);
    assert_eq!(discovered.overrides()[0].isa_irq, 4);
    assert_eq!(
        discovered.issues(),
        &[X86DiscoveryIssue::UnknownGsi {
            isa_irq: 3,
            gsi: 20,
        }]
    );
}

#[def_test]
fn duplicate_override_for_the_same_isa_line_is_last_wins() {
    let mut entries = Vec::new();
    push_io_apic(&mut entries, 0, 0xFEC0_0000, 0);
    push_iso(&mut entries, 0, 4, 5, 0);
    push_iso(&mut entries, 0, 4, 6, 0b1111);

    let discovered = discover(&entries).expect("controller with overrides");
    assert_eq!(discovered.overrides().len(), 1);
    assert_eq!(discovered.overrides()[0].gsi, 6);
    assert_eq!(discovered.overrides()[0].trigger, IrqTrigger::LevelLow);
    assert_eq!(
        discovered.issues(),
        &[X86DiscoveryIssue::OverriddenIsaMapping {
            isa_irq: 4,
            previous_gsi: 5,
            gsi: 6,
        }]
    );
}

#[def_test]
fn repeated_identity_override_still_reports_last_wins() {
    let mut entries = Vec::new();
    push_io_apic(&mut entries, 0, 0xFEC0_0000, 0);
    // The second declaration invalidates ISA 9's target route, which removes
    // the previous override for ISA 9 itself. The duplicate diagnostic must be
    // captured before that removal.
    push_iso(&mut entries, 0, 9, 9, 0);
    push_iso(&mut entries, 0, 9, 9, 0);

    let discovered = discover(&entries).expect("repeated identity override");
    assert_eq!(discovered.overrides().len(), 1);
    assert_eq!(discovered.overrides()[0].isa_irq, 9);
    assert_eq!(discovered.overrides()[0].gsi, 9);
    assert_eq!(
        discovered.issues(),
        &[X86DiscoveryIssue::OverriddenIsaMapping {
            isa_irq: 9,
            previous_gsi: 9,
            gsi: 9,
        }]
    );

    let config = discovered
        .to_init_config(super::IoApicInitPolicy::platform_legacy())
        .expect("routed controller");
    let desc = config.legacy_irq_desc(9).expect("explicit ISA 9 route");
    assert_eq!(desc.hwirq, 9);
}

#[def_test]
fn invalid_sources_and_reserved_flags_are_skipped() {
    let mut entries = Vec::new();
    push_io_apic(&mut entries, 0, 0xFEC0_0000, 0);
    push_iso(&mut entries, 1, 0, 5, 0);
    push_iso(&mut entries, 0, 16, 5, 0);
    push_iso(&mut entries, 0, 2, 5, 0b10);
    push_iso(&mut entries, 0, 3, 5, 0b1000);
    push_iso(&mut entries, 0, 4, 5, 0);

    let discovered = discover(&entries).expect("one valid override");
    assert_eq!(discovered.overrides().len(), 1);
    assert_eq!(discovered.overrides()[0].isa_irq, 4);
    assert_eq!(
        discovered.issues(),
        &[
            X86DiscoveryIssue::InvalidIsaSource { bus: 1, source: 0 },
            X86DiscoveryIssue::InvalidIsaSource {
                bus: 0,
                source: 16,
            },
            X86DiscoveryIssue::ReservedIsoFlags {
                bus: 0,
                source: 2,
                flags: 0b10,
            },
            X86DiscoveryIssue::ReservedIsoFlags {
                bus: 0,
                source: 3,
                flags: 0b1000,
            },
        ]
    );
}

#[def_test]
fn iso_flags_decode_polarity_and_trigger() {
    let mut entries = Vec::new();
    let mut expected = Vec::new();
    push_io_apic(&mut entries, 0, 0xFEC0_0000, 0);
    for (source, flags, trigger) in [
        (0, 0b0000, IrqTrigger::EdgeRising),
        (1, 0b0011, IrqTrigger::EdgeFalling),
        (2, 0b1100, IrqTrigger::LevelHigh),
        (3, 0b1111, IrqTrigger::LevelLow),
    ] {
        push_iso(&mut entries, 0, source, u32::from(source) + 5, flags);
        expected.push(trigger);
    }

    let discovered = discover(&entries).expect("decodable overrides");
    let observed: Vec<_> = discovered
        .overrides()
        .iter()
        .map(|override_| override_.trigger)
        .collect();
    assert_eq!(observed, expected);
}

#[def_test]
fn faulty_level_timer_override_is_forced_to_edge() {
    let mut entries = Vec::new();
    push_io_apic(&mut entries, 0, 0xFEC0_0000, 0);
    // Linux `mp_override_legacy_irq`: IRQ0 level overrides come from faulty
    // firmware and would storm; other lines keep their decoded trigger.
    push_iso(&mut entries, 0, 0, 2, 0b1111);
    push_iso(&mut entries, 0, 1, 9, 0b1111);

    let discovered = discover(&entries).expect("timer override");
    let observed: Vec<_> = discovered
        .overrides()
        .iter()
        .map(|override_| override_.trigger)
        .collect();
    assert_eq!(observed, [IrqTrigger::EdgeFalling, IrqTrigger::LevelLow]);
}

#[def_test]
fn fixed_capacity_limits_controllers() {
    let mut entries = Vec::new();
    for index in 0..=MAX_IO_APICS {
        push_io_apic(
            &mut entries,
            index as u8,
            0xFEC0_0000 + (index as u32) * 0x1000,
            (index as u32) * 24,
        );
    }

    let controllers = discover(&entries).expect("bounded controller set");
    assert_eq!(controllers.io_apics().len(), MAX_IO_APICS);
    assert!(controllers.issues().iter().any(|issue| matches!(
        issue,
        X86DiscoveryIssue::TooManyIoApics { .. }
    )));
}

#[def_test]
fn skipped_overrides_do_not_consume_capacity() {
    let mut entries = Vec::new();
    push_io_apic(&mut entries, 0, 0xFEC0_0000, 24);
    // No controller owns GSI 5, so these are skipped only when overrides are
    // interpreted. They must not crowd out the later usable declaration.
    for source in 0..super::MAX_LEGACY_IRQS as u8 {
        push_iso(&mut entries, 0, source, 5, 0);
    }
    push_iso(&mut entries, 0, 4, 30, 0);

    let discovered = discover(&entries).expect("one valid override");
    assert_eq!(discovered.overrides().len(), 1);
    assert_eq!(discovered.overrides()[0].isa_irq, 4);
    assert_eq!(discovered.overrides()[0].gsi, 30);
}

#[def_test]
fn later_duplicate_still_wins_after_all_lines_are_filled() {
    let mut entries = Vec::new();
    push_io_apic(&mut entries, 0, 0xFEC0_0000, 0);
    for source in 0..super::MAX_LEGACY_IRQS as u8 {
        push_iso(&mut entries, 0, source, u32::from(source), 0);
    }
    // The seventeenth raw ISO updates an existing line; it must replace the
    // earlier mapping rather than be dropped as a capacity overflow.
    push_iso(&mut entries, 0, 4, 20, 0b1111);

    let discovered = discover(&entries).expect("bounded override set");
    assert_eq!(discovered.overrides().len(), super::MAX_LEGACY_IRQS);
    let irq4 = discovered
        .overrides()
        .iter()
        .find(|override_| override_.isa_irq == 4)
        .expect("ISA line 4 retained");
    assert_eq!(irq4.gsi, 20);
    assert_eq!(irq4.trigger, IrqTrigger::LevelLow);
    assert!(discovered.issues().iter().any(|issue| matches!(
        issue,
        X86DiscoveryIssue::OverriddenIsaMapping {
            isa_irq: 4,
            previous_gsi: 4,
            gsi: 20,
        }
    )));
}

#[def_test]
fn single_controller_converts_to_the_init_config() {
    let mut entries = Vec::new();
    push_io_apic(&mut entries, 3, 0xFEC0_0000, 0);

    let discovered = discover(&entries).expect("controller");
    let config = discovered
        .to_init_config(super::IoApicInitPolicy::platform_legacy())
        .expect("single controller is representable");
    assert_eq!(
        config,
        X86IrqInitConfig::from_acpi(
            IoApicDeclaration {
                id: 3,
                address: 0xFEC0_0000,
                gsi_base: 0,
            },
            super::IoApicInitPolicy::platform_legacy(),
        )
    );
    assert_eq!(config.source, FwSource::Acpi);
    assert_eq!(config.legacy_irq_desc(4).expect("identity route").hwirq, 4);
}

#[def_test]
fn conversion_accepts_one_controller_at_a_non_zero_base() {
    let mut entries = Vec::new();
    push_io_apic(&mut entries, 0, 0xFEC0_0000, 24);
    push_iso(&mut entries, 0, 4, 28, 0b1111);

    let discovered = discover(&entries).expect("one controller with a base");
    let config = discovered
        .to_init_config(super::IoApicInitPolicy::platform_legacy())
        .expect("one controller is representable at any base");

    assert_eq!(
        config.controllers().next().expect("one instance").declaration.gsi_base,
        24
    );
    let desc = config.legacy_irq_desc(4).expect("routed ISA 4");
    assert_eq!(desc.hwirq, 4, "GSI 28 is local pin 4");
    assert_eq!(desc.trigger, IrqTrigger::LevelLow);
}

#[def_test]
fn conversion_routes_each_controller_to_its_own_instance_domain() {
    let mut entries = Vec::new();
    push_io_apic(&mut entries, 0, 0xFEC0_0000, 0);
    push_io_apic(&mut entries, 1, 0xFEC0_1000, 24);
    // ISA 4 explicitly routed to GSI 28, which belongs to controller B.
    push_iso(&mut entries, 0, 4, 28, 0b1111);

    let discovered = discover(&entries).expect("two controllers");
    let config = discovered
        .to_init_config(super::IoApicInitPolicy::platform_legacy())
        .expect("multiple controllers are representable");
    assert_eq!(config.controller_count(), 2);

    let instances: Vec<_> = config.controllers().collect();
    assert_eq!(instances[0].domain, super::super::IO_APIC_DOMAIN);
    assert_ne!(instances[0].domain, instances[1].domain);

    // GSI 4 resolves to controller A pin 4; GSI 28 to controller B pin 4.
    let a = config.legacy_irq_desc(4).expect("routed ISA 4");
    assert_eq!(a.hwirq, 4);
    assert_eq!(a.domain, Some(instances[1].domain));
    assert_eq!(a.trigger, IrqTrigger::LevelLow);

    let capabilities = [
        x86_apic::IoApicCapabilities { pin_count: 24 },
        x86_apic::IoApicCapabilities { pin_count: 24 },
    ];
    assert_eq!(
        super::super::validate_ranges(&config, &capabilities),
        Ok(())
    );
    assert_eq!(
        super::super::validate_legacy_routes(&config, &capabilities),
        Ok(())
    );
}

#[def_test]
fn identity_route_prefers_the_controller_that_owns_the_gsi() {
    let mut entries = Vec::new();
    push_io_apic(&mut entries, 0, 0xFEC0_0000, 0);
    push_io_apic(&mut entries, 1, 0xFEC0_1000, 24);

    let discovered = discover(&entries).expect("two controllers");
    let config = discovered
        .to_init_config(super::IoApicInitPolicy::platform_legacy())
        .expect("multiple controllers are representable");
    let instances: Vec<_> = config.controllers().collect();

    // Identity ISA 4 and ISA 12 stay on controller A: B starts at GSI 24, so
    // its larger base must not capture a lower GSI. The descriptor path uses
    // only the declared base and the backend's 32-pin bound.
    assert_eq!(
        config.legacy_irq_desc(4).expect("ISA 4").domain,
        Some(instances[0].domain)
    );
    assert_eq!(
        config.legacy_irq_desc(12).expect("ISA 12").domain,
        Some(instances[0].domain)
    );
}

#[def_test]
fn non_identity_override_replaces_the_default_route() {
    let mut entries = Vec::new();
    push_io_apic(&mut entries, 0, 0xFEC0_0000, 0);
    push_iso(&mut entries, 0, 4, 9, 0b1111);

    let discovered = discover(&entries).expect("override");
    let config = discovered
        .to_init_config(super::IoApicInitPolicy::platform_legacy())
        .expect("routed controller");

    let desc = config.legacy_irq_desc(4).expect("routed ISA 4");
    assert_eq!(desc.hwirq, 9);
    assert_eq!(desc.trigger, IrqTrigger::LevelLow);
    // Linux invalidates the identity route of the override target so ISA 9
    // does not alias the same GSI through a different route.
    assert!(config.legacy_irq_desc(9).is_none());
}

#[def_test]
fn duplicate_override_last_wins_for_the_routed_descriptor() {
    let mut entries = Vec::new();
    push_io_apic(&mut entries, 0, 0xFEC0_0000, 0);
    push_iso(&mut entries, 0, 4, 5, 0);
    push_iso(&mut entries, 0, 4, 6, 0b1111);

    let discovered = discover(&entries).expect("duplicate override");
    let config = discovered
        .to_init_config(super::IoApicInitPolicy::platform_legacy())
        .expect("routed controller");

    let desc = config.legacy_irq_desc(4).expect("routed ISA 4");
    assert_eq!(desc.hwirq, 6);
    assert_eq!(desc.trigger, IrqTrigger::LevelLow);
    assert!(config.legacy_irq_desc(5).is_none());
    assert!(config.legacy_irq_desc(6).is_none());
}

#[def_test]
fn chained_overrides_invalidate_each_identity_target() {
    let mut entries = Vec::new();
    push_io_apic(&mut entries, 0, 0xFEC0_0000, 0);
    push_iso(&mut entries, 0, 4, 9, 0);
    push_iso(&mut entries, 0, 9, 10, 0);

    let discovered = discover(&entries).expect("chained overrides");
    let config = discovered
        .to_init_config(super::IoApicInitPolicy::platform_legacy())
        .expect("routed controller");

    assert_eq!(config.legacy_irq_desc(4).expect("ISA 4").hwirq, 9);
    assert_eq!(config.legacy_irq_desc(9).expect("ISA 9").hwirq, 10);
    assert!(config.legacy_irq_desc(10).is_none());
}

#[def_test]
fn later_override_clears_an_identity_route_restored_earlier() {
    let mut entries = Vec::new();
    push_io_apic(&mut entries, 0, 0xFEC0_0000, 0);
    // Linux checks the target's live mapping while processing each ISO:
    // 9 -> 9 leaves ISA 9 mapped, so the later 4 -> 9 clears it.
    push_iso(&mut entries, 0, 9, 9, 0);
    push_iso(&mut entries, 0, 4, 9, 0b1111);

    let discovered = discover(&entries).expect("ordered overrides");
    assert_eq!(discovered.overrides().len(), 1);
    assert_eq!(discovered.overrides()[0].isa_irq, 4);

    let config = discovered
        .to_init_config(super::IoApicInitPolicy::platform_legacy())
        .expect("routed controller");
    assert_eq!(config.legacy_irq_desc(4).expect("ISA 4").hwirq, 9);
    assert!(config.legacy_irq_desc(9).is_none());
}

#[def_test]
fn later_explicit_identity_override_restores_an_invalidated_line() {
    let mut entries = Vec::new();
    push_io_apic(&mut entries, 0, 0xFEC0_0000, 0);
    // Reverse order: 4 -> 9 invalidates ISA 9, then the explicit 9 -> 9 is a
    // real later declaration and revives it. Sharing GSI 9 is not rejected.
    push_iso(&mut entries, 0, 4, 9, 0b1111);
    push_iso(&mut entries, 0, 9, 9, 0);

    let discovered = discover(&entries).expect("ordered overrides");
    assert_eq!(discovered.overrides().len(), 2);

    let config = discovered
        .to_init_config(super::IoApicInitPolicy::platform_legacy())
        .expect("routed controller");
    assert_eq!(config.legacy_irq_desc(4).expect("ISA 4").hwirq, 9);
    let restored = config
        .legacy_irq_desc(9)
        .expect("ISA 9 explicitly restored");
    assert_eq!(restored.hwirq, 9);
    assert_eq!(restored.trigger, IrqTrigger::EdgeRising);
}

#[def_test]
fn routed_descriptor_programs_the_backend_pin_and_firmware_flags() {
    let mut entries = Vec::new();
    push_io_apic(&mut entries, 0, 0xFEC0_0000, 0);
    push_iso(&mut entries, 0, 4, 9, 0b1111);

    let discovered = discover(&entries).expect("non-identity override");
    let config = discovered
        .to_init_config(super::IoApicInitPolicy::platform_legacy())
        .expect("routed controller");
    assert_eq!(
        super::super::validate_legacy_routes(
            &config,
            &[x86_apic::IoApicCapabilities { pin_count: 24 }]
        ),
        Ok(())
    );

    let desc = config.legacy_irq_desc(4).expect("routed ISA 4");
    let observed = Cell::new(None);
    super::super::configure_with(
        desc,
        super::super::io_pin,
        |instance, pin, policy| {
            observed.set(Some((instance.as_usize(), pin.as_usize(), policy)));
            true
        },
    )
    .expect("backend accepts the routed pin");

    assert_eq!(
        observed.get(),
        Some((
            0,
            9,
            x86_apic::IoApicTriggerPolicy::Program {
                trigger: x86_apic::IoApicTriggerMode::Level,
                polarity: x86_apic::IoApicPolarity::Low,
            },
        ))
    );
}
