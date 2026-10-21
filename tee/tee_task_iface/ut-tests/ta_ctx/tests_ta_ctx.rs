// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq};

use super::*;

#[unittest::def_test]
fn test_bytes_to_ta_head() {
    let ta_head = ta_head {
        uuid: Default::default(),
        stack_size: 1024,
        flags: 1,
        depr_entry: u64::MAX,
    };
    let data = bytemuck::bytes_of(&ta_head);
    let ta_head_from_bytes = bytes_to_ta_head(data).unwrap();
    assert_eq!(ta_head_from_bytes, ta_head);
}

// Test function for basic ta_ctx operations
#[unittest::def_test]
fn test_ta_ctx() {
    let mut ta_ctx = TeeTaCtx::default();
    ta_ctx.set_uuid("/tee/ta/936da01f-9abd-4d9d-80c7-02af85c822a8.ta");
    assert_eq!(ta_ctx.uuid, "936da01f-9abd-4d9d-80c7-02af85c822a8");
    ta_ctx.uuid.clear();
    assert!(ta_ctx.uuid.is_empty());
    ta_ctx.set_uuid("/tee/ta/936da01f-9abd-4d9d-80c7-02af85c822a.ta");
    assert!(ta_ctx.uuid.is_empty());
    ta_ctx.set_uuid("/tee/ta/936da01f-9abd-4d9d-80c7-02af85c822a8");
    assert!(ta_ctx.uuid.is_empty());
    ta_ctx.set_uuid("/tee/ta/936DA01F-9ABD-4D9D-80C7-02AF85C822A8.ta");
    assert_eq!(ta_ctx.uuid, "936DA01F-9ABD-4D9D-80C7-02AF85C822A8");

    assert!(TeeTaCtx::is_ta(
        "/tee/ta/936da01f-9abd-4d9d-80c7-02af85c822a8.ta"
    ));
    assert!(TeeTaCtx::is_ta(
        "/tee/ta/936DA01F-9ABD-4D9D-80C7-02AF85C822A8.ta"
    ));
    assert!(!TeeTaCtx::is_ta("/tee/ta/foo.ta"));
    assert!(!TeeTaCtx::is_ta(
        "/tee/ta/936da01f9abd4d9d80c702af85c822a8.ta"
    ));
    assert!(!TeeTaCtx::is_ta(
        "/tee/ta/../936da01f-9abd-4d9d-80c7-02af85c822a8.ta"
    ));
    assert!(!TeeTaCtx::is_ta(
        "/tee/ta/subdir/936da01f-9abd-4d9d-80c7-02af85c822a8.ta"
    ));
    assert!(!TeeTaCtx::is_ta(
        "/tmp/936da01f-9abd-4d9d-80c7-02af85c822a8.ta"
    ));

    assert!(looks_like_ta("/036da01f-9abd-4d9d-80c7-02af85c822a8.ta"));
    assert!(looks_like_ta(
        "/tee/ta/936da01f-9abd-4d9d-80c7-02af85c822a8.ta"
    ));
    assert!(!looks_like_ta(
        "/tee/ta/../936da01f-9abd-4d9d-80c7-02af85c822a8.ta"
    ));
    assert!(!looks_like_ta("/tee/ta/foo.ta"));
    assert!(!looks_like_ta("/bin/sh"));
    assert!(!TeeTaCtx::is_ta("/036da01f-9abd-4d9d-80c7-02af85c822a8.ta"));
}
