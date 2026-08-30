// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kvfs::NodePermission;

pub(crate) fn writable_mode() -> NodePermission {
    NodePermission::OWNER_READ
        | NodePermission::OWNER_WRITE
        | NodePermission::GROUP_READ
        | NodePermission::OTHER_READ
}

pub(crate) fn readonly_mode() -> NodePermission {
    NodePermission::OWNER_READ | NodePermission::GROUP_READ | NodePermission::OTHER_READ
}
