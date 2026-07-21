// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! SysV shared memory management.

use alloc::{collections::btree_map::BTreeMap, format, sync::Arc, vec::Vec};

use filemap::{FileMmapRequest, mmap_shared_file};
use kcred::Cred;
use kerrno::{KError, KResult};
use khal::{
    paging::{MappingFlags, PageSize},
    time::monotonic_time_nanos,
};
use kprocess::{Pid, current_user_process, current_user_thread};
use ksync::{Mutex, static_lock};
use kvfs::VfsFile;
use linux_raw_sys::general::*;
use memaddr::{PAGE_SIZE_4K, VirtAddr, VirtAddrRange};
use memfs::shmem::create_kernel_file;
use osvm::VirtPtr;
use posix_types::{IpcPerm, UserPtr, shmid_ds};

use super::{IPC_PRIVATE, IPC_RMID, IPC_SET, IPC_STAT, next_ipc_id};

fn new_shmid_ds(
    key: i32,
    size: usize,
    mode: __kernel_mode_t,
    pid: __kernel_pid_t,
    cred: &Cred,
) -> shmid_ds {
    shmid_ds {
        shm_perm: IpcPerm {
            key,
            uid: cred.euid(),
            gid: cred.egid(),
            cuid: cred.euid(),
            cgid: cred.egid(),
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
    /// Multiple attachments per PID are allowed, one VMA per `shmat`.
    va_range: BTreeMap<Pid, Vec<VirtAddrRange>>,
    pub file: Arc<VfsFile>,
    pub rmid: bool,
    pub mapping_flags: MappingFlags,
    pub shmid_ds: shmid_ds,
}

impl ShmInner {
    /// Creates a SysV shared-memory segment using the caller's credential snapshot.
    pub fn new(
        key: i32,
        shmid: i32,
        size: usize,
        mapping_flags: MappingFlags,
        pid: Pid,
        cred: Arc<Cred>,
    ) -> KResult<Self> {
        let page_num = memaddr::align_up_4k(size) / PAGE_SIZE_4K;
        let shm_obj = create_kernel_file(
            &format!("SYSV{shmid:x}"),
            kvfs::NodePermission::from_bits_truncate(0o600),
            cred.clone(),
        )?;
        // Unlink the backing file from tmpfs so the inode's lifetime is
        // tied solely to the returned VfsFile; when the last reference
        // drops the page cache is freed.
        shm_obj
            .location()
            .mount()
            .root_path()
            .unlink(&shm_obj.location().name(), &cred)?;
        let file = shm_obj.into_file(cred.clone())?;
        file.truncate((page_num * PAGE_SIZE_4K) as u64)?;

        Ok(ShmInner {
            shmid,
            page_num,
            va_range: BTreeMap::new(),
            file,
            rmid: false,
            mapping_flags,
            shmid_ds: new_shmid_ds(
                key,
                size,
                mapping_flags.bits() as __kernel_mode_t,
                pid as _,
                &cred,
            ),
        })
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

    pub fn attach_count(&self) -> usize {
        self.va_range.values().map(Vec::len).sum()
    }

    pub fn get_addr_range_by_vaddr(&self, pid: Pid, vaddr: VirtAddr) -> Option<VirtAddrRange> {
        self.va_range
            .get(&pid)?
            .iter()
            .find(|range| range.start == vaddr)
            .cloned()
    }

    pub fn ranges_for_pid(&self, pid: Pid) -> Vec<VirtAddrRange> {
        self.va_range.get(&pid).cloned().unwrap_or_default()
    }

    pub fn attach_process(&mut self, pid: Pid, va_range: VirtAddrRange) {
        self.va_range.entry(pid).or_default().push(va_range);
        self.shmid_ds.shm_nattch += 1;
        self.shmid_ds.shm_lpid = pid as __kernel_pid_t;
        self.shmid_ds.shm_atime = monotonic_time_nanos() as __kernel_time_t;
    }

    pub fn detach_all_for_pid(&mut self, pid: Pid) {
        if let Some(ranges) = self.va_range.remove(&pid) {
            self.shmid_ds.shm_nattch -= ranges.len() as u16;
            self.shmid_ds.shm_lpid = pid as __kernel_pid_t;
            self.shmid_ds.shm_dtime = monotonic_time_nanos() as __kernel_time_t;
        }
    }

    pub fn detach_process_by_vaddr(&mut self, pid: Pid, vaddr: VirtAddr) {
        if let Some(ranges) = self.va_range.get_mut(&pid)
            && let Some(pos) = ranges.iter().position(|range| range.start == vaddr)
        {
            ranges.remove(pos);
            if ranges.is_empty() {
                self.va_range.remove(&pid);
            }
            self.shmid_ds.shm_nattch -= 1;
            self.shmid_ds.shm_lpid = pid as __kernel_pid_t;
            self.shmid_ds.shm_dtime = monotonic_time_nanos() as __kernel_time_t;
        }
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
    pid_shmid_vaddr: BTreeMap<Pid, BTreeMap<VirtAddr, i32>>,
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
            .and_then(|map| map.get(&vaddr))
            .cloned()
    }

    fn get_shmids_by_pid(&self, pid: Pid) -> Option<Vec<i32>> {
        let map = self.pid_shmid_vaddr.get(&pid)?;
        let mut res: Vec<i32> = map.values().copied().collect();
        res.sort();
        res.dedup();
        Some(res)
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
            .insert(vaddr, shmid);
    }

    pub fn remove_shmaddr(&mut self, pid: Pid, shmaddr: VirtAddr) {
        let mut empty: bool = false;
        if let Some(map) = self.pid_shmid_vaddr.get_mut(&pid) {
            map.remove(&shmaddr);
            empty = map.is_empty();
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
        // SHM_MANAGER is held throughout (&mut self).  Lock order is
        // SHM_MANAGER → ShmInner, matching sys_shmget and avoiding
        // the AB/BA deadlock with sys_shmat/sys_shmdt.
        //
        // Collect ShmInner refs first so the per-segment work below
        // doesn't re-borrow self.shmid_inner while iterating.
        let shmids: Vec<i32> = self.get_shmids_by_pid(pid).into_iter().flatten().collect();
        let inners: Vec<(i32, Arc<Mutex<ShmInner>>)> = shmids
            .iter()
            .filter_map(|&shmid| self.get_inner_by_shmid(shmid).map(|inner| (shmid, inner)))
            .collect();
        let mut to_remove: Vec<i32> = Vec::new();
        for (shmid, inner) in &inners {
            let mut shm_inner = inner.lock();
            shm_inner.detach_all_for_pid(pid);
            // rmid+attach_count checked atomically: SHM_MANAGER is held
            // (via &mut self) so no other thread can call remove_shmid
            // or set rmid via IPC_RMID concurrently.
            if shm_inner.rmid && shm_inner.attach_count() == 0 {
                to_remove.push(*shmid);
            }
        }
        for shmid in to_remove {
            self.remove_shmid(shmid);
        }
        self.remove_pid(pid);
    }
}

static_lock! {
    pub static SHM_MANAGER: Mutex<ShmManager> = Mutex::new(ShmManager::new());
}

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

    let cur_pid = current_user_thread().pid();
    let cred = kprocess::current_cred();
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
        cred,
    )?));
    shm_manager.insert_key_shmid(key, shmid);
    shm_manager.insert_shmid_inner(shmid, shm_inner);

    Ok(shmid as isize)
}

