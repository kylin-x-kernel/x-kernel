// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! RISC-V CPU context, trap, and userspace support.

#[macro_use]
mod macros;

mod ctx;
mod excp;

pub mod instrs;
pub use instrs as asm;
pub mod boot;

#[cfg(feature = "uspace")]
pub mod userspace;

pub use self::ctx::{
    ExceptionContext as TrapFrame, ExceptionContext, FpState, GeneralRegisters, TaskContext,
};
