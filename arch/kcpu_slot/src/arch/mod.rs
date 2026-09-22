// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Architecture-specific per-CPU base register access.
//!
//! Every supported architecture exposes the same two operations: reading the
//! base register installed for the current CPU, and installing a new one
//! during early CPU setup. Selection follows the same `cfg_select!` pattern
//! as `kcpu_id_map`: one arm per architecture exporting an `imp` alias, so
//! the rest of the crate stays architecture-agnostic.

cfg_select! {
    target_arch = "x86_64" => {
        pub(crate) mod x86_64;
        pub(crate) use self::x86_64 as imp;
    }
    target_arch = "aarch64" => {
        pub(crate) mod aarch64;
        pub(crate) use self::aarch64 as imp;
    }
    any(target_arch = "riscv32", target_arch = "riscv64") => {
        pub(crate) mod riscv;
        pub(crate) use self::riscv as imp;
    }
    target_arch = "loongarch64" => {
        pub(crate) mod loongarch64;
        pub(crate) use self::loongarch64 as imp;
    }
    not(test) => {
        compile_error!("kcpu_slot: unsupported architecture for per-CPU base register");
    }
    _ => {
        pub(crate) mod fallback;
        pub(crate) use self::fallback as imp;
    }
}

pub(crate) use imp::current_base;
#[cfg(not(test))]
pub(crate) use imp::set_current_base;
