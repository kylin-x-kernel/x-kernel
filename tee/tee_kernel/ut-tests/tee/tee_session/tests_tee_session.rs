// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::string::String;

use unittest::assert_eq;

use super::*;

// Test function for with_tee_ta_ctx helper functions
#[unittest::def_test(user)]
fn test_with_tee_ta_ctx() {
    let mut uuid_back: String = String::new();

    with_tee_ta_ctx(|ta_ctx| {
        uuid_back = ta_ctx.uuid.clone();
        Ok(())
    })
    .unwrap();
    assert_eq!(uuid_back, "00000000-0000-0000-0000-000000000000");

    with_tee_ta_ctx_mut(|ta_ctx| {
        ta_ctx.uuid = String::from("936da01f-9abd-4d9d-80c7-02af85c822a8");
        Ok(())
    })
    .unwrap();

    let mut uuid_modify: String = String::new();
    with_tee_ta_ctx(|ta_ctx| {
        uuid_modify = ta_ctx.uuid.clone();
        Ok(())
    })
    .unwrap();
    assert_eq!(uuid_modify, "936da01f-9abd-4d9d-80c7-02af85c822a8");

    with_tee_ta_ctx_mut(|ta_ctx| {
        ta_ctx.uuid = uuid_back;
        Ok(())
    })
    .unwrap();
}
