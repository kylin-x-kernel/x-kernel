// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use unittest::{TestResult, assert, assert_eq, def_test};

use super::*;
use crate::{FileSystemType, Mount, Path};

static TEST_FILE_SYSTEM_TYPE: FileSystemType = FileSystemType::internal("simple-dir-test");

#[def_test]
fn static_entries_preserve_inode_identity_after_dentry_invalidation() {
    let super_block = SimpleFs::new_with(&TEST_FILE_SYSTEM_TYPE, 0, |fs| {
        let mut entries = DirMapping::new();
        entries.add(
            "file",
            crate::SimpleFile::new_regular(fs.clone(), || Ok("content")),
        );
        entries.add(
            "directory",
            SimpleDir::new_maker(fs.clone(), Arc::new(DirMapping::new())),
        );
        SimpleDir::new_maker(fs, Arc::new(entries))
    })
    .unwrap();
    let root = Mount::new_root(&super_block).root_path();
    let mut inodes = Vec::new();
    for name in ["file", "directory"] {
        let first = root.dentry().lookup(name).unwrap();
        root.dentry().invalidate_cached_child(name).unwrap();
        let second = root.dentry().lookup(name).unwrap();
        assert!(Arc::ptr_eq(&first.inode_ref(), &second.inode_ref()));
        inodes.push(Arc::downgrade(&first.inode_ref()));
    }
    drop(root);
    drop(super_block);
    for inode in inodes {
        assert!(inode.upgrade().is_none());
    }
}

#[def_test]
fn simple_filesystem_rejects_owner_policy_before_building_root() {
    let built = core::cell::Cell::new(false);
    let result = SimpleFs::new_with_allocation_flags(
        &TEST_FILE_SYSTEM_TYPE,
        0,
        crate::SuperBlockFlags::empty(),
        kalloc_types::AllocFlags::from_bits_retain(1 << 31),
        |fs| {
            built.set(true);
            SimpleDir::new_maker(fs, Arc::new(DirMapping::default()))
        },
    );
    assert!(matches!(result, Err(VfsError::InvalidInput)));
    assert!(!built.get());
}

struct NamesOps {
    drops: Arc<AtomicU64>,
    fail: bool,
}

struct NamesIter<'a> {
    drops: Arc<AtomicU64>,
    remaining: core::option::IntoIter<Cow<'a, str>>,
}

impl<'a> Iterator for NamesIter<'a> {
    type Item = Cow<'a, str>;

    fn next(&mut self) -> Option<Self::Item> {
        self.remaining.next()
    }
}

impl Drop for NamesIter<'_> {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::Relaxed);
    }
}

impl SimpleDirOps for NamesOps {
    fn child_names<'a>(&'a self) -> VfsResult<KBox<dyn Iterator<Item = Cow<'a, str>> + 'a>> {
        if self.fail {
            return Err(VfsError::NoMemory);
        }
        Ok(KBox::new(NamesIter {
            drops: self.drops.clone(),
            remaining: Some(Cow::Borrowed("entry")).into_iter(),
        })?)
    }

    fn lookup_child(&self, _lookup: SimpleDirLookup<'_>, _name: &str) -> VfsResult<Dentry> {
        Err(VfsError::NotFound)
    }
}

/// Writable simple-directory ops exercising the opt-in create/link hooks.
struct WritableMapping {
    fs: Arc<SimpleFs>,
    mapping: crate::Mutex<DirMapping>,
}

impl SimpleDirOps for WritableMapping {
    fn child_names<'a>(&'a self) -> VfsResult<KBox<dyn Iterator<Item = Cow<'a, str>> + 'a>> {
        let names: Vec<Cow<'a, str>> = SimpleDirOps::child_names(&*self.mapping.lock())?
            .map(|name| Cow::Owned(name.into_owned()))
            .collect();
        Ok(KBox::new(names.into_iter())?)
    }

    fn lookup_child(&self, lookup: SimpleDirLookup<'_>, name: &str) -> VfsResult<Dentry> {
        SimpleDirOps::lookup_child(&*self.mapping.lock(), lookup, name)
    }

    fn create_inode_child(
        &self,
        lookup: SimpleDirLookup<'_>,
        name: &str,
        inode: Arc<VfsInode>,
    ) -> VfsResult<Dentry> {
        SimpleDirOps::create_inode_child(&*self.mapping.lock(), lookup, name, inode)
    }

    fn create_file(
        &self,
        _dir: &VfsInode,
        _name: &str,
        mode: Umode,
        _uid: u32,
        _gid: u32,
    ) -> VfsResult<Arc<VfsInode>> {
        let file = crate::SimpleFile::new_regular_with_permission(
            self.fs.clone(),
            mode.permission(),
            || Ok("content"),
        );
        Ok(file.new_inode(crate::NodeFlags::empty()))
    }

    fn install_link(
        &self,
        lookup: SimpleDirLookup<'_>,
        target: &Dentry,
        name: &str,
    ) -> VfsResult<()> {
        SimpleDirOps::install_link(&*self.mapping.lock(), lookup, target, name)
    }

    fn unlink(&self, dir: &VfsInode, victim: &LockedDentry<'_>) -> VfsResult<()> {
        SimpleDirOps::unlink(&*self.mapping.lock(), dir, victim)
    }
}

