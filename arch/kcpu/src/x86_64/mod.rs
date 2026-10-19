// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! x86_64 CPU context, trap, and userspace support.

mod ctx;
mod current;
pub use current::{current_task_ptr, set_current_task_ptr};
mod gdt;
mod idt;

pub mod instrs;
pub use instrs as asm;
pub use karch::hypercall;
pub(crate) mod boot;
pub use boot::init_trap;

mod excp;

pub mod userspace;

pub use self::ctx::{
    ExceptionContext as TrapFrame, ExceptionContext, ExtendedState, FxStateBlock, TaskContext,
};

#[cfg(all(unittest, target_arch = "x86_64"))]
unittest::path_mod_test!(pub mod tests_arch, "../../ut-tests/x86_64.rs");
