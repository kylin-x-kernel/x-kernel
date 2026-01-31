// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Address types and iterators with units.
mod addr;
mod iter;
mod range;

pub use self::{
    addr::{AddrOps, MemoryAddr, PhysAddr, VirtAddr},
    iter::{DynPageIter, PageIter},
    range::{AddrRange, PhysAddrRange, VirtAddrRange},
};
