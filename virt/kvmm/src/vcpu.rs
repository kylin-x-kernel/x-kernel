// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! vCPU control block and VMM run loop.

use alloc::sync::Arc;
use core::sync::atomic::Ordering;

#[cfg(target_arch = "aarch64")]
use crate::vdev::aarch64::vpsci;
use crate::{
    arch::VmmArch,
    mm::GuestMem,
    vm::{
        EXIT_CAT_HALT, EXIT_CAT_HYPERCALL, EXIT_CAT_INTERRUPT, EXIT_CAT_MMIO, EXIT_CAT_OTHER,
        VcpuRunState, VmRef, VmShared,
    },
};

/// VMM exit action returned by the architecture exit handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitAction {
    /// Continue running the guest.
    Resume  = 0,
    /// Guest exited normally.
    VmExit  = 1,
    /// Guest aborted.
    VmAbort = 2,
    /// Skip the current instruction and resume.
    VmSkip  = 3,
    /// Unhandled exit, stop VMM loop.
    Exit    = 4,
}

/// Maximum number of vCPUs per VM.
pub const MAX_VCPUS: usize = 16;

/// Architecture-specific vCPU state.
///
/// Each architecture defines its own `ArchVcpu` that contains
/// guest registers, host context save area, and system registers.
/// The layout of `ArchVcpu` is ABI-stable for assembly access.
#[repr(C)]
pub struct Vcpu<A: VmmArch> {
    /// Architecture-specific state (GPRs, sysregs, host context).
    pub arch: A::ArchVcpu,
    /// vCPU identifier within the VM.
    pub vcpu_id: u32,
    /// Whether `vmlaunch` has run for this vCPU (x86 VMX only).
    /// Read by `vmx_run.S` at a fixed ABI offset from `arch`.
    pub launched: bool,
    /// Exit reason category set by the arch exit handler (see `EXIT_CAT_*`).
    pub exit_category: u8,
    /// Back-reference to the parent VM's shared state.
    pub vm: VmRef<A>,
    /// Hardware pages owned by this vCPU (e.g. x86 VMCS page).
    /// Placed after all assembly-visible fields so ABI offsets are stable.
    pub hw_pages: alloc::vec::Vec<kalloc::GlobalPage>,
    /// World-switch hooks (e.g. the vGIC) run inside the IRQ-masked window
    /// around guest entry/exit to load/read-back per-physical-CPU device state.
    pub hooks: alloc::vec::Vec<alloc::boxed::Box<dyn crate::vdev::VcpuHook<A>>>,
}

impl<A: VmmArch> Vcpu<A> {
    /// Create a new vCPU bound to the given VM.
    pub fn new(vcpu_id: u32, vm: Arc<VmShared<A>>) -> Self {
        let mut vcpu = Self {
            arch: A::ArchVcpu::default(),
            vcpu_id,
            launched: false,
            exit_category: EXIT_CAT_OTHER,
            vm,
            hw_pages: alloc::vec::Vec::new(),
            hooks: alloc::vec::Vec::new(),
        };
        let vm = Arc::clone(&vcpu.vm);
        vm.devices().install_vcpu_hooks(&mut vcpu);
        vcpu
    }
}

