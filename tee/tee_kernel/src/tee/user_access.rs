// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, vec};
use core::mem::{MaybeUninit, size_of, transmute};

use osvm::*;
use tee_raw_sys::{libc_compat::size_t, *};

use super::TeeResult;

pub(crate) fn copy_from_user(kaddr: &mut [u8], uaddr: &[u8], len: size_t) -> TeeResult {
    read_vm_mem(uaddr.as_ptr(), unsafe {
        transmute::<&mut [u8], &mut [MaybeUninit<u8>]>(&mut kaddr[..len])
    })
    .map_err(|error| match error {
        MemError::InvalidAddr | MemError::NoAccess => TEE_ERROR_BAD_PARAMETERS,
        _ => TEE_ERROR_GENERIC,
    })?;
    Ok(())
}

pub(crate) fn copy_to_user(uaddr: &mut [u8], kaddr: &[u8], _len: size_t) -> TeeResult {
    write_vm_mem(uaddr.as_mut_ptr(), kaddr).map_err(|error| match error {
        MemError::InvalidAddr | MemError::NoAccess => TEE_ERROR_BAD_PARAMETERS,
        _ => TEE_ERROR_GENERIC,
    })?;
    Ok(())
}

pub fn copy_from_user_u64(s: &mut u64, user_s: &u64) -> TeeResult {
    let s_bytes: &mut [u8] = unsafe {
        core::slice::from_raw_parts_mut(s as *mut u64 as *mut u8, core::mem::size_of::<u64>())
    };
    let size_bytes: &[u8] = unsafe {
        core::slice::from_raw_parts(
            user_s as *const u64 as *const u8,
            core::mem::size_of::<u64>(),
        )
    };
    copy_from_user(s_bytes, size_bytes, core::mem::size_of::<u64>())?;

    Ok(())
}

pub fn copy_to_user_u64(user_s: &mut u64, s: &u64) -> TeeResult {
    let s_bytes: &[u8] = unsafe {
        core::slice::from_raw_parts(s as *const u64 as *const u8, core::mem::size_of::<u64>())
    };
    let size_bytes: &mut [u8] = unsafe {
        core::slice::from_raw_parts_mut(user_s as *mut u64 as *mut u8, core::mem::size_of::<u64>())
    };
    copy_to_user(size_bytes, s_bytes, core::mem::size_of::<u64>())?;

    Ok(())
}

/// 将内核空间的结构体复制到用户空间
///
/// # 参数
/// - `user_dst`: 用户空间的目标结构体（可变引用）
/// - `kernel_src`: 内核空间的源结构体（不可变引用）
///
/// # 类型参数
/// - `T`: 要复制的结构体类型，必须是 `Sized` 类型
///
/// # 安全性
/// - 调用者必须确保 `user_dst` 指向有效的用户空间内存
/// - `T` 必须是 `repr(C)` 或 `repr(transparent)` 结构体，以确保内存布局可预测
pub fn copy_to_user_struct<T: Sized>(user_dst: &mut T, kernel_src: &T) -> TeeResult {
    let src_bytes: &[u8] =
        unsafe { core::slice::from_raw_parts(kernel_src as *const T as *const u8, size_of::<T>()) };
    let dst_bytes: &mut [u8] =
        unsafe { core::slice::from_raw_parts_mut(user_dst as *mut T as *mut u8, size_of::<T>()) };
    copy_to_user(dst_bytes, src_bytes, size_of::<T>())
}

/// 从用户空间的结构体复制到内核空间
///
/// # 参数
/// - `kernel_dst`: 内核空间的目标结构体（可变引用）
/// - `user_src`: 用户空间的源结构体（不可变引用）
///
/// # 类型参数
/// - `T`: 要复制的结构体类型，必须是 `Sized` 类型
///
/// # 安全性
/// - 调用者必须确保 `user_src` 指向有效的用户空间内存
/// - `T` 必须是 `repr(C)` 或 `repr(transparent)` 结构体，以确保内存布局可预测
pub fn copy_from_user_struct<T: Sized>(kernel_dst: &mut T, user_src: &T) -> TeeResult {
    let dst_bytes: &mut [u8] =
        unsafe { core::slice::from_raw_parts_mut(kernel_dst as *mut T as *mut u8, size_of::<T>()) };
    let src_bytes: &[u8] =
        unsafe { core::slice::from_raw_parts(user_src as *const T as *const u8, size_of::<T>()) };
    copy_from_user(dst_bytes, src_bytes, size_of::<T>())
}

#[inline(always)]
/// copy from user private
///
/// TODO: need check access permission
pub fn copy_from_user_private(kaddr: &mut [u8], uaddr: &[u8], len: size_t) -> TeeResult {
    copy_from_user(kaddr, uaddr, len)
}

