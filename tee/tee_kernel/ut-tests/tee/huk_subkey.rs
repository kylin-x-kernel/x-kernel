// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use super::*;

#[unittest::def_test]
fn test_huk_subkey_derive() {
    // derive with const_data NULL in subkey_1
    let mut subkey_1 = [0; HUK_SUBKEY_MAX_LEN];
    huk_subkey_derive(HukSubkeyUsage::Ssk, None, &mut subkey_1).unwrap();
    assert_eq!(subkey_1.len(), HUK_SUBKEY_MAX_LEN);
    // subkey must  not be all zero
    assert!(!subkey_1.iter().all(|x| *x == 0));

    // derive with const_data NULL in subkey_2
    let mut subkey_2 = [0; HUK_SUBKEY_MAX_LEN];
    huk_subkey_derive(HukSubkeyUsage::Ssk, None, &mut subkey_2).unwrap();
    assert_eq!(subkey_2.len(), HUK_SUBKEY_MAX_LEN);
    // subkey must  not be all zero
    assert!(!subkey_2.iter().all(|x| *x == 0));

    // subkey_1 and subkey_2 must be same
    assert_eq!(subkey_1, subkey_2);

    // derive with const_data in subkey_3
    let const_data = b"test_const_data";
    let mut subkey_3 = [0; HUK_SUBKEY_MAX_LEN];
    huk_subkey_derive(HukSubkeyUsage::Ssk, Some(const_data), &mut subkey_3).unwrap();
    assert_eq!(subkey_3.len(), HUK_SUBKEY_MAX_LEN);
    // subkey must  not be all zero
    assert!(!subkey_3.iter().all(|x| *x == 0));

    // subkey_1 and subkey_3 must be different
    assert!(subkey_1 != subkey_3);
}