#[def_test]
fn writable_simple_dir_create_link_unlink_tracks_nlink() {
    let cred = kcred::initial_cred();
    let super_block = SimpleFs::new_with(&TEST_FILE_SYSTEM_TYPE, 0, |fs| {
        SimpleDir::new_maker(
            fs.clone(),
            Arc::new(WritableMapping {
                fs,
                mapping: crate::Mutex::new(DirMapping::new()),
            }),
        )
    })
    .unwrap();
    let root = Mount::new_root(&super_block).root_path();
    let lookup = |name: &str| root.with_dentry(root.dentry().lookup(name).unwrap());

    root.create("a", crate::NodePermission::from_bits_truncate(0o644), &cred)
        .unwrap();
    assert_eq!(lookup("a").inode().metadata().nlink, 1);

    let source = lookup("a");
    root.link("b", &source, &cred).unwrap();
    let alias = lookup("b");
    assert_eq!(alias.inode().metadata().nlink, 2);
    assert!(Arc::ptr_eq(
        &source.dentry().inode_ref(),
        &alias.dentry().inode_ref()
    ));

    root.unlink("b", &cred).unwrap();
    assert_eq!(lookup("a").inode().metadata().nlink, 1);
}

#[def_test]
fn chained_names_propagate_failure_and_drop_first_iterator() {
    let drops = Arc::new(AtomicU64::new(0));
    let chained = NamesOps {
        drops: drops.clone(),
        fail: false,
    }
    .chain(NamesOps {
        drops: drops.clone(),
        fail: true,
    });
    assert_eq!(chained.child_names().err(), Some(VfsError::NoMemory));
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}

#[def_test]
fn chained_names_preserve_order_and_release_iterators() {
    let drops = Arc::new(AtomicU64::new(0));
    let chained = NamesOps {
        drops: drops.clone(),
        fail: false,
    }
    .chain(NamesOps {
        drops: drops.clone(),
        fail: false,
    });
    let names: Vec<_> = chained.child_names().unwrap().collect();
    assert_eq!(
        names.as_slice(),
        [Cow::Borrowed("entry"), Cow::Borrowed("entry")]
    );
    assert_eq!(drops.load(Ordering::Relaxed), 2);
}

#[def_test]
fn child_names_failure_does_not_publish_dot_entries() {
    let root = root_path(Arc::new(NamesOps {
        drops: Arc::new(AtomicU64::new(0)),
        fail: true,
    }));
    let file = crate::dentry_open(root, 0, kcred::initial_cred()).unwrap();
    let mut accepted = 0;
    let mut sink = |_: &str, _, _, _| {
        accepted += 1;
        true
    };
    let mut context = DirContext::new(0, &mut sink);
    assert_eq!(file.iterate_dir(&mut context), Err(VfsError::NoMemory));
    assert_eq!(context.pos(), 0);
    assert_eq!(file.position(), 0);
    assert_eq!(accepted, 0);
}

#[def_test]
fn vanished_child_does_not_abort_directory_read() {
    for error in [VfsError::NotFound, VfsError::Io] {
        let super_block = SimpleFs::new_with(&TEST_FILE_SYSTEM_TYPE, 0, |fs| {
            let mut entries = DirMapping::new();
            entries.add_child("gone", move |_, _| Err(error));
            entries.add(
                "live",
                crate::SimpleFile::new_regular(fs.clone(), || Ok("content")),
            );
            SimpleDir::new_maker(fs, Arc::new(entries))
        })
        .unwrap();
        let root = Mount::new_root(&super_block).root_path();
        let file = crate::dentry_open(root, 0, kcred::initial_cred()).unwrap();
        let mut names = Vec::new();
        let mut sink = |name: &str, _, _, cookie| {
            names.push((name.to_owned(), cookie));
            true
        };
        let mut context = DirContext::new(0, &mut sink);
        let result = file.iterate_dir(&mut context);
        if error == VfsError::NotFound {
            assert_eq!(result, Ok(3));
            assert_eq!(context.pos(), 4);
            assert_eq!(file.iterate_dir(&mut context), Ok(0));
            assert_eq!(
                names,
                [
                    (".".to_owned(), 1),
                    ("..".to_owned(), 2),
                    ("live".to_owned(), 4)
                ]
            );
        } else {
            assert_eq!(result, Err(VfsError::Io));
        }
    }
}

struct LinkCountOps {
    value: AtomicU64,
    fail: bool,
}

impl SimpleDirOps for LinkCountOps {
    fn child_names<'a>(&'a self) -> VfsResult<KBox<dyn Iterator<Item = Cow<'a, str>> + 'a>> {
        Ok(KBox::new(core::iter::empty())?)
    }

    fn lookup_child(&self, _lookup: SimpleDirLookup<'_>, _name: &str) -> VfsResult<Dentry> {
        Err(VfsError::NotFound)
    }

    fn link_count(&self) -> VfsResult<Option<u64>> {
        if self.fail {
            Err(VfsError::Io)
        } else {
            Ok(Some(self.value.load(Ordering::Relaxed)))
        }
    }
}

fn root_path<O: SimpleDirOps>(ops: Arc<O>) -> Path {
    let super_block = SimpleFs::new_with(&TEST_FILE_SYSTEM_TYPE, 0, move |fs| {
        SimpleDir::new_maker(fs, ops)
    })
    .unwrap();
    Mount::new_root(&super_block).root_path()
}

fn getattr(path: &Path) -> VfsResult<Metadata> {
    path.getattr_nosec(
        crate::GetattrRequestMask::empty(),
        crate::GetattrQueryFlags::empty(),
    )
}

