// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{format, rc::Rc, vec, vec::Vec};
use core::cell::Cell;

use acpi_rs::aml::{AmlError, object::Object};
use unittest::def_test;

use super::*;
use crate::acpi_test_support::{
    END_TAG, MockHost, add_device, buffer, bus_range, bus_range_no_end, extended_irq,
    extended_irq_with_source, insert, insert_native, install_root, integer, package, prt_entry,
    prt_object, small_irq, string,
    test_context, test_tables,
};
use crate::context::AcpiContext;
use crate::host::HostAccessKind;

const ROOT: &str = "\\_SB.PCI0";

fn root_with_route(entries: Vec<acpi_rs::aml::object::WrappedObject>) -> AcpiContext<MockHost> {
    let mut context = test_context();
    install_root(&mut context, ROOT, string("PNP0A03"));
    context.init_irq_model(IrqModel::IoApic).unwrap();
    insert(&mut context, "\\_SB.PCI0._PRT", prt_object(entries));
    context
}

fn ready_context() -> AcpiContext<MockHost> {
    let mut context = test_context();
    install_root(&mut context, ROOT, string("PNP0A03"));
    context.init_irq_model(IrqModel::IoApic).unwrap();
    context
}

fn install_link(context: &mut AcpiContext<MockHost>, path: &str, crs: Object) {
    add_device(context, path);
    insert(context, &format!("{path}._CRS"), crs);
}

fn route(gsi: u32, trigger: PrtTrigger) -> Result<Option<PrtRoute>, PrtLookupError> {
    Ok(Some(PrtRoute { gsi, trigger }))
}

// -- link `_CRS` support scope ----------------------------------------------
//
// Descriptor framing, field widths and descriptor decoding are covered by the
// vendor AML resource tests. These tests cover what this subsystem accepts out
// of that decoded data.

#[def_test]
fn decodes_ovmf_extended_irq() {
    let crs = [
        0x89, 0x06, 0x00, 0x09, 0x01, 0x10, 0x00, 0x00, 0x00, 0x79, 0x00,
    ];
    let irq = decode_link_irq(&crs).unwrap();
    assert_eq!(irq.gsi, 16);
    assert_eq!(irq.trigger, PrtTrigger::LevelHigh);
    assert!(irq.shared);
    assert!(!irq.wake);
    assert!(irq.consumer);
}

#[def_test]
fn decodes_level_low_and_edge_triggers() {
    let level_low = decode_link_irq(&extended_irq(20, 0x0d)).unwrap();
    assert_eq!(level_low.trigger, PrtTrigger::LevelLow);

    let rising = decode_link_irq(&extended_irq(21, 0x02)).unwrap();
    assert_eq!(rising.trigger, PrtTrigger::EdgeRising);

    let falling = decode_link_irq(&extended_irq(22, 0x06)).unwrap();
    assert_eq!(falling.trigger, PrtTrigger::EdgeFalling);
}

#[def_test]
fn decodes_small_irq_descriptors() {
    // The mask is a bitmask; bit 10 names IRQ 10, it is not the value 0x0400.
    let rising = decode_link_irq(&small_irq(10, None)).unwrap();
    assert_eq!(rising.gsi, 10);
    assert_eq!(rising.trigger, PrtTrigger::EdgeRising);

    let level_low = decode_link_irq(&small_irq(10, Some(0x08))).unwrap();
    assert_eq!(level_low.trigger, PrtTrigger::LevelLow);

    let edge_low = decode_link_irq(&small_irq(10, Some(0x09))).unwrap();
    assert_eq!(edge_low.trigger, PrtTrigger::EdgeFalling);
}

#[def_test]
fn accepts_gsi_zero() {
    // A zero extended vector is a valid GSI, not a missing interrupt.
    assert_eq!(decode_link_irq(&extended_irq(0, 0x0d)).unwrap().gsi, 0);
}

