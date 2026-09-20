// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

mod access;
mod cache;
mod io;
mod slot;
mod sync;

#[cfg(unittest)]
mod tests;

pub(crate) use access::{MetadataBuffer, MetadataWriteAccess};
#[cfg(unittest)]
use cache::MetadataBlockCache;
pub(crate) use io::Ext4MetadataIo;
#[cfg(unittest)]
use slot::MetadataBufferState;
