// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, collections::VecDeque, string::String, sync::Arc};
use core::{
    default, fmt,
    fmt::Debug,
    ptr,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

use klazy::lazy_static;
use ksync::{Mutex, static_lock};
use tee_raw_sys::*;

use super::{TeeResult, tee_ree_fs::TeeFileOperations, uuid::Uuid};

static_lock! {
    static POBJS_MUTEX: Mutex<()> = Mutex::new(());
}
static_lock! {
    static POBJS_USAGE_MUTEX: Mutex<()> = Mutex::new(());
}
// static POBJS: LazyInit<Arc<Mutex<VecDeque<TeePobj>>>> = LazyInit::new();
lazy_static! {
    static ref POBJS: TeePobjs = TeePobjs::new();
}

#[derive(Debug, Default)]
pub(crate) struct ObjId {
    pub(crate) obj_id: Box<[u8]>,
    pub(crate) obj_id_len: u32,
}

#[repr(C)]
/// GP: `struct tee_pobj`
pub struct TeePobj {
    pub refcnt: AtomicU32,
    pub uuid: TEE_UUID,
    pub obj_id: Mutex<ObjId>,
    pub flags: AtomicU32,
    pub obj_info_usage: AtomicU32,
    pub temporary: AtomicBool, // can be changed while creating == true
    pub creating: AtomicBool,  // can only be changed with mutex held
    pub fops: Option<&'static TeeFileOperations>, // Filesystem handling this object
}

impl Debug for TeePobj {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let objid = self.obj_id.lock();
        let obj_id = String::from_utf8_lossy(&objid.obj_id[..objid.obj_id_len as usize]);
        let obj_id_len = objid.obj_id_len;
        let uuid: Uuid = self.uuid.into();

        write!(
            f,
            "TeePobj{{refcnt: {:?}, uuid: {}, obj_id: {:?}, obj_id_len: {:?}, flags: {:#010X?}, \
             obj_info_usage: {:010X?}, temporary: {:?}, creating: {:?}, fops: {:?}}}",
            self.refcnt.load(Ordering::Relaxed),
            uuid,
            obj_id,
            obj_id_len,
            self.flags.load(Ordering::Relaxed),
            self.obj_info_usage.load(Ordering::Relaxed),
            self.temporary.load(Ordering::Relaxed),
            self.creating.load(Ordering::Relaxed),
            self.fops.as_ref().map(|fops| fops.name).unwrap_or("None"),
        )
    }
}

impl default::Default for TeePobj {
    fn default() -> Self {
        TeePobj {
            refcnt: AtomicU32::new(0),
            uuid: TEE_UUID::default(),
            obj_id: Mutex::new(ObjId {
                obj_id: Box::new([]),
                obj_id_len: 0,
            }),
            flags: AtomicU32::new(0),
            obj_info_usage: AtomicU32::new(0),
            temporary: AtomicBool::new(false),
            creating: AtomicBool::new(false),
            fops: None,
        }
    }
}

impl TeePobj {
    /// Check if the TeePobj matches the given parameters
    ///
    /// # Arguments
    /// * `uuid` - The UUID of the object
    /// * `obj_id` - The object ID
    /// * `obj_id_len` - The actual length of the object ID
    /// * `fops` - The reference to the TeeFileOperations struct
    pub fn matches(
        &self,
        uuid: &TEE_UUID,
        obj_id: &[u8],
        obj_id_len: u32,
        fops: &Option<&'static TeeFileOperations>,
    ) -> bool {
        let objid = self.obj_id.lock();

        // check obj_id_len
        if objid.obj_id_len != obj_id_len {
            return false;
        }
        // check obj_id
        if objid.obj_id[..obj_id_len as usize] != obj_id[..obj_id_len as usize] {
            return false;
        }
        // check uuid
        if self.uuid != *uuid {
            return false;
        }
        // info!("matches fops with {:?}, {:?}", self.fops, fops);
        // check fops, using ptr::eq
        match (&self.fops, fops) {
            (Some(a), Some(b)) => {
                // info!("matches fops: {:p}, {:p}", *a as *const _, *b as *const _);
                ptr::eq(*a, *b)
                // info!("matches fops result: {}", result);
            }
            (None, None) => true,
            _ => false,
        }
    }
}

/// GP: `struct tee_pobjs`
#[derive(Debug)]
pub struct TeePobjs {
    inner: Mutex<VecDeque<Arc<TeePobj>>>,
}

impl TeePobjs {
    /// Create a new TeePobjs
    pub fn new() -> Self {
        TeePobjs {
            inner: Mutex::new(VecDeque::new()),
        }
    }

