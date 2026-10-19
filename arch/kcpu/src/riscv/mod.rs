// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! RISC-V CPU context, trap, and userspace support.

#[macro_use]
mod macros;

mod ctx;
mod current;
pub use current::{current_task_ptr, set_current_task_ptr};
mod excp;
mod hwprobe;

pub mod instrs;
pub use instrs as asm;
pub(crate) mod boot;

pub mod userspace;

pub use boot::init_trap;
pub use hwprobe::{
    RISCV_HWPROBE_UNKNOWN_KEY, hwprobe_aggregate_value, hwprobe_cpu_matches, hwprobe_key_is_known,
    init_hwprobe_from_fdt,
};

pub use self::ctx::{
    ExceptionContext as TrapFrame, ExceptionContext, FpState, GeneralRegisters, TaskContext,
};

#[cfg(all(unittest, any(target_arch = "riscv32", target_arch = "riscv64")))]
unittest::path_mod_test!(pub mod tests_arch, "../../ut-tests/riscv.rs");
