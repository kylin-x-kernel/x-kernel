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

#[cfg(unittest)]
mod tests {
    use alloc::sync::Arc;

    use super::*;

    #[unittest::def_test]
    fn initial_stdio_shares_read_write_console() {
        let fs = fs_context::init_fs().lock().clone_for_process();
        let mut fds = FdTable::default();
        add_stdio(&mut fds, &fs).unwrap();
        assert_eq!(fds.count(), 3);
        let stdin = fds.get_file(0).unwrap();
        assert!(stdin.is_stream(), "console I/O must not serialize on f_pos");
        assert!(
            stdin
                .mode()
                .contains(kvfs::FMode::READ | kvfs::FMode::WRITE)
        );
        for fd in [1, 2] {
            assert!(Arc::ptr_eq(&stdin, &fds.get_file(fd).unwrap()));
        }
    }
}
