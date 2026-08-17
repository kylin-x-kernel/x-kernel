// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VMM self-test: creates a minimal VM and runs a finite guest to verify
//! the context-switch path end-to-end.

use alloc::{boxed::Box, vec::Vec};

use vdev_test_mmio::{TEST_MMIO_GPA, TEST_MMIO_SIZE, TestMmioDevice};

use crate::{
    arch::VmmArch,
    mm::GuestMem,
    vcpu::{Vcpu, spawn_vcpu_thread, vmm_run_vcpu},
    vm::{Vm, VmConfig},
};

struct BackgroundVm {
    _vm: Vm<CurrentArch>,
    _task: ktask::KtaskRef,
}

static BACKGROUND_VMS: ksync::Mutex<Vec<BackgroundVm>> = ksync::Mutex::new(Vec::new());

/// Pages owned by the guest loader, freed when dropped.
struct GuestPages {
    code_page: kalloc::GlobalPage,
    stack_page: kalloc::GlobalPage,
}

impl GuestPages {
    /// Move all pages into the vCPU's hw_pages vec for lifetime binding.
    fn bind_to_vcpu<A: VmmArch>(self, vcpu: &mut Vcpu<A>) {
        vcpu.hw_pages.push(self.code_page);
        vcpu.hw_pages.push(self.stack_page);
    }
}

/// Allocate pages, copy guest test code and set up a stack.
///
/// Returns `(entry_va, stack_top_va, pages)` — kernel VAs suitable for
/// `init_vcpu`. The `GuestPages` must be kept alive for the vCPU's lifetime.
fn load_guest<A: VmmArch>() -> (u64, u64, GuestPages) {
    let (code_src, code_size) = A::guest_test_code();
    let copy_len = if code_size > 0 { code_size } else { 4096 };

    let code_page = kalloc::GlobalPage::alloc_zero().unwrap();
    let code_dst = code_page.as_ptr() as *mut u8;
    // SAFETY: code_src points to readable .text; code_dst is a fresh page.
    unsafe { core::ptr::copy_nonoverlapping(code_src, code_dst, copy_len) };
    let entry_va = code_dst as u64;

    log::info!("[vmm] loader: copied {} bytes to {:#x}", copy_len, entry_va);

    let stack_page = kalloc::GlobalPage::alloc_zero().unwrap();
    // SAFETY: adding PAGE_SIZE reaches the top of a valid 4 KiB page.
    let stack_top = unsafe { stack_page.as_ptr().add(4096) } as u64;

    log::info!("[vmm] loader: entry={:#x} sp={:#x}", entry_va, stack_top);
    (
        entry_va,
        stack_top,
        GuestPages {
            code_page,
            stack_page,
        },
    )
}

/// Run the VMM self-test for the current architecture.
///
/// Creates a single-vCPU VM, points it at a built-in guest test program
/// that does 100 iterations of hypercall loops, then exits via DONE.
///
/// Returns `true` on success (guest exited normally), `false` on failure.
pub fn vmm_selftest() -> bool {
    log::warn!("[vmm] === VMM Self-Test ===");
    selftest_impl::<CurrentArch>()
}

fn selftest_impl<A: VmmArch + 'static>() -> bool {
    if !A::percpu_hw_init() {
        log::error!("[vmm] selftest: per-CPU init failed");
        return false;
    }

    let cfg = VmConfig::new(0, 0, 1);

    let vm: Vm<A> = match Vm::new(cfg) {
        Some(vm) => vm,
        None => {
            log::error!("[vmm] selftest: vm_create failed");
            return false;
        }
    };

    let mut vcpu = vm.create_vcpu(0);

    let (entry, sp, pages) = load_guest::<A>();
    if !A::init_vcpu(&mut vcpu, entry, sp) {
        log::error!("[vmm] selftest: init_vcpu failed");
        return false;
    }
    pages.bind_to_vcpu(&mut vcpu);

    let task = spawn_vcpu_thread::<A>(vcpu);
    let exit_code = task.join();
    vm.shutdown();

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
    let nr_cpus = kcpu_id_map::nr_cpus();
    let total = nr_cpus * VCPUS_PER_CPU;
    log::warn!(
        "[vmm] === SMP Self-Test: {} vCPUs on {} CPUs ===",
        total,
        nr_cpus,
    );
    selftest_smp_impl::<CurrentArch>()
}