#[def_test]
fn rejects_bad_irq_resources() {
    assert_eq!(
        decode_link_irq(&[0x22, 0x00, 0x00, 0x79, 0x00]),
        Err(CrsError::Malformed)
    );
    assert_eq!(
        decode_link_irq(&[0x22, 0x06, 0x00, 0x79, 0x00]),
        Err(CrsError::MultipleIrqs)
    );
    assert_eq!(
        decode_link_irq(&[0x22, 0x04, 0x00, 0x22, 0x08, 0x00, 0x79, 0x00]),
        Err(CrsError::MultipleIrqs)
    );
    assert_eq!(decode_link_irq(&[0x22, 0x00]), Err(CrsError::Malformed));
    assert_eq!(
        decode_link_irq(&[0x22, 0x00, 0x04]),
        Err(CrsError::MissingEndTag)
    );
    assert_eq!(decode_link_irq(&[0x79, 0x00]), Err(CrsError::NoIrq));
}

#[def_test]
fn rejects_bad_extended_irq() {
    assert_eq!(
        decode_link_irq(&[0x89, 0x06, 0x00, 0x0d, 0x00, 0x14, 0x00, 0x00, 0x00, 0x79, 0x00]),
        Err(CrsError::Malformed)
    );
    assert_eq!(
        decode_link_irq(&[
            0x89, 0x0a, 0x00, 0x0d, 0x02, 0x14, 0x00, 0x00, 0x00, 0x15, 0x00, 0x00, 0x00, 0x79,
            0x00,
        ]),
        Err(CrsError::MultipleIrqs)
    );
    let short = [0x89, 0x04, 0x00, 0x0d, 0x01, 0x79, 0x00];
    assert_eq!(decode_link_irq(&short), Err(CrsError::Malformed));
}

#[def_test]
fn extended_irq_resource_source_is_unsupported_not_malformed() {
    let crs = extended_irq_with_source(20, 0x0d, 0, b"LNK0");
    assert_eq!(decode_link_irq(&crs), Err(CrsError::UnsupportedSource));
}

#[def_test]
fn truncated_end_tag_is_malformed() {
    // `0x79` without its checksum byte is a truncated EndTag, not a
    // terminator.
    assert_eq!(
        decode_link_irq(&[0x22, 0x02, 0x00, 0x79]),
        Err(CrsError::Malformed)
    );
}

#[def_test]
fn skips_unrelated_descriptors() {
    let mut crs = bus_range_no_end(4, 0, 0xff);
    crs.extend_from_slice(&small_irq(10, None));
    let irq = decode_link_irq(&crs).unwrap();
    assert_eq!(irq.gsi, 10);
}

#[def_test]
fn bounds_resource_descriptors() {
    let mut bytes = Vec::new();
    for _ in 0..=MAX_CRS_DESCRIPTORS {
        bytes.extend_from_slice(&[0x80, 0x00, 0x00]);
    }
    bytes.extend_from_slice(&END_TAG);
    assert_eq!(
        decode_link_irq(&bytes),
        Err(CrsError::TooManyDescriptors)
    );

    let truncated = [0x88, 0x20, 0x00, 0x02];
    assert_eq!(decode_link_irq(&truncated), Err(CrsError::Malformed));
}

#[def_test]
fn decodes_bus_ranges() {
    // Word, DWord and QWord address-space descriptors use distinct tags and
    // widths; the vendor layer must select the field width from the tag.
    assert_eq!(
        decode_bus_range(&bus_range(2, 0, 0xff)),
        Ok(Some((0, 0xff)))
    );
    assert_eq!(
        decode_bus_range(&bus_range(4, 0, 0x100)),
        Ok(Some((0, 0x100)))
    );
    assert_eq!(
        decode_bus_range(&bus_range(8, 0, 0xffff)),
        Ok(Some((0, 0xffff)))
    );
    assert_eq!(decode_bus_range(&END_TAG), Ok(None));
}

#[def_test]
fn rejects_bad_bus_ranges() {
    let mut multiple = bus_range_no_end(4, 0, 0xff);
    multiple.extend_from_slice(&bus_range_no_end(4, 0, 0x0f));
    multiple.extend_from_slice(&END_TAG);
    assert_eq!(
        decode_bus_range(&multiple),
        Err(CrsError::MultipleBusRanges)
    );

    assert_eq!(
        decode_bus_range(&bus_range(4, 5, 1)),
        Err(CrsError::BadBusRange)
    );
    assert_eq!(
        decode_bus_range(&bus_range(8, 0, 0x1_0000_0000)),
        Err(CrsError::BadBusRange)
    );
    let truncated = [0x88, 0x04, 0x00, 0x02, 0x00, 0x00, 0x79, 0x00];
    assert_eq!(decode_bus_range(&truncated), Err(CrsError::Malformed));
}

