// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

// Portions of this file are derived from open source projects including rust-embedded/cortex-a and openeuler/rust_shyper.
// See project documentation and repository history for details.
// SPDX-License-Identifier: MIT license OR Apache-2.0
//
// This file incorporates code from:
// - rust-embedded/cortex-a (https://github.com/rust-embedded/cortex-a)
// - openeuler/rust_shyper (https://gitee.com/openeuler/rust_shyper)
//
// Please refer to the respective repositories for original license terms and copyright.
//

/// Move to ARM register from system coprocessor register.
/// MRS Xd, sysreg "Xd = sysreg"
#[macro_export]
macro_rules! mrs {
    ($reg:expr) => {{
        let r: u64;
        // SAFETY: `mrs` reads a system register into a general-purpose
        // register.  `nomem nostack` constrain the asm to have no
        // observable side-effects beyond the register read.
        unsafe {
            core::arch::asm!(concat!("mrs {0}, ", stringify!($reg)), out(reg) r, options(nomem, nostack));
        }
        r
    }};
    ($val:expr, $reg:expr $(,)?) => {
        $crate::mrs!(@inner $val, $reg)
    };
    ($val:expr, $reg:expr, $asm_width:tt $(,)?) => {
        $crate::mrs!(@inner_w $val, $reg, $asm_width)
    };
    (@inner $val:expr, $reg:expr) => {{
        // `out(reg)` requires the destination variable directly in the
        // asm! block — local capture is not possible.
        #[allow(clippy::macro_metavars_in_unsafe)]
        {
            // SAFETY: reads a system register identified by the caller.
            // `nomem nostack` constrain the asm to the register access only.
            unsafe {
                core::arch::asm!(concat!("mrs {0}, ", stringify!($reg)), out(reg) $val, options(nomem, nostack));
            }
        }
    }};
    (@inner_w $val:expr, $reg:expr, $asm_width:tt) => {{
        #[allow(clippy::macro_metavars_in_unsafe)]
        {
            // SAFETY: reads a system register identified by the caller.
            // `nomem nostack` constrain the asm to the register access only.
            unsafe {
                core::arch::asm!(concat!("mrs {0:", $asm_width, "}, ", stringify!($reg)), out(reg) $val, options(nomem, nostack));
            }
        }
    }};
}

/// Move to system coprocessor register from ARM register.
/// MSR sysreg, Xn "sysreg = Xn"
#[macro_export]
macro_rules! msr {
    ($reg:expr, $val:expr $(,)?) => {
        $crate::msr!(@inner $reg, $val)
    };
    ($reg:expr, $val:expr, $asm_width:tt $(,)?) => {
        $crate::msr!(@inner_w $reg, $val, $asm_width)
    };
    (@inner $reg:expr, $val:expr) => {{
        // Bind the caller's value outside the unsafe block.
        let __val = $val;
        // SAFETY: writes a system register identified by the caller.
        // `nomem nostack` constrain the asm to the register write only.
        unsafe {
            core::arch::asm!(concat!("msr ", stringify!($reg), ", {0}"), in(reg) __val, options(nomem, nostack));
        }
    }};
    (@inner_w $reg:expr, $val:expr, $asm_width:tt) => {{
        let __val = $val;
        // SAFETY: writes a system register identified by the caller.
        // `nomem nostack` constrain the asm to the register write only.
        unsafe {
            core::arch::asm!(concat!("msr ", stringify!($reg), ", {0:", $asm_width, "}"), in(reg) __val, options(nomem, nostack));
        }
    }};
}

/// Instruction Synchronization Barrier — flushes the pipeline.
#[macro_export]
macro_rules! isb {
    () => {
        $crate::regs::isb()
    };
}

#[macro_export]
macro_rules! sysreg_encode_addr {
    ($op0:expr, $op1:expr, $crn:expr, $crm:expr, $op2:expr) => {{
        // (Op0[21..20] + Op2[19..17] + Op1[16..14] + CRn[13..10]) + CRm[4..1]
        ((($op0 & 0b11) << 20)
            | (($op2 & 0b111) << 17)
            | (($op1 & 0b111) << 14)
            | (($crn & 0xf) << 10)
            | (($crm & 0xf) << 1))
    }};
}

/// Address Translation instruction.
#[macro_export]
macro_rules! arm_at {
    ($at_op:expr, $addr:expr) => {
        $crate::arm_at!(@inner $at_op, $addr)
    };
    (@inner $at_op:expr, $addr:expr) => {{
        let __addr = $addr;
        // SAFETY: `AT` performs address translation.  The caller must pass
        // a valid address operand and AT operation string.
        unsafe {
            core::arch::asm!(concat!("AT ", $at_op, ", {0}"), in(reg) __addr, options(nomem, nostack));
        }
        isb!();
    }};
}

macro_rules! __read_raw {
    ($width:ty, $asm_instr:tt, $asm_reg_name:tt, $asm_width:tt) => {
        /// Reads the raw bits of the CPU register.
        #[inline]
        fn get(&self) -> $width {
            match () {
                #[cfg(target_arch = "aarch64")]
                () => {
                    let reg;
                    // SAFETY: reads a system or general-purpose register
                    // identified by `$asm_reg_name`.  The caller selects a
                    // valid register; `nomem nostack` constrain the asm to
                    // have no unexpected side-effects.
                    unsafe {
                        core::arch::asm!(concat!($asm_instr, " {reg:", $asm_width, "}, ", $asm_reg_name), reg = out(reg) reg, options(nomem, nostack));
                    }
                    reg
                }

                #[cfg(not(target_arch = "aarch64"))]
                () => unimplemented!(),
            }
        }
    };
}

macro_rules! __write_raw {
    ($width:ty, $asm_instr:tt, $asm_reg_name:tt, $asm_width:tt) => {
        /// Writes raw bits to the CPU register.
        #[cfg_attr(not(target_arch = "aarch64"), allow(unused_variables))]
        #[inline]
        fn set(&self, value: $width) {
            match () {
                #[cfg(target_arch = "aarch64")]
                () => {
                    // SAFETY: writes a system or general-purpose register
                    // identified by `$asm_reg_name`.  The caller selects a
                    // valid register and value; `nomem nostack` constrain
                    // the asm to have no unexpected side-effects.
                    unsafe {
                        core::arch::asm!(concat!($asm_instr, " ", $asm_reg_name, ", {reg:", $asm_width, "}"), reg = in(reg) value, options(nomem, nostack))
                    }
                }

                #[cfg(not(target_arch = "aarch64"))]
                () => unimplemented!(),
            }
        }
    };
}

/// Raw read from system coprocessor registers.
macro_rules! sys_coproc_read_raw {
    ($width:ty, $asm_reg_name:tt, $asm_width:tt) => {
        __read_raw!($width, "mrs", $asm_reg_name, $asm_width);
    };
}

/// Raw write to system coprocessor registers.
macro_rules! sys_coproc_write_raw {
    ($width:ty, $asm_reg_name:tt, $asm_width:tt) => {
        __write_raw!($width, "msr", $asm_reg_name, $asm_width);
    };
}

/// Raw read from (ordinary) registers.
#[macro_export]
macro_rules! read_raw {
    ($width:ty, $asm_reg_name:tt, $asm_width:tt) => {
        __read_raw!($width, "mov", $asm_reg_name, $asm_width);
    };
}

/// Raw write to (ordinary) registers.
#[macro_export]
macro_rules! write_raw {
    ($width:ty, $asm_reg_name:tt, $asm_width:tt) => {
        __write_raw!($width, "mov", $asm_reg_name, $asm_width);
    };
}
