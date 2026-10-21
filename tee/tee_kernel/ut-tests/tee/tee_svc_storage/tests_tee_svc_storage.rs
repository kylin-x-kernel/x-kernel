// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{string::String, vec::Vec};
use core::ffi::c_ulong;

use kvfs::{
    DirContext, DirEntrySink, Filename, LookupFlags, LookupIntent, NodeType, dentry_open,
};
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

#[derive(Default)]
struct TestStorageDirSink {
    entries: Vec<(String, NodeType)>,
    next_offset: Option<u64>,
}

impl DirEntrySink for TestStorageDirSink {
    fn accept(&mut self, name: &str, _ino: u64, node_type: NodeType, offset: u64) -> bool {
        if name != "." && name != ".." {
            self.entries.push((name.into(), node_type));
        }
        self.next_offset = Some(offset);
        true
    }
}

fn list_test_storage_entries(path: &str) -> Vec<(String, NodeType)> {
    let cred = kprocess::current_cred();
    let fs_struct = kprocess::current_fs_context();
    let fs = fs_struct.lock();
    let Ok(dir) = Filename::new(path).lookup_at(
        fs.root(),
        fs.pwd(),
        LookupIntent::Open,
        LookupFlags::follow(),
        &cred,
    ) else {
        return Vec::new();
    };

    let mut entries = Vec::new();
    let mut offset = 0;
    loop {
        let mut sink = TestStorageDirSink::default();
        let Ok(file) = dentry_open(dir.clone(), 0, cred.clone()) else {
            break;
        };
        file.set_position(offset);
        let mut ctx = DirContext::new(offset, &mut sink);
        let Ok(read) = file.iterate_dir(&mut ctx) else {
            break;
        };
        entries.extend(sink.entries);
        let Some(next_offset) = sink.next_offset else {
            break;
        };
        if read == 0 || next_offset <= offset {
            break;
        }
        offset = next_offset;
    }
    entries
}

fn join_test_storage_path(parent: &str, name: &str) -> String {
    if parent.ends_with('/') {
        alloc::format!("{parent}{name}")
    } else {
        alloc::format!("{parent}/{name}")
    }
}

fn cleanup_test_storage_contents(path: &str) {
    for (name, node_type) in list_test_storage_entries(path) {
        let path = join_test_storage_path(path, &name);
        if node_type == NodeType::Directory {
            remove_test_storage_tree(&path);
        } else {
            let _ = FileVariant::remove_file(&path);
        }
    }
}

fn remove_test_storage_tree(path: &str) {
    cleanup_test_storage_contents(path);
    let _ = FileVariant::remove_dir(path);
}