fn selftest_smp_impl<A: VmmArch + 'static>() -> bool {
    let nr_cpus = kcpu_id_map::nr_cpus();
    let total = nr_cpus * VCPUS_PER_CPU;

    let cfg = VmConfig::new(0, 0, total);
    let vm: Vm<A> = match Vm::new(cfg) {
        Some(vm) => vm,
        None => {
            log::error!("[vmm] smp: vm_create failed");
            return false;
        }
    };

    // Load guest code once — shared read-only by all vCPUs.
    let (code_src, code_size) = A::guest_test_code();
    let copy_len = if code_size > 0 { code_size } else { 4096 };
    let code_page = kalloc::GlobalPage::alloc_zero().unwrap();
    let code_dst = code_page.as_ptr() as *mut u8;
    // SAFETY: code_src points to readable .text; code_dst is a fresh page.
    unsafe { core::ptr::copy_nonoverlapping(code_src, code_dst, copy_len) };
    let entry_va = code_dst as u64;
    log::info!("[vmm] loader: copied {} bytes to {:#x}", copy_len, entry_va);

    let vm_ref = alloc::sync::Arc::clone(vm.shared());
    let mut tasks = alloc::vec::Vec::with_capacity(total);

    for cpu in 0..nr_cpus {
        for v in 0..VCPUS_PER_CPU {
            let vcpu_id = (cpu * VCPUS_PER_CPU + v) as u32;
            let vm_ref = alloc::sync::Arc::clone(&vm_ref);
            tasks.push(ktask::spawn_with_name(
                move || {
                    let mask = ktask::KCpuMask::one_shot(cpu);
                    ktask::set_current_affinity(mask);

                    if !A::percpu_hw_init() {
                        log::error!("[vmm] smp: per-CPU init failed on CPU {}", cpu);
                        ktask::exit(1);
                    }

                    let stack_page = kalloc::GlobalPage::alloc_zero().unwrap();
                    // SAFETY: adding PAGE_SIZE reaches the top of a valid 4 KiB page.
                    let stack_top = unsafe { stack_page.as_ptr().add(4096) } as u64;

                    let mut vcpu = Vcpu::<A>::new(vcpu_id, vm_ref);
                    if !A::init_vcpu(&mut vcpu, entry_va, stack_top) {
                        log::error!("[vmm] smp: init_vcpu failed vcpu{}", vcpu_id);
                        ktask::exit(1);
                    }
                    vcpu.hw_pages.push(stack_page);

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
    drop(code_page);
    vm.shutdown();
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

// ── Guest Memory Self-Test ──

/// Run the VMM guest-memory self-test for the current architecture.
///
/// Creates a single-vCPU VM with a 4 GiB identity-mapped second-stage
/// page table (Stage-2 / EPT / G-stage), activates it, and runs the
/// same guest test program. If the identity map is correct, the guest
/// executes normally. Any page-table bug causes an immediate fault.
///
/// Returns `true` on success (guest exited normally).
pub fn vmm_selftest_guest_mem() -> bool {
    log::warn!("[vmm] === VMM Guest-Memory Self-Test ===");
    selftest_guest_mem_impl::<CurrentArch>()
}

fn selftest_guest_mem_impl<A: VmmArch + 'static>() -> bool {
    if !A::percpu_hw_init() {
        log::error!("[vmm] guest-mem selftest: per-CPU init failed");
        return false;
    }

    let cfg: VmConfig = VmConfig::new(0, 0x1_0000_0000, 1);

    let mut vm: Vm<A> = match Vm::new(cfg) {
        Some(vm) => vm,
        None => {
            log::error!("[vmm] guest-mem selftest: vm_create failed");
            return false;
        }
    };

    // Register test MMIO device and unmap the GPA range so guest access traps.
    vm.shared()
        .mmio_bus()
        .lock()
        .register(Box::new(TestMmioDevice));
    if let Some(gm) = vm.guest_mem_mut() {
        gm.unmap_range(TEST_MMIO_GPA, TEST_MMIO_SIZE);
    }

    let mut vcpu = vm.create_vcpu(0);

    let (entry, sp, pages) = load_guest::<A>();
    if !A::init_vcpu(&mut vcpu, entry, sp) {
        log::error!("[vmm] guest-mem selftest: init_vcpu failed");
        return false;
    }
    pages.bind_to_vcpu(&mut vcpu);

    let task = spawn_vcpu_thread::<A>(vcpu);
    let exit_code = task.join();
    vm.shutdown();

    if exit_code == 0 {
        log::info!("[vmm] guest-mem selftest: PASSED");
        true
    } else {
        log::error!("[vmm] guest-mem selftest: FAILED");
        false
    }
}

// ── Multi-VM Self-Test ──

/// Guest test mode: infinite WFI/HLT loop (VM stays alive in background).
const GUEST_MODE_INFINITE: u64 = 1;

/// Run the multi-VM self-test: create 2 independent VMs with distinct VMIDs
/// running in infinite-loop mode to verify the system remains functional
/// with background VMs.
pub fn vmm_selftest_multi_vm() -> bool {
    log::warn!("[vmm] === Multi-VM Self-Test ===");
    selftest_multi_vm_impl()
}

fn selftest_multi_vm_impl() -> bool {
    const NUM_VMS: usize = 2;

    if !CurrentArch::percpu_hw_init() {
        log::error!("[vmm] multi-vm selftest: per-CPU init failed");
        return false;
    }

    for vm_idx in 0..NUM_VMS {
        let mut vm: Vm<CurrentArch> = match Vm::new(VmConfig::new(0, 0x1_0000_0000, 1)) {
            Some(vm) => vm,
            None => {
                log::error!("[vmm] multi-vm selftest: vm_create failed for VM{}", vm_idx);
                return false;
            }
        };

        vm.shared()
            .mmio_bus()
            .lock()
            .register(Box::new(TestMmioDevice));
        if let Some(gm) = vm.guest_mem_mut() {
            gm.unmap_range(TEST_MMIO_GPA, TEST_MMIO_SIZE);
        }

        let mut vcpu = vm.create_vcpu(0);

        let (entry, sp, pages) = load_guest::<CurrentArch>();
        if !CurrentArch::init_vcpu(&mut vcpu, entry, sp) {
            log::error!("[vmm] multi-vm selftest: init_vcpu failed VM{}", vm_idx);
            return false;
        }
        pages.bind_to_vcpu(&mut vcpu);

        // Set guest mode register to infinite loop.
        set_guest_mode::<CurrentArch>(&mut vcpu, GUEST_MODE_INFINITE);

        log::info!("[vmm] multi-vm: VM{} launched (infinite mode)", vm_idx);

        vm.register();
        let task = spawn_vcpu_thread::<CurrentArch>(vcpu);
        BACKGROUND_VMS.lock().push(BackgroundVm {
            _vm: vm,
            _task: task,
        });
    }

    log::info!(
        "[vmm] multi-vm selftest: PASSED ({} VMs running in background)",
        NUM_VMS
    );
    true
}

/// Set the guest "test mode" register before first entry.
///
/// aarch64: x0, x86_64: rdi, riscv64: a0
fn set_guest_mode<A: VmmArch>(vcpu: &mut Vcpu<A>, mode: u64) {
    let _ = mode;
    let _ = vcpu;
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: on aarch64, A::ArchVcpu is Aarch64Vcpu — same type at same address.
        let ctx: &mut crate::arch::aarch64::Aarch64Vcpu =
            unsafe { &mut *((&mut vcpu.arch) as *mut A::ArchVcpu as *mut _) };
        ctx.gprs[0] = mode;
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: on x86_64, A::ArchVcpu is X86Vcpu — same type at same address.
        let ctx: &mut crate::arch::x86_64::X86Vcpu =
            unsafe { &mut *((&mut vcpu.arch) as *mut A::ArchVcpu as *mut _) };
        ctx.rdi = mode;
    }
    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: on riscv64, A::ArchVcpu is RiscvVcpu — same type at same address.
        let ctx: &mut crate::arch::riscv64::RiscvVcpu =
            unsafe { &mut *((&mut vcpu.arch) as *mut A::ArchVcpu as *mut _) };
        ctx.gprs[10] = mode;
    }
}

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

    pub struct UnsupportedGuestMem;

    impl crate::mm::GuestMem for UnsupportedGuestMem {
        fn new(_mem_base: u64, _mem_size: u64, _vmid: u32) -> Option<Self> {
            None
        }

        fn map_region(
            &mut self,
            _gpa: u64,
            _hpa: u64,
            _size: u64,
            _perm: crate::mm::GuestPerm,
        ) -> bool {
            false
        }

        fn gpa_to_hpa(&self, _gpa: u64) -> Option<u64> {
            None
        }

        fn activate(&self) {}
    }

    impl VmmArch for UnsupportedArch {
        type ArchVcpu = UnsupportedVcpu;
        type GuestMem = UnsupportedGuestMem;

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

        fn guest_test_code() -> (*const u8, usize) {
            (core::ptr::null(), 0)
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
