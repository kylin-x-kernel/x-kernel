// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{string::String, sync::Arc, vec, vec::Vec};
use core::{ffi::c_char, mem::size_of};

use bpffs::{BpfMap, BpfProgram, BpfProgramMapValue};
use kerrno::{KError, KResult};
use klogger::debug;
use kuaccess::vm_load_string;
use kvfs::{Filename, LookupFlags, LookupIntent};
use posix_types::{UserConstPtr, UserPtr, UserRead};

/// `BPF_MAP_CREATE` (`enum bpf_cmd` in Linux `uapi/linux/bpf.h`).
const BPF_MAP_CREATE: u32 = 0;
/// `BPF_MAP_LOOKUP_ELEM` (`enum bpf_cmd` in Linux `uapi/linux/bpf.h`).
const BPF_MAP_LOOKUP_ELEM: u32 = 1;
/// `BPF_MAP_UPDATE_ELEM` (`enum bpf_cmd` in Linux `uapi/linux/bpf.h`).
const BPF_MAP_UPDATE_ELEM: u32 = 2;
/// `BPF_PROG_LOAD` (`enum bpf_cmd` in Linux `uapi/linux/bpf.h`).
const BPF_PROG_LOAD: u32 = 5;
/// `BPF_OBJ_PIN` (`enum bpf_cmd` in Linux `uapi/linux/bpf.h`).
const BPF_OBJ_PIN: u32 = 6;
/// `BPF_OBJ_GET` (`enum bpf_cmd` in Linux `uapi/linux/bpf.h`).
const BPF_OBJ_GET: u32 = 7;
/// `BPF_PROG_TEST_RUN` (`enum bpf_cmd` in Linux `uapi/linux/bpf.h`).
const BPF_PROG_TEST_RUN: u32 = 10;
/// `BPF_OBJ_GET_INFO_BY_FD` (`enum bpf_cmd` in Linux `uapi/linux/bpf.h`).
const BPF_OBJ_GET_INFO_BY_FD: u32 = 15;
/// `BPF_MAP_FREEZE` (`enum bpf_cmd` in Linux `uapi/linux/bpf.h`).
const BPF_MAP_FREEZE: u32 = 22;

/// `BPF_MAP_TYPE_ARRAY` (`enum bpf_map_type` in Linux `uapi/linux/bpf.h`).
const BPF_MAP_TYPE_ARRAY: u32 = 2;
/// `BPF_ANY` update mode for `BPF_MAP_UPDATE_ELEM`.
const BPF_ANY: u64 = 0;
/// `lddw` opcode (`BPF_LD | BPF_DW | BPF_IMM`).
const BPF_LD_DW_IMM: u8 = 0x18;
/// `lddw` source marker for direct map-value addresses.
const BPF_PSEUDO_MAP_VALUE: u8 = 2;

/// Minimum `size` for `BPF_MAP_CREATE` covering fields through `map_flags`
/// (Linux `union bpf_attr` map_create prefix).
const BPF_MAP_CREATE_MIN_ATTR_SIZE: usize = size_of::<BpfMapCreateAttrPrefix>();

#[repr(C)]
#[derive(Clone, Copy, UserRead)]
struct BpfMapCreateAttrPrefix {
    map_type: u32,
    key_size: u32,
    value_size: u32,
    max_entries: u32,
    map_flags: u32,
}

const _: () = assert!(size_of::<BpfMapCreateAttrPrefix>() == 20);

/// Minimum `size` for `BPF_MAP_UPDATE_ELEM`.
const BPF_MAP_UPDATE_ELEM_MIN_ATTR_SIZE: usize = size_of::<BpfMapElemAttrPrefix>();

#[repr(C)]
#[derive(Clone, Copy, UserRead)]
struct BpfMapElemAttrPrefix {
    map_fd: u32,
    _pad: u32,
    key: u64,
    value: u64,
    flags: u64,
}

const _: () = assert!(size_of::<BpfMapElemAttrPrefix>() == 32);

