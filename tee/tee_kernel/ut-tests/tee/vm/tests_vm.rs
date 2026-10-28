// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use tee_raw_sys::TEE_ERROR_NOT_SUPPORTED;
use unittest::assert_eq;

use super::sys_tee_scn_check_access_rights;

#[unittest::def_test]
fn test_check_access_rights_placeholder_stays_unsupported() {
    assert_eq!(
        sys_tee_scn_check_access_rights(0, 0, 0),
        Err(TEE_ERROR_NOT_SUPPORTED)
    );
    assert_eq!(
        sys_tee_scn_check_access_rights(usize::MAX, 0, usize::MAX),
        Err(TEE_ERROR_NOT_SUPPORTED)
    );
}
