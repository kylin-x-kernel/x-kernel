// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, ffi::CString};
use core::ffi::{CStr, c_uint, c_ulong, c_void};

use osvm::MemError;
use posix_types::{UserConstPtr, UserPtr};
use tee_raw_sys::{
    TEE_ERROR_BAD_PARAMETERS, TEE_ERROR_ITEM_NOT_FOUND, TEE_ERROR_SHORT_BUFFER, TEE_Identity,
    TEE_PROPSET_CURRENT_CLIENT, TEE_PROPSET_CURRENT_TA, TEE_PROPSET_TEE_IMPLEMENTATION,
    TEE_PropSetHandle, TEE_UUID,
};

use crate::tee::{TeeResult, tee_session::with_tee_session_ctx};

fn map_user_mem_error(err: MemError) -> u32 {
    match err {
        MemError::InvalidAddr | MemError::NoAccess => TEE_ERROR_BAD_PARAMETERS,
        _ => tee_raw_sys::TEE_ERROR_GENERIC,
    }
}

/// Trait representing a TA property.
trait TeeProps {
    fn name(&self) -> CString;
    fn prop_type(&self) -> PropType;
    fn get(&self, buf: *mut c_void, blen: &mut u32) -> TeeResult;
}

/// Represents a TEE property set according to the TEE Internal API.
/// The property set is a collection of properties that can be
/// queried from the TEE. The property set is identified by a
/// handle, which is a pointer to a TEE_PropSetHandle structure.
enum PropertySet {
    CurrentClient,
    CurrentTa,
    TeeImplementation,
}

impl PropertySet {
    fn from_raw(raw: c_ulong) -> TeeResult<Self> {
        let handle = raw as usize as TEE_PropSetHandle;
        match handle {
            TEE_PROPSET_CURRENT_CLIENT => Ok(PropertySet::CurrentClient),
            TEE_PROPSET_CURRENT_TA => Ok(PropertySet::CurrentTa),
            TEE_PROPSET_TEE_IMPLEMENTATION => Ok(PropertySet::TeeImplementation),
            // OP-TEE `get_prop_set` reports a missing property set, not a bad argument.
            _ => Err(TEE_ERROR_ITEM_NOT_FOUND),
        }
    }
}

#[allow(dead_code)]
#[derive(Clone, Copy)]
enum PropType {
    Bool,        // bool
    U32,         // uint32_t
    Uuid,        // TEE_UUID
    Identity,    // TEE_Identity
    String,      // zero terminated string of char
    Binaryblock, // zero terminated base64 coded string
    U64,         // uint64_t
    Invalid,     // invalid value
}

impl PropType {
    fn as_raw(&self) -> c_uint {
        match self {
            PropType::Bool => 0,
            PropType::U32 => 1,
            PropType::Uuid => 2,
            PropType::Identity => 3,
            PropType::String => 4,
            PropType::Binaryblock => 5,
            PropType::U64 => 6,
            PropType::Invalid => 7,
        }
    }
}

// Properties of the set TEE_PROPSET_CURRENT_CLIENT
struct ClientIdentity;
struct ClientEndian;

impl TeeProps for ClientIdentity {
    fn name(&self) -> CString {
        c"gpd.client.identity".into()
    }

    fn prop_type(&self) -> PropType {
        PropType::Identity
    }

    fn get(&self, buf: *mut c_void, blen: &mut u32) -> TeeResult {
        let prop_size = size_of::<TEE_Identity>() as u32;
        if *blen < prop_size {
            *blen = prop_size;
            return Err(TEE_ERROR_SHORT_BUFFER);
        }
        *blen = prop_size;
        let clnt_id = with_tee_session_ctx(|ctx| Ok(ctx.clnt_id))?;
        UserPtr::<TEE_Identity>::from(buf.cast::<TEE_Identity>())
            .write_vm(clnt_id)
            .map_err(map_user_mem_error)
    }
}

impl TeeProps for ClientEndian {
    fn name(&self) -> CString {
        c"gpd.client.endian".into()
    }