/// Minimum `size` for `BPF_MAP_LOOKUP_ELEM`.
const BPF_MAP_LOOKUP_ELEM_MIN_ATTR_SIZE: usize = size_of::<BpfMapLookupElemAttrPrefix>();

#[repr(C)]
#[derive(Clone, Copy, UserRead)]
struct BpfMapLookupElemAttrPrefix {
    map_fd: u32,
    _pad: u32,
    key: u64,
    value: u64,
}

const _: () = assert!(size_of::<BpfMapLookupElemAttrPrefix>() == 24);

/// Minimum `size` for `BPF_MAP_FREEZE`.
const BPF_MAP_FREEZE_MIN_ATTR_SIZE: usize = size_of::<BpfMapFreezeAttrPrefix>();

#[repr(C)]
#[derive(Clone, Copy, UserRead)]
struct BpfMapFreezeAttrPrefix {
    map_fd: u32,
}

const _: () = assert!(size_of::<BpfMapFreezeAttrPrefix>() == 4);

/// Minimum `size` for `BPF_PROG_LOAD` covering fields through `prog_flags`
/// (Linux `union bpf_attr` prog_load prefix).
const BPF_PROG_LOAD_MIN_ATTR_SIZE: usize = size_of::<BpfProgLoadAttrPrefix>();

#[repr(C)]
#[derive(Clone, Copy, UserRead)]
struct BpfProgLoadAttrPrefix {
    prog_type: u32,
    insn_cnt: u32,
    insns: u64,
    license: u64,
    log_level: u32,
    log_size: u32,
    log_buf: u64,
    kern_version: u32,
    prog_flags: u32,
}

const _: () = assert!(size_of::<BpfProgLoadAttrPrefix>() == 48);

/// Minimum `size` for `BPF_OBJ_ATTR` covering fields through `file_flags`
/// (Linux `union bpf_attr` obj_attr prefix).
const BPF_OBJ_ATTR_MIN_SIZE: usize = size_of::<BpfObjAttrPrefix>();

#[repr(C)]
#[derive(Clone, Copy, UserRead)]
struct BpfObjAttrPrefix {
    pathname: u64,
    bpf_fd: u32,
    file_flags: u32,
}

const _: () = assert!(size_of::<BpfObjAttrPrefix>() == 16);

/// Minimum `size` for `BPF_PROG_TEST_RUN` covering fields through `duration`.
const BPF_PROG_TEST_RUN_MIN_ATTR_SIZE: usize = size_of::<BpfProgTestRunAttrPrefix>();

#[repr(C)]
#[derive(Clone, Copy, UserRead)]
struct BpfProgTestRunAttrPrefix {
    prog_fd: u32,
    retval: u32,
    data_size_in: u32,
    data_size_out: u32,
    data_in: u64,
    data_out: u64,
    repeat: u32,
    duration: u32,
}

const _: () = assert!(size_of::<BpfProgTestRunAttrPrefix>() == 40);

/// Minimum `size` for `BPF_OBJ_GET_INFO_BY_FD` (`union bpf_attr info` prefix).
const BPF_OBJ_GET_INFO_BY_FD_MIN_ATTR_SIZE: usize = size_of::<BpfObjInfoAttrPrefix>();

#[repr(C)]
#[derive(Clone, Copy, UserRead)]
struct BpfObjInfoAttrPrefix {
    bpf_fd: u32,
    info_len: u32,
    info: u64,
}

const _: () = assert!(size_of::<BpfObjInfoAttrPrefix>() == 16);

const BPF_PROG_INFO_PREFIX_SIZE: usize = 24;
const BPF_PROG_INFO_TYPE_OFFSET: usize = 0;
const BPF_PROG_INFO_XLATED_PROG_LEN_OFFSET: usize = 20;

