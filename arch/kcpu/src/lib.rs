// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![cfg_attr(not(test), no_std)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![doc = include_str!("../README.md")]
#![cfg_attr(
    target_arch = "x86_64",
    expect(
        clippy::new_without_default,
        reason = "CG-024-lint-exceptions: clippy::new_without_default: x86_64 TaskContext::new() \
                  reads the live CR3, so it must not be Default"
    )
)]
#![allow(
    clippy::panic,
    reason = "CG-021-no-panic-on-reachable-failure: case (b): 15 legacy trap-path panics \
              (unhandled exceptions, kernel faults, invalid page-fault codes, LoongArch unaligned \
              emulation) across four architectures need a recovery-policy review rather than \
              per-site judgement; new code remains reviewed"
)]

#[macro_use]
extern crate log;

#[macro_use]
extern crate memaddr;

macro_rules! impl_user_context_deref {
    ($target:ty, $field:tt) => {
        impl core::ops::Deref for UserContext {
            type Target = $target;

            fn deref(&self) -> &Self::Target {
                &self.$field
            }
        }

        impl core::ops::DerefMut for UserContext {
            fn deref_mut(&mut self) -> &mut Self::Target {
                &mut self.$field
            }
        }
    };
}

#[macro_use]
pub mod excp;

mod active_exception_context;

pub use active_exception_context::{
    ExceptionContextGuard, SuspendedExceptionContext, active_exception_context,
    in_exception_context, resume_active_exception_context, suspend_active_exception_context,
    with_active_exception_context,
};

mod userspace_common;

cfg_if::cfg_if! {
    if #[cfg(target_arch = "x86_64")] {
        mod x86_64;
        pub use self::x86_64::*;
    } else if #[cfg(any(target_arch = "riscv32", target_arch = "riscv64"))] {
        mod riscv;
        pub use self::riscv::*;
    } else if #[cfg(target_arch = "aarch64")]{
        mod aarch64;
        pub use self::aarch64::*;
    } else if #[cfg(any(target_arch = "loongarch64"))] {
        mod loongarch64;
        pub use self::loongarch64::*;
    }
}
