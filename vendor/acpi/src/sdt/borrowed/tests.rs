use super::*;
use std::{vec, vec::Vec};

fn fix_checksum(bytes: &mut [u8], checksum_offset: usize) {
    bytes[checksum_offset] = 0;
    bytes[checksum_offset] = 0u8.wrapping_sub(bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)));
}

fn table(signature: &[u8; 4], size: usize) -> Vec<u8> {
    let mut bytes = vec![0; size];
    bytes[..4].copy_from_slice(signature);
    bytes[4..8].copy_from_slice(&(size as u32).to_le_bytes());
    bytes[8] = 2;
    fix_checksum(&mut bytes, 9);
    bytes
}

#[test]
fn facs_address_obeys_revision_and_complete_field_extent() {
    for (size, revision, extended, expected) in [
        (40, 1, 0, Some(0x1000)),
        (140, 1, 0x2000, Some(0x1000)),
        (139, 2, 0, Some(0x1000)),
        (140, 2, 0, Some(0x1000)),
        (140, 2, 0x2000, Some(0x2000)),
    ] {
        let mut bytes = table(b"FACP", size);
        bytes[8] = revision;
        bytes[36..40].copy_from_slice(&0x1000u32.to_le_bytes());
        if size >= 140 {
            bytes[132..140].copy_from_slice(&(extended as u64).to_le_bytes());
        }
        fix_checksum(&mut bytes, 9);
        let fadt = fadt::Fadt::new(Table::parse(&bytes).unwrap()).unwrap();
        assert_eq!(fadt.firmware_control_address(), expected);
    }
    let bytes = table(b"FACP", 39);
    let fadt = fadt::Fadt::new(Table::parse(&bytes).unwrap()).unwrap();
    assert_eq!(fadt.firmware_control_address(), None);
}

fn rsdp(extended: bool) -> Vec<u8> {
    let mut bytes = vec![0; if extended { 36 } else { 20 }];
    bytes[..8].copy_from_slice(b"RSD PTR ");
    bytes[15] = if extended { 2 } else { 0 };
    bytes[16..20].copy_from_slice(&2u32.to_le_bytes());
    fix_checksum(&mut bytes[..20], 8);
    if extended {
        bytes[20..24].copy_from_slice(&36u32.to_le_bytes());
        bytes[24..32].copy_from_slice(&2u64.to_le_bytes());
        fix_checksum(&mut bytes, 32);
    }
    bytes
}

#[test]
fn root_walk_accepts_rsdt_and_xsdt_without_allocating_in_parser() {
    for extended in [false, true] {
        let rsdp = rsdp(extended);
        let mut root = table(if extended { b"XSDT" } else { b"RSDT" }, HEADER_SIZE + if extended { 8 } else { 4 });
        root[HEADER_SIZE] = 3;
        fix_checksum(&mut root, 9);
        let child = table(b"FACP", 90);
        let regions = [&rsdp[..], &root[..], &child[..]];
        let read = |address: u64, size: usize| {
            regions.get(address as usize - 1).and_then(|bytes| bytes.get(..size)).ok_or(Error::Truncated)
        };
        let (address, result) = find_table(1, *b"FACP", read).unwrap().unwrap();
        assert_eq!(address, 3);
        assert_eq!(result.header().signature, super::super::Signature(*b"FACP"));
        assert!(find_table(1, *b"APIC", read).unwrap().is_none());
    }
}

#[test]
fn rsdp_requires_basic_and_extended_checksums() {
    let mut bytes = rsdp(true);
    bytes[10] ^= 1;
    fix_checksum(&mut bytes, 32);
    assert_eq!(find_table(1, *b"APIC", |_, size| Ok(&bytes[..size])).err(), Some(Error::InvalidChecksum));
    let mut bytes = rsdp(true);
    bytes[32] ^= 1;
    assert_eq!(find_table(1, *b"APIC", |_, size| Ok(&bytes[..size])).err(), Some(Error::InvalidChecksum));
}

#[test]
fn rsdp_short_extended_length_is_rejected() {
    let mut bytes = rsdp(true);
    bytes[20..24].copy_from_slice(&20u32.to_le_bytes());
    assert_eq!(find_table(1, *b"APIC", |_, size| Ok(&bytes[..size])).err(), Some(Error::InvalidLength));
}

#[test]
fn missing_root_is_distinct_from_malformed_firmware() {
    let mut bytes = rsdp(false);
    bytes[16..20].fill(0);
    fix_checksum(&mut bytes, 8);
    assert_eq!(find_table(1, *b"APIC", |_, size| Ok(&bytes[..size])).err(), Some(Error::MissingRoot));
}

#[test]
fn unrelated_candidate_corruption_retains_strict_lookup_policy() {
    let rsdp = rsdp(false);
    let mut root = table(b"RSDT", HEADER_SIZE + 8);
    root[HEADER_SIZE] = 3;
    root[HEADER_SIZE + 4] = 4;
    fix_checksum(&mut root, 9);
    let mut unrelated = table(b"SSDT", HEADER_SIZE);
    unrelated[9] ^= 1;
    let target = table(b"FACP", 90);
    let regions = [&rsdp[..], &root[..], &unrelated[..], &target[..]];
    assert_eq!(
        find_table(1, *b"FACP", |address, size| Ok(&regions[address as usize - 1][..size])).err(),
        Some(Error::InvalidChecksum)
    );
}