/// Linux `bpf(cmd, attr, size)` — see `man 2 bpf`.
pub fn sys_bpf(cmd: i32, attr_ptr: usize, size: u32) -> KResult<isize> {
    if cmd < 0 {
        return Err(KError::InvalidInput);
    }
    let cmd_u = cmd as u32;
    match cmd_u {
        BPF_MAP_CREATE => bpf_map_create(attr_ptr, size),
        BPF_MAP_LOOKUP_ELEM => bpf_map_lookup_elem(attr_ptr, size),
        BPF_MAP_UPDATE_ELEM => bpf_map_update_elem(attr_ptr, size),
        BPF_PROG_LOAD => bpf_prog_load(attr_ptr, size),
        BPF_OBJ_PIN => bpf_obj_pin(attr_ptr, size),
        BPF_OBJ_GET => bpf_obj_get(attr_ptr, size),
        BPF_PROG_TEST_RUN => bpf_prog_test_run(attr_ptr, size),
        BPF_OBJ_GET_INFO_BY_FD => bpf_obj_get_info_by_fd(attr_ptr, size),
        BPF_MAP_FREEZE => bpf_map_freeze(attr_ptr, size),
        _ => {
            debug!("sys_bpf: cmd {cmd} not implemented");
            Err(KError::OperationNotSupported)
        }
    }
}

fn bpf_map_create(attr_ptr: usize, size: u32) -> KResult<isize> {
    if attr_ptr == 0 {
        return Err(KError::InvalidInput);
    }
    if (size as usize) < BPF_MAP_CREATE_MIN_ATTR_SIZE {
        return Err(KError::InvalidInput);
    }

    let attr = UserConstPtr::<BpfMapCreateAttrPrefix>::from(attr_ptr).read_vm()?;
    if attr.map_type != BPF_MAP_TYPE_ARRAY
        || attr.key_size != size_of::<u32>() as u32
        || attr.value_size == 0
        || attr.max_entries == 0
    {
        return Err(KError::OperationNotSupported);
    }

    let fd = kprocess::current_resources().add_file(
        BpfMap::new_file(
            attr.map_type,
            attr.key_size,
            attr.value_size,
            attr.max_entries,
            attr.map_flags,
        )?,
        true,
    )?;
    Ok(fd as isize)
}

fn bpf_map_lookup_elem(attr_ptr: usize, size: u32) -> KResult<isize> {
    if attr_ptr == 0 {
        return Err(KError::InvalidInput);
    }
    if (size as usize) < BPF_MAP_LOOKUP_ELEM_MIN_ATTR_SIZE {
        return Err(KError::InvalidInput);
    }

    let attr = UserConstPtr::<BpfMapLookupElemAttrPrefix>::from(attr_ptr).read_vm()?;
    if attr.key == 0 || attr.value == 0 {
        return Err(KError::InvalidInput);
    }

    let map = kprocess::current_resources().get_file_private::<BpfMap>(attr.map_fd as _)?;
    if map.map_type() != BPF_MAP_TYPE_ARRAY || map.key_size() != size_of::<u32>() as u32 {
        return Err(KError::OperationNotSupported);
    }

    let key = UserConstPtr::<u32>::from(attr.key as usize).read_vm()?;
    let mut value = vec![0; map.value_size() as usize];
    map.lookup_elem(key, &mut value)?;
    UserPtr::<u8>::from(attr.value as usize).write_vm_slice(&value)?;
    Ok(0)
}

fn bpf_map_update_elem(attr_ptr: usize, size: u32) -> KResult<isize> {
    if attr_ptr == 0 {
        return Err(KError::InvalidInput);
    }
    if (size as usize) < BPF_MAP_UPDATE_ELEM_MIN_ATTR_SIZE {
        return Err(KError::InvalidInput);
    }

    let attr = UserConstPtr::<BpfMapElemAttrPrefix>::from(attr_ptr).read_vm()?;
    if attr.key == 0 || attr.value == 0 || attr.flags != BPF_ANY {
        return Err(KError::InvalidInput);
    }

    let map = kprocess::current_resources().get_file_private::<BpfMap>(attr.map_fd as _)?;
    if map.map_type() != BPF_MAP_TYPE_ARRAY || map.key_size() != size_of::<u32>() as u32 {
        return Err(KError::OperationNotSupported);
    }

    let key = UserConstPtr::<u32>::from(attr.key as usize).read_vm()?;
    let value = UserConstPtr::<u8>::from(attr.value as usize).load_vm_vec(map.value_size() as _)?;
    map.update_elem(key, &value)?;
    Ok(0)
}

