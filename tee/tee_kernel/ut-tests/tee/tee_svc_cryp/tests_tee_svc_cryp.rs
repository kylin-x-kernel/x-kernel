// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{TestResult, assert, assert_eq};
use zerocopy::IntoBytes;

use super::*;
use crate::TestUserValue;

#[unittest::def_test]
fn test_tee_svc_cryp_utils() {
    // test attr_bytes from u32
    let a_u32: u32 = 0xAABBCCDD;
    let attr_bytes = a_u32.to_ne_bytes();
    let value: [u32; 2] = [u32::from_ne_bytes(attr_bytes), 0];
    assert_eq!(value[0], 0xAABBCCDD_u32);
    assert_eq!(size_of_val(&value), 8);

    // test tee_u32_to_big_endian
    let val: u32 = 0x12345678;
    let be_val = tee_u32_to_big_endian(val);
    assert_eq!(be_val, 0x78563412);
    assert_eq!(be_val.as_bytes(), &[0x12, 0x34, 0x56, 0x78]);

    // test op_u32_to_binary_helper
    let mut buffer: [u8; 8] = [0; 8];
    let mut offs: size_t = 0;
    op_u32_to_binary_helper(0x11223344, &mut buffer, &mut offs).unwrap();
    assert_eq!(offs, 4);
    assert_eq!(&buffer[0..4], &[0x11, 0x22, 0x33, 0x44]);

    // test op_u32_to_binary_helper with offset
    op_u32_to_binary_helper(0x55667788, &mut buffer, &mut offs).unwrap();
    assert_eq!(offs, 8);
    assert_eq!(&buffer[4..8], &[0x55, 0x66, 0x77, 0x88]);

    // test overflow
    let mut small_buffer: [u8; 4] = [0; 4];
    let mut offs_overflow: size_t = usize::MAX - 2;
    let result = op_u32_to_binary_helper(0x99AABBCC, &mut small_buffer, &mut offs_overflow);
    assert_eq!(result.err(), Some(TEE_ERROR_OVERFLOW));

    // test insufficient buffer
    let mut insufficient_buffer: [u8; 4] = [0; 4];
    let mut offs_insufficient: size_t = 2;
    let result =
        op_u32_to_binary_helper(0x11223344, &mut insufficient_buffer, &mut offs_insufficient);
    assert!(result.is_ok());
    assert_eq!(offs_insufficient, 6);
    assert_eq!(&insufficient_buffer, &[0; 4]); // buffer remains unchanged
}

#[unittest::def_test]
fn test_tee_svc_find_type_props() {
    let props = tee_svc_find_type_props(TEE_TYPE_AES);
    assert!(props.is_some());
    let props = props.unwrap();
    assert_eq!(props.obj_type, TEE_TYPE_AES);
    assert_eq!(props.min_size, 128);
    assert_eq!(props.max_size, 256);
}

#[unittest::def_test(user)]
fn test_op_attr_secret_value_from_user() {
    // 测试基础数据
    let user_key = [0xAAu8; 16];
    let mut secret_wrapper = TeeCrypObjSecretWrapper::new(32);

    // 从用户空间导入密钥
    op_attr_secret_value_from_user(&mut secret_wrapper, &user_key).unwrap();

    // 验证密钥大小和内容
    assert_eq!(secret_wrapper.secret().key_size, 16);
    assert_eq!(secret_wrapper.secret().alloc_size, 32);
    assert_eq!(&secret_wrapper.data()[..16], &user_key);

    // 测试长度超出分配大小的情况
    let long_user_key = [0xBBu8; 40];
    let result = op_attr_secret_value_from_user(&mut secret_wrapper, &long_user_key);
    assert_eq!(result.err(), Some(TEE_ERROR_SHORT_BUFFER));
}

#[unittest::def_test(user)]
fn test_op_attr_secret_value_to_bytes() {
    // 准备测试数据
    let mut secret_wrapper = TeeCrypObjSecretWrapper::new(32);
    let key_data: [u8; 16] = [0xCC; 16];
    // 手动设置密钥数据和大小
    {
        let data_slice = secret_wrapper.data_mut();
        data_slice[..16].copy_from_slice(&key_data);
        secret_wrapper.secret_mut().key_size = 16;
    }
    let exported = op_attr_secret_value_to_bytes(&secret_wrapper);
    assert_eq!(exported.len(), 16);
    assert_eq!(&*exported, &key_data);
}

