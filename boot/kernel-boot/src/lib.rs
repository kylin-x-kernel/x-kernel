// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unified position-independent boot layer for x-kernel (AArch64 support).

#![cfg_attr(target_os = "none", no_std)]
#![cfg(target_os = "none")]

pub use linkme::{distributed_slice as def_boot_init, distributed_slice as register_boot_init};

use crate::arch::LogicalCpuId;

#[def_boot_init]
pub static PRIMARY_KERNEL_ENTRY: [fn(usize) -> !];

#[def_boot_init]
pub static SECOND_KERNEL_ENTRY: [fn(LogicalCpuId) -> !];

macro_rules! call_kernel_entry {
    ($entry:ident, $($args:tt)*) => {{
        let mut iter = $crate::$entry.iter();
        if let Some(func) = iter.next() {
            func($($args)*)
        }
    }}
}

pub mod arch;
pub use boot_info as bootinfo;
pub mod bootconsole;
pub(crate) mod bootconsole_config;
pub mod size_const;

#[macro_export]
macro_rules! bootlog {
    ($($arg:tt)*) => {{
        $crate::bootconsole::log(::core::format_args!($($arg)*));
    }};
}

#[macro_export]
macro_rules! bootln {
    () => {{
        $crate::bootconsole::write_str("\n");
    }};
    ($($arg:tt)*) => {{
        $crate::bootconsole::log(::core::format_args!($($arg)*));
        $crate::bootconsole::write_str("\n");
    }};
}