// -- orchestration -----------------------------------------------------------

#[def_test]
fn requires_irq_model() {
    let mut context = test_context();
    install_root(&mut context, ROOT, string("PNP0A03"));
    assert!(matches!(
        load(&mut context, 0),
        Err(PrtError::IrqModelNotInitialized)
    ));
}

#[def_test]
fn rejects_nonzero_bus() {
    let mut context = ready_context();
    assert!(matches!(
        load(&mut context, 1),
        Err(PrtError::UnsupportedBus)
    ));
}

#[def_test]
fn requires_one_root_bridge() {
    let mut context = test_context();
    context.init_irq_model(IrqModel::IoApic).unwrap();
    assert!(matches!(load(&mut context, 0), Err(PrtError::NoRootBridge)));

    install_root(&mut context, ROOT, string("PNP0A03"));
    install_root(&mut context, "\\_SB.PCI1", string("PNP0A03"));
    assert!(matches!(
        load(&mut context, 0),
        Err(PrtError::MultipleRootBridges)
    ));
}

#[def_test]
fn accepts_root_bridge_identity_forms() {
    let mut context = test_context();
    install_root(&mut context, ROOT, integer(0x030a_d041));
    context.init_irq_model(IrqModel::IoApic).unwrap();
    let table = load(&mut context, 0).unwrap();
    assert_eq!(table.origin(), PrtOrigin::Absent);

    let mut context = test_context();
    add_device(&mut context, ROOT);
    insert(&mut context, "\\_SB.PCI0._CID", string("PNP0A08"));
    context.init_irq_model(IrqModel::IoApic).unwrap();
    let table = load(&mut context, 0).unwrap();
    assert_eq!(table.origin(), PrtOrigin::Absent);

    let mut context = test_context();
    add_device(&mut context, ROOT);
    insert(
        &mut context,
        "\\_SB.PCI0._CID",
        package(vec![string("PNP0A08").wrap()]),
    );
    context.init_irq_model(IrqModel::IoApic).unwrap();
    let table = load(&mut context, 0).unwrap();
    assert_eq!(table.origin(), PrtOrigin::Absent);

    let mut context = test_context();
    install_root(&mut context, ROOT, string("PNP0A05"));
    context.init_irq_model(IrqModel::IoApic).unwrap();
    assert!(matches!(load(&mut context, 0), Err(PrtError::NoRootBridge)));
}

#[def_test]
fn validates_segment() {
    let mut context = ready_context();
    insert(&mut context, "\\_SB.PCI0._SEG", integer(0));
    assert!(matches!(
        load(&mut context, 0),
        Ok(table) if table.origin() == PrtOrigin::Absent
    ));

    let mut context = ready_context();
    insert(&mut context, "\\_SB.PCI0._SEG", integer(1));
    assert!(matches!(
        load(&mut context, 0),
        Err(PrtError::UnsupportedRootBridge)
    ));

    let mut context = ready_context();
    insert(&mut context, "\\_SB.PCI0._SEG", string("0"));
    assert!(matches!(
        load(&mut context, 0),
        Err(PrtError::UnsupportedRootBridge)
    ));
}

#[def_test]
fn validates_base_bus_number() {
    let mut context = ready_context();
    insert(&mut context, "\\_SB.PCI0._BBN", integer(0));
    assert!(load(&mut context, 0).is_ok());

    let mut context = ready_context();
    insert(&mut context, "\\_SB.PCI0._BBN", integer(1));
    assert!(matches!(
        load(&mut context, 0),
        Err(PrtError::UnsupportedRootBridge)
    ));

    let mut context = ready_context();
    insert(&mut context, "\\_SB.PCI0._BBN", string("zero"));
    assert!(matches!(
        load(&mut context, 0),
        Err(PrtError::UnsupportedRootBridge)
    ));
}