#[unittest::def_test]
fn test_op_attr_secret_value_to_binary() {
    // 准备测试数据
    let mut secret_wrapper = TeeCrypObjSecretWrapper::new(32);
    let key_data: [u8; 16] = [0xDD; 16];
    // 手动设置密钥数据和大小
    {
        let data_slice = secret_wrapper.data_mut();
        data_slice[..16].copy_from_slice(&key_data);
        secret_wrapper.secret_mut().key_size = 16;
    }
    // 准备目标缓冲区
    let mut buffer: [u8; 64] = [0; 64];
    let mut offs: size_t = 0;
    // 调用函数进行序列化
    let result = op_attr_secret_value_to_binary(&secret_wrapper, &mut buffer, &mut offs);
    assert!(result.is_ok());
    // 验证偏移量
    assert_eq!(offs, 4 + 16); // 4 bytes for key_size + 16 bytes for key data
    // 验证序列化内容
    let expected_key_size_bytes: [u8; 4] = [0x00, 0x00, 0x00, 0x10]; // big-endian
    assert_eq!(&buffer[0..4], &expected_key_size_bytes);
    assert_eq!(&buffer[4..20], &key_data);

    // test op_attr_secret_value_from_binary
    let mut new_secret_wrapper = TeeCrypObjSecretWrapper::new(32);
    let mut offs_from: size_t = 0;
    let result =
        op_attr_secret_value_from_binary(&mut new_secret_wrapper, &buffer, &mut offs_from);
    assert!(result.is_ok());
    // 验证偏移量
    assert_eq!(offs_from, 4 + 16);
    // 验证反序列化内容
    assert_eq!(new_secret_wrapper.secret().key_size, 16);
    assert_eq!(new_secret_wrapper.secret().alloc_size, 32);
    assert_eq!(&new_secret_wrapper.data()[..16], &key_data);
}

#[unittest::def_test]
fn test_op_attr_secret_value_from_binary_rejects_bad_inputs() {
    let mut secret_wrapper = TeeCrypObjSecretWrapper::new(8);
    let mut offs: size_t = 0;

    let truncated = [0x00, 0x00, 0x00, 0x04, 0xAA, 0xBB];
    let result = op_attr_secret_value_from_binary(&mut secret_wrapper, &truncated, &mut offs);
    assert_eq!(result.err(), Some(TEE_ERROR_BAD_PARAMETERS));

    let oversized = [0x00, 0x00, 0x00, 0x10];
    offs = 0;
    let result = op_attr_secret_value_from_binary(&mut secret_wrapper, &oversized, &mut offs);
    assert_eq!(result.err(), Some(TEE_ERROR_BAD_PARAMETERS));
}

#[unittest::def_test]
fn test_op_u32_from_binary_helper() {
    let data: [u8; 8] = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88];
    let mut offs: size_t = 0;
    let mut value: u32 = 0;

    // 第一次读取
    let result = op_u32_from_binary_helper(&mut value, &data, &mut offs);
    assert!(result.is_ok());
    assert_eq!(value, 0x11223344);
    assert_eq!(offs, 4);

    // 第二次读取
    let result = op_u32_from_binary_helper(&mut value, &data, &mut offs);
    assert!(result.is_ok());
    assert_eq!(value, 0x55667788);
    assert_eq!(offs, 8);

    // 测试读取超出边界
    let result = op_u32_from_binary_helper(&mut value, &data, &mut offs);
    assert_eq!(result.err(), Some(TEE_ERROR_BAD_PARAMETERS));

    // call op_u32_to_binary_helper
    let mut buffer: [u8; 4] = [0; 4];
    let mut offs_write: size_t = 0;
    op_u32_to_binary_helper(0x99AABBCC, &mut buffer, &mut offs_write).unwrap();
    assert_eq!(offs_write, 4);
    assert_eq!(&buffer, &[0x99, 0xAA, 0xBB, 0xCC]);
    // read back
    let mut read_value: u32 = 0;
    let mut offs_read: size_t = 0;
    let result = op_u32_from_binary_helper(&mut read_value, &buffer, &mut offs_read);
    assert!(result.is_ok());
    assert_eq!(read_value, 0x99AABBCC_u32);
    assert_eq!(offs_read, 4);
}

