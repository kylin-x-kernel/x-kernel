// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use super::*;

#[test]
fn test_file_name() {
    assert_eq!(Some("c"), Pathname::new("../a/b/c").file_name());
    assert_eq!(Some("b"), Pathname::new("a/b/.").file_name());
    assert_eq!(None, Pathname::new("a/..").file_name());
}
