// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{string::String, sync::Arc, vec, vec::Vec};
use core::{
    ffi::c_char,
    mem::{offset_of, size_of},
};

#[cfg(feature = "cgroup2")]
use bpffs::device;
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
const BPF_PROG_ATTACH: u32 = 8;
const BPF_PROG_DETACH: u32 = 9;
const BPF_PROG_GET_NEXT_ID: u32 = 11;
const BPF_PROG_GET_FD_BY_ID: u32 = 13;
const BPF_PROG_QUERY: u32 = 16;
const BPF_LINK_CREATE: u32 = 28;

/// `BPF_PROG_TYPE_CGROUP_DEVICE` (`enum bpf_prog_type`, value 15).
const BPF_PROG_TYPE_CGROUP_DEVICE: u32 = 15;
#[cfg(feature = "cgroup2")]
const BPF_CGROUP_DEVICE_ATTACH_TYPE: u32 = 6;
#[cfg(feature = "cgroup2")]
const BPF_F_ALLOW_OVERRIDE: u32 = 1 << 0;
#[cfg(feature = "cgroup2")]
const BPF_F_ALLOW_MULTI: u32 = 1 << 1;
#[cfg(feature = "cgroup2")]
const BPF_F_QUERY_EFFECTIVE: u32 = 1 << 0;

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
    // Every implemented command sits behind Linux's `bpf_capable()` gate;
    // this kernel has no unprivileged-bpf configuration, so the check is
    // uniform at the entry.
    if !kprocess::current_cred().has_sys_admin() {
        return Err(KError::OperationNotPermitted);
    }
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
        #[cfg(feature = "cgroup2")]
        BPF_PROG_ATTACH => bpf_prog_attach(attr_ptr, size).map(|()| 0),
        #[cfg(feature = "cgroup2")]
        BPF_PROG_DETACH => bpf_prog_detach(attr_ptr, size).map(|()| 0),
        #[cfg(feature = "cgroup2")]
        BPF_LINK_CREATE => bpf_link_create(attr_ptr, size),
        #[cfg(feature = "cgroup2")]
        BPF_PROG_QUERY => bpf_prog_query(attr_ptr, size).map(|()| 0),
        BPF_PROG_GET_NEXT_ID => bpf_prog_get_next_id(attr_ptr, size),
        BPF_PROG_GET_FD_BY_ID => bpf_prog_get_fd_by_id(attr_ptr, size),
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

    let fd = kprocess::current_files().add_file(
        BpfMap::new_file(
            attr.map_type,
            attr.key_size,
            attr.value_size,
            attr.max_entries,
            attr.map_flags,
            kprocess::current_cred(),
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

    let map = kprocess::current_files().get_file_private::<BpfMap>(attr.map_fd as _)?;
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

    let map = kprocess::current_files().get_file_private::<BpfMap>(attr.map_fd as _)?;
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
    let map = kprocess::current_files().get_file_private::<BpfMap>(attr.map_fd as _)?;
    map.freeze();
    Ok(0)
}

/// `BPF_PROG_LOAD`: copies the instruction bytes and publishes a program
/// object. `BPF_PROG_TYPE_CGROUP_DEVICE` programs skip the map-value binding
/// step (device programs carry no `BPF_PSEUDO_MAP_VALUE` references).
///
/// Requires a current process thread: instructions and license are copied
/// from user memory and the program fd installs into the current files
/// table; the copies may fault and sleep. Never call from IRQ context.
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
    if prefix.prog_type == BPF_PROG_TYPE_CGROUP_DEVICE {
        // The device controller's programs run inside the kernel on every
        // device access; their instructions are interpreted, so map-value
        // binding (load-time only) does not apply.
        let arc_insns: Arc<[u8]> = insns.into_boxed_slice().into();
        let fd = kprocess::current_files().add_file(
            BpfProgram::new_file(
                arc_insns,
                prefix.prog_type,
                Arc::from(Vec::new()),
                kprocess::current_cred(),
            )?,
            true,
        )?;
        return Ok(fd as isize);
    }

    let map_values = bind_program_map_values(&insns)?;

    let arc_insns: Arc<[u8]> = insns.into_boxed_slice().into();
    let fd = kprocess::current_files().add_file(
        BpfProgram::new_file(
            arc_insns,
            prefix.prog_type,
            map_values,
            kprocess::current_cred(),
        )?,
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
    let file = kprocess::current_files().get_file(attr.bpf_fd as _)?;
    let program = BpfProgram::from_file(&file)?;

    let fs_struct = kprocess::current_fs_context();
    let fs = fs_struct.lock();
    let cred = kprocess::current_cred();
    let (parent, name) = Filename::new(pathname.as_str())
        .parent_at(fs.root(), fs.pwd(), LookupIntent::Open, &cred)?
        .into_normal()?;
    bpffs::pin_program(&parent, name.as_str(), program, &cred)?;
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
    let cred = kprocess::current_cred();
    let location = Filename::new(pathname.as_str()).lookup_at(
        fs.root(),
        fs.pwd(),
        LookupIntent::Open,
        LookupFlags::follow(),
        &cred,
    )?;
    let program = bpffs::program_from_location(&location)?;
    let fd = kprocess::current_files().add_file(program.into_file(cred)?, true)?;
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
    let program = kprocess::current_files().get_file_private::<BpfProgram>(attr.prog_fd as _)?;
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

    let program = kprocess::current_files().get_file_private::<BpfProgram>(attr.bpf_fd as _)?;
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

        let map = kprocess::current_files().get_file_private::<BpfMap>(map_fd as _)?;
        values.push(BpfProgramMapValue::new(map_id, map.snapshot_values()));
    }

    Ok(values.into_boxed_slice().into())
}