#[test]
fn root_partial_entry_is_not_silently_ignored() {
    let rsdp = rsdp(true);
    let root = table(b"XSDT", HEADER_SIZE + 1);
    assert_eq!(
        find_table(1, *b"FACP", |address, size| {
            let bytes = if address == 1 { &rsdp } else { &root };
            Ok(&bytes[..size])
        })
        .err(),
        Some(Error::InvalidLength)
    );
}

#[test]
fn table_rejects_truncated_invalid_length_and_checksum() {
    assert_eq!(Table::parse(&[0; 35]).err(), Some(Error::Truncated));
    let mut bytes = table(b"APIC", 44);
    bytes[4..8].copy_from_slice(&35u32.to_le_bytes());
    assert_eq!(Table::parse(&bytes).err(), Some(Error::InvalidLength));
    bytes[4..8].copy_from_slice(&45u32.to_le_bytes());
    assert_eq!(Table::parse(&bytes).err(), Some(Error::Truncated));
    bytes[4..8].copy_from_slice(&44u32.to_le_bytes());
    bytes[43] ^= 1;
    assert_eq!(Table::parse(&bytes).err(), Some(Error::InvalidChecksum));
}

#[test]
fn fadt_extended_gas_address_is_at_offset_four() {
    let mut bytes = table(b"FACP", 184);
    bytes[172..176].copy_from_slice(&[1, 16, 0, 2]);
    bytes[176..184].copy_from_slice(&0x604u64.to_le_bytes());
    fix_checksum(&mut bytes, 9);
    let view = fadt::Fadt::new(Table::parse(&bytes).unwrap()).unwrap();
    let gas = view.extended_pm1a_control_address().unwrap();
    let address = gas.address;
    assert_eq!(address, 0x604);
}

#[test]
fn fadt_short_and_old_revisions_do_not_read_extended_fields() {
    let mut bytes = table(b"FACP", 90);
    bytes[64..68].copy_from_slice(&0x604u32.to_le_bytes());
    bytes[89] = 2;
    fix_checksum(&mut bytes, 9);
    let view = fadt::Fadt::new(Table::parse(&bytes).unwrap()).unwrap();
    assert!(view.extended_pm1a_control_address().is_none());
    assert_eq!(view.legacy_pm1a_control_address(), Some(0x604));
    assert_eq!(view.pm1_control_length(), Some(2));
    let mut bytes = table(b"FACP", 184);
    bytes[8] = 1;
    bytes[40..44].copy_from_slice(&0x1234u32.to_le_bytes());
    bytes[140..148].copy_from_slice(&0x5678u64.to_le_bytes());
    fix_checksum(&mut bytes, 9);
    let view = fadt::Fadt::new(Table::parse(&bytes).unwrap()).unwrap();
    assert!(view.extended_pm1a_control_address().is_none());
    assert_eq!(view.dsdt_address(), Some(0x1234));
}

#[test]
fn madt_malformed_entries_report_one_error_and_stop() {
    use madt::{MadtEntryIter, MadtParseErrorKind::*};
    for (bytes, kind) in [
        (&[1][..], TruncatedEntry),
        (&[1, 0][..], InvalidLength),
        (&[1, 12][..], TruncatedEntry),
        (&[1, 2][..], ShortEntry),
    ] {
        let mut entries = MadtEntryIter::new(bytes, 44);
        assert_eq!(entries.next(), Some(Err(madt::MadtParseError { offset: 44, kind })));
        assert!(entries.next().is_none());
    }
}

#[test]
fn madt_unknown_entries_are_skipped_and_values_preserved() {
    let entries = [99, 2, 0, 8, 3, 7, 1, 0, 0, 0];
    let mut iterator = madt::MadtEntryIter::new(&entries, 44);
    let madt::MadtEntry::LocalApic(entry) = iterator.next().unwrap().unwrap() else { panic!() };
    assert_eq!(entry.processor_uid, 3);
    assert_eq!(entry.apic_id, 7);
    assert!(entry.enabled());
    assert!(iterator.next().is_none());
}

#[test]
fn mcfg_preserves_all_segments_and_rejects_partial_record() {
    let mut bytes = table(b"MCFG", 44 + 32);
    bytes[44..52].copy_from_slice(&0x80000000u64.to_le_bytes());
    bytes[52..54].copy_from_slice(&1u16.to_le_bytes());
    bytes[54] = 2;
    bytes[55] = 3;
    fix_checksum(&mut bytes, 9);
    let entries: Vec<_> = mcfg::allocations(Table::parse(&bytes).unwrap()).unwrap().collect();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].pci_segment, 1);
    assert_eq!(entries[0].ecam_region(), Some((0x80200000, 0x200000)));
    let short = table(b"MCFG", 45);
    assert_eq!(mcfg::allocations(Table::parse(&short).unwrap()).err(), Some(Error::InvalidLength));
}

#[test]
fn resource_descriptors_are_bounds_checked() {
    let mut bytes = [0u8; 46];
    bytes[0] = 0x8a;
    bytes[1] = 43;
    bytes[14..22].copy_from_slice(&0x80000000u64.to_le_bytes());
    bytes[38..46].copy_from_slice(&0x10000000u64.to_le_bytes());
    let (resource, size) = resource::address_descriptor(&bytes).unwrap().unwrap();
    assert_eq!(size, 46);
    assert_eq!(resource.minimum, 0x80000000);
    assert_eq!(resource.length, 0x10000000);
    assert_eq!(resource::address_descriptor(&bytes[..45]).err(), Some(Error::Truncated));
}
