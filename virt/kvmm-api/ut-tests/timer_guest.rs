// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Real GICv2/CNTV smoke test. The external QEMU timeout bounds delivery failure;
//! no elapsed-time threshold decides correctness. The guest checks its IAR and
//! compare value before acknowledging each of eight independently armed ticks.

use alloc::{boxed::Box, sync::Arc};
use core::sync::atomic::{AtomicUsize, Ordering};

use kvmm::{
    arch::VmmArch,
    mm::GuestMem,
    vcpu::spawn_vcpu_thread,
    vm::{Vm, VmConfig},
};

struct MigrationCheckpoint {
    visits: Arc<AtomicUsize>,
    checkpoints: Arc<AtomicUsize>,
    withdrawals: Arc<AtomicUsize>,
    ready: Arc<AtomicUsize>,
    guest: usize,
}

struct StopProbe(Arc<AtomicUsize>);

impl kvmm::vdev::VcpuHookFactory<super::CurrentArch> for StopProbe {
    #[expect(
        clippy::allow_attributes,
        reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
    )]
    #[allow(
        unknown_lints,
        std_box_in_kernel_tests,
        reason = "CG-049-no-std-box-in-kernel-tests: VcpuHookFactory requires this standard Box return type and ownership transfer"
    )]
    fn make_vcpu_hook(&self, _: u32) -> Option<Box<dyn kvmm::vdev::VcpuHook<super::CurrentArch>>> {
        Some(Box::new(Self(self.0.clone())))
    }
}

impl kvmm::vdev::VcpuHook<super::CurrentArch> for StopProbe {
    fn on_entry(&mut self, _: &mut kvmm::vcpu::Vcpu<super::CurrentArch>) {}

    fn on_exit(&mut self, _: u32) {}

    fn on_entry_failure(&mut self, _: u32) {}

    fn on_stop(&mut self, _: u32) {
        let control: u64;
        // SAFETY: the stop hook runs on the owning CPU at EL2 with IRQs masked.
        unsafe {
            core::arch::asm!("mrs {control}, S3_5_C14_C3_1", control = out(reg) control, options(nostack))
        };
        core::assert_eq!(control & 1, 0);
        core::assert_eq!(
            irq_driver::gic::local_ppi_state_for_test(27),
            Some((false, false, false))
        );
        self.0.fetch_add(1, Ordering::Release);
    }
}

impl kvmm::vdev::MmioDevice for MigrationCheckpoint {
    fn name(&self) -> &str {
        "timer-migration-checkpoint"
    }

    fn mmio_range(&self) -> (u64, u64) {
        (0x1000_0000, 0x1000)
    }

    fn read(&self, _: u64, _: u8) -> u64 {
        0
    }

    fn write(&mut self, offset: u64, _: u8, _: u64) {
        use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};
        if offset == 24 {
            // No migration is requested here: a switch must not hide a stale
            // physical active bit left after lowering an unacknowledged level.
            let (_, active, forwarded) =
                irq_driver::gic::local_ppi_state_for_test(27).expect("GICv2 PPI");
            core::assert!(!active);
            core::assert!(forwarded);
            self.withdrawals.fetch_add(1, Ordering::Relaxed);
            return;
        }
        core::assert!(matches!(offset, 0 | 8 | 16));
        let count = self.checkpoints.fetch_add(1, Ordering::Relaxed);
        if cfg!(feature = "smp") && kcpu_id_map::nr_cpus() >= 2 {
            let cpu = (count + 1) & 1;
            core::assert!(ktask::set_current_affinity(
                ktask::KCpuMask::one_shot_logical(LogicalCpuId::new(cpu))
            ));
            core::assert_eq!(khal::percpu::this_cpu_id(), LogicalCpuId::new(cpu));
            self.visits.fetch_or(1 << cpu, Ordering::Relaxed);
        }
        if count == 0 {
            self.ready.fetch_or(1 << self.guest, Ordering::Release);
            while self.ready.load(Ordering::Acquire) != 3 {
                ktask::yield_now();
            }
        }
    }
}