/// Architecture-independent vCPU execution main loop.
///
/// Drives the guest through the four architecture hooks:
/// `restore_guest_ctx` → `enter_guest` → `save_guest_ctx` → `exit_handler`.
///
/// Returns `Ok(())` on normal exit, `Err(())` on unhandled exit or entry failure.
#[allow(clippy::result_unit_err)]
pub fn vmm_run_vcpu<A: VmmArch>(vcpu: &mut Vcpu<A>) -> Result<(), ()> {
    log::info!("[VMM] Starting vcpu{}", vcpu.vcpu_id);

    // One-time guest memory activation on the vCPU thread.
    // x86: writes EPTP to VMCS. aarch64/riscv64: writes per-CPU register.
    {
        let vm_ref = Arc::clone(&vcpu.vm);
        if let Some(gm) = vm_ref.guest_mem() {
            A::activate_guest_mem(vcpu, gm);
        }
    }

    let vm_stats = Arc::clone(&vcpu.vm);
    let mut exit_start = khal::time::now_ticks();
    // Cooperative fairness (kernel preemption is disabled): bound how long the
    // vCPU monopolises its pCPU by yielding at most once per `yield_interval`
    // of wall time, rather than on every exit.
    let mut last_yield = khal::time::now_ticks();
    let yield_interval = (khal::time::freq() / 1000).max(1); // ~1 ms

    let result = loop {
        // The per-vCPU state that actually lives in *per-physical-CPU* hardware
        // — the Stage-2/EPT root (VTTBR/EPTP/hgatp), the guest EL1 register
        // bank, and, once interrupt injection lands, the GICH list registers —
        // must be loaded, entered, and read back with IRQs masked. Otherwise
        // the scheduler can preempt this thread and migrate it to another pCPU
        // between the load and the `eret`, so the guest resumes on a CPU
        // holding *another* vCPU's hardware state (corrupt Stage-2 / lost
        // registers / a dropped just-injected IRQ). This extends the narrow
        // IRQ-masked `tpidr_el2` window inside `el2_enter_guest` to cover the
        // whole Rust-side setup and read-back.
        let irq_guard = ksync::spin::IrqSave::new();

        let pcpu = khal::percpu::this_cpu_id().as_usize() as i32;
        vcpu.vm.set_vcpu_pcpu(vcpu.vcpu_id, pcpu);

        if let Some(gm) = vcpu.vm.guest_mem() {
            gm.activate();
        }
        A::restore_guest_ctx(vcpu);
        // Load per-physical-CPU device state (vGIC list registers) for entry.
        let hooks = core::mem::take(&mut vcpu.hooks);
        for mut hook in hooks {
            hook.on_entry(vcpu);
            vcpu.hooks.push(hook);
        }
        vcpu.vm
            .set_vcpu_run_state(vcpu.vcpu_id, VcpuRunState::RunningGuest);

        // Account host-handling time since the previous exit.
        let guest_start = khal::time::now_ticks();
        vm_stats.vcpu_stats(vcpu.vcpu_id).exit_ticks.fetch_add(
            guest_start.wrapping_duration_since(exit_start).as_raw(),
            Ordering::Relaxed,
        );

        if !A::enter_guest(vcpu) {
            vcpu.vm.set_vcpu_pcpu(vcpu.vcpu_id, -1);
            vcpu.vm
                .set_vcpu_run_state(vcpu.vcpu_id, VcpuRunState::HostHandlingExit);
            log::error!("[VMM] vcpu{}: guest entry failed", vcpu.vcpu_id);
            break Err(()); // irq_guard drops here → IRQs restored
        }

        vcpu.vm.set_vcpu_pcpu(vcpu.vcpu_id, -1);
        vcpu.vm
            .set_vcpu_run_state(vcpu.vcpu_id, VcpuRunState::HostHandlingExit);

        // Account guest execution time for this entry.
        exit_start = khal::time::now_ticks();
        {
            let stats = vm_stats.vcpu_stats(vcpu.vcpu_id);
            stats.guest_ticks.fetch_add(
                exit_start.wrapping_duration_since(guest_start).as_raw(),
                Ordering::Relaxed,
            );
            stats.exit_count.fetch_add(1, Ordering::Relaxed);
        }

        A::save_guest_ctx(vcpu);

        // Read back per-physical-CPU device state (vGIC list registers).
        for hook in &mut vcpu.hooks {
            hook.on_exit(vcpu.vcpu_id);
        }

        // Re-enable IRQs before `exit_handler`, which may block/yield (WFI
        // sleep, MMIO dispatch, logging).
        drop(irq_guard);

        let action = A::exit_handler(vcpu);

        // Tally the exit category the handler classified this exit as.
        let stats = vm_stats.vcpu_stats(vcpu.vcpu_id);
        let cat_counter = match vcpu.exit_category {
            EXIT_CAT_HALT => &stats.exits_halt,
            EXIT_CAT_HYPERCALL => &stats.exits_hypercall,
            EXIT_CAT_MMIO => &stats.exits_mmio,
            EXIT_CAT_INTERRUPT => &stats.exits_interrupt,
            _ => &stats.exits_other,
        };
        cat_counter.fetch_add(1, Ordering::Relaxed);

        // Bounded cooperative yield: at most once per `yield_interval`. When
        // the vCPU is the only runnable task this is a cheap self-repick (~µs);
        // when a host task is ready on this pCPU it hands the CPU over.
        let now = khal::time::now_ticks();
        if now.wrapping_duration_since(last_yield).as_raw() >= yield_interval {
            ktask::yield_now();
            last_yield = khal::time::now_ticks();
        }

        match action {
            ExitAction::Resume | ExitAction::VmSkip => continue,
            ExitAction::VmExit => {
                log::info!("[VMM] vcpu{}: guest exited normally", vcpu.vcpu_id);
                break Ok(());
            }
            ExitAction::VmAbort => {
                log::warn!("[VMM] vcpu{}: guest aborted", vcpu.vcpu_id);
                break Ok(());
            }
            ExitAction::Exit => {
                log::error!("[VMM] vcpu{}: unhandled exit, stopping VMM", vcpu.vcpu_id);
                break Err(());
            }
        }
    };

    vcpu.vm
        .set_vcpu_run_state(vcpu.vcpu_id, VcpuRunState::Offline);
    A::teardown_vcpu(vcpu);
    result
}

