//! 9P protocol constants and data types.
#![allow(unused)]
/// Special values used by the protocol.
pub const NO_FID: u32 = 0xFFFF_FFFF;
pub const NO_TAG: u16 = 0xFFFF;

pub const TVERSION: u8 = 100;
pub const RVERSION: u8 = 101;
pub const TATTACH: u8 = 104;
pub const RATTACH: u8 = 105;
pub const RERROR: u8 = 107;
pub const RLERROR: u8 = 7;
pub const TREMOVE: u8 = 122;
pub const RREMOVE: u8 = 123;
pub const TWALK: u8 = 110;
pub const RWALK: u8 = 111;
pub const TOPEN: u8 = 112;
pub const ROPEN: u8 = 113;
pub const TCREATE: u8 = 114;
pub const RCREATE: u8 = 115;
pub const TREAD: u8 = 116;
pub const RREAD: u8 = 117;
pub const TWRITE: u8 = 118;
pub const RWRITE: u8 = 119;
pub const TCLUNK: u8 = 120;
pub const RCLUNK: u8 = 121;
pub const TREADLINK: u8 = 22;
pub const RREADLINK: u8 = 23;
pub const TLINK: u8 = 70;
pub const RLINK: u8 = 71;
pub const TMKDIR: u8 = 72;
pub const RMKDIR: u8 = 73;
pub const TSYMLINK: u8 = 16;
pub const RSYMLINK: u8 = 17;
pub const TSETATTR: u8 = 26;
pub const RSETATTR: u8 = 27;
pub const TLOPEN: u8 = 12;
pub const RLOPEN: u8 = 13;
pub const TLCREATE: u8 = 14;
pub const RLCREATE: u8 = 15;
pub const TGETATTR: u8 = 24;
pub const RGETATTR: u8 = 25;
pub const TRENAME: u8 = 20;
pub const RRENAME: u8 = 21;
pub const TREADDIR: u8 = 40;
pub const RREADDIR: u8 = 41;
pub const TFSYNC: u8 = 50;
pub const RFSYNC: u8 = 51;

pub const OREAD: u8 = 0;
#[allow(dead_code)]
pub const OWRITE: u8 = 1;
#[allow(dead_code)]
pub const ORDWR: u8 = 2;
#[allow(dead_code)]
pub const OTRUNC: u8 = 0x10;
#[allow(dead_code)]
pub const OAPPEND: u8 = 0x80;

pub const P9_DOTL_RDONLY: u32 = 0;
pub const P9_DOTL_WRONLY: u32 = 1;
pub const P9_DOTL_RDWR: u32 = 2;
pub const P9_DOTL_CREATE: u32 = 0x100;
pub const P9_DOTL_TRUNC: u32 = 0x1000;
pub const P9_DOTL_APPEND: u32 = 0x2000;

pub const P9_ATTR_SIZE: u32 = 1 << 3;
pub const P9_SETATTR_MODE: u32 = 1;
pub const P9_STATS_BASIC: u64 = 0x000007ff;

pub const DMDIR: u32 = 0x8000_0000;

pub const DEFAULT_MSIZE: u32 = 16384;

/// Qid identifies a file within a 9P server.
#[derive(Clone, Copy, Debug)]
pub struct Qid {
    pub(crate) type_: u8,
    pub(crate) _version: u32,
    pub(crate) _path: u64,
}
