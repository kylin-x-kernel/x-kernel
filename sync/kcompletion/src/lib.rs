// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Allocation-free synchronous completion and predicate broadcast waiting.
//!
//! Hosts supply a borrowed [`SyncWake`] and a matching parker; this crate owns
//! private pinned stack registrations without task or poll dependencies.

#![no_std]
#![deny(missing_docs)]

extern crate alloc;

mod completion;

pub use completion::{Completion, DeferredWake, SyncWake, WaitQueue};
