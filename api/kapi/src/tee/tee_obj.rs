// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{
    boxed::Box,
    sync::Arc,
    vec::{self, Vec},
};
use core::{default, ffi::c_ulong, fmt, fmt::Debug};

use bincode::de;
use flatten_objects::FlattenObjects;
use kcore::task::{AsThread, TeeSessionCtxTrait};
use kerrno::{KError, KResult};
use ksync::{Mutex, RwLock};
use ktask::current;
use slab::Slab;
use tee_raw_sys::{libc_compat::size_t, *};

use super::{
    TeeResult,
    libmbedtls::bignum::BigNum,
    tee_pobj::{tee_pobj, tee_pobj_release},
    tee_ree_fs::tee_file_handle,
    tee_session::{with_tee_session_ctx, with_tee_session_ctx_mut},
    tee_svc_cryp::{TeeCryptObj, TeeCryptObjAttr},
};

pub type tee_obj_id_type = c_ulong; //usize;
/// The maximum number of open files
pub const AX_TEE_OBJ_LIMIT: usize = 1024;

// scope_local::scope_local! {
//     /// The open objects for TA.
//     pub static TEE_OBJ_TABLE: Arc<RwLock<Slab<Arc<tee_obj>>>> = Arc::default();
// }

#[repr(C)]
pub struct tee_obj {
    pub info: TEE_ObjectInfo,
    pub busy: bool,      // true if used by an operation
    pub have_attrs: u32, // bitfield identifying set properties
    // void *attr;
    pub attr: Vec<TeeCryptObj>,
    pub ds_pos: size_t,
    pub pobj: Option<Arc<RwLock<tee_pobj>>>,
    /// file handle for the pobject
    pub fh: Option<Box<tee_file_handle>>,
}

impl Debug for tee_obj {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let fd_dbg = self.fh.as_ref().map(|h| h.fd.fd).unwrap_or(-1);
        write!(
            f,
            "tee_obj{{info: {:?}, busy: {:?}, have_attrs: {:010X?}, attr: {:?}, ds_pos: {:010X?}, \
             pobj: {:?}, fh: {:?}}}",
            self.info, self.busy, self.have_attrs, self.attr, self.ds_pos, self.pobj, fd_dbg
        )
    }
}

impl default::Default for tee_obj {
    fn default() -> Self {
        tee_obj {
            info: TEE_ObjectInfo {
                objectId: 0,
                objectType: 0,
                objectSize: 0,
                maxObjectSize: 0,
                objectUsage: 0,
                dataSize: 0,
                dataPosition: 0,
                handleFlags: 0,
            },
            busy: false,
            have_attrs: 0,
            attr: Vec::new(),
            ds_pos: 0,
            pobj: None,
            fh: None,
        }
    }
}

fn obj_inner_to_outer(obj: tee_obj_id_type) -> tee_obj_id_type {
    (obj + 1) as tee_obj_id_type
}

fn obj_outer_to_inner(obj: tee_obj_id_type) -> tee_obj_id_type {
    debug_assert!(obj > 0);
    (obj - 1) as tee_obj_id_type
}

pub fn tee_obj_add(mut obj: tee_obj) -> TeeResult<tee_obj_id_type> {
    with_tee_session_ctx_mut(|ctx| {
        // 获取一个可用的 ID
        let vacant = ctx.objects.vacant_entry();
        let mut id = vacant.key();

        id = obj_inner_to_outer(id as tee_obj_id_type) as usize;
        // 设置 objectId
        obj.info.objectId = id as u32;

        // 创建 Arc 并插入
        #[allow(clippy::arc_with_non_send_sync)]
        let arc_obj = Arc::new(Mutex::new(obj));
        let inserted_id = vacant.insert(arc_obj);
        tee_debug!("tee_obj_add: id: {}", id);

        Ok(id as tee_obj_id_type)
    })
}

pub fn tee_obj_get(obj_id: tee_obj_id_type) -> TeeResult<Arc<Mutex<tee_obj>>> {
    let obj_id = obj_outer_to_inner(obj_id);
    with_tee_session_ctx(|ctx| match ctx.objects.get(obj_id as _) {
        Some(obj) => Ok(Arc::clone(obj)),
        None => Err(TEE_ERROR_ITEM_NOT_FOUND),
    })
}

/// delete the tee_obj from the session objects table
///
/// # Arguments
/// * `obj_id` - the id of the tee_obj
pub fn tee_obj_delete(obj_id: u32) -> TeeResult<Arc<Mutex<tee_obj>>> {
    let obj_id = obj_outer_to_inner(obj_id as tee_obj_id_type);
    // remove from session objects
    with_tee_session_ctx_mut(|ctx| -> TeeResult<Arc<Mutex<tee_obj>>> {
        let obj = ctx
            .objects
            .try_remove(obj_id as _)
            .ok_or(TEE_ERROR_ITEM_NOT_FOUND)?;
        Ok(obj)
    })
}

/// close the tee_obj
///
/// 1. delete the tee_obj from the session objects table
/// 2. if the tee_obj is persistent, close the file handle and
/// 3. release the tee_pobj
/// 4. free the tee_obj memory
///
/// # Arguments
/// * `obj_id` - the id of the tee_obj
/// # Returns
/// * `TeeResult` - the result of the operation
pub fn tee_obj_close(obj_id: u32) -> TeeResult {
    let obj = tee_obj_delete(obj_id as _)?;

    let mut obj_guard = obj.lock();
    if obj_guard.info.handleFlags & TEE_HANDLE_FLAG_PERSISTENT != 0 {
        // borrow checker will ensure the pobj is not used after the scope ends
        let (fops, pobj_clone) = {
            let pobj = obj_guard.pobj.as_ref().ok_or(TEE_ERROR_BAD_STATE)?;
            let fops = pobj.read().fops.ok_or(TEE_ERROR_BAD_STATE)?;
            let pobj_clone = Arc::clone(pobj);
            (fops, pobj_clone)
        };

        // now we can safely close the file handle if it was opened
        (fops.close)(&mut obj_guard.fh);
        tee_pobj_release(pobj_clone)?;
    }

    Ok(())
}

#[cfg(feature = "tee_test")]
pub mod tests_tee_obj {
    use unittest::{
        test_fn, test_framework::TestDescriptor, test_framework_basic::TestResult, tests_name,
    };

    use super::*;
    test_fn! {
        using TestResult;

        fn test_tee_obj_add_get() {
            let mut obj = tee_obj {
                busy: true,
                ..Default::default()
            };
            let obj_id = tee_obj_add(obj).expect("Failed to add tee_obj");
            info!("Added tee_obj with id {}", obj_id);
            let retrieved_obj = tee_obj_get(obj_id).expect("Failed to get tee_obj");
            assert!(retrieved_obj.lock().busy);
        }
    }

    tests_name! {
        TEST_TEE_OBJ;
        tee_obj;
        //------------------------
        test_tee_obj_add_get,
    }
}