    /// Find a TeePobj in the collection
    ///
    /// # Arguments
    /// * `uuid` - The UUID of the object
    /// * `obj_id` - The object ID
    /// * `obj_id_len` - The actual length of the object ID
    /// * `fops` - The reference to the TeeFileOperations struct
    pub fn find_pobj(
        &self,
        uuid: &TEE_UUID,
        obj_id: &[u8],
        obj_id_len: u32,
        fops: &Option<&'static TeeFileOperations>,
    ) -> Option<Arc<TeePobj>> {
        let pobjs = self.inner.lock();
        pobjs
            .iter()
            .find(|pobj_arc| pobj_arc.matches(uuid, obj_id, obj_id_len, fops))
            .map(Arc::clone)
    }
}

/// GP: `tee_pobj_usage`
#[derive(PartialEq, Debug)]
pub enum TeePobjUsage {
    /// GP: `TEE_POBJ_USAGE_OPEN`
    Open      = 0,
    /// GP: `TEE_POBJ_USAGE_RENAME`
    Rename    = 1,
    /// GP: `TEE_POBJ_USAGE_CREATE`
    Create    = 2,
    /// GP: `TEE_POBJ_USAGE_ENUM`
    Enumerate = 3,
}

/// Check if the TeePobj needs usage lock
///
/// # Arguments
/// * `obj` - The TeePobj
fn pobj_need_usage_lock(flags: u32) -> bool {
    flags & (TEE_DATA_FLAG_SHARE_WRITE | TEE_DATA_FLAG_SHARE_READ) != 0
}

/// With usage lock
///
/// # Arguments
/// * `obj` - The TeePobj
/// * `f` - The function to execute
pub fn with_pobj_usage_lock<R, F>(flags: u32, f: F) -> R
where
    F: FnOnce() -> R,
{
    if pobj_need_usage_lock(flags) {
        let _guard = POBJS_USAGE_MUTEX.lock();
        tee_debug!("with_pobj_usage_lock: POBJS_USAGE_MUTEX locked");
        f()
    } else {
        tee_debug!("with_pobj_usage_lock: POBJS_USAGE_MUTEX not locked");
        f()
    }
}

/// Check if the new access flags conflict with the existing flags.
///
/// Implements the TEE Internal Core API Specification v1.1 rules:
/// - `TEE_DATA_FLAG_ACCESS_WRITE_META` is exclusive
/// - If any handle has `ACCESS_READ`, all handles must have `SHARE_READ`
/// - If any handle has `ACCESS_WRITE`, all handles must have `SHARE_WRITE`
/// - `SHARE_READ` / `SHARE_WRITE` flags must be consistent across handles
fn tee_pobj_check_access(oflags: u32, nflags: u32) -> TeeResult {
    // meta is exclusive
    if (oflags & TEE_DATA_FLAG_ACCESS_WRITE_META) != 0
        || (nflags & TEE_DATA_FLAG_ACCESS_WRITE_META) != 0
    {
        return Err(TEE_ERROR_ACCESS_CONFLICT);
    }

    // If more than one handle is opened on the same object, and if any
    // of these object handles was opened with the flag
    // TEE_DATA_FLAG_ACCESS_READ, then all the object handles MUST have been
    // opened with the flag TEE_DATA_FLAG_SHARE_READ
    if ((oflags & TEE_DATA_FLAG_ACCESS_READ) != 0 || (nflags & TEE_DATA_FLAG_ACCESS_READ) != 0)
        && !((nflags & TEE_DATA_FLAG_SHARE_READ) != 0 && (oflags & TEE_DATA_FLAG_SHARE_READ) != 0)
    {
        return Err(TEE_ERROR_ACCESS_CONFLICT);
    }

    // An object can be opened with only share flags, which locks the access
    // to an object against a given mode.
    // An object can be opened with no flag set, which completely locks all
    // subsequent attempts to access the object
    if (nflags & TEE_DATA_FLAG_SHARE_READ) != (oflags & TEE_DATA_FLAG_SHARE_READ) {
        return Err(TEE_ERROR_ACCESS_CONFLICT);
    }

    // Same on WRITE access
    if ((oflags & TEE_DATA_FLAG_ACCESS_WRITE) != 0 || (nflags & TEE_DATA_FLAG_ACCESS_WRITE) != 0)
        && !((nflags & TEE_DATA_FLAG_SHARE_WRITE) != 0 && (oflags & TEE_DATA_FLAG_SHARE_WRITE) != 0)
    {
        return Err(TEE_ERROR_ACCESS_CONFLICT);
    }
    if (nflags & TEE_DATA_FLAG_SHARE_WRITE) != (oflags & TEE_DATA_FLAG_SHARE_WRITE) {
        return Err(TEE_ERROR_ACCESS_CONFLICT);
    }

    Ok(())
}

