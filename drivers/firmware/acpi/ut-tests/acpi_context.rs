// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{vec, vec::Vec};
use core::str::FromStr;

use acpi_rs::aml::{AmlError, namespace::AmlName, object::Object, op_region::RegionSpace};
use unittest::def_test;

use super::*;
use crate::acpi_test_support::{
    DSDT_OFFSET, MockHost, RSDT_OFFSET, add_many_devices, fixture_context, insert_native,
    test_context, test_tables, test_tables_xsdt,
};
use crate::host::{HostAccessKind, HostFailureReason};

fn name(path: &str) -> AmlName {
    AmlName::from_str(path).unwrap()
}

#[def_test]
fn s3_requires_a_present_package_with_bounded_sleep_types() {
    use crate::acpi_test_support::insert;

    let mut absent = test_context();
    assert_eq!(absent.s3_sleep_types().unwrap(), None);
    for object in [
        Object::Integer(1),
        Object::Package(vec![Object::Integer(1).wrap()]),
        Object::Package(vec![Object::Integer(8).wrap(), Object::Integer(1).wrap()]),
        Object::Package(vec![
            Object::Integer(1).wrap(),
            Object::Buffer(vec![]).wrap(),
        ]),
    ] {
        let mut context = test_context();
        insert(&mut context, "\\_S3", object);
        assert_eq!(context.s3_sleep_types().unwrap(), None);
    }
    let mut context = test_context();
    insert(
        &mut context,
        "\\_S3",
        Object::Package(vec![
            Object::Integer(1).wrap(),
            Object::Integer(2).wrap(),
            Object::Integer(0).wrap(),
            Object::Integer(0).wrap(),
        ]),
    );
    assert_eq!(context.s3_sleep_types().unwrap(), Some([1, 2]));
}

#[def_test]
fn loads_dsdt_and_ssdts() {
    let tables = test_tables(1);
    let mut context = tables.context();
    assert_eq!(context.irq_model(), None);
    context.init_irq_model(IrqModel::IoApic).unwrap();
    assert_eq!(context.irq_model(), Some(IrqModel::IoApic));
}

#[def_test]
fn loads_dsdt_and_ssdts_from_xsdt() {
    // Revision-2 RSDP and a real XSDT layout: the first 8-byte entry starts at
    // XSDT + 36, which is not 8-byte aligned.
    let tables = test_tables_xsdt(1);
    let mut context = tables.context();
    context.init_irq_model(IrqModel::IoApic).unwrap();
}

#[def_test]
fn rejects_aml_table_with_bad_checksum() {
    let tables = test_tables(0);
    // Corrupt the AML body without changing the declared length; the vendor
    // strict parser must reject the table before the interpreter sees it.
    tables.host.write(DSDT_OFFSET + 36, &[0xff]);
    let result = fixture_context(tables.host.clone(), tables.rsdp);
    assert!(matches!(
        result,
        Err(ContextError::InvalidAmlTable(
            acpi_rs::sdt::borrowed::Error::InvalidChecksum
        ))
    ));
}

#[def_test]
fn rejects_aml_table_with_wrong_signature() {
    let tables = test_tables(0);
    tables.set_dsdt_signature(b"SSDT");
    let result = fixture_context(tables.host.clone(), tables.rsdp);
    assert!(matches!(
        result,
        Err(ContextError::InvalidAmlTable(
            acpi_rs::sdt::borrowed::Error::InvalidSignature
        ))
    ));
}

#[def_test]
fn rejects_zero_rsdp() {
    let result = fixture_context(MockHost::small(), 0);
    assert!(matches!(result, Err(ContextError::MissingRsdp)));
}

#[def_test]
fn reports_rsdp_map_failure() {
    let tables = test_tables(0);
    tables.host.fail_map(tables.rsdp, 36);
    let result = fixture_context(tables.host.clone(), tables.rsdp);
    let Err(ContextError::HostAccess(failure)) = result else {
        panic!("expected host failure");
    };
    assert_eq!(failure.access, HostAccessKind::TableMap);
}