#[cfg(feature = "cgroup2")]
/// `union bpf_attr` prefixes for the cgroup-device controller commands
/// (Linux `include/uapi/linux/bpf.h`). `BpfCgroupAttr` serves both
/// `BPF_PROG_ATTACH` and `BPF_PROG_DETACH`; `relative_fd` is the uapi's
/// `relative_fd`/`relative_id` union member.
#[repr(C)]
#[derive(Clone, Copy, UserRead)]
struct BpfCgroupAttr {
    target_fd: i32,
    attach_bpf_fd: i32,
    attach_type: u32,
    attach_flags: u32,
    replace_bpf_fd: i32,
    relative_fd: u32,
    expected_revision: u64,
}
#[cfg(feature = "cgroup2")]
const _: () = assert!(size_of::<BpfCgroupAttr>() == 32);

#[repr(C)]
#[derive(Clone, Copy, UserRead)]
struct BpfLinkCreateAttr {
    prog_fd: i32,
    target_fd: i32,
    attach_type: u32,
    flags: u32,
    /// `attr->link_create.cgroup.relative_fd`: anchor ordering, unmodelled.
    relative_fd: u32,
}
const _: () = assert!(size_of::<BpfLinkCreateAttr>() == 20);

/// `BPF_PROG_QUERY` layout through `query.revision` (Linux's per-cgroup
/// attach revision counter, written as 0). The skipped per-program flag
/// outputs (`prog_attach_flags`, `link_ids`, `link_attach_flags`) keep their
/// offsets so a caller declaring them still gets `revision` at the right
/// address.
#[repr(C)]
#[derive(Clone, Copy, UserRead)]
struct BpfProgQueryAttr {
    target_fd: i32,
    attach_type: u32,
    query_flags: u32,
    /// Output: the attachment's flags (0 for effective queries).
    attach_flags: u32,
    prog_ids: u64,
    /// Input: caller capacity. Output: total count.
    prog_cnt: u32,
    _type_id: u32,
    _prog_attach_flags: u64,
    _link_ids: u64,
    _link_attach_flags: u64,
    /// Output: the cgroup's attach revision (0; revisions are not counted).
    revision: u64,
}
const _: () = assert!(size_of::<BpfProgQueryAttr>() == 64);