    fn prop_type(&self) -> PropType {
        PropType::U32
    }

    fn get(&self, buf: *mut c_void, blen: &mut u32) -> TeeResult {
        let endian: u32 = 0;
        let prop_size = size_of::<u32>() as u32;
        if *blen < prop_size {
            *blen = prop_size;
            return Err(TEE_ERROR_SHORT_BUFFER);
        }
        *blen = prop_size;
        UserPtr::<u32>::from(buf.cast::<u32>())
            .write_vm(endian)
            .map_err(map_user_mem_error)
    }
}

// Properties of the set TEE_PROPSET_CURRENT_TA
struct TaAppId;

impl TeeProps for TaAppId {
    fn name(&self) -> CString {
        c"gpd.ta.appID".into()
    }

    fn prop_type(&self) -> PropType {
        PropType::Uuid
    }

    fn get(&self, buf: *mut c_void, blen: &mut u32) -> TeeResult {
        let prop_size = size_of::<TEE_UUID>() as u32;
        if *blen < prop_size {
            *blen = prop_size;
            return Err(TEE_ERROR_SHORT_BUFFER);
        }
        *blen = prop_size;
        let uuid = with_tee_session_ctx(|ctx| Ok(ctx.clnt_id))?.uuid;
        UserPtr::<TEE_UUID>::from(buf.cast::<TEE_UUID>())
            .write_vm(uuid)
            .map_err(map_user_mem_error)
    }
}

// Properties of the set TEE_PROPSET_TEE_IMPLEMENTATION.
// Order matches OP-TEE `tee_propset_tee` in core/tee/tee_svc.c.
// `gpd.tee.arith.maxBigIntSize` stays in libutee, as it does upstream.
const U32_ZERO: [u8; 4] = 0u32.to_le_bytes();
const U32_PROT_LEVEL_REE: [u8; 4] = 100u32.to_le_bytes();
const BOOL_TRUE: [u8; 1] = [1];
const UUID_ZERO: [u8; 16] = [0; 16];

struct ConstProp {
    name: &'static CStr,
    prop_type: PropType,
    bytes: &'static [u8],
}

impl TeeProps for ConstProp {
    fn name(&self) -> CString {
        self.name.into()
    }

    fn prop_type(&self) -> PropType {
        self.prop_type
    }

    fn get(&self, buf: *mut c_void, blen: &mut u32) -> TeeResult {
        let prop_size = self.bytes.len() as u32;
        if *blen < prop_size {
            *blen = prop_size;
            return Err(TEE_ERROR_SHORT_BUFFER);
        }
        *blen = prop_size;
        UserPtr::<u8>::from(buf.cast::<u8>())
            .write_vm_slice(self.bytes)
            .map_err(map_user_mem_error)
    }
}

