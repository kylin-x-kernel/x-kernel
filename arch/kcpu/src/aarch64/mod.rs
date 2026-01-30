// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! AArch64 CPU context, trap, and userspace support.

mod ctx;

pub mod boot;
pub mod instrs;

mod excp;

#[cfg(feature = "uspace")]
pub mod userspace;

pub use self::ctx::{ExceptionContext as TrapFrame, ExceptionContext, FpState, TaskContext};
