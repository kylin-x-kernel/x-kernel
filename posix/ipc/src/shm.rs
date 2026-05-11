// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared memory management.

use alloc::{collections::btree_map::BTreeMap, sync::Arc, vec::Vec};

use kerrno::{KError, KResult};
use khal::{
    paging::{MappingFlags, PageSize},
    time::monotonic_time_nanos,
};
use kprocess::Pid;
use ksync::Mutex;
use kthread::current_process_state;
use linux_raw_sys::general::*;
use memaddr::{PAGE_SIZE_4K, VirtAddr, VirtAddrRange};
use memspace::backend::{Backend, SharedPages};
use osvm::VirtPtr;
use posix_types::{IpcPerm, UserPtr, shmid_ds};

use super::{IPC_PRIVATE, IPC_RMID, IPC_SET, IPC_STAT, next_ipc_id};

fn new_shmid_ds(key: i32, size: usize, mode: __kernel_mode_t, pid: __kernel_pid_t) -> shmid_ds {
    shmid_ds {
        shm_perm: IpcPerm {
            key,
            uid: 0,
            gid: 0,
            cuid: 0,
            cgid: 0,
            mode,
            seq: 0,
            pad: 0,
            unused0: 0,
            unused1: 0,
        },
        shm_segsz: size as __kernel_size_t,
        shm_atime: 0,
        shm_dtime: 0,
        shm_ctime: 0,
        shm_cpid: pid,
        shm_lpid: pid,
        shm_nattch: 0,
        abi_pad: [0; 6],
    }
}

/// Internal shared memory segment state.
pub struct ShmInner {
    pub shmid: i32,
    pub page_num: usize,
    va_range: BTreeMap<Pid, VirtAddrRange>,
    pub phys_pages: Option<Arc<SharedPages>>,
    pub rmid: bool,
    pub mapping_flags: MappingFlags,
    pub shmid_ds: shmid_ds,
}

impl ShmInner {
    pub fn new(key: i32, shmid: i32, size: usize, mapping_flags: MappingFlags, pid: Pid) -> Self {
        ShmInner {
            shmid,
            page_num: memaddr::align_up_4k(size) / PAGE_SIZE_4K,
            va_range: BTreeMap::new(),
            phys_pages: None,
            rmid: false,
            mapping_flags,
            shmid_ds: new_shmid_ds(key, size, mapping_flags.bits() as __kernel_mode_t, pid as _),
        }
    }

    pub fn try_update(
        &mut self,
        size: usize,
        mapping_flags: MappingFlags,
        pid: Pid,
    ) -> KResult<isize> {
        if size as __kernel_size_t != self.shmid_ds.shm_segsz
            || mapping_flags.bits() as __kernel_mode_t != self.shmid_ds.shm_perm.mode
        {
            return Err(KError::InvalidInput);
        }
        self.shmid_ds.shm_lpid = pid as i32;
        Ok(self.shmid as isize)
    }

    pub fn map_to_phys(&mut self, phys_pages: Arc<SharedPages>) {
        self.phys_pages = Some(phys_pages);
    }

    pub fn attach_count(&self) -> usize {
        self.va_range.len()
    }

    pub fn get_addr_range(&self, pid: Pid) -> Option<VirtAddrRange> {
        self.va_range.get(&pid).cloned()
    }

    pub fn attach_process(&mut self, pid: Pid, va_range: VirtAddrRange) {
        assert!(self.get_addr_range(pid).is_none());
        self.va_range.insert(pid, va_range);
        self.shmid_ds.shm_nattch += 1;
        self.shmid_ds.shm_lpid = pid as __kernel_pid_t;
        self.shmid_ds.shm_atime = monotonic_time_nanos() as __kernel_time_t;
    }

    pub fn detach_process(&mut self, pid: Pid) {
        assert!(self.get_addr_range(pid).is_some());
        self.va_range.remove(&pid);
        self.shmid_ds.shm_nattch -= 1;
        self.shmid_ds.shm_lpid = pid as __kernel_pid_t;
        self.shmid_ds.shm_dtime = monotonic_time_nanos() as __kernel_time_t;
    }
}

