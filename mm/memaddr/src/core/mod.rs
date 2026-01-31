// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Core address types and iterators.
mod addr;
mod iter;
mod range;

pub use self::addr::{AddrOps, MemoryAddr, PhysAddr, VirtAddr};
pub use self::iter::{DynPageIter, PageIter};
pub use self::range::{AddrRange, PhysAddrRange, VirtAddrRange};
