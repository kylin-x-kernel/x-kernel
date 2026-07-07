// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VMM self-test: creates a minimal VM and runs a finite guest to verify
//! the context-switch path end-to-end.

use crate::{
    arch::VmmArch,
    vcpu::{Vcpu, spawn_vcpu_thread, vmm_run_vcpu},
    vm::{Vm, VmConfig},
};

/// Guest stack size (16 KB).
const GUEST_STACK_SIZE: usize = 16 * 1024;

/// Static guest stack (shared across self-test runs — not reentrant).
#[repr(C, align(16))]
struct GuestStack(core::cell::UnsafeCell<[u8; GUEST_STACK_SIZE]>);

// SAFETY: The selftest is single-threaded; no concurrent access occurs.
unsafe impl Sync for GuestStack {}

static GUEST_STACK: GuestStack = GuestStack(core::cell::UnsafeCell::new([0; GUEST_STACK_SIZE]));

/// Run the VMM self-test for the current architecture.
///
/// Creates a single-vCPU VM, points it at a built-in guest test program
/// that does 100 iterations of hypercall loops, then exits via DONE.
///
/// Returns `true` on success (guest exited normally), `false` on failure.
pub fn vmm_selftest() -> bool {
    log::info!("[vmm] === VMM Self-Test ===");
    selftest_impl::<CurrentArch>()
}

fn guest_stack_top() -> usize {
    let base = GUEST_STACK.0.get() as *const u8;
    // SAFETY: pointer arithmetic within the known-size static buffer.
    unsafe { base.add(GUEST_STACK_SIZE) as usize }
}

fn selftest_impl<A: VmmArch + 'static>() -> bool {
    if !A::percpu_hw_init() {
        log::error!("[vmm] selftest: per-CPU init failed");
        return false;
    }

    let cfg = VmConfig::new(0, 0, 1);

    let mut vm: Vm<A> = match Vm::new(cfg) {
        Some(vm) => vm,
        None => {
            log::error!("[vmm] selftest: vm_create failed");
            return false;
        }
    };

    let vcpu = vm.vcpu_mut(0).unwrap();
    let mut vcpu = core::mem::replace(vcpu, Vcpu::new(0));

    let entry = A::guest_test_entry();
    let sp = guest_stack_top() as u64;
    if !A::init_vcpu(&mut vcpu, entry, sp) {
        log::error!("[vmm] selftest: init_vcpu failed");
        return false;
    }

    log::info!("[vmm] selftest: guest entry={:#x} sp={:#x}", entry, sp,);

    let task = spawn_vcpu_thread::<A>(vcpu);
    let exit_code = task.join();

    if exit_code == 0 {
        log::info!("[vmm] selftest: PASSED");
        true
    } else {
        log::error!("[vmm] selftest: FAILED");
        false
    }
}

// ── SMP Multi-Core Multi-vCPU Self-Test ──

const VCPUS_PER_CPU: usize = 2;

/// Run the SMP VMM self-test: spawn 2 vCPUs per physical CPU.
///
/// Each vCPU thread pins itself to a target CPU, performs per-CPU
/// hardware init, then runs the guest test program independently.
/// Returns `true` if all vCPUs exit normally.
pub fn vmm_selftest_smp() -> bool {
    let nr_cpus = kbuild_config::CPU_NUM;
    let total = nr_cpus * VCPUS_PER_CPU;
    log::info!(
        "[vmm] === SMP Self-Test: {} vCPUs on {} CPUs ===",
        total,
        nr_cpus,
    );
    selftest_smp_impl::<CurrentArch>()
}

fn selftest_smp_impl<A: VmmArch + 'static>() -> bool {
    let nr_cpus = kbuild_config::CPU_NUM;
    let total = nr_cpus * VCPUS_PER_CPU;
    let entry_fn = A::guest_test_entry();

    let mut tasks = alloc::vec::Vec::with_capacity(total);

    for cpu in 0..nr_cpus {
        for v in 0..VCPUS_PER_CPU {
            let vcpu_id = (cpu * VCPUS_PER_CPU + v) as u32;
            tasks.push(ktask::spawn_with_name(
                move || {
                    let mask = ktask::KCpuMask::one_shot(cpu);
                    ktask::set_current_affinity(mask);

                    if !A::percpu_hw_init() {
                        log::error!("[vmm] smp: per-CPU init failed on CPU {}", cpu);
                        ktask::exit(1);
                    }

                    let stack = kalloc::GlobalPage::alloc_zero().unwrap();
                    // SAFETY: as_ptr() returns a valid 4096-byte page; adding 4096 reaches the top.
                    let sp = unsafe { stack.as_ptr().add(4096) } as u64;
                    core::mem::forget(stack);

                    let mut vcpu = Vcpu::<A>::new(vcpu_id);
                    if !A::init_vcpu(&mut vcpu, entry_fn, sp) {
                        log::error!("[vmm] smp: init_vcpu failed vcpu{}", vcpu_id);
                        ktask::exit(1);
                    }

                    match vmm_run_vcpu::<A>(&mut vcpu) {
                        Ok(()) => ktask::exit(0),
                        Err(()) => ktask::exit(1),
                    }
                },
                alloc::format!("vcpu-{}", vcpu_id),
            ));
        }
    }

    let passed = tasks.iter().all(|t| t.join() == 0);
    if passed {
        log::info!("[vmm] smp selftest: PASSED ({} vCPUs)", total);
    } else {
        log::error!("[vmm] smp selftest: FAILED");
    }
    passed
}

// ── Architecture type alias ──

#[cfg(target_arch = "aarch64")]
type CurrentArch = crate::arch::aarch64::Aarch64Vhe;
#[cfg(target_arch = "riscv64")]
type CurrentArch = crate::arch::riscv64::RiscvHext;
#[cfg(target_arch = "x86_64")]
type CurrentArch = crate::arch::x86_64::X86Vmx;
#[cfg(not(any(
    target_arch = "aarch64",
    target_arch = "riscv64",
    target_arch = "x86_64"
)))]
type CurrentArch = UnsupportedArch;

#[cfg(not(any(
    target_arch = "aarch64",
    target_arch = "riscv64",
    target_arch = "x86_64"
)))]
mod unsupported {
    use crate::{
        arch::VmmArch,
        vcpu::{ExitAction, Vcpu},
    };

    pub struct UnsupportedArch;

    #[derive(Default)]
    pub struct UnsupportedVcpu;

    // SAFETY: UnsupportedVcpu is zero-sized and stateless.
    unsafe impl Send for UnsupportedVcpu {}

    impl VmmArch for UnsupportedArch {
        type ArchVcpu = UnsupportedVcpu;

        fn init_vcpu(_vcpu: &mut Vcpu<Self>, _entry: u64, _sp: u64) -> bool {
            false
        }

        fn restore_guest_ctx(_vcpu: &mut Vcpu<Self>) {}

        fn enter_guest(_vcpu: &mut Vcpu<Self>) -> bool {
            false
        }

        fn exit_handler(_vcpu: &mut Vcpu<Self>) -> ExitAction {
            ExitAction::Exit
        }

        fn save_guest_ctx(_vcpu: &mut Vcpu<Self>) {}

        fn guest_test_entry() -> u64 {
            0
        }

        fn percpu_hw_init() -> bool {
            log::warn!("[vmm] VMM not supported on this architecture");
            false
        }
    }
}

#[cfg(not(any(
    target_arch = "aarch64",
    target_arch = "riscv64",
    target_arch = "x86_64"
)))]
use unsupported::UnsupportedArch;
