// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, string::String, sync::Arc, vec, vec::Vec};
use core::{
    ffi::{c_uint, c_ulong, c_void},
    mem::{offset_of, size_of, size_of_val},
};

use bytemuck::{Pod, Zeroable, bytes_of_mut};
use klazy::Once;
use ksync::Mutex;
use osvm::MemError;
use posix_types::{UserConstPtr, UserPtr};
use tee_raw_sys::*;

use super::{
    common::file_ops::FileVariant,
    fs_dirfile::{TeeFsDirfileFileh, tee_fs_dirfile_fileh_to_fname},
    tee_fs::TeeFsDirent,
    tee_obj::{TeeObj, TeeObjIdType, tee_obj_add, tee_obj_close, tee_obj_get},
    tee_pobj::{
        TeePobj, TeePobjUsage, tee_pobj_create_final, tee_pobj_get, tee_pobj_release,
        tee_pobj_rename, with_pobj_usage_lock,
    },
    tee_ree_fs::{TeeFileOperations, TeeFsDir, tee_svc_storage_file_ops},
    tee_session::{with_tee_session_ctx, with_tee_session_ctx_mut, with_tee_ta_ctx},
    tee_svc_cryp::{
        tee_obj_attr_copy_from, tee_obj_attr_from_binary, tee_obj_attr_to_binary, tee_obj_set_type,
    },
    uuid::Uuid,
};
use crate::tee::TeeResult;

fn map_user_mem_error(err: MemError) -> u32 {
    match err {
        MemError::InvalidAddr | MemError::NoAccess => TEE_ERROR_BAD_PARAMETERS,
        _ => TEE_ERROR_GENERIC,
    }
}

