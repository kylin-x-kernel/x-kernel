// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Type aliases for `fatfs`.

use fatfs::{DefaultTimeProvider, LossyOemCpConverter};

use crate::disk::SeekableDisk;

pub type FileSystem = fatfs::FileSystem<SeekableDisk, DefaultTimeProvider, LossyOemCpConverter>;

pub type Dir<'a> = fatfs::Dir<'a, SeekableDisk, DefaultTimeProvider, LossyOemCpConverter>;

pub type DirEntry<'a> = fatfs::DirEntry<'a, SeekableDisk, DefaultTimeProvider, LossyOemCpConverter>;

pub type File<'a> = fatfs::File<'a, SeekableDisk, DefaultTimeProvider, LossyOemCpConverter>;