core::arch::global_asm!(include_str!("timer_guest.S"));

unsafe extern "C" {
    fn kvmm_timer_guest_start();
    fn kvmm_timer_guest_end();
}

#[unittest::def_test(serial)]
fn two_gicv2_guests_keep_timer_ownership_across_migration() {
    if kirq::IntrManagerIf::guest_interface().is_none() {
        return unittest::TestResult::Ignored;
    }
    assert!(super::CurrentArch::percpu_hw_init());
    let ready = Arc::new(AtomicUsize::new(0));
    let stopped = Arc::new(AtomicUsize::new(0));
    let mut guests = alloc::vec::Vec::new();
    for guest in 0..2 {
        let ram = 0x4000_0000 + guest as u64 * 0x20_0000;
        let mut vm = Vm::<super::CurrentArch>::new(VmConfig::new(ram, 0x20_0000, 1)).expect("VM");
        super::setup_gic(&mut vm).expect("GICv2 virtualization setup");
        vm.shared()
            .devices()
            .add_hook_factory(Arc::new(StopProbe(stopped.clone())));
        let visits = Arc::new(AtomicUsize::new(0));
        let checkpoints = Arc::new(AtomicUsize::new(0));
        let withdrawals = Arc::new(AtomicUsize::new(0));
        vm.shared()
            .devices()
            .register_mmio(kalloc_types::KBox::new_nowait(MigrationCheckpoint {
                visits: visits.clone(),
                checkpoints: checkpoints.clone(),
                withdrawals: withdrawals.clone(),
                ready: ready.clone(),
                guest,
            }).expect("migration checkpoint allocation"));
        let memory = vm.shared().guest_mem().expect("guest memory");
        let code = kaddr_layout::p2v(memory.gpa_to_hpa(ram).expect("code GPA") as usize);
        let mailbox =
            kaddr_layout::p2v(memory.gpa_to_hpa(ram + 0x10000).expect("mailbox GPA") as usize);
        let source = kvmm_timer_guest_start as *const u8;
        let size = kvmm_timer_guest_end as *const () as usize - source as usize;
        assert!(size <= 0x2000);
        // SAFETY: the finite assembly blob is readable; VM RAM owns at least two
        // destination pages and remains alive until the vCPU joins below.
        unsafe { core::ptr::copy_nonoverlapping(source, code as *mut u8, size) };
        karch::flush_icache_range(memaddr::VirtAddr::from_usize(code), size);
        let mut vcpu = vm.create_vcpu(0);
        vcpu.arch.elr = ram;
        vcpu.arch.gprs[0] = ram + 0x10000;
        vcpu.arch.gprs[1] = guest as u64;
        vcpu.arch.sp_el1 = ram + 0x20000;
        vcpu.arch.spsr = 0x5 | (0xf << 6);
        let task = spawn_vcpu_thread(vcpu);
        guests.push((vm, task, mailbox, visits, checkpoints, withdrawals, guest));
    }
    for (vm, task, mailbox, visits, checkpoints, withdrawals, guest) in guests {
        let status = task.join();
        // SAFETY: the joined guest no longer writes its mailbox. The VM still
        // owns this mapped page, whose first three words are the fixture results.
        let results = unsafe { core::slice::from_raw_parts(mailbox as *const u64, 3) };
        assert_eq!(status, 0);
        assert_eq!(results, &[8, 0, 0x600d]);
        assert_eq!(checkpoints.load(Ordering::Relaxed), 24 - guest);
        assert_eq!(withdrawals.load(Ordering::Relaxed), 8);
        if cfg!(feature = "smp") && kcpu_id_map::nr_cpus() >= 2 {
            assert_eq!(visits.load(Ordering::Relaxed), 3);
        }
        vm.shutdown();
    }
    assert_eq!(stopped.load(Ordering::Acquire), 2);
}
