// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Memory file descriptor syscalls.

use core::ffi::c_char;

use kerrno::{KError, KResult};
use kprocess::current_user_process;
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
/// inode-backed `pagecache::Mapping`, similar to `shmem_file_setup()` in
/// Linux `mm/shmem.c`.
pub fn sys_memfd_create(name: UserConstPtr<c_char>, flags: u32) -> KResult<isize> {
    let flags = MemfdFlags::from_raw(flags)?;
    let display_name = name.load_string_with_max_len(MEMFD_NAME_MAX_LEN)?;
    validate_memfd_name(&display_name)?;
    let cred = kprocess::current_cred();
    let file = create_memfd_file(
        &format_memfd_name(&display_name),
        flags.allows_sealing(),
        cred.clone(),
    )?
    .into_file(cred)?;
    current_user_process()
        .resources()?
        .add_file(file, flags.is_cloexec())
        .map(|fd| fd as _)
}

fn validate_memfd_name(name: &str) -> KResult<()> {
    if name.contains('/') {
        return Err(KError::InvalidInput);
    }
    Ok(())
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

#[cfg(unittest)]
mod tests {
    use linux_raw_sys::general::MFD_ALLOW_SEALING;
    use unittest::{assert_eq, def_test};

    use super::*;

    #[def_test]
    fn memfd_flags_accept_allow_sealing() {
        let flags = MemfdFlags::from_raw(MFD_CLOEXEC | MFD_ALLOW_SEALING).unwrap();

        assert!(flags.is_cloexec());
        assert!(flags.allows_sealing());
    }

    #[def_test]
    fn memfd_flags_reject_unknown_bits() {
        assert_eq!(MemfdFlags::from_raw(0x8000_0000), Err(KError::InvalidInput));
    }

    #[def_test]
    fn memfd_name_rejects_slash() {
        assert_eq!(validate_memfd_name("bad/name"), Err(KError::InvalidInput));
        assert_eq!(validate_memfd_name("good:name"), Ok(()));
    }
}
