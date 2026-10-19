// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Trap handling.

pub use linkme::{
    distributed_slice as def_trap_handler, distributed_slice as register_trap_handler,
};
use memaddr::VirtAddr;
pub use page_table::PagingFlags as PageFaultFlags;

pub use crate::TrapFrame;

/// A slice of IRQ handler functions.
#[def_trap_handler]
pub static IRQ: [fn(usize) -> bool];

/// A slice of NMI handler functions.
///
/// These fire when the taken IRQ exception is an NMI: either a hardware NMI
/// (GICv3.3 NMI attribute + FEAT_NMI) or a pseudo‑NMI (IRQ taken while
/// normal IRQs are masked by PMR).  Handlers must use the lock‑free
/// [`NMI`] dispatch path.
#[def_trap_handler]
pub static NMI: [fn(usize) -> bool];

/// A slice of page fault handler functions.
#[def_trap_handler]
pub static PAGE_FAULT: [fn(VirtAddr, PageFaultFlags) -> bool];

#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Trap dispatch macro is used by architecture-specific \
              exception handlers"
)]
#[allow(
    unused_macros,
    reason = "CG-024-lint-exceptions: unused_macros: Trap dispatch macro is used by \
              architecture-specific exception handlers"
)]
macro_rules! handle_trap {
    ($trap:ident, $($args:tt)*) => {{
        let mut iter = $crate::excp::$trap.iter();
        if let Some(func) = iter.next() {
            if iter.next().is_some() {
                warn!("Multiple handlers for trap {} are not currently supported", stringify!($trap));
            }
            func($($args)*)
        } else {
            warn!("No registered handler for trap {}", stringify!($trap));
            false
        }
    }}
}

unittest::path_mod_test!(pub mod tests_excp, "../ut-tests/excp.rs");