#[def_test]
fn validates_bus_range() {
    let mut context = ready_context();
    insert(
        &mut context,
        "\\_SB.PCI0._CRS",
        buffer(bus_range(4, 0, 0xff)),
    );
    insert(&mut context, "\\_SB.PCI0._BBN", integer(1));
    assert!(load(&mut context, 0).is_ok());

    let mut context = ready_context();
    insert(
        &mut context,
        "\\_SB.PCI0._CRS",
        buffer(bus_range(4, 1, 0xff)),
    );
    assert!(matches!(
        load(&mut context, 0),
        Err(PrtError::UnsupportedRootBridge)
    ));

    let mut context = ready_context();
    insert(
        &mut context,
        "\\_SB.PCI0._CRS",
        buffer(bus_range(4, 0, 0xff)),
    );
    assert!(load(&mut context, 0).is_ok());
}

#[def_test]
fn rejects_malformed_root_crs() {
    let mut context = ready_context();
    insert(&mut context, "\\_SB.PCI0._CRS", integer(1));
    assert!(matches!(
        load(&mut context, 0),
        Err(PrtError::MalformedRootCrs)
    ));

    let mut context = ready_context();
    insert(
        &mut context,
        "\\_SB.PCI0._CRS",
        buffer(vec![0x88, 0x04, 0x00, 0x02, 0x00, 0x00, 0x79, 0x00]),
    );
    assert!(matches!(
        load(&mut context, 0),
        Err(PrtError::MalformedRootCrs)
    ));
}

#[def_test]
fn accepts_root_crs_without_bus_range() {
    let mut context = ready_context();
    insert(&mut context, "\\_SB.PCI0._CRS", buffer(END_TAG.to_vec()));
    let table = load(&mut context, 0).unwrap();
    assert_eq!(table.origin(), PrtOrigin::Absent);
}

#[def_test]
fn absent_prt_is_empty() {
    let mut context = ready_context();
    let table = load(&mut context, 0).unwrap();
    assert_eq!(table.origin(), PrtOrigin::Absent);
    assert!(table.is_empty());
    assert_eq!(table.len(), 0);
    assert_eq!(table.lookup(0, 2, 0, 1), Ok(None));
}

#[def_test]
fn empty_prt_is_present_and_empty() {
    let mut context = ready_context();
    insert(&mut context, "\\_SB.PCI0._PRT", prt_object(Vec::new()));
    let table = load(&mut context, 0).unwrap();
    assert_eq!(table.origin(), PrtOrigin::Present);
    assert!(table.is_empty());
}

#[def_test]
fn rejects_malformed_prt() {
    let mut context = ready_context();
    insert(&mut context, "\\_SB.PCI0._PRT", integer(1));
    assert!(matches!(
        load(&mut context, 0),
        Err(PrtError::MalformedPrt)
    ));

    let mut context = ready_context();
    let entry = Object::Package(vec![integer(0).wrap()]).wrap();
    insert(
        &mut context,
        "\\_SB.PCI0._PRT",
        prt_object(vec![entry]),
    );
    assert!(matches!(
        load(&mut context, 0),
        Err(PrtError::MalformedPrt)
    ));
}

#[def_test]
fn rejects_oversized_prt_orchestration() {
    let mut context = ready_context();
    let entries = (0..=MAX_PRT_ENTRIES)
        .map(|index| prt_entry(index as u8, 0xffff, 0, integer(0), 1))
        .collect();
    insert(
        &mut context,
        "\\_SB.PCI0._PRT",
        prt_object(entries),
    );
    assert!(matches!(
        load(&mut context, 0),
        Err(PrtError::TableTooLarge)
    ));
}

#[def_test]
fn resolves_direct_route() {
    let mut context = root_with_route(vec![prt_entry(2, 0xffff, 0, integer(0), 16)]);
    let table = load(&mut context, 0).unwrap();
    assert_eq!(table.origin(), PrtOrigin::Present);
    assert_eq!(table.len(), 1);
    assert_eq!(table.lookup(0, 2, 0, 1), route(16, PrtTrigger::LevelLow));
    assert_eq!(table.lookup(0, 2, 7, 1), route(16, PrtTrigger::LevelLow));
    assert_eq!(table.lookup(0, 3, 0, 1), Ok(None));
}

