// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use unittest::{assert, assert_eq, def_test};

use super::{IrqDesc, IrqDescError, IrqLine, IrqSpec, IrqState, IrqStateDesc};

#[def_test]
fn force_enable_rejects_absence_without_allocating_or_applying_hardware() {
    let virq = 0x5fa8;
    let desc = IrqDesc::from_hwirq(0x5fa9).with_domain(crate::domain::TEST_DOMAIN);
    for spec in [
        IrqSpec::PlainVirq(virq),
        IrqDesc::from_virq(virq).into(),
        desc.into(),
        desc.with_virq(virq).into(),
    ] {
        for is_masked in [false, true] {
            let registry = kspin::SpinNoIrq::new(IrqState::new());
            let next = registry.lock().next_virq_for_tests();
            let irq_before = karch::local_irq_enabled();
            let irq_guard = is_masked.then(kspin::IrqSave::new);
            let expected_irq = karch::local_irq_enabled();
            let mut has_applied = false;
            let result = super::enable_line(&registry, spec, true, |_| {
                has_applied = true;
                Ok(())
            });
            assert_eq!(result, Err(IrqDescError::UnknownIrq));
            assert!(!has_applied);
            assert_eq!(karch::local_irq_enabled(), expected_irq);
            drop(irq_guard);
            assert_eq!(karch::local_irq_enabled(), irq_before);
            let state = registry.lock();
            assert!(state.descs.is_empty());
            assert_eq!(state.next_virq_for_tests(), next);
            assert_eq!(
                state.translated_hwirq(crate::domain::TEST_DOMAIN, desc.hwirq),
                None
            );
        }
    }
}

#[def_test]
fn force_enable_does_not_reconstruct_a_line_from_a_reverse_mapping() {
    // Model an incomplete legacy registry privately. Activation must report
    // absence, not construct a new identity behind the old reverse mapping.
    let desc = IrqDesc::from_hwirq(0x5faa).with_domain(crate::domain::TEST_DOMAIN);
    let original = Arc::new(IrqLine::new(IrqStateDesc::new(desc)));
    let registry = kspin::SpinNoIrq::new(IrqState::new());
    let virq = registry
        .lock()
        .try_resolve_desc(desc, Some(&original))
        .unwrap()
        .into_desc()
        .logical_irq()
        .unwrap();
    let removed = registry.lock().descs.remove(&virq).unwrap();
    assert!(Arc::ptr_eq(&removed, &original));
    let mut has_applied = false;
    assert_eq!(
        super::enable_line(&registry, desc.into(), true, |_| {
            has_applied = true;
            Ok(())
        }),
        Err(IrqDescError::UnknownIrq)
    );
    assert!(!has_applied);
    assert!(registry.lock().descs.is_empty());
    assert_eq!(
        registry
            .lock()
            .translated_hwirq(crate::domain::TEST_DOMAIN, desc.hwirq),
        Some(virq)
    );
    assert!(!original.lock().is_enabled_for_tests());
}
