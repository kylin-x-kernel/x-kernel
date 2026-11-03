// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Group ID types.

/// Linux group identifier.
pub type Gid = u32;

/// Sentinel value that never names a valid group ID.
pub const INVALID_GID: Gid = u32::MAX;

/// Returns whether `gid` can name a valid group ID.
pub const fn is_valid_gid(gid: Gid) -> bool {
    gid != INVALID_GID
}