pub fn sys_shmat(shmid: i32, addr: usize, shmflg: u32) -> KResult<isize> {
    let shm_inner_arc = {
        let shm_manager = SHM_MANAGER.lock();
        shm_manager
            .get_inner_by_shmid(shmid)
            .ok_or(KError::InvalidInput)?
    };
    let shm_inner = shm_inner_arc.lock();
    let mut mapping_flags = shm_inner.mapping_flags;
    let shm_flg = ShmAtFlags::from_bits_truncate(shmflg);

    if shm_flg.contains(ShmAtFlags::SHM_RDONLY) {
        mapping_flags.remove(MappingFlags::WRITE);
    }

    let process = current_user_process();
    let pid = process.pid();
    let length = shm_inner.page_num * PAGE_SIZE_4K;
    let file = shm_inner.file.clone();
    drop(shm_inner);

    let aspace_ref = process.address_space()?;
    let (start_addr, va_range) = aspace_ref.with_mapping_owner(|mut mapping| {
        let start_aligned = if addr != 0 {
            if shm_flg.contains(ShmAtFlags::SHM_RND) {
                memaddr::align_down_4k(addr)
            } else if !addr.is_multiple_of(PAGE_SIZE_4K) {
                return Err(KError::InvalidInput);
            } else {
                addr
            }
        } else {
            0
        };
        let aspace_base = mapping.aspace().base();
        let aspace_end = mapping.aspace().end();
        let search_range = VirtAddrRange::new(aspace_base, aspace_end);
        let start_addr = mapping
            .aspace_mut()
            .find_free_area(
                VirtAddr::from(start_aligned),
                length,
                search_range,
                PAGE_SIZE_4K,
            )
            .or_else(|| {
                mapping
                    .aspace_mut()
                    .find_free_area(aspace_base, length, search_range, PAGE_SIZE_4K)
            })
            .ok_or(KError::NoMemory)?;
        let end_addr = VirtAddr::from(start_addr.as_usize() + length);
        let va_range = VirtAddrRange::new(start_addr, end_addr);

        info!(
            "Process {} alloc shm virt addr start: {:#x}, size: {}, mapping_flags: {:#x?}",
            pid,
            start_addr.as_usize(),
            length,
            mapping_flags
        );

        let invalidate = mapping.invalidate_handle();
        let (vma, runtime) = mmap_shared_file(FileMmapRequest {
            start: start_addr,
            length,
            offset: 0,
            page_size: PageSize::Size4K,
            flags: mapping_flags,
            max_flags: mapping_flags,
            file: file.clone(),
            mm_id: mapping.aspace().mm_id(),
            observer: mapping.observer(),
            invalidate,
        })?;
        mapping.aspace_mut().map_runtime_vma(vma, false, runtime)?;
        Ok((start_addr, va_range))
    })?;

    let mut shm_inner = shm_inner_arc.lock();
    shm_inner.attach_process(pid, va_range);
    let shmid_val = shm_inner.shmid;
    drop(shm_inner);

    SHM_MANAGER
        .lock()
        .insert_shmid_vaddr(pid, shmid_val, start_addr);
    Ok(start_addr.as_usize() as isize)
}

