// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, vec, vec::Vec};
use core::{
    ffi::{c_uint, c_ulong, c_void},
    fmt,
    fmt::Debug,
    mem::size_of,
    ops::{Deref, DerefMut},
};

use osvm::MemError;
use posix_types::{UserConstPtr, UserPtr};
use tee_raw_sys::{libc_compat::size_t, *};

use super::{
    TeeResult,
    config::{CFG_COMPAT_GP10_DES, CFG_CORE_BIGNUM_MAX_BITS, CFG_RSA_PUB_EXPONENT_3},
    crypto::{
        bignum::{
            crypto_bignum_bin2bn, crypto_bignum_bn2bin, crypto_bignum_copy, crypto_bignum_num_bits,
            crypto_bignum_num_bytes,
        },
        crypto::{
            EccKeypair, EccPublicKey, Ed25519Keypair, Ed25519PublicKey, RsaKeypair, X448Keypair,
            X448PublicKey, X25519Keypair, X25519PublicKey, crypto_acipher_gen_ecc_key,
            crypto_acipher_gen_ed25519_key, crypto_acipher_gen_x448_key,
            crypto_acipher_gen_x25519_key,
        },
        crypto_impl::crypto_acipher_gen_rsa_key,
    },
    curve25519_key::{
        ATTR_OPS_INDEX_25519, KEY_SIZE_BYTES_25519, key32_clear, key32_to_binary,
        key32_update_from_binary,
    },
    libutee::{tee_api_objects::TEE_USAGE_DEFAULT, utee_defines::tee_u32_to_big_endian},
    memtag::memtag_strip_tag_vaddr,
    rng_software::crypto_rng_read,
    tee_obj::{TeeObj, TeeObjIdType, tee_obj_add, tee_obj_close, tee_obj_get},
    tee_pobj::with_pobj_usage_lock,
    tee_svc_storage::tee_svc_storage_write_usage,
    user_ta::user_ta_ctx,
    utils::{bit, bit32, slice_fmt},
    vm::vm_check_access_rights,
    x448_key::{
        ATTR_OPS_INDEX_448, KEY_SIZE_BYTES_448, key56_clear, key56_to_binary,
        key56_update_from_binary,
    },
};
use crate::tee::{
    TEE_TYPE_CONCAT_KDF_Z, TEE_TYPE_HKDF_IKM, TEE_TYPE_PBKDF2_PASSWORD,
    crypto::{bignum::BigNum, crypto::RsaPublicKey},
};

fn map_user_mem_error(err: MemError) -> u32 {
    match err {
        MemError::InvalidAddr | MemError::NoAccess => TEE_ERROR_BAD_PARAMETERS,
        _ => TEE_ERROR_GENERIC,
    }
}

pub(super) const TEE_TYPE_ATTR_REQUIRED: u32 = bit(1);
pub(super) const TEE_TYPE_ATTR_OPTIONAL_GROUP: u32 = bit(2);
pub(super) const TEE_TYPE_ATTR_SIZE_INDICATOR: u32 = bit(3);
pub(super) const TEE_TYPE_ATTR_GEN_KEY_OPT: u32 = bit(4);
pub(super) const TEE_TYPE_ATTR_GEN_KEY_REQ: u32 = bit(5);
pub(super) const TEE_TYPE_ATTR_BIGNUM_MAXBITS: u32 = bit(6);

// Handle storing of generic secret keys of varying lengths
pub(super) const ATTR_OPS_INDEX_SECRET: u32 = 0;
// Convert to/from big-endian byte array and provider-specific bignum
pub(super) const ATTR_OPS_INDEX_BIGNUM: u32 = 1;
// Convert to/from value attribute depending on direction
// Convert to/from big-endian byte array and provider-specific bignum
pub(super) const ATTR_OPS_INDEX_VALUE: u32 = 2;

#[repr(C)]
/// GP: `struct tee_cryp_obj_type_attrs`
pub(crate) struct TeeCrypObjTypeAttrs {
    attr_id: u32,
    flags: u16,
    ops_index: u16,
    // raw_offs: u16,
    // raw_size: u16,
}

#[derive(Debug)]
pub(crate) enum KernelAttribute {
    Value {
        id: u32,
        a: u32,
        b: u32,
    },
    Memref {
        id: u32,
        data: Box<[u8]>,
    },
    /// Output memref: kernel writes derived data back to the user buffer after the operation.
    MemrefOut {
        id: u32,
        user_addr: usize,
        len: usize,
    },
}

impl KernelAttribute {
    pub(crate) fn id(&self) -> u32 {
        match self {
            Self::Value { id, .. } | Self::Memref { id, .. } | Self::MemrefOut { id, .. } => *id,
        }
    }

    pub(crate) fn value(&self) -> TeeResult<(u32, u32)> {
        match self {
            Self::Value { a, b, .. } => Ok((*a, *b)),
            Self::Memref { .. } | Self::MemrefOut { .. } => Err(TEE_ERROR_BAD_PARAMETERS),
        }
    }

    pub(crate) fn memref_data(&self) -> TeeResult<&[u8]> {
        match self {
            Self::Memref { data, .. } => Ok(data),
            Self::Value { .. } | Self::MemrefOut { .. } => Err(TEE_ERROR_BAD_PARAMETERS),
        }
    }
}

pub(super) trait TeeCryptObjAttrOps {
    fn import_from_bytes(&mut self, buffer: &[u8]) -> TeeResult;

    fn export_to_bytes(&self) -> TeeResult<Box<[u8]>>;

    fn to_binary(&self, data: &mut [u8], offs: &mut usize) -> TeeResult;

    fn update_from_binary(&mut self, data: &[u8], offs: &mut usize) -> TeeResult;

    #[allow(dead_code)]
    fn update_from_obj(&mut self, src_obj: &TeeCryptObjAttr) -> TeeResult;

    fn update_from_crypto_attr_ref(&mut self, src_obj: &CryptoAttrRef) -> TeeResult;

    #[allow(dead_code)]
    fn free(&mut self) {
        // default do nothing
    }