fn tee_impl_prop(index: c_ulong) -> TeeResult<ConstProp> {
    let prop = match index {
        // CFG_TEE_API_VERSION
        0 => ConstProp {
            name: c"gpd.tee.apiversion",
            prop_type: PropType::String,
            bytes: b"GPD-1.1-dev\0",
        },
        // Implementation description. OP-TEE's default is "OPTEE".
        1 => ConstProp {
            name: c"gpd.tee.description",
            prop_type: PropType::String,
            bytes: b"x-kernel\0",
        },
        // OP-TEE hashes the OTP die id into a UUID. There is no die id here.
        2 => ConstProp {
            name: c"gpd.tee.deviceID",
            prop_type: PropType::Uuid,
            bytes: &UUID_ZERO,
        },
        // REE-controlled clock, same level as OP-TEE tee_time_ree.c.
        3 => ConstProp {
            name: c"gpd.tee.systemTime.protectionLevel",
            prop_type: PropType::U32,
            bytes: &U32_PROT_LEVEL_REE,
        },
        4 => ConstProp {
            name: c"gpd.tee.TAPersistentTime.protectionLevel",
            prop_type: PropType::U32,
            bytes: &U32_PROT_LEVEL_REE,
        },
        5 => ConstProp {
            name: c"gpd.tee.cryptography.ecc",
            prop_type: PropType::Bool,
            bytes: &BOOL_TRUE,
        },
        // 100: anti-rollback enforced at REE level (OP-TEE without CFG_RPMB_FS).
        6 => ConstProp {
            name: c"gpd.tee.trustedStorage.antiRollback.protectionLevel",
            prop_type: PropType::U32,
            bytes: &U32_PROT_LEVEL_REE,
        },
        7 => ConstProp {
            name: c"gpd.tee.trustedos.implementation.version",
            prop_type: PropType::String,
            bytes: b"x-kernel\0",
        },
        8 => ConstProp {
            name: c"gpd.tee.trustedos.implementation.binaryversion",
            prop_type: PropType::U32,
            bytes: &U32_ZERO,
        },
        9 => ConstProp {
            name: c"gpd.tee.trustedos.manufacturer",
            prop_type: PropType::String,
            bytes: b"KylinSoft\0",
        },
        // OP-TEE defaults when firmware identity is not configured.
        10 => ConstProp {
            name: c"gpd.tee.firmware.implementation.version",
            prop_type: PropType::String,
            bytes: b"FW_IMPL_UNDEF\0",
        },
        11 => ConstProp {
            name: c"gpd.tee.firmware.implementation.binaryversion",
            prop_type: PropType::U32,
            bytes: &U32_ZERO,
        },
        12 => ConstProp {
            name: c"gpd.tee.firmware.manufacturer",
            prop_type: PropType::String,
            bytes: b"FW_MAN_UNDEF\0",
        },
        _ => return Err(TEE_ERROR_ITEM_NOT_FOUND),
    };
    Ok(prop)
}

fn get_prop_struct(prop_set: PropertySet, index: c_ulong) -> TeeResult<Box<dyn TeeProps>> {
    match prop_set {
        PropertySet::CurrentClient => match index {
            0 => Ok(Box::new(ClientIdentity)),
            1 => Ok(Box::new(ClientEndian)),
            _ => Err(TEE_ERROR_ITEM_NOT_FOUND),
        },
        PropertySet::CurrentTa => match index {
            0 => Ok(Box::new(TaAppId)),
            _ => Err(TEE_ERROR_ITEM_NOT_FOUND),
        },
        PropertySet::TeeImplementation => Ok(Box::new(tee_impl_prop(index)?)),
    }
}

fn get_prop_index(prop_set: PropertySet, name: &str) -> TeeResult<u32> {
    match (prop_set, name) {
        (PropertySet::CurrentClient, "gpd.client.identity") => Ok(0),
        (PropertySet::CurrentClient, "gpd.client.endian") => Ok(1),
        (PropertySet::CurrentTa, "gpd.ta.appID") => Ok(0),
        (PropertySet::TeeImplementation, "gpd.tee.apiversion") => Ok(0),
        (PropertySet::TeeImplementation, "gpd.tee.description") => Ok(1),
        (PropertySet::TeeImplementation, "gpd.tee.deviceID") => Ok(2),
        (PropertySet::TeeImplementation, "gpd.tee.systemTime.protectionLevel") => Ok(3),
        (PropertySet::TeeImplementation, "gpd.tee.TAPersistentTime.protectionLevel") => Ok(4),
        (PropertySet::TeeImplementation, "gpd.tee.cryptography.ecc") => Ok(5),
        (PropertySet::TeeImplementation, "gpd.tee.trustedStorage.antiRollback.protectionLevel") => {
            Ok(6)
        }
        (PropertySet::TeeImplementation, "gpd.tee.trustedos.implementation.version") => Ok(7),
        (PropertySet::TeeImplementation, "gpd.tee.trustedos.implementation.binaryversion") => Ok(8),
        (PropertySet::TeeImplementation, "gpd.tee.trustedos.manufacturer") => Ok(9),
        (PropertySet::TeeImplementation, "gpd.tee.firmware.implementation.version") => Ok(10),
        (PropertySet::TeeImplementation, "gpd.tee.firmware.implementation.binaryversion") => Ok(11),
        (PropertySet::TeeImplementation, "gpd.tee.firmware.manufacturer") => Ok(12),
        _ => Err(TEE_ERROR_ITEM_NOT_FOUND),
    }
}

