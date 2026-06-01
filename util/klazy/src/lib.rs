// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `no_std`-compatible synchronization primitives for one-time initialization and
//! lazy evaluation.
//!
//! Provides [`Once<T>`] and [`Lazy<T, F>`], the foundational building blocks for
//! static lazy initialization of kernel services (tracing, memory management,
//! TEE, cryptography, etc.).
//!
//! Unlike their `std::sync` counterparts, these types work in bare-metal `no_std`
//! environments without depending on OS threads or condition variables. Spin-wait
//! is used for coordination during initialization.
//!
//! For detailed design rationale and security analysis, see `docs/design.md` and
//! `docs/security.md` in the crate source directory.

#![cfg_attr(not(test), no_std)]

mod lazy;
mod once;
pub use lazy::Lazy;
pub use once::Once;
