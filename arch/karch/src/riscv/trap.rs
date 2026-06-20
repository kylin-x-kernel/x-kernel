// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Exception/trap vector operations for RISC-V.

use riscv::register::stvec;

/// Writes the Supervisor Trap Vector Base Address register (`stvec`).
///
/// # Safety
///
/// This function is unsafe as it changes the exception handling behavior of the
/// current CPU.
#[inline]
pub unsafe fn write_trap_vector_base(addr: usize) {
    let mut reg = stvec::read();
    reg.set_address(addr);
    reg.set_trap_mode(stvec::TrapMode::Direct);
    // SAFETY: the caller guarantees `addr` names a valid trap entry for the
    // current hart, and this only updates that hart's `stvec` register.
    unsafe { stvec::write(reg) }
}