#[unittest::def_test(user)]
fn test_op_attr_value_to_user() {
    let mut attr: [u8; 8] = [0; 8];
    // 设置属性值为 0x11223344
    let value: u32 = 0x11223344;
    let value_bytes = value.to_ne_bytes();
    attr[..4].copy_from_slice(&value_bytes);

    let mut size = 8;
    let mut buffer = [0u8; 8];

    let result = op_attr_value_to_user(&attr, &mut buffer, &mut size);
    assert!(result.is_ok());
    assert_eq!(size, 8);
    assert_eq!(&buffer[..4], &value_bytes);
}

#[unittest::def_test(user)]
fn test_op_attr_value_to_user_short_buffer() {
    let mut attr: [u8; 8] = [0; 8];
    let value: u32 = 0x11223344;
    attr[..4].copy_from_slice(&value.to_ne_bytes());

    let mut size = 4;
    let mut buffer = [0u8; 4];

    let result = op_attr_value_to_user(&attr, &mut buffer, &mut size);
    assert_eq!(result.err(), Some(TEE_ERROR_SHORT_BUFFER));
}

#[unittest::def_test]
fn test_op_attr_value_from_binary() {
    let mut attr: [u8; 8] = [0; 8];
    let value: u32 = 0x11223344;
    let value_bytes = value.to_ne_bytes();

    // attr[..4].copy_from_slice(value_bytes);
    let mut offs: size_t = 0;
    let result = op_attr_value_from_binary(&mut attr, &value_bytes, &mut offs);
    // info!("result: {:?}, offs: {}, attr: {:?}", result, offs, attr);
    assert!(result.is_ok());
    assert_eq!(offs, 4);
    assert_eq!(&attr[..4], &[0x11, 0x22, 0x33, 0x44]);

    // // test op_attr_value_to_binary
    let mut buffer: [u8; 8] = [0; 8];
    let mut offs_write: size_t = 0;
    let result = op_attr_value_to_binary(&attr, &mut buffer, &mut offs_write);
    assert!(result.is_ok());
    assert_eq!(offs_write, 4);
    assert_eq!(&buffer[..4], &value_bytes);
}

#[unittest::def_test]
fn test_tee_obj_set_type() {
    // test with TEE_TYPE_AES
    let mut obj = TeeObj::default();
    let result = tee_obj_set_type(&mut obj, TEE_TYPE_AES, 256);
    assert!(result.is_ok());
    assert_eq!(obj.info.objectType, TEE_TYPE_AES);
    assert_eq!(obj.info.maxObjectSize, 256);
    assert_eq!(obj.info.objectUsage, TEE_USAGE_DEFAULT);
    assert_eq!(obj.info.handleFlags, 0);
    assert_eq!(obj.info.dataSize, 0);
    assert_eq!(obj.info.dataPosition, 0);
    assert_eq!(obj.attr.len(), 1);
    assert!(matches!(obj.attr[0], TeeCryptObj::ObjSecret(_)));

    let mut obj = TeeObj::default();
    let result = tee_obj_set_type(&mut obj, TEE_TYPE_ECDSA_PUBLIC_KEY, 256);
    assert!(result.is_ok());
    assert_eq!(obj.info.objectType, TEE_TYPE_ECDSA_PUBLIC_KEY);
    assert_eq!(obj.info.maxObjectSize, 256);
    assert_eq!(obj.info.objectUsage, TEE_USAGE_DEFAULT);
    assert_eq!(obj.info.handleFlags, 0);
    assert_eq!(obj.info.dataSize, 0);
    assert_eq!(obj.info.dataPosition, 0);
    assert_eq!(obj.attr.len(), 1);
    assert!(matches!(obj.attr[0], TeeCryptObj::EccPublicKey(_)));
}