#[inline(always)]
/// copy to user private
///
/// TODO: need check access permission
pub fn copy_to_user_private(uaddr: &mut [u8], kaddr: &[u8], len: size_t) -> TeeResult {
    copy_to_user(uaddr, kaddr, len)
}

/// allocate memory from kernel
///
/// use for temporary memory allocation, can be optimized
pub fn bb_alloc(len: usize) -> TeeResult<Box<[u8]>> {
    let kbuf: Box<[u8]> = vec![0u8; len as _].into_boxed_slice();

    Ok(kbuf)
}

/// free memory to kernel
///
/// use for temporary memory allocation, can be optimized
pub fn bb_free(kbuf: Box<[u8]>, len: usize) {
    drop(kbuf);
    let _ = len;
}

fn __bb_memdup_user(
    copy_func: fn(&mut [u8], &[u8], size_t) -> TeeResult,
    src: &[u8],
) -> TeeResult<Box<[u8]>> {
    let mut buf = bb_alloc(src.len())?;
    copy_func(&mut buf, src, src.len())?;
    Ok(buf)
}

pub fn bb_memdup_user(src: &[u8]) -> TeeResult<Box<[u8]>> {
    __bb_memdup_user(copy_from_user, src)
}

pub(crate) fn bb_memdup_user_private(src: &[u8]) -> TeeResult<Box<[u8]>> {
    __bb_memdup_user(copy_from_user_private, src)
}

/// Enter user access context
/// This enables safe access to user-space memory
#[inline(always)]
pub(crate) fn enter_user_access() {
    // Implementation would enable user memory access permissions
    // In OP-TEE, this dispatch_irqs entering user context for cryptographic operations
}

/// Exit user access context
/// This restores kernel/secure-world memory access permissions
#[inline(always)]
pub(crate) fn exit_user_access() {
    // Implementation would disable user memory access permissions
    // In OP-TEE, this dispatch_irqs returning from user context
}

#[unittest::mod_test]
pub mod tests_user_access {
    use unittest::{assert, assert_eq};

    use super::*;
    use crate::{TestUserBuffer, TestUserValue};

    fn user_buffer_from_bytes(bytes: &[u8]) -> TestUserBuffer {
        let buffer = TestUserBuffer::new(bytes.len()).unwrap();
        buffer.write_bytes(bytes).unwrap();
        buffer
    }

    #[unittest::def_test(custom)]
    fn test_copy_from_user() {
        let user_data = [1, 2, 3, 4, 5];
        let mut user_data_buf = user_buffer_from_bytes(&user_data);
        let mut kernel_data: [u8; 5] = [0; 5];

        copy_from_user(
            &mut kernel_data,
            user_data_buf.as_user_slice(user_data.len()),
            5,
        )
        .unwrap();
        assert_eq!(kernel_data, user_data);
    }

    #[unittest::def_test(custom)]
    fn test_copy_to_user() {
        let kernel_data = [10, 20, 30, 40, 50];
        let mut user_data = TestUserBuffer::new(kernel_data.len()).unwrap();

        copy_to_user(user_data.as_user_slice(kernel_data.len()), &kernel_data, 5).unwrap();
        let user_data_bytes = user_data.read_bytes(kernel_data.len()).unwrap();
        assert_eq!(user_data_bytes.as_slice(), &kernel_data);
    }

    #[unittest::def_test(custom)]
    fn test_copy_from_user_u64() {
        let mut user_value = TestUserValue::<u64>::from_value(0x1122334455667788).unwrap();
        let mut kernel_value: u64 = 0;

        copy_from_user_u64(&mut kernel_value, user_value.as_user_ref()).unwrap();
        assert_eq!(kernel_value, user_value.read());

        let mut user_value_out = TestUserValue::<u64>::from_value(1).unwrap();
        copy_to_user_u64(user_value_out.as_user_ref(), &kernel_value).unwrap();
        assert_eq!(user_value_out.read(), kernel_value);
    }

    #[unittest::def_test]
    fn test_bb_alloc_free() {
        let kbuf = bb_alloc(10).unwrap();
        assert!(kbuf.iter().all(|byte| *byte == 0));
        assert_eq!(kbuf.len(), 10);
        bb_free(kbuf, 10);
    }

    #[unittest::def_test(custom)]
    fn test_bb_memdup_user() {
        let src = [1, 2, 3, 4, 5];
        let mut src_user = user_buffer_from_bytes(&src);
        let buf = bb_memdup_user(src_user.as_user_slice(src.len())).unwrap();
        assert_eq!(&buf[..], &src[..]);
    }

    #[unittest::def_test(custom)]
    fn test_bb_memdup_user_private() {
        let src = [1, 2, 3, 4, 5];
        let mut src_user = user_buffer_from_bytes(&src);
        let buf = bb_memdup_user_private(src_user.as_user_slice(src.len())).unwrap();
        assert_eq!(&buf[..], &src[..]);
    }
}
