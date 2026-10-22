// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! AArch64 guest round-trip regression for host current-task restoration.

use alloc::{boxed::Box, sync::Arc};
use core::sync::atomic::{AtomicUsize, Ordering};

use super::{CurrentArch, Vm, VmConfig, load_guest, spawn_vcpu_thread};
use crate::{arch::VmmArch, vcpu::Vcpu, vdev::VcpuHook};

struct HostCurrentObserver {
    task: Option<ktask::KtaskRef>,
    exits: Arc<AtomicUsize>,
}

impl VcpuHook<CurrentArch> for HostCurrentObserver {
    fn on_entry(&mut self, _vcpu: &mut Vcpu<CurrentArch>) {
        self.task = Some(ktask::current().clone());
    }

    fn on_exit(&mut self, _vcpu_id: u32) {
        // This runs on the vCPU task before IRQs are restored, so a later
        // scheduler switch cannot hide a broken guest-exit restoration.
        if let Some(task) = &self.task {
            assert_eq!(khal::context::current_task_ptr(), ktask::KtaskRef::as_ptr(task).cast());
            self.exits.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[unittest::def_test(serial)]
fn guest_roundtrip_preserves_host_current() {
    assert!(CurrentArch::percpu_hw_init());
    let vm = Vm::<CurrentArch>::new(VmConfig::new(0, 0, 1)).expect("test VM");
    let mut vcpu = vm.create_vcpu(0);
    let (entry, sp, pages) = load_guest::<CurrentArch>();
    assert!(CurrentArch::init_vcpu(&mut vcpu, entry, sp));
    pages.bind_to_vcpu(&mut vcpu);
    let exits = Arc::new(AtomicUsize::new(0));
    vcpu.hooks.push(Box::new(HostCurrentObserver {
        task: None,
        exits: exits.clone(),
    }));

    let exit_code = spawn_vcpu_thread::<CurrentArch>(vcpu).join();
    vm.shutdown();
    assert_eq!(exit_code, 0);
    assert!(exits.load(Ordering::Relaxed) > 1);
}
