// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use linux_raw_sys::general::MFD_ALLOW_SEALING;
use unittest::{assert_eq, def_test};

use super::*;

#[def_test]
fn memfd_flags_accept_allow_sealing() {
    let flags = MemfdFlags::from_raw(MFD_CLOEXEC | MFD_ALLOW_SEALING).unwrap();

    assert!(flags.is_cloexec());
    assert!(flags.allows_sealing());
}

#[def_test]
fn memfd_flags_reject_unknown_bits() {
    assert_eq!(MemfdFlags::from_raw(0x8000_0000), Err(KError::InvalidInput));
}

#[def_test]
fn memfd_debug_name_formats_a_single_backing_component() {
    assert_eq!(format_memfd_name("/../buffer"), "memfd::..:buffer");
    assert_eq!(format_memfd_name("good:name"), "memfd:good:name");
}
