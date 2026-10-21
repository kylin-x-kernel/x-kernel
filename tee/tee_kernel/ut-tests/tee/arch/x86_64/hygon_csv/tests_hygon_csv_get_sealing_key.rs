// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::assert;

use super::*;

#[unittest::def_test]
fn test_hygon_csv_get_sealing_key() {
    let result = get_sealing_key();
    assert!(result.is_ok());
    let key_buf = result.unwrap();
    assert!(key_buf.len() == 32);
    assert!(key_buf.iter().any(|&x| x != 0));

    let result2 = get_sealing_key();
    assert!(result2.is_ok());
    let key_buf2 = result2.unwrap();
    assert!(key_buf2.len() == 32);
    assert!(key_buf2.iter().any(|&x| x != 0));

    assert!(key_buf == key_buf2);
}