fn cleanup_test_storage_root() {
    cleanup_test_storage_contents(CFG_TEE_FS_PARENT_PATH);
    let _ = FileVariant::create_dir(CFG_TEE_FS_PARENT_PATH);
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

#[unittest::def_test(user)]
fn test_syscall_storage_obj_create_type_data() {
    let storage_id = TEE_STORAGE_PRIVATE as c_ulong;
    let object_id = "test_object_create";
    cleanup_test_storage_root();
    let flags = TEE_DATA_FLAG_ACCESS_READ
        | TEE_DATA_FLAG_ACCESS_WRITE
        | TEE_DATA_FLAG_ACCESS_WRITE_META
        | TEE_DATA_FLAG_OVERWRITE;
    let attr = TEE_HANDLE_NULL;
    let data_create = b"test_data";

    let object_id_user = user_buffer_from_bytes(object_id.as_bytes());
    let data_create_user = user_buffer_from_bytes(data_create);
    let obj = TestUserValue::<c_uint>::from_value(0).unwrap();

    let result = syscall_storage_obj_create(
        storage_id,
        object_id_user.as_user_ptr(),
        object_id.len(),
        flags as c_ulong,
        attr as c_ulong,
        data_create_user.as_user_ptr(),
        data_create.len(),
        obj.as_user_ptr(),
    );
    assert!(result.is_ok());

    let obj_id = obj.read() as c_ulong;

    let data_read = TestUserBuffer::new(data_create.len()).unwrap();
    let count = TestUserValue::<u64>::from_value(0).unwrap();
    let result = syscall_storage_obj_read(
        obj_id,
        data_read.as_user_ptr(),
        data_create.len(),
        count.as_user_ptr(),
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
    let count = TestUserValue::<u64>::from_value(0).unwrap();
    let result = syscall_storage_obj_read(
        obj_id,
        data_read.as_user_ptr(),
        data_write.len(),
        count.as_user_ptr(),
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
    let count = TestUserValue::<u64>::from_value(0).unwrap();
    let result = syscall_storage_obj_read(
        obj_id,
        data_read.as_user_ptr(),
        data_create.len(),
        count.as_user_ptr(),
    );
    assert!(result.is_ok());
    let data_read_back = data_read.read_bytes(data_create.len()).unwrap();
    assert_eq!(data_read_back.as_slice(), data_create);
    assert_eq!(count.read(), data_create.len() as u64);

    let data_read = TestUserBuffer::new(1).unwrap();
    let count = TestUserValue::<u64>::from_value(0).unwrap();
    let _result =
        syscall_storage_obj_read(obj_id, data_read.as_user_ptr(), 1, count.as_user_ptr());
    assert_eq!(count.read(), 0);

    let result = syscall_storage_obj_seek(
        obj_id,
        (data_create.len() + 1) as i32,
        TEE_DATA_SEEK_SET as c_ulong,
    );
    assert!(result.is_ok());

    let info =
        TestUserValue::<utee_object_info>::from_value(utee_object_info::default()).unwrap();
    let result = syscall_cryp_obj_get_info(obj_id, info.as_user_ptr());
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

#[unittest::def_test(user)]
fn test_syscall_storage_obj_create_rejects_invalid_parameters() {
    let object_id = user_buffer_from_bytes(b"invalid_create");
    let obj = TestUserValue::<c_uint>::from_value(0).unwrap();

    let result = syscall_storage_obj_create(
        TEE_STORAGE_PRIVATE as c_ulong,
        object_id.as_user_ptr(),
        "invalid_create".len(),
        1u64 << 63,
        TEE_HANDLE_NULL as c_ulong,
        core::ptr::null_mut(),
        0,
        obj.as_user_ptr(),
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
        obj.as_user_ptr(),
    );
    assert_eq!(result.err(), Some(TEE_ERROR_BAD_PARAMETERS));
}

#[unittest::def_test(user)]
fn test_syscall_storage_obj_open() {
    let storage_id = TEE_STORAGE_PRIVATE as c_ulong;
    let object_id = "test_object";
    cleanup_test_storage_root();
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
    let created_obj = TestUserValue::<c_uint>::from_value(0).unwrap();
    let result = syscall_storage_obj_create(
        storage_id,
        object_id_create_user.as_user_ptr(),
        object_id.len(),
        create_flags as c_ulong,
        TEE_HANDLE_NULL as c_ulong,
        data_create_user.as_user_ptr(),
        data_create.len(),
        created_obj.as_user_ptr(),
    );
    assert!(result.is_ok());

    let created_obj_id = created_obj.read() as c_ulong;
    let result = syscall_cryp_obj_close(created_obj_id);
    assert!(result.is_ok());

    let object_id_user = user_buffer_from_bytes(object_id.as_bytes());
    let obj = TestUserValue::<c_uint>::from_value(0).unwrap();
    let result = syscall_storage_obj_open(
        storage_id,
        object_id_user.as_user_ptr(),
        object_id.len(),
        open_flags as c_ulong,
        obj.as_user_ptr(),
    );
    assert!(result.is_ok());

    let opened_obj_id = obj.read() as c_ulong;
    let result = syscall_storage_obj_del(opened_obj_id);
    assert!(result.is_ok());
}

#[unittest::def_test(user)]
fn test_syscall_storage_obj_open_rejects_invalid_parameters() {
    let object_id = user_buffer_from_bytes(b"missing_object");
    let obj = TestUserValue::<c_uint>::from_value(0).unwrap();

    let result = syscall_storage_obj_open(
        TEE_STORAGE_PRIVATE as c_ulong,
        object_id.as_user_ptr(),
        "missing_object".len(),
        1u64 << 62,
        obj.as_user_ptr(),
    );
    assert_eq!(result.err(), Some(TEE_ERROR_BAD_PARAMETERS));

    let result = syscall_storage_obj_open(
        TEE_STORAGE_PRIVATE as c_ulong,
        object_id.as_user_ptr(),
        "missing_object".len(),
        TEE_DATA_FLAG_ACCESS_READ as c_ulong,
        obj.as_user_ptr(),
    );
    assert!(result.is_err());
    assert_ne!(result.unwrap_err(), TEE_ERROR_BAD_PARAMETERS);
}

#[unittest::def_test(user)]
fn test_syscall_storage_obj_seek_rejects_invalid_whence() {
    let storage_id = TEE_STORAGE_PRIVATE as c_ulong;
    let object_id = "seek_invalid_whence";
    cleanup_test_storage_root();
    let data_create = b"seek_data";
    let object_id_user = user_buffer_from_bytes(object_id.as_bytes());
    let data_create_user = user_buffer_from_bytes(data_create);
    let obj = TestUserValue::<c_uint>::from_value(0).unwrap();

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
        obj.as_user_ptr(),
    );
    assert!(result.is_ok());

    let obj_id = obj.read() as c_ulong;
    let result = syscall_storage_obj_seek(obj_id, 0, 0xFFFF_FFFF);
    assert_eq!(result.err(), Some(TEE_ERROR_BAD_PARAMETERS));

    let result = syscall_storage_obj_del(obj_id);
    assert!(result.is_ok());
}
