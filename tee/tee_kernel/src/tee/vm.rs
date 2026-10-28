// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use tee_raw_sys::TEE_ERROR_NOT_SUPPORTED;

use crate::tee::TeeResult;

pub(super) fn vm_check_access_rights(_flags: u32, _uaddr: usize, _len: usize) -> TeeResult {
    Ok(())
}

/// Placeholder for GP `TEE_CheckMemoryAccessRights` (Linux syscall 508).
///
/// Private regions and secure mappings are not recorded yet, so a success result would be
/// indistinguishable from a real permission check. Queries stay unsupported until that
/// state exists. In-kernel copies keep using [`vm_check_access_rights`].
pub(super) fn sys_tee_scn_check_access_rights(
    _flags: usize,
    _buf: usize,
    _len: usize,
) -> TeeResult {
    Err(TEE_ERROR_NOT_SUPPORTED)
}

unittest::path_mod_test!(pub(super) mod tests_vm, "../../ut-tests/tee/vm/tests_vm.rs");
