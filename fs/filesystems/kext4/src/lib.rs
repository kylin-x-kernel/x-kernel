// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Checked ext4 storage primitives.
//!
//! The first implementation stage provides disk decoding, filesystem block
//! I/O, feature negotiation, and a read-only mount path. It deliberately does
//! not expose metadata mutation or journal replay.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod disk;
mod error;
mod io;
mod mount;
mod types;

pub use disk::{BlockGroupDescriptor, FeatureSet, Superblock};
pub use error::{ChecksumTarget, CorruptKind, Ext4Error, Ext4Result, FeatureClass};
pub use io::FilesystemDevice;
pub use mount::{FilesystemLayout, ReadOnlyFilesystem};
pub use types::{BlockGroupNumber, FilesystemBlock};
