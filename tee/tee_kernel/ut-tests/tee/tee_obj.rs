// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use super::*;

#[unittest::def_test(user)]
fn test_tee_obj_add_get() {
    let obj = TeeObj {
        busy: true,
        ..Default::default()
    };
    let obj_id = tee_obj_add(obj).expect("Failed to add TeeObj");
    info!("Added TeeObj with id {}", obj_id);
    let retrieved_obj = tee_obj_get(obj_id).expect("Failed to get TeeObj");
    assert!(retrieved_obj.lock().busy);
}
