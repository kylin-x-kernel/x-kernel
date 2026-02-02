// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![cfg_attr(not(test), no_std)]
#![doc = include_str!("../README.md")]
#![warn(missing_docs)]
#![allow(rustdoc::broken_intra_doc_links)]

//! # Architecture
//!
//! The crate is organized into three main components:
//!
//! ## Guards (`guard` module)
//!
//! RAII guards that manage critical sections:
//! - [`NoOp`]: No protection (for IRQ-disabled contexts)
//! - [`NoPreempt`]: Disables kernel preemption
//! - [`IrqSave`]: Saves/restores IRQ state
//! - [`NoPreemptIrqSave`]: Disables both preemption and IRQs
//!
//! ## Locks (`lock` module)
//!
//! Generic spinlock implementation [`SpinLock<G, T>`] parameterized
//! by guard type.
//!
//! ## Type Aliases
//!
//! Convenient aliases for common lock types:
//! - [`SpinRaw`]: Lock with no guards
//! - [`SpinNoPreempt`]: Lock with preemption disabled
//! - [`SpinNoIrq`]: Lock with IRQs and preemption disabled
//!
//! # Feature Flags
//!
//! - `smp`: Enable for multi-core systems (adds atomic lock state)
//! - `preempt`: Enable preemption control (requires implementing [`KernelGuardIf`])
//!
//! # Usage Patterns
//!
//! ## Basic Usage
//!
//! ```rust,ignore
//! use kspin::SpinNoIrq;
//!
//! static COUNTER: SpinNoIrq<u32> = SpinNoIrq::new(0);
//!
//! fn increment() {
//!     let mut count = COUNTER.lock();
//!     *count += 1;
//! }
//! ```
//!
//! ## Interrupt Context
//!
//! ```rust,ignore
//! use kspin::SpinNoIrq;
//!
//! static DATA: SpinNoIrq<Vec<u8>> = SpinNoIrq::new(Vec::new());
//!
//! fn irq_handler() {
//!     // Safe to use in IRQ context
//!     let mut data = DATA.lock();
//!     data.push(42);
//! }
//! ```
//!
//! ## Implementing KernelGuardIf
//!
//! ```rust,ignore
//! use kspin::KernelGuardIf;
//!
//! struct MyKernelGuard;
//!
//! #[crate_interface::impl_interface]
//! impl KernelGuardIf for MyKernelGuard {
//!     fn enable_preempt() {
//!         // Your implementation
//!     }
//!
//!     fn disable_preempt() {
//!         // Your implementation
//!     }
//!
//!     fn save_disable() -> usize {
//!         // Your implementation
//!         0
//!     }
//!
//!     fn restore(flags: usize) {
//!         // Your implementation
//!     }
//! }
//! ```

extern crate alloc;

mod guard;
mod lock;
mod tests;

pub use guard::{BaseGuard, IrqSave, KernelGuardIf, NoOp, NoPreempt, NoPreemptIrqSave};
pub use lock::{SpinLock, SpinLockGuard};

/// Raw spinlock with no guards.
///
/// **Warning**: Must only be used in contexts where preemption and IRQs
/// are already disabled.
pub type SpinRaw<T> = SpinLock<NoOp, T>;

/// Guard for [`SpinRaw`].
pub type SpinRawGuard<'a, T> = SpinLockGuard<'a, NoOp, T>;

/// Spinlock that disables preemption.
///
/// Suitable for use in IRQ-disabled contexts or when IRQ handlers
/// don't access the same data.
pub type SpinNoPreempt<T> = SpinLock<NoPreempt, T>;

/// Guard for [`SpinNoPreempt`].
pub type SpinNoPreemptGuard<'a, T> = SpinLockGuard<'a, NoPreempt, T>;

/// Spinlock that disables IRQs and preemption.
///
/// This is the safest option and can be used from any context
/// including interrupt handlers.
pub type SpinNoIrq<T> = SpinLock<NoPreemptIrqSave, T>;

/// Guard for [`SpinNoIrq`].
pub type SpinNoIrqGuard<'a, T> = SpinLockGuard<'a, NoPreemptIrqSave, T>;