#[unittest::def_test]
fn test_tee_obj_set_type_rejects_invalid_reuse_and_key_size() {
    let mut obj = TeeObj::default();
    obj.attr.push(TeeCryptObj::None);
    let result = tee_obj_set_type(&mut obj, TEE_TYPE_DATA, 0);
    assert_eq!(result.err(), Some(TEE_ERROR_BAD_STATE));

    let mut data_obj = TeeObj::default();
    let result = tee_obj_set_type(&mut data_obj, TEE_TYPE_DATA, 1);
    assert_eq!(result.err(), Some(TEE_ERROR_NOT_SUPPORTED));
}

#[unittest::def_test(user)]
fn test_cryptoattrref_u32() {
    // test CryptoAttrRef::U32
    let mut value: u32 = 0;
    let value_c: [u32; 2] = [0x11223344, 0];
    let mut value_bytes = [0u8; size_of::<[u32; 2]>()];
    value_bytes[..size_of::<u32>()].copy_from_slice(&value_c[0].to_ne_bytes());
    value_bytes[size_of::<u32>()..].copy_from_slice(&value_c[1].to_ne_bytes());
    {
        let mut attr_ref = CryptoAttrRef::U32(&mut value);
        let result = attr_ref.import_from_bytes(&value_bytes);
        assert!(result.is_ok());
    }
    assert_eq!(value, 0x11223344);

    let attr_ref = CryptoAttrRef::U32(&mut value);
    let exported = attr_ref.export_to_bytes().unwrap();
    assert_eq!(&*exported, &value_bytes);
}

#[unittest::def_test(user)]
fn test_cryptoattrref_bignum() {
    // test CryptoAttrRef::BigNum
    let bn = BigNum::new(0x11223344).unwrap();
    let buffer = bn.export_to_bytes().unwrap();
    let mut bn_from = BigNum::new(0).unwrap();
    let result = bn_from.import_from_bytes(&buffer);
    assert!(result.is_ok());
    assert_eq!(bn_from, bn);
}

#[unittest::def_test(user)]
fn test_secret_value() {
    // set secret value data to
    let mut secret = TeeCrypObjSecretWrapper::new(16);
    secret.secret_mut().key_size = 16;
    secret.data_mut()[..16].copy_from_slice(&[0xaa; 16]);

    // 1. test TeeCrypObjSecretWrapper to user
    // - test export_to_bytes
    let buffer = secret.export_to_bytes().unwrap();
    assert_eq!(buffer.len(), 16);
    assert_eq!(&*buffer, &secret.data()[..16]);
    // - test import_from_bytes
    let mut secret_dest = TeeCrypObjSecretWrapper::new(16);
    let result = secret_dest.import_from_bytes(&buffer);
    assert!(result.is_ok());
    assert_eq!(secret_dest.secret().key_size, secret.secret().key_size);
    assert_eq!(&secret_dest.data()[..16], &secret.data()[..16]);
    //  - test to_binary
    let mut data: [u8; 16 + size_of::<u32>()] = [0x55; 16 + size_of::<u32>()];
    let mut offs: usize = 0;
    let result = secret_dest.to_binary(&mut data, &mut offs);
    assert!(result.is_ok());
    assert_eq!(offs, 16 + size_of::<u32>());
    assert_eq!(
        &data[..size_of::<u32>()],
        &secret_dest.secret().key_size.to_be_bytes()
    );
    assert_eq!(
        &data[size_of::<u32>()..16 + size_of::<u32>()],
        &secret_dest.data()[..16]
    );
    //  - test update_from_binary
    let mut secret_from = TeeCrypObjSecretWrapper::new(16);
    offs = 0;
    let result = secret_from.update_from_binary(&data, &mut offs);
    assert!(result.is_ok());
    assert_eq!(offs, 16 + size_of::<u32>());
    assert_eq!(secret_from.secret().key_size, secret_dest.secret().key_size);
    assert_eq!(&secret_from.data()[..16], &secret_dest.data()[..16]);

    // - test update_from_obj
    let mut secret_dest = TeeCrypObjSecretWrapper::new(16);
    let result =
        secret_dest.update_from_obj(&TeeCryptObjAttr::SecretValue(secret_from.clone()));
    assert!(result.is_ok());
    assert_eq!(secret_dest.secret().key_size, secret_from.secret().key_size);
    assert_eq!(&secret_dest.data(), &secret_from.data());

    // 2. test CryptoAttrRef::SecretValue
    let attr_ref = CryptoAttrRef::SecretValue(&mut secret);
    let exported = attr_ref.export_to_bytes().unwrap();
    assert_eq!(exported.len(), 16);
    assert_eq!(&*exported, &secret.data()[..16]);
}

