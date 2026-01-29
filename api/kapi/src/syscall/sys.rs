use alloc::vec;
use core::ffi::c_char;

use kcore::task::processes;
use kerrno::{KError, KResult};
use kfs::FS_CONTEXT;
use linux_raw_sys::{
    general::{GRND_INSECURE, GRND_NONBLOCK, GRND_RANDOM},
    system::{new_utsname, sysinfo},
};
use osvm::{VirtMutPtr, write_vm_mem};
use platconfig::ARCH;

pub fn sys_getuid() -> KResult<isize> {
    Ok(0)
}

pub fn sys_geteuid() -> KResult<isize> {
    Ok(0)
}

pub fn sys_getgid() -> KResult<isize> {
    Ok(0)
}

pub fn sys_getegid() -> KResult<isize> {
    Ok(0)
}

pub fn sys_setuid(_uid: u32) -> KResult<isize> {
    debug!("sys_setuid <= uid: {_uid}");
    Ok(0)
}

pub fn sys_setgid(_gid: u32) -> KResult<isize> {
    debug!("sys_setgid <= gid: {_gid}");
    Ok(0)
}

pub fn sys_getgroups(size: usize, list: *mut u32) -> KResult<isize> {
    debug!("sys_getgroups <= size: {size}");
    if size < 1 {
        return Err(KError::InvalidInput);
    }
    write_vm_mem(list, &[0])?;
    Ok(1)
}

pub fn sys_setgroups(_size: usize, _list: *const u32) -> KResult<isize> {
    Ok(0)
}

const fn pad_str(info: &str) -> [c_char; 65] {
    let mut data: [c_char; 65] = [0; 65];
    // this needs #![feature(const_copy_from_slice)]
    // data[..info.len()].copy_from_slice(info.as_bytes());
    unsafe {
        core::ptr::copy_nonoverlapping(info.as_ptr().cast(), data.as_mut_ptr(), info.len());
    }
    data
}

const UTSNAME: new_utsname = new_utsname {
    sysname: pad_str("Linux"),
    nodename: pad_str("starry"),
    release: pad_str("10.0.0"),
    version: pad_str("10.0.0"),
    machine: pad_str(ARCH),
    domainname: pad_str("https://github.com/Starry-OS/StarryOS"),
};

pub fn sys_uname(name: *mut new_utsname) -> KResult<isize> {
    name.write_vm(UTSNAME)?;
    Ok(0)
}

pub fn sys_sysinfo(info: *mut sysinfo) -> KResult<isize> {
    // FIXME: Zeroable
    let mut kinfo: sysinfo = unsafe { core::mem::zeroed() };
    kinfo.procs = processes().len() as _;
    kinfo.mem_unit = 1;
    info.write_vm(kinfo)?;
    Ok(0)
}

pub fn sys_syslog(_type: i32, _buf: *mut c_char, _len: usize) -> KResult<isize> {
    Ok(0)
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct GetRandomFlags: u32 {
        const NONBLOCK = GRND_NONBLOCK;
        const RANDOM = GRND_RANDOM;
        const INSECURE = GRND_INSECURE;
    }
}

pub fn sys_getrandom(buf: *mut u8, len: usize, flags: u32) -> KResult<isize> {
    if len == 0 {
        return Ok(0);
    }
    let flags = GetRandomFlags::from_bits_retain(flags);

    debug!("sys_getrandom <= buf: {buf:p}, len: {len}, flags: {flags:?}");

    let path = if flags.contains(GetRandomFlags::RANDOM) {
        "/dev/random"
    } else {
        "/dev/urandom"
    };

    let f = FS_CONTEXT.lock().resolve(path)?;
    let mut kbuf = vec![0; len];
    let len = f.entry().as_file()?.read_at(&mut kbuf, 0)?;

    write_vm_mem(buf, &kbuf)?;

    Ok(len as _)
}

pub fn sys_seccomp(_op: u32, _flags: u32, _args: *const ()) -> KResult<isize> {
    warn!("dummy sys_seccomp");
    Ok(0)
}

#[cfg(target_arch = "riscv64")]
pub fn sys_riscv_flush_icache() -> KResult<isize> {
    riscv::asm::fence_i();
    Ok(0)
}
