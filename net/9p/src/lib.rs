// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_std]

//! 9P2000.L protocol client objects for `no_std` targets.

extern crate alloc;

mod client;
mod message;
mod protocol;
mod transport;

pub use client::{
    FileAttr, P9Client, P9DirEntry, P9Fid, P9OpenFlags, P9OpenMode, P9SetAttr, P9SetAttrTime,
    P9StatFs,
};
pub use protocol::Qid;
pub use transport::P9Transport;
