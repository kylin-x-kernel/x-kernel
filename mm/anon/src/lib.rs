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
//! relationships they consume. Private page identities own typed physical
//! backing; logical slot guards track COW sharing separately from observer and
//! managed-PTE holds. Runtime callers serialize reuse/fork under the owning-MM
//! mutex and retain removed logical shares through old-access retirement.
#![no_std]

extern crate alloc;

mod ids;
mod private;
mod shared;

pub use ids::AnonLineageId;
pub use private::{
    AnonPrivateForkPage, AnonPrivateObject, AnonPrivatePageCommitError, AnonPrivatePageHandle,
    AnonPrivateViewGuard, PreparedAnonPrivateFork, PreparedAnonPrivatePage, RetiredAnonPrivatePage,
};
pub use shared::{AnonSharedObject, AnonSharedViewGuard};
