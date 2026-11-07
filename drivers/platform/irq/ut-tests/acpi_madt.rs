// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec::Vec;

use unittest::{assert, assert_eq, def_test};

use acpi::{
    InterruptSourceOverride, IoApicEntry, LocalApicEntry, MadtEntry, MadtEntryIter,
    MadtParseError, MadtParseErrorKind,
};

const BASE_OFFSET: usize = 44;

#[def_test]
fn parses_known_entries_in_order() {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&[0, 8, 7, 3, 1, 0, 0, 0]);
    bytes.extend_from_slice(&[1, 12, 2, 0, 0x00, 0x00, 0xC0, 0xFE, 24, 0, 0, 0]);
    bytes.extend_from_slice(&[2, 10, 0, 9, 20, 0, 0, 0, 0x0D, 0]);

    let entries = MadtEntryIter::new(&bytes, BASE_OFFSET)
        .collect::<Result<Vec<_>, _>>()
        .expect("well-formed entry stream");
    assert_eq!(entries.len(), 3);

    match entries[0] {
        MadtEntry::LocalApic(LocalApicEntry {
            processor_uid,
            apic_id,
            flags,
        }) => {
            assert_eq!(processor_uid, 7);
            assert_eq!(apic_id, 3);
            assert_eq!(flags, 1);
        }
        _ => panic!("expected local APIC entry"),
    }

    match entries[1] {
        MadtEntry::IoApic(IoApicEntry {
            id,
            address,
            global_system_interrupt_base,
        }) => {
            assert_eq!(id, 2);
            assert_eq!(address, 0xFEC0_0000);
            assert_eq!(global_system_interrupt_base, 24);
        }
        _ => panic!("expected I/O APIC entry"),
    }

    match entries[2] {
        MadtEntry::InterruptSourceOverride(InterruptSourceOverride {
            bus,
            source,
            gsi,
            flags,
        }) => {
            assert_eq!(bus, 0);
            assert_eq!(source, 9);
            assert_eq!(gsi, 20);
            assert_eq!(flags, 0x0D);
        }
        _ => panic!("expected interrupt source override"),
    }
}

#[def_test]
fn unknown_entry_types_are_skipped_by_length() {
    let bytes = [
        9, 6, 0xAA, 0xBB, 0xCC, 0xDD, // unknown type, variable length
        0, 8, 4, 2, 1, 0, 0, 0, // local APIC
    ];

    let entries = MadtEntryIter::new(&bytes, 0)
        .collect::<Result<Vec<_>, _>>()
        .expect("unknown entries are framed, not errors");
    assert_eq!(entries.len(), 1);
    assert!(matches!(entries[0], MadtEntry::LocalApic(_)));
}

#[def_test]
fn truncated_header_reports_table_offset_and_stops() {
    let bytes = [0, 8, 1, 2, 1, 0, 0, 0, 0];
    let mut entries = MadtEntryIter::new(&bytes, BASE_OFFSET);

    assert!(matches!(entries.next(), Some(Ok(MadtEntry::LocalApic(_)))));
    assert_eq!(
        entries.next(),
        Some(Err(MadtParseError {
            offset: BASE_OFFSET + 8,
            kind: MadtParseErrorKind::TruncatedEntry,
        }))
    );
    assert_eq!(entries.next(), None);
}

#[def_test]
fn zero_or_single_byte_length_is_invalid() {
    for bytes in [[0u8, 0], [1u8, 1]] {
        assert_eq!(
            MadtEntryIter::new(&bytes, 0).next(),
            Some(Err(MadtParseError {
                offset: 0,
                kind: MadtParseErrorKind::InvalidLength,
            }))
        );
    }
}

#[def_test]
fn declared_length_past_entry_region_is_truncated() {
    let bytes = [1, 12, 0, 0, 0x00, 0xC0];
    assert_eq!(
        MadtEntryIter::new(&bytes, 0).next(),
        Some(Err(MadtParseError {
            offset: 0,
            kind: MadtParseErrorKind::TruncatedEntry,
        }))
    );
}

#[def_test]
fn known_entry_shorter_than_its_structure_is_rejected() {
    for bytes in [
        &[0u8, 4, 0, 0][..],             // local APIC needs 8 bytes
        &[1u8, 8, 0, 0, 0, 0, 0, 0][..], // I/O APIC needs 12 bytes
        &[2u8, 6, 0, 0, 0, 0][..],       // ISO needs 10 bytes
    ] {
        assert_eq!(
            MadtEntryIter::new(bytes, 0).next(),
            Some(Err(MadtParseError {
                offset: 0,
                kind: MadtParseErrorKind::ShortEntry,
            }))
        );
    }
}

#[def_test]
fn short_known_entry_stops_iteration_before_a_later_valid_entry() {
    let bytes = [
        1, 8, 0, 0, 0, 0, 0, 0, // I/O APIC declared as 8 bytes, needs 12
        1, 12, 2, 0, 0x00, 0x00, 0xC0, 0xFE, 0, 0, 0, 0, // valid I/O APIC
    ];
    let mut entries = MadtEntryIter::new(&bytes, BASE_OFFSET);

    assert_eq!(
        entries.next(),
        Some(Err(MadtParseError {
            offset: BASE_OFFSET,
            kind: MadtParseErrorKind::ShortEntry,
        }))
    );
    assert_eq!(entries.next(), None);
}
