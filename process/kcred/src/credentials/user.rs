// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! User ID types.

/// Linux user identifier.
pub type Uid = u32;

/// Sentinel value that never names a valid user ID.
pub const INVALID_UID: Uid = u32::MAX;

/// Returns whether `uid` can name a valid user ID.
pub const fn is_valid_uid(uid: Uid) -> bool {
    uid != INVALID_UID
}
