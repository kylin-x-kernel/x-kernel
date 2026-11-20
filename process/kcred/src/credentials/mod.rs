// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX process credentials.

pub(crate) mod caps;
mod file_caps;
mod group;
mod model;
mod securebits;
mod user;

pub use caps::*;
pub use file_caps::*;
pub use group::{Gid, INVALID_GID, is_valid_gid};
pub use model::{Cred, ExecIds, ExecSecurity, PtraceAccessMode};
pub use user::{INVALID_UID, Uid, is_valid_uid};
