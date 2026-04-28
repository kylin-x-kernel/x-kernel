// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared TEE-related task APIs (traits and helpers).
//!
//! # Cargo features
//!
//! - **`tee_ta_sign`** — Enables the `tasign` submodule (TA ELF signature verification),
//!   pulls in `tasign` / `kfs` / `kerrno` / `log`, and links `tasign-kernel-shim` for mbedTLS
//!   libc shims on bare-metal. Without this feature, `tasign` is not compiled.
//! - **`ta_verify_with_root`** — Uses an embedded CA PEM to verify certificate chain during
//!   TA signature verification (depends on `tee_ta_sign`).
#![no_std]

extern crate alloc;
#[cfg(feature = "tee_ta_sign")]
extern crate tasign_kernel_shim;

use core::any::Any;

pub mod ta_ctx;
#[cfg(feature = "tee_ta_sign")]
pub mod tasign;
pub mod tee_procfs;

pub use ta_ctx::{SessionIdentity, TeeTaCtx};

/// Tee session context trait.
///
/// Stored behind `dyn` and used for type-erased access, so it must provide
/// downcasting via `as_any`.
pub trait TeeSessionCtxTrait {
    /// Get the any reference of the tee session context.
    fn as_any(&self) -> &dyn Any;
    /// Get the any mutable reference of the tee session context.
    fn as_any_mut(&mut self) -> &mut dyn Any;
}
