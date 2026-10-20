// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::this_cpu_id;

#[def_test]
fn test_this_cpu_id_consistent() {
    let id = this_cpu_id();
    assert_eq!(id, this_cpu_id());
}
