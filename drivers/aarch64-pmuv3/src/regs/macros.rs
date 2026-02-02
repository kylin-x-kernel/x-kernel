// Move to ARM register from system coprocessor register.
// MRS Xd, sysreg "Xd = sysreg"
#[macro_export]
macro_rules! mrs {
    ($reg: expr) => {
        ({
            let r: u64;
            unsafe {
                core::arch::asm!(concat!("mrs {0}, ", stringify!($reg)), out(reg) r, options(nomem, nostack));
            }
            r
        })
    };
    ($val: expr, $reg: expr, $asm_width:tt) => {
        unsafe {
            core::arch::asm!(concat!("mrs {0:", $asm_width, "}, ", stringify!($reg)), out(reg) $val, options(nomem, nostack));
        }
    };
    ($val: expr, $reg: expr) => {
        unsafe {
            core::arch::asm!(concat!("mrs {0}, ", stringify!($reg)), out(reg) $val, options(nomem, nostack));
        }
    };
}

// Move to system coprocessor register from ARM register.
// MSR sysreg, Xn "sysreg = Xn"
#[macro_export]
macro_rules! msr {
    ($reg: expr, $val: expr, $asm_width:tt) => {
        unsafe {
            core::arch::asm!(concat!("msr ", stringify!($reg), ", {0:", $asm_width, "}"), in(reg) $val, options(nomem, nostack));
        }
    };
    ($reg: expr, $val: expr) => {
        unsafe {
            core::arch::asm!(concat!("msr ", stringify!($reg), ", {0}"), in(reg) $val, options(nomem, nostack));
        }
    };
}

#[macro_export]
macro_rules! isb {
    () => {
        unsafe { core::arch::asm!("isb", options(nomem, nostack)) }
    };
}

#[macro_export]
macro_rules! sysreg_encode_addr {
    ($op0:expr, $op1:expr, $crn:expr, $crm:expr, $op2:expr) => {
        // (Op0[21..20] + Op2[19..17] + Op1[16..14] + CRn[13..10]) + CRm[4..1]
        ((($op0 & 0b11) << 20)
            | (($op2 & 0b111) << 17)
            | (($op1 & 0b111) << 14)
            | (($crn & 0xf) << 10)
            | (($crm & 0xf) << 1))
    };
}

#[macro_export]
macro_rules! arm_at {
    ($at_op:expr, $addr:expr) => {
        unsafe {
            core::arch::asm!(concat!("AT ", $at_op, ", {0}"), in(reg) $addr, options(nomem, nostack));
        }
        isb!();
    };
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

#[macro_export]
/// Raw read from (ordinary) registers.
macro_rules! read_raw {
    ($width:ty, $asm_reg_name:tt, $asm_width:tt) => {
        __read_raw!($width, "mov", $asm_reg_name, $asm_width);
    };
}

#[macro_export]
/// Raw write to (ordinary) registers.
macro_rules! write_raw {
    ($width:ty, $asm_reg_name:tt, $asm_width:tt) => {
        __write_raw!($width, "mov", $asm_reg_name, $asm_width);
    };
}
