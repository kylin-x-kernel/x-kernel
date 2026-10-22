// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! RISC-V guest round-trip regression for host current-task and guest tp isolation.

use super::{CAUSE_VS_ECALL, RiscvVcpu, hext_enter_guest, read_hstatus, write_hstatus};

/// # Safety
///
/// Enter only in VS-mode with identity-mapped physical code and an HS trap
/// vector that saves guest registers on ecall. This is not a host ABI function.
#[unsafe(naked)]
unsafe extern "C" fn guest_changes_tp() -> ! {
    core::arch::naked_asm!(
        "li tp, 0x1234",
        "ecall",
        "addi tp, tp, 1",
        "ecall",
        "1: j 1b",
    );
}

#[unittest::def_test(serial)]
fn guest_roundtrips_keep_host_current_and_guest_tp_separate() {
    let owner = ktask::current().clone();
    let expected = ktask::KtaskRef::as_ptr(&owner).cast::<()>();
    let mut vcpu = RiscvVcpu {
        pc: kaddr_layout::v2p(guest_changes_tp as *const () as usize) as u64,
        ..Default::default()
    };
    let mut observations = [(0, 0, 0, core::ptr::null()); 2];
    {
        let _irq = ksync::spin::IrqSave::new();
        let hstatus = read_hstatus();
        let hgatp: usize;
        let sie: usize;
        // SAFETY: IRQs/migration are excluded on this H-extension CPU. The
        // guest executes physical kernel text without memory accesses; disable
        // G-stage translation and IRQ sources for these two bounded ecalls.
        // Restore both CSRs before releasing the guard, including on failure.
        unsafe {
            core::arch::asm!(
                "csrrw {hgatp}, 0x680, zero",
                "hfence.gvma",
                "csrrw {sie}, sie, zero",
                hgatp = out(reg) hgatp,
                sie = out(reg) sie,
                options(nostack),
            );
        }
        for observation in &mut observations {
            // SAFETY: vcpu is exclusively borrowed and live throughout entry;
            // IRQs/migration are excluded, and its PC names the VS-only code.
            let entered = unsafe { hext_enter_guest(&mut vcpu) };
            *observation = (
                entered,
                vcpu.scause_save,
                vcpu.gprs[4],
                khal::context::current_task_ptr(),
            );
            if entered != 1 || vcpu.scause_save != CAUSE_VS_ECALL {
                break;
            }
            vcpu.pc += 4;
        }
        // SAFETY: These are the saved host CSR values from this pinned CPU;
        // no assertion or fallible operation intervenes before restoration.
        unsafe {
            core::arch::asm!(
                "csrw 0x680, {hgatp}",
                "hfence.gvma",
                "csrw sie, {sie}",
                hgatp = in(reg) hgatp,
                sie = in(reg) sie,
                options(nostack),
            );
        }
        write_hstatus(hstatus);
    }
    for (index, (entered, cause, guest_tp, current)) in observations.into_iter().enumerate() {
        assert_eq!(entered, 1);
        assert_eq!(cause, CAUSE_VS_ECALL);
        assert_eq!(guest_tp, 0x1234 + index as u64);
        assert_eq!(current, expected);
    }
}
