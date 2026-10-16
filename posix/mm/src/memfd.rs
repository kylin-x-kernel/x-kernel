// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Memory file descriptor syscalls.

use core::ffi::c_char;

use kerrno::{KError, KResult};
use linux_raw_sys::general::{MFD_ALLOW_SEALING, MFD_CLOEXEC};
use memfs::shmem::create_memfd_file;
use posix_types::UserConstPtr;

const MEMFD_NAME_MAX_LEN: usize = 249;

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct MemfdFlags: u32 {
        const CLOEXEC = MFD_CLOEXEC;
        const ALLOW_SEALING = MFD_ALLOW_SEALING;
    }
}

impl MemfdFlags {
    fn from_raw(bits: u32) -> KResult<Self> {
        Self::from_bits(bits).ok_or(KError::InvalidInput)
    }

    fn is_cloexec(self) -> bool {
        self.contains(Self::CLOEXEC)
    }

    fn allows_sealing(self) -> bool {
        self.contains(Self::ALLOW_SEALING)
    }
}

/// Creates an anonymous in-memory file descriptor.
///
/// This creates a private tmpfs-backed file object whose content is owned by an
/// inode-backed `kvfs::AddressSpace`, similar to `shmem_file_setup()` in
/// Linux `mm/shmem.c`. The user name is a debug label, so slash characters are
/// accepted and escaped before creating the private backing-file component.
pub fn sys_memfd_create(name: UserConstPtr<c_char>, flags: u32) -> KResult<isize> {
    let flags = MemfdFlags::from_raw(flags)?;
    let display_name = name.load_string_with_max_len(MEMFD_NAME_MAX_LEN)?;
    let cred = kprocess::current_cred();
    let file = create_memfd_file(
        &format_memfd_name(&display_name),
        flags.allows_sealing(),
        cred.clone(),
    )?
    .into_file(cred)?;
    kprocess::current_files()
        .add_file(file, flags.is_cloexec())
        .map(|fd| fd as _)
}

fn format_memfd_name(name: &str) -> alloc::string::String {
    if name.is_empty() {
        return "memfd".into();
    }
    let mut formatted = alloc::string::String::from("memfd:");
    for ch in name.chars() {
        formatted.push(if ch == '/' { ':' } else { ch });
    }
    formatted
}

unittest::path_mod_test!(tests, "../ut-tests/memfd.rs");