/// Get a TeePobj from the collection
///
/// # Arguments
/// * `uuid` - The UUID of the object
/// * `obj_id` - The object ID
/// * `obj_id_len` - The actual length of the object ID
/// * `flags` - The flags of the object
/// * `usage` - The usage of the TeePobj
/// * `fops` - The reference to the TeeFileOperations struct
///
/// # Returns
/// * The TeePobj, can safe shared reference
pub fn tee_pobj_get(
    uuid: &TEE_UUID,
    obj_id: &[u8],
    obj_id_len: u32,
    flags: u32,
    usage: TeePobjUsage,
    fops: &'static TeeFileOperations,
) -> TeeResult<Arc<TeePobj>> {
    // Serialize metadata operations to match the GP reference's pobj mutex model.
    let _guard = POBJS_MUTEX.lock();

    // info!(
    //     "tee_pobj_get: uuid: {:x?}, obj_id: {:x?}, obj_id_len: {}, flags: {}, usage: {:?}, fops: \
    //      {:p}",
    //     uuid, obj_id, obj_id_len, flags, usage, fops as *const _
    // );
    // lock the pobjs
    if let Some(obj) = POBJS.find_pobj(uuid, obj_id, obj_id_len, &Some(fops)) {
        let creating = obj.creating.load(Ordering::Relaxed);
        // Enumeration only holds a temporary pobj reference while reading object
        // metadata (flags are always zero). It is not an open with access flags,
        // so skip creating and access-conflict checks; only bump refcnt.
        if usage == TeePobjUsage::Enumerate {
            obj.refcnt.fetch_add(1, Ordering::Relaxed);
            return Ok(Arc::clone(&obj));
        }

        if creating || (usage == TeePobjUsage::Create && (flags & TEE_DATA_FLAG_OVERWRITE) == 0) {
            return Err(TEE_ERROR_ACCESS_CONFLICT);
        }

        let oflags = obj.flags.load(Ordering::Relaxed);
        tee_pobj_check_access(oflags, flags)?;
        let _prev = obj.refcnt.fetch_add(1, Ordering::Relaxed);
        return Ok(Arc::clone(&obj));
    }

    // new file
    let obj = TeePobj {
        refcnt: AtomicU32::new(1),
        uuid: *uuid,
        obj_id: Mutex::new(ObjId {
            obj_id: obj_id[..obj_id_len as usize].to_vec().into_boxed_slice(),
            obj_id_len,
        }),
        flags: AtomicU32::new(flags),
        fops: Some(fops),
        temporary: AtomicBool::new(usage == TeePobjUsage::Create),
        creating: AtomicBool::new(usage == TeePobjUsage::Create),
        obj_info_usage: AtomicU32::new(0),
    };

    // add to pobjs (still under POBJS_MUTEX thanks to the guard above)
    let mut pobjs = POBJS.inner.lock();
    let pobj = Arc::new(obj);
    pobjs.push_back(pobj.clone());
    Ok(pobj)
}

pub fn tee_pobj_create_final(po: &TeePobj) {
    let _guard = POBJS_MUTEX.lock();
    po.temporary.store(false, Ordering::Relaxed);
    po.creating.store(false, Ordering::Relaxed);
}

/// Release a TeePobj
///
/// if no reference to the TeePobj, the TeePobj will be removed from the collection POBJS.
///
/// # Arguments
/// * `obj` - The TeePobj
/// # Returns
/// * `TeeResult` - the result of the operation
pub fn tee_pobj_release(obj: Arc<TeePobj>) -> TeeResult {
    let _guard = POBJS_MUTEX.lock();
    let prev = obj.refcnt.fetch_sub(1, Ordering::Relaxed);
    if prev == 0 {
        warn!("tee_pobj_release: refcnt already 0");
        return Ok(());
    }

    let next_is_zero = prev == 1;
    tee_debug!(
        "tee_pobj_release: obj.refcnt from: {:?} to: {:?}",
        prev,
        prev - 1
    );

    if next_is_zero {
        let mut pobjs = POBJS.inner.lock();
        pobjs.retain(|pobj_arc| !Arc::ptr_eq(pobj_arc, &obj));
    }
    Ok(())
}

/// Rename a TeePobj
///
/// # Arguments
/// * `obj` - The TeePobj
/// * `obj_id` - The new object ID
/// * `obj_id_len` - The actual length of the new object ID
/// # Returns
/// * `TeeResult` - the result of the operation
pub fn tee_pobj_rename(obj: &TeePobj, obj_id: &[u8], obj_id_len: u32) -> TeeResult {
    let _guard = POBJS_MUTEX.lock();

    let refcnt = obj.refcnt.load(Ordering::Relaxed);
    if refcnt != 1 {
        return Err(TEE_ERROR_BAD_STATE);
    }

    // check obj_id_len is not greater than obj_id length
    if obj_id_len as usize > obj_id.len() {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    let mut objid = obj.obj_id.lock();
    objid.obj_id = obj_id[..obj_id_len as usize].to_vec().into_boxed_slice();
    objid.obj_id_len = obj_id_len;
    Ok(())
}

#[unittest::mod_test]
mod tests {
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
}