/// Copies a `bpf(2)` attribute out of user memory with Linux's bounded
/// semantics: only `min(size, size_of::<T>())` bytes are read and the
/// remaining fields keep their zero value
/// (`copy_from_user(&attr, uattr, min(size, sizeof(attr)))` in
/// `kernel/bpf/syscall.c`). Trailing-garbage rejection (`CHECK_ATTR`) is not
/// implemented: shorter-than-declared or larger-than-modelled inputs are
/// accepted, never misread.
fn read_attr_bounded<T: Copy>(attr_ptr: usize, size: u32) -> KResult<T> {
    let total = size_of::<T>();
    let declared = (size as usize).min(total);
    let mut buf = vec![0u8; total];
    if declared > 0 {
        let copied = UserConstPtr::<u8>::from(attr_ptr).load_vm_vec(declared)?;
        buf[..declared].copy_from_slice(&copied);
    }
    // SAFETY: `T` is a `repr(C)` aggregate of plain integers, so any bit
    // pattern is a valid value and `buf` is exactly `size_of::<T>()` long.
    Ok(unsafe { core::ptr::read_unaligned(buf.as_ptr().cast::<T>()) })
}

#[cfg(feature = "cgroup2")]
fn is_cgroup_device_attach_type(attach_type: u32) -> bool {
    attach_type == BPF_CGROUP_DEVICE_ATTACH_TYPE
}

#[cfg(feature = "cgroup2")]
/// Resolves a cgroup2 descriptor to its cgroup object.
///
/// Requires a current process thread: the descriptor is looked up in the
/// current files table, which may lock and sleep; never call from IRQ
/// context or with kernel-internal spinlocks held beyond file-table ones.
fn cgroup_for_fd(fd: i32) -> KResult<Arc<kcgroup::Cgroup>> {
    let file = kprocess::current_files().get_file(fd)?;
    cgroup2fs::cgroup_from_file(&file).ok_or(KError::BadFileDescriptor)
}

#[cfg(feature = "cgroup2")]
/// Resolves a descriptor to a device program — Linux's
/// `bpf_prog_get_type(fd, BPF_PROG_TYPE_CGROUP_DEVICE)`: a descriptor for a
/// file that is not a program, or a program of any other type, is
/// `EINVAL`/unresolvable so callers fall back to the NULL-program paths.
///
/// Requires a current process thread (current files table lookup, may sleep).
fn program_from_fd_typed(fd: i32) -> KResult<Arc<BpfProgram>> {
    let file = kprocess::current_files().get_file(fd)?;
    // Accepts both an anonymous program fd and a bpffs-pinned one (the
    // pinned file's private data is the location node, which wraps the
    // program).
    let program = BpfProgram::from_file(&file)
        .or_else(|_| bpffs::program_from_file(&file).ok_or(KError::BadFileDescriptor))?;
    if program.prog_type() != BPF_PROG_TYPE_CGROUP_DEVICE {
        return Err(KError::InvalidInput);
    }
    Ok(program)
}

