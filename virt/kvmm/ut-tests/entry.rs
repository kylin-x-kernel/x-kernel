// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Exercise the real run loop with architecture entry rejected before execution.

use alloc::{boxed::Box, sync::Arc, vec::Vec};

use super::{ExitAction, Vcpu, vmm_run_vcpu};
use crate::{
    arch::VmmArch,
    vdev::VcpuHook,
    vm::{Vm, VmConfig},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Event {
    Prepared(u8),
    Aborted(u8),
    Exited(u8),
    Stopped(u8),
}

struct RejectEntry;

#[derive(Default)]
struct GuestState {
    was_saved: bool,
    was_handled: bool,
    was_torn_down: bool,
}

impl VmmArch for RejectEntry {
    type ArchVcpu = GuestState;
    type GuestMem = <crate::arch::CurrentArch as VmmArch>::GuestMem;

    fn init_vcpu(_: &mut Vcpu<Self>, _: u64, _: u64) -> bool {
        true
    }

    fn restore_guest_ctx(_: &mut Vcpu<Self>) {}

    fn enter_guest(_: &mut Vcpu<Self>) -> bool {
        false
    }

    fn exit_handler(vcpu: &mut Vcpu<Self>) -> ExitAction {
        vcpu.arch.was_handled = true;
        ExitAction::Exit
    }

    fn save_guest_ctx(vcpu: &mut Vcpu<Self>) {
        vcpu.arch.was_saved = true;
    }

    fn guest_test_code() -> (*const u8, usize) {
        (core::ptr::null(), 0)
    }

    fn teardown_vcpu(vcpu: &mut Vcpu<Self>) {
        core::assert!(!karch::local_irq_enabled());
        vcpu.arch.was_torn_down = true;
    }
}

struct DeviceProbe {
    id: u8,
    events: Arc<kspin::SpinNoIrq<Vec<Event>>>,
}

impl VcpuHook<RejectEntry> for DeviceProbe {
    fn on_stop(&mut self, _: u32) {
        core::assert!(!karch::local_irq_enabled());
        self.events.lock().push(Event::Stopped(self.id));
    }

    fn on_entry(&mut self, _: &mut Vcpu<RejectEntry>) {
        core::assert!(!karch::local_irq_enabled());
        self.events.lock().push(Event::Prepared(self.id));
    }

    fn on_exit(&mut self, _: u32) {
        self.events.lock().push(Event::Exited(self.id));
    }

    fn on_entry_failure(&mut self, _: u32) {
        core::assert!(!karch::local_irq_enabled());
        self.events.lock().push(Event::Aborted(self.id));
    }
}

#[unittest::def_test(serial)]
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: VCPU hook installation still consumes standard Box owners; its lifecycle migration is deferred"
)]
fn rejected_entry_unwinds_devices_without_guest_readback() {
    let vm = Vm::<RejectEntry>::new(VmConfig::new(0, 0, 1)).expect("test VM");
    let mut vcpu = vm.create_vcpu(0);
    // Preallocate: hooks must not allocate with IRQs masked.
    let events = Arc::new(kspin::SpinNoIrq::new(Vec::with_capacity(6)));
    for id in 0..2 {
        vcpu.hooks.push(Box::new(DeviceProbe {
            id,
            events: events.clone(),
        }));
    }
    let were_irqs_enabled = karch::local_irq_enabled();
    unittest::assert!(vmm_run_vcpu(&mut vcpu).is_err());
    unittest::assert_eq!(karch::local_irq_enabled(), were_irqs_enabled);
    let recorded = events.lock();
    unittest::assert_eq!(
        recorded.as_slice(),
        &[
            Event::Prepared(0),
            Event::Prepared(1),
            Event::Aborted(1),
            Event::Aborted(0),
            Event::Stopped(0),
            Event::Stopped(1),
        ]
    );
    unittest::assert!(!vcpu.arch.was_saved);
    unittest::assert!(!vcpu.arch.was_handled);
    unittest::assert!(vcpu.arch.was_torn_down);
}