#[unittest::def_test(user)]
fn test_syscall_cryp_obj_alloc() {
    let obj_id = TestUserValue::<c_uint>::from_value(0).unwrap();
    let result =
        syscall_cryp_obj_alloc(TEE_TYPE_ECDSA_PUBLIC_KEY as _, 256, obj_id.as_user_ptr());
    assert!(result.is_ok());
    let obj_id = obj_id.read();
    let obj_arc = tee_obj_get(obj_id as TeeObjIdType).unwrap();
    let obj = obj_arc.lock();
    assert_eq!(obj.info.objectType, TEE_TYPE_ECDSA_PUBLIC_KEY);
    assert_eq!(obj.info.maxObjectSize, 256);
    assert_eq!(obj.info.objectUsage, TEE_USAGE_DEFAULT);
    assert_eq!(obj.info.handleFlags, 0);
    assert_eq!(obj.info.dataSize, 0);
    assert_eq!(obj.info.dataPosition, 0);
    assert_eq!(obj.attr.len(), 1);
    assert!(matches!(obj.attr[0], TeeCryptObj::EccPublicKey(_)));
}

#[unittest::def_test(user)]
fn test_syscall_cryp_obj_get_attr() {
    let obj_id = TestUserValue::<c_uint>::from_value(0).unwrap();
    let result =
        syscall_cryp_obj_alloc(TEE_TYPE_ECDSA_PUBLIC_KEY as _, 256, obj_id.as_user_ptr());
    assert!(result.is_ok());
    let _obj_id = obj_id.read();
    let _buffer: [u8; 8] = [1; 8];
    let _size: u64 = 8;
    // TODO: need to implement syscall_cryp_obj_get_attr
    // let result = syscall_cryp_obj_get_attr(obj_id, TEE_ATTR_ECC_CURVE as c_ulong, &mut buffer, &mut size);
    // info!("result: {:x?}, size: {}, buffer: {:?}", result, size, buffer);
    // assert!(result.is_ok());
    // assert_eq!(size, 8);
    // assert_eq!(&buffer[..4], &[0x00, 0x00, 0x00, 0x00]);
}

#[unittest::def_test(user)]
fn test_syscall_cryp_generate_key_ecc_keypair() {
    // alloc sm2 key pair
    let obj_id = TestUserValue::<c_uint>::from_value(0).unwrap();
    let result =
        syscall_cryp_obj_alloc(TEE_TYPE_SM2_DSA_KEYPAIR as _, 256, obj_id.as_user_ptr());
    assert!(result.is_ok());
    let obj_id = obj_id.read();
    // sm2 no need usr_params
    let result = syscall_obj_generate_key(obj_id as c_ulong, 256, core::ptr::null(), 0);
    assert!(result.is_ok());
    // get attr from obj
    let obj_arc = tee_obj_get(obj_id as TeeObjIdType);
    assert!(obj_arc.is_ok());
    let obj_arc = obj_arc.unwrap();
    let obj = obj_arc.lock();
    assert_eq!(obj.info.objectType, TEE_TYPE_SM2_DSA_KEYPAIR);
    assert_eq!(obj.info.maxObjectSize, 256);
    assert_eq!(obj.info.objectUsage, TEE_USAGE_DEFAULT);
    assert_eq!(obj.attr.len(), 1);
    assert!(matches!(obj.attr[0], TeeCryptObj::EccKeypair(_)));
    // get EccKeypair from obj
    let ecc_kp = match &obj.attr[0] {
        TeeCryptObj::EccKeypair(kp) => kp,
        _ => panic!("EccKeypair not found"),
    };
    assert_eq!(ecc_kp.curve, TEE_ECC_CURVE_SM2);
    tee_debug!("EccKeypair: {:#?}", ecc_kp);
    let d_len = ecc_kp.d.byte_length();
    let x_len = ecc_kp.x.byte_length();
    let y_len = ecc_kp.y.byte_length();
    assert!(d_len == 31 || d_len == 32);
    assert!(x_len == 31 || x_len == 32);
    assert!(y_len == 31 || y_len == 32);
}

