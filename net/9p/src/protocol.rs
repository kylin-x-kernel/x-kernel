// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! 9P protocol constants and data types.

/// Special values used by the protocol.
pub(crate) const NO_FID: u32 = 0xFFFF_FFFF;
pub(crate) const NO_TAG: u16 = 0xFFFF;

pub(crate) const TVERSION: u8 = 100;
pub(crate) const RVERSION: u8 = 101;
pub(crate) const TATTACH: u8 = 104;
pub(crate) const RATTACH: u8 = 105;
pub(crate) const RERROR: u8 = 107;
pub(crate) const RLERROR: u8 = 7;
pub(crate) const TSTATFS: u8 = 8;
pub(crate) const RSTATFS: u8 = 9;
pub(crate) const TREMOVE: u8 = 122;
pub(crate) const RREMOVE: u8 = 123;
pub(crate) const TWALK: u8 = 110;
pub(crate) const RWALK: u8 = 111;
pub(crate) const TREAD: u8 = 116;
pub(crate) const RREAD: u8 = 117;
pub(crate) const TWRITE: u8 = 118;
pub(crate) const RWRITE: u8 = 119;
pub(crate) const TCLUNK: u8 = 120;
pub(crate) const RCLUNK: u8 = 121;
pub(crate) const TREADLINK: u8 = 22;
pub(crate) const RREADLINK: u8 = 23;
pub(crate) const TLINK: u8 = 70;
pub(crate) const RLINK: u8 = 71;
pub(crate) const TMKDIR: u8 = 72;
pub(crate) const RMKDIR: u8 = 73;
pub(crate) const TRENAMEAT: u8 = 74;
pub(crate) const RRENAMEAT: u8 = 75;
pub(crate) const TUNLINKAT: u8 = 76;
pub(crate) const RUNLINKAT: u8 = 77;
pub(crate) const TSYMLINK: u8 = 16;
pub(crate) const RSYMLINK: u8 = 17;
pub(crate) const TMKNOD: u8 = 18;
pub(crate) const RMKNOD: u8 = 19;
pub(crate) const TSETATTR: u8 = 26;
pub(crate) const RSETATTR: u8 = 27;
pub(crate) const TLOPEN: u8 = 12;
pub(crate) const RLOPEN: u8 = 13;
pub(crate) const TGETATTR: u8 = 24;
pub(crate) const RGETATTR: u8 = 25;
pub(crate) const TRENAME: u8 = 20;
pub(crate) const RRENAME: u8 = 21;
pub(crate) const TREADDIR: u8 = 40;
pub(crate) const RREADDIR: u8 = 41;
pub(crate) const TFSYNC: u8 = 50;
pub(crate) const RFSYNC: u8 = 51;

pub(crate) const P9_DOTL_RDONLY: u32 = 0;
pub(crate) const P9_DOTL_WRONLY: u32 = 1;
pub(crate) const P9_DOTL_RDWR: u32 = 2;
pub(crate) const P9L_MODE_MASK: u32 = 0x1fff;

pub(crate) const P9_SETATTR_MODE: u32 = 1;
pub(crate) const P9_SETATTR_UID: u32 = 1 << 1;
pub(crate) const P9_SETATTR_GID: u32 = 1 << 2;
pub(crate) const P9_SETATTR_SIZE: u32 = 1 << 3;
pub(crate) const P9_SETATTR_ATIME: u32 = 1 << 4;
pub(crate) const P9_SETATTR_MTIME: u32 = 1 << 5;
pub(crate) const P9_SETATTR_CTIME: u32 = 1 << 6;
pub(crate) const P9_SETATTR_ATIME_SET: u32 = 1 << 7;
pub(crate) const P9_SETATTR_MTIME_SET: u32 = 1 << 8;
pub(crate) const P9_DOTL_AT_REMOVEDIR: u32 = 0x200;
pub(crate) const P9_STATS_BASIC: u64 = 0x000007ff;
pub(crate) const P9_STATS_GEN: u64 = 1 << 12;

pub(crate) const P9_IOHDRSZ: u32 = 24;
pub(crate) const P9_MAXWELEM: usize = 16;
pub(crate) const DEFAULT_MSIZE: u32 = 128 * 1024 + P9_IOHDRSZ;

/// Identifies one file object within a 9P server.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Qid {
    pub(crate) type_: u8,
    pub(crate) version: u32,
    pub(crate) path: u64,
}

impl Qid {
    /// Returns the protocol file-type bits.
    pub const fn file_type(self) -> u8 {
        self.type_
    }

    /// Returns the server-maintained file version.
    pub const fn version(self) -> u32 {
        self.version
    }

    /// Returns the server-stable file identity used as the VFS inode number.
    pub const fn path(self) -> u64 {
        self.path
    }

    /// Returns whether this qid identifies a directory.
    pub const fn is_dir(self) -> bool {
        self.type_ & 0x80 != 0
    }

    /// Returns whether this qid identifies a symbolic link.
    pub const fn is_symlink(self) -> bool {
        self.type_ & 0x02 != 0
    }
}
