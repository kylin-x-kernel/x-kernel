// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::Ordering;

use crate::tee::{
    tee_pobj::{
        POBJS, TEE_DATA_FLAG_SHARE_READ, TEE_DATA_FLAG_SHARE_WRITE, TEE_UUID, TeePobj,
        TeePobjUsage, tee_pobj_get, with_pobj_usage_lock,
    },
    tee_ree_fs::{REE_FS_OPS, TeeFileOperations},
};
#[unittest::def_test]
fn test_tee_pobj_default() {
    let pobj = TeePobj::default();
    assert_eq!(pobj.obj_id.lock().obj_id_len, 0);
}

#[unittest::def_test]
fn test_with_pobj_usage_lock() {
    let pobj = TeePobj::default();
    let result: Result<(), ()> =
        with_pobj_usage_lock(pobj.flags.load(Ordering::Relaxed), || Ok(()));
    assert_eq!(result, Ok::<(), ()>(()));
    // set flag
    pobj.flags
        .store(TEE_DATA_FLAG_SHARE_WRITE, Ordering::Relaxed);
    let result: Result<(), ()> =
        with_pobj_usage_lock(pobj.flags.load(Ordering::Relaxed), || Ok(()));
    assert_eq!(result, Ok::<(), ()>(()));
    // set flag
    pobj.flags
        .store(TEE_DATA_FLAG_SHARE_READ, Ordering::Relaxed);
    let result: Result<(), ()> =
        with_pobj_usage_lock(pobj.flags.load(Ordering::Relaxed), || Ok(()));
    assert_eq!(result, Ok::<(), ()>(()));
}

#[unittest::def_test]
fn test_tee_pobj_get() {
    // 1. create a new pobj
    let obj_id = [0x12, 0x34, 0x56, 0x78];
    {
        let result = tee_pobj_get(
            &TEE_UUID::default(),
            &obj_id,
            obj_id.len() as u32,
            0,
            TeePobjUsage::Enumerate,
            &REE_FS_OPS,
        );
        assert!(result.is_ok());
        // check VecQueue size
        let pobjs = POBJS.inner.lock();
        assert_eq!(pobjs.len(), 1);
        // check pobj
        let pobj = result.unwrap();
        let pobj_guard = pobj.obj_id.lock();
        assert_eq!(pobj_guard.obj_id, obj_id.to_vec().into_boxed_slice());
        assert_eq!(pobj_guard.obj_id_len, obj_id.len() as u32);
        assert_eq!(pobj.flags.load(Ordering::Relaxed), 0);
        assert_eq!(
            pobj.fops.unwrap() as *const TeeFileOperations,
            &REE_FS_OPS as *const TeeFileOperations
        );
        let echo = (pobj.fops.unwrap().echo)();
        assert_eq!(echo, "TeeFileOperations->echo");
    }
    // 2. get the same pobj
    {
        let result = tee_pobj_get(
            &TEE_UUID::default(),
            &obj_id,
            obj_id.len() as u32,
            0,
            TeePobjUsage::Enumerate,
            &REE_FS_OPS,
        );
        assert!(result.is_ok());
        // check VecQueue size
        let pobjs = POBJS.inner.lock();
        assert_eq!(pobjs.len(), 1);
        // check pobj
        let pobj = result.unwrap();
        let pobj_guard = pobj.obj_id.lock();
        assert_eq!(pobj_guard.obj_id, obj_id.to_vec().into_boxed_slice());
        assert_eq!(pobj_guard.obj_id_len, obj_id.len() as u32);
        assert_eq!(pobj.flags.load(Ordering::Relaxed), 0);
        assert_eq!(
            pobj.fops.unwrap() as *const TeeFileOperations,
            &REE_FS_OPS as *const TeeFileOperations
        );
        let echo = (pobj.fops.unwrap().echo)();
        assert_eq!(echo, "TeeFileOperations->echo");
    }
}