fn read_user_bytes_optional(addr: *const c_void, len: usize) -> TeeResult<Box<[u8]>> {
    if len == 0 {
        return Ok(Box::new([]));
    }
    if addr.is_null() {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    Ok(UserConstPtr::<u8>::from(addr.cast::<u8>())
        .load_vm_vec(len)
        .map_err(map_user_mem_error)?
        .into_boxed_slice())
}

fn write_user_bytes(addr: *mut c_void, data: &[u8]) -> TeeResult {
    if data.is_empty() {
        return Ok(());
    }
    if addr.is_null() {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    UserPtr::<u8>::from(addr.cast::<u8>())
        .write_vm_slice(data)
        .map_err(map_user_mem_error)
}

/// Copy object id from TA user memory. `object_id_len == 0` is valid (GP).
fn bb_memdup_object_id_from_user(
    object_id: *const c_void,
    object_id_len: usize,
) -> TeeResult<Box<[u8]>> {
    read_user_bytes_optional(object_id, object_id_len)
}

#[cfg(unittest)]
pub const TEE_UUID_HEX_LEN: usize = size_of::<TEE_UUID>();

pub static ROOT_TEE_FS_INIT: Once<()> = Once::new();
/// GP persistent object header on storage media.
///
/// GP: `struct tee_svc_storage_head`
#[repr(C)]
#[derive(Copy, Clone, Default, Pod, Zeroable)]
struct TeeSvcStorageHead {
    pub attr_size: u32,
    /// GP: `objectSize`
    pub object_size: u32,
    /// GP: `maxObjectSize`
    pub max_object_size: u32,
    /// GP: `objectUsage`
    pub object_usage: u32,
    /// GP: `objectType`
    pub object_type: u32,
    pub have_attrs: u32,
}

/// GP: `struct tee_storage_enum`
pub struct TeeStorageEnum {
    pub id: c_ulong,
    pub dir: Option<Box<TeeFsDir>>,
    pub fops: Option<&'static TeeFileOperations>,
}

pub fn tee_svc_storage_add_enum(mut obj: TeeStorageEnum) -> TeeResult<c_ulong> {
    with_tee_session_ctx_mut(|ctx| {
        // 获取一个可用的 ID
        let vacant = ctx.storage_enums.vacant_entry();
        let id = vacant.key();

        // 设置 objectId
        obj.id = id as c_ulong;

        // 创建 Arc 并插入
        #[allow(clippy::arc_with_non_send_sync)]
        let arc_obj = Arc::new(Mutex::new(obj));
        let _inserted_id = vacant.insert(arc_obj);
        tee_debug!("tee_svc_storage_add_enum: id: {}", id);

        Ok(id as c_ulong)
    })
}

fn tee_svc_storage_delete_enum(enum_id: c_ulong) -> TeeResult<Arc<Mutex<TeeStorageEnum>>> {
    // remove from session objects
    with_tee_session_ctx_mut(|ctx| -> TeeResult<Arc<Mutex<TeeStorageEnum>>> {
        let obj = ctx
            .storage_enums
            .try_remove(enum_id as _)
            .ok_or(TEE_ERROR_ITEM_NOT_FOUND)?;
        Ok(obj)
    })
}

fn tee_svc_storage_get_enum(enum_id: c_ulong) -> TeeResult<Arc<Mutex<TeeStorageEnum>>> {
    with_tee_session_ctx_mut(|ctx| {
        let e = ctx
            .storage_enums
            .get(enum_id as usize)
            .ok_or(TEE_ERROR_BAD_PARAMETERS)?;
        Ok(e.clone())
    })
}

fn tee_svc_close_enum(enum_id: c_ulong) -> TeeResult {
    let obj = tee_svc_storage_delete_enum(enum_id)?;

    // get the lock of the dir, and get the &mut TeeFsDir borrow
    let fops = {
        let obj_guard = obj.lock();
        obj_guard.fops
    };

    if let Some(fops) = fops {
        let mut obj_guard = obj.lock();
        if let Some(dir) = obj_guard.dir.as_mut() {
            (fops.closedir)(dir)?;
        }
    }

    // obj auto released when the scope ends
    Ok(())
}

/// Close all storage enumerators in the current session context.
///
/// Mirrors GP `tee_svc_storage_close_all_enum()` in `release_utc_state`.
pub fn tee_svc_storage_close_all_enum() -> TeeResult {
    let ids: Vec<c_ulong> = with_tee_session_ctx(|ctx| {
        Ok(ctx
            .storage_enums
            .iter()
            .map(|(k, _)| k as c_ulong)
            .collect())
    })?;
    for id in ids {
        if let Err(e) = tee_svc_close_enum(id) {
            error!("tee_svc_storage_close_all_enum: close_enum({id}): {e:#010X?}");
        }
    }
    Ok(())
}

/// 创建一个基于 TEE_UUID 的目录名。
///
/// 目录名格式为：`/` + UUID 的大写十六进制表示 + `\0` (用于 C 兼容)。
/// C 函数中的 +1 是为了 null 终止符。
/// 因此，所需的缓冲区大小是 TEE_UUID 的十六进制长度 + 1 (null 终止符)。
/// pub const TEE_DIRNAME_BUFFER_REQUIRED_LEN: usize = TEE_UUID_HEX_LEN * 2 + 1;
///
/// # 参数
/// * `buf` - 用于写入目录名的可变字节切片。
/// * `uuid` - 用于生成目录名的 `TEE_UUID`。
///
/// # 返回
/// `Ok(())` - 目录名成功写入 `buf`。
/// `Err(TeeError::ShortBuffer)` - 提供的 `buf` 缓冲区太小。
/// `Err(TeeError::Generic)` - 其他转换错误。
#[cfg(unittest)]
pub fn tee_svc_storage_create_dirname(buf: &mut [u8], uuid: &TEE_UUID) -> TeeResult {
    use super::tee_misc::{tee_b2hs, tee_b2hs_hsbuf_size};

    let required_len = tee_b2hs_hsbuf_size(TEE_UUID_HEX_LEN) + 1; // '/' + UUID_HEX_CHARS + '\0'

    if buf.len() < required_len {
        return Err(TEE_ERROR_SHORT_BUFFER);
    }

    buf[0] = b'/';

    let uuid_hex_start_idx = 1; // 从 buf 的第二个字节开始写入 UUID

    let mut uuid_bytes = [0u8; TEE_UUID_HEX_LEN];
    uuid_bytes[..4].copy_from_slice(&uuid.timeLow.to_ne_bytes());
    uuid_bytes[4..6].copy_from_slice(&uuid.timeMid.to_ne_bytes());
    uuid_bytes[6..8].copy_from_slice(&uuid.timeHiAndVersion.to_ne_bytes());
    uuid_bytes[8..].copy_from_slice(&uuid.clockSeqAndNode);

    tee_b2hs(&uuid_bytes, &mut buf[uuid_hex_start_idx..]).map_err(|_| TEE_ERROR_GENERIC)?;

    Ok(())
}

const CFG_TEE_FS_PARENT_PATH: &str = "/tee/";

pub fn tee_svc_storage_create_filename_dfh(
    buf: &mut [u8],
    dfh: Option<&TeeFsDirfileFileh>,
) -> TeeResult<usize> {
    let prefix = CFG_TEE_FS_PARENT_PATH;

    ROOT_TEE_FS_INIT.call_once(|| {
        let _ = FileVariant::create_dir(CFG_TEE_FS_PARENT_PATH);
    });
    if buf.len() < prefix.len() + 1 {
        return Err(TEE_ERROR_SHORT_BUFFER);
    }

    // 复制前缀
    buf[..prefix.len()].copy_from_slice(prefix.as_bytes());

    // 获取剩余部分用于文件名
    let remaining_buf = &mut buf[prefix.len()..];

    let filename_len = tee_fs_dirfile_fileh_to_fname(dfh, remaining_buf)?;

    Ok(prefix.len() + filename_len)
}

fn remove_corrupt_obj(o: &mut TeeObj) -> TeeResult {
    // remove the corrupt object from the session
    let pobj = o.pobj.as_ref().ok_or(TEE_ERROR_BAD_STATE)?;
    let fops = pobj.fops.ok_or(TEE_ERROR_BAD_STATE)?;
    // REE-FS remove takes &TeePobj
    (fops.remove)(pobj)?;
    // pobj.write().remove(pobj);

    Ok(())
}

fn tee_svc_storage_read_head(o: &mut TeeObj) -> TeeResult {
    tee_debug!("tee_svc_storage_read_head: o: {:?}", o);

    // 先获取 fops，然后立即释放读锁，避免与后续的写锁冲突
    let fops = o
        .pobj
        .as_ref()
        .ok_or(TEE_ERROR_BAD_STATE)?
        .fops
        .ok_or(TEE_ERROR_BAD_STATE)?;

    tee_debug!("tee_svc_storage_read_head: fops: {:?}", fops);
    let mut size: usize = 0;
    // open the file, store the file handle in TeeObj.fh
    let pobj = o.pobj.as_ref().ok_or(TEE_ERROR_BAD_STATE)?;
    o.fh = Some((fops.open)(pobj, Some(&mut size)).inspect_err(|e| {
        error!("open failed: {:X?}", e);
    })?);
    tee_debug!("tee_svc_storage_read_head: size: {}", size);
    // read the head
    let mut head = TeeSvcStorageHead::zeroed();
    let head_slice = bytes_of_mut(&mut head);
    let mut bytes: usize = head_slice.len();
    let fh = o.fh.as_mut().ok_or(TEE_ERROR_BAD_STATE)?;
    (fops.read)(fh, 0, head_slice, &mut [], &mut bytes).inspect_err(|e| {
        if *e == TEE_ERROR_CORRUPT_OBJECT {
            error!("head corrupt");
        }
    })?;

    // check size overflow
    let tmp = (head.attr_size as usize)
        .checked_add(size_of_val(&head))
        .ok_or(TEE_ERROR_OVERFLOW)?;

    if tmp > size {
        return Err(TEE_ERROR_CORRUPT_OBJECT);
    }

    tee_debug!(
        "bytes: {}, size_of_val(&head): {}",
        bytes,
        size_of_val(&head)
    );
    if bytes != size_of_val(&head) {
        error!(
            "bytes != size_of_val(&head): {} != {}",
            bytes,
            size_of_val(&head)
        );
        return Err(TEE_ERROR_BAD_FORMAT);
    }

    tee_obj_set_type(o, head.object_type as _, head.max_object_size as _)?;
    o.ds_pos = tmp;

    // Read attr data if attr_size > 0, otherwise use empty slice
    let attr_data = if head.attr_size > 0 {
        let mut attr = vec![0u8; head.attr_size as usize];
        // read meta
        bytes = head.attr_size as usize;
        (fops.read)(
            o.fh.as_mut().ok_or(TEE_ERROR_BAD_STATE)?,
            size_of_val(&head),
            &mut attr,
            &mut [],
            &mut bytes,
        )
        .inspect_err(|e| {
            if *e == TEE_ERROR_CORRUPT_OBJECT {
                error!("attr corrupt");
            }
        })?;

        if bytes != head.attr_size as usize {
            return Err(TEE_ERROR_CORRUPT_OBJECT);
        }

        attr
    } else {
        vec![]
    };

    tee_obj_attr_from_binary(o, &attr_data)?;

    o.info.dataSize = size - size_of_val(&head) - head.attr_size as usize;
    o.info.objectSize = head.object_size;
    // Update persistent object's usage (atomic in A2).
    o.pobj
        .as_ref()
        .ok_or(TEE_ERROR_BAD_STATE)?
        .obj_info_usage
        .store(head.object_usage, core::sync::atomic::Ordering::Relaxed);
    o.info.objectType = head.object_type;
    o.have_attrs = head.have_attrs;

    Ok(())
}

/// Open a storage object
///
/// # Arguments
/// * `storage_id` - The storage ID
/// * `object_id` - The object ID
/// * `object_id_len` - The actual length of the object ID
/// * `flags` - The flags of the object
/// * `obj` - The object handle
///
/// # Returns
/// * The tee_obj_id
///
/// TODO: need add remove_corrupt_obj() while TEE_ERROR_CORRUPT_OBJECT
pub fn syscall_storage_obj_open(
    storage_id: c_ulong,
    object_id: *mut c_void,
    object_id_len: usize,
    flags: c_ulong,
    obj: *mut c_uint,
) -> TeeResult {
    tee_debug!(
        "syscall_storage_obj_open: storage_id: {:X?}, object_id: {:?}, object_id_len: {:X?}, \
         flags: {:X?}, obj: {:X?}",
        storage_id,
        object_id,
        object_id_len,
        flags,
        obj
    );
    let valid_flags: c_ulong = (TEE_DATA_FLAG_ACCESS_READ
        | TEE_DATA_FLAG_ACCESS_WRITE
        | TEE_DATA_FLAG_ACCESS_WRITE_META
        | TEE_DATA_FLAG_SHARE_READ
        | TEE_DATA_FLAG_SHARE_WRITE) as c_ulong;
    let fops =
        tee_svc_storage_file_ops(storage_id as c_uint).map_err(|_| TEE_ERROR_ITEM_NOT_FOUND)?;

    tee_debug!(
        "syscall_storage_obj_open: flags: {:#010X?}, valid_flags: {:#010X?}",
        flags,
        valid_flags
    );
    if flags & !valid_flags != 0 {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    if object_id_len > TEE_OBJECT_ID_MAX_LEN as usize {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    if object_id_len != 0 && object_id.is_null() {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    let oid_bbuf = bb_memdup_object_id_from_user(object_id, object_id_len)?;

    let uuid = with_tee_ta_ctx(|ctx| Ok(ctx.uuid.clone()))?;
    let uuid = Uuid::parse_str(&uuid)?;

    tee_debug!("syscall_storage_obj_open: step 1 : tee_pobj_get");
    let po = tee_pobj_get(
        uuid.as_raw_ref(),
        &oid_bbuf,
        object_id_len as u32,
        flags as u32,
        TeePobjUsage::Open,
        fops,
    )?;

    let mut o = TeeObj::default();

    tee_debug!("syscall_storage_obj_open: step 2 : tee_obj_add");
    // set handleFlags
    o.info.handleFlags = TEE_HANDLE_FLAG_PERSISTENT | TEE_HANDLE_FLAG_INITIALIZED | flags as u32;
    o.pobj = Some(po.clone());
    let tee_obj_id: u32 = tee_obj_add(o)? as u32;

    let o_arc = tee_obj_get(tee_obj_id as TeeObjIdType)?;
    tee_debug!("o_arc: {:?}", o_arc);
    // 只需要 flags 的原子读取即可（避免 pobj 的读写锁语义）
    let pobj_flags = po.flags.load(core::sync::atomic::Ordering::Relaxed);
    let obj_open = (|| -> TeeResult {
        tee_debug!("syscall_storage_obj_open: step 3 : tee_svc_storage_read_head");
        with_pobj_usage_lock(pobj_flags, || -> TeeResult {
            // TODO: implement call tee_svc_storage_read_head();
            tee_svc_storage_read_head(&mut o_arc.lock())
            // check if need call tee_obj_close()
            // Ok(())
        })?;

        UserPtr::<c_uint>::from(obj)
            .write_vm(tee_obj_id)
            .map_err(map_user_mem_error)?;

        Ok(())
    })();

    if let Err(err) = obj_open
        && err != TEE_ERROR_CORRUPT_OBJECT
    {
        let _ = tee_obj_close(tee_obj_id).inspect_err(|e| {
            error!("tee_obj_close failed: {:X?}", e);
        });
        return Err(err);
    }

    Ok(())
}

fn tee_svc_storage_init_file(
    o: &mut TeeObj,
    overwrite: bool,
    attr_o: Option<&mut TeeObj>,
    src_is_dst: bool,
    data: &[u8],
) -> TeeResult {
    let fops = {
        let pobj = o.pobj.as_ref().ok_or(TEE_ERROR_BAD_STATE)?;
        pobj.fops.ok_or(TEE_ERROR_BAD_STATE)?
    };

    let mut attr_size = 0;
    let mut attr: Box<[u8]> = Vec::<u8>::new().into_boxed_slice();
    if let Some(attr_o) = attr_o {
        if !src_is_dst {
            tee_obj_set_type(o, attr_o.info.objectType, attr_o.info.maxObjectSize as _)?;

            tee_obj_attr_copy_from(o, attr_o)?;
            o.have_attrs = attr_o.have_attrs;
            o.pobj
                .as_ref()
                .ok_or(TEE_ERROR_BAD_STATE)?
                .obj_info_usage
                .store(
                    attr_o.info.objectUsage,
                    core::sync::atomic::Ordering::Relaxed,
                );
            o.info.objectSize = attr_o.info.objectSize;
        }
        tee_obj_attr_to_binary(o, &mut [], &mut attr_size)?;
        if attr_size > 0 {
            attr = vec![0u8; attr_size].into_boxed_slice();
            tee_obj_attr_to_binary(o, &mut attr, &mut attr_size)?;
        }
    } else {
        tee_obj_set_type(o, TEE_TYPE_DATA, 0)?;
    }

    o.ds_pos = size_of::<TeeSvcStorageHead>() + attr_size;

    // write head
    let mut head = TeeSvcStorageHead {
        attr_size: attr_size as u32,
        object_size: o.info.objectSize,
        max_object_size: o.info.maxObjectSize,
        object_type: o.info.objectType,
        have_attrs: o.have_attrs,
        ..Default::default()
    };

    head.object_usage = o
        .pobj
        .as_ref()
        .ok_or(TEE_ERROR_BAD_STATE)?
        .obj_info_usage
        .load(core::sync::atomic::Ordering::Relaxed);
    o.fh = Some(
        (fops.create)(
            o.pobj.as_ref().ok_or(TEE_ERROR_BAD_STATE)?.as_ref(),
            overwrite,
            bytemuck::bytes_of(&head),
            &attr,
            &[],
            data,
            data.len(),
        )
        .inspect_err(|e| {
            o.ds_pos = 0;
            error!("create failed: {:X?}", e);
        })?,
    );
    o.info.dataSize = data.len();

    Ok(())
}

/// inner result for syscall_storage_obj_create_inner
enum CreateInnerResult {
    /// 成功：转换已有对象为持久化对象（第一分支）
    ConvertedExisting,
    /// 成功：创建了新的持久化对象，返回 object id
    CreatedNew(u32),
    /// 失败：在 tee_obj_add 之前失败，需要清理 o.fh 和 po
    ErrBeforeAdd {
        error: u32,
        po: Option<Arc<TeePobj>>,
        o: Option<TeeObj>,
    },
}

/// inner context for syscall_storage_obj_create_inner
struct CreateInnerCtx {
    po: Option<Arc<TeePobj>>,
}

/// inner function: execute the core logic, return the result or the resources to clean up
///
/// # Arguments
/// * `ctx` - the inner context
/// * `flags` - the flags
/// * `attr` - the attribute
/// * `data` - the data
/// * `obj_is_null` - whether the object is null
/// # Returns
/// * `CreateInnerResult` - the result of the operation
fn syscall_storage_obj_create_inner(
    ctx: &mut CreateInnerCtx,
    flags: c_ulong,
    attr: c_ulong,
    data: &[u8],
    obj_is_null: bool,
) -> CreateInnerResult {
    // === 获取 attr_o ===
    let attr_o = if attr != TEE_HANDLE_NULL as c_ulong {
        match tee_obj_get(attr) {
            Ok(o) => {
                let guard = o.lock();
                if guard.info.handleFlags & TEE_HANDLE_FLAG_INITIALIZED == 0 {
                    return CreateInnerResult::ErrBeforeAdd {
                        error: TEE_ERROR_BAD_PARAMETERS,
                        po: ctx.po.take(),
                        o: None,
                    };
                }
                drop(guard);
                Some(o)
            }
            Err(e) => {
                return CreateInnerResult::ErrBeforeAdd {
                    error: e,
                    po: ctx.po.take(),
                    o: None,
                };
            }
        }
    } else {
        None
    };

    // === C: if (!obj && attr_o && !PERSISTENT) - 转换已有对象 ===
    let mut attr_o = attr_o;
    if obj_is_null {
        let convert_existing = attr_o
            .as_ref()
            .is_some_and(|o| o.lock().info.handleFlags & TEE_HANDLE_FLAG_PERSISTENT == 0);
        if convert_existing {
            // convert temporary object to persistent object
            // 1. obj == null means caller does not need to return a new handle(cause handle exists)
            // 2. attr_o != null means attributes object is provided(attr != TEE_HANDLE_NULL)
            // 3. TEE_HANDLE_FLAG_PERSISTENT == 0 means attributes object is not a persistent object
            let Some(attr_o) = attr_o.take() else {
                return CreateInnerResult::ErrBeforeAdd {
                    error: TEE_ERROR_BAD_STATE,
                    po: ctx.po.take(),
                    o: None,
                };
            };
            let mut a = attr_o.lock();

            let saved_flags = a.info.handleFlags;
            a.info.handleFlags =
                TEE_HANDLE_FLAG_PERSISTENT | TEE_HANDLE_FLAG_INITIALIZED | flags as u32;

            // 转移 po 所有权给 attr_o
            let Some(po_for_attr) = ctx.po.take() else {
                a.info.handleFlags = saved_flags;
                return CreateInnerResult::ErrBeforeAdd {
                    error: TEE_ERROR_BAD_STATE,
                    po: None,
                    o: None,
                };
            };

            po_for_attr
                .obj_info_usage
                .store(a.info.objectUsage, core::sync::atomic::Ordering::Relaxed);
            a.pobj = Some(po_for_attr);

            if let Err(e) = tee_svc_storage_init_file(
                &mut a,
                (flags & TEE_DATA_FLAG_OVERWRITE as u64) != 0,
                Some(&mut TeeObj::default()),
                true,
                data,
            ) {
                // 恢复状态
                let po_back = a.pobj.take();
                a.info.handleFlags = saved_flags;
                return CreateInnerResult::ErrBeforeAdd {
                    error: e,
                    po: po_back,
                    o: None,
                };
            }

            a.info.objectUsage = 0;
            return CreateInnerResult::ConvertedExisting;
        }
    }

    // === 创建新 persistent object ===
    let mut o = TeeObj::default();
    o.info.handleFlags = TEE_HANDLE_FLAG_PERSISTENT | TEE_HANDLE_FLAG_INITIALIZED | flags as u32;

    // 转移 po 所有权给 o
    let Some(po_for_o) = ctx.po.take() else {
        return CreateInnerResult::ErrBeforeAdd {
            error: TEE_ERROR_BAD_STATE,
            po: None,
            o: Some(o),
        };
    };
    o.pobj = Some(po_for_o.clone());

    let init_result = if let Some(attr_o) = attr_o {
        let mut a = attr_o.lock();
        tee_svc_storage_init_file(
            &mut o,
            (flags & TEE_DATA_FLAG_OVERWRITE as u64) != 0,
            Some(&mut a),
            false,
            data,
        )
    } else {
        tee_svc_storage_init_file(
            &mut o,
            (flags & TEE_DATA_FLAG_OVERWRITE as u64) != 0,
            None,
            false,
            data,
        )
    };

    if let Err(e) = init_result {
        // 失败时，po 所有权在 o.pobj 中，需要取出来返回
        let po_back = o.pobj.take();
        return CreateInnerResult::ErrBeforeAdd {
            error: e,
            po: po_back,
            o: Some(o),
        };
    }

    o.info.objectUsage = 0;

    let o_id = match tee_obj_add(o) {
        Ok(id) => id as u32,
        Err(e) => {
            // tee_obj_add 失败比较特殊，o 的所有权已经被 move 进去了
            // 这种情况下 o 不会被添加到表中，但我们也无法拿回来
            // 实际上 tee_obj_add 不太可能失败（只是 slab insert）
            return CreateInnerResult::ErrBeforeAdd {
                error: e,
                po: Some(po_for_o),
                o: None, // o 已经被 move 了
            };
        }
    };

    // 成功添加到全局表，返回 o_id
    CreateInnerResult::CreatedNew(o_id)
}

/// create a new persistent object
///
/// # Arguments
/// * `storage_id` - the storage id
/// * `object_id` - the object id
/// * `object_id_len` - the length of the object id
/// * `flags` - the flags
/// * `attr` - the attribute
/// * `data` - the data
/// * `len` - the length of the data
/// * `obj` - the object
/// # Returns
/// * `TeeResult` - the result of the operation
#[allow(clippy::too_many_arguments)]
pub fn syscall_storage_obj_create(
    storage_id: c_ulong,
    object_id: *mut c_void,
    object_id_len: usize,
    flags: c_ulong,
    attr: c_ulong,
    data: *mut c_void,
    len: usize,
    obj: *mut c_uint,
) -> TeeResult {
    tee_debug!(
        "syscall_storage_obj_create: storage_id: {:X?}, object_id: {:?}, object_id_len: {:X?}, \
         flags: {:X?}, attr: {:X?}, data: {:?}, len: {:X?}, obj: {:X?}",
        storage_id,
        object_id,
        object_id_len,
        flags,
        attr,
        data,
        len,
        obj
    );
    const VALID_FLAGS: c_ulong = (TEE_DATA_FLAG_ACCESS_READ
        | TEE_DATA_FLAG_ACCESS_WRITE
        | TEE_DATA_FLAG_ACCESS_WRITE_META
        | TEE_DATA_FLAG_SHARE_READ
        | TEE_DATA_FLAG_SHARE_WRITE
        | TEE_DATA_FLAG_OVERWRITE) as _;

    // === 参数校验（这些错误不需要资源清理）===
    if flags & !VALID_FLAGS != 0 {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    let fops = tee_svc_storage_file_ops(storage_id as _).map_err(|_| TEE_ERROR_ITEM_NOT_FOUND)?;

    if object_id_len > TEE_OBJECT_ID_MAX_LEN as usize {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    // Check presence of optional buffer
    if len != 0 && data.is_null() {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    tee_debug!(
        "syscall_storage_obj_create object_id_len: {:X?}",
        object_id_len
    );
    if object_id_len != 0 && object_id.is_null() {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    let oid_bbuf = bb_memdup_object_id_from_user(object_id, object_id_len)?;

    let uuid = with_tee_ta_ctx(|ctx| Ok(ctx.uuid.clone()))
        .inspect_err(|e| error!("with_tee_ta_ctx error: {:#X?}", e))?;
    tee_debug!("uuid: {:?}", uuid);
    let uuid = Uuid::parse_str(&uuid)?;

    // === tee_pobj_get - need resource cleanup from here ===
    let po = tee_pobj_get(
        uuid.as_raw_ref(),
        &oid_bbuf,
        object_id_len as u32,
        flags as u32,
        TeePobjUsage::Create,
        fops,
    )?;
    tee_debug!("syscall_storage_obj_create: tee_pobj_get po: {:?}", po);

    let data_buf = read_user_bytes_optional(data.cast_const(), len)?;

    // === call inner function ===
    let mut inner_ctx = CreateInnerCtx {
        po: Some(po.clone()),
    };

    let result =
        syscall_storage_obj_create_inner(&mut inner_ctx, flags, attr, &data_buf, obj.is_null());

    // === 根据结果处理 ===
    match result {
        CreateInnerResult::ConvertedExisting => {
            // 第一分支成功，po 所有权已转移给 attr_o
            Ok(())
        }

        CreateInnerResult::CreatedNew(o_id) => {
            // 第二分支成功，继续处理
            if !obj.is_null()
                && let Err(e) = UserPtr::<c_uint>::from(obj)
                    .write_vm(o_id)
                    .map_err(map_user_mem_error)
            {
                // oclose 路径：C 逻辑中 oclose 不进行错误码转换
                let _ = tee_obj_close(o_id);
                return Err(e);
            }

            tee_pobj_create_final(&po);

            if obj.is_null() {
                tee_obj_close(o_id)?;
            }

            Ok(())
        }

        CreateInnerResult::ErrBeforeAdd { error, po, o } => {
            // err: 路径
            let error = convert_error(error);

            if let Some(mut o) = o {
                (fops.close)(&mut o.fh);
                // o 会在这里 drop
            }

            if error == TEE_ERROR_CORRUPT_OBJECT
                && let Some(ref po_ref) = po
            {
                tee_debug!("CreateInnerResult::ErrBeforeAdd: fops.remove");
                let _ = (fops.remove)(po_ref);
            }

            if let Some(po) = po {
                let _ = tee_pobj_release(po);
            }

            Err(error)
        }
    }
}

fn convert_error(mut e: u32) -> u32 {
    if e == TEE_ERROR_NO_DATA || e == TEE_ERROR_BAD_FORMAT {
        e = TEE_ERROR_CORRUPT_OBJECT;
    }
    e
}

/// delete a persistent object
///
/// # Arguments
/// * `obj_id` - the object id
/// # Returns
/// * `TeeResult` - the result of the operation
pub fn syscall_storage_obj_del(obj_id: c_ulong) -> TeeResult {
    tee_debug!("syscall_storage_obj_del: obj_id: {:X?}", obj_id);
    let o = tee_obj_get(obj_id)?;

    // check permission and get necessary information, then release o_guard immediately
    let (pobj_arc, fops) = {
        let o_guard = o.lock();

        if o_guard.info.handleFlags & TEE_DATA_FLAG_ACCESS_WRITE_META == 0 {
            return Err(TEE_ERROR_ACCESS_CONFLICT);
        }

        let pobj_arc = o_guard.pobj.as_ref().ok_or(TEE_ERROR_BAD_STATE)?.clone();

        let fops = { pobj_arc.fops.ok_or(TEE_ERROR_BAD_STATE)? };

        // clone pobj_arc and get fops before releasing o_guard
        (pobj_arc, fops)
    };

    // now it is safe to get the write lock of pobj, because o_guard is released
    let res = (fops.remove)(&pobj_arc);

    // now it is safe to call tee_obj_close, because all locks are released
    let _ = tee_obj_close(obj_id as u32);

    res
}

/// rename a persistent object
///
/// # Arguments
/// * `obj` - the object id
/// * `object_id` - the new object id
/// * `object_id_len` - the length of the new object id
/// # Returns
/// * `TeeResult` - the result of the operation
pub fn syscall_storage_obj_rename(
    obj: c_ulong,
    object_id: *mut c_void,
    object_id_len: usize,
) -> TeeResult {
    if object_id_len > TEE_OBJECT_ID_MAX_LEN as usize {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    if object_id_len != 0 && object_id.is_null() {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    let oid_bbuf = bb_memdup_object_id_from_user(object_id, object_id_len)?;

    tee_debug!(
        "syscall_storage_obj_rename: obj: {:X?}, object_id: {:?}, object_id_len: {:04X?}",
        obj,
        String::from_utf8_lossy(&oid_bbuf),
        object_id_len
    );
    let (o, fops) = {
        let o = tee_obj_get(obj)?;
        let o_guard = o.lock();
        if o_guard.info.handleFlags & TEE_HANDLE_FLAG_PERSISTENT == 0 {
            return Err(TEE_ERROR_BAD_STATE);
        }
        if o_guard.info.handleFlags & TEE_DATA_FLAG_ACCESS_WRITE_META == 0 {
            return Err(TEE_ERROR_ACCESS_CONFLICT);
        }

        let old_pobj = o_guard.pobj.as_ref().ok_or(TEE_ERROR_BAD_STATE)?;

        // reserve dest name
        let fops = old_pobj.fops.ok_or(TEE_ERROR_BAD_STATE)?;
        drop(o_guard);
        (o, fops)
    };

    let uuid = with_tee_ta_ctx(|ctx| Ok(ctx.uuid.clone()))?;
    let uuid = Uuid::parse_str(&uuid)?;

    let po = tee_pobj_get(
        uuid.as_raw_ref(),
        &oid_bbuf,
        object_id_len as u32,
        TEE_DATA_FLAG_ACCESS_WRITE_META,
        TeePobjUsage::Rename,
        fops,
    )
    .inspect_err(|e| {
        error!("syscall_storage_obj_rename: tee_pobj_get error: {:#X?}", e);
    })?;
    tee_debug!(
        "syscall_storage_obj_rename: dest reserved po refcnt: {}, obj_id_len: {}",
        po.refcnt.load(core::sync::atomic::Ordering::Relaxed),
        po.obj_id.lock().obj_id_len
    );

    // move (`?` must stay inside a closure: in a plain block it returns from this
    // syscall and would skip tee_pobj_release(po) below)
    let res = (|| -> TeeResult<()> {
        let o_guard = o.lock();
        let old_pobj = o_guard.pobj.as_ref().ok_or(TEE_ERROR_BAD_STATE)?;
        tee_debug!(
            "syscall_storage_obj_rename: source po refcnt: {}, obj_id_len: {}",
            old_pobj.refcnt.load(core::sync::atomic::Ordering::Relaxed),
            old_pobj.obj_id.lock().obj_id_len
        );

        let fs_res = (fops.rename)(
            old_pobj.as_ref(),
            po.as_ref(),
            false, // no overwrite
        );
        if let Err(e) = fs_res {
            error!("syscall_storage_obj_rename: fops.rename error: {:#X?}", e);
        } else {
            tee_debug!("syscall_storage_obj_rename: fops.rename -> TEE_SUCCESS");
        }
        fs_res?;

        let (new_obj_id_buf, new_obj_id_len) = {
            let objid = po.obj_id.lock();
            (
                objid.obj_id[..objid.obj_id_len as usize].to_vec(),
                objid.obj_id_len,
            )
        };
        let pobj_res = tee_pobj_rename(old_pobj.as_ref(), &new_obj_id_buf, new_obj_id_len);
        if let Err(e) = pobj_res {
            error!(
                "syscall_storage_obj_rename: tee_pobj_rename error: {:#X?} (fops.rename already \
                 committed)",
                e
            );
        } else {
            tee_debug!("syscall_storage_obj_rename: tee_pobj_rename -> TEE_SUCCESS");
        }
        pobj_res
    })();

    // Always release the new po, regardless of success or failure
    // This matches the C implementation which calls tee_pobj_release(po) in the exit label
    let _ = tee_pobj_release(po);

    if let Err(e) = res {
        error!("syscall_storage_obj_rename: syscall return {:#X?}", e);
    } else {
        tee_debug!("syscall_storage_obj_rename: syscall return TEE_SUCCESS");
    }
    res
}

/// read data from the object
///
/// # Arguments
/// * `obj` - the object id
/// * `data` - the data to read
/// * `len` - the length of the data
/// * `count` - the count of the data
/// # Returns
/// * `TeeResult` - the result of the operation
pub fn syscall_storage_obj_read(
    obj: c_ulong,
    data: *mut c_void,
    len: usize,
    count: *mut u64,
) -> TeeResult {
    tee_debug!(
        "syscall_storage_obj_read: obj: {:X?}, data_len: {:X?}, count: {:X?}",
        obj,
        len,
        count
    );
    let o = tee_obj_get(obj)?;

    let (fops, pos_tmp) = {
        let o_guard = o.lock();

        if o_guard.info.handleFlags & TEE_HANDLE_FLAG_PERSISTENT == 0 {
            return Err(TEE_ERROR_BAD_STATE);
        }

        if o_guard.info.handleFlags & TEE_DATA_FLAG_ACCESS_READ == 0 {
            return Err(TEE_ERROR_ACCESS_CONFLICT);
        }

        let _pos_tmp = o_guard
            .info
            .dataPosition
            .checked_add(len)
            .ok_or(TEE_ERROR_OVERFLOW)?;

        // data = memtag_strip_tag(data);
        tee_debug!("syscall_storage_obj_read: ds_pos: {:X?}", o_guard.ds_pos);

        let pos_tmp = o_guard
            .ds_pos
            .checked_add(o_guard.info.dataPosition)
            .ok_or(TEE_ERROR_OVERFLOW)?;

        (
            o_guard
                .pobj
                .as_ref()
                .ok_or(TEE_ERROR_BAD_STATE)?
                .fops
                .ok_or(TEE_ERROR_BAD_STATE)?,
            pos_tmp,
        )
    };

    let mut bytes = len;
    let mut o_guard = o.lock();
    let mut data_buf = vec![0u8; len].into_boxed_slice();
    tee_debug!(
        "syscall_storage_obj_read: bytes: {:X?} dataPosition: 0x{:X?}",
        bytes,
        o_guard.info.dataPosition
    );
    let fh = o_guard.fh.as_mut().ok_or(TEE_ERROR_BAD_STATE)?;
    (fops.read)(fh, pos_tmp, &mut [], &mut data_buf, &mut bytes).inspect_err(|e| {
        if *e == TEE_ERROR_CORRUPT_OBJECT {
            error!("Object corrupt");
            if let Err(rem_err) = remove_corrupt_obj(&mut o_guard) {
                debug!("remove_corrupt_obj failed: {:#X?}", rem_err);
            }
        }
    })?;
    o_guard.info.dataPosition += bytes;

    let u_count = bytes as u64;
    UserPtr::<u64>::from(count)
        .write_vm(u_count)
        .map_err(map_user_mem_error)?;
    write_user_bytes(data, &data_buf[..bytes])?;

    Ok(())
}

/// write data to the object
///
/// # Arguments
/// * `obj` - the object id
/// * `data` - the data to write
/// * `len` - the length of the data
/// # Returns
/// * `TeeResult` - the result of the operation
pub fn syscall_storage_obj_write(obj: c_ulong, data: *mut c_void, len: usize) -> TeeResult {
    tee_debug!(
        "syscall_storage_obj_write: obj: {:X?}, data_len: {:X?}",
        obj,
        len
    );
    let o = tee_obj_get(obj)?;
    let (fops, pos_tmp) = {
        let o_guard = o.lock();
        if o_guard.info.handleFlags & TEE_HANDLE_FLAG_PERSISTENT == 0 {
            return Err(TEE_ERROR_BAD_STATE);
        }
        if o_guard.info.handleFlags & TEE_DATA_FLAG_ACCESS_WRITE == 0 {
            return Err(TEE_ERROR_ACCESS_CONFLICT);
        }

        let _pos_tmp = o_guard
            .info
            .dataPosition
            .checked_add(len)
            .ok_or(TEE_ERROR_OVERFLOW)?;

        let pos_tmp = o_guard
            .ds_pos
            .checked_add(o_guard.info.dataPosition)
            .ok_or(TEE_ERROR_OVERFLOW)?;

        (
            o_guard
                .pobj
                .as_ref()
                .ok_or(TEE_ERROR_BAD_STATE)?
                .fops
                .ok_or(TEE_ERROR_BAD_STATE)?,
            pos_tmp,
        )
    };

    let mut o_guard = o.lock();

    tee_debug!(
        "syscall_storage_obj_write: dataPosition: {:X?}",
        o_guard.info.dataPosition
    );
    let data_buf = read_user_bytes_optional(data.cast_const(), len)?;
    let fh = o_guard.fh.as_mut().ok_or(TEE_ERROR_BAD_STATE)?;
    (fops.write)(fh, pos_tmp, &[], &data_buf, len).inspect_err(|e| {
        error!("syscall_storage_obj_write: write failed: {:X?}", e);
    })?;
    o_guard.info.dataPosition += len;
    if o_guard.info.dataPosition > o_guard.info.dataSize {
        o_guard.info.dataSize = o_guard.info.dataPosition;
    }
    Ok(())
}

pub fn tee_svc_storage_write_usage(o: &mut TeeObj, usage: u32) -> TeeResult {
    let pos = offset_of!(TeeSvcStorageHead, object_usage);

    let fops = {
        o.pobj
            .as_ref()
            .ok_or(TEE_ERROR_BAD_STATE)?
            .fops
            .ok_or(TEE_ERROR_BAD_STATE)?
    };

    let usage_bytes = usage.to_ne_bytes();

    let fh = o.fh.as_mut().ok_or(TEE_ERROR_BAD_STATE)?;
    (fops.write)(fh, pos, &usage_bytes, &[], usage_bytes.len()).inspect_err(|e| {
        error!("tee_svc_storage_write_usage: write failed: {:X?}", e);
    })
}

/// truncate the object to the length
///
/// # Arguments
/// * `obj` - the object id
/// * `len` - the length to truncate
/// # Returns
/// * `TeeResult` - the result of the operation
pub fn syscall_storage_obj_trunc(obj: c_ulong, len: usize) -> TeeResult {
    let o = tee_obj_get(obj)?;

    // check flags and get fops and attr size
    let (fops, attr_size) = {
        let mut o_guard = o.lock();
        if o_guard.info.handleFlags & TEE_HANDLE_FLAG_PERSISTENT == 0 {
            return Err(TEE_ERROR_BAD_STATE);
        }
        if o_guard.info.handleFlags & TEE_DATA_FLAG_ACCESS_WRITE == 0 {
            return Err(TEE_ERROR_ACCESS_CONFLICT);
        }

        let mut attr_size: usize = 0;
        tee_obj_attr_to_binary(&mut o_guard, &mut [], &mut attr_size)?;
        (
            o_guard
                .pobj
                .as_ref()
                .ok_or(TEE_ERROR_BAD_STATE)?
                .fops
                .ok_or(TEE_ERROR_BAD_STATE)?,
            attr_size,
        )
    };

    // calculate offset
    let mut offs = size_of::<TeeSvcStorageHead>()
        .checked_add(attr_size)
        .ok_or(TEE_ERROR_OVERFLOW)?;
    offs = offs.checked_add(len).ok_or(TEE_ERROR_OVERFLOW)?;

    // call truncate
    let res = {
        let mut o_guard = o.lock();
        let fh = o_guard.fh.as_mut().ok_or(TEE_ERROR_BAD_STATE)?;
        (fops.truncate)(fh, offs)
    };

    match res {
        Ok(()) => {
            let mut o_guard = o.lock();
            tee_debug!(
                "truncate success, dataSize from {:X?} to {:X?}",
                o_guard.info.dataSize,
                len
            );
            o_guard.info.dataSize = len;
            Ok(())
        }
        Err(e) if e == TEE_ERROR_CORRUPT_OBJECT => {
            error!("Object corruption");
            let _ = remove_corrupt_obj(&mut o.lock()); // Not using the return value
            Err(TEE_ERROR_CORRUPT_OBJECT)
        }
        Err(_) => Err(TEE_ERROR_GENERIC),
    }
}

/// seek to the offset in the object
///
/// # Arguments
/// * `obj` - the object id
/// * `offset` - the offset to seek
/// * `whence` - the whence to seek
/// # Returns
/// * `TeeResult` - the result of the operation
pub fn syscall_storage_obj_seek(obj: c_ulong, offset: i32, whence: c_ulong) -> TeeResult {
    let o = tee_obj_get(obj)?;
    let o_guard = o.lock();
    if o_guard.info.handleFlags & TEE_HANDLE_FLAG_PERSISTENT == 0 {
        return Err(TEE_ERROR_BAD_STATE);
    }
    let data_pos_snapshot = o_guard.info.dataPosition as i64;
    let data_size_snapshot = o_guard.info.dataSize as i64;
    let new_pos: i64 = match whence as u32 {
        TEE_DATA_SEEK_SET => offset as i64,
        TEE_DATA_SEEK_CUR => data_pos_snapshot
            .checked_add(offset as i64)
            .ok_or(TEE_ERROR_OVERFLOW)?,
        TEE_DATA_SEEK_END => data_size_snapshot
            .checked_add(offset as i64)
            .ok_or(TEE_ERROR_OVERFLOW)?,
        _ => return Err(TEE_ERROR_BAD_PARAMETERS),
    };
    drop(o_guard);

    let mut new_pos = new_pos;
    tee_debug!(
        "syscall_storage_obj_seek: obj={:#x} whence={:#x} offset={} snapshot(dataPos={}, \
         dataSize={}) -> new_pos={}",
        obj,
        whence,
        offset,
        data_pos_snapshot,
        data_size_snapshot,
        new_pos
    );
    if new_pos < 0 {
        new_pos = 0;
    }

    if new_pos > TEE_DATA_MAX_POSITION as i64 {
        return Err(TEE_ERROR_OVERFLOW);
    }

    let mut o_guard = o.lock();
    let prev = o_guard.info.dataPosition;
    o_guard.info.dataPosition = new_pos as usize;
    tee_debug!(
        "syscall_storage_obj_seek: obj={:#x} dataPosition {} -> {}",
        obj,
        prev,
        o_guard.info.dataPosition
    );

    Ok(())
}

/// allocate the enumeration of the object
///
/// # Arguments
/// * `obj_enum` - the object enumeration id
/// # Returns
/// * `TeeResult` - the result of the operation
pub fn syscall_storage_alloc_enum(obj_enum: *mut c_uint) -> TeeResult {
    let obj = TeeStorageEnum {
        id: 0,
        dir: None,
        fops: None,
    };
    let id = tee_svc_storage_add_enum(obj)? as u32;
    UserPtr::<c_uint>::from(obj_enum)
        .write_vm(id)
        .map_err(map_user_mem_error)?;
    Ok(())
}

/// free the enumeration of the object
///
/// # Arguments
/// * `obj_enum` - the object enumeration id
/// # Returns
/// * `TeeResult` - the result of the operation
pub fn syscall_storage_free_enum(obj_enum: c_ulong) -> TeeResult {
    tee_svc_close_enum(obj_enum)
}

/// reset the enumeration of the object
///
/// # Arguments
/// * `obj_enum` - the object enumeration id
/// # Returns
/// * `TeeResult` - the result of the operation
pub fn syscall_storage_reset_enum(obj_enum: c_ulong) -> TeeResult {
    let obj = tee_svc_storage_get_enum(obj_enum)?;
    // get the lock of the dir, and get the &mut TeeFsDir borrow
    let fops = {
        let obj_guard = obj.lock();
        obj_guard.fops
    };

    if let Some(fops) = fops {
        let mut obj_guard = obj.lock();
        if let Some(dir) = obj_guard.dir.as_mut() {
            (fops.closedir)(dir)?;
        }
        obj_guard.fops = None;
        obj_guard.dir = None;
    }

    let obj_guard = obj.lock();
    debug_assert!(obj_guard.dir.is_none());
    Ok(())
}

/// start the enumeration of the object
///
/// # Arguments
/// * `obj_enum` - the object enumeration id
/// * `storage_id` - the storage id
/// # Returns
/// * `TeeResult` - the result of the operation
pub fn syscall_storage_start_enum(obj_enum: c_ulong, storage_id: c_ulong) -> TeeResult {
    let fops =
        tee_svc_storage_file_ops(storage_id as u32).map_err(|_e| TEE_ERROR_ITEM_NOT_FOUND)?;

    let e = tee_svc_storage_get_enum(obj_enum)?;

    let e_fops = {
        let obj_guard = e.lock();
        obj_guard.fops
    };

    let mut obj_guard = e.lock();
    if obj_guard.dir.is_some() {
        let e_fops = e_fops.ok_or(TEE_ERROR_BAD_STATE)?;
        (e_fops.closedir)(obj_guard.dir.as_mut().ok_or(TEE_ERROR_BAD_STATE)?)?;
        obj_guard.dir = None;
    }

    let uuid = with_tee_ta_ctx(|ctx| Ok(ctx.uuid.clone()))?;
    let uuid = Uuid::parse_str(&uuid)?;

    let dir = (fops.opendir)(uuid.as_raw_ref())?;
    obj_guard.fops = Some(fops);
    obj_guard.dir = Some(dir);
    Ok(())
}

/// get the next object in the enumeration
///
/// # Arguments
/// * `obj_enum` - the object enumeration id
/// * `info` - the information of the object
/// * `obj_id` - the object id
/// * `len` - the length of the object id
/// # Returns
/// * `TeeResult` - the result of the operation
pub fn syscall_storage_next_enum(
    obj_enum: c_ulong,
    info: *mut utee_object_info,
    obj_id: *mut c_void,
    len: *mut u64,
) -> TeeResult {
    let mut o: Option<Box<TeeObj>> = None;
    // Hold the pobj from tee_pobj_get(USAGE_ENUM) so we can always release it,
    // even when the closure fails before storing it in o.pobj.
    let mut enum_pobj: Option<Arc<TeePobj>> = None;

    let res = (|| -> TeeResult {
        let e = tee_svc_storage_get_enum(obj_enum)?;

        let fops = {
            let obj_guard = e.lock();
            obj_guard.fops.ok_or(TEE_ERROR_ITEM_NOT_FOUND)?
        };

        let mut obj_guard = e.lock();
        let dir = obj_guard.dir.as_mut().ok_or(TEE_ERROR_BAD_STATE)?;
        let mut d = TeeFsDirent::default();
        (fops.readdir)(dir, &mut d)
            .inspect_err(|e| debug!("syscall_storage_next_enum: readdir: {:#010X?}", e))?;
        drop(obj_guard); // 释放 e 的锁，避免在 tee_pobj_get 中持有多个锁

        o = Some(Box::new(TeeObj::default()));
        let o = o.as_mut().ok_or(TEE_ERROR_BAD_STATE)?;

        let uuid = with_tee_ta_ctx(|ctx| Ok(ctx.uuid.clone()))?;
        let uuid = Uuid::parse_str(&uuid)?;

        let pobj = tee_pobj_get(
            uuid.as_raw_ref(),
            d.oid.as_ref(),
            d.oid_len,
            0,
            TeePobjUsage::Enumerate,
            fops,
        )?;

        // Store in outer scope so cleanup can release it even on early failure.
        enum_pobj = Some(pobj.clone());

        o.pobj = Some(pobj.clone());
        o.info.handleFlags = pobj.flags.load(core::sync::atomic::Ordering::Relaxed)
            | TEE_HANDLE_FLAG_PERSISTENT
            | TEE_HANDLE_FLAG_INITIALIZED;

        let pobj_flags = pobj.flags.load(core::sync::atomic::Ordering::Relaxed);

        let mut bbuf: utee_object_info = utee_object_info::default();
        with_pobj_usage_lock(pobj_flags, || -> TeeResult {
            tee_svc_storage_read_head(o)?;

            bbuf.obj_type = o.info.objectType;
            bbuf.obj_size = o.info.objectSize;
            bbuf.max_obj_size = o.info.maxObjectSize;
            bbuf.obj_usage = pobj
                .obj_info_usage
                .load(core::sync::atomic::Ordering::Relaxed);
            bbuf.data_size = o.info.dataSize as _;
            bbuf.data_pos = o.info.dataPosition as _;
            bbuf.handle_flags = o.info.handleFlags as _;
            Ok(())
        })?;

        UserPtr::<utee_object_info>::from(info)
            .write_vm(bbuf)
            .map_err(map_user_mem_error)?;

        let (obj_id_len, obj_id_vec, l) = {
            let objid = pobj.obj_id.lock();
            let obj_id_len = objid.obj_id_len as usize;
            let obj_id_vec = objid.obj_id[..obj_id_len].to_vec();
            let l = objid.obj_id_len as u64;
            (obj_id_len, obj_id_vec, l)
        };

        write_user_bytes(obj_id, &obj_id_vec[..obj_id_len])?;

        UserPtr::<u64>::from(len)
            .write_vm(l)
            .map_err(map_user_mem_error)?;

        Ok(())
    })();

    if let Some(mut o) = o
        && let Some(pobj) = o.pobj.take()
    {
        let fops = pobj.fops.ok_or(TEE_ERROR_BAD_STATE)?;

        (fops.close)(&mut o.fh);
        let _ = tee_pobj_release(pobj);
        // Success path: o.pobj was released, so clear enum_pobj to avoid
        // double-releasing the same refcnt below.
        enum_pobj = None;
    }

    // If the closure failed before o.pobj was set, release directly.
    if let Some(pobj) = enum_pobj {
        let _ = tee_pobj_release(pobj);
    }

    res
}

#[unittest::mod_test]
pub mod tests_tee_svc_storage {
    use core::ffi::c_ulong;

    use unittest::{assert, assert_eq, assert_ne};

    use super::*;
    use crate::{
        TestUserBuffer, TestUserValue,
        tee::{
            tee_misc::{tee_b2hs, tee_b2hs_hsbuf_size},
            tee_svc_cryp::{syscall_cryp_obj_close, syscall_cryp_obj_get_info},
        },
    };

    const TEE_DIRNAME_BUFFER_REQUIRED_LEN: usize = tee_b2hs_hsbuf_size(TEE_UUID_HEX_LEN) + 1;

    #[unittest::def_test]
    fn test_size_of_val() {
        assert_eq!(
            size_of_val(&TeeSvcStorageHead::default()),
            size_of::<TeeSvcStorageHead>()
        );
    }

    // Helper to create a TeeUuid from its raw byte representation for predictable testing
    // This assumes little-endian for u16/u32 fields, adjust if your target is big-endian.
    fn create_uuid_from_bytes(bytes: [u8; 16]) -> TEE_UUID {
        TEE_UUID {
            timeLow: u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            timeMid: u16::from_le_bytes([bytes[4], bytes[5]]),
            timeHiAndVersion: u16::from_le_bytes([bytes[6], bytes[7]]),
            clockSeqAndNode: [
                bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14],
                bytes[15],
            ],
        }
    }

    fn user_buffer_from_bytes(bytes: &[u8]) -> TestUserBuffer {
        let buffer = TestUserBuffer::new(bytes.len()).unwrap();
        buffer.write_bytes(bytes).unwrap();
        buffer
    }

    // --- Tests for tee_svc_storage_create_dirname ---

    #[unittest::def_test]
    fn test_create_dirname_standard_uuid() {
        let uuid_bytes: [u8; 16] = [
            0x78, 0x56, 0x34, 0x12, 0xBC, 0x9A, 0xF0, 0xDE, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66,
            0x77, 0x88,
        ];
        let uuid = create_uuid_from_bytes(uuid_bytes);
        let mut buf = [0u8; TEE_DIRNAME_BUFFER_REQUIRED_LEN];
        let result = tee_svc_storage_create_dirname(&mut buf, &uuid);

        assert!(result.is_ok());
        assert_eq!(
            str::from_utf8(&buf[..TEE_DIRNAME_BUFFER_REQUIRED_LEN - 1]).unwrap(),
            "/78563412BC9AF0DE1122334455667788"
        );
        assert_eq!(buf[TEE_DIRNAME_BUFFER_REQUIRED_LEN - 1], 0);
    }

    #[unittest::def_test]
    fn test_create_dirname_all_zeros_uuid() {
        let uuid = TEE_UUID {
            timeLow: 0,
            timeMid: 0,
            timeHiAndVersion: 0,
            clockSeqAndNode: [0; 8],
        };
        let mut buf = [0u8; TEE_DIRNAME_BUFFER_REQUIRED_LEN];
        let result = tee_svc_storage_create_dirname(&mut buf, &uuid);

        assert!(result.is_ok());
        assert_eq!(
            str::from_utf8(&buf[..TEE_DIRNAME_BUFFER_REQUIRED_LEN - 1]).unwrap(),
            "/00000000000000000000000000000000"
        );
        assert_eq!(buf[TEE_DIRNAME_BUFFER_REQUIRED_LEN - 1], 0);
    }

    #[unittest::def_test]
    fn test_create_dirname_specific_uuid_values() {
        let uuid_bytes: [u8; 16] = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E,
            0x0F, 0x10,
        ];
        let uuid = create_uuid_from_bytes(uuid_bytes);
        let mut buf = [0u8; TEE_DIRNAME_BUFFER_REQUIRED_LEN];
        let result = tee_svc_storage_create_dirname(&mut buf, &uuid);

        assert!(result.is_ok());
        assert_eq!(
            str::from_utf8(&buf[..TEE_DIRNAME_BUFFER_REQUIRED_LEN - 1]).unwrap(),
            "/0102030405060708090A0B0C0D0E0F10"
        );
        assert_eq!(buf[TEE_DIRNAME_BUFFER_REQUIRED_LEN - 1], 0);
    }

    #[unittest::def_test]
    fn test_create_dirname_short_buffer() {
        let uuid = TEE_UUID {
            timeLow: 0,
            timeMid: 0,
            timeHiAndVersion: 0,
            clockSeqAndNode: [0; 8],
        };
        let mut buf = [0u8; TEE_DIRNAME_BUFFER_REQUIRED_LEN - 1];
        let result = tee_svc_storage_create_dirname(&mut buf, &uuid);

        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), TEE_ERROR_SHORT_BUFFER);
    }

    #[unittest::def_test]
    fn test_create_dirname_empty_buffer() {
        let uuid = TEE_UUID {
            timeLow: 0,
            timeMid: 0,
            timeHiAndVersion: 0,
            clockSeqAndNode: [0; 8],
        };
        let mut buf = [0u8; 0];
        let result = tee_svc_storage_create_dirname(&mut buf, &uuid);

        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), TEE_ERROR_SHORT_BUFFER);
    }

    #[unittest::def_test]
    fn test_create_dirname_exact_buffer() {
        let uuid = TEE_UUID {
            timeLow: 0xAABBCCDD,
            timeMid: 0xEEFF,
            timeHiAndVersion: 0x1122,
            clockSeqAndNode: [0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA],
        };
        let mut buf = [0u8; TEE_DIRNAME_BUFFER_REQUIRED_LEN];
        let result = tee_svc_storage_create_dirname(&mut buf, &uuid);

        assert!(result.is_ok());
        assert_eq!(
            str::from_utf8(&buf[..TEE_DIRNAME_BUFFER_REQUIRED_LEN - 1]).unwrap(),
            "/DDCCBBAAFFEE221133445566778899AA"
        );
        assert_eq!(buf[TEE_DIRNAME_BUFFER_REQUIRED_LEN - 1], 0);
    }

    // --- Additional tests for tee_b2hs if needed ---

    #[unittest::def_test]
    fn test_tee_b2hs_uppercase_conversion() {
        let b = &[0xab, 0xcd, 0xef];
        let mut hs = [0u8; tee_b2hs_hsbuf_size(3)];
        let result = tee_b2hs(b, &mut hs);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 6);
        assert_eq!(str::from_utf8(&hs[..6]).unwrap(), "ABCDEF");
        assert_eq!(hs[6], 0);
    }

    #[unittest::def_test]
    fn test_tee_b2hs_null_termination() {
        let b = &[0x12];
        let mut hs = [0u8; tee_b2hs_hsbuf_size(1)];
        let result = tee_b2hs(b, &mut hs);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 2);
        assert_eq!(str::from_utf8(&hs[..2]).unwrap(), "12");
        assert_eq!(hs[2], 0);
    }

    #[unittest::def_test]
    fn test_tee_b2hs_short_output_buffer() {
        let b = &[0x12, 0x34];
        let mut hs = [0u8; tee_b2hs_hsbuf_size(2) - 1];
        let result = tee_b2hs(b, &mut hs);
        assert!(result.is_err());
    }

    #[unittest::def_test]
    fn test_tee_b2hs_empty_input() {
        let mut hs = [0u8; tee_b2hs_hsbuf_size(0)];
        let result = tee_b2hs(&[], &mut hs);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 0);
        assert_eq!(hs, [0]);
    }

    #[unittest::def_test(custom)]
    fn test_syscall_storage_obj_create_type_data() {
        let storage_id = TEE_STORAGE_PRIVATE as c_ulong;
        let object_id = "test_object_create";
        let flags = TEE_DATA_FLAG_ACCESS_READ
            | TEE_DATA_FLAG_ACCESS_WRITE
            | TEE_DATA_FLAG_ACCESS_WRITE_META
            | TEE_DATA_FLAG_OVERWRITE;
        let attr = TEE_HANDLE_NULL;
        let data_create = b"test_data";

        let object_id_user = user_buffer_from_bytes(object_id.as_bytes());
        let data_create_user = user_buffer_from_bytes(data_create);
        let mut obj = TestUserValue::<c_uint>::from_value(0).unwrap();

        let result = syscall_storage_obj_create(
            storage_id,
            object_id_user.as_user_ptr(),
            object_id.len(),
            flags as c_ulong,
            attr as c_ulong,
            data_create_user.as_user_ptr(),
            data_create.len(),
            obj.as_user_ref(),
        );
        assert!(result.is_ok());

        let obj_id = obj.read() as c_ulong;

        let data_read = TestUserBuffer::new(data_create.len()).unwrap();
        let mut count = TestUserValue::<u64>::from_value(0).unwrap();
        let result = syscall_storage_obj_read(
            obj_id,
            data_read.as_user_ptr(),
            data_create.len(),
            count.as_user_ref(),
        );
        assert!(result.is_ok());
        let data_read_back = data_read.read_bytes(data_create.len()).unwrap();
        assert_eq!(data_read_back.as_slice(), data_create);
        assert_eq!(count.read(), data_create.len() as u64);

        let data_write = b"TEST_DATA";
        let data_write_user = user_buffer_from_bytes(data_write);
        let result =
            syscall_storage_obj_write(obj_id, data_write_user.as_user_ptr(), data_write.len());
        assert!(result.is_ok());

        let result = syscall_storage_obj_seek(
            obj_id,
            -(data_write.len() as i32),
            TEE_DATA_SEEK_CUR as c_ulong,
        );
        assert!(result.is_ok());

        let data_read = TestUserBuffer::new(data_write.len()).unwrap();
        let mut count = TestUserValue::<u64>::from_value(0).unwrap();
        let result = syscall_storage_obj_read(
            obj_id,
            data_read.as_user_ptr(),
            data_write.len(),
            count.as_user_ref(),
        );
        assert!(result.is_ok());
        let data_read_back = data_read.read_bytes(data_write.len()).unwrap();
        assert_eq!(data_read_back.as_slice(), data_write);
        assert_eq!(count.read(), data_write.len() as u64);

        let result = syscall_storage_obj_trunc(obj_id, data_create.len());
        assert!(result.is_ok());
        let result = syscall_storage_obj_seek(obj_id, 0, TEE_DATA_SEEK_SET as c_ulong);
        assert!(result.is_ok());

        let data_read = TestUserBuffer::new(data_create.len()).unwrap();
        let mut count = TestUserValue::<u64>::from_value(0).unwrap();
        let result = syscall_storage_obj_read(
            obj_id,
            data_read.as_user_ptr(),
            data_create.len(),
            count.as_user_ref(),
        );
        assert!(result.is_ok());
        let data_read_back = data_read.read_bytes(data_create.len()).unwrap();
        assert_eq!(data_read_back.as_slice(), data_create);
        assert_eq!(count.read(), data_create.len() as u64);

        let data_read = TestUserBuffer::new(1).unwrap();
        let mut count = TestUserValue::<u64>::from_value(0).unwrap();
        let _result =
            syscall_storage_obj_read(obj_id, data_read.as_user_ptr(), 1, count.as_user_ref());
        assert_eq!(count.read(), 0);

        let result = syscall_storage_obj_seek(
            obj_id,
            (data_create.len() + 1) as i32,
            TEE_DATA_SEEK_SET as c_ulong,
        );
        assert!(result.is_ok());

        let mut info =
            TestUserValue::<utee_object_info>::from_value(utee_object_info::default()).unwrap();
        let result = syscall_cryp_obj_get_info(obj_id, info.as_user_ref());
        assert!(result.is_ok());
        let info = info.read();
        assert_eq!(info.data_size, data_create.len() as u32);
        assert!(
            info.handle_flags
                & (TEE_HANDLE_FLAG_PERSISTENT
                    | TEE_HANDLE_FLAG_INITIALIZED
                    | TEE_DATA_FLAG_ACCESS_READ
                    | TEE_DATA_FLAG_ACCESS_WRITE
                    | TEE_DATA_FLAG_ACCESS_WRITE_META)
                != 0
        );
        assert_eq!(info.obj_type, TEE_TYPE_DATA);

        let object_id_new = "test_object_new";
        let object_id_new_user = user_buffer_from_bytes(object_id_new.as_bytes());
        let result = syscall_storage_obj_rename(
            obj_id,
            object_id_new_user.as_user_ptr(),
            object_id_new.len(),
        );
        assert!(result.is_ok());

        let result = syscall_storage_obj_del(obj_id);
        assert!(result.is_ok());
        let result = tee_obj_get(obj_id as TeeObjIdType);
        assert!(matches!(result, Err(TEE_ERROR_ITEM_NOT_FOUND)));
    }

    #[unittest::def_test]
    fn test_syscall_storage_init() {}

    #[unittest::def_test(custom)]
    fn test_syscall_storage_obj_create_rejects_invalid_parameters() {
        let object_id = user_buffer_from_bytes(b"invalid_create");
        let mut obj = TestUserValue::<c_uint>::from_value(0).unwrap();

        let result = syscall_storage_obj_create(
            TEE_STORAGE_PRIVATE as c_ulong,
            object_id.as_user_ptr(),
            "invalid_create".len(),
            1u64 << 63,
            TEE_HANDLE_NULL as c_ulong,
            core::ptr::null_mut(),
            0,
            obj.as_user_ref(),
        );
        assert_eq!(result.err(), Some(TEE_ERROR_BAD_PARAMETERS));

        let result = syscall_storage_obj_create(
            TEE_STORAGE_PRIVATE as c_ulong,
            object_id.as_user_ptr(),
            "invalid_create".len(),
            TEE_DATA_FLAG_ACCESS_READ as c_ulong,
            TEE_HANDLE_NULL as c_ulong,
            core::ptr::null_mut(),
            4,
            obj.as_user_ref(),
        );
        assert_eq!(result.err(), Some(TEE_ERROR_BAD_PARAMETERS));
    }

    #[unittest::def_test(custom)]
    fn test_syscall_storage_obj_open() {
        let storage_id = TEE_STORAGE_PRIVATE as c_ulong;
        let object_id = "test_object";
        let create_flags = TEE_DATA_FLAG_ACCESS_READ
            | TEE_DATA_FLAG_ACCESS_WRITE
            | TEE_DATA_FLAG_ACCESS_WRITE_META
            | TEE_DATA_FLAG_OVERWRITE;
        let open_flags = TEE_DATA_FLAG_ACCESS_READ
            | TEE_DATA_FLAG_ACCESS_WRITE
            | TEE_DATA_FLAG_ACCESS_WRITE_META;
        let data_create = b"test_data";

        let object_id_create_user = user_buffer_from_bytes(object_id.as_bytes());
        let data_create_user = user_buffer_from_bytes(data_create);
        let mut created_obj = TestUserValue::<c_uint>::from_value(0).unwrap();
        let result = syscall_storage_obj_create(
            storage_id,
            object_id_create_user.as_user_ptr(),
            object_id.len(),
            create_flags as c_ulong,
            TEE_HANDLE_NULL as c_ulong,
            data_create_user.as_user_ptr(),
            data_create.len(),
            created_obj.as_user_ref(),
        );
        assert!(result.is_ok());

        let created_obj_id = created_obj.read() as c_ulong;
        let result = syscall_cryp_obj_close(created_obj_id);
        assert!(result.is_ok());

        let object_id_user = user_buffer_from_bytes(object_id.as_bytes());
        let mut obj = TestUserValue::<c_uint>::from_value(0).unwrap();
        let result = syscall_storage_obj_open(
            storage_id,
            object_id_user.as_user_ptr(),
            object_id.len(),
            open_flags as c_ulong,
            obj.as_user_ref(),
        );
        assert!(result.is_ok());

        let opened_obj_id = obj.read() as c_ulong;
        let result = syscall_storage_obj_del(opened_obj_id);
        assert!(result.is_ok());
    }

    #[unittest::def_test(custom)]
    fn test_syscall_storage_obj_open_rejects_invalid_parameters() {
        let object_id = user_buffer_from_bytes(b"missing_object");
        let mut obj = TestUserValue::<c_uint>::from_value(0).unwrap();

        let result = syscall_storage_obj_open(
            TEE_STORAGE_PRIVATE as c_ulong,
            object_id.as_user_ptr(),
            "missing_object".len(),
            1u64 << 62,
            obj.as_user_ref(),
        );
        assert_eq!(result.err(), Some(TEE_ERROR_BAD_PARAMETERS));

        let result = syscall_storage_obj_open(
            TEE_STORAGE_PRIVATE as c_ulong,
            object_id.as_user_ptr(),
            "missing_object".len(),
            TEE_DATA_FLAG_ACCESS_READ as c_ulong,
            obj.as_user_ref(),
        );
        assert!(result.is_err());
        assert_ne!(result.unwrap_err(), TEE_ERROR_BAD_PARAMETERS);
    }

    #[unittest::def_test(custom)]
    fn test_syscall_storage_obj_seek_rejects_invalid_whence() {
        let storage_id = TEE_STORAGE_PRIVATE as c_ulong;
        let object_id = "seek_invalid_whence";
        let data_create = b"seek_data";
        let object_id_user = user_buffer_from_bytes(object_id.as_bytes());
        let data_create_user = user_buffer_from_bytes(data_create);
        let mut obj = TestUserValue::<c_uint>::from_value(0).unwrap();

        let result = syscall_storage_obj_create(
            storage_id,
            object_id_user.as_user_ptr(),
            object_id.len(),
            (TEE_DATA_FLAG_ACCESS_READ
                | TEE_DATA_FLAG_ACCESS_WRITE
                | TEE_DATA_FLAG_ACCESS_WRITE_META
                | TEE_DATA_FLAG_OVERWRITE) as c_ulong,
            TEE_HANDLE_NULL as c_ulong,
            data_create_user.as_user_ptr(),
            data_create.len(),
            obj.as_user_ref(),
        );
        assert!(result.is_ok());

        let obj_id = obj.read() as c_ulong;
        let result = syscall_storage_obj_seek(obj_id, 0, 0xFFFF_FFFF);
        assert_eq!(result.err(), Some(TEE_ERROR_BAD_PARAMETERS));

        let result = syscall_storage_obj_del(obj_id);
        assert!(result.is_ok());
    }
}
