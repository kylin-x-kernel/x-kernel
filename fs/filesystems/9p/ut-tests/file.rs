// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::*;

#[def_test]
fn open_flags_follow_linux_dotl_mapping() {
    assert_eq!(
        p9_open_flags(OpenFlags::APPEND | OpenFlags::SYNC, false),
        P9OpenFlags::APPEND | P9OpenFlags::DSYNC | P9OpenFlags::SYNC
    );
    assert_eq!(
        p9_open_flags(OpenFlags::TRUNCATE, false),
        P9OpenFlags::empty()
    );
    assert_eq!(
        p9_open_flags(OpenFlags::CREATE | OpenFlags::EXCLUSIVE, true),
        P9OpenFlags::empty()
    );
}
