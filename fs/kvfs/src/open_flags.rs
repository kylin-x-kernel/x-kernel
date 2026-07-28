// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! File open flag normalization.

use linux_raw_sys::general::{
    FASYNC, O_ACCMODE, O_APPEND, O_CLOEXEC, O_CREAT, O_DIRECT, O_DIRECTORY, O_DSYNC, O_EXCL,
    O_LARGEFILE, O_NOATIME, O_NOCTTY, O_NOFOLLOW, O_NONBLOCK, O_PATH, O_RDONLY, O_RDWR, O_SYNC,
    O_TRUNC, O_WRONLY,
};

use crate::{LookupFlags, NodePermission, NodeType, Permission, Umode, VfsError, VfsResult};

bitflags::bitflags! {
    /// Normalized Linux `O_*` bits used inside the VFS open path.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct OpenFlags: u32 {
        /// Open for writing only.
        const WRITE_ONLY = O_WRONLY;
        /// Open for reading and writing.
        const READ_WRITE = O_RDWR;
        /// Create the file when it does not exist.
        const CREATE = O_CREAT;
        /// Require exclusive creation.
        const EXCLUSIVE = O_EXCL;
        /// Do not assign a controlling terminal.
        const NO_CONTROLLING_TTY = O_NOCTTY;
        /// Truncate a regular file after opening it.
        const TRUNCATE = O_TRUNC;
        /// Append writes to the end of the file.
        const APPEND = O_APPEND;
        /// Use nonblocking I/O semantics.
        const NONBLOCK = O_NONBLOCK;
        /// Request data-integrity synchronized writes.
        const DSYNC = O_DSYNC;
        /// Enable signal-driven I/O.
        const ASYNC = FASYNC;
        /// Request direct I/O.
        const DIRECT = O_DIRECT;
        /// Allow large-file offsets.
        const LARGE_FILE = O_LARGEFILE;
        /// Require the opened object to be a directory.
        const DIRECTORY = O_DIRECTORY;
        /// Do not follow the final symbolic link.
        const NO_FOLLOW = O_NOFOLLOW;
        /// Do not update access time.
        const NO_ATIME = O_NOATIME;
        /// Close the descriptor during exec.
        const CLOSE_ON_EXEC = O_CLOEXEC;
        /// Request file-integrity synchronized writes.
        const SYNC = O_SYNC;
        /// Open only a path reference.
        const PATH = O_PATH;
    }

    /// Namei intent bits derived from open flags.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub(crate) struct OpenIntent: u32 {
        const OPEN = 1 << 16;
        const CREATE = 1 << 17;
        const EXCL = 1 << 18;
    }
}

const O_PATH_FLAGS: OpenFlags = OpenFlags::DIRECTORY
    .union(OpenFlags::NO_FOLLOW)
    .union(OpenFlags::PATH)
    .union(OpenFlags::CLOSE_ON_EXEC);
const VALID_OPEN_FLAGS: u32 = OpenFlags::all().bits();