#[cfg(feature = "cgroup2")]
/// `BPF_PROG_ATTACH`: attaches a loaded device program to a cgroup.
///
/// `attach_flags` selects the attachment mode: `0` (NONE) and
/// `BPF_F_ALLOW_OVERRIDE` replace whatever the cgroup carried,
/// `BPF_F_ALLOW_MULTI` appends. The OVERRIDE/MULTI combination and any other
/// flags are `EINVAL`; `BPF_F_REPLACE`-style replacement (`replace_bpf_fd`)
/// and the `relative_fd`/`expected_revision` ordering inputs are not modelled
/// and are rejected with `EINVAL`.
fn bpf_prog_attach(attr_ptr: usize, size: u32) -> KResult<()> {
    let attr = read_attr_bounded::<BpfCgroupAttr>(attr_ptr, size)?;
    if !is_cgroup_device_attach_type(attr.attach_type) {
        return Err(KError::InvalidInput);
    }
    if attr.replace_bpf_fd != 0 || attr.relative_fd != 0 || attr.expected_revision != 0 {
        return Err(KError::InvalidInput);
    }
    // `__cgroup_bpf_attach` rejects OVERRIDE+MULTI together and
    // REPLACE without MULTI; the three single values are the legal modes.
    if attr.attach_flags & !(BPF_F_ALLOW_OVERRIDE | BPF_F_ALLOW_MULTI) != 0
        || attr.attach_flags == (BPF_F_ALLOW_OVERRIDE | BPF_F_ALLOW_MULTI)
    {
        return Err(KError::InvalidInput);
    }

    let target = cgroup_for_fd(attr.target_fd)?;
    let program = program_from_fd_typed(attr.attach_bpf_fd)?;
    device::attach(&target, &program, attr.attach_flags)
}

#[cfg(feature = "cgroup2")]
/// `BPF_PROG_DETACH`: removes device programs from a cgroup.
///
/// Linux's cgroup branch rejects any `attach_flags` or `relative_fd`, then
/// resolves `attach_bpf_fd`: a device-program fd detaches that program (only
/// a direct attachment matches on a multi-attached cgroup), while an fd that
/// does not resolve acts as the NULL-program detach — legacy removal of the
/// single attachment, `EINVAL` in multi mode (`find_detach_entry`). The
/// revision input is not modelled.
fn bpf_prog_detach(attr_ptr: usize, size: u32) -> KResult<()> {
    let attr = read_attr_bounded::<BpfCgroupAttr>(attr_ptr, size)?;
    if !is_cgroup_device_attach_type(attr.attach_type) {
        return Err(KError::InvalidInput);
    }
    if attr.attach_flags != 0 || attr.relative_fd != 0 {
        return Err(KError::InvalidInput);
    }
    // Linux resolves the target first (EBADF on a bad fd) and only then
    // answers -ESTALE for a revision mismatch. Revisions are never counted
    // here (QUERY always reports 0), so any nonzero `expected_revision` is
    // a mismatch and always stale.
    let target = cgroup_for_fd(attr.target_fd)?;
    if attr.expected_revision != 0 {
        return Err(KError::Stale);
    }
    match program_from_fd_typed(attr.attach_bpf_fd) {
        Ok(program) => device::detach(&target, program.id()),
        Err(_) => device::detach_legacy(&target),
    }
}

#[cfg(feature = "cgroup2")]
/// `BPF_LINK_CREATE`: creates a link whose lifetime owns the attachment.
///
/// Requires a current process thread: the cgroup and program fds resolve
/// through the current files table, the link fd installs into it, and the
/// anonymous-inode allocation may sleep; never call from IRQ context or with
/// unrelated kernel locks held.
///
/// The link file owns its attachment: if the fd install fails, the dropped
/// file's release detaches exactly this link's entry by identity, and the
/// original error (for example `EMFILE`) reaches user space unchanged.
fn bpf_link_create(attr_ptr: usize, size: u32) -> KResult<isize> {
    let attr = read_attr_bounded::<BpfLinkCreateAttr>(attr_ptr, size)?;
    if !is_cgroup_device_attach_type(attr.attach_type) {
        return Err(KError::InvalidInput);
    }
    // Linux rejects anchor/insertion flags for cgroup links
    // (`cgroup_bpf_link_create`'s mask check, and `relative_fd` requires
    // `BPF_F_LINK`); ordering is not modelled.
    if attr.flags != 0 || attr.relative_fd != 0 {
        return Err(KError::InvalidInput);
    }

    let target = cgroup_for_fd(attr.target_fd)?;
    let program = program_from_fd_typed(attr.prog_fd)?;

    let file = device::link_file(&target, &program, kprocess::current_cred())?;
    let fd = kprocess::current_files().add_file(file, true)?;
    Ok(fd as isize)
}