    fn clear(&mut self) {
        // default do nothing
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct AttrValue(u32);

impl AttrValue {
    /// 获取内部值
    pub fn get(self) -> u32 {
        self.0
    }

    /// 获取内部值的引用
    pub fn as_u32(&self) -> &u32 {
        &self.0
    }

    /// 获取内部值的可变引用
    pub fn as_mut_u32(&mut self) -> &mut u32 {
        &mut self.0
    }
}

impl Deref for AttrValue {
    type Target = u32;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for AttrValue {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl From<u32> for AttrValue {
    fn from(value: u32) -> Self {
        AttrValue(value)
    }
}

impl From<AttrValue> for u32 {
    fn from(attr: AttrValue) -> Self {
        attr.0
    }
}

impl AsRef<u32> for AttrValue {
    fn as_ref(&self) -> &u32 {
        &self.0
    }
}

impl AsMut<u32> for AttrValue {
    fn as_mut(&mut self) -> &mut u32 {
        &mut self.0
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(super) enum TeeCryptObjAttr {
    /// GP: `secret_value`
    SecretValue(TeeCrypObjSecretWrapper),
    /// GP: `bignum`
    Bignum(BigNum),
    /// GP: `value`
    Value(AttrValue),
}

/// 用于包装不同类型的属性值引用
pub(super) enum CryptoAttrRef<'a> {
    BigNum(&'a mut BigNum),
    U32(&'a mut u32),
    SecretValue(&'a mut TeeCrypObjSecretWrapper),
    Key32(&'a mut [u8; KEY_SIZE_BYTES_25519]),
    Key56(&'a mut [u8; KEY_SIZE_BYTES_448]),
}

impl TeeCryptObjAttrOps for CryptoAttrRef<'_> {
    fn import_from_bytes(&mut self, buffer: &[u8]) -> TeeResult {
        match self {
            CryptoAttrRef::BigNum(bn) => bn.import_from_bytes(buffer),
            CryptoAttrRef::U32(val) => {
                let mut attr = AttrValue::from(**val);
                attr.import_from_bytes(buffer)?;
                **val = *attr.as_u32();
                Ok(())
            }
            CryptoAttrRef::SecretValue(attr) => attr.import_from_bytes(buffer),
            CryptoAttrRef::Key32(key) => {
                let bytes: &[u8; KEY_SIZE_BYTES_25519] =
                    buffer.try_into().map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
                key.copy_from_slice(bytes);
                Ok(())
            }
            CryptoAttrRef::Key56(key) => {
                let bytes: &[u8; KEY_SIZE_BYTES_448] =
                    buffer.try_into().map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
                key.copy_from_slice(bytes);
                Ok(())
            }
        }
    }

    fn export_to_bytes(&self) -> TeeResult<Box<[u8]>> {
        match self {
            CryptoAttrRef::BigNum(bn) => bn.export_to_bytes(),
            CryptoAttrRef::U32(val) => {
                let attr = AttrValue::from(**val);
                attr.export_to_bytes()
            }
            CryptoAttrRef::SecretValue(attr) => attr.export_to_bytes(),
            CryptoAttrRef::Key32(key) => Ok(key.to_vec().into_boxed_slice()),
            CryptoAttrRef::Key56(key) => Ok(key.to_vec().into_boxed_slice()),
        }
    }

    fn update_from_obj(&mut self, src_obj: &TeeCryptObjAttr) -> TeeResult {
        match self {
            CryptoAttrRef::BigNum(bn) => bn.update_from_obj(src_obj),
            CryptoAttrRef::U32(val) => {
                let mut attr = AttrValue::from(**val);
                attr.update_from_obj(src_obj)?;
                **val = *attr.as_u32();
                Ok(())
            }
            CryptoAttrRef::SecretValue(attr) => attr.update_from_obj(src_obj),
            CryptoAttrRef::Key32(_) | CryptoAttrRef::Key56(_) => Err(TEE_ERROR_BAD_PARAMETERS),
        }
    }

    fn update_from_crypto_attr_ref(&mut self, src_obj: &CryptoAttrRef) -> TeeResult {
        match self {
            CryptoAttrRef::BigNum(bn) => bn.update_from_crypto_attr_ref(src_obj),
            CryptoAttrRef::U32(val) => {
                let mut attr = AttrValue::from(**val);
                attr.update_from_crypto_attr_ref(src_obj)?;
                **val = *attr.as_u32();
                Ok(())
            }
            CryptoAttrRef::SecretValue(attr) => attr.update_from_crypto_attr_ref(src_obj),
            CryptoAttrRef::Key32(key) => match src_obj {
                CryptoAttrRef::Key32(src) => {
                    (**key).copy_from_slice(&**src);
                    Ok(())
                }
                _ => Err(TEE_ERROR_BAD_PARAMETERS),
            },
            CryptoAttrRef::Key56(key) => match src_obj {
                CryptoAttrRef::Key56(src) => {
                    (**key).copy_from_slice(&**src);
                    Ok(())
                }
                _ => Err(TEE_ERROR_BAD_PARAMETERS),
            },
        }
    }

    fn to_binary(&self, data: &mut [u8], offs: &mut usize) -> TeeResult {
        match self {
            CryptoAttrRef::BigNum(bn) => bn.to_binary(data, offs),
            CryptoAttrRef::U32(val) => {
                let attr = AttrValue::from(**val);
                attr.to_binary(data, offs)
            }
            CryptoAttrRef::SecretValue(attr) => attr.to_binary(data, offs),
            CryptoAttrRef::Key32(key) => key32_to_binary(key, data, offs),
            CryptoAttrRef::Key56(key) => key56_to_binary(key, data, offs),
        }
    }

    fn update_from_binary(&mut self, data: &[u8], offs: &mut usize) -> TeeResult {
        match self {
            CryptoAttrRef::BigNum(bn) => bn.update_from_binary(data, offs),
            CryptoAttrRef::U32(val) => {
                let mut attr = AttrValue::from(**val);
                attr.update_from_binary(data, offs)?;
                **val = *attr.as_u32();
                Ok(())
            }
            CryptoAttrRef::SecretValue(attr) => attr.update_from_binary(data, offs),
            CryptoAttrRef::Key32(key) => key32_update_from_binary(key, data, offs),
            CryptoAttrRef::Key56(key) => key56_update_from_binary(key, data, offs),
        }
    }

    fn free(&mut self) {
        match self {
            CryptoAttrRef::BigNum(bn) => bn.free(),
            CryptoAttrRef::U32(val) => **val = 0,
            CryptoAttrRef::SecretValue(attr) => attr.free(),
            CryptoAttrRef::Key32(key) => key32_clear(key),
            CryptoAttrRef::Key56(key) => key56_clear(key),
        }
    }

    fn clear(&mut self) {
        match self {
            CryptoAttrRef::BigNum(bn) => bn.clear(),
            CryptoAttrRef::U32(val) => **val = 0,
            CryptoAttrRef::SecretValue(attr) => attr.clear(),
            CryptoAttrRef::Key32(key) => key32_clear(key),
            CryptoAttrRef::Key56(key) => key56_clear(key),
        }
    }
}

impl<'a> CryptoAttrRef<'a> {
    /// 尝试转换为 &BigNum，如果不是 BigNum 类型则返回 None
    pub(super) fn as_bignum(&self) -> Option<&BigNum> {
        match self {
            CryptoAttrRef::BigNum(bn) => Some(bn),
            _ => None,
        }
    }
}

/// GP: `tee_crypto_ops`
pub(super) trait TeeCryptoOps {
    // const TEE_TYPE : u32;
    fn new(key_type: u32, key_size_bits: usize) -> TeeResult<Self>
    where
        Self: Sized;

    fn get_attr_by_id(&mut self, attr_id: c_ulong) -> TeeResult<CryptoAttrRef<'_>>
    where
        Self: Sized;
}

/// 加密对象类型
///
/// 对应类型 TEE_TYPE_*
#[derive(Default)]
pub(super) enum TeeCryptObj {
    /// GP: `rsa_keypair`
    RsaKeypair(RsaKeypair),
    /// GP: `rsa_public_key`
    RsaPublicKey(RsaPublicKey),
    /// GP: `ecc_keypair`
    EccKeypair(EccKeypair),
    /// GP: `ecc_public_key`
    EccPublicKey(EccPublicKey),
    /// GP: `ed25519_keypair`
    Ed25519Keypair(Ed25519Keypair),
    /// GP: `ed25519_public_key`
    Ed25519PublicKey(Ed25519PublicKey),
    /// GP: `x25519_keypair`
    X25519Keypair(X25519Keypair),
    /// GP: `x25519_public_key`
    X25519PublicKey(X25519PublicKey),
    /// GP: `x448_keypair`
    X448Keypair(X448Keypair),
    /// GP: `x448_public_key`
    X448PublicKey(X448PublicKey),
    /// GP: `obj_secret`
    ObjSecret(TeeCrypObjSecretWrapper),
    // obj_value(AttrValue),
    // obj_bignum(BigNum),
    #[default]
    None,
}

impl Debug for TeeCryptObj {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TeeCryptObj::RsaKeypair(key) => write!(f, "TeeCryptObj::RsaKeypair:{:#?}", key),
            TeeCryptObj::RsaPublicKey(_) => write!(f, "TeeCryptObj::RsaPublicKey"),
            TeeCryptObj::EccKeypair(keypair) => {
                write!(f, "TeeCryptObj::EccKeypair:{:#?}", keypair)
            }
            TeeCryptObj::EccPublicKey(_) => write!(f, "TeeCryptObj::EccPublicKey"),
            TeeCryptObj::Ed25519Keypair(_) => write!(f, "TeeCryptObj::Ed25519Keypair"),
            TeeCryptObj::Ed25519PublicKey(_) => write!(f, "TeeCryptObj::Ed25519PublicKey"),
            TeeCryptObj::X25519Keypair(_) => write!(f, "TeeCryptObj::X25519Keypair"),
            TeeCryptObj::X25519PublicKey(_) => write!(f, "TeeCryptObj::X25519PublicKey"),
            TeeCryptObj::X448Keypair(_) => write!(f, "TeeCryptObj::X448Keypair"),
            TeeCryptObj::X448PublicKey(_) => write!(f, "TeeCryptObj::X448PublicKey"),
            TeeCryptObj::ObjSecret(_) => write!(f, "TeeCryptObj::ObjSecret"),
            TeeCryptObj::None => write!(f, "TeeCryptObj::None"),
        }
    }
}

// impl TeeCryptObj {
//     pub fn new(obj_type: TEE_ObjectType) -> Self {
//         match obj_type {
//             TEE_TYPE_ECC_PUBLIC_KEY => TeeCryptObj::EccPublicKey(EccPublicKey::default()),
//             TEE_TYPE_ECC_KEYPAIR => TeeCryptObj::EccKeypair(EccKeypair::default()),
//             _ => TeeCryptObj::None,
//         }
//     }
// }
impl TeeCryptoOps for TeeCryptObj {
    fn new(key_type: u32, key_size_bits: usize) -> TeeResult<Self>
    where
        Self: Sized,
    {
        match key_type {
            TEE_TYPE_RSA_KEYPAIR => {
                RsaKeypair::new(key_type, key_size_bits).map(TeeCryptObj::RsaKeypair)
            }
            TEE_TYPE_RSA_PUBLIC_KEY => {
                RsaPublicKey::new(key_type, key_size_bits).map(TeeCryptObj::RsaPublicKey)
            }
            TEE_TYPE_ECDSA_PUBLIC_KEY
            | TEE_TYPE_ECDH_PUBLIC_KEY
            | TEE_TYPE_SM2_DSA_PUBLIC_KEY
            | TEE_TYPE_SM2_PKE_PUBLIC_KEY
            | TEE_TYPE_SM2_KEP_PUBLIC_KEY => {
                EccPublicKey::new(key_type, key_size_bits).map(TeeCryptObj::EccPublicKey)
            }
            TEE_TYPE_ECDSA_KEYPAIR
            | TEE_TYPE_ECDH_KEYPAIR
            | TEE_TYPE_SM2_DSA_KEYPAIR
            | TEE_TYPE_SM2_PKE_KEYPAIR
            | TEE_TYPE_SM2_KEP_KEYPAIR => {
                EccKeypair::new(key_type, key_size_bits).map(TeeCryptObj::EccKeypair)
            }
            TEE_TYPE_ED25519_KEYPAIR => {
                Ed25519Keypair::new(key_type, key_size_bits).map(TeeCryptObj::Ed25519Keypair)
            }
            TEE_TYPE_ED25519_PUBLIC_KEY => Ed25519PublicKey::new(key_type, key_size_bits)
                .map(TeeCryptObj::Ed25519PublicKey),
            TEE_TYPE_X25519_KEYPAIR => {
                X25519Keypair::new(key_type, key_size_bits).map(TeeCryptObj::X25519Keypair)
            }
            TEE_TYPE_X25519_PUBLIC_KEY => {
                X25519PublicKey::new(key_type, key_size_bits).map(TeeCryptObj::X25519PublicKey)
            }
            TEE_TYPE_X448_KEYPAIR => {
                X448Keypair::new(key_type, key_size_bits).map(TeeCryptObj::X448Keypair)
            }
            TEE_TYPE_X448_PUBLIC_KEY => {
                X448PublicKey::new(key_type, key_size_bits).map(TeeCryptObj::X448PublicKey)
            }
            TEE_TYPE_DATA => Ok(TeeCryptObj::None),
            TEE_TYPE_AES
            | TEE_TYPE_DES
            | TEE_TYPE_DES3
            | TEE_TYPE_SM4
            | TEE_TYPE_HMAC_MD5
            | TEE_TYPE_HMAC_SHA1
            | TEE_TYPE_HMAC_SHA224
            | TEE_TYPE_HMAC_SHA256
            | TEE_TYPE_HMAC_SHA384
            | TEE_TYPE_HMAC_SHA512
            // | TEE_TYPE_HMAC_SHA3_224
            // | TEE_TYPE_HMAC_SHA3_256
            // | TEE_TYPE_HMAC_SHA3_384
            // | TEE_TYPE_HMAC_SHA3_512
            | TEE_TYPE_HMAC_SM3
            | TEE_TYPE_GENERIC_SECRET
            | TEE_TYPE_HKDF_IKM
            | TEE_TYPE_CONCAT_KDF_Z
            | TEE_TYPE_PBKDF2_PASSWORD
            => {
                <TeeCrypObjSecretWrapper as TeeCryptoOps>::new(key_type, key_size_bits).map(TeeCryptObj::ObjSecret)
            }
            _ => Err(TEE_ERROR_NOT_SUPPORTED),
        }
    }

    fn get_attr_by_id(&mut self, attr_id: c_ulong) -> TeeResult<CryptoAttrRef<'_>> {
        match self {
            TeeCryptObj::RsaKeypair(key) => key.get_attr_by_id(attr_id),
            TeeCryptObj::RsaPublicKey(key) => key.get_attr_by_id(attr_id),
            TeeCryptObj::EccPublicKey(key) => key.get_attr_by_id(attr_id),
            TeeCryptObj::EccKeypair(keypair) => keypair.get_attr_by_id(attr_id),
            TeeCryptObj::Ed25519Keypair(keypair) => keypair.get_attr_by_id(attr_id),
            TeeCryptObj::Ed25519PublicKey(key) => key.get_attr_by_id(attr_id),
            TeeCryptObj::X25519Keypair(keypair) => keypair.get_attr_by_id(attr_id),
            TeeCryptObj::X25519PublicKey(key) => key.get_attr_by_id(attr_id),
            TeeCryptObj::X448Keypair(keypair) => keypair.get_attr_by_id(attr_id),
            TeeCryptObj::X448PublicKey(key) => key.get_attr_by_id(attr_id),
            TeeCryptObj::ObjSecret(secret) => secret.get_attr_by_id(attr_id),
            _ => Err(TEE_ERROR_ITEM_NOT_FOUND),
        }
    }
}

impl TeeCryptObjAttrOps for TeeCryptObjAttr {
    fn import_from_bytes(&mut self, buffer: &[u8]) -> TeeResult {
        match self {
            TeeCryptObjAttr::SecretValue(attr) => attr.import_from_bytes(buffer),
            TeeCryptObjAttr::Bignum(attr) => attr.import_from_bytes(buffer),
            TeeCryptObjAttr::Value(attr) => attr.import_from_bytes(buffer),
        }
    }

    fn export_to_bytes(&self) -> TeeResult<Box<[u8]>> {
        match self {
            TeeCryptObjAttr::SecretValue(attr) => attr.export_to_bytes(),
            TeeCryptObjAttr::Bignum(attr) => attr.export_to_bytes(),
            TeeCryptObjAttr::Value(attr) => attr.export_to_bytes(),
        }
    }

    fn update_from_obj(&mut self, src_obj: &TeeCryptObjAttr) -> TeeResult {
        // TeeCryptObjAttr 需要根据 src_obj 的类型来提取对应的属性
        match self {
            TeeCryptObjAttr::SecretValue(attr) => attr.update_from_obj(src_obj),
            TeeCryptObjAttr::Bignum(attr) => attr.update_from_obj(src_obj),
            TeeCryptObjAttr::Value(attr) => attr.update_from_obj(src_obj),
        }
    }

    fn update_from_crypto_attr_ref(&mut self, src_obj: &CryptoAttrRef) -> TeeResult {
        match self {
            TeeCryptObjAttr::SecretValue(attr) => attr.update_from_crypto_attr_ref(src_obj),
            TeeCryptObjAttr::Bignum(attr) => attr.update_from_crypto_attr_ref(src_obj),
            TeeCryptObjAttr::Value(attr) => attr.update_from_crypto_attr_ref(src_obj),
        }
    }

    fn to_binary(&self, data: &mut [u8], offs: &mut usize) -> TeeResult {
        match self {
            TeeCryptObjAttr::SecretValue(attr) => attr.to_binary(data, offs),
            TeeCryptObjAttr::Bignum(attr) => attr.to_binary(data, offs),
            TeeCryptObjAttr::Value(attr) => attr.to_binary(data, offs),
        }
    }

    fn update_from_binary(&mut self, data: &[u8], offs: &mut usize) -> TeeResult {
        match self {
            TeeCryptObjAttr::SecretValue(attr) => attr.update_from_binary(data, offs),
            TeeCryptObjAttr::Bignum(attr) => attr.update_from_binary(data, offs),
            TeeCryptObjAttr::Value(attr) => attr.update_from_binary(data, offs),
        }
    }

    fn free(&mut self) {
        // 根据类型释放资源
        match self {
            TeeCryptObjAttr::SecretValue(attr) => attr.free(),
            TeeCryptObjAttr::Bignum(attr) => attr.free(),
            TeeCryptObjAttr::Value(attr) => attr.free(),
        }
    }

    fn clear(&mut self) {
        match self {
            TeeCryptObjAttr::SecretValue(attr) => attr.clear(),
            TeeCryptObjAttr::Bignum(attr) => attr.clear(),
            TeeCryptObjAttr::Value(attr) => attr.clear(),
        }
    }
}

impl TeeCryptObjAttrOps for AttrValue {
    fn import_from_bytes(&mut self, buffer: &[u8]) -> TeeResult {
        if buffer.len() < size_of::<u32>() {
            return Err(TEE_ERROR_GENERIC);
        }

        let value_bytes: [u8; size_of::<u32>()] = buffer[..size_of::<u32>()]
            .try_into()
            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        *self.as_mut_u32() = u32::from_ne_bytes(value_bytes);
        Ok(())
    }

    fn export_to_bytes(&self) -> TeeResult<Box<[u8]>> {
        let mut value_bytes = [0u8; size_of::<u32>() * 2];
        value_bytes[..size_of::<u32>()].copy_from_slice(&self.as_u32().to_ne_bytes());
        Ok(Box::new(value_bytes))
    }

    fn update_from_obj(&mut self, src_obj: &TeeCryptObjAttr) -> TeeResult {
        match src_obj {
            TeeCryptObjAttr::Value(value) => {
                *self = *value;
                Ok(())
            }
            _ => Err(TEE_ERROR_BAD_PARAMETERS),
        }
    }

    fn update_from_crypto_attr_ref(&mut self, src_obj: &CryptoAttrRef) -> TeeResult {
        match src_obj {
            CryptoAttrRef::U32(val) => {
                *self = AttrValue::from(**val);
                Ok(())
            }
            _ => Err(TEE_ERROR_BAD_PARAMETERS),
        }
    }

    fn to_binary(&self, data: &mut [u8], offs: &mut usize) -> TeeResult {
        let value: u32 = *self.as_u32();
        op_u32_to_binary_helper(value, data, offs)
    }

    fn update_from_binary(&mut self, data: &[u8], offs: &mut usize) -> TeeResult {
        let value_ref = self.as_mut_u32();
        op_u32_from_binary_helper(value_ref, data, offs)
    }

    fn free(&mut self) {
        // set value to 0
        self.clear();
    }

    fn clear(&mut self) {
        // set value to 0
        *self.as_mut_u32() = 0;
    }
}

impl TeeCryptObjAttrOps for BigNum {
    fn import_from_bytes(&mut self, buffer: &[u8]) -> TeeResult {
        crypto_bignum_bin2bn(buffer, self)?;
        Ok(())
    }

    fn export_to_bytes(&self) -> TeeResult<Box<[u8]>> {
        let req_size = crypto_bignum_num_bytes(self)?;
        let mut kbuf: Box<[u8]> = vec![0u8; req_size as _].into_boxed_slice();
        crypto_bignum_bn2bin(self, kbuf.as_mut())?;
        Ok(kbuf)
    }

    fn update_from_obj(&mut self, src_obj: &TeeCryptObjAttr) -> TeeResult {
        match src_obj {
            TeeCryptObjAttr::Bignum(value) => {
                crypto_bignum_copy(self, value);
                Ok(())
            }
            _ => Err(TEE_ERROR_BAD_PARAMETERS),
        }
    }

    fn update_from_crypto_attr_ref(&mut self, src_obj: &CryptoAttrRef) -> TeeResult {
        match src_obj {
            CryptoAttrRef::BigNum(bn) => {
                crypto_bignum_copy(self, bn);
                Ok(())
            }
            _ => Err(TEE_ERROR_BAD_PARAMETERS),
        }
    }

    fn to_binary(&self, data: &mut [u8], offs: &mut usize) -> TeeResult {
        let n: u32 = crypto_bignum_num_bytes(self)? as u32;

        op_u32_to_binary_helper(n, data, offs)?;
        let next_offs: usize = offs.checked_add(n as usize).ok_or(TEE_ERROR_OVERFLOW)?;

        if data.len() >= next_offs {
            crypto_bignum_bn2bin(self, &mut data[*offs..*offs + n as usize])?;
        }

        *offs = next_offs;
        Ok(())
    }

    fn update_from_binary(&mut self, data: &[u8], offs: &mut usize) -> TeeResult {
        let mut n: u32 = 0;

        op_u32_from_binary_helper(&mut n, data, offs)?;

        if offs
            .checked_add(n as usize)
            .ok_or(TEE_ERROR_BAD_PARAMETERS)?
            > data.len()
        {
            return Err(TEE_ERROR_BAD_PARAMETERS);
        }

        crypto_bignum_bin2bn(&data[*offs..*offs + n as usize], self)?;

        *offs += n as usize;

        Ok(())
    }

    fn clear(&mut self) {
        BigNum::clear(self);
    }
}

impl TeeCryptObjAttrOps for TeeCrypObjSecretWrapper {
    fn import_from_bytes(&mut self, buffer: &[u8]) -> TeeResult {
        let size = buffer.len();

        if size > self.secret().alloc_size as usize {
            return Err(TEE_ERROR_SHORT_BUFFER);
        }

        let data_slice = self.data_mut();
        data_slice[..size].copy_from_slice(buffer);
        self.secret_mut().key_size = size as u32;

        Ok(())
    }

    fn export_to_bytes(&self) -> TeeResult<Box<[u8]>> {
        Ok(self.key().to_vec().into_boxed_slice())
    }

    fn update_from_obj(&mut self, src_obj: &TeeCryptObjAttr) -> TeeResult {
        // 从 TeeCryptObjAttr 中提取 TeeCrypObjSecretWrapper
        match src_obj {
            TeeCryptObjAttr::SecretValue(secret) => self.from(secret),
            _ => Err(TEE_ERROR_BAD_PARAMETERS),
        }
    }

    fn update_from_crypto_attr_ref(&mut self, src_obj: &CryptoAttrRef) -> TeeResult {
        match src_obj {
            CryptoAttrRef::SecretValue(secret) => self.from(secret),
            _ => Err(TEE_ERROR_BAD_PARAMETERS),
        }
    }

    fn to_binary(&self, data: &mut [u8], offs: &mut usize) -> TeeResult {
        let key = self.secret();

        op_u32_to_binary_helper(key.key_size, data, offs)?;

        let next_offs: usize = offs
            .checked_add(key.key_size as usize)
            .ok_or(TEE_ERROR_OVERFLOW)?;

        if data.len() >= next_offs {
            data[*offs..*offs + key.key_size as usize]
                .copy_from_slice(&self.data()[..key.key_size as usize]);
        }
        *offs = next_offs;

        Ok(())
    }

    fn update_from_binary(&mut self, data: &[u8], offs: &mut usize) -> TeeResult {
        let key = self.secret();
        let mut s: u32 = 0;

        op_u32_from_binary_helper(&mut s, data, offs)?;

        if offs
            .checked_add(s as usize)
            .ok_or(TEE_ERROR_BAD_PARAMETERS)?
            > data.len()
        {
            return Err(TEE_ERROR_BAD_PARAMETERS);
        }

        // 数据大小必须适合分配的缓冲区
        if s > key.alloc_size {
            return Err(TEE_ERROR_BAD_PARAMETERS);
        }

        self.secret_mut().key_size = s;

        let data_slice = self.data_mut();
        data_slice[..s as usize].copy_from_slice(&data[*offs..*offs + s as usize]);

        *offs += s as usize;

        Ok(())
    }

    fn free(&mut self) {
        self.clear();
    }

    fn clear(&mut self) {
        // set key_size to 0
        self.secret_mut().key_size = 0;
        // set data to 0
        self.data_mut().fill(0);
    }
}

/// GP: `tee_cryp_obj_ecc_pub_key_attrs`
pub(super) const TEE_CRYP_OBJ_ECC_PUB_KEY_ATTRS: &[TeeCrypObjTypeAttrs] = &[
    TeeCrypObjTypeAttrs {
        attr_id: TEE_ATTR_ECC_PUBLIC_VALUE_X,
        flags: TEE_TYPE_ATTR_REQUIRED as _,
        ops_index: ATTR_OPS_INDEX_BIGNUM as _,
    },
    TeeCrypObjTypeAttrs {
        attr_id: TEE_ATTR_ECC_PUBLIC_VALUE_Y,
        flags: TEE_TYPE_ATTR_REQUIRED as _,
        ops_index: ATTR_OPS_INDEX_BIGNUM as _,
    },
    TeeCrypObjTypeAttrs {
        attr_id: TEE_ATTR_ECC_CURVE,
        flags: (TEE_TYPE_ATTR_REQUIRED | TEE_TYPE_ATTR_SIZE_INDICATOR) as _,
        ops_index: ATTR_OPS_INDEX_VALUE as _,
    },
];

/// GP: `tee_cryp_obj_rsa_pub_key_attrs`
pub(super) const TEE_CRYP_OBJ_RSA_PUB_KEY_ATTRS: &[TeeCrypObjTypeAttrs] = &[
    TeeCrypObjTypeAttrs {
        attr_id: TEE_ATTR_RSA_MODULUS,
        flags: (TEE_TYPE_ATTR_REQUIRED | TEE_TYPE_ATTR_SIZE_INDICATOR) as _,
        ops_index: ATTR_OPS_INDEX_BIGNUM as _,
    },
    TeeCrypObjTypeAttrs {
        attr_id: TEE_ATTR_RSA_PUBLIC_EXPONENT,
        flags: TEE_TYPE_ATTR_REQUIRED as _,
        ops_index: ATTR_OPS_INDEX_BIGNUM as _,
    },
];

/// GP: `tee_cryp_obj_rsa_keypair_attrs`
pub(super) const TEE_CRYP_OBJ_RSA_KEYPAIR_ATTRS: &[TeeCrypObjTypeAttrs] = &[
    TeeCrypObjTypeAttrs {
        attr_id: TEE_ATTR_RSA_MODULUS,
        flags: (TEE_TYPE_ATTR_REQUIRED | TEE_TYPE_ATTR_SIZE_INDICATOR) as _,
        ops_index: ATTR_OPS_INDEX_BIGNUM as _,
    },
    TeeCrypObjTypeAttrs {
        attr_id: TEE_ATTR_RSA_PUBLIC_EXPONENT,
        flags: (TEE_TYPE_ATTR_REQUIRED | TEE_TYPE_ATTR_GEN_KEY_OPT) as _,
        ops_index: ATTR_OPS_INDEX_BIGNUM as _,
    },
    TeeCrypObjTypeAttrs {
        attr_id: TEE_ATTR_RSA_PRIVATE_EXPONENT,
        flags: TEE_TYPE_ATTR_REQUIRED as _,
        ops_index: ATTR_OPS_INDEX_BIGNUM as _,
    },
    TeeCrypObjTypeAttrs {
        attr_id: TEE_ATTR_RSA_PRIME1,
        flags: TEE_TYPE_ATTR_OPTIONAL_GROUP as _,
        ops_index: ATTR_OPS_INDEX_BIGNUM as _,
    },
    TeeCrypObjTypeAttrs {
        attr_id: TEE_ATTR_RSA_PRIME2,
        flags: TEE_TYPE_ATTR_OPTIONAL_GROUP as _,
        ops_index: ATTR_OPS_INDEX_BIGNUM as _,
    },
    TeeCrypObjTypeAttrs {
        attr_id: TEE_ATTR_RSA_EXPONENT1,
        flags: TEE_TYPE_ATTR_OPTIONAL_GROUP as _,
        ops_index: ATTR_OPS_INDEX_BIGNUM as _,
    },
    TeeCrypObjTypeAttrs {
        attr_id: TEE_ATTR_RSA_EXPONENT2,
        flags: TEE_TYPE_ATTR_OPTIONAL_GROUP as _,
        ops_index: ATTR_OPS_INDEX_BIGNUM as _,
    },
    TeeCrypObjTypeAttrs {
        attr_id: TEE_ATTR_RSA_COEFFICIENT,
        flags: TEE_TYPE_ATTR_OPTIONAL_GROUP as _,
        ops_index: ATTR_OPS_INDEX_BIGNUM as _,
    },
];

/// GP: `tee_cryp_obj_ecc_keypair_attrs`
pub(super) const TEE_CRYP_OBJ_ECC_KEYPAIR_ATTRS: &[TeeCrypObjTypeAttrs] = &[
    TeeCrypObjTypeAttrs {
        attr_id: TEE_ATTR_ECC_PRIVATE_VALUE,
        flags: TEE_TYPE_ATTR_REQUIRED as _,
        ops_index: ATTR_OPS_INDEX_BIGNUM as _,
    },
    TeeCrypObjTypeAttrs {
        attr_id: TEE_ATTR_ECC_PUBLIC_VALUE_X,
        flags: TEE_TYPE_ATTR_REQUIRED as _,
        ops_index: ATTR_OPS_INDEX_BIGNUM as _,
    },
    TeeCrypObjTypeAttrs {
        attr_id: TEE_ATTR_ECC_PUBLIC_VALUE_Y,
        flags: TEE_TYPE_ATTR_REQUIRED as _,
        ops_index: ATTR_OPS_INDEX_BIGNUM as _,
    },
    TeeCrypObjTypeAttrs {
        attr_id: TEE_ATTR_ECC_CURVE,
        flags: (TEE_TYPE_ATTR_REQUIRED | TEE_TYPE_ATTR_SIZE_INDICATOR | TEE_TYPE_ATTR_GEN_KEY_REQ)
            as _,
        ops_index: ATTR_OPS_INDEX_VALUE as _,
    },
];

/// GP: `tee_cryp_obj_sm2_keypair_attrs`
pub(super) const TEE_CRYP_OBJ_SM2_KEYPAIR_ATTRS: &[TeeCrypObjTypeAttrs] = &[
    TeeCrypObjTypeAttrs {
        attr_id: TEE_ATTR_ECC_PRIVATE_VALUE,
        flags: TEE_TYPE_ATTR_REQUIRED as _,
        ops_index: ATTR_OPS_INDEX_BIGNUM as _,
    },
    TeeCrypObjTypeAttrs {
        attr_id: TEE_ATTR_ECC_PUBLIC_VALUE_X,
        flags: TEE_TYPE_ATTR_REQUIRED as _,
        ops_index: ATTR_OPS_INDEX_BIGNUM as _,
    },
    TeeCrypObjTypeAttrs {
        attr_id: TEE_ATTR_ECC_PUBLIC_VALUE_Y,
        flags: TEE_TYPE_ATTR_REQUIRED as _,
        ops_index: ATTR_OPS_INDEX_BIGNUM as _,
    },
];

/// GP: `tee_cryp_obj_sm2_pub_key_attrs`
pub(super) const TEE_CRYP_OBJ_SM2_PUB_KEY_ATTRS: &[TeeCrypObjTypeAttrs] = &[
    TeeCrypObjTypeAttrs {
        attr_id: TEE_ATTR_ECC_PUBLIC_VALUE_X,
        flags: TEE_TYPE_ATTR_REQUIRED as _,
        ops_index: ATTR_OPS_INDEX_BIGNUM as _,
    },
    TeeCrypObjTypeAttrs {
        attr_id: TEE_ATTR_ECC_PUBLIC_VALUE_Y,
        flags: TEE_TYPE_ATTR_REQUIRED as _,
        ops_index: ATTR_OPS_INDEX_BIGNUM as _,
    },
];

/// GP: `tee_cryp_obj_ed25519_keypair_attrs`
pub(super) const TEE_CRYP_OBJ_ED25519_KEYPAIR_ATTRS: &[TeeCrypObjTypeAttrs] = &[
    TeeCrypObjTypeAttrs {
        attr_id: TEE_ATTR_ED25519_PRIVATE_VALUE,
        flags: TEE_TYPE_ATTR_REQUIRED as _,
        ops_index: ATTR_OPS_INDEX_25519 as _,
    },
    TeeCrypObjTypeAttrs {
        attr_id: TEE_ATTR_ED25519_PUBLIC_VALUE,
        flags: TEE_TYPE_ATTR_REQUIRED as _,
        ops_index: ATTR_OPS_INDEX_25519 as _,
    },
];

/// GP: `tee_cryp_obj_ed25519_pub_key_attrs`
pub(super) const TEE_CRYP_OBJ_ED25519_PUB_KEY_ATTRS: &[TeeCrypObjTypeAttrs] =
    &[TeeCrypObjTypeAttrs {
        attr_id: TEE_ATTR_ED25519_PUBLIC_VALUE,
        flags: TEE_TYPE_ATTR_REQUIRED as _,
        ops_index: ATTR_OPS_INDEX_25519 as _,
    }];

/// GP: `tee_cryp_obj_x25519_keypair_attrs`
pub(super) const TEE_CRYP_OBJ_X25519_KEYPAIR_ATTRS: &[TeeCrypObjTypeAttrs] = &[
    TeeCrypObjTypeAttrs {
        attr_id: TEE_ATTR_X25519_PRIVATE_VALUE,
        flags: TEE_TYPE_ATTR_REQUIRED as _,
        ops_index: ATTR_OPS_INDEX_25519 as _,
    },
    TeeCrypObjTypeAttrs {
        attr_id: TEE_ATTR_X25519_PUBLIC_VALUE,
        flags: TEE_TYPE_ATTR_REQUIRED as _,
        ops_index: ATTR_OPS_INDEX_25519 as _,
    },
];

/// GP: `tee_cryp_obj_x25519_pub_key_attrs`
pub(super) const TEE_CRYP_OBJ_X25519_PUB_KEY_ATTRS: &[TeeCrypObjTypeAttrs] =
    &[TeeCrypObjTypeAttrs {
        attr_id: TEE_ATTR_X25519_PUBLIC_VALUE,
        flags: TEE_TYPE_ATTR_REQUIRED as _,
        ops_index: ATTR_OPS_INDEX_25519 as _,
    }];

/// GP: `tee_cryp_obj_x448_keypair_attrs`
pub(super) const TEE_CRYP_OBJ_X448_KEYPAIR_ATTRS: &[TeeCrypObjTypeAttrs] = &[
    TeeCrypObjTypeAttrs {
        attr_id: TEE_ATTR_X448_PRIVATE_VALUE,
        flags: TEE_TYPE_ATTR_REQUIRED as _,
        ops_index: ATTR_OPS_INDEX_448 as _,
    },
    TeeCrypObjTypeAttrs {
        attr_id: TEE_ATTR_X448_PUBLIC_VALUE,
        flags: TEE_TYPE_ATTR_REQUIRED as _,
        ops_index: ATTR_OPS_INDEX_448 as _,
    },
];

/// GP: `tee_cryp_obj_x448_pub_key_attrs`
pub(super) const TEE_CRYP_OBJ_X448_PUB_KEY_ATTRS: &[TeeCrypObjTypeAttrs] = &[TeeCrypObjTypeAttrs {
    attr_id: TEE_ATTR_X448_PUBLIC_VALUE,
    flags: TEE_TYPE_ATTR_REQUIRED as _,
    ops_index: ATTR_OPS_INDEX_448 as _,
}];

#[repr(C)]
/// GP: `struct tee_cryp_obj_type_props`
pub(super) struct TeeCrypObjTypeProps {
    pub obj_type: TEE_ObjectType,
    pub min_size: u16,
    pub max_size: u16,
    pub alloc_size: u16,
    pub quanta: u8,
    pub num_type_attrs: u8,
    pub type_attrs: &'static [TeeCrypObjTypeAttrs],
}

impl Debug for TeeCrypObjTypeProps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "TeeCrypObjTypeProps{{obj_type: {:#06X?}, min_size: {:#04X?}, max_size: {:#04X?}, \
             alloc_size: {:#04X?}, quanta: {:#03X?}, num_type_attrs: {:#03X?}, type_attrs.id: \
             {:X?}}}",
            self.obj_type,
            self.min_size,
            self.max_size,
            self.alloc_size,
            self.quanta,
            self.num_type_attrs,
            self.type_attrs
                .iter()
                .map(|attr| attr.attr_id)
                .collect::<Vec<_>>()
        )
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
/// GP: `struct tee_cryp_obj_secret`
pub(crate) struct TeeCrypObjSecret {
    pub key_size: u32,
    pub alloc_size: u32,
}

/// GP: `struct tee_cryp_obj_secret` (wrapper with key material buffer)
pub(super) struct TeeCrypObjSecretWrapper {
    secret: TeeCrypObjSecret,
    data: Box<[u8]>,
}

impl Debug for TeeCrypObjSecretWrapper {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "TeeCrypObjSecretWrapper{{key: {:#?}, alloc_size: {:#06X?}}}",
            slice_fmt(self.key()),
            self.secret().alloc_size
        )
    }
}

impl TeeCrypObjSecretWrapper {
    pub(super) fn new(alloc_size: usize) -> Self {
        Self {
            secret: TeeCrypObjSecret {
                key_size: 0,
                alloc_size: alloc_size as u32,
            },
            data: vec![0u8; alloc_size].into_boxed_slice(),
        }
    }

    pub(super) fn secret(&self) -> &TeeCrypObjSecret {
        &self.secret
    }

    pub(super) fn secret_mut(&mut self) -> &mut TeeCrypObjSecret {
        &mut self.secret
    }

    pub(super) fn data_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }

    pub(super) fn data(&self) -> &[u8] {
        &self.data
    }

    pub(super) fn key(&self) -> &[u8] {
        &self.data[..self.secret.key_size as usize]
    }

    #[cfg(unittest)]
    pub(super) fn set_secret_data(&mut self, data: &[u8]) -> TeeResult {
        if data.len() > self.secret().alloc_size as usize {
            return Err(TEE_ERROR_BAD_PARAMETERS);
        }
        let data_slice = self.data_mut();
        data_slice[..data.len()].copy_from_slice(data);
        self.secret_mut().key_size = data.len() as u32;
        Ok(())
    }

    pub(super) fn from(&mut self, secret: &TeeCrypObjSecretWrapper) -> TeeResult {
        let key = self.secret();
        let src_key = secret.secret();
        let src_key_size = src_key.key_size;

        if src_key_size > key.alloc_size {
            return Err(TEE_ERROR_BAD_STATE);
        }

        let key_data = self.data_mut();
        let src_key_data = secret.data();

        key_data[..src_key_size as usize].copy_from_slice(&src_key_data[..src_key_size as usize]);
        self.secret_mut().key_size = src_key_size;

        Ok(())
    }
}

impl Clone for TeeCrypObjSecretWrapper {
    fn clone(&self) -> Self {
        let src_secret = self.secret();
        let alloc_size = src_secret.alloc_size as usize;
        let key_size = src_secret.key_size as usize;

        let mut new_wrapper = Self::new(alloc_size);
        new_wrapper.secret_mut().key_size = key_size as u32;

        if key_size > 0 {
            let src_data = self.data();
            let dst_data = new_wrapper.data_mut();
            dst_data[..key_size].copy_from_slice(&src_data[..key_size]);
        }

        new_wrapper
    }
}

impl TeeCryptoOps for TeeCrypObjSecretWrapper {
    fn new(_key_type: u32, key_size_bits: usize) -> TeeResult<Self> {
        Ok(Self::new(key_size_bits))
    }

    fn get_attr_by_id(&mut self, _attr_id: c_ulong) -> TeeResult<CryptoAttrRef<'_>> {
        Ok(CryptoAttrRef::SecretValue(self))
    }
}

pub(super) static TEE_CRYP_OBJ_SECRET_VALUE_ATTRS: [TeeCrypObjTypeAttrs; 1] =
    [TeeCrypObjTypeAttrs {
        attr_id: TEE_ATTR_SECRET_VALUE,
        flags: (TEE_TYPE_ATTR_REQUIRED | TEE_TYPE_ATTR_SIZE_INDICATOR) as _,
        ops_index: ATTR_OPS_INDEX_SECRET as _,
    }];

pub(super) static TEE_CRYP_OBJ_HKDF_IKM_ATTRS: [TeeCrypObjTypeAttrs; 1] = [TeeCrypObjTypeAttrs {
    attr_id: crate::tee::TEE_ATTR_HKDF_IKM,
    flags: (TEE_TYPE_ATTR_REQUIRED | TEE_TYPE_ATTR_SIZE_INDICATOR) as _,
    ops_index: ATTR_OPS_INDEX_SECRET as _,
}];

pub(super) static TEE_CRYP_OBJ_CONCAT_KDF_Z_ATTRS: [TeeCrypObjTypeAttrs; 1] =
    [TeeCrypObjTypeAttrs {
        attr_id: crate::tee::TEE_ATTR_CONCAT_KDF_Z,
        flags: (TEE_TYPE_ATTR_REQUIRED | TEE_TYPE_ATTR_SIZE_INDICATOR) as _,
        ops_index: ATTR_OPS_INDEX_SECRET as _,
    }];

pub(super) static TEE_CRYP_OBJ_PBKDF2_PASSWORD_ATTRS: [TeeCrypObjTypeAttrs; 1] =
    [TeeCrypObjTypeAttrs {
        attr_id: crate::tee::TEE_ATTR_PBKDF2_PASSWORD,
        flags: (TEE_TYPE_ATTR_REQUIRED | TEE_TYPE_ATTR_SIZE_INDICATOR) as _,
        ops_index: ATTR_OPS_INDEX_SECRET as _,
    }];

pub(super) const fn prop(
    obj_type: TEE_ObjectType,
    quanta: u8,
    min_size: u16,
    max_size: u16,
    alloc_size: u16,
    type_attrs: &'static [TeeCrypObjTypeAttrs],
) -> TeeCrypObjTypeProps {
    TeeCrypObjTypeProps {
        obj_type,
        min_size,
        max_size,
        alloc_size,
        quanta,
        num_type_attrs: type_attrs.len() as u8,
        type_attrs,
    }
}

pub(super) static TEE_CRYP_OBJ_PROPS: [TeeCrypObjTypeProps; 33] = [
    // AES
    prop(
        TEE_TYPE_AES,
        64,
        128,
        256,
        256 / 8,
        &TEE_CRYP_OBJ_SECRET_VALUE_ATTRS,
    ),
    // DES
    prop(
        TEE_TYPE_DES,
        64,
        64,
        64,
        64 / 8,
        &TEE_CRYP_OBJ_SECRET_VALUE_ATTRS,
    ),
    // DES3
    prop(
        TEE_TYPE_DES3,
        64,
        128,
        192,
        192 / 8,
        &TEE_CRYP_OBJ_SECRET_VALUE_ATTRS,
    ),
    // SM4
    prop(
        TEE_TYPE_SM4,
        128,
        128,
        128,
        128 / 8,
        &TEE_CRYP_OBJ_SECRET_VALUE_ATTRS,
    ),
    // HMAC-MD5
    prop(
        TEE_TYPE_HMAC_MD5,
        8,
        64,
        512,
        512 / 8,
        &TEE_CRYP_OBJ_SECRET_VALUE_ATTRS,
    ),
    // HMAC-SHA1
    prop(
        TEE_TYPE_HMAC_SHA1,
        8,
        80,
        512,
        512 / 8,
        &TEE_CRYP_OBJ_SECRET_VALUE_ATTRS,
    ),
    // HMAC-SHA224
    prop(
        TEE_TYPE_HMAC_SHA224,
        8,
        112,
        512,
        512 / 8,
        &TEE_CRYP_OBJ_SECRET_VALUE_ATTRS,
    ),
    // HMAC-SHA256
    prop(
        TEE_TYPE_HMAC_SHA256,
        8,
        192,
        1024,
        1024 / 8,
        &TEE_CRYP_OBJ_SECRET_VALUE_ATTRS,
    ),
    // HMAC-SHA384
    prop(
        TEE_TYPE_HMAC_SHA384,
        8,
        256,
        1024,
        1024 / 8,
        &TEE_CRYP_OBJ_SECRET_VALUE_ATTRS,
    ),
    // HMAC-SHA512
    prop(
        TEE_TYPE_HMAC_SHA512,
        8,
        256,
        1024,
        1024 / 8,
        &TEE_CRYP_OBJ_SECRET_VALUE_ATTRS,
    ),
    // HMAC-SM3
    prop(
        TEE_TYPE_HMAC_SM3,
        8,
        80,
        1024,
        512 / 8,
        &TEE_CRYP_OBJ_SECRET_VALUE_ATTRS,
    ),
    prop(
        TEE_TYPE_GENERIC_SECRET,
        8,
        0,
        4096,
        4096 / 8,
        &TEE_CRYP_OBJ_SECRET_VALUE_ATTRS,
    ),
    prop(
        crate::tee::TEE_TYPE_HKDF_IKM,
        8,
        0,
        4096,
        4096 / 8,
        &TEE_CRYP_OBJ_HKDF_IKM_ATTRS,
    ),
    prop(
        TEE_TYPE_CONCAT_KDF_Z,
        8,
        0,
        4096,
        4096 / 8,
        &TEE_CRYP_OBJ_CONCAT_KDF_Z_ATTRS,
    ),
    prop(
        TEE_TYPE_PBKDF2_PASSWORD,
        8,
        0,
        4096,
        4096 / 8,
        &TEE_CRYP_OBJ_PBKDF2_PASSWORD_ATTRS,
    ),
    // RSA keypair
    prop(
        TEE_TYPE_RSA_KEYPAIR,
        1,
        256,
        CFG_CORE_BIGNUM_MAX_BITS as _,
        0,
        TEE_CRYP_OBJ_RSA_KEYPAIR_ATTRS,
    ),
    prop(
        TEE_TYPE_RSA_PUBLIC_KEY,
        1,
        256,
        CFG_CORE_BIGNUM_MAX_BITS as _,
        0,
        TEE_CRYP_OBJ_RSA_PUB_KEY_ATTRS,
    ),
    prop(
        TEE_TYPE_ECDSA_KEYPAIR,
        1,
        192,
        521,
        0,
        TEE_CRYP_OBJ_ECC_KEYPAIR_ATTRS,
    ),
    prop(
        TEE_TYPE_ECDSA_PUBLIC_KEY,
        1,
        192,
        521,
        0,
        TEE_CRYP_OBJ_ECC_PUB_KEY_ATTRS,
    ),
    prop(
        TEE_TYPE_ECDH_KEYPAIR,
        1,
        192,
        521,
        0,
        TEE_CRYP_OBJ_ECC_KEYPAIR_ATTRS,
    ),
    prop(
        TEE_TYPE_ECDH_PUBLIC_KEY,
        1,
        192,
        521,
        0,
        TEE_CRYP_OBJ_ECC_PUB_KEY_ATTRS,
    ),
    prop(
        TEE_TYPE_SM2_DSA_KEYPAIR,
        1,
        256,
        256,
        0,
        TEE_CRYP_OBJ_SM2_KEYPAIR_ATTRS,
    ),
    prop(
        TEE_TYPE_SM2_DSA_PUBLIC_KEY,
        1,
        256,
        256,
        0,
        TEE_CRYP_OBJ_SM2_PUB_KEY_ATTRS,
    ),
    prop(
        TEE_TYPE_SM2_PKE_KEYPAIR,
        1,
        256,
        256,
        0,
        TEE_CRYP_OBJ_SM2_KEYPAIR_ATTRS,
    ),
    prop(
        TEE_TYPE_SM2_PKE_PUBLIC_KEY,
        1,
        256,
        256,
        0,
        TEE_CRYP_OBJ_SM2_PUB_KEY_ATTRS,
    ),
    prop(
        TEE_TYPE_SM2_KEP_PUBLIC_KEY,
        1,
        256,
        256,
        0,
        TEE_CRYP_OBJ_SM2_PUB_KEY_ATTRS,
    ),
    prop(
        TEE_TYPE_SM2_KEP_KEYPAIR,
        1,
        256,
        256,
        0,
        TEE_CRYP_OBJ_SM2_KEYPAIR_ATTRS,
    ),
    prop(
        TEE_TYPE_ED25519_PUBLIC_KEY,
        1,
        256,
        256,
        0,
        TEE_CRYP_OBJ_ED25519_PUB_KEY_ATTRS,
    ),
    prop(
        TEE_TYPE_ED25519_KEYPAIR,
        1,
        256,
        256,
        0,
        TEE_CRYP_OBJ_ED25519_KEYPAIR_ATTRS,
    ),
    prop(
        TEE_TYPE_X25519_PUBLIC_KEY,
        1,
        256,
        256,
        0,
        TEE_CRYP_OBJ_X25519_PUB_KEY_ATTRS,
    ),
    prop(
        TEE_TYPE_X25519_KEYPAIR,
        1,
        256,
        256,
        0,
        TEE_CRYP_OBJ_X25519_KEYPAIR_ATTRS,
    ),
    prop(
        TEE_TYPE_X448_PUBLIC_KEY,
        1,
        448,
        448,
        0,
        TEE_CRYP_OBJ_X448_PUB_KEY_ATTRS,
    ),
    prop(
        TEE_TYPE_X448_KEYPAIR,
        1,
        448,
        448,
        0,
        TEE_CRYP_OBJ_X448_KEYPAIR_ATTRS,
    ),
];

pub(super) fn tee_obj_set_type(
    obj: &mut TeeObj,
    obj_type: u32,
    max_key_size: size_t,
) -> TeeResult<isize> {
    // Can only set type for newly allocated objs
    if !obj.attr.is_empty() {
        return Err(TEE_ERROR_BAD_STATE);
    }

    if obj_type == TEE_TYPE_DATA {
        if max_key_size != 0 {
            return Err(TEE_ERROR_NOT_SUPPORTED);
        }

        obj.attr.push(TeeCryptObj::None);
    } else {
        // Find description of object
        let type_props = tee_svc_find_type_props(obj_type).ok_or(TEE_ERROR_NOT_SUPPORTED)?;

        // Check that max_key_size follows restrictions
        check_key_size(type_props, max_key_size)?;

        // 检查是否有属性使用 SECRET 操作索引
        let mut alloc_size = max_key_size;
        if type_props
            .type_attrs
            .iter()
            .any(|attr| attr.ops_index == ATTR_OPS_INDEX_SECRET as u16)
        {
            alloc_size = type_props.alloc_size as usize;
        }
        obj.attr.push(TeeCryptObj::new(obj_type, alloc_size)?);
        // o->attr = calloc(1, type_props->alloc_size);
        // if (!o->attr)
        // 	return TEE_ERROR_OUT_OF_MEMORY;
    }

    obj.info.objectType = obj_type;
    obj.info.maxObjectSize = max_key_size as u32;
    if obj.info.handleFlags & TEE_HANDLE_FLAG_PERSISTENT != 0 {
        let pobj = obj.pobj.as_mut().ok_or(TEE_ERROR_BAD_STATE)?;
        pobj.obj_info_usage
            .store(TEE_USAGE_DEFAULT, core::sync::atomic::Ordering::Relaxed);
    } else {
        obj.info.objectUsage = TEE_USAGE_DEFAULT;
    }

    Ok(0)
}

/// Allocate a new object
///
/// # Arguments
/// * `obj_type` - the type of the object
/// * `max_key_size` - the maximum key size of the object
/// # Returns
/// * `TeeResult` - the result of the operation
pub(crate) fn syscall_cryp_obj_alloc(
    obj_type: c_ulong,
    max_key_size: c_ulong,
    obj: *mut c_uint,
) -> TeeResult {
    let mut o = TeeObj::default();

    tee_obj_set_type(&mut o, obj_type as _, max_key_size as _)?;
    let obj_id: c_uint = tee_obj_add(o)? as c_uint;

    UserPtr::<c_uint>::from(obj)
        .write_vm(obj_id)
        .map_err(map_user_mem_error)?;

    Ok(())
}

/// Close an object
///
/// # Arguments
/// * `obj_id` - the object id
/// # Returns
/// * `TeeResult` - the result of the operation
pub(super) fn syscall_cryp_obj_close(obj_id: c_ulong) -> TeeResult {
    {
        let o = tee_obj_get(obj_id as TeeObjIdType)?;
        let o_guard = o.lock();

        // If it's busy it's used by an operation, a client should never have
        // this handle.
        if o_guard.busy {
            return Err(TEE_ERROR_ITEM_NOT_FOUND);
        }
    }

    tee_obj_close(obj_id as u32)
}

/// reset the object
///
/// # Arguments
/// * `obj_id` - the object id
/// # Returns
/// * `TeeResult` - the result of the operation
pub(super) fn syscall_cryp_obj_reset(obj_id: c_ulong) -> TeeResult {
    let o_arc = tee_obj_get(obj_id as TeeObjIdType)?;
    let mut o = o_arc.lock();

    if o.info.handleFlags & TEE_HANDLE_FLAG_PERSISTENT == 0 {
        let _ = tee_obj_attr_clear(&mut o);
        o.info.objectSize = 0;
        o.info.objectUsage = TEE_USAGE_DEFAULT;
    } else {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    // the object is no more initialized
    o.info.handleFlags &= !(TEE_HANDLE_FLAG_INITIALIZED);

    Ok(())
}

fn tee_svc_cryp_obj_find_type_attr_idx(attr_id: u32, type_props: &TeeCrypObjTypeProps) -> isize {
    for (n, attr) in type_props.type_attrs.iter().enumerate() {
        if attr_id == attr.attr_id {
            return n as isize;
        }
    }
    -1
}

pub(super) fn tee_svc_find_type_props(
    obj_type: TEE_ObjectType,
) -> Option<&'static TeeCrypObjTypeProps> {
    TEE_CRYP_OBJ_PROPS
        .iter()
        .find(|&props| props.obj_type == obj_type)
        .map(|v| v as _)
}

// Set an attribute on an object
pub(crate) fn set_attribute(o: &mut TeeObj, props: &TeeCrypObjTypeProps, attr: u32) {
    let idx = tee_svc_cryp_obj_find_type_attr_idx(attr, props);
    if idx < 0 {
        return;
    }
    o.have_attrs |= bit(idx as u32);
}

// Get an attribute on an object
fn get_attribute(o: &TeeObj, props: &TeeCrypObjTypeProps, attr: u32) -> u32 {
    let idx = tee_svc_cryp_obj_find_type_attr_idx(attr, props);
    if idx < 0 {
        return 0;
    }
    o.have_attrs & bit(idx as u32)
}

#[cfg(unittest)]
fn op_attr_secret_value_from_user(attr: &mut TeeCrypObjSecretWrapper, buffer: &[u8]) -> TeeResult {
    let size = buffer.len();

    if size > attr.secret().alloc_size as usize {
        return Err(TEE_ERROR_SHORT_BUFFER);
    }

    let data_slice = attr.data_mut();
    data_slice[..size].copy_from_slice(buffer);
    attr.secret_mut().key_size = size as u32;

    Ok(())
}

#[cfg(unittest)]
fn op_attr_secret_value_to_bytes(attr: &TeeCrypObjSecretWrapper) -> Box<[u8]> {
    attr.key().to_vec().into_boxed_slice()
}

fn op_u32_to_binary_helper(v: u32, data: &mut [u8], offs: &mut size_t) -> TeeResult {
    let field: u32;
    let next_offs: size_t = offs
        .checked_add(size_of::<u32>())
        .ok_or(TEE_ERROR_OVERFLOW)?;

    if data.len() >= next_offs {
        field = tee_u32_to_big_endian(v);
        data[*offs..*offs + size_of::<u32>()].copy_from_slice(&field.to_ne_bytes());
    }
    *offs = next_offs;

    Ok(())
}

fn op_u32_from_binary_helper(v: &mut u32, data: &[u8], offs: &mut size_t) -> TeeResult {
    if data.len() < *offs + size_of::<u32>() {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    let field_bytes = &data[*offs..*offs + size_of::<u32>()];
    let field: u32 = u32::from_be_bytes(
        field_bytes
            .try_into()
            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?,
    );
    *v = field;
    *offs += size_of::<u32>();

    Ok(())
}

/// 将密钥属性序列化到二进制缓冲区
///
/// data: 目标缓冲区,可以为空 []
#[cfg(unittest)]
fn op_attr_secret_value_to_binary(
    attr: &TeeCrypObjSecretWrapper,
    data: &mut [u8],
    offs: &mut size_t,
) -> TeeResult {
    let key = attr.secret();

    op_u32_to_binary_helper(key.key_size, data, offs)?;

    let next_offs: size_t = offs
        .checked_add(key.key_size as usize)
        .ok_or(TEE_ERROR_OVERFLOW)?;

    if data.len() >= next_offs {
        data[*offs..*offs + key.key_size as usize]
            .copy_from_slice(&attr.data()[..key.key_size as usize]);
    }
    *offs = next_offs;

    Ok(())
}

#[cfg(unittest)]
fn op_attr_secret_value_from_binary(
    attr: &mut TeeCrypObjSecretWrapper,
    data: &[u8],
    offs: &mut size_t,
) -> TeeResult {
    let key = attr.secret();
    let mut s: u32 = 0;

    op_u32_from_binary_helper(&mut s, data, offs)?;

    if offs
        .checked_add(s as usize)
        .ok_or(TEE_ERROR_BAD_PARAMETERS)?
        > data.len()
    {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    // 数据大小必须适合分配的缓冲区
    if s > key.alloc_size {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    attr.secret_mut().key_size = s;

    let data_slice = attr.data_mut();
    data_slice[..s as usize].copy_from_slice(&data[*offs..*offs + s as usize]);

    *offs += s as usize;

    Ok(())
}

#[cfg(unittest)]
fn op_attr_value_to_user(attr: &[u8], buffer: &mut [u8], size_ref: &mut u64) -> TeeResult {
    if attr.len() < size_of::<u32>() {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    let req_size: u64 = (size_of::<u32>() * 2) as u64;
    let s = *size_ref;

    if s < req_size || buffer.is_empty() {
        return Err(TEE_ERROR_SHORT_BUFFER);
    }

    if buffer.len() < req_size as usize {
        return Err(TEE_ERROR_SHORT_BUFFER);
    }

    buffer[..size_of::<u32>()].copy_from_slice(&attr[..size_of::<u32>()]);
    buffer[size_of::<u32>()..req_size as usize].fill(0);
    *size_ref = req_size;

    Ok(())
}

#[cfg(unittest)]
fn op_attr_value_to_binary(attr: &[u8], data: &mut [u8], offs: &mut size_t) -> TeeResult {
    let value = u32::from_ne_bytes(
        attr.get(..size_of::<u32>())
            .ok_or(TEE_ERROR_BAD_PARAMETERS)?
            .try_into()
            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?,
    );
    op_u32_to_binary_helper(value, data, offs)
}

#[cfg(unittest)]
fn op_attr_value_from_binary(attr: &mut [u8], data: &[u8], offs: &mut size_t) -> TeeResult {
    if attr.len() < size_of::<u32>() {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    let mut value = 0;
    op_u32_from_binary_helper(&mut value, data, offs)?;
    attr[..size_of::<u32>()].copy_from_slice(&value.to_ne_bytes());
    Ok(())
}

/// convert the attributes of the object to binary data
/// the order is defined by TEE_CRYP_OBJ_PROPS table
///
/// # Arguments
/// * `o` - the object
/// * `data` - the data to store the binary data
/// * `data_len` - the length of the data
///
/// # Returns
/// * `TeeResult` - the result of the operation
pub(super) fn tee_obj_attr_to_binary(
    o: &mut TeeObj,
    data: &mut [u8],
    data_len: &mut size_t,
) -> TeeResult {
    if o.info.objectType == TEE_TYPE_DATA {
        *data_len = 0;
        return Ok(()); /* pure data object */
    }
    if o.attr.is_empty() {
        return Err(TEE_ERROR_BAD_STATE);
    }

    let tp = tee_svc_find_type_props(o.info.objectType).ok_or(TEE_ERROR_BAD_STATE)?;

    let mut offs: size_t = 0;
    for ta in tp.type_attrs.iter() {
        let attr = o.attr[0].get_attr_by_id(ta.attr_id as _)?;
        attr.to_binary(data, &mut offs)?;
    }

    *data_len = offs;

    if !data.is_empty() && offs > data.len() {
        return Err(TEE_ERROR_SHORT_BUFFER);
    }

    Ok(())
}

/// construct the attributes of the object from the binary data
/// the order is defined by TEE_CRYP_OBJ_PROPS table
///
/// # Arguments
/// * `o` - the object
/// * `data` - the data to convert the attributes
/// * `data_len` - the length of the data
///
/// # Returns
/// * `TeeResult` - the result of the operation
pub(super) fn tee_obj_attr_from_binary(o: &mut TeeObj, data: &[u8]) -> TeeResult {
    if o.info.objectType == TEE_TYPE_DATA {
        return Ok(()); /* pure data object */
    }
    if o.attr.is_empty() {
        return Err(TEE_ERROR_BAD_STATE);
    }

    let tp = tee_svc_find_type_props(o.info.objectType).ok_or(TEE_ERROR_BAD_STATE)?;

    let mut offs: size_t = 0;
    for ta in tp.type_attrs.iter() {
        let mut attr = o.attr[0].get_attr_by_id(ta.attr_id as _)?;
        attr.update_from_binary(data, &mut offs)?;
    }

    if offs != data.len() {
        return Err(TEE_ERROR_CORRUPT_OBJECT);
    }

    Ok(())
}

pub(super) fn tee_obj_attr_copy_from(dst: &mut TeeObj, src: &mut TeeObj) -> TeeResult {
    let have_atts: u32;
    if dst.info.objectType == TEE_TYPE_DATA {
        return Ok(());
    }
    if dst.attr.is_empty() {
        return Err(TEE_ERROR_BAD_STATE);
    }

    let tp = tee_svc_find_type_props(dst.info.objectType).ok_or(TEE_ERROR_BAD_STATE)?;

    if dst.info.objectType == src.info.objectType {
        have_atts = src.have_attrs;
        for ta in tp.type_attrs.iter() {
            let attr_id = ta.attr_id;
            let mut attr_ref = dst.attr[0].get_attr_by_id(attr_id as c_ulong)?;
            let attr_src_ref = src.attr[0].get_attr_by_id(attr_id as c_ulong)?;
            attr_ref.update_from_crypto_attr_ref(&attr_src_ref)?;
        }
    } else {
        if dst.info.objectType == TEE_TYPE_RSA_PUBLIC_KEY {
            if src.info.objectType != TEE_TYPE_RSA_KEYPAIR {
                return Err(TEE_ERROR_BAD_PARAMETERS);
            }
        } else if dst.info.objectType == TEE_TYPE_DSA_PUBLIC_KEY {
            if src.info.objectType != TEE_TYPE_DSA_KEYPAIR {
                return Err(TEE_ERROR_BAD_PARAMETERS);
            }
        } else if dst.info.objectType == TEE_TYPE_ECDSA_PUBLIC_KEY {
            if src.info.objectType != TEE_TYPE_ECDSA_KEYPAIR {
                return Err(TEE_ERROR_BAD_PARAMETERS);
            }
        } else if dst.info.objectType == TEE_TYPE_ECDH_PUBLIC_KEY {
            if src.info.objectType != TEE_TYPE_ECDH_KEYPAIR {
                return Err(TEE_ERROR_BAD_PARAMETERS);
            }
        } else if dst.info.objectType == TEE_TYPE_SM2_DSA_PUBLIC_KEY {
            if src.info.objectType != TEE_TYPE_SM2_DSA_KEYPAIR {
                return Err(TEE_ERROR_BAD_PARAMETERS);
            }
        } else if dst.info.objectType == TEE_TYPE_SM2_PKE_PUBLIC_KEY {
            if src.info.objectType != TEE_TYPE_SM2_PKE_KEYPAIR {
                return Err(TEE_ERROR_BAD_PARAMETERS);
            }
        } else if dst.info.objectType == TEE_TYPE_SM2_KEP_PUBLIC_KEY {
            if src.info.objectType != TEE_TYPE_SM2_KEP_KEYPAIR {
                return Err(TEE_ERROR_BAD_PARAMETERS);
            }
        } else if dst.info.objectType == TEE_TYPE_ED25519_PUBLIC_KEY {
            if src.info.objectType != TEE_TYPE_ED25519_KEYPAIR {
                return Err(TEE_ERROR_BAD_PARAMETERS);
            }
        } else if dst.info.objectType == TEE_TYPE_X25519_PUBLIC_KEY {
            if src.info.objectType != TEE_TYPE_X25519_KEYPAIR {
                return Err(TEE_ERROR_BAD_PARAMETERS);
            }
        } else if dst.info.objectType == TEE_TYPE_X448_PUBLIC_KEY {
            if src.info.objectType != TEE_TYPE_X448_KEYPAIR {
                return Err(TEE_ERROR_BAD_PARAMETERS);
            }
        } else {
            return Err(TEE_ERROR_BAD_PARAMETERS);
        }

        let _tp_src = tee_svc_find_type_props(src.info.objectType).ok_or(TEE_ERROR_BAD_STATE)?;
        have_atts = bit32(tp.num_type_attrs as u32) - 1;
        for ta in tp.type_attrs.iter() {
            let attr_id = ta.attr_id;
            let mut attr_ref = dst.attr[0].get_attr_by_id(attr_id as c_ulong)?;
            let attr_src_ref = src.attr[0].get_attr_by_id(attr_id as c_ulong)?;
            attr_ref.update_from_crypto_attr_ref(&attr_src_ref)?;
        }
    }

    dst.have_attrs = have_atts;
    Ok(())
}

pub(super) fn is_gp_legacy_des_key_size(obj_type: TEE_ObjectType, sz: size_t) -> bool {
    CFG_COMPAT_GP10_DES
        && ((obj_type == TEE_TYPE_DES && sz == 56)
            || (obj_type == TEE_TYPE_DES3 && (sz == 112 || sz == 168)))
}

fn check_key_size(props: &TeeCrypObjTypeProps, key_size: size_t) -> TeeResult {
    let mut sz = key_size;

    // In GP Internal API Specification 1.0 the partity bits aren't
    // counted when telling the size of the key in bits so add them
    // here if missing.
    if is_gp_legacy_des_key_size(props.obj_type, sz) {
        sz += sz / 7;
    }

    if !sz.is_multiple_of(props.quanta as usize) {
        return Err(TEE_ERROR_NOT_SUPPORTED);
    }

    if sz < props.min_size as usize {
        return Err(TEE_ERROR_NOT_SUPPORTED);
    }

    if sz > props.max_size as usize {
        return Err(TEE_ERROR_NOT_SUPPORTED);
    }

    Ok(())
}

/// Get the information of the object
///
/// # Arguments
/// * `obj_id` - the object id
/// * `info` - the information to store the object information
/// # Returns
/// * `TeeResult` - the result of the operation
pub(super) fn syscall_cryp_obj_get_info(obj_id: c_ulong, info: *mut utee_object_info) -> TeeResult {
    tee_debug!(
        "syscall_cryp_obj_get_info: obj_id: {:#010X?}, info: {:#010X?}",
        obj_id,
        info
    );
    let mut o_info: utee_object_info = utee_object_info::default();
    let o_arc = tee_obj_get(obj_id as TeeObjIdType)?;
    let o = o_arc.lock();

    o_info.obj_type = o.info.objectType;
    o_info.obj_size = o.info.objectSize;
    o_info.max_obj_size = o.info.maxObjectSize;
    if o.info.handleFlags & TEE_HANDLE_FLAG_PERSISTENT != 0 {
        let pobj = o.pobj.as_ref().ok_or(TEE_ERROR_BAD_STATE)?;

        with_pobj_usage_lock(
            pobj.flags.load(core::sync::atomic::Ordering::Relaxed),
            || {
                o_info.obj_usage = pobj
                    .obj_info_usage
                    .load(core::sync::atomic::Ordering::Relaxed);
            },
        );
    } else {
        o_info.obj_usage = o.info.objectUsage;
    }
    o_info.data_size = o.info.dataSize as _;
    o_info.data_pos = o.info.dataPosition as _;
    o_info.handle_flags = o.info.handleFlags as _;

    UserPtr::<utee_object_info>::from(info)
        .write_vm(o_info)
        .map_err(map_user_mem_error)?;
    Ok(())
}

/// restrict the usage of the object
///
/// # Arguments
/// * `obj_id` - the object id
/// * `usage` - the usage to restrict
/// # Returns
/// * `TeeResult` - the result of the operation
pub(super) fn syscall_cryp_obj_restrict_usage(obj_id: c_ulong, usage: c_ulong) -> TeeResult {
    let _o_info = utee_object_info::default();

    let o_arc = tee_obj_get(obj_id as TeeObjIdType)?;
    let mut o = o_arc.lock();
    if o.info.handleFlags & TEE_HANDLE_FLAG_PERSISTENT != 0 {
        // get pobj arc and flags in the closure, avoid multiple borrows in the closure
        let pobj_arc = o.pobj.as_ref().ok_or(TEE_ERROR_BAD_STATE)?.clone();
        let pobj_flags = pobj_arc.flags.load(core::sync::atomic::Ordering::Relaxed);

        let mut new_usage: u32 = 0;
        let write_res = with_pobj_usage_lock(pobj_flags, || -> TeeResult {
            new_usage = pobj_arc
                .obj_info_usage
                .load(core::sync::atomic::Ordering::Relaxed)
                & usage as u32;

            // call write_usage（need &mut o，now can borrow safely，because pobj's lock is released）
            tee_svc_storage_write_usage(&mut o, new_usage)?;

            // get write lock to update obj_info_usage
            pobj_arc
                .obj_info_usage
                .store(new_usage, core::sync::atomic::Ordering::Relaxed);
            Ok(())
        });

        write_res?;
    } else {
        o.info.objectUsage &= usage as u32;
    }

    Ok(())
}

/// Get the attribute of the object
///
/// # Arguments
/// * `obj_id` - the object id
/// * `attr_id` - the attribute id
/// * `buffer` - the buffer to store the attribute
/// * `size` - the size of the attribute
/// # Returns
/// * `TeeResult` - the result of the operation
pub(super) fn syscall_cryp_obj_get_attr(
    obj_id: c_ulong,
    attr_id: c_ulong,
    buffer: *mut c_void,
    size: *mut c_ulong,
) -> TeeResult {
    tee_debug!(
        "syscall_cryp_obj_get_attr: obj_id: {:x?}, attr_id: {:x?}, buffer: {:x?}, size: {:x?}",
        obj_id,
        attr_id,
        buffer,
        size
    );
    let mut obj_usage = 0;
    let o_arc = tee_obj_get(obj_id as TeeObjIdType)?;
    let mut o = o_arc.lock();
    if size.is_null() {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    let user_size = UserPtr::<c_ulong>::from(size);
    let buffer_len = user_size.read_vm().map_err(map_user_mem_error)? as usize;

    if o.info.handleFlags & TEE_HANDLE_FLAG_INITIALIZED == 0 {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    tee_debug!(
        "attr_id: {:x?}, handleFlags: {:x?}",
        attr_id,
        o.info.handleFlags
    );
    if attr_id & TEE_ATTR_FLAG_PUBLIC as c_ulong == 0 {
        if o.info.handleFlags & TEE_HANDLE_FLAG_PERSISTENT != 0 {
            let pobj = o.pobj.as_ref().ok_or(TEE_ERROR_BAD_STATE)?;
            let pobj_flags = pobj.flags.load(core::sync::atomic::Ordering::Relaxed);
            with_pobj_usage_lock(pobj_flags, || {
                obj_usage = pobj
                    .obj_info_usage
                    .load(core::sync::atomic::Ordering::Relaxed);
            });
        } else {
            obj_usage = o.info.objectUsage;
        }
        if obj_usage & TEE_USAGE_EXTRACTABLE == 0 {
            return Err(TEE_ERROR_BAD_PARAMETERS);
        }
    }

    let type_props = tee_svc_find_type_props(o.info.objectType).ok_or(TEE_ERROR_BAD_STATE)?;

    let idx = tee_svc_cryp_obj_find_type_attr_idx(attr_id as u32, type_props);
    tee_debug!("idx: {}, have_attrs: {:x?}", idx, o.have_attrs);
    if idx < 0 || (o.have_attrs & (1 << idx)) == 0 {
        return Err(TEE_ERROR_ITEM_NOT_FOUND);
    }

    // let ops = type_props.type_attrs[idx].ops_index;
    // let attr = (o.attr[idx] as *const u8) as *const u8;
    // return ops.export_to_bytes(attr, sess, buffer, size);
    if !o.attr.is_empty() {
        let attr_ref = o.attr[0].get_attr_by_id(attr_id)?;
        let exported = attr_ref.export_to_bytes()?;
        user_size
            .write_vm(exported.len() as c_ulong)
            .map_err(map_user_mem_error)?;
        if buffer_len < exported.len() || (buffer.is_null() && !exported.is_empty()) {
            return Err(TEE_ERROR_SHORT_BUFFER);
        }
        UserPtr::<u8>::from(buffer.cast::<u8>())
            .write_vm_slice(&exported)
            .map_err(map_user_mem_error)?;
    }

    Ok(())
}

pub(super) fn tee_obj_attr_clear(o: &mut TeeObj) -> TeeResult {
    let tp = tee_svc_find_type_props(o.info.objectType).ok_or(TEE_ERROR_BAD_STATE)?;
    if o.attr.is_empty() {
        return Ok(());
    }

    for ta in tp.type_attrs.iter() {
        let mut attr = o.attr[0].get_attr_by_id(ta.attr_id as _)?;
        attr.clear();
    }

    Ok(())
}

/// Capture an output memref attribute (kernel writes back after the operation).
///
/// Returns `Ok(None)` when `id` is a normal input memref and should follow the
/// usual copy-in path in [`copy_in_attrs`].
fn copy_in_memref_out_attr(
    id: u32,
    user_addr: usize,
    len: usize,
) -> TeeResult<Option<KernelAttribute>> {
    if id != TEE_ATTR_SM2_KEP_CONFIRMATION_OUT {
        return Ok(None);
    }
    let flags = TEE_MEMORY_ACCESS_READ | TEE_MEMORY_ACCESS_WRITE | TEE_MEMORY_ACCESS_ANY_OWNER;
    vm_check_access_rights(flags, user_addr, len)?;
    Ok(Some(KernelAttribute::MemrefOut { id, user_addr, len }))
}

/// Largest attribute array a TA may pass into a crypto syscall.
///
/// RSA keypairs use 8 attributes and SM2 KEP derive uses 9.
/// `UserConstPtr::load_vm_vec` allocates before copying user memory, and a
/// capacity above `isize::MAX` bytes panics.
pub(super) const TEE_CRYP_MAX_USER_ATTRS: usize = 32;

/// Reject an attribute count that would make the following copy unbounded.
pub(super) fn check_user_attr_count(count: usize) -> TeeResult {
    if count > TEE_CRYP_MAX_USER_ATTRS {
        Err(TEE_ERROR_BAD_PARAMETERS)
    } else {
        Ok(())
    }
}

/// Copy `count` user attributes after enforcing [`TEE_CRYP_MAX_USER_ATTRS`].
pub(super) fn load_user_attrs(
    usr_params: *const utee_attribute,
    count: usize,
) -> TeeResult<Vec<utee_attribute>> {
    check_user_attr_count(count)?;
    if count == 0 {
        return Ok(Vec::new());
    }
    UserConstPtr::<utee_attribute>::from(usr_params)
        .load_vm_vec(count)
        .map_err(map_user_mem_error)
}

/// Copy user ABI attributes into kernel-owned semantic attributes.
pub(crate) fn copy_in_attrs(
    _uctx: &mut user_ta_ctx,
    usr_attrs: &[utee_attribute],
) -> TeeResult<Box<[KernelAttribute]>> {
    let mut attrs = Vec::with_capacity(usr_attrs.len());
    for usr_attr in usr_attrs {
        let id = usr_attr.attribute_id;
        if id & TEE_ATTR_FLAG_VALUE != 0 {
            attrs.push(KernelAttribute::Value {
                id,
                a: usr_attr.a as u32,
                b: usr_attr.b as u32,
            });
        } else {
            let mut buf = usr_attr.a;
            let len = usr_attr.b;
            buf = memtag_strip_tag_vaddr(buf as *const c_void) as u64;
            if let Some(attr) = copy_in_memref_out_attr(id, buf as usize, len as usize)? {
                attrs.push(attr);
                continue;
            }
            let flags = TEE_MEMORY_ACCESS_READ | TEE_MEMORY_ACCESS_ANY_OWNER;
            vm_check_access_rights(flags, buf as usize, len as usize)?;
            let data = if len == 0 {
                Box::new([])
            } else {
                UserConstPtr::<u8>::from(buf as usize)
                    .load_vm_vec(len as usize)
                    .map_err(map_user_mem_error)?
                    .into_boxed_slice()
            };
            attrs.push(KernelAttribute::Memref { id, data });
        }
    }
    let attrs = attrs.into_boxed_slice();
    tee_debug!(
        "copy_in_attrs: usr_attrs: {:#?}, attrs: {:#?}",
        usr_attrs,
        attrs
    );
    Ok(attrs)
}

/// Write `data` into a user memref previously captured as [`KernelAttribute::MemrefOut`].
pub(crate) fn write_user_memref_out(user_addr: usize, buf_len: usize, data: &[u8]) -> TeeResult {
    if data.len() > buf_len {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    if data.is_empty() {
        return Ok(());
    }
    let flags = TEE_MEMORY_ACCESS_WRITE | TEE_MEMORY_ACCESS_ANY_OWNER;
    vm_check_access_rights(flags, user_addr, data.len())?;
    UserPtr::<u8>::from(user_addr)
        .write_vm_slice(data)
        .map_err(map_user_mem_error)
}

/// GP: `enum attr_usage`
enum AttrUsage {
    /// GP: `ATTR_USAGE_POPULATE`
    Populate    = 0,
    /// GP: `ATTR_USAGE_GENERATE_KEY`
    GenerateKey = 1,
}

fn tee_svc_cryp_check_attr(
    usage: AttrUsage,
    type_props: &TeeCrypObjTypeProps,
    attrs: &[KernelAttribute],
) -> TeeResult {
    let required_flag;
    let opt_flag;
    let all_opt_needed;
    let mut req_attrs: u32 = 0;
    let mut opt_grp_attrs: u32 = 0;
    let mut attrs_found: u32 = 0;
    let mut bit: u32;
    let mut flags: u32;
    let mut idx: isize;

    match usage {
        AttrUsage::Populate => {
            required_flag = TEE_TYPE_ATTR_REQUIRED;
            opt_flag = TEE_TYPE_ATTR_OPTIONAL_GROUP;
            all_opt_needed = true;
        }
        AttrUsage::GenerateKey => {
            required_flag = TEE_TYPE_ATTR_GEN_KEY_REQ;
            opt_flag = TEE_TYPE_ATTR_GEN_KEY_OPT;
            all_opt_needed = false;
        }
    }

    // First find out which attributes are required and which belong to
    // the optional group
    for n in 0..type_props.num_type_attrs as usize {
        bit = 1 << n;
        flags = type_props.type_attrs[n].flags as u32;

        if flags & required_flag != 0 {
            req_attrs |= bit;
        } else if flags & opt_flag != 0 {
            opt_grp_attrs |= bit;
        }
    }

    // Verify that all required attributes are in place and
    // that the same attribute isn't repeated.
    for attr in attrs.iter() {
        idx = tee_svc_cryp_obj_find_type_attr_idx(attr.id(), type_props);

        // attribute not defined in current object type
        if idx < 0 {
            return Err(TEE_ERROR_ITEM_NOT_FOUND);
        }

        bit = 1 << idx;

        // attribute not repeated
        if (attrs_found & bit) != 0 {
            return Err(TEE_ERROR_ITEM_NOT_FOUND);
        }

        // Attribute not defined in current object type for this
        // usage.
        if (bit & (req_attrs | opt_grp_attrs)) == 0 {
            return Err(TEE_ERROR_ITEM_NOT_FOUND);
        }

        attrs_found |= bit;
    }

    // Required attribute missing
    if (attrs_found & req_attrs) != req_attrs {
        return Err(TEE_ERROR_ITEM_NOT_FOUND);
    }

    // If the flag says that "if one of the optional attributes are included
    // all of them has to be included" this must be checked.
    if all_opt_needed
        && (attrs_found & opt_grp_attrs) != 0
        && (attrs_found & opt_grp_attrs) != opt_grp_attrs
    {
        return Err(TEE_ERROR_ITEM_NOT_FOUND);
    }

    Ok(())
}

fn get_ec_key_size(curve: u32) -> TeeResult<usize> {
    let key_size: usize = match curve {
        TEE_ECC_CURVE_NIST_P192 => 192,
        TEE_ECC_CURVE_NIST_P224 => 224,
        TEE_ECC_CURVE_NIST_P256 => 256,
        TEE_ECC_CURVE_NIST_P384 => 384,
        TEE_ECC_CURVE_NIST_P521 => 521,
        TEE_ECC_CURVE_SM2 | TEE_ECC_CURVE_25519 => 256,
        _ => {
            return Err(TEE_ERROR_NOT_SUPPORTED);
        }
    };
    Ok(key_size)
}

fn tee_svc_cryp_obj_populate_type(
    obj: &mut TeeObj,
    type_props: &TeeCrypObjTypeProps,
    attrs: &[KernelAttribute],
) -> TeeResult {
    let mut have_attrs: u32 = 0;
    let mut obj_size: usize = 0;
    let mut idx: isize;

    if obj.attr.is_empty() {
        return Err(TEE_ERROR_BAD_STATE);
    }

    for attr in attrs {
        // find attribute index in type properties
        tee_debug!(
            "tee_svc_cryp_obj_populate_type, find attribute index: attr_id: {:06X?}, type_props: \
             {:#?}",
            attr.id(),
            type_props
        );
        idx = tee_svc_cryp_obj_find_type_attr_idx(attr.id(), type_props);
        tee_debug!(
            "tee_svc_cryp_obj_populate_type, attribute index: {:#X?}",
            idx
        );
        // attribute not defined in current object type
        if idx < 0 {
            return Err(TEE_ERROR_ITEM_NOT_FOUND);
        }
        have_attrs |= bit32(idx as u32);

        let mut attr_ref = obj.attr[0].get_attr_by_id(attr.id() as c_ulong)?;
        match attr {
            KernelAttribute::Value { a, b, .. } => match &mut attr_ref {
                CryptoAttrRef::U32(v) => {
                    **v = *a;
                }
                _ => {
                    let value = [a.to_ne_bytes(), b.to_ne_bytes()].concat();
                    attr_ref.import_from_bytes(&value)?;
                }
            },
            KernelAttribute::Memref { data, .. } => {
                attr_ref.import_from_bytes(data)?;
            }
            KernelAttribute::MemrefOut { .. } => return Err(TEE_ERROR_BAD_PARAMETERS),
        }

        // The attribute that gives the size of the object is
        // flagged with TEE_TYPE_ATTR_SIZE_INDICATOR.
        if type_props.type_attrs[idx as usize].flags & TEE_TYPE_ATTR_SIZE_INDICATOR as u16 != 0 {
            // There should be only one
            if obj_size != 0 {
                return Err(TEE_ERROR_BAD_STATE);
            }

            // For ECDSA/ECDH we need to translate curve into
            // object size
            if attr.id() == TEE_ATTR_ECC_CURVE {
                // get ECC curve size
                obj_size = get_ec_key_size(attr.value()?.0)?;
            } else {
                let obj_type: TEE_ObjectType = obj.info.objectType;
                let sz: usize = obj.info.maxObjectSize as usize;

                obj_size = attr.memref_data()?.len() * 8;
                if is_gp_legacy_des_key_size(obj_type, sz) {
                    obj_size -= obj_size / 8;
                }
            }
            if obj_size > obj.info.maxObjectSize as usize {
                return Err(TEE_ERROR_BAD_STATE);
            }
            check_key_size(type_props, obj_size)?;
        }
        // Bignum attributes limited by the number of bits in
        // o->info.objectSize are flagged with
        // TEE_TYPE_ATTR_BIGNUM_MAXBITS.
        if type_props.type_attrs[idx as usize].flags & TEE_TYPE_ATTR_BIGNUM_MAXBITS as u16 != 0
            && crypto_bignum_num_bits(attr_ref.as_bignum().ok_or(TEE_ERROR_BAD_STATE)?)?
                > obj.info.maxObjectSize as usize
        {
            return Err(TEE_ERROR_BAD_STATE);
        }

        obj.have_attrs |= have_attrs;
        obj.info.objectSize = obj_size as u32;
        // In GP Internal API Specification 1.0 the partity bits aren't
        // counted when telling the size of the key in bits so remove the
        // parity bits here.
        if is_gp_legacy_des_key_size(obj.info.objectType, obj.info.maxObjectSize as usize) {
            obj.info.objectSize -= obj.info.objectSize / 8;
        }
    }

    Ok(())
}

/// Populate a transient object
///
/// # Arguments
/// * `obj_id` - the object id
/// * `user_attrs` - the user attributes
/// * `attr_count` - the number of attributes
/// # Returns
/// * `TeeResult` - the result of the operation
pub(super) fn syscall_cryp_obj_populate(
    obj_id: c_ulong,
    user_attrs: *mut utee_attribute,
    attr_count: c_ulong,
) -> TeeResult {
    let usr_attrs = load_user_attrs(user_attrs.cast_const(), attr_count as usize)?;

    let o_arc = tee_obj_get(obj_id as TeeObjIdType)?;
    let mut o = o_arc.lock();

    // Must be a transient object
    if o.info.handleFlags & TEE_HANDLE_FLAG_PERSISTENT != 0 {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    // Must not be initialized already
    if o.info.handleFlags & TEE_HANDLE_FLAG_INITIALIZED != 0 {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    let type_props = tee_svc_find_type_props(o.info.objectType).ok_or(TEE_ERROR_NOT_IMPLEMENTED)?;

    let attrs = copy_in_attrs(&mut user_ta_ctx::default(), &usr_attrs)?;

    tee_svc_cryp_check_attr(AttrUsage::Populate, type_props, &attrs)?;

    tee_svc_cryp_obj_populate_type(&mut o, type_props, &attrs)?;

    o.info.handleFlags |= TEE_HANDLE_FLAG_INITIALIZED;

    Ok(())
}

/// Copy an object from source to destination
///
/// # Arguments
/// * `dst` - the destination object id
/// * `src` - the source object id
/// # Returns
/// * `TeeResult` - the result of the operation
pub(super) fn syscall_cryp_obj_copy(dst: c_ulong, src: c_ulong) -> TeeResult {
    let dst_o_arc = tee_obj_get(dst as TeeObjIdType)?;
    let mut dst_o = dst_o_arc.lock();
    let src_o_arc = tee_obj_get(src as TeeObjIdType)?;
    let mut src_o = src_o_arc.lock();

    if src_o.info.handleFlags & TEE_HANDLE_FLAG_INITIALIZED == 0 {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    if dst_o.info.handleFlags & TEE_HANDLE_FLAG_PERSISTENT != 0 {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    if dst_o.info.handleFlags & TEE_HANDLE_FLAG_INITIALIZED != 0 {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    tee_obj_attr_copy_from(&mut dst_o, &mut src_o)?;
    dst_o.info.handleFlags |= TEE_HANDLE_FLAG_INITIALIZED;
    dst_o.info.objectSize = src_o.info.objectSize;
    if src_o.info.handleFlags & TEE_HANDLE_FLAG_PERSISTENT != 0 {
        let pobj = src_o.pobj.as_ref().ok_or(TEE_ERROR_BAD_STATE)?;
        let pobj_flags = pobj.flags.load(core::sync::atomic::Ordering::Relaxed);
        with_pobj_usage_lock(pobj_flags, || {
            dst_o.info.objectUsage = pobj
                .obj_info_usage
                .load(core::sync::atomic::Ordering::Relaxed);
        });
    } else {
        dst_o.info.objectUsage = src_o.info.objectUsage;
    }
    Ok(())
}

fn check_pub_rsa_key(e: &BigNum) -> TeeResult {
    let n = crypto_bignum_num_bytes(e)?;
    let mut bin_key = [0u8; 256 / 8];

    // NIST SP800-56B requires public RSA key to be an odd integer in
    // the range 65537 <= e < 2^256. AOSP requires implementations to
    // support public exponents >= 3, which can be allowed by enabling
    // CFG_RSA_PUB_EXPONENT_3.
    if n > bin_key.len() || n < 1 {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    crypto_bignum_bn2bin(e, &mut bin_key)?;

    if (bin_key[n - 1] & 1) == 0 {
        // key must be odd
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    if n <= 3 {
        let mut min_key: u32 = 65537;
        let mut key: u32 = 0;

        if CFG_RSA_PUB_EXPONENT_3 {
            min_key = 3;
        }

        for &byte in bin_key.iter().take(n) {
            key <<= 8;
            key |= byte as u32;
        }

        if key < min_key {
            return Err(TEE_ERROR_BAD_PARAMETERS);
        }
    }

    Ok(())
}

pub(super) fn tee_svc_obj_generate_key_rsa(
    o: &mut TeeObj,
    type_props: &TeeCrypObjTypeProps,
    key_size: u32,
    params: &[KernelAttribute],
    _object_type: u32,
) -> TeeResult {
    tee_debug!("tee_svc_obj_generate_key_rsa: params: {:#?}", params);
    tee_svc_cryp_obj_populate_type(o, type_props, params)?;

    if o.attr.is_empty() {
        return Err(TEE_ERROR_BAD_STATE);
    }
    let pub_exp = get_attribute(o, type_props, TEE_ATTR_RSA_PUBLIC_EXPONENT);

    let rsa_key = match &mut o.attr[0] {
        TeeCryptObj::RsaKeypair(key) => key,
        _ => return Err(TEE_ERROR_BAD_STATE),
    };

    tee_debug!("tee_svc_obj_generate_key_rsa: pub_exp: {:X?}", pub_exp);
    if pub_exp != 0 {
        check_pub_rsa_key(&rsa_key.e)?;
    } else {
        // set default public exponent to 65537 (big endian)
        let e_bytes = 65537u32.to_be_bytes();
        crypto_bignum_bin2bn(&e_bytes, &mut rsa_key.e)?;
    }

    crypto_acipher_gen_rsa_key(rsa_key, key_size as usize)?;

    // Set bits for all known attributes for this object type
    o.have_attrs = (1 << type_props.num_type_attrs) - 1;

    Ok(())
}

pub(super) fn tee_svc_obj_generate_key_ecc(
    o: &mut TeeObj,
    type_props: &TeeCrypObjTypeProps,
    key_size: u32,
    params: &[KernelAttribute],
    object_type: u32,
) -> TeeResult {
    tee_svc_cryp_obj_populate_type(o, type_props, params)?;

    if o.attr.is_empty() {
        return Err(TEE_ERROR_BAD_STATE);
    }

    let tee_ecc_key = match &mut o.attr[0] {
        TeeCryptObj::EccKeypair(key) => key,
        _ => return Err(TEE_ERROR_BAD_STATE),
    };

    crypto_acipher_gen_ecc_key(tee_ecc_key, key_size as usize, object_type)?;

    set_attribute(o, type_props, TEE_ATTR_ECC_PRIVATE_VALUE);
    set_attribute(o, type_props, TEE_ATTR_ECC_PUBLIC_VALUE_X);
    set_attribute(o, type_props, TEE_ATTR_ECC_PUBLIC_VALUE_Y);
    set_attribute(o, type_props, TEE_ATTR_ECC_CURVE);

    Ok(())
}

pub(super) fn tee_svc_obj_generate_key_ed25519(
    o: &mut TeeObj,
    type_props: &TeeCrypObjTypeProps,
    key_size: u32,
    params: &[KernelAttribute],
) -> TeeResult {
    tee_svc_cryp_obj_populate_type(o, type_props, params)?;

    if o.attr.is_empty() {
        return Err(TEE_ERROR_BAD_STATE);
    }

    let ed25519_key = match &mut o.attr[0] {
        TeeCryptObj::Ed25519Keypair(key) => key,
        _ => return Err(TEE_ERROR_BAD_STATE),
    };

    crypto_acipher_gen_ed25519_key(ed25519_key, key_size as usize)?;

    set_attribute(o, type_props, TEE_ATTR_ED25519_PRIVATE_VALUE);
    set_attribute(o, type_props, TEE_ATTR_ED25519_PUBLIC_VALUE);
    Ok(())
}

pub(super) fn tee_svc_obj_generate_key_x25519(
    o: &mut TeeObj,
    type_props: &TeeCrypObjTypeProps,
    key_size: u32,
    params: &[KernelAttribute],
) -> TeeResult {
    tee_svc_cryp_obj_populate_type(o, type_props, params)?;

    if o.attr.is_empty() {
        return Err(TEE_ERROR_BAD_STATE);
    }

    let x25519_key = match &mut o.attr[0] {
        TeeCryptObj::X25519Keypair(key) => key,
        _ => return Err(TEE_ERROR_BAD_STATE),
    };

    crypto_acipher_gen_x25519_key(x25519_key, key_size as usize)?;

    set_attribute(o, type_props, TEE_ATTR_X25519_PRIVATE_VALUE);
    set_attribute(o, type_props, TEE_ATTR_X25519_PUBLIC_VALUE);
    Ok(())
}

pub(super) fn tee_svc_obj_generate_key_x448(
    o: &mut TeeObj,
    type_props: &TeeCrypObjTypeProps,
    key_size: u32,
    params: &[KernelAttribute],
) -> TeeResult {
    tee_svc_cryp_obj_populate_type(o, type_props, params)?;

    if o.attr.is_empty() {
        return Err(TEE_ERROR_BAD_STATE);
    }

    let x448_key = match &mut o.attr[0] {
        TeeCryptObj::X448Keypair(key) => key,
        _ => return Err(TEE_ERROR_BAD_STATE),
    };

    crypto_acipher_gen_x448_key(x448_key, key_size as usize)?;

    set_attribute(o, type_props, TEE_ATTR_X448_PRIVATE_VALUE);
    set_attribute(o, type_props, TEE_ATTR_X448_PUBLIC_VALUE);
    Ok(())
}

/// Generates a cryptographic key for the specified secure object.
/// The attributes of key is stored in the object attr(TeeObj.attr).
///
/// # Parameters
/// - `obj`: Handle of the object (object ID).
/// - `key_size`: The length of the key to be generated (in bits).
/// - `usr_params`: Pointer to an array of user-supplied key attributes.
/// - `param_count`: The number of attributes in the array.
///
/// # Returns
/// Returns a `TeeResult` indicating success or failure.
#[expect(
    clippy::too_many_lines,
    reason = "CG-028-focused-functions: TODO(cg): legacy key generation; split per key type"
)]
pub(super) fn syscall_obj_generate_key(
    obj: c_ulong,
    key_size: c_ulong,
    usr_params: *const utee_attribute,
    param_count: c_ulong,
) -> TeeResult {
    check_user_attr_count(param_count as usize)?;
    let mut byte_size;
    let o = tee_obj_get(obj as TeeObjIdType)?;

    let type_props = {
        let o_guard = o.lock();
        // Must be a transient object
        if o_guard.info.handleFlags & TEE_HANDLE_FLAG_PERSISTENT != 0 {
            return Err(TEE_ERROR_BAD_STATE);
        }

        // Must not be initialized already
        if o_guard.info.handleFlags & TEE_HANDLE_FLAG_INITIALIZED != 0 {
            return Err(TEE_ERROR_BAD_STATE);
        }

        // Find description of object
        tee_svc_find_type_props(o_guard.info.objectType).ok_or(TEE_ERROR_NOT_SUPPORTED)?
    };

    // Check that key_size follows restrictions
    check_key_size(type_props, key_size as _)?;

    let usr_attrs_slice = load_user_attrs(usr_params, param_count as usize)?;
    let attrs = copy_in_attrs(&mut user_ta_ctx::default(), &usr_attrs_slice)?;
    tee_svc_cryp_check_attr(AttrUsage::GenerateKey, type_props, &attrs).inspect_err(|e| {
        tee_debug!("tee_svc_cryp_check_attr error: {:X?}", e);
    })?;

    let mut o_guard = o.lock();
    let object_type = o_guard.info.objectType;
    match object_type {
        TEE_TYPE_AES
        | TEE_TYPE_DES
        | TEE_TYPE_DES3
        | TEE_TYPE_SM4
        | TEE_TYPE_HMAC_MD5
        | TEE_TYPE_HMAC_SHA1
        | TEE_TYPE_HMAC_SHA224
        | TEE_TYPE_HMAC_SHA256
        | TEE_TYPE_HMAC_SHA384
        | TEE_TYPE_HMAC_SHA512
        | TEE_TYPE_HMAC_SHA3_224
        | TEE_TYPE_HMAC_SHA3_256
        | TEE_TYPE_HMAC_SHA3_384
        | TEE_TYPE_HMAC_SHA3_512
        | TEE_TYPE_HMAC_SM3
        | TEE_TYPE_GENERIC_SECRET => {
            byte_size = key_size as usize / 8;

            // In GP Internal API Specification 1.0 the partity bits
            // aren't counted when telling the size of the key in bits.
            if is_gp_legacy_des_key_size(object_type, key_size as _) {
                byte_size = (key_size as usize + key_size as usize / 7) / 8;
            }

            // check attr
            if o_guard.attr.is_empty() {
                return Err(TEE_ERROR_BAD_STATE);
            }

            // get secret value
            let secret_value = match &mut o_guard.attr[0] {
                TeeCryptObj::ObjSecret(secret_value) => secret_value,
                _ => return Err(TEE_ERROR_BAD_STATE),
            };

            if byte_size > secret_value.secret().alloc_size as usize {
                return Err(TEE_ERROR_EXCESS_DATA);
            }

            // read random data
            crypto_rng_read(&mut secret_value.data_mut()[..byte_size])?;

            secret_value.secret_mut().key_size = byte_size as _;

            // Set bits for all known attributes for this object type
            o_guard.have_attrs = (1 << type_props.num_type_attrs as u32) - 1;
        }
        TEE_TYPE_RSA_KEYPAIR => {
            tee_svc_obj_generate_key_rsa(
                &mut o_guard,
                type_props,
                key_size as _,
                &attrs,
                object_type,
            )
            .inspect_err(|e| {
                tee_debug!("tee_svc_obj_generate_key_rsa error: {:X?}", e);
            })?;
        }
        #[expect(
            clippy::todo,
            reason = "CG-023-unfinished-implementations: TODO(cg): DSA key-pair generation is not \
                      implemented; the mbedtls backend provides no DSA key generation"
        )]
        TEE_TYPE_DSA_KEYPAIR => {
            // mbedtls do not support DSA key generation
            todo!()
        }
        #[expect(
            clippy::todo,
            reason = "CG-023-unfinished-implementations: TODO(cg): DH key-pair generation is not \
                      implemented; the mbedtls backend provides no DH key generation"
        )]
        TEE_TYPE_DH_KEYPAIR => {
            // mbedtls do not support DH key generation
            todo!()
        }
        TEE_TYPE_ECDSA_KEYPAIR
        | TEE_TYPE_ECDH_KEYPAIR
        | TEE_TYPE_SM2_DSA_KEYPAIR
        | TEE_TYPE_SM2_KEP_KEYPAIR
        | TEE_TYPE_SM2_PKE_KEYPAIR => {
            tee_svc_obj_generate_key_ecc(
                &mut o_guard,
                type_props,
                key_size as _,
                &attrs,
                object_type,
            )?;
        }
        TEE_TYPE_X25519_KEYPAIR => {
            tee_svc_obj_generate_key_x25519(&mut o_guard, type_props, key_size as _, &attrs)?;
        }
        TEE_TYPE_X448_KEYPAIR => {
            tee_svc_obj_generate_key_x448(&mut o_guard, type_props, key_size as _, &attrs)?;
        }
        TEE_TYPE_ED25519_KEYPAIR => {
            tee_svc_obj_generate_key_ed25519(&mut o_guard, type_props, key_size as _, &attrs)?;
        }
        _ => {
            return Err(TEE_ERROR_BAD_FORMAT);
        }
    }

    o_guard.info.objectSize = key_size as _;
    o_guard.info.handleFlags |= TEE_HANDLE_FLAG_INITIALIZED;
    Ok(())
}

#[cfg(unittest)]
fn long2byte(value: u64, ch: &mut [u8]) -> u32 {
    // Convert value to big-endian byte array
    // Store valid bytes from the beginning of the array (ch[0..len])
    // Example: e = 65539 (0x00010003) -> ch[0..3] = [0x01, 0x00, 0x03], len = 3
    if value == 0 {
        return 0;
    }

    // Calculate the number of bytes needed
    let mut num_bytes = 0;
    let mut temp = value;
    while temp > 0 {
        num_bytes += 1;
        temp >>= 8;
    }

    // Store bytes from most significant to least significant, starting at ch[0]
    let mut temp = value;
    for i in (0..num_bytes).rev() {
        ch[i] = (temp & 0xff) as u8;
        temp >>= 8;
    }

    num_bytes as u32
}

#[cfg(unittest)]
#[expect(
    clippy::panic,
    reason = "CG-021-no-panic-on-reachable-failure: test scenario: unittest-only helper that \
              rejects a value-attribute id passed as a reference attribute"
)]
pub(crate) fn tee_init_ref_attribute(
    attr: &mut utee_attribute,
    attribute_id: u32,
    buffer: *const u8,
    length: u32,
) {
    if (attribute_id & TEE_ATTR_FLAG_VALUE) != 0 {
        panic!("attributeID is value attribute");
    }
    attr.attribute_id = attribute_id;
    attr.a = buffer as u64;
    attr.b = length as u64;
}

unittest::path_mod_test!(pub(super) mod tests_tee_svc_cryp, "../../ut-tests/tee/tee_svc_cryp/tests_tee_svc_cryp.rs");
