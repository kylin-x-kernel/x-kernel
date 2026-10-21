// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

mod access;
mod cache;
mod io;
mod slot;
mod sync;

#[cfg(unittest)]
unittest::path_mod_test!(tests, "../../ut-tests/buffer/tests.rs");

pub(crate) use access::{MetadataBuffer, MetadataWriteAccess};
#[cfg(unittest)]
use cache::MetadataBlockCache;
pub(crate) use io::Ext4MetadataIo;
#[cfg(unittest)]
use slot::MetadataBufferState;