#[derive(Debug, Clone)]
pub struct BiBTreeMap<K, V>
where
    K: Ord + Clone,
    V: Ord + Clone,
{
    forward: BTreeMap<K, V>,
    reverse: BTreeMap<V, K>,
}

impl<K, V> BiBTreeMap<K, V>
where
    K: Ord + Clone,
    V: Ord + Clone,
{
    pub const fn new() -> Self {
        BiBTreeMap {
            forward: BTreeMap::new(),
            reverse: BTreeMap::new(),
        }
    }

    pub fn insert(&mut self, key: K, value: V) {
        if let Some(old_key) = self.reverse.insert(value.clone(), key.clone()) {
            self.forward.remove(&old_key);
        }
        if let Some(old_value) = self.forward.insert(key, value.clone()) {
            self.reverse.remove(&old_value);
        }
    }

    pub fn get_by_key(&self, key: &K) -> Option<&V> {
        self.forward.get(key)
    }

    pub fn get_by_value(&self, value: &V) -> Option<&K> {
        self.reverse.get(value)
    }

    pub fn remove_by_key(&mut self, key: &K) -> Option<V> {
        if let Some(value) = self.forward.remove(key) {
            self.reverse.remove(&value);
            Some(value)
        } else {
            None
        }
    }

    pub fn remove_by_value(&mut self, value: &V) -> Option<K> {
        if let Some(key) = self.reverse.remove(value) {
            self.forward.remove(&key);
            Some(key)
        } else {
            None
        }
    }
}

impl<K, V> Default for BiBTreeMap<K, V>
where
    K: Ord + Clone,
    V: Ord + Clone,
{
    fn default() -> Self {
        Self::new()
    }
}

pub struct ShmManager {
    key_shmid: BiBTreeMap<i32, i32>,
    shmid_inner: BTreeMap<i32, Arc<Mutex<ShmInner>>>,
    pid_shmid_vaddr: BTreeMap<Pid, BiBTreeMap<i32, VirtAddr>>,
}

impl ShmManager {
    const fn new() -> Self {
        ShmManager {
            key_shmid: BiBTreeMap::new(),
            shmid_inner: BTreeMap::new(),
            pid_shmid_vaddr: BTreeMap::new(),
        }
    }

    pub fn get_shmid_by_key(&self, key: i32) -> Option<i32> {
        self.key_shmid.get_by_key(&key).cloned()
    }

    pub fn get_inner_by_shmid(&self, shmid: i32) -> Option<Arc<Mutex<ShmInner>>> {
        self.shmid_inner.get(&shmid).cloned()
    }

    pub fn get_shmid_by_vaddr(&self, pid: Pid, vaddr: VirtAddr) -> Option<i32> {
        self.pid_shmid_vaddr
            .get(&pid)
            .and_then(|map| map.get_by_value(&vaddr))
            .cloned()
    }

    fn get_shmids_by_pid(&self, pid: Pid) -> Option<Vec<i32>> {
        let map = self.pid_shmid_vaddr.get(&pid)?;
        let mut res = Vec::new();
        for key in map.forward.keys() {
            res.push(*key);
        }
        Some(res)
    }

    #[allow(dead_code)]
    fn find_vaddr_by_shmid(&self, pid: Pid, shmid: i32) -> Option<VirtAddr> {
        self.pid_shmid_vaddr
            .get(&pid)
            .and_then(|map| map.get_by_key(&shmid))
            .cloned()
    }

    pub fn insert_key_shmid(&mut self, key: i32, shmid: i32) {
        self.key_shmid.insert(key, shmid);
    }

    pub fn insert_shmid_inner(&mut self, shmid: i32, shm_inner: Arc<Mutex<ShmInner>>) {
        self.shmid_inner.insert(shmid, shm_inner);
    }

