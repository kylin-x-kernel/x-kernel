// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Private metadata storage and PFN lookup strategy selection.
//!
//! A backend initializes from normalized physical ranges, reserving storage via
//! free_ranges::with_reserved_storage and mapping only that storage. It captures coverage before any
//! reservation, initializes PageMeta values and marks its own backing permanent.
//! Its Table provides lookup and covered_pages; global publication belongs
//! to the common layer. Index geometry and storage planning stay backend-local.

mod segments;

pub(crate) use segments::{Table, init};
