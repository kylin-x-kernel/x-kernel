// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Register storage model; no claim of GIC hardware virtualization coverage.

use alloc::sync::Arc;

use super::*;
use crate::vm::{Vm, VmConfig};

struct NoWake;

impl VcpuWaker for NoWake {
    fn wake_vcpu(&self, _: u32) {}
}

#[unittest::def_test]
fn shared_hooks_serialize_saved_image_queries_and_stop() {
    let waker: Arc<dyn VcpuWaker> = Arc::new(NoWake);
    // These operations inspect software state only; no GICH access is allowed.
    let vgic = Vgic::new(1, 0, Arc::downgrade(&waker));
    vgic.cores[0].hw.lock().lr[0] = LR_STATE_PENDING | 1;
    let other = vgic.clone();
    let task = ktask::spawn_task(ktask::TaskInner::new_pidless_kthread(
        move || {
            let mut hook = VgicHook::new(other);
            for _ in 0..64 {
                hook.on_stop(0);
                ktask::yield_now();
            }
        },
        "vgic-image-stop".into(),
        kbuild_config::TASK_STACK_SIZE,
    ));
    let hook = VgicHook::new(vgic);
    for _ in 0..64 {
        let _ = hook.has_pending_interrupt(0);
        ktask::yield_now();
    }
    assert_eq!(task.join(), 0);
    assert!(!hook.has_pending_interrupt(0));
}

#[unittest::def_test(serial)]
fn entry_failure_detaches_hardware_without_consuming_saved_pending() {
    let mut registers = alloc::vec![0u32; 1024];
    let waker: Arc<dyn VcpuWaker> = Arc::new(NoWake);
    let vgic = Vgic::new(1, registers.as_mut_ptr() as usize, Arc::downgrade(&waker));
    let vm = Vm::<Aarch64Vhe>::new(VmConfig::new(0, 0, 1)).expect("test VM");
    let mut vcpu = vm.create_vcpu(0);
    let mut hook = VgicHook::new(vgic.clone());
    vgic.set_pending(0, 1);
    let guard = kspin::IrqSave::new();
    hook.on_entry(&mut vcpu);
    let prepared_lr = vgic.gich_read(GICH_LR0);
    core::assert_ne!(prepared_lr & LR_STATE_PENDING, 0);
    core::assert_eq!(vgic.gich_read(GICH_HCR), HCR_EN);

    // Invalid entry-time hardware must not replace the saved software image.
    vgic.gich_write(GICH_LR0, 0);
    vgic.gich_write(GICH_VMCR, u32::MAX);
    vgic.gich_write(GICH_APR, u32::MAX);
    hook.on_entry_failure(0);
    core::assert_eq!(vgic.gich_read(GICH_HCR), 0);
    core::assert_eq!(vgic.gich_read(GICH_VMCR), 0);
    core::assert_eq!(vgic.gich_read(GICH_APR), 0);
    for i in 0..MAX_LRS {
        core::assert_eq!(vgic.gich_read(GICH_LR0 + i * 4), 0);
    }
    hook.on_entry(&mut vcpu);
    core::assert_eq!(vgic.gich_read(GICH_LR0), prepared_lr);
    core::assert_eq!(vgic.gich_read(GICH_VMCR), 0);
    core::assert_eq!(vgic.gich_read(GICH_APR), 0);
    hook.on_entry_failure(0);
    // Hardware-backed active state must never be restored as pending+active.
    {
        vgic.cores[0].hw.lock().lr[0] =
            LR_HW | LR_STATE_PENDING | LR_STATE_ACTIVE | (27 << LR_PHYSID_SHIFT) | 27;
    }
    hook.on_entry(&mut vcpu);
    core::assert_eq!(vgic.gich_read(GICH_LR0) & LR_STATE_MASK, LR_STATE_ACTIVE);
    hook.on_entry_failure(0);
    hook.on_stop(0);
    hook.on_entry(&mut vcpu);
    for i in 0..MAX_LRS {
        core::assert_eq!(vgic.gich_read(GICH_LR0 + i * 4), 0);
    }
    hook.on_entry_failure(0);
    drop(guard);
}