    pub fn insert_shmid_vaddr(&mut self, pid: Pid, shmid: i32, vaddr: VirtAddr) {
        self.pid_shmid_vaddr
            .entry(pid)
            .or_default()
            .insert(shmid, vaddr);
    }

    pub fn remove_shmaddr(&mut self, pid: Pid, shmaddr: VirtAddr) {
        let mut empty: bool = false;
        if let Some(map) = self.pid_shmid_vaddr.get_mut(&pid) {
            map.remove_by_value(&shmaddr);
            empty = map.forward.is_empty();
        }
        if empty {
            self.pid_shmid_vaddr.remove(&pid);
        }
    }

    fn remove_pid(&mut self, pid: Pid) {
        self.pid_shmid_vaddr.remove(&pid);
    }

    pub fn remove_shmid(&mut self, shmid: i32) {
        self.key_shmid.remove_by_value(&shmid);
        self.shmid_inner.remove(&shmid);
    }

    pub fn clear_proc_shm(&mut self, pid: Pid) {
        if let Some(shmids) = self.get_shmids_by_pid(pid) {
            for shmid in shmids {
                if let Some(shm_inner) = self.get_inner_by_shmid(shmid) {
                    let mut shm_inner = shm_inner.lock();
                    shm_inner.detach_process(pid);
                    if shm_inner.rmid && shm_inner.attach_count() == 0 {
                        self.remove_shmid(shmid);
                    }
                }
            }
        }
        self.remove_pid(pid);
    }
}

pub static SHM_MANAGER: Mutex<ShmManager> = Mutex::new(ShmManager::new());

bitflags::bitflags! {
    #[derive(Debug)]
    struct ShmAtFlags: u32 {
        const SHM_RDONLY = 0o10000;
        const SHM_RND = 0o20000;
        const SHM_REMAP = 0o40000;
    }
}

pub fn sys_shmget(key: i32, size: usize, shmflg: usize) -> KResult<isize> {
    let page_num = memaddr::align_up_4k(size) / PAGE_SIZE_4K;
    if page_num == 0 {
        return Err(KError::InvalidInput);
    }

    let mut mapping_flags = MappingFlags::from_name("USER").unwrap();
    if shmflg & 0o400 != 0 {
        mapping_flags.insert(MappingFlags::READ);
    }
    if shmflg & 0o200 != 0 {
        mapping_flags.insert(MappingFlags::WRITE);
    }
    if shmflg & 0o100 != 0 {
        mapping_flags.insert(MappingFlags::EXECUTE);
    }

    let cur_pid = kthread::current_thread().pid();
    let mut shm_manager = SHM_MANAGER.lock();

    if key != IPC_PRIVATE
        && let Some(shmid) = shm_manager.get_shmid_by_key(key)
    {
        let shm_inner = shm_manager
            .get_inner_by_shmid(shmid)
            .ok_or(KError::InvalidInput)?;
        let mut shm_inner = shm_inner.lock();
        return shm_inner.try_update(size, mapping_flags, cur_pid);
    }

    let shmid = next_ipc_id();
    let shm_inner = Arc::new(Mutex::new(ShmInner::new(
        key,
        shmid,
        size,
        mapping_flags,
        cur_pid,
    )));
    shm_manager.insert_key_shmid(key, shmid);
    shm_manager.insert_shmid_inner(shmid, shm_inner);

    Ok(shmid as isize)
}

