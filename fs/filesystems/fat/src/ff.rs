// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Type aliases for `fatfs`.

use fatfs::{DefaultTimeProvider, LossyOemCpConverter};

use crate::FatDisk;

pub(crate) type FileSystem = fatfs::FileSystem<FatDisk, DefaultTimeProvider, LossyOemCpConverter>;

pub(crate) type Dir<'a> = fatfs::Dir<'a, FatDisk, DefaultTimeProvider, LossyOemCpConverter>;

pub(crate) type DirEntry<'a> =
    fatfs::DirEntry<'a, FatDisk, DefaultTimeProvider, LossyOemCpConverter>;

pub(crate) type File<'a> = fatfs::File<'a, FatDisk, DefaultTimeProvider, LossyOemCpConverter>;
