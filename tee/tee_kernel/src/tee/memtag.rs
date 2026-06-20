// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::ffi::c_void;

use super::types_ext::*;
use crate::tee::TeeResult;

#[inline]
pub fn memtag_strip_tag_vaddr(addr: *const c_void) -> Vaddr {
    addr as Vaddr
}

#[inline]
pub(crate) fn memtag_strip_tag() -> TeeResult {
    Ok(())
}

#[inline]
pub(crate) fn memtag_strip_tag_const() -> TeeResult {
    Ok(())
}