pub fn sys_shmctl(shmid: i32, cmd: u32, buf: UserPtr<shmid_ds>) -> KResult<isize> {
    let shm_inner_arc = {
        let shm_manager = SHM_MANAGER.lock();
        shm_manager
            .get_inner_by_shmid(shmid)
            .ok_or(KError::InvalidInput)?
    };
    let mut shm_inner = shm_inner_arc.lock();

    let cmd = cmd as i32;
    if cmd == IPC_SET {
        shm_inner.shmid_ds = buf.read_vm()?;
        shm_inner.shmid_ds.shm_ctime = monotonic_time_nanos() as __kernel_time_t;
    } else if cmd == IPC_STAT {
        if let Some(buf) = buf.check_non_null() {
            buf.write_vm(shm_inner.shmid_ds)?;
        }
    } else if cmd == IPC_RMID {
        shm_inner.rmid = true;
        if shm_inner.attach_count() == 0 {
            drop(shm_inner);
            let mut shm_manager = SHM_MANAGER.lock();
            let shm_inner_recheck = shm_inner_arc.lock();
            if shm_inner_recheck.rmid && shm_inner_recheck.attach_count() == 0 {
                shm_manager.remove_shmid(shmid);
            }
            return Ok(0);
        }
    } else {
        return Err(KError::InvalidInput);
    }

    Ok(0)
}