fn assert_other_metadata_unchanged(actual: &Metadata, cached: &Metadata) -> TestResult {
    assert_eq!(actual.device, cached.device);
    assert_eq!(actual.inode, cached.inode);
    assert_eq!(actual.mode, cached.mode);
    assert_eq!(actual.uid, cached.uid);
    assert_eq!(actual.gid, cached.gid);
    assert_eq!(actual.size, cached.size);
    // getattr uses the node's block_size (0 for this directory), whereas
    // metadata() reconstructs 1 << block_bits (1). Compare only same-source
    // fields here; their pre-existing normalization difference is unrelated
    // to the dynamic link-count callback.
    assert_eq!(actual.blocks, cached.blocks);
    assert_eq!(actual.rdev, cached.rdev);
    assert_eq!(actual.atime, cached.atime);
    assert_eq!(actual.mtime, cached.mtime);
    assert_eq!(actual.ctime, cached.ctime);
    TestResult::Ok
}

#[def_test]
fn dynamic_link_count_changes_only_the_getattr_copy() {
    let ops = Arc::new(LinkCountOps {
        value: AtomicU64::new(7),
        fail: false,
    });
    let root = root_path(ops.clone());
    let cached = root.metadata();
    let first = getattr(&root).unwrap();
    assert_eq!(first.nlink, 7);
    assert!(assert_other_metadata_unchanged(&first, &cached).is_ok());

    ops.value.store(9, Ordering::Relaxed);
    let second = getattr(&root).unwrap();
    assert_eq!(second.nlink, 9);
    assert!(assert_other_metadata_unchanged(&second, &cached).is_ok());
    let after = root.metadata();
    assert_eq!(after.nlink, cached.nlink);
    assert!(assert_other_metadata_unchanged(&after, &cached).is_ok());
}

#[def_test]
fn default_link_count_keeps_the_backing_node_value() {
    let root = root_path(Arc::new(DirMapping::new()));
    let cached = root.metadata();
    let result = getattr(&root).unwrap();
    assert_eq!(result.nlink, cached.nlink);
    assert!(assert_other_metadata_unchanged(&result, &cached).is_ok());
}

#[def_test]
fn link_count_error_propagates_without_changing_cached_metadata() {
    let root = root_path(Arc::new(LinkCountOps {
        value: AtomicU64::new(7),
        fail: true,
    }));
    let cached = root.metadata();
    assert_eq!(getattr(&root).err(), Some(VfsError::Io));
    let after = root.metadata();
    assert_eq!(after.nlink, cached.nlink);
    assert!(assert_other_metadata_unchanged(&after, &cached).is_ok());
}

struct DenyOpenDir(DirMapping);

impl SimpleDirOps for DenyOpenDir {
    fn validate_open(&self) -> VfsResult<()> {
        Err(VfsError::PermissionDenied)
    }

    fn child_names<'a>(&'a self) -> VfsResult<KBox<dyn Iterator<Item = Cow<'a, str>> + 'a>> {
        <DirMapping as SimpleDirOps>::child_names(&self.0)
    }

    fn lookup_child(&self, lookup: SimpleDirLookup<'_>, name: &str) -> VfsResult<Dentry> {
        <DirMapping as SimpleDirOps>::lookup_child(&self.0, lookup, name)
    }
}

#[def_test]
fn default_open_validation_rejects_without_metadata_substitution() {
    let super_block = SimpleFs::new_with(&TEST_FILE_SYSTEM_TYPE, 0, |fs| {
        let mut root = DirMapping::new();
        root.add_dir(
            "deny",
            SimpleDir::new_maker(fs.clone(), Arc::new(DenyOpenDir(DirMapping::new()))),
        );
        SimpleDir::new_maker(fs, Arc::new(root))
    })
    .unwrap();
    let root = Mount::new_root(&super_block).root_path();
    let result = crate::Filename::new("deny").open_with_flags_at(
        &root,
        &root,
        linux_raw_sys::general::O_RDONLY,
        crate::NodePermission::empty(),
        crate::NodePermission::empty(),
        kcred::initial_cred(),
    );
    assert_eq!(result.err(), Some(VfsError::PermissionDenied));
}

#[def_test]
fn chained_directory_requires_both_open_validations() {
    let super_block = SimpleFs::new_with(&TEST_FILE_SYSTEM_TYPE, 0, |fs| {
        let mut root = DirMapping::new();
        let left_deny = DenyOpenDir(DirMapping::new()).chain(DirMapping::new());
        let right_deny = DirMapping::new().chain(DenyOpenDir(DirMapping::new()));
        root.add_dir(
            "left",
            SimpleDir::new_maker(fs.clone(), Arc::new(left_deny)),
        );
        root.add_dir(
            "right",
            SimpleDir::new_maker(fs.clone(), Arc::new(right_deny)),
        );
        SimpleDir::new_maker(fs, Arc::new(root))
    })
    .unwrap();
    let root = Mount::new_root(&super_block).root_path();
    for name in ["left", "right"] {
        let result = crate::Filename::new(name).open_with_flags_at(
            &root,
            &root,
            linux_raw_sys::general::O_RDONLY,
            crate::NodePermission::empty(),
            crate::NodePermission::empty(),
            kcred::initial_cred(),
        );
        assert_eq!(result.err(), Some(VfsError::PermissionDenied));
    }
}

const MAX_POSITION_RENDEZVOUS_YIELDS: usize = 100_000;

struct PositionOps {
    entered: Arc<AtomicBool>,
    entered_queue: Arc<ktask::WaitQueue>,
    release: Arc<AtomicBool>,
    release_queue: Arc<ktask::WaitQueue>,
    calls: Arc<AtomicUsize>,
    callback_done: Arc<AtomicBool>,
    b_saw_callback_done: Arc<AtomicBool>,
}