#[def_test]
fn resolves_link_route() {
    let mut context = ready_context();
    install_link(
        &mut context,
        "\\_SB.LNK0",
        buffer(small_irq(10, Some(0x08))),
    );
    insert(
        &mut context,
        "\\_SB.PCI0._PRT",
        prt_object(vec![prt_entry(2, 0xffff, 1, string("LNK0"), 0)]),
    );
    let table = load(&mut context, 0).unwrap();
    assert_eq!(table.lookup(0, 2, 0, 2), route(10, PrtTrigger::LevelLow));
}

#[def_test]
fn stores_keyed_source_errors() {
    let mut context = root_with_route(vec![
        prt_entry(1, 0xffff, 0, integer(5), 0),
        prt_entry(2, 0xffff, 0, string("LNK0"), 1),
        prt_entry(3, 0xffff, 0, integer(0), u64::from(u32::MAX) + 1),
    ]);
    let table = load(&mut context, 0).unwrap();
    assert_eq!(
        table.lookup(0, 1, 0, 1),
        Err(PrtLookupError::MalformedEntry)
    );
    assert_eq!(
        table.lookup(0, 2, 0, 1),
        Err(PrtLookupError::UnsupportedLinkIndex)
    );
    assert_eq!(
        table.lookup(0, 3, 0, 1),
        Err(PrtLookupError::MalformedEntry)
    );
}

#[def_test]
fn stores_unresolved_and_unusable_links() {
    let mut context = ready_context();
    add_device(&mut context, "\\_SB.LNK1");
    insert(
        &mut context,
        "\\_SB.PCI0._PRT",
        prt_object(vec![
            prt_entry(1, 0xffff, 0, string("MISS"), 0),
            prt_entry(2, 0xffff, 0, string("LNK1"), 0),
        ]),
    );
    let table = load(&mut context, 0).unwrap();
    assert_eq!(table.lookup(0, 1, 0, 1), Err(PrtLookupError::UnusableCrs));
    assert_eq!(table.lookup(0, 2, 0, 1), Err(PrtLookupError::UnusableCrs));
}

#[def_test]
fn rejects_wrong_type_and_malformed_link_crs() {
    let mut context = ready_context();
    install_link(&mut context, "\\_SB.LNK0", integer(10));
    install_link(
        &mut context,
        "\\_SB.LNK1",
        buffer(vec![0x22, 0x00, 0x04]),
    );
    install_link(
        &mut context,
        "\\_SB.LNK2",
        buffer(extended_irq_with_source(20, 0x0d, 0, b"LNK0")),
    );
    insert(
        &mut context,
        "\\_SB.PCI0._PRT",
        prt_object(vec![
            prt_entry(1, 0xffff, 0, string("LNK0"), 0),
            prt_entry(2, 0xffff, 0, string("LNK1"), 0),
            prt_entry(3, 0xffff, 0, string("LNK2"), 0),
        ]),
    );
    let table = load(&mut context, 0).unwrap();
    assert_eq!(table.lookup(0, 1, 0, 1), Err(PrtLookupError::UnusableCrs));
    assert_eq!(table.lookup(0, 2, 0, 1), Err(PrtLookupError::UnusableCrs));
    assert_eq!(table.lookup(0, 3, 0, 1), Err(PrtLookupError::UnusableCrs));
}

#[def_test]
fn evaluates_shared_link_once() {
    let mut context = ready_context();
    add_device(&mut context, "\\_SB.LNK0");
    let calls = Rc::new(Cell::new(0u32));
    let counter = calls.clone();
    insert_native(&mut context, "\\_SB.LNK0._CRS", 0, move |_| {
        let call = counter.get() + 1;
        counter.set(call);
        let gsi = if call == 1 { 20 } else { 21 };
        Ok(Object::Buffer(extended_irq(gsi, 0x0d)).wrap())
    });
    insert(
        &mut context,
        "\\_SB.PCI0._PRT",
        prt_object(vec![
            prt_entry(1, 0xffff, 0, string("LNK0"), 0),
            prt_entry(2, 0xffff, 0, string("LNK0"), 0),
        ]),
    );
    let table = load(&mut context, 0).unwrap();
    assert_eq!(calls.get(), 1);
    assert_eq!(table.lookup(0, 1, 0, 1), route(20, PrtTrigger::LevelLow));
    assert_eq!(table.lookup(0, 2, 0, 1), route(20, PrtTrigger::LevelLow));
}