#[def_test]
fn reports_fadt_map_failure() {
    let tables = test_tables(0);
    tables.host.fail_map(tables.fadt, 36);
    let result = fixture_context(tables.host.clone(), tables.rsdp);
    assert!(matches!(result, Err(ContextError::HostAccess(_))));
}

#[def_test]
fn reports_dsdt_map_failure() {
    let tables = test_tables(0);
    tables.host.fail_map(tables.dsdt, 36);
    let result = fixture_context(tables.host.clone(), tables.rsdp);
    assert!(matches!(result, Err(ContextError::HostAccess(_))));
}

#[def_test]
fn rejects_short_dsdt() {
    let tables = test_tables(0);
    tables.set_dsdt_length(20);
    let result = fixture_context(tables.host.clone(), tables.rsdp);
    assert!(matches!(result, Err(ContextError::TableTooShort)));
}

#[def_test]
fn rejects_oversized_dsdt() {
    let tables = test_tables(0);
    tables.set_dsdt_length((MAX_AML_TABLE_LEN + 1) as u32);
    let result = fixture_context(tables.host.clone(), tables.rsdp);
    assert!(matches!(result, Err(ContextError::TableTooLarge)));
}

#[def_test]
fn rejects_short_ssdt() {
    let tables = test_tables(1);
    tables.set_ssdt_length(0, 20);
    let result = fixture_context(tables.host.clone(), tables.rsdp);
    assert!(matches!(result, Err(ContextError::TableTooShort)));
}

#[def_test]
fn reports_ssdt_body_map_failure() {
    let tables = test_tables(1);
    let offset = tables.ssdt_offset(0);
    tables.host.fail_map(tables.host.addr(offset), 0x100);
    let result = fixture_context(tables.host.clone(), tables.rsdp);
    assert!(matches!(result, Err(ContextError::HostAccess(_))));
}

#[def_test]
fn tolerates_absent_pic() {
    let mut context = test_context();
    context.init_irq_model(IrqModel::IoApic).unwrap();
    assert_eq!(context.irq_model(), Some(IrqModel::IoApic));
}

#[def_test]
fn evaluates_present_pic_once() {
    let mut context = test_context();
    insert_native(&mut context, "\\_PIC", 1, |args| {
        assert_eq!(args.len(), 1);
        assert_eq!(args[0].as_integer().unwrap(), 1);
        Ok(Object::Integer(0).wrap())
    });
    context.init_irq_model(IrqModel::IoApic).unwrap();
    assert_eq!(context.irq_model(), Some(IrqModel::IoApic));
}

#[def_test]
fn rejects_second_init() {
    let mut context = test_context();
    context.init_irq_model(IrqModel::IoApic).unwrap();
    assert!(matches!(
        context.init_irq_model(IrqModel::IoApic),
        Err(ContextError::AlreadyInitialized)
    ));
}

#[def_test]
fn poisons_after_failing_pic() {
    let mut context = test_context();
    insert_native(&mut context, "\\_PIC", 1, |_| {
        Err(AmlError::LibUnimplemented)
    });
    assert!(matches!(
        context.init_irq_model(IrqModel::IoApic),
        Err(ContextError::Aml(AmlError::LibUnimplemented))
    ));
    assert_eq!(context.irq_model(), None);
    assert!(matches!(
        context.init_irq_model(IrqModel::IoApic),
        Err(ContextError::Poisoned)
    ));
}

#[def_test]
fn host_failure_wins_over_pic_result() {
    let mut context = test_context();
    let host = context.host.clone();
    insert_native(&mut context, "\\_PIC", 1, move |_| {
        host.record_failure(HostAccessKind::SystemIo);
        Ok(Object::Integer(0).wrap())
    });
    let Err(ContextError::HostAccess(failure)) = context.init_irq_model(IrqModel::IoApic) else {
        panic!("expected host failure");
    };
    assert_eq!(failure.access, HostAccessKind::SystemIo);
    assert_eq!(failure.reason, HostFailureReason::Unsupported);
    assert!(matches!(
        context.init_irq_model(IrqModel::IoApic),
        Err(ContextError::HostAccess(_))
    ));
}

