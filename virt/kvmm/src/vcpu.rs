// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! vCPU control block and VMM run loop.

use alloc::sync::Arc;

use crate::{
    arch::VmmArch,
    mm::GuestMem,
    vm::{VmRef, VmShared},
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
    /// Back-reference to the parent VM's shared state.
    pub vm: VmRef<A>,
    /// Hardware pages owned by this vCPU (e.g. x86 VMCS page).
    /// Placed after all assembly-visible fields so ABI offsets are stable.
    pub hw_pages: alloc::vec::Vec<kalloc::GlobalPage>,
}

impl<A: VmmArch> Vcpu<A> {
    /// Create a new vCPU bound to the given VM.
    pub fn new(vcpu_id: u32, vm: Arc<VmShared<A>>) -> Self {
        Self {
            arch: A::ArchVcpu::default(),
            vcpu_id,
            launched: false,
            vm,
            hw_pages: alloc::vec::Vec::new(),
        }
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

    A::restore_guest_ctx(vcpu);

    let result = loop {
        let pcpu = khal::percpu::this_cpu_id().as_usize() as i32;
        vcpu.vm.set_vcpu_pcpu(vcpu.vcpu_id, pcpu);

        if let Some(gm) = vcpu.vm.guest_mem() {
            gm.activate();
        }

        if !A::enter_guest(vcpu) {
            vcpu.vm.set_vcpu_pcpu(vcpu.vcpu_id, -1);
            log::error!("[VMM] vcpu{}: guest entry failed", vcpu.vcpu_id);
            break Err(());
        }

        vcpu.vm.set_vcpu_pcpu(vcpu.vcpu_id, -1);

        A::save_guest_ctx(vcpu);

        match A::exit_handler(vcpu) {
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
    ktask::spawn_with_name(
        move || {
            let mut vcpu = vcpu;
            match vmm_run_vcpu::<A>(&mut vcpu) {
                Ok(()) => ktask::exit(0),
                Err(()) => ktask::exit(1),
            }
        },
        alloc::format!("vcpu-{}", id),
    )
}
