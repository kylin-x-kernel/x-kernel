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


#[unittest::def_test]
fn test_session_handle_allocation_rules() {
    let mut ctx = TeeTaCtx::default();
    assert_eq!(ctx.next_session_handle, 1, "初始候选值为 1");

    let h1 = ctx.insert_session(String::from("uuid-a"), 100).unwrap();
    let h2 = ctx.insert_session(String::from("uuid-b"), 101).unwrap();
    assert_eq!((h1, h2), (1, 2), "首个 handle 是 1,后续单调递增");

    assert_eq!(ctx.session(1).unwrap().manager_session_id, 100);
    assert_eq!(ctx.session(1).unwrap().target_uuid, "uuid-a");
    assert!(ctx.session(3).is_none());

    assert!(ctx.remove_session(1).is_some());
    let h3 = ctx.insert_session(String::from("uuid-c"), 102).unwrap();
    assert_eq!(h3, 3, "分配从 next_session_handle 继续,不立即复用旧槽");

    ctx.next_session_handle = 2;
    let h4 = ctx.insert_session(String::from("uuid-d"), 103).unwrap();
    assert_eq!(h4, 4, "扫描跳过被占用槽");
}

#[unittest::def_test]
fn test_session_handle_wraps_past_max_and_resolves_collision() {
    let mut ctx = TeeTaCtx::default();
    ctx.insert_session(String::from("uuid-a"), 200);
    ctx.next_session_handle = u32::MAX;

    let h_max = ctx.insert_session(String::from("uuid-b"), 201).unwrap();
    assert_eq!(h_max, u32::MAX);
    assert_eq!(ctx.next_session_handle, 1, "MAX 之后回绕到 1");

    let h_after_wrap = ctx.insert_session(String::from("uuid-c"), 202).unwrap();
    assert_eq!(h_after_wrap, 2, "回绕撞上 slot 1 后继续扫描到 2");
}

#[unittest::def_test]
fn test_next_free_handle_full_table_returns_none() {
    assert_eq!(next_free_handle(1, 3, |_| true), None);
    assert_eq!(next_free_handle(0, 2, |h| h == 3), Some(1));
    assert_eq!(next_free_handle(1, 0, |h| h == 5), Some(1));
}

#[unittest::def_test]
fn test_clear_sessions_drops_all_and_resets_allocation() {
    let mut ctx = TeeTaCtx::default();
    assert!(!ctx.has_open_sessions());
    ctx.insert_session(String::from("uuid-a"), 300);
    ctx.insert_session(String::from("uuid-b"), 301);
    assert!(ctx.has_open_sessions());

    ctx.clear_sessions();
    assert!(!ctx.has_open_sessions());
    assert!(ctx.session(1).is_none());
    assert_eq!(ctx.next_session_handle, 1, "清空后 handle 分配重置");
    let h = ctx.insert_session(String::from("uuid-new"), 302).unwrap();
    assert_eq!(h, 1);
}

#[unittest::def_test]
fn uncertain_close_preserves_owner_release_without_a_live_handle() {
    let mut ctx = TeeTaCtx::default();
    let handle = ctx.insert_session(String::from("target"), 42).unwrap();
    ctx.remove_session(handle);
    assert!(!ctx.has_open_sessions());
    assert!(!ctx.needs_owner_release());
    ctx.require_owner_release();
    assert!(ctx.needs_owner_release());
    assert!(ctx.session(handle).is_none());
    ctx.clear_sessions();
    assert!(!ctx.needs_owner_release());
}