#[cfg(feature = "cgroup2")]
/// `BPF_PROG_QUERY`: reports attached (or effective) program ids.
///
/// Mirrors `__cgroup_bpf_query`: `attach_flags` (the recorded mode for a
/// non-effective query, 0 for an effective one) and `revision` are always
/// written back, then `prog_cnt` receives the total. Queries with no capacity, no ids buffer,
/// or an empty result end there (the count-only path libbpf uses); otherwise
/// up to `prog_cnt` ids are copied and a shortfall answers `ENOSPC`.
fn bpf_prog_query(attr_ptr: usize, size: u32) -> KResult<()> {
    let attr = read_attr_bounded::<BpfProgQueryAttr>(attr_ptr, size)?;
    if !is_cgroup_device_attach_type(attr.attach_type) {
        return Err(KError::InvalidInput);
    }
    let query_flags = attr.query_flags;
    if query_flags & !BPF_F_QUERY_EFFECTIVE != 0 {
        return Err(KError::InvalidInput);
    }

    let target = cgroup_for_fd(attr.target_fd)?;
    // One controller-lock acquisition for the whole answer: flags and ids
    // describe a single attachment state, not two.
    let (ids, attach_flags) = device::query(&target, query_flags & BPF_F_QUERY_EFFECTIVE != 0);

    // Linux writes the outputs into the user union regardless of the
    // declared size; `revision` is only produced when the caller declared
    // through the end of it.
    UserPtr::<u32>::from(attr_ptr + offset_of!(BpfProgQueryAttr, attach_flags))
        .write_vm(attach_flags)?;
    UserPtr::<u32>::from(attr_ptr + offset_of!(BpfProgQueryAttr, prog_cnt))
        .write_vm(ids.len() as u32)?;
    if (size as usize) >= size_of::<BpfProgQueryAttr>() {
        UserPtr::<u64>::from(attr_ptr + offset_of!(BpfProgQueryAttr, revision)).write_vm(0)?;
    }
    if attr.prog_cnt == 0 || attr.prog_ids == 0 || ids.is_empty() {
        return Ok(());
    }

    let capacity = attr.prog_cnt as usize;
    let written = ids.len().min(capacity);
    if written > 0 {
        UserPtr::<u32>::from(attr.prog_ids as usize).write_vm_slice(&ids[..written])?;
    }
    if ids.len() > capacity {
        return Err(KError::StorageFull);
    }
    Ok(())
}

/// `BPF_PROG_GET_NEXT_ID`: enumerates live program ids.
fn bpf_prog_get_next_id(attr_ptr: usize, size: u32) -> KResult<isize> {
    let start = read_attr_bounded::<u32>(attr_ptr, size)?;
    if start >= i32::MAX as u32 {
        return Err(KError::InvalidInput);
    }
    let next = BpfProgram::next_id_after(start).ok_or(KError::NotFound)?;
    UserPtr::<u32>::from(attr_ptr + size_of::<u32>()).write_vm(next)?;
    Ok(0)
}

/// `BPF_PROG_GET_FD_BY_ID`: mints a new descriptor for a live program.
///
/// Requires a current process thread: the new descriptor installs into the
/// current files table (may sleep); never call from IRQ context.
fn bpf_prog_get_fd_by_id(attr_ptr: usize, size: u32) -> KResult<isize> {
    let id = read_attr_bounded::<u32>(attr_ptr, size)?;
    let program = BpfProgram::from_id(id)?;
    let fd = kprocess::current_files()
        .add_file(program.clone().into_file(kprocess::current_cred())?, true)?;
    Ok(fd as isize)
}
