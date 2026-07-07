// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! AArch64 exception level switching for early boot.

use aarch64_cpu::{asm::barrier, registers::*};

/// Swtich current exception level to EL1.
///
/// It usually used in the system booting process, where the startup code is
/// running in EL2 or EL3. Besides, the stack is not available and the MMU is
/// not enabled.
///
/// # Safety
///
/// This function may only run during early boot before normal kernel
/// execution begins. The caller must ensure the CPU is currently running in
/// EL2 or EL3 with the expected boot entry register state, that the current
/// link register names a valid continuation point after the `eret`, and that
/// switching to EL1 will not strand required stack or code mappings.
#[cfg(not(feature = "vmm"))]
#[unsafe(link_section = ".idmap.text")]
pub unsafe fn switch_to_el1() {
    let current_sp = aarch64_cpu::registers::SP.get();
    SPSel.write(SPSel::SP::ELx);
    aarch64_cpu::registers::SP.set(current_sp);
    SP_EL0.set(0);
    let current_el = CurrentEL.read(CurrentEL::EL);
    if current_el >= 2 {
        if current_el == 3 {
            // Set EL2 to 64bit and enable the HVC instruction.
            SCR_EL3.write(
                SCR_EL3::NS::NonSecure + SCR_EL3::HCE::HvcEnabled + SCR_EL3::RW::NextELIsAarch64,
            );
            // Set the return address and exception level.
            SPSR_EL3.write(
                SPSR_EL3::M::EL1h
                    + SPSR_EL3::D::Masked
                    + SPSR_EL3::A::Masked
                    + SPSR_EL3::I::Masked
                    + SPSR_EL3::F::Masked,
            );
            ELR_EL3.set(LR.get());
        }
        // Disable EL1 timer traps and the timer offset.
        CNTHCTL_EL2.modify(CNTHCTL_EL2::EL1PCEN::SET + CNTHCTL_EL2::EL1PCTEN::SET);
        CNTVOFF_EL2.set(0);
        // Set EL1 to 64bit.
        HCR_EL2.write(HCR_EL2::RW::EL1IsAarch64);
        // Set the return address and exception level.
        SPSR_EL2.write(
            SPSR_EL2::M::EL1h
                + SPSR_EL2::D::Masked
                + SPSR_EL2::A::Masked
                + SPSR_EL2::I::Masked
                + SPSR_EL2::F::Masked,
        );
        SP_EL1.set(SP.get());
        ELR_EL2.set(LR.get());
        barrier::isb(barrier::SY);
        aarch64_cpu::asm::eret();
    }
}

/// Initialize EL2 with VHE (Virtualization Host Extensions).
///
/// Instead of dropping to EL1, the kernel stays in EL2 with `HCR_EL2.E2H=1`.
/// VHE hardware aliases `_EL1` register names to their EL2 counterparts,
/// so all subsequent kernel code operates transparently at EL2.
///
/// # Safety
///
/// Same preconditions as [`switch_to_el1`]: must run during early boot
/// while the CPU is in EL2 with the expected boot register state. The
/// caller must ensure VHE is supported by the CPU (ARMv8.1+).
#[cfg(feature = "vmm")]
#[unsafe(link_section = ".idmap.text")]
pub unsafe fn init_el2_vhe() {
    let current_el = CurrentEL.read(CurrentEL::EL);
    if current_el < 2 {
        return;
    }

    // Stay in EL2 with VHE: E2H=1, RW=1, TGE=1
    HCR_EL2.write(HCR_EL2::E2H::SET + HCR_EL2::RW::EL1IsAarch64 + HCR_EL2::TGE::SET);
    barrier::isb(barrier::SY);

    // Clear CPTR_EL2.TFP (bit 10) to allow FP/NEON.
    // The aarch64-cpu crate does not define TFP, so we use raw bit ops.
    let cptr = CPTR_EL2.get();
    CPTR_EL2.set(cptr & !(1 << 10));

    // VHE-mode CNTHCTL_EL2 bit layout differs from non-VHE:
    //   bit 0  = EL0PCTEN  (don't trap EL0 CNTPCT reads)
    //   bit 1  = EL0VCTEN  (don't trap EL0 CNTVCT reads)
    //   bit 10 = EL1PTEN   (don't trap EL1 physical timer access)
    // SAFETY: raw MSR for VHE-specific bit layout not modeled by the crate.
    unsafe {
        core::arch::asm!(
            "mov {tmp}, #(1 | (1 << 1) | (1 << 10))",
            "msr cnthctl_el2, {tmp}",
            "isb",
            tmp = out(reg) _,
        );
    }

    CNTVOFF_EL2.set(0);
}
