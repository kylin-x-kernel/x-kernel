// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unified position-independent boot layer for x-kernel (AArch64 support).

#![cfg_attr(target_os = "none", no_std)]

use kcpu_id_map::LogicalCpuId;
pub use linkme::{distributed_slice as def_boot_init, distributed_slice as register_boot_init};

#[def_boot_init]
pub static PRIMARY_KERNEL_ENTRY: [fn(usize) -> !];

#[def_boot_init]
pub static SECOND_KERNEL_ENTRY: [fn(LogicalCpuId) -> !];

#[cfg(target_os = "none")]
macro_rules! call_kernel_entry {
    ($entry:ident, $($args:tt)*) => {{
        let mut iter = $crate::$entry.iter();
        if let Some(func) = iter.next() {
            func($($args)*)
        }
    }}
}

#[cfg(target_os = "none")]
pub mod arch;

// Dummy boot-entry symbols for doc generation on the host target.
// The real definitions live in arch-specific entry.rs files gated behind
// `#[cfg(target_os = "none")]`, so they are absent when `cargo doc --workspace`
// compiles all platform crates on the host.
#[cfg(not(target_os = "none"))]
pub mod arch {
    use kcpu_id_map::LogicalCpuId;

    pub fn _start_secondary() -> ! {
        unreachable!("arch::_start_secondary should never be called on the host target");
    }

    pub fn set_secondary_boot_context(_logical_cpu_id: LogicalCpuId, _stack_top_paddr: usize) {}
}

pub use boot_info as bootinfo;
#[cfg(target_os = "none")]
pub mod bootconsole;
#[cfg(target_os = "none")]
pub(crate) mod bootconsole_config;
pub mod size_const;

// Real macros for kernel targets

#[cfg(target_os = "none")]
#[macro_export]
macro_rules! bootlog {
    ($($arg:tt)*) => {{
        $crate::bootconsole::log(::core::format_args!($($arg)*));
    }};
}

#[cfg(target_os = "none")]
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

// Stub macros for host / doc builds

#[cfg(not(target_os = "none"))]
#[macro_export]
macro_rules! bootlog {
    ($($arg:tt)*) => {{ let _ = ::core::format_args!($($arg)*); }};
}

#[cfg(not(target_os = "none"))]
#[macro_export]
macro_rules! bootln {
    () => {{}};
    ($($arg:tt)*) => {{ let _ = ::core::format_args!($($arg)*); }};
}