// Helper function to test RSA keypair generation and verification
fn test_rsa_keypair(key_size: usize, e: u64) -> TestResult {
    let mut e_bytes: [u8; 8] = [0; 8];
    let mut usr_params = crate::user_vec![utee_attribute::default(); 1];
    let mut usr_exp = crate::user_vec![0u8; 8];

    let (usr_params, param_count) = {
        if e == 0 {
            (core::ptr::null_mut(), 0)
        } else {
            let e_len = long2byte(e, &mut e_bytes);
            let mut usr_exp_data = [0u8; 8];
            usr_exp_data[..e_len as usize].copy_from_slice(&e_bytes[..e_len as usize]);
            usr_exp.write(usr_exp_data);
            let mut usr_params_data = [utee_attribute::default(); 1];
            tee_init_ref_attribute(
                &mut usr_params_data[0],
                TEE_ATTR_RSA_PUBLIC_EXPONENT,
                usr_exp.as_user_ptr(),
                e_len,
            );
            usr_params.write(usr_params_data);
            (usr_params.as_user_ptr(), 1)
        }
    };

    let obj_id = TestUserValue::<c_uint>::from_value(0).unwrap();
    let result = syscall_cryp_obj_alloc(
        TEE_TYPE_RSA_KEYPAIR as _,
        key_size as _,
        obj_id.as_user_ptr(),
    );
    assert!(result.is_ok());
    let obj_id = obj_id.read();

    let result =
        syscall_obj_generate_key(obj_id as c_ulong, key_size as _, usr_params, param_count);
    assert!(result.is_ok());
    // get attr from obj
    let obj_arc = tee_obj_get(obj_id as TeeObjIdType);
    assert!(obj_arc.is_ok());
    let obj_arc = obj_arc.unwrap();
    let obj = obj_arc.lock();
    assert_eq!(obj.info.objectType, TEE_TYPE_RSA_KEYPAIR);
    assert_eq!(obj.info.maxObjectSize, key_size as u32);
    assert_eq!(obj.info.objectUsage, TEE_USAGE_DEFAULT);
    assert_eq!(obj.attr.len(), 1);
    assert!(matches!(obj.attr[0], TeeCryptObj::RsaKeypair(_)));
    // get RsaKeypair from obj
    let rsa_kp = match &obj.attr[0] {
        TeeCryptObj::RsaKeypair(kp) => kp,
        _ => panic!("RsaKeypair not found"),
    };
    assert_eq!(rsa_kp.n.byte_length(), key_size / 8);
    // print the keypai exponent
    tee_debug!("RsaKeypair: {:#?}", rsa_kp);

    // test RsaKeypair.e equals to the exponent
    if e != 0 {
        assert_eq!(rsa_kp.e.as_u32().unwrap(), e as u32);
    } else {
        assert_eq!(rsa_kp.e.as_u32().unwrap(), 65537u32);
    }

    TestResult::Ok
}

#[unittest::def_test(user)]
fn test_syscall_cryp_generate_key_rsa() {
    // step1: test without usr_params (use default exponent 65537)
    if let TestResult::Failed = test_rsa_keypair(2048, 0) {
        return TestResult::Failed;
    }
    // step2: test with custom exponent
    if let TestResult::Failed = test_rsa_keypair(2048, 65539) {
        return TestResult::Failed;
    }
    // step3: test with custom exponent 65537
    if let TestResult::Failed = test_rsa_keypair(2048, 65537) {
        return TestResult::Failed;
    }
}

