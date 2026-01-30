<<<<<<< HEAD
//! Generic page table abstractions and implementations.
=======
// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

>>>>>>> 62a4f63a (./init, io, mm, net, platforms, process, sync over)
#![cfg_attr(not(test), no_std)]

mod arch;
mod defs;
mod table64;

pub use arch::*;
pub use defs::*;
pub use table64::*;
