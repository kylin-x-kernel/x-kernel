// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::tee::TeeResult;

pub fn vm_check_access_rights(_flags: u32, _uaddr: usize, _len: usize) -> TeeResult {
    Ok(())
}