fn bpf_map_freeze(attr_ptr: usize, size: u32) -> KResult<isize> {
    if attr_ptr == 0 {
        return Err(KError::InvalidInput);
    }
    if (size as usize) < BPF_MAP_FREEZE_MIN_ATTR_SIZE {
        return Err(KError::InvalidInput);
    }

    let attr = UserConstPtr::<BpfMapFreezeAttrPrefix>::from(attr_ptr).read_vm()?;
    let map = kprocess::current_resources().get_file_private::<BpfMap>(attr.map_fd as _)?;
    map.freeze();
    Ok(0)
}

fn bpf_prog_load(attr_ptr: usize, size: u32) -> KResult<isize> {
    if attr_ptr == 0 {
        return Err(KError::InvalidInput);
    }
    if (size as usize) < BPF_PROG_LOAD_MIN_ATTR_SIZE {
        return Err(KError::InvalidInput);
    }

    let prefix = UserConstPtr::<BpfProgLoadAttrPrefix>::from(attr_ptr).read_vm()?;

    let prog_bytes = (prefix.insn_cnt as usize)
        .checked_mul(8)
        .ok_or(KError::InvalidInput)?;

    let insns = if prog_bytes == 0 {
        Vec::new()
    } else {
        UserConstPtr::<u8>::from(prefix.insns as usize).load_vm_vec(prog_bytes)?
    };
    let map_values = bind_program_map_values(&insns)?;

    let arc_insns: Arc<[u8]> = insns.into_boxed_slice().into();
    let fd = kprocess::current_resources().add_file(
        BpfProgram::new_file(arc_insns, prefix.prog_type, map_values)?,
        true,
    )?;
    Ok(fd as isize)
}

fn bpf_obj_pin(attr_ptr: usize, size: u32) -> KResult<isize> {
    let attr = read_obj_attr(attr_ptr, size)?;
    if attr.file_flags != 0 {
        return Err(KError::InvalidInput);
    }
    let pathname = load_pathname(attr.pathname)?;
    let file = kprocess::current_resources().get_file(attr.bpf_fd as _)?;
    let program = BpfProgram::from_file(&file)?;

    let fs_struct = kprocess::current_fs_context();
    let fs = fs_struct.lock();
    let (parent, name) = Filename::new(pathname.as_str())
        .parent_at(fs.root(), fs.pwd(), LookupIntent::Open)?
        .into_normal()?;
    bpffs::pin_program(&parent, name.as_str(), program)?;
    Ok(0)
}

fn bpf_obj_get(attr_ptr: usize, size: u32) -> KResult<isize> {
    let attr = read_obj_attr(attr_ptr, size)?;
    if attr.file_flags != 0 {
        return Err(KError::InvalidInput);
    }
    let pathname = load_pathname(attr.pathname)?;

    let fs_struct = kprocess::current_fs_context();
    let fs = fs_struct.lock();
    let location = Filename::new(pathname.as_str()).lookup_at(
        fs.root(),
        fs.pwd(),
        LookupIntent::Open,
        LookupFlags::follow(),
    )?;
    let program = bpffs::program_from_location(&location)?;
    let fd = kprocess::current_resources().add_file(program.into_file()?, true)?;
    Ok(fd as isize)
}

fn bpf_prog_test_run(attr_ptr: usize, size: u32) -> KResult<isize> {
    if attr_ptr == 0 {
        return Err(KError::InvalidInput);
    }
    if (size as usize) < BPF_PROG_TEST_RUN_MIN_ATTR_SIZE {
        return Err(KError::InvalidInput);
    }

    let attr = UserConstPtr::<BpfProgTestRunAttrPrefix>::from(attr_ptr).read_vm()?;
    let program =
        kprocess::current_resources().get_file_private::<BpfProgram>(attr.prog_fd as _)?;
    let mut vm = kbpf::Vm::new();
    for map_value in program.map_values() {
        vm.add_read_only_memory(kbpf::ReadOnlyMemory::map_value(
            map_value.id(),
            map_value.bytes(),
        ));
    }
    let retval = vm.run(program.insns()).map_err(|_| KError::InvalidInput)?;

    UserPtr::<u32>::from(attr_ptr + 4).write_vm(retval as u32)?;
    UserPtr::<u32>::from(attr_ptr + 36).write_vm(0)?;
    Ok(0)
}

