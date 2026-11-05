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
