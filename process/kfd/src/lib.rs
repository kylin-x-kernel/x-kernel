// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process-local descriptor tables, stable file snapshots, and Linux metadata values.
//!
//! [`FdTable`] stores [`FileDescriptor`] entries referencing `kvfs::VfsFile`.
//! [`FdSnapshot`] retains an open file across descriptor changes; [`Kstat`] handles
//! metadata conversion. Shared tables need external locking, normally via kresources.
//!
//! # Example
//!
//! ```no_run
//! use kfd::FdTable;
//! let table = FdTable::new_shared();
//! assert_eq!(table.read().count(), 0);
//! let copied = FdTable::clone_shared_from(&table);
//! assert_eq!(copied.read().count(), 0);
//! ```

#![no_std]

extern crate alloc;

mod fd_slots;
mod fd_table;
mod fd_table_owner;
mod file_descriptor;
mod stat;

pub use self::{
    fd_table::FdTable,
    fd_table_owner::{FdTableOwner, FdTableRef},
    file_descriptor::{FdSnapshot, FileDescriptor},
    stat::Kstat,
};

unittest::path_mod_test!(tests, "../ut-tests/lib.rs");