impl SimpleDirOps for PositionOps {
    fn child_names<'a>(&'a self) -> VfsResult<KBox<dyn Iterator<Item = Cow<'a, str>> + 'a>> {
        let call = self.calls.fetch_add(1, Ordering::AcqRel);
        if call == 0 {
            self.entered.store(true, Ordering::Release);
            self.entered_queue.notify_all(true);
            self.release_queue
                .wait_until(|| self.release.load(Ordering::Acquire));
            self.callback_done.store(true, Ordering::Release);
        } else if call == 1 {
            self.b_saw_callback_done.store(
                self.callback_done.load(Ordering::Acquire),
                Ordering::Release,
            );
        }
        Ok(KBox::new(core::iter::empty())?)
    }

    fn lookup_child(&self, _lookup: SimpleDirLookup<'_>, _name: &str) -> VfsResult<Dentry> {
        Err(VfsError::NotFound)
    }
}

struct IterateResult {
    result: VfsResult<usize>,
    count: usize,
    context_position: u64,
}

fn wait_until(flag: &AtomicBool) {
    let mut waits = 0;
    while !flag.load(Ordering::Acquire) {
        if waits == MAX_POSITION_RENDEZVOUS_YIELDS {
            panic!("directory position wait timed out");
        }
        waits += 1;
        ktask::yield_now();
    }
}

fn wait_until_task_blocked(task: &ktask::KtaskRef) {
    let mut waits = 0;
    loop {
        let state = task.state();
        if state.is_blocked() {
            return;
        }
        assert_ne!(
            state,
            ktask::TaskState::Exited,
            "directory task exited without blocking"
        );
        if waits == MAX_POSITION_RENDEZVOUS_YIELDS {
            panic!("directory position lock did not block the second task");
        }
        waits += 1;
        ktask::yield_now();
    }
}

struct PositionHarness {
    ops: Arc<PositionOps>,
    entered: Arc<AtomicBool>,
    entered_queue: Arc<ktask::WaitQueue>,
    release: Arc<AtomicBool>,
    release_queue: Arc<ktask::WaitQueue>,
    calls: Arc<AtomicUsize>,
    callback_done: Arc<AtomicBool>,
    b_saw_callback_done: Arc<AtomicBool>,
}

