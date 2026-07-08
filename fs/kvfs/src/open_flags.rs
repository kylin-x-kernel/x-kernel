// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! File open flag normalization.

use linux_raw_sys::general::{
    __O_SYNC, FASYNC, O_ACCMODE, O_APPEND, O_CLOEXEC, O_CREAT, O_DIRECT, O_DIRECTORY, O_DSYNC,
    O_EXCL, O_LARGEFILE, O_NDELAY, O_NOATIME, O_NOCTTY, O_NOFOLLOW, O_NONBLOCK, O_PATH, O_RDONLY,
    O_RDWR, O_SYNC, O_TRUNC, O_WRONLY,
};

use crate::{LookupFlags, NodePermission, NodeType, Permission, Umode, VfsError, VfsResult};

bitflags::bitflags! {
    /// Namei intent bits derived from open flags.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub(crate) struct OpenIntent: u32 {
        const OPEN = 1 << 16;
        const CREATE = 1 << 17;
        const EXCL = 1 << 18;
    }
}

const O_PATH_FLAGS: u32 = O_DIRECTORY | O_NOFOLLOW | O_PATH | O_CLOEXEC;
const VALID_OPEN_FLAGS: u32 = O_RDONLY
    | O_WRONLY
    | O_RDWR
    | O_CREAT
    | O_EXCL
    | O_NOCTTY
    | O_TRUNC
    | O_APPEND
    | O_NDELAY
    | O_NONBLOCK
    | __O_SYNC
    | O_DSYNC
    | FASYNC
    | O_DIRECT
    | O_LARGEFILE
    | O_DIRECTORY
    | O_NOFOLLOW
    | O_NOATIME
    | O_CLOEXEC
    | O_PATH;

fn is_create_like_open(flags: u32) -> bool {
    flags & O_CREAT != 0
}

/// Raw open arguments after legacy syscall cleanup.
#[derive(Clone, Copy, Debug)]
pub(crate) struct OpenHow {
    pub(crate) flags: u64,
    pub(crate) mode: NodePermission,
}

impl OpenHow {
    /// Builds open arguments from legacy `open`/`openat` syscall inputs.
    pub(crate) fn from_legacy(flags: u32, mode: NodePermission) -> Self {
        let mut how = Self {
            flags: u64::from(flags & VALID_OPEN_FLAGS),
            mode: mode.valid_mode_bits(),
        };
        if how.flags as u32 & O_PATH != 0 {
            how.flags &= u64::from(O_PATH_FLAGS);
        }
        if !is_create_like_open(how.flags as u32) {
            how.mode = NodePermission::empty();
        }
        how
    }

    /// Converts raw open arguments into normalized parameters for namei.
    pub(crate) fn into_open_flags(self) -> VfsResult<OpenFlags> {
        let mut flags = u32::try_from(self.flags).map_err(|_| VfsError::InvalidInput)?;
        let mut acc_mode = AccMode::from_access_mode(flags & O_ACCMODE);

        if flags & !VALID_OPEN_FLAGS != 0 {
            return Err(VfsError::InvalidInput);
        }

        flags &= !O_CLOEXEC;

        let mode = if is_create_like_open(flags) {
            Umode::new(NodeType::RegularFile, self.mode)
        } else {
            if !self.mode.is_empty() {
                return Err(VfsError::InvalidInput);
            }
            Umode::new(NodeType::RegularFile, NodePermission::empty())
        };

        if (flags & (O_DIRECTORY | O_CREAT)) == (O_DIRECTORY | O_CREAT) {
            return Err(VfsError::InvalidInput);
        }

        if flags & O_PATH != 0 {
            if flags & !(O_PATH_FLAGS & !O_CLOEXEC) != 0 {
                return Err(VfsError::InvalidInput);
            }
            acc_mode = AccMode::empty();
        }

        if flags & O_SYNC != 0 {
            flags |= O_DSYNC;
        }
        if flags & O_TRUNC != 0 {
            acc_mode.insert_write();
        }
        if flags & O_APPEND != 0 {
            acc_mode.insert_append();
        }

        let mut intent = if flags & O_PATH != 0 {
            OpenIntent::empty()
        } else {
            OpenIntent::OPEN
        };
        if flags & O_CREAT != 0 {
            intent |= OpenIntent::CREATE;
            if flags & O_EXCL != 0 {
                intent |= OpenIntent::EXCL;
                flags |= O_NOFOLLOW;
            }
        }

        let mut lookup_flags = LookupFlags::empty();
        if flags & O_DIRECTORY != 0 {
            lookup_flags |= LookupFlags::DIRECTORY;
        }
        if flags & O_NOFOLLOW == 0 {
            lookup_flags |= LookupFlags::FOLLOW_FINAL;
        }

        Ok(OpenFlags {
            open_flag: flags,
            mode,
            acc_mode,
            intent,
            lookup_flags,
        })
    }
}

/// Open parameters consumed by namei.
#[derive(Debug, Clone)]
pub(crate) struct OpenFlags {
    pub(crate) open_flag: u32,
    pub(crate) mode: Umode,
    pub(crate) acc_mode: AccMode,
    pub(crate) intent: OpenIntent,
    pub(crate) lookup_flags: LookupFlags,
}

impl OpenFlags {
    pub(crate) const fn will_create(&self) -> bool {
        self.intent.bits() & OpenIntent::CREATE.bits() != 0
    }

    pub(crate) const fn is_exclusive_create(&self) -> bool {
        self.intent.bits() & OpenIntent::EXCL.bits() != 0
    }

    pub(crate) fn may_open_args(&self, was_created: bool) -> (u32, AccMode) {
        let mut open_flag = self.open_flag;
        let mut acc_mode = self.acc_mode;
        if was_created {
            open_flag &= !O_TRUNC;
            acc_mode = AccMode::empty();
        }
        (open_flag, acc_mode)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AccMode(Permission);

impl AccMode {
    pub(crate) const fn empty() -> Self {
        Self(Permission::empty())
    }

    fn from_access_mode(access_mode: u32) -> Self {
        match access_mode {
            O_RDONLY => Self(Permission::MAY_READ),
            O_WRONLY => Self(Permission::MAY_WRITE),
            O_RDWR => Self(Permission::MAY_READ | Permission::MAY_WRITE),
            _ => Self(Permission::MAY_READ | Permission::MAY_WRITE),
        }
    }

    fn insert_write(&mut self) {
        self.0 |= Permission::MAY_WRITE;
    }

    fn insert_append(&mut self) {
        self.0 |= Permission::MAY_APPEND;
    }

    pub(crate) fn requires_write(self) -> bool {
        self.0.contains(Permission::MAY_WRITE)
    }
}
