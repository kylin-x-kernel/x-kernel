// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::bytes_to_kib;

#[def_test]
fn test_bytes_to_kib_rounds_down() {
    assert_eq!(bytes_to_kib(4097), 4);
}
