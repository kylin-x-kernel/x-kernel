// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use kalloc_types::{AllocFlags, KBox};
use ksync::Mutex;
use tee_raw_sys::{TEE_ERROR_BAD_PARAMETERS, TEE_ERROR_BAD_STATE, TEE_ERROR_OUT_OF_MEMORY};

use crate::tee::{
    tee_pobj::{
        ObjId, TEE_DATA_FLAG_SHARE_READ, TEE_DATA_FLAG_SHARE_WRITE, TEE_UUID, TeePobj,
        TeePobjUsage, tee_pobj_get, tee_pobj_get_with_owner, tee_pobj_release, tee_pobj_rename,
        with_pobj_usage_lock,
    },
    tee_ree_fs::{REE_FS_OPS, TeeFileOperations},
};
fn fixture(owner: KBox<[u8]>) -> TeePobj {
    TeePobj {
        refcnt: AtomicU32::new(1),
        uuid: TEE_UUID::default(),
        obj_id: Mutex::new(ObjId {
            obj_id_len: owner.len() as u32,
            obj_id: owner,
        }),
        flags: AtomicU32::new(0),
        obj_info_usage: AtomicU32::new(0),
        temporary: AtomicBool::new(false),
        creating: AtomicBool::new(false),
        fops: None,
    }
}

#[unittest::def_test]
fn test_with_pobj_usage_lock() {
    let pobj = fixture(KBox::from_slice_with_flags(&[], AllocFlags::NOWAIT).unwrap());
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
        // check pobj
        let pobj = result.unwrap();
        let pobj_guard = pobj.obj_id.lock();
        assert_eq!(&*pobj_guard.obj_id, &obj_id);
        assert_eq!(pobj_guard.obj_id_len, obj_id.len() as u32);
        assert_eq!(pobj.flags.load(Ordering::Relaxed), 0);
        assert_eq!(
            pobj.fops.unwrap() as *const TeeFileOperations,
            &REE_FS_OPS as *const TeeFileOperations
        );
        let echo = (pobj.fops.unwrap().echo)();
        assert_eq!(echo, "TeeFileOperations->echo");
        drop(pobj_guard);
        let reused = tee_pobj_get_with_owner(
            &TEE_UUID::default(),
            &obj_id,
            obj_id.len() as u32,
            0,
            TeePobjUsage::Enumerate,
            &REE_FS_OPS,
            |_, _| Err(TEE_ERROR_OUT_OF_MEMORY),
        )
        .unwrap();
        assert!(alloc::sync::Arc::ptr_eq(&pobj, &reused));
        tee_pobj_release(reused).unwrap();
        tee_pobj_release(pobj).unwrap();
    }
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
        // check pobj
        let pobj = result.unwrap();
        let pobj_guard = pobj.obj_id.lock();
        assert_eq!(&*pobj_guard.obj_id, &obj_id);
        assert_eq!(pobj_guard.obj_id_len, obj_id.len() as u32);
        assert_eq!(pobj.flags.load(Ordering::Relaxed), 0);
        assert_eq!(
            pobj.fops.unwrap() as *const TeeFileOperations,
            &REE_FS_OPS as *const TeeFileOperations
        );
        let echo = (pobj.fops.unwrap().echo)();
        assert_eq!(echo, "TeeFileOperations->echo");
        drop(pobj_guard);
        tee_pobj_release(pobj).unwrap();
    }
}

#[unittest::def_test]
fn id_owner_failure_does_not_publish() {
    let bytes = b"pobj-owner-failure-test";
    let result = tee_pobj_get_with_owner(
        &TEE_UUID::default(),
        bytes,
        bytes.len() as u32,
        0,
        TeePobjUsage::Enumerate,
        &REE_FS_OPS,
        |source, flags| {
            assert_eq!(source, bytes);
            assert_eq!(flags, AllocFlags::NOWAIT);
            Err(TEE_ERROR_OUT_OF_MEMORY)
        },
    );
    assert_eq!(result.unwrap_err(), TEE_ERROR_OUT_OF_MEMORY);
    assert!(
        super::POBJS
            .find_pobj(
                &TEE_UUID::default(),
                bytes,
                bytes.len() as u32,
                &Some(&REE_FS_OPS),
            )
            .is_none()
    );
}

#[unittest::def_test]
fn rename_transfers_prepared_storage() {
    let pobj = fixture(KBox::from_slice_with_flags(b"old", AllocFlags::NOWAIT).unwrap());
    let owner = KBox::from_slice_with_flags(b"new", AllocFlags::NOWAIT).unwrap();
    let address = owner.as_ptr();
    tee_pobj_rename(&pobj, owner, 3).unwrap();
    let id = pobj.obj_id.lock();
    assert_eq!(id.obj_id.as_ptr(), address);
    assert_eq!(&*id.obj_id, b"new");
    assert_eq!(id.obj_id_len, 3);
}

#[unittest::def_test]
fn rejected_rename_preserves_id() {
    let pobj = fixture(KBox::from_slice_with_flags(b"old", AllocFlags::NOWAIT).unwrap());
    let address = pobj.obj_id.lock().obj_id.as_ptr();
    let owner = KBox::from_slice_with_flags(b"new", AllocFlags::NOWAIT).unwrap();
    assert_eq!(
        tee_pobj_rename(&pobj, owner, 4),
        Err(TEE_ERROR_BAD_PARAMETERS)
    );
    pobj.refcnt.store(2, Ordering::Relaxed);
    let owner = KBox::from_slice_with_flags(b"new", AllocFlags::NOWAIT).unwrap();
    assert_eq!(tee_pobj_rename(&pobj, owner, 3), Err(TEE_ERROR_BAD_STATE));
    let id = pobj.obj_id.lock();
    assert_eq!(id.obj_id.as_ptr(), address);
    assert_eq!(&*id.obj_id, b"old");
    assert_eq!(id.obj_id_len, 3);
}