/// Spawn a kernel thread to run a vCPU.
///
/// The vCPU is moved into the thread and executes `vmm_run_vcpu` in a loop.
/// The thread is named `vcpu-N` and participates in normal scheduler scheduling.
/// Use `task_ref.join()` to wait for completion (returns 0 on success, 1 on error).
pub fn spawn_vcpu_thread<A: VmmArch + 'static>(vcpu: Vcpu<A>) -> ktask::KtaskRef {
    let id = vcpu.vcpu_id;
    let vm = Arc::clone(&vcpu.vm);
    let task = ktask::spawn_with_name(
        move || {
            let mut vcpu: Vcpu<A> = vcpu;
            match vmm_run_vcpu::<A>(&mut vcpu) {
                Ok(()) => ktask::exit(0),
                Err(()) => ktask::exit(1),
            }
        },
        alloc::format!("vcpu-{}", id),
    );
    // Publish the owning task so `inject_irq` can wake this vCPU when it is
    // parked in the WFI path.
    vm.set_vcpu_task(id, task.clone());
    task
}

/// Power on a secondary vCPU for PSCI `CPU_ON`.
#[cfg(target_arch = "aarch64")]
pub fn power_on_secondary<A: VmmArch + 'static>(
    vm: &Arc<VmShared<A>>,
    target_cpu: u64,
    entry_pa: u64,
    context_id: u64,
) -> u64 {
    let target_id = target_cpu & 0xff;
    let Some(id) = u32::try_from(target_id).ok() else {
        return vpsci::PSCI_RET_INVALID_PARAMS;
    };
    if id as usize >= vm.nr_vcpus() {
        log::warn!(
            "[VMM] CPU_ON: target {:#x} -> vcpu{} out of range (nr_vcpus={})",
            target_cpu,
            id,
            vm.nr_vcpus()
        );
        return vpsci::PSCI_RET_INVALID_PARAMS;
    }
    if !vm.try_mark_cpu_on(id) {
        return vpsci::PSCI_RET_ALREADY_ON;
    }

    let mut vcpu = Vcpu::<A>::new(id, Arc::clone(vm));
    if !A::init_secondary_vcpu(&mut vcpu, entry_pa, context_id) {
        log::error!("[VMM] CPU_ON: init_secondary_vcpu failed for vcpu{}", id);
        return vpsci::PSCI_RET_NOT_SUPPORTED;
    }
    spawn_vcpu_thread::<A>(vcpu);

    log::info!(
        "[VMM] CPU_ON: powered on target={:#x} vcpu{} entry={:#x} ctx={:#x}",
        target_cpu,
        id,
        entry_pa,
        context_id,
    );
    vpsci::PSCI_RET_SUCCESS
}
