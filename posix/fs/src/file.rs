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
use linux_raw_sys::general::O_RDWR;

/// Install stdin, stdout, and stderr sharing one read-write `/dev/console` file.
pub fn add_stdio(fd_table: &mut FdTable, fs_struct: &FsStruct) -> KResult<()> {
    assert_eq!(fd_table.count(), 0);

    // Pagers can read commands from stderr when stdin carries piped text.
    // Match Linux's initial console setup rather than restricting fd directions.
    let console = Filename::new("/dev/console").open_with_flags_at(
        fs_struct.root(),
        fs_struct.pwd(),
        O_RDWR as _,
        NodePermission::empty(),
        NodePermission::empty(),
        kcred::initial_cred(),
    )?;
    for _ in 0..3 {
        fd_table
            .insert_file(console.clone(), false)
            .map_err(|_| KError::TooManyOpenFiles)?;
    }

    Ok(())
}

unittest::path_mod_test!(tests, "../ut-tests/file.rs");
