// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Filesystem-local file helper utilities owned by `posix-fs`.
//!
//! Fd-backed kernel objects with their own object state live in owner crates.
//! This module keeps only filesystem-local helpers like stdio installation.

use fs_context::FsStruct;
use kerrno::{KError, KResult};
use kfd::FdTable;
use kvfs::{Filename, NodePermission};
use linux_raw_sys::general::{O_RDONLY, O_WRONLY};

/// Add stdin, stdout, and stderr backed by `/dev/console`.
pub fn add_stdio(fd_table: &mut FdTable, fs_struct: &FsStruct) -> KResult<()> {
    assert_eq!(fd_table.count(), 0);

    let open = |flags| {
        Filename::new("/dev/console").open_with_flags_at(
            fs_struct.root(),
            fs_struct.pwd(),
            flags,
            NodePermission::empty(),
            kcred::initial_cred(),
        )
    };

    let tty_in = open(O_RDONLY as _)?;
    let tty_out = open(O_WRONLY as _)?;
    fd_table
        .insert_file(tty_in, false)
        .map_err(|_| KError::TooManyOpenFiles)?;
    fd_table
        .insert_file(tty_out.clone(), false)
        .map_err(|_| KError::TooManyOpenFiles)?;
    fd_table
        .insert_file(tty_out, false)
        .map_err(|_| KError::TooManyOpenFiles)?;

    Ok(())
}