/// Get a TEE property from a property set by index.
///
/// An unknown property set or index yields [`TEE_ERROR_ITEM_NOT_FOUND`].
/// When both `buf` and `blen` are non-null and the value does not fit,
/// `*blen` receives the required size and the result is [`TEE_ERROR_SHORT_BUFFER`].
/// A failure while writing that length is returned ahead of the short-buffer error.
pub(super) fn sys_tee_scn_get_property(
    prop_set: c_ulong,
    index: c_ulong,
    name: *mut c_void,
    name_len: *mut c_uint,
    buf: *mut c_void,
    blen: *mut c_uint,
    prop_type: *mut c_uint,
) -> TeeResult {
    let prop = get_prop_struct(PropertySet::from_raw(prop_set)?, index)?;

    // Get the property type
    if !prop_type.is_null() {
        let raw_type = prop.prop_type().as_raw();
        UserPtr::<c_uint>::from(prop_type)
            .write_vm(raw_type)
            .map_err(map_user_mem_error)?;
    }

    // Get the property
    if !buf.is_null() && !blen.is_null() {
        let user_blen = UserPtr::<c_uint>::from(blen);
        let mut klen = user_blen.read_vm().map_err(map_user_mem_error)?;

        let res = prop.get(buf, &mut klen);
        // Callers size a retry from this length, including when the value did not fit.
        // A failed length write hides that size, so it is reported first.
        user_blen.write_vm(klen).map_err(map_user_mem_error)?;
        res?;
    }

    // Get the property name
    if !name.is_null() && !name_len.is_null() {
        let prop_name = prop.name();
        let prop_name_bytes = prop_name.to_bytes_with_nul();
        let prop_name_len = prop_name_bytes.len() as u32;

        let user_name_len = UserPtr::<c_uint>::from(name_len);
        let mut klen = user_name_len.read_vm().map_err(map_user_mem_error)?;

        if klen < prop_name_len {
            klen = prop_name_len;
            user_name_len.write_vm(klen).map_err(map_user_mem_error)?;
            return Err(TEE_ERROR_SHORT_BUFFER);
        }

        UserPtr::<u8>::from(name.cast::<u8>())
            .write_vm_slice(prop_name_bytes)
            .map_err(map_user_mem_error)?;

        klen = prop_name_len;
        user_name_len.write_vm(klen).map_err(map_user_mem_error)?;
    }

    Ok(())
}

/// Convert a TEE property name to its index.
///
/// An unknown property set or name yields [`TEE_ERROR_ITEM_NOT_FOUND`].
pub(super) fn sys_tee_scn_get_property_name_to_index(
    prop_set: c_ulong,
    name: *mut c_void,
    name_len: c_ulong,
    index: *mut c_uint,
) -> TeeResult {
    if name_len == 0 {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    let kname_buf = UserConstPtr::<u8>::from(name.cast_const().cast::<u8>())
        .load_vm_vec(name_len as usize)
        .map_err(map_user_mem_error)?;
    let kname = match core::str::from_utf8(&kname_buf[..(name_len as usize - 1)]) {
        Ok(kname) => kname,
        Err(_) => return Err(TEE_ERROR_BAD_PARAMETERS),
    };

    let prop_index = get_prop_index(PropertySet::from_raw(prop_set)?, kname)?;
    UserPtr::<c_uint>::from(index)
        .write_vm(prop_index)
        .map_err(map_user_mem_error)?;

    Ok(())
}

unittest::path_mod_test!(
    pub(super) mod tests_tee_property,
    "../../ut-tests/tee/tee_property/tests_tee_property.rs"
);
