// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

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
