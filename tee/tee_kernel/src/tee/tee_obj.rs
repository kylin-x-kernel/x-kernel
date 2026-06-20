// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::{default, ffi::c_ulong, fmt, fmt::Debug};

use ksync::Mutex;
use tee_raw_sys::{libc_compat::size_t, *};

use super::{
    TeeResult,
    tee_pobj::{TeePobj, tee_pobj_release},
    tee_ree_fs::TeeFsFd,
    tee_session::{with_tee_session_ctx, with_tee_session_ctx_mut},
    tee_svc_cryp::TeeCryptObj,
};

/// GP: `tee_obj_id_type`
pub type TeeObjIdType = c_ulong; //usize;

// scope_local::scope_local! {
//     /// The open objects for TA.
//     pub static TEE_OBJ_TABLE: Arc<RwLock<Slab<Arc<TeeObj>>>> = Arc::default();
// }

#[repr(C)]
/// GP: `struct tee_obj`
pub struct TeeObj {
    pub info: TEE_ObjectInfo,
    pub busy: bool,      // true if used by an operation
    pub have_attrs: u32, // bitfield identifying set properties
    // void *attr;
    pub attr: Vec<TeeCryptObj>,
    pub ds_pos: size_t,
    pub pobj: Option<Arc<TeePobj>>,
    /// file handle for the pobject
    pub fh: Option<Box<TeeFsFd>>,
}

impl Debug for TeeObj {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let fd_dbg = self.fh.as_ref().map(|h| h.fd.fd).unwrap_or(-1);
        write!(
            f,
            "TeeObj{{info: {:?}, busy: {:?}, have_attrs: {:010X?}, attr: {:?}, ds_pos: {:010X?}, \
             pobj: {:?}, fh: {:?}}}",
            self.info, self.busy, self.have_attrs, self.attr, self.ds_pos, self.pobj, fd_dbg
        )
    }
}

impl default::Default for TeeObj {
    fn default() -> Self {
        TeeObj {
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

fn obj_inner_to_outer(obj: TeeObjIdType) -> TeeObjIdType {
    (obj + 1) as TeeObjIdType
}

fn obj_outer_to_inner(obj: TeeObjIdType) -> TeeObjIdType {
    debug_assert!(obj > 0);
    (obj - 1) as TeeObjIdType
}

pub fn tee_obj_add(mut obj: TeeObj) -> TeeResult<TeeObjIdType> {
    with_tee_session_ctx_mut(|ctx| {
        // 获取一个可用的 ID
        let vacant = ctx.objects.vacant_entry();
        let mut id = vacant.key();

        id = obj_inner_to_outer(id as TeeObjIdType) as usize;
        // 设置 objectId
        obj.info.objectId = id as u32;

        // 创建 Arc 并插入
        #[allow(clippy::arc_with_non_send_sync)]
        let arc_obj = Arc::new(Mutex::new(obj));
        vacant.insert(arc_obj);

        Ok(id as TeeObjIdType)
    })
}

pub fn tee_obj_get(obj_id: TeeObjIdType) -> TeeResult<Arc<Mutex<TeeObj>>> {
    let obj_id = obj_outer_to_inner(obj_id);
    with_tee_session_ctx(|ctx| match ctx.objects.get(obj_id as _) {
        Some(obj) => Ok(Arc::clone(obj)),
        None => Err(TEE_ERROR_ITEM_NOT_FOUND),
    })
}

/// delete the TeeObj from the session objects table
///
/// # Arguments
/// * `obj_id` - the id of the TeeObj
pub fn tee_obj_delete(obj_id: u32) -> TeeResult<Arc<Mutex<TeeObj>>> {
    let obj_id = obj_outer_to_inner(obj_id as TeeObjIdType);
    // remove from session objects
    with_tee_session_ctx_mut(|ctx| -> TeeResult<Arc<Mutex<TeeObj>>> {
        let obj = ctx
            .objects
            .try_remove(obj_id as _)
            .ok_or(TEE_ERROR_ITEM_NOT_FOUND)?;
        Ok(obj)
    })
}

/// close the TeeObj
///
/// 1. delete the TeeObj from the session objects table
/// 2. if the TeeObj is persistent, close the file handle and
/// 3. release the TeePobj
/// 4. free the TeeObj memory
///
/// # Arguments
/// * `obj_id` - the id of the TeeObj
/// # Returns
/// * `TeeResult` - the result of the operation
pub fn tee_obj_close(obj_id: u32) -> TeeResult {
    let obj = tee_obj_delete(obj_id as _)?;

    let mut obj_guard = obj.lock();
    if obj_guard.info.handleFlags & TEE_HANDLE_FLAG_PERSISTENT != 0 {
        // borrow checker will ensure the pobj is not used after the scope ends
        let (fops, pobj_clone) = {
            let pobj = obj_guard.pobj.as_ref().ok_or(TEE_ERROR_BAD_STATE)?;
            let fops = pobj.fops.ok_or(TEE_ERROR_BAD_STATE)?;
            let pobj_clone = Arc::clone(pobj);
            (fops, pobj_clone)
        };

        // now we can safely close the file handle if it was opened
        (fops.close)(&mut obj_guard.fh);
        tee_pobj_release(pobj_clone)?;
    }

    Ok(())
}

/// Close all open objects in the current session context.
///
/// Mirrors GP `tee_obj_close_all()` in `release_utc_state`.
pub fn tee_obj_close_all() -> TeeResult {
    let ids: Vec<u32> = with_tee_session_ctx(|ctx| {
        Ok(ctx
            .objects
            .iter()
            .map(|(k, _)| obj_inner_to_outer(k as TeeObjIdType) as u32)
            .collect())
    })?;
    for id in ids {
        if let Err(e) = tee_obj_close(id) {
            error!("tee_obj_close_all: tee_obj_close({id}): {e:#010X?}");
        }
    }
    Ok(())
}

#[unittest::def_test(custom)]
fn test_tee_obj_add_get() {
    let obj = TeeObj {
        busy: true,
        ..Default::default()
    };
    let obj_id = tee_obj_add(obj).expect("Failed to add TeeObj");
    info!("Added TeeObj with id {}", obj_id);
    let retrieved_obj = tee_obj_get(obj_id).expect("Failed to get TeeObj");
    assert!(retrieved_obj.lock().busy);
}