pub fn sys_shmat(shmid: i32, addr: usize, shmflg: u32) -> KResult<isize> {
    let shm_inner = {
        let shm_manager = SHM_MANAGER.lock();
        shm_manager
            .get_inner_by_shmid(shmid)
            .ok_or(KError::InvalidInput)?
    };
    let mut shm_inner = shm_inner.lock();
    let mut mapping_flags = shm_inner.mapping_flags;
    let shm_flg = ShmAtFlags::from_bits_truncate(shmflg);

    if shm_flg.contains(ShmAtFlags::SHM_RDONLY) {
        mapping_flags.remove(MappingFlags::WRITE);
    }

    let proc_state = current_process_state();
    let pid = proc_state.proc.pid();
    let mut aspace = proc_state.address_space().lock();

    let start_aligned = memaddr::align_down_4k(addr);
    let length = shm_inner.page_num * PAGE_SIZE_4K;

    assert!(shm_inner.get_addr_range(pid).is_none());
    let start_addr = aspace
        .find_free_area(
            VirtAddr::from(start_aligned),
            length,
            VirtAddrRange::new(aspace.base(), aspace.end()),
            PAGE_SIZE_4K,
        )
        .or_else(|| {
            aspace.find_free_area(
                aspace.base(),
                length,
                VirtAddrRange::new(aspace.base(), aspace.end()),
                PAGE_SIZE_4K,
            )
        })
        .ok_or(KError::NoMemory)?;
    let end_addr = VirtAddr::from(start_addr.as_usize() + length);
    let va_range = VirtAddrRange::new(start_addr, end_addr);

    let mut shm_manager = SHM_MANAGER.lock();
    shm_manager.insert_shmid_vaddr(pid, shm_inner.shmid, start_addr);
    info!(
        "Process {} alloc shm virt addr start: {:#x}, size: {}, mapping_flags: {:#x?}",
        pid,
        start_addr.as_usize(),
        length,
        mapping_flags
    );

    if let Some(phys_pages) = shm_inner.phys_pages.clone() {
        let backend = Backend::new_shared(start_addr, phys_pages);
        aspace.map(start_addr, length, mapping_flags, false, backend)?;
    } else {
        let pages = Arc::new(SharedPages::new(length, PageSize::Size4K)?);
        let backend = Backend::new_shared(start_addr, pages.clone());
        aspace.map(start_addr, length, mapping_flags, false, backend)?;

        shm_inner.map_to_phys(pages);
    }

    shm_inner.attach_process(pid, va_range);
    Ok(start_addr.as_usize() as isize)
}

pub fn sys_shmctl(shmid: i32, cmd: u32, buf: UserPtr<shmid_ds>) -> KResult<isize> {
    let shm_inner = {
        let shm_manager = SHM_MANAGER.lock();
        shm_manager
            .get_inner_by_shmid(shmid)
            .ok_or(KError::InvalidInput)?
    };
    let mut shm_inner = shm_inner.lock();

    let cmd = cmd as i32;
    if cmd == IPC_SET {
        shm_inner.shmid_ds = buf.read_vm()?;
    } else if cmd == IPC_STAT {
        if let Some(buf) = buf.check_non_null() {
            buf.write_vm(shm_inner.shmid_ds)?;
        }
    } else if cmd == IPC_RMID {
        shm_inner.rmid = true;
    } else {
        return Err(KError::InvalidInput);
    }

    shm_inner.shmid_ds.shm_ctime = monotonic_time_nanos() as __kernel_time_t;
    Ok(0)
}

pub fn sys_shmdt(shmaddr: usize) -> KResult<isize> {
    let shmaddr = VirtAddr::from(shmaddr);

    let proc_state = current_process_state();

    let pid = proc_state.proc.pid();
    let shmid = {
        let shm_manager = SHM_MANAGER.lock();
        shm_manager
            .get_shmid_by_vaddr(pid, shmaddr)
            .ok_or(KError::InvalidInput)?
    };

    let shm_inner = {
        let shm_manager = SHM_MANAGER.lock();
        shm_manager
            .get_inner_by_shmid(shmid)
            .ok_or(KError::InvalidInput)?
    };
    let mut shm_inner = shm_inner.lock();
    let va_range = shm_inner.get_addr_range(pid).ok_or(KError::InvalidInput)?;

    let mut aspace = proc_state.address_space().lock();
    aspace.unmap(va_range.start, va_range.size())?;

    let mut shm_manager = SHM_MANAGER.lock();
    shm_manager.remove_shmaddr(pid, shmaddr);
    shm_inner.detach_process(pid);

    if shm_inner.rmid && shm_inner.attach_count() == 0 {
        shm_manager.remove_shmid(shmid);
    }

    Ok(0)
}