fn position_ops() -> PositionHarness {
    let entered = Arc::new(AtomicBool::new(false));
    let entered_queue = Arc::new(ktask::WaitQueue::new());
    let release = Arc::new(AtomicBool::new(false));
    let release_queue = Arc::new(ktask::WaitQueue::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let callback_done = Arc::new(AtomicBool::new(false));
    let b_saw_callback_done = Arc::new(AtomicBool::new(false));
    PositionHarness {
        ops: Arc::new(PositionOps {
            entered: entered.clone(),
            entered_queue: entered_queue.clone(),
            release: release.clone(),
            release_queue: release_queue.clone(),
            calls: calls.clone(),
            callback_done: callback_done.clone(),
            b_saw_callback_done: b_saw_callback_done.clone(),
        }),
        entered,
        entered_queue,
        release,
        release_queue,
        calls,
        callback_done,
        b_saw_callback_done,
    }
}

#[def_test]
fn directory_position_lock_serializes_iterate_and_seek() {
    let PositionHarness {
        ops,
        entered,
        entered_queue,
        release,
        release_queue,
        ..
    } = position_ops();
    let root = root_path(ops);
    let file = crate::dentry_open(root, 0, kcred::initial_cred()).unwrap();

    let a_file = file.clone();
    let a_result = Arc::new(ksync::Mutex::new(None));
    let a_slot = a_result.clone();
    let a = ktask::spawn(move || {
        let mut count = 0;
        let mut sink = |_: &str, _, _, _| {
            count += 1;
            true
        };
        let mut ctx = DirContext::new(0, &mut sink);
        let result = a_file.iterate_dir(&mut ctx);
        let context_position = ctx.pos();
        *a_slot.lock() = Some(IterateResult {
            result,
            count,
            context_position,
        });
    });
    entered_queue.wait_until(|| entered.load(Ordering::Acquire));

    let b_file = file.clone();
    let b_started = Arc::new(AtomicBool::new(false));
    let b_result = Arc::new(ksync::Mutex::new(None));
    let b_started_thread = b_started.clone();
    let b_slot = b_result.clone();
    let b = ktask::spawn(move || {
        b_started_thread.store(true, Ordering::Release);
        let current = b_file.llseek(0, crate::file::SEEK_CUR);
        let final_position = b_file.llseek(5, crate::file::SEEK_SET);
        *b_slot.lock() = Some((current, final_position));
    });
    wait_until(&b_started);
    wait_until_task_blocked(&b);
    release.store(true, Ordering::Release);
    release_queue.notify_all(true);
    a.join();
    b.join();

    let a = a_result.lock();
    let a = a.as_ref().expect("iterate result");
    assert_eq!(a.result, Ok(2));
    assert_eq!(a.count, 2);
    assert_eq!(a.context_position, 2);

    let b = b_result.lock();
    let (current, final_position) = b.as_ref().expect("seek result");
    assert_eq!(*current, Ok(2));
    assert_eq!(*final_position, Ok(5));
    assert_eq!(file.position(), 5);
}

#[def_test]
fn directory_position_lock_serializes_two_iterates() {
    let PositionHarness {
        ops,
        entered,
        entered_queue,
        release,
        release_queue,
        calls,
        callback_done,
        b_saw_callback_done,
    } = position_ops();
    let root = root_path(ops);
    let file = crate::dentry_open(root, 0, kcred::initial_cred()).unwrap();

    let a_file = file.clone();
    let a_result = Arc::new(ksync::Mutex::new(None));
    let a_slot = a_result.clone();
    let a = ktask::spawn(move || {
        let mut count = 0;
        let mut sink = |_: &str, _, _, _| {
            count += 1;
            true
        };
        let mut ctx = DirContext::new(0, &mut sink);
        let result = a_file.iterate_dir(&mut ctx);
        let context_position = ctx.pos();
        *a_slot.lock() = Some(IterateResult {
            result,
            count,
            context_position,
        });
    });
    entered_queue.wait_until(|| entered.load(Ordering::Acquire));

    let b_file = file.clone();
    let b_started = Arc::new(AtomicBool::new(false));
    let b_result = Arc::new(ksync::Mutex::new(None));
    let b_started_thread = b_started.clone();
    let b_slot = b_result.clone();
    let b = ktask::spawn(move || {
        b_started_thread.store(true, Ordering::Release);
        let mut count = 0;
        let mut sink = |_: &str, _, _, _| {
            count += 1;
            true
        };
        let mut ctx = DirContext::new(0, &mut sink);
        let result = b_file.iterate_dir(&mut ctx);
        let context_position = ctx.pos();
        *b_slot.lock() = Some(IterateResult {
            result,
            count,
            context_position,
        });
    });
    wait_until(&b_started);
    wait_until_task_blocked(&b);
    release.store(true, Ordering::Release);
    release_queue.notify_all(true);
    a.join();
    b.join();

    let a = a_result.lock();
    let a = a.as_ref().expect("first iterate result");
    assert_eq!(a.result, Ok(2));
    assert_eq!(a.count, 2);
    assert_eq!(a.context_position, 2);

    let b = b_result.lock();
    let b = b.as_ref().expect("second iterate result");
    assert_eq!(b.result, Ok(0));
    assert_eq!(b.count, 0);
    assert_eq!(b.context_position, 2);

    assert!(callback_done.load(Ordering::Acquire));
    assert!(b_saw_callback_done.load(Ordering::Acquire));
    assert_eq!(calls.load(Ordering::Acquire), 2);
    assert_eq!(file.position(), 2);
}

use alloc::collections::BTreeMap;
use core::ops::Bound::{Excluded, Unbounded};

use crate::SimpleFile;

#[derive(Clone)]
struct SparseTestEntry {
    name: String,
    file: Arc<SimpleFile>,
}

struct SparseTestSource {
    entries: Arc<ksync::Mutex<BTreeMap<u64, SparseTestEntry>>>,
    fs: Arc<ksync::Mutex<Option<Arc<SimpleFs>>>>,
    first_calls: Arc<AtomicUsize>,
    next_calls: Arc<AtomicUsize>,
    name_calls: Arc<AtomicUsize>,
    lookup_calls: Arc<AtomicUsize>,
    missing_lookup: Arc<ksync::Mutex<Option<u64>>>,
    missing_name: Arc<ksync::Mutex<Option<u64>>>,
    name_error: Arc<ksync::Mutex<Option<VfsError>>>,
    lookup_error: Arc<ksync::Mutex<Option<VfsError>>>,
    mismatch_lookup: Arc<AtomicBool>,
}

impl SparseTestSource {
    fn new() -> Self {
        Self {
            entries: Arc::new(ksync::Mutex::new(BTreeMap::new())),
            fs: Arc::new(ksync::Mutex::new(None)),
            first_calls: Arc::new(AtomicUsize::new(0)),
            next_calls: Arc::new(AtomicUsize::new(0)),
            name_calls: Arc::new(AtomicUsize::new(0)),
            lookup_calls: Arc::new(AtomicUsize::new(0)),
            missing_lookup: Arc::new(ksync::Mutex::new(None)),
            missing_name: Arc::new(ksync::Mutex::new(None)),
            name_error: Arc::new(ksync::Mutex::new(None)),
            lookup_error: Arc::new(ksync::Mutex::new(None)),
            mismatch_lookup: Arc::new(AtomicBool::new(false)),
        }
    }

    fn insert(&self, key: u64, name: &str) {
        let fs = self.fs.lock().as_ref().expect("test fs").clone();
        let file = SimpleFile::new_regular(fs, || Ok("x"));
        self.entries.lock().insert(
            key,
            SparseTestEntry {
                name: name.to_owned(),
                file,
            },
        );
    }

    fn insert_existing(&self, key: u64, name: &str) {
        let existing = self
            .entries
            .lock()
            .values()
            .next()
            .expect("existing sparse entry")
            .file
            .clone();
        self.entries.lock().insert(
            key,
            SparseTestEntry {
                name: name.to_owned(),
                file: existing,
            },
        );
    }

    fn replace(&self, key: u64, name: &str) {
        let fs = self.fs.lock().as_ref().expect("test fs").clone();
        let file = SimpleFile::new_regular(fs, || Ok("replacement"));
        self.entries.lock().insert(
            key,
            SparseTestEntry {
                name: name.to_owned(),
                file,
            },
        );
    }
}

impl SparseDirSource for SparseTestSource {
    fn first_key(&self) -> VfsResult<Option<u64>> {
        self.first_calls.fetch_add(1, Ordering::AcqRel);
        Ok(self.entries.lock().keys().next().copied())
    }

    fn next_key(&self, after: u64) -> VfsResult<Option<u64>> {
        self.next_calls.fetch_add(1, Ordering::AcqRel);
        Ok(self
            .entries
            .lock()
            .range((Excluded(after), Unbounded))
            .next()
            .map(|(key, _)| *key))
    }

    fn name_for_key(&self, key: u64) -> VfsResult<String> {
        self.name_calls.fetch_add(1, Ordering::AcqRel);
        if *self.missing_name.lock() == Some(key) {
            return Err(VfsError::NotFound);
        }
        if let Some(error) = *self.name_error.lock() {
            return Err(error);
        }
        self.entries
            .lock()
            .get(&key)
            .map(|entry| entry.name.clone())
            .ok_or(VfsError::NotFound)
    }

    fn lookup_key(&self, lookup: SimpleDirLookup<'_>, key: u64, name: &str) -> VfsResult<Dentry> {
        self.lookup_calls.fetch_add(1, Ordering::AcqRel);
        if *self.missing_lookup.lock() == Some(key) {
            return Err(VfsError::NotFound);
        }
        if let Some(error) = *self.lookup_error.lock() {
            return Err(error);
        }
        let entry = self
            .entries
            .lock()
            .get(&key)
            .cloned()
            .ok_or(VfsError::NotFound)?;
        if entry.name != name {
            return Err(VfsError::NotFound);
        }
        if self.mismatch_lookup.load(Ordering::Acquire) {
            return lookup.file("wrong-name", entry.file);
        }
        lookup.file(name, entry.file)
    }

    fn lookup_name(&self, lookup: SimpleDirLookup<'_>, name: &str) -> VfsResult<Dentry> {
        let entry = self
            .entries
            .lock()
            .values()
            .find(|entry| entry.name == name)
            .cloned()
            .ok_or(VfsError::NotFound)?;
        lookup.file(name, entry.file)
    }
}

fn sparse_fs(entries: &[(u64, &str)]) -> (Path, Arc<SparseTestSource>) {
    let source = Arc::new(SparseTestSource::new());
    let source_builder = source.clone();
    let entries = entries.to_vec();
    let super_block = SimpleFs::new_with(&TEST_FILE_SYSTEM_TYPE, 0, move |fs| {
        *source_builder.fs.lock() = Some(fs.clone());
        for (index, (key, name)) in entries.into_iter().enumerate() {
            if index == 0 {
                source_builder.insert(key, name);
            } else {
                source_builder.insert_existing(key, name);
            }
        }
        let inode = new_sparse_dir_inode(
            fs.clone(),
            source_builder.clone(),
            NodePermission::from_bits_truncate(0o755),
            0,
            0,
        );
        Arc::new(move || inode.clone())
    })
    .unwrap();
    (Mount::new_root(&super_block).root_path(), source)
}

fn read_one(file: &VfsFile) -> Vec<(String, u64)> {
    let mut entries = Vec::new();
    let mut sink = |name: &str, _, _, cookie| {
        entries.push((name.to_owned(), cookie));
        true
    };
    let mut ctx = DirContext::new(file.position(), &mut sink);
    file.iterate_dir(&mut ctx).unwrap();
    entries
}

fn read_single(file: &VfsFile) -> Vec<(String, u64)> {
    let mut entries = Vec::new();
    let mut accepted = 0;
    let mut sink = |name: &str, _, _, cookie| {
        if accepted == 0 {
            accepted += 1;
            entries.push((name.to_owned(), cookie));
            true
        } else {
            false
        }
    };
    let mut ctx = DirContext::new(file.position(), &mut sink);
    file.iterate_dir(&mut ctx).unwrap();
    entries
}

#[def_test]
fn sparse_short_reads_are_incremental() {
    let (root, source) = sparse_fs(&[(1, "one"), (2, "two"), (3, "three")]);
    let file = crate::dentry_open(root, 0, kcred::initial_cred()).unwrap();
    let mut seen = Vec::new();
    loop {
        let mut accepted = 0;
        let mut sink = |name: &str, _, _, cookie| {
            if accepted == 0 {
                accepted += 1;
                seen.push((name.to_owned(), cookie));
                true
            } else {
                false
            }
        };
        let mut ctx = DirContext::new(file.position(), &mut sink);
        let count = file.iterate_dir(&mut ctx).unwrap();
        if count == 0 {
            break;
        }
    }
    assert_eq!(
        seen,
        [
            (".".to_owned(), 1),
            ("..".to_owned(), 2),
            ("one".to_owned(), 3),
            ("two".to_owned(), 4),
            ("three".to_owned(), 5),
        ]
    );
    assert_eq!(source.first_calls.load(Ordering::Acquire), 1);
    assert_eq!(source.next_calls.load(Ordering::Acquire), 4);
}

#[def_test]
fn sparse_candidate_disappearance_does_not_consume_cookie() {
    let (root, source) = sparse_fs(&[(1, "one"), (2, "two")]);
    *source.missing_lookup.lock() = Some(1);
    let file = crate::dentry_open(root, 0, kcred::initial_cred()).unwrap();
    let entries = read_one(&file);
    assert_eq!(
        entries,
        [
            (".".to_owned(), 1),
            ("..".to_owned(), 2),
            ("two".to_owned(), 3),
        ]
    );
}

#[def_test]
fn sparse_max_key_does_not_overflow_cookie() {
    let (root, _source) = sparse_fs(&[(u64::MAX, "max")]);
    let file = crate::dentry_open(root, 0, kcred::initial_cred()).unwrap();
    let entries = read_one(&file);
    assert_eq!(
        entries,
        [
            (".".to_owned(), 1),
            ("..".to_owned(), 2),
            ("max".to_owned(), 3),
        ]
    );
}

#[def_test]
fn sparse_eof_observes_later_insertion() {
    let (root, source) = sparse_fs(&[(1, "one"), (2, "two")]);
    let file = crate::dentry_open(root, 0, kcred::initial_cred()).unwrap();
    let first = read_one(&file);
    assert_eq!(first.len(), 4);
    assert_eq!(file.position(), 4);
    source.insert_existing(3, "three");
    let next = read_one(&file);
    assert_eq!(next, [("three".to_owned(), 5)]);
}

#[def_test]
fn sparse_seek_beyond_eof_preserves_position_and_recovers() {
    let (root, source) = sparse_fs(&[(1, "one"), (2, "two")]);
    let file = crate::dentry_open(root, 0, kcred::initial_cred()).unwrap();
    assert_eq!(read_one(&file).len(), 4);
    assert_eq!(file.llseek(5, crate::file::SEEK_SET), Ok(5));
    assert_eq!(read_one(&file), []);
    assert_eq!(file.position(), 5);

    source.insert_existing(3, "three");
    source.insert_existing(4, "four");
    let recovered = read_one(&file);
    assert_eq!(recovered, [("four".to_owned(), 6)]);
    assert_eq!(file.position(), 6);
}

#[def_test]
fn sparse_seek_end_is_rejected() {
    let (root, _source) = sparse_fs(&[(1, "one")]);
    let file = crate::dentry_open(root, 0, kcred::initial_cred()).unwrap();
    assert_eq!(
        file.llseek(0, crate::file::SEEK_END),
        Err(VfsError::InvalidInput)
    );
}

#[def_test]
fn sparse_sink_rejection_retries_same_candidate() {
    let (root, source) = sparse_fs(&[(1, "one")]);
    let file = crate::dentry_open(root, 0, kcred::initial_cred()).unwrap();

    let mut accepted = 0;
    let mut sink = |_: &str, _, _, _| {
        accepted += 1;
        accepted <= 2
    };
    let mut ctx = DirContext::new(0, &mut sink);
    assert_eq!(file.iterate_dir(&mut ctx), Ok(2));
    assert_eq!(file.position(), 2);
    assert_eq!(source.name_calls.load(Ordering::Acquire), 1);
    assert_eq!(source.lookup_calls.load(Ordering::Acquire), 1);

    let entries = read_one(&file);
    assert_eq!(entries, [("one".to_owned(), 3)]);
    assert_eq!(file.position(), 3);
}

#[def_test]
fn sparse_name_mismatch_and_invalid_name_are_rejected() {
    let (root, source) = sparse_fs(&[(1, "one")]);
    source.mismatch_lookup.store(true, Ordering::Release);
    let file = crate::dentry_open(root, 0, kcred::initial_cred()).unwrap();
    let mut sink = |_: &str, _, _, _| true;
    let mut ctx = DirContext::new(0, &mut sink);
    assert_eq!(file.iterate_dir(&mut ctx), Err(VfsError::InvalidData));

    let (root, source) = sparse_fs(&[(1, "")]);
    let file = crate::dentry_open(root, 0, kcred::initial_cred()).unwrap();
    let mut sink = |_: &str, _, _, _| true;
    let mut ctx = DirContext::new(0, &mut sink);
    assert_eq!(file.iterate_dir(&mut ctx), Err(VfsError::InvalidData));
    assert_eq!(source.name_calls.load(Ordering::Acquire), 1);
}

#[def_test]
fn sparse_dup_shares_cursor_and_position() {
    let (root, _source) = sparse_fs(&[(1, "one"), (2, "two")]);
    let file = crate::dentry_open(root, 0, kcred::initial_cred()).unwrap();
    let dup = file.clone();

    let mut accepted = 0;
    let mut sink = |_: &str, _, _, _| {
        accepted += 1;
        accepted <= 2
    };
    let mut ctx = DirContext::new(0, &mut sink);
    assert_eq!(file.iterate_dir(&mut ctx), Ok(2));
    assert_eq!(file.position(), 2);
    assert_eq!(dup.position(), 2);

    let entries = read_single(&dup);
    assert_eq!(entries, [("one".to_owned(), 3)]);
    assert_eq!(file.position(), 3);
    assert_eq!(dup.position(), 3);
}

#[def_test]
fn sparse_arbitrary_seek_rebuilds_visible_ordinal() {
    let (root, _source) = sparse_fs(&[(1, "one"), (2, "two"), (3, "three")]);
    let file = crate::dentry_open(root, 0, kcred::initial_cred()).unwrap();
    assert_eq!(read_one(&file).len(), 5);
    assert_eq!(file.llseek(3, crate::file::SEEK_SET), Ok(3));
    assert_eq!(read_single(&file), [("two".to_owned(), 4)]);
    assert_eq!(file.llseek(1, crate::file::SEEK_SET), Ok(1));
    assert_eq!(read_single(&file), [("..".to_owned(), 2)]);
}

#[def_test]
fn sparse_same_key_name_replacement_uses_current_inode() {
    let (root, source) = sparse_fs(&[(1, "one"), (2, "two")]);
    let file = crate::dentry_open(root, 0, kcred::initial_cred()).unwrap();

    let mut old = Vec::new();
    let mut sink = |name: &str, ino, _, _| {
        old.push((name.to_owned(), ino));
        true
    };
    let mut ctx = DirContext::new(0, &mut sink);
    file.iterate_dir(&mut ctx).unwrap();
    let old_one = old.iter().find(|(name, _)| name == "one").unwrap().1;

    source.replace(1, "one");
    assert_eq!(file.llseek(0, crate::file::SEEK_SET), Ok(0));
    let mut new = Vec::new();
    let mut sink = |name: &str, ino, _, _| {
        new.push((name.to_owned(), ino));
        true
    };
    let mut ctx = DirContext::new(0, &mut sink);
    file.iterate_dir(&mut ctx).unwrap();
    let new_one = new.iter().find(|(name, _)| name == "one").unwrap().1;
    assert_ne!(old_one, new_one);
}

#[def_test]
fn sparse_path_lookup_uses_source_name_mapping() {
    let (root, _source) = sparse_fs(&[(1, "one"), (2, "two")]);
    let entry = root.dentry().lookup("one").unwrap();
    assert_eq!(entry.name_snapshot(), "one");
    assert!(matches!(
        root.dentry().lookup("missing"),
        Err(VfsError::NotFound)
    ));
}

#[def_test]
fn sparse_4097_entries_with_short_buffer() {
    let names: Vec<String> = (1..=4097).map(|key| alloc::format!("n{key}")).collect();
    let entries: Vec<(u64, &str)> = names
        .iter()
        .enumerate()
        .map(|(index, name)| ((index + 1) as u64, name.as_str()))
        .collect();
    let (root, source) = sparse_fs(&entries);
    let file = crate::dentry_open(root, 0, kcred::initial_cred()).unwrap();

    let mut total = 0;
    loop {
        let mut accepted = 0;
        let mut sink = |_: &str, _, _, _| {
            if accepted < 7 {
                accepted += 1;
                true
            } else {
                false
            }
        };
        let mut ctx = DirContext::new(file.position(), &mut sink);
        let count = file.iterate_dir(&mut ctx).unwrap();
        if count == 0 {
            break;
        }
        total += count;
    }
    assert_eq!(total, 4099);
    assert_eq!(source.first_calls.load(Ordering::Acquire), 1);
    // A rejected pending candidate is re-resolved on the next batch, so name
    // and lookup calls are proportional to entries plus short-read batches.
    assert!(source.name_calls.load(Ordering::Acquire) >= 4097);
    assert!(source.lookup_calls.load(Ordering::Acquire) >= 4097);
    assert!(source.name_calls.load(Ordering::Acquire) < 8194);
    assert!(source.lookup_calls.load(Ordering::Acquire) < 8194);
}

#[def_test]
fn sparse_empty_eof_observes_first_insertion() {
    let (root, source) = sparse_fs(&[]);
    let file = crate::dentry_open(root, 0, kcred::initial_cred()).unwrap();
    assert_eq!(read_one(&file), [(".".to_owned(), 1), ("..".to_owned(), 2)]);
    source.insert(1, "one");
    assert_eq!(read_one(&file), [("one".to_owned(), 3)]);
}

#[def_test]
fn sparse_name_not_found_skips_without_advancing_cookie() {
    let (root, source) = sparse_fs(&[(1, "one"), (2, "two")]);
    *source.missing_name.lock() = Some(1);
    let file = crate::dentry_open(root, 0, kcred::initial_cred()).unwrap();
    assert_eq!(
        read_one(&file),
        [
            (".".to_owned(), 1),
            ("..".to_owned(), 2),
            ("two".to_owned(), 3),
        ]
    );
}

#[def_test]
fn sparse_name_and_lookup_errors_are_retryable() {
    let (root, source) = sparse_fs(&[(1, "one"), (2, "two")]);
    let file = crate::dentry_open(root, 0, kcred::initial_cred()).unwrap();

    *source.name_error.lock() = Some(VfsError::Io);
    let mut sink = |_: &str, _, _, _| true;
    let mut ctx = DirContext::new(0, &mut sink);
    assert_eq!(file.iterate_dir(&mut ctx), Err(VfsError::Io));
    assert_eq!(file.position(), 2);
    *source.name_error.lock() = None;
    assert_eq!(read_single(&file), [("one".to_owned(), 3)]);

    *source.lookup_error.lock() = Some(VfsError::Io);
    assert_eq!(file.llseek(2, crate::file::SEEK_SET), Ok(2));
    let mut sink = |_: &str, _, _, _| true;
    let mut ctx = DirContext::new(2, &mut sink);
    assert_eq!(file.iterate_dir(&mut ctx), Err(VfsError::Io));
    assert_eq!(file.position(), 2);
    *source.lookup_error.lock() = None;
    assert_eq!(read_single(&file), [("one".to_owned(), 3)]);
}

#[def_test]
fn sparse_independent_opens_have_independent_cursors() {
    let (root, _source) = sparse_fs(&[(1, "one")]);
    let first = crate::dentry_open(root.clone(), 0, kcred::initial_cred()).unwrap();
    let second = crate::dentry_open(root, 0, kcred::initial_cred()).unwrap();
    assert_eq!(read_single(&first), [(".".to_owned(), 1)]);
    assert_eq!(read_single(&first), [("..".to_owned(), 2)]);
    assert_eq!(read_single(&second), [(".".to_owned(), 1)]);
    assert_eq!(read_single(&second), [("..".to_owned(), 2)]);
}

#[def_test]
fn sparse_cookie_positive_overflow_is_eoverflow_and_preserves_position() {
    let (root, _source) = sparse_fs(&[(1, "one")]);
    let file = crate::dentry_open(root, 0, kcred::initial_cred()).unwrap();
    assert_eq!(
        file.llseek(i64::MAX, crate::file::SEEK_SET),
        Ok(i64::MAX as u64)
    );
    assert_eq!(
        file.llseek(1, crate::file::SEEK_CUR),
        Err(VfsError::from(kerrno::LinuxError::EOVERFLOW))
    );
    assert_eq!(file.position(), i64::MAX as u64);
    assert_eq!(read_one(&file), []);
}

#[def_test]
fn sparse_replacement_invalidates_fresh_path_lookup() {
    let (root, source) = sparse_fs(&[(1, "one")]);
    let old = root.dentry().lookup("one").unwrap().metadata().inode;
    source.replace(1, "one");
    let new = root.dentry().lookup("one").unwrap().metadata().inode;
    assert_ne!(old, new);
}