#[def_test]
fn host_failure_wins_over_decode() {
    let mut context = test_context();
    let host = context.host.clone();
    insert_native(&mut context, "\\_TST", 0, move |_| {
        host.record_failure(HostAccessKind::SystemMemory);
        Ok(Object::Integer(7).wrap())
    });
    let result = context.evaluate(&name("\\_TST"), Vec::new(), |_| ());
    assert!(matches!(result, Err(ContextError::HostAccess(_))));
    assert!(matches!(
        context.check_host(),
        Err(ContextError::HostAccess(_))
    ));
}

#[def_test]
fn evaluate_if_present_distinguishes_absence() {
    let mut context = test_context();
    let result = context.evaluate_if_present(&name("\\_TST"), Vec::new(), |object| {
        object.as_integer().unwrap()
    });
    assert!(matches!(result, Ok(None)));
}

#[def_test]
fn records_unsupported_region_when_host_does_not() {
    let mut context = test_context();
    context.host.set_record_unsupported(false);
    let error = context.aml_error(
        AmlError::NoHandlerForRegionAccess(RegionSpace::SmBus),
        false,
    );
    assert!(matches!(error, ContextError::UnsupportedRegion(4)));
    assert_eq!(context.host.unsupported_regions(), vec![4]);
    assert!(matches!(
        context.check_host(),
        Err(ContextError::UnsupportedRegion(4))
    ));
}

#[def_test]
fn host_unsupported_region_failure_takes_precedence() {
    let mut context = test_context();
    let error = context.aml_error(
        AmlError::NoHandlerForRegionAccess(RegionSpace::EmbeddedControl),
        false,
    );
    let ContextError::HostAccess(failure) = error else {
        panic!("expected host failure");
    };
    assert_eq!(failure.access, HostAccessKind::UnsupportedRegion);
    assert_eq!(failure.address, 3);
    assert_eq!(context.host.unsupported_regions(), vec![3]);
}

#[def_test]
fn requires_absolute_scopes() {
    let mut context = test_context();
    crate::acpi_test_support::add_device(&mut context, "\\_SB.LNK0");
    let relative = name("_SB_");
    let absolute = name("\\_SB");

    assert!(matches!(
        context.search_for_level(&name("LNK0"), &relative),
        Err(ContextError::NotAbsolute)
    ));
    assert!(matches!(
        context.resolve_name(&relative, &name("\\_SB.PCI0")),
        Err(ContextError::NotAbsolute)
    ));
    assert_eq!(
        context.search_for_level(&name("LNK0"), &absolute).unwrap(),
        name("\\_SB.LNK0")
    );
    assert_eq!(
        context.resolve_name(&absolute, &name("PCI0")).unwrap(),
        name("\\_SB.PCI0")
    );
}

#[def_test]
fn caps_device_paths() {
    let mut context = test_context();
    add_many_devices(&mut context, MAX_DEVICE_PATHS + 1);
    assert!(matches!(
        context.device_paths(),
        Err(ContextError::NamespaceTooLarge)
    ));
}

#[def_test]
fn collects_device_paths() {
    let mut context = test_context();
    crate::acpi_test_support::add_device(&mut context, "\\_SB.PCI0");
    let paths = context.device_paths().unwrap();
    assert!(paths.iter().any(|path| *path == name("\\_SB.PCI0")));
}

#[def_test]
fn checksum_failure_is_tolerated_for_recognized_rsdp() {
    let tables = test_tables(0);
    tables.host.write(8, &[0xff]);
    let result = fixture_context(tables.host.clone(), tables.rsdp);
    assert!(result.is_ok());
}

#[def_test]
fn rsdt_offsets_are_stable_fixtures() {
    // Guards the fixture layout the other tests rely on.
    assert_eq!(RSDT_OFFSET, 0x100);
    assert_eq!(DSDT_OFFSET, 0x800);
}
