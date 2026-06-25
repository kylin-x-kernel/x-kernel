// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Anonymous VM object ownership and lineage.
//!
//! This crate owns the Linux-aligned anonymous object boundary:
//!
//! - shared anonymous objects with stable object identity;
//! - private anonymous objects with stable object identity;
//! - private anonymous lineage used by fork/COW families.
//!
//! It does not implement fault handling or page-table mutation itself.
//! Those remain in `mm/memspace` runtime code; this crate only owns the
//! anonymous object identities, object-side mapped views, and lineage
//! relationships they consume.
#![no_std]

extern crate alloc;

mod ids;
mod private;
mod shared;

pub use ids::AnonLineageId;
pub use private::{
    AnonPrivateForkPage, AnonPrivateObject, AnonPrivatePageCommitError, AnonPrivatePageHandle,
    AnonPrivateReleasedPage, AnonPrivateViewGuard, DetachedAnonPrivatePages,
    PreparedAnonPrivateFork, PreparedAnonPrivatePage,
};
pub use shared::{AnonSharedObject, AnonSharedViewGuard};