pub fn sys_shmdt(shmaddr: usize) -> KResult<isize> {
    let shmaddr = VirtAddr::from(shmaddr);

    let process = current_user_process();

    let pid = process.pid();
    let shmid = {
        let shm_manager = SHM_MANAGER.lock();
        shm_manager
            .get_shmid_by_vaddr(pid, shmaddr)
            .ok_or(KError::InvalidInput)?
    };

    let shm_inner_arc = {
        let shm_manager = SHM_MANAGER.lock();
        shm_manager
            .get_inner_by_shmid(shmid)
            .ok_or(KError::InvalidInput)?
    };
    let mut shm_inner = shm_inner_arc.lock();
    let va_range = shm_inner
        .get_addr_range_by_vaddr(pid, shmaddr)
        .ok_or(KError::InvalidInput)?;

    let aspace_ref = process.address_space()?;
    let mut aspace = aspace_ref.lock();
    aspace.unmap(va_range.start, va_range.size())?;
    drop(aspace);

    // Detach under shm_inner, then release to avoid holding it
    // across SHM_MANAGER (sys_shmget locks SHM_MANAGER → ShmInner).
    shm_inner.detach_process_by_vaddr(pid, shmaddr);
    drop(shm_inner);

    let mut shm_manager = SHM_MANAGER.lock();
    shm_manager.remove_shmaddr(pid, shmaddr);

    // Re-validate rmid+attach_count atomically under both locks,
    // in the correct order (SHM_MANAGER → ShmInner).  No TOCTOU:
    // whatever another thread did between drop(shm_inner) and here
    // is reflected in the re-check.
    let shm_inner = shm_inner_arc.lock();
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
    fn test_shmid_ds_uses_effective_credential_ids() {
        let mut cred = Cred::root();
        cred.set_resgid(Some(2001), Some(1001), Some(3001)).unwrap();
        cred.set_resuid(Some(2000), Some(1000), Some(3000)).unwrap();
        let metadata = new_shmid_ds(1, 4096, 0o600, 1, &cred);

        assert_eq!(metadata.shm_perm.uid, 1000);
        assert_eq!(metadata.shm_perm.gid, 1001);
        assert_eq!(metadata.shm_perm.cuid, 1000);
        assert_eq!(metadata.shm_perm.cgid, 1001);
    }

    #[def_test]
    fn test_shminner_attach_detach_and_update() {
        let mut inner =
            ShmInner::new(1, 2, 4096, MappingFlags::READ, 1, kcred::initial_cred()).unwrap();
        let range = VirtAddrRange::try_from(0x1000usize..0x2000usize).unwrap();
        inner.attach_process(1, range);
        assert_eq!(inner.attach_count(), 1);
        assert!(inner.get_addr_range_by_vaddr(1, range.start).is_some());
        assert!(inner.try_update(4096, MappingFlags::READ, 2).is_ok());
        assert!(inner.try_update(8192, MappingFlags::READ, 2).is_err());
        inner.detach_process_by_vaddr(1, range.start);
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
    fn test_shminner_file_backing_and_mode_mismatch() {
        let mut inner = ShmInner::new(
            7,
            8,
            5000,
            MappingFlags::READ | MappingFlags::WRITE,
            9,
            kcred::initial_cred(),
        )
        .unwrap();
        assert_eq!(inner.page_num, 2);
        assert!(
            inner
                .try_update(5000, MappingFlags::READ | MappingFlags::WRITE, 10)
                .is_ok()
        );
        assert!(inner.try_update(5000, MappingFlags::READ, 10).is_err());
        assert_eq!(
            inner.file.path().getattr().unwrap().size,
            2 * PAGE_SIZE_4K as u64
        );
    }

    #[def_test]
    fn test_shm_manager_clear_proc_shm_removes_rmid_segment_after_last_detach() {
        let mut manager = ShmManager::new();
        let shm_inner = Arc::new(Mutex::new(
            ShmInner::new(11, 22, 4096, MappingFlags::READ, 7, kcred::initial_cred()).unwrap(),
        ));
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

    #[def_test]
    fn test_clear_proc_shm_rmid_false_preserves_segment() {
        let mut manager = ShmManager::new();
        let shm_inner = Arc::new(Mutex::new(
            ShmInner::new(11, 33, 4096, MappingFlags::READ, 7, kcred::initial_cred()).unwrap(),
        ));
        let range = VirtAddrRange::try_from(0x3000usize..0x4000usize).unwrap();
        shm_inner.lock().attach_process(7, range);
        // NOTE: rmid stays false.
        manager.insert_key_shmid(11, 33);
        manager.insert_shmid_inner(33, shm_inner.clone());
        manager.insert_shmid_vaddr(7, 33, range.start);

        manager.clear_proc_shm(7);

        // Segment preserved (rmid=false).
        assert!(manager.get_inner_by_shmid(33).is_some());
        // pid mapping cleared.
        assert!(manager.get_shmids_by_pid(7).is_none());
    }

    #[def_test]
    fn test_clear_proc_shm_no_mappings_does_not_panic() {
        let mut manager = ShmManager::new();
        manager.clear_proc_shm(999);
        // Graceful no-op.
    }
}
