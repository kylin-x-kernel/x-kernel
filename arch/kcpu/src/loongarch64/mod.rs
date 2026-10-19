// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! LoongArch64 CPU context, trap, and userspace support.

#[macro_use]
mod macros;

mod ctx;
mod current;
mod excp;
mod unaligned;

pub mod instrs;
pub use instrs as asm;
pub(crate) mod boot;
pub use boot::init_trap;

pub mod userspace;

pub use self::{
    ctx::{
        ExceptionContext as TrapFrame, ExceptionContext, FpuState, GeneralRegisters, TaskContext,
    },
    current::{current_task_ptr, set_current_task_ptr},
    unaligned::UnalignedError,
};

#[cfg(all(unittest, target_arch = "loongarch64"))]
unittest::path_mod_test!(pub mod tests_arch, "../../ut-tests/loongarch64.rs");
