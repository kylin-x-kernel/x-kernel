// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use super::{VmmArch, X86Vmx};
use crate::vm::{Vm, VmConfig};

#[unittest::def_test]
fn irq_enabled_entry_is_rejected_before_vmx_access() {
    let vm = Vm::<X86Vmx>::new(VmConfig::new(0, 0, 1)).expect("test VM");
    let mut vcpu = vm.create_vcpu(0);
    unittest::assert!(karch::local_irq_enabled());
    unittest::assert!(!X86Vmx::enter_guest(&mut vcpu));
    unittest::assert!(karch::local_irq_enabled());
}