fn test_syscall_cryp_generate_secret_key(key_type: u32, key_size: usize) -> TestResult {
    tee_debug!(
        "test_syscall_cryp_generate_secret_key: key_type: {:?}, key_size: {:?}",
        key_type,
        key_size
    );
    // alloc sm4 key
    let obj_id = TestUserValue::<c_uint>::from_value(0).unwrap();
    let result = syscall_cryp_obj_alloc(key_type as _, key_size as _, obj_id.as_user_ptr());
    assert!(result.is_ok());
    let obj_id = obj_id.read();
    // assert!(obj_id != 0);
    // secret key no need usr_params
    let result =
        syscall_obj_generate_key(obj_id as c_ulong, key_size as _, core::ptr::null(), 0);
    assert!(result.is_ok());
    // get attr from obj
    let obj_arc = tee_obj_get(obj_id as TeeObjIdType);
    assert!(obj_arc.is_ok());
    let obj_arc = obj_arc.unwrap();
    let obj = obj_arc.lock();
    assert_eq!(obj.info.objectType, key_type);
    assert_eq!(obj.info.maxObjectSize, key_size as u32);
    assert_eq!(obj.info.objectUsage, TEE_USAGE_DEFAULT);
    assert_eq!(obj.attr.len(), 1);
    assert!(matches!(obj.attr[0], TeeCryptObj::ObjSecret(_)));
    // get secret key from obj
    let secret_key = match &obj.attr[0] {
        TeeCryptObj::ObjSecret(obj_secret) => obj_secret,
        _ => panic!("secret key not found"),
    };
    assert_eq!(secret_key.secret().key_size, (key_size / 8) as u32);
    tee_debug!("secret key: {:#?}", &obj.attr[0]);

    TestResult::Ok
}

#[unittest::def_test(user)]
fn test_syscall_cryp_generate_key_sm4() {
    if let TestResult::Failed = test_syscall_cryp_generate_secret_key(TEE_TYPE_SM4 as _, 128) {
        return TestResult::Failed;
    }
}

#[unittest::def_test(user)]
fn test_syscall_cryp_generate_key_hmac_sm3() {
    if let TestResult::Failed =
        test_syscall_cryp_generate_secret_key(TEE_TYPE_HMAC_SM3 as _, 128)
    {
        return TestResult::Failed;
    }
}

#[unittest::def_test(user)]
fn test_copy_in_attrs() {
    let mut usr_attrs = [utee_attribute::default(); 2];
    // index 0 is value attribute
    usr_attrs[0].attribute_id = TEE_ATTR_FLAG_VALUE;
    usr_attrs[0].a = 0x11223344_u64;
    usr_attrs[0].b = 0x55667788_u64;
    // index 1 is memref attribute
    // allocate memory for memref
    let mem = crate::user_vec![0xAAu8; 16];
    usr_attrs[1].attribute_id &= !TEE_ATTR_FLAG_VALUE;
    usr_attrs[1].a = mem.as_user_ptr() as u64;
    usr_attrs[1].b = 16;
    // copy in attrs
    let attrs = copy_in_attrs(&mut user_ta_ctx::default(), &usr_attrs).unwrap();
    match &attrs[0] {
        KernelAttribute::Value { id, a, b } => {
            assert_eq!(*id, TEE_ATTR_FLAG_VALUE);
            assert_eq!(*a, 0x11223344_u32);
            assert_eq!(*b, 0x55667788_u32);
        }
        _ => panic!("expected value attribute"),
    }
    match &attrs[1] {
        KernelAttribute::Memref { id, data } => {
            assert_eq!(*id, 0);
            assert_eq!(data.len(), 16);
            assert_eq!(&**data, &[0xAAu8; 16]);
        }
        _ => panic!("expected memref attribute"),
    }
}

#[unittest::def_test]
fn test_bignum_to_bytes() {
    use crate::tee::crypto::bignum::crypto_bignum_bin2bn;
    let mut bn = BigNum::new(0).unwrap();
    crypto_bignum_bin2bn(&[0x00, 0x01, 0x00], &mut bn).unwrap();
    let mut buf = [0u8; 4];
    crate::tee::crypto::bignum::crypto_bignum_bn2bin(&bn, &mut buf).unwrap();
    assert_eq!(bn.as_u32().unwrap(), 256);
}