fn is_create_like_open(flags: OpenFlags) -> bool {
    flags.contains(OpenFlags::CREATE)
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
            mode,
        };
        if OpenFlags::from_bits_retain(how.flags as u32).contains(OpenFlags::PATH) {
            how.flags &= u64::from(O_PATH_FLAGS.bits());
        }
        if !is_create_like_open(OpenFlags::from_bits_retain(how.flags as u32)) {
            how.mode = NodePermission::empty();
        }
        how
    }

    /// Converts raw open arguments into normalized parameters for namei.
    pub(crate) fn into_open_params(self) -> VfsResult<OpenParams> {
        let raw_flags = u32::try_from(self.flags).map_err(|_| VfsError::InvalidInput)?;
        let mut acc_mode = AccMode::from_access_mode(raw_flags & O_ACCMODE);

        let mut flags = OpenFlags::from_bits(raw_flags).ok_or(VfsError::InvalidInput)?;

        flags.remove(OpenFlags::CLOSE_ON_EXEC);

        let mode = if is_create_like_open(flags) {
            Umode::new(NodeType::RegularFile, self.mode)
        } else {
            if !self.mode.is_empty() {
                return Err(VfsError::InvalidInput);
            }
            Umode::new(NodeType::RegularFile, NodePermission::empty())
        };

        if flags.contains(OpenFlags::DIRECTORY | OpenFlags::CREATE) {
            return Err(VfsError::InvalidInput);
        }

        if flags.contains(OpenFlags::PATH) {
            if !O_PATH_FLAGS.contains(flags) {
                return Err(VfsError::InvalidInput);
            }
            acc_mode = AccMode::empty();
        }

        if flags.contains(OpenFlags::SYNC) {
            flags.insert(OpenFlags::DSYNC);
        }
        if flags.contains(OpenFlags::TRUNCATE) {
            acc_mode.insert_write();
        }
        if flags.contains(OpenFlags::APPEND) {
            acc_mode.insert_append();
        }

        let mut intent = if flags.contains(OpenFlags::PATH) {
            OpenIntent::empty()
        } else {
            OpenIntent::OPEN
        };
        if flags.contains(OpenFlags::CREATE) {
            intent |= OpenIntent::CREATE;
            if flags.contains(OpenFlags::EXCLUSIVE) {
                intent |= OpenIntent::EXCL;
                flags.insert(OpenFlags::NO_FOLLOW);
            }
        }

        let mut lookup_flags = LookupFlags::empty();
        if flags.contains(OpenFlags::DIRECTORY) {
            lookup_flags |= LookupFlags::DIRECTORY;
        }
        if !flags.contains(OpenFlags::NO_FOLLOW) {
            lookup_flags |= LookupFlags::FOLLOW_FINAL;
        }

        Ok(OpenParams {
            flags,
            mode,
            acc_mode,
            intent,
            lookup_flags,
        })
    }
}

/// Open parameters consumed by namei.
#[derive(Debug, Clone)]
pub(crate) struct OpenParams {
    flags: OpenFlags,
    mode: Umode,
    acc_mode: AccMode,
    intent: OpenIntent,
    lookup_flags: LookupFlags,
}

impl OpenParams {
    pub(crate) const fn will_create(&self) -> bool {
        self.intent.contains(OpenIntent::CREATE)
    }

    pub(crate) const fn is_exclusive_create(&self) -> bool {
        self.intent.contains(OpenIntent::EXCL)
    }

    pub(crate) const fn mode(&self) -> Umode {
        self.mode
    }

    pub(crate) const fn lookup_flags(&self) -> LookupFlags {
        self.lookup_flags
    }

    pub(crate) const fn is_path(&self) -> bool {
        self.flags.contains(OpenFlags::PATH)
    }

    pub(crate) const fn file_flags(&self) -> OpenFlags {
        self.flags
    }

    pub(crate) fn may_open_args(&self, was_created: bool) -> (OpenFlags, AccMode) {
        let mut open_flags = self.flags;
        let mut acc_mode = self.acc_mode;
        if was_created {
            open_flags.remove(OpenFlags::TRUNCATE);
            acc_mode = AccMode::empty();
        }
        (open_flags, acc_mode)
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

    pub(crate) const fn requires_write(self) -> bool {
        self.0.contains(Permission::MAY_WRITE)
    }

    pub(crate) const fn permission(self) -> Permission {
        self.0
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::*;

    #[def_test]
    fn exclusive_without_create_does_not_set_create_intent() {
        let flags = OpenHow::from_legacy(O_EXCL, NodePermission::empty())
            .into_open_params()
            .unwrap();

        assert!(!flags.will_create());
        assert!(!flags.is_exclusive_create());
    }

    #[def_test]
    fn exclusive_create_sets_intent_and_disables_final_symlink_following() {
        let flags = OpenHow::from_legacy(O_CREAT | O_EXCL, NodePermission::empty())
            .into_open_params()
            .unwrap();

        assert!(flags.will_create());
        assert!(flags.is_exclusive_create());
        assert!(!flags.lookup_flags().follows_final());
        assert!(flags.flags.contains(OpenFlags::NO_FOLLOW));
    }
}
