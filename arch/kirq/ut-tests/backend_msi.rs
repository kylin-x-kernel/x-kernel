// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::MsiMessage;

#[def_test]
fn test_msi_message_accessors() {
    let message = MsiMessage::new(0xfee0_0000, 0x80);
    assert_eq!(message.address(), 0xfee0_0000);
    assert_eq!(message.data(), 0x80);
}

#[def_test]
fn msi_preparation_failure_is_retryable_and_reuses_table() {
    use kalloc_types::KBox;

    use crate::{
        IrqDescError,
        domain::{MsiTable, Published, prepare_msi_with},
    };
    let table = Published::new();
    let result = prepare_msi_with(
        &table,
        32..36,
        33,
        0x5100,
        |_| Err(IrqDescError::NoMemory),
        |_| Err(IrqDescError::NoMemory),
    );
    assert!(matches!(result, Err(IrqDescError::NoMemory)));
    assert!(table.get().is_none());
    let result = prepare_msi_with(
        &table,
        32..36,
        33,
        0x5100,
        |virq| Ok(KBox::new_nowait(virq).unwrap()),
        |_| Err(IrqDescError::NoMemory),
    );
    assert!(matches!(result, Err(IrqDescError::NoMemory)));
    assert!(table.get().is_none());
    let prepared = prepare_msi_with(
        &table,
        32..36,
        33,
        0x5100,
        |virq| Ok(KBox::new_nowait(virq).unwrap()),
        |range| Ok(KBox::new_nowait(MsiTable::new(range).unwrap()).unwrap()),
    )
    .unwrap();
    let slot = table.get().unwrap().slot(33).unwrap();
    assert!(slot.get().is_none());
    prepared.publish();
    assert_eq!(slot.get(), Some(&0x5100));
    assert!(slot.clear_with(|| true));
    let prepared = prepare_msi_with(
        &table,
        32..36,
        33,
        0x5101,
        |virq| Ok(KBox::new_nowait(virq).unwrap()),
        |_| Err(IrqDescError::NoMemory),
    )
    .unwrap();
    prepared.publish();
    assert_eq!(slot.get(), Some(&0x5101));
}

#[def_test]
fn stable_msi_slots_preserve_old_readers_and_restore_rejected_free() {
    use kalloc_types::KBox;
    let table = crate::domain::MsiTable::new(32..36).unwrap();
    assert!(table.slot(31).is_none());
    assert!(table.slot(36).is_none());
    let slot = table.slot(33).unwrap();
    slot.publish(KBox::new_nowait(0x5000).unwrap());
    let old = slot.get().unwrap();
    assert!(!slot.clear_with(|| {
        assert!(slot.get().is_none());
        false
    }));
    assert_eq!(slot.get(), Some(&0x5000));
    assert!(slot.clear_with(|| {
        assert!(slot.get().is_none());
        true
    }));
    assert!(slot.get().is_none());
    slot.publish(KBox::new_nowait(0x5001).unwrap());
    assert_eq!(*old, 0x5000);
    assert_eq!(slot.get(), Some(&0x5001));
}

#[def_test(serial)]
fn rejected_or_discarded_snapshots_do_not_publish() {
    use crate::{
        IrqDescError,
        domain::{MsiTable, TEST_DOMAIN, domain, prepare_snapshot},
    };
    let domain = domain(TEST_DOMAIN).unwrap();
    let count = domain.publish_count();
    drop(prepare_snapshot(TEST_DOMAIN, &[(9, 0x5000)]).unwrap());
    assert_eq!(domain.publish_count(), count);
    assert!(matches!(
        prepare_snapshot(crate::IO_APIC_DOMAIN, &[(0x1000, 1)]),
        Err(IrqDescError::InvalidHwirq { .. })
    ));
    assert!(matches!(
        MsiTable::new(5..5),
        Err(IrqDescError::InvalidHwirq { .. })
    ));
}

#[cfg(target_arch = "x86_64")]
struct UnprogrammedMsi(crate::Virq);

#[cfg(target_arch = "x86_64")]
impl Drop for UnprogrammedMsi {
    fn drop(&mut self) {
        // The test never programs a device, so no producer can use this vector.
        drop(crate::free_irq(self.0));
        super::free_msix(self.0);
    }
}

#[cfg(target_arch = "x86_64")]
#[def_test(serial)]
fn allocated_msi_retains_reservation_until_action_removal_and_explicit_free() {
    use alloc::sync::Arc;

    use unittest::{assert, assert_eq};

    let allocation = super::alloc_msix(crate::IrqAffinity::Any).unwrap();
    let virq = allocation.virq();
    let cleanup = UnprogrammedMsi(virq);
    let desc = crate::descriptor(virq).unwrap();
    assert_eq!(desc.domain, Some(crate::MSI_DOMAIN));
    assert_eq!(
        crate::translate_hwirq(crate::MSI_DOMAIN, desc.hwirq),
        Some(virq)
    );
    let handler: crate::platform::Handler = Arc::new(|_| crate::IrqEvent::HANDLED);
    assert_eq!(
        crate::try_register_with_options(
            virq,
            handler.clone(),
            crate::IrqRegisterOptions::disabled(),
        ),
        Ok(true)
    );
    assert!(!super::free_msix(virq));
    assert_eq!(crate::descriptor(virq), Some(desc));
    assert!(crate::free_irq(virq).is_some());
    assert_eq!(Arc::strong_count(&handler), 1);
    assert!(crate::descriptor(virq).is_some());
    assert!(super::free_msix(virq));
    assert!(crate::descriptor(virq).is_none());
    assert_eq!(crate::translate_hwirq(crate::MSI_DOMAIN, desc.hwirq), None);
    assert!(!super::free_msix(virq));
    drop(cleanup);
}
