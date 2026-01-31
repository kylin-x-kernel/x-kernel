//! Unit tests for osvm

#![cfg(unittest)]

extern crate alloc;

use alloc::vec::Vec;
use core::mem::MaybeUninit;

use unittest::{assert, assert_eq, def_test};

use crate::{
    MemError, VirtMutPtr, VirtPtr, load_vec, load_vec_until_null, read_vm_mem, write_vm_mem,
};

#[def_test]
fn test_read_write_vm_mem_local() {
    let mut val: u64 = 0x1234567890ABCDEF;
    let ptr = &mut val as *mut u64;

    // Test Reading
    let mut out = MaybeUninit::<u64>::uninit();
    // read_vm_mem takes *const T
    let res = read_vm_mem(ptr as *const u64, core::slice::from_mut(&mut out));
    assert!(res.is_ok());
    let read_val = unsafe { out.assume_init() };
    assert_eq!(read_val, 0x1234567890ABCDEF);

    // Test Writing
    let new_val: u64 = 0xFEDCBA0987654321;
    let res = write_vm_mem(ptr, core::slice::from_ref(&new_val));
    assert!(res.is_ok());
    assert_eq!(val, 0xFEDCBA0987654321);
}

#[def_test]
fn test_virt_ptr_helpers() {
    let val: u32 = 0xDEADBEEF;
    let ptr = &val as *const u32;

    // Test VirtPtr trait
    let virt_ptr: *const u32 = ptr;
    // as_ptr
    assert_eq!(virt_ptr.as_ptr(), ptr);
    // check_non_null
    assert!(virt_ptr.check_non_null().is_some());
    let null_ptr: *const u32 = core::ptr::null();
    assert!(null_ptr.check_non_null().is_none());

    // read_vm
    let read_res = virt_ptr.read_vm();
    assert!(read_res.is_ok());
    assert_eq!(read_res.unwrap(), 0xDEADBEEF);
}

#[def_test]
fn test_virt_mut_ptr_helpers() {
    let mut val: u32 = 0xCAFEBABE;
    let ptr = &mut val as *mut u32;

    let mut_ptr: *mut u32 = ptr;

    // write_vm
    let new_val = 0x00C0FFEE;
    let write_res = mut_ptr.write_vm(new_val);
    assert!(write_res.is_ok());
    assert_eq!(val, 0x00C0FFEE);
}

#[def_test]
fn test_load_vec() {
    let data: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];
    let ptr = data.as_ptr();

    let res = load_vec(ptr, 4);
    assert!(res.is_ok());
    let vec = res.unwrap();
    assert_eq!(vec.len(), 4);
    assert_eq!(vec, Vec::from([1, 2, 3, 4]));
}

#[def_test]
fn test_load_vec_until_null() {
    // 1, 2, 3, 0, 5
    let data: [u8; 5] = [1, 2, 3, 0, 5];
    let ptr = data.as_ptr();

    let res = load_vec_until_null(ptr);
    assert!(res.is_ok());
    let vec = res.unwrap();
    assert_eq!(vec.len(), 3);
    assert_eq!(vec, Vec::from([1, 2, 3]));
}

#[def_test]
fn test_invalid_alignment_checks() {
    // read_vm_mem checks is_aligned
    let data: [u64; 2] = [0, 0];
    let ptr = data.as_ptr();
    // Unaligned pointer + 1 byte
    let unaligned_ptr = unsafe { (ptr as *const u8).add(1) as *const u64 };

    let mut out = MaybeUninit::<u64>::uninit();
    let res = read_vm_mem(unaligned_ptr, core::slice::from_mut(&mut out));
    assert!(res.is_err());
    assert_eq!(res.unwrap_err(), MemError::InvalidAddr);
}