fn bpf_obj_get_info_by_fd(attr_ptr: usize, size: u32) -> KResult<isize> {
    if attr_ptr == 0 {
        return Err(KError::InvalidInput);
    }
    if (size as usize) < BPF_OBJ_GET_INFO_BY_FD_MIN_ATTR_SIZE {
        return Err(KError::InvalidInput);
    }

    let attr = UserConstPtr::<BpfObjInfoAttrPrefix>::from(attr_ptr).read_vm()?;
    if attr.info == 0 {
        return Err(KError::InvalidInput);
    }

    let program = kprocess::current_resources().get_file_private::<BpfProgram>(attr.bpf_fd as _)?;
    let info_len = (attr.info_len as usize).min(BPF_PROG_INFO_PREFIX_SIZE);
    let mut info = [0u8; BPF_PROG_INFO_PREFIX_SIZE];
    info[BPF_PROG_INFO_TYPE_OFFSET..BPF_PROG_INFO_TYPE_OFFSET + 4]
        .copy_from_slice(&program.prog_type().to_ne_bytes());
    info[BPF_PROG_INFO_XLATED_PROG_LEN_OFFSET..BPF_PROG_INFO_XLATED_PROG_LEN_OFFSET + 4]
        .copy_from_slice(&(program.insns().len() as u32).to_ne_bytes());

    UserPtr::<u8>::from(attr.info as usize).write_vm_slice(&info[..info_len])?;
    UserPtr::<u32>::from(attr_ptr + 4).write_vm(BPF_PROG_INFO_PREFIX_SIZE as u32)?;
    Ok(0)
}

fn read_obj_attr(attr_ptr: usize, size: u32) -> KResult<BpfObjAttrPrefix> {
    if attr_ptr == 0 {
        return Err(KError::InvalidInput);
    }
    if (size as usize) < BPF_OBJ_ATTR_MIN_SIZE {
        return Err(KError::InvalidInput);
    }

    Ok(UserConstPtr::<BpfObjAttrPrefix>::from(attr_ptr).read_vm()?)
}

fn load_pathname(pathname: u64) -> KResult<String> {
    if pathname == 0 {
        return Err(KError::InvalidInput);
    }
    vm_load_string(pathname as *const c_char)
}

fn bind_program_map_values(insns: &[u8]) -> KResult<Arc<[BpfProgramMapValue]>> {
    if !insns.len().is_multiple_of(kbpf::SLOT_SIZE) {
        return Err(KError::InvalidInput);
    }

    let mut values = Vec::<BpfProgramMapValue>::new();
    let slots = insns.len() / kbpf::SLOT_SIZE;
    for slot in 0..slots {
        let off = slot * kbpf::SLOT_SIZE;
        if insns[off] != BPF_LD_DW_IMM || ((insns[off + 1] >> 4) & 0x0f) != BPF_PSEUDO_MAP_VALUE {
            continue;
        }
        if slot + 1 >= slots {
            return Err(KError::InvalidInput);
        }

        let map_fd = i32::from_le_bytes([
            insns[off + 4],
            insns[off + 5],
            insns[off + 6],
            insns[off + 7],
        ]);
        if map_fd < 0 {
            return Err(KError::InvalidInput);
        }
        let map_id = map_fd as u32;
        if values.iter().any(|value| value.id() == map_id) {
            continue;
        }

        let map = kprocess::current_resources().get_file_private::<BpfMap>(map_fd as _)?;
        values.push(BpfProgramMapValue::new(map_id, map.snapshot_values()));
    }

    Ok(values.into_boxed_slice().into())
}
