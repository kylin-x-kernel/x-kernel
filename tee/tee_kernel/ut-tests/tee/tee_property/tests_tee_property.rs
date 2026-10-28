// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{
    ffi::{c_ulong, c_void},
    ptr,
};

use tee_raw_sys::{
    TEE_ERROR_ITEM_NOT_FOUND, TEE_ERROR_SHORT_BUFFER, TEE_PROPSET_TEE_IMPLEMENTATION,
};
use unittest::assert_eq;

use super::{sys_tee_scn_get_property, sys_tee_scn_get_property_name_to_index};
use crate::{TestUserBuffer, TestUserValue};

fn impl_prop_set() -> c_ulong {
    TEE_PROPSET_TEE_IMPLEMENTATION as usize as c_ulong
}

#[unittest::def_test]
fn test_unknown_property_set_is_not_found() {
    let res = sys_tee_scn_get_property(
        0,
        0,
        ptr::null_mut(),
        ptr::null_mut(),
        ptr::null_mut(),
        ptr::null_mut(),
        ptr::null_mut(),
    );
    assert_eq!(res, Err(TEE_ERROR_ITEM_NOT_FOUND));

    let res = sys_tee_scn_get_property(
        impl_prop_set(),
        100,
        ptr::null_mut(),
        ptr::null_mut(),
        ptr::null_mut(),
        ptr::null_mut(),
        ptr::null_mut(),
    );
    assert_eq!(res, Err(TEE_ERROR_ITEM_NOT_FOUND));
}

#[unittest::def_test(user)]
fn test_short_property_value_reports_required_len() {
    // `gpd.tee.description` is the NUL-terminated string "x-kernel".
    const REQUIRED_LEN: u32 = 9;
    let prop_set = impl_prop_set();
    let buf = TestUserBuffer::new(REQUIRED_LEN as usize).unwrap();
    let mut blen = TestUserValue::from_value(1u32).unwrap();

    let res = sys_tee_scn_get_property(
        prop_set,
        1,
        ptr::null_mut(),
        ptr::null_mut(),
        buf.as_user_ptr::<u8>().cast::<c_void>(),
        blen.as_user_ptr(),
        ptr::null_mut(),
    );
    assert_eq!(res, Err(TEE_ERROR_SHORT_BUFFER));
    assert_eq!(blen.read(), REQUIRED_LEN);

    blen.write(REQUIRED_LEN);
    let res = sys_tee_scn_get_property(
        prop_set,
        1,
        ptr::null_mut(),
        ptr::null_mut(),
        buf.as_user_ptr::<u8>().cast::<c_void>(),
        blen.as_user_ptr(),
        ptr::null_mut(),
    );
    assert_eq!(res, Ok(()));
    assert_eq!(blen.read(), REQUIRED_LEN);
    assert_eq!(buf.read_bytes(REQUIRED_LEN as usize).unwrap(), b"x-kernel\0");
}

#[unittest::def_test(user)]
fn test_name_to_index_unknown_set_is_not_found() {
    let name = b"gpd.tee.apiversion\0";
    let name_buf = TestUserBuffer::new(name.len()).unwrap();
    name_buf.write_bytes(name).unwrap();
    let index = TestUserValue::from_value(0u32).unwrap();

    let res = sys_tee_scn_get_property_name_to_index(
        0,
        name_buf.as_user_ptr::<u8>().cast::<c_void>(),
        name.len() as c_ulong,
        index.as_user_ptr(),
    );
    assert_eq!(res, Err(TEE_ERROR_ITEM_NOT_FOUND));

    let res = sys_tee_scn_get_property_name_to_index(
        impl_prop_set(),
        name_buf.as_user_ptr::<u8>().cast::<c_void>(),
        name.len() as c_ulong,
        index.as_user_ptr(),
    );
    assert_eq!(res, Ok(()));
    assert_eq!(index.read(), 0);
}
