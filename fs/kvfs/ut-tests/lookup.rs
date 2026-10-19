// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::LookupFlags;

#[def_test]
fn lookup_flags_encode_follow_policy() {
    assert!(LookupFlags::follow().follows_final());
    assert!(!LookupFlags::no_follow().follows_final());
}

#[def_test]
fn lookup_flags_encode_magic_link_rejection() {
    let flags = LookupFlags::follow() | LookupFlags::NO_MAGIC_LINKS;
    assert!(flags.follows_final());
    assert!(flags.rejects_magic_links());
}
