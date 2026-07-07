// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! vCPU control block and VMM run loop.

use crate::arch::VmmArch;

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
pub const MAX_VCPUS: usize = 4;

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
    /// Whether the guest has been entered at least once.
    pub launched: bool,
}

impl<A: VmmArch> Vcpu<A> {
    /// Create a new vCPU with zeroed architecture state.
    pub fn new(vcpu_id: u32) -> Self {
        Self {
            arch: A::ArchVcpu::default(),
            vcpu_id,
            launched: false,
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

    A::restore_guest_ctx(vcpu);

    loop {
        if !A::enter_guest(vcpu) {
            log::error!("[VMM] vcpu{}: guest entry failed", vcpu.vcpu_id);
            return Err(());
        }

        A::save_guest_ctx(vcpu);

        match A::exit_handler(vcpu) {
            ExitAction::Resume | ExitAction::VmSkip => continue,
            ExitAction::VmExit => {
                log::info!("[VMM] vcpu{}: guest exited normally", vcpu.vcpu_id);
                return Ok(());
            }
            ExitAction::VmAbort => {
                log::warn!("[VMM] vcpu{}: guest aborted", vcpu.vcpu_id);
                return Ok(());
            }
            ExitAction::Exit => {
                log::error!("[VMM] vcpu{}: unhandled exit, stopping VMM", vcpu.vcpu_id);
                return Err(());
            }
        }
    }
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