#[def_test]
fn keeps_first_duplicate_entry() {
    let mut context = root_with_route(vec![
        prt_entry(2, 0xffff, 0, integer(0), 16),
        prt_entry(2, 0xffff, 0, integer(0), 17),
    ]);
    let table = load(&mut context, 0).unwrap();
    assert_eq!(table.len(), 1);
    assert_eq!(table.lookup(0, 2, 0, 1), route(16, PrtTrigger::LevelLow));
}

#[def_test]
fn table_outlives_context() {
    let table = {
        let mut context = root_with_route(vec![prt_entry(2, 0xffff, 0, integer(0), 16)]);
        load(&mut context, 0).unwrap()
    };
    assert_eq!(table.lookup(0, 2, 0, 1), route(16, PrtTrigger::LevelLow));
}

#[def_test]
fn lookup_rejects_bad_keys() {
    let mut context = root_with_route(vec![prt_entry(2, 0xffff, 0, integer(0), 16)]);
    let table = load(&mut context, 0).unwrap();
    assert_eq!(
        table.lookup(1, 2, 0, 1),
        Err(PrtLookupError::Unsupported)
    );
    assert_eq!(table.lookup(0, 2, 0, 0), Ok(None));
    assert_eq!(table.lookup(0, 2, 0, 0xff), Ok(None));
    assert_eq!(
        table.lookup(0, 2, 0, 5),
        Err(PrtLookupError::InvalidPin)
    );
}

// -- host failures -----------------------------------------------------------

#[def_test]
fn aborts_on_host_failure_during_root_probe() {
    let tables = test_tables(0);
    let host = tables.host.clone();
    let mut context = tables.context();
    add_device(&mut context, ROOT);
    insert_native(&mut context, "\\_SB.PCI0._HID", 0, move |_| {
        host.record_failure(HostAccessKind::SystemMemory);
        Ok(string("PNP0A03").wrap())
    });
    context.init_irq_model(IrqModel::IoApic).unwrap();
    assert!(matches!(load(&mut context, 0), Err(PrtError::HostAccess(_))));
}

#[def_test]
fn aborts_on_host_failure_during_prt() {
    let tables = test_tables(0);
    let host = tables.host.clone();
    let mut context = tables.context();
    install_root(&mut context, ROOT, string("PNP0A03"));
    insert_native(&mut context, "\\_SB.PCI0._PRT", 0, move |_| {
        host.record_failure(HostAccessKind::SystemMemory);
        Ok(prt_object(Vec::new()).wrap())
    });
    context.init_irq_model(IrqModel::IoApic).unwrap();
    assert!(matches!(load(&mut context, 0), Err(PrtError::HostAccess(_))));
}

#[def_test]
fn aborts_on_host_failure_during_link_crs() {
    let tables = test_tables(0);
    let host = tables.host.clone();
    let mut context = tables.context();
    install_root(&mut context, ROOT, string("PNP0A03"));
    add_device(&mut context, "\\_SB.LNK0");
    insert_native(&mut context, "\\_SB.LNK0._CRS", 0, move |_| {
        host.record_failure(HostAccessKind::SystemMemory);
        Ok(Object::Buffer(extended_irq(20, 0x0d)).wrap())
    });
    insert(
        &mut context,
        "\\_SB.PCI0._PRT",
        prt_object(vec![prt_entry(1, 0xffff, 0, string("LNK0"), 0)]),
    );
    context.init_irq_model(IrqModel::IoApic).unwrap();
    assert!(matches!(load(&mut context, 0), Err(PrtError::HostAccess(_))));
}

#[def_test]
fn nonfatal_probe_errors_do_not_poison() {
    let mut context = test_context();
    add_device(&mut context, ROOT);
    insert_native(&mut context, "\\_SB.PCI0._HID", 0, |_| {
        Err(AmlError::LibUnimplemented)
    });
    context.init_irq_model(IrqModel::IoApic).unwrap();
    assert!(matches!(load(&mut context, 0), Err(PrtError::NoRootBridge)));
    assert!(context.check_host().is_ok());
}
