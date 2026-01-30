// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! This module defines the generic abstraction layer for ARM interrupt management.
//! The interfaces provided here are architecturally neutral, ensuring compatibility
//! with various GIC implementations and facilitating seamless integration of
//! additional controller versions as requirements evolve.
//!
//! # Example
//!
//! ```
//! use arm_gic::{
//!     gicv3::{GicV3, IntId, SgiTarget},
//!     irq_enable,
//! };
//!
//! // Define MMIO base addresses for the distributor and redistributor interfaces.
//! const PLAT_DIST_BASE: *mut u64 = 0x800_0000 as _;
//! const PLAT_REDIST_BASE: *mut u64 = 0x80A_0000 as _;
//!
//! // Initialize the Interrupt Controller (INTC) hardware instance.
//! let mut intc_dev = unsafe { GicV3::new(PLAT_DIST_BASE, PLAT_REDIST_BASE) };
//! intc_dev.setup();
//!
//! // Prepare a Software Generated Interrupt (SGI) with ID 3.
//! let target_irq = IntId::sgi(3);
//!
//! // Configure priority masking and set individual IRQ priority levels.
//! GicV3::set_prio_mask(0xff);
//! intc_dev.set_interrupt_priority(target_irq, 0x80);
//!
//! // Enable the specific interrupt line and unmask processor-local interrupts.
//! intc_dev.enable_interrupt(target_irq, true);
//! irq_enable();
//!
//! // Dispatch the SGI to the current processor core.
//! GicV3::send_sgi(
//!     target_irq,
//!     SgiTarget::List {
//!         affinity3: 0,
//!         affinity2: 0,
//!         affinity1: 0,
//!         target_list: 0b1,
//!     },
//! );
//! ```

#![no_std]

pub mod gicv3;
mod sysreg;

use core::arch::asm;

/// Disables debug, SError, IRQ and FIQ exceptions.
pub fn irq_disable() {
    // Safe because writing to this system register doesn't access memory in any way.
    unsafe {
        asm!("msr DAIFSet, #0xf", options(nomem, nostack));
    }
}

/// Enables debug, SError, IRQ and FIQ exceptions.
pub fn irq_enable() {
    // Safe because writing to this system register doesn't access memory in any way.
    unsafe {
        asm!("msr DAIFClr, #0xf", options(nomem, nostack));
    }
}

/// Waits for an interrupt.
pub fn wfi() {
    // Safe because this doesn't access memory in any way.
    unsafe {
        asm!("wfi", options(nomem, nostack));
    }
}