#[cfg(unittest)]
pub mod tests_shm {
    use khal::paging::MappingFlags;
    use memaddr::VirtAddrRange;
    use unittest::def_test;

    use super::*;

    #[def_test]
    fn test_bibtree_insert_lookup() {
        let mut map: BiBTreeMap<u32, u32> = BiBTreeMap::new();
        map.insert(1, 10);
        map.insert(2, 20);
        assert_eq!(map.get_by_key(&1), Some(&10));
        assert_eq!(map.get_by_value(&20), Some(&2));
    }

    #[def_test]
    fn test_bibtree_replace_value() {
        let mut map: BiBTreeMap<u32, u32> = BiBTreeMap::new();
        map.insert(1, 10);
        map.insert(2, 10);
        assert!(map.get_by_key(&1).is_none());
        assert_eq!(map.get_by_key(&2), Some(&10));
        assert_eq!(map.get_by_value(&10), Some(&2));
    }

    #[def_test]
    fn test_shminner_attach_detach_and_update() {
        let mut inner = ShmInner::new(1, 2, 4096, MappingFlags::READ, 1);
        let range = VirtAddrRange::try_from(0x1000usize..0x2000usize).unwrap();
        inner.attach_process(1, range);
        assert_eq!(inner.attach_count(), 1);
        assert!(inner.get_addr_range(1).is_some());
        assert!(inner.try_update(4096, MappingFlags::READ, 2).is_ok());
        assert!(inner.try_update(8192, MappingFlags::READ, 2).is_err());
        inner.detach_process(1);
        assert_eq!(inner.attach_count(), 0);
    }

    #[def_test]
    fn test_bibtree_remove_by_key_and_value() {
        let mut map: BiBTreeMap<u32, u32> = BiBTreeMap::new();
        map.insert(1, 10);
        map.insert(2, 20);

        assert_eq!(map.remove_by_key(&1), Some(10));
        assert_eq!(map.get_by_key(&1), None);
        assert_eq!(map.get_by_value(&10), None);

        assert_eq!(map.remove_by_value(&20), Some(2));
        assert_eq!(map.get_by_key(&2), None);
        assert_eq!(map.get_by_value(&20), None);
        assert_eq!(map.remove_by_key(&99), None);
        assert_eq!(map.remove_by_value(&99), None);
    }

    #[def_test]
    fn test_shminner_map_to_phys_and_mode_mismatch() {
        let mut inner = ShmInner::new(7, 8, 5000, MappingFlags::READ | MappingFlags::WRITE, 9);
        assert_eq!(inner.page_num, 2);
        assert!(inner.phys_pages.is_none());
        assert!(
            inner
                .try_update(5000, MappingFlags::READ | MappingFlags::WRITE, 10)
                .is_ok()
        );
        assert!(inner.try_update(5000, MappingFlags::READ, 10).is_err());
    }

    #[def_test]
    fn test_shm_manager_clear_proc_shm_removes_rmid_segment_after_last_detach() {
        let mut manager = ShmManager::new();
        let shm_inner = Arc::new(Mutex::new(ShmInner::new(
            11,
            22,
            4096,
            MappingFlags::READ,
            7,
        )));
        let range = VirtAddrRange::try_from(0x3000usize..0x4000usize).unwrap();
        shm_inner.lock().attach_process(7, range);
        shm_inner.lock().rmid = true;
        manager.insert_key_shmid(11, 22);
        manager.insert_shmid_inner(22, shm_inner.clone());
        manager.insert_shmid_vaddr(7, 22, range.start);

        manager.clear_proc_shm(7);

        assert!(manager.get_inner_by_shmid(22).is_none());
        assert!(manager.get_shmid_by_key(11).is_none());
    }
}
