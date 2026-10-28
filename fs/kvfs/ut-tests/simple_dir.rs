// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::{AtomicU64, Ordering};

use unittest::{TestResult, assert, assert_eq, def_test};

use super::*;
use crate::{FileSystemType, Mount, Path};

static TEST_FILE_SYSTEM_TYPE: FileSystemType = FileSystemType::internal("simple-dir-test");

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

#[def_test]
fn chained_names_propagate_failure_and_drop_first_iterator() {
    let drops = Arc::new(AtomicU64::new(0));
    let chained = NamesOps { drops: drops.clone(), fail: false }
        .chain(NamesOps { drops: drops.clone(), fail: true });
    assert_eq!(chained.child_names().err(), Some(VfsError::NoMemory));
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}

#[def_test]
fn chained_names_preserve_order_and_release_iterators() {
    let drops = Arc::new(AtomicU64::new(0));
    let chained = NamesOps { drops: drops.clone(), fail: false }
        .chain(NamesOps { drops: drops.clone(), fail: false });
    let names: Vec<_> = chained.child_names().unwrap().collect();
    assert_eq!(names.as_slice(), [Cow::Borrowed("entry"), Cow::Borrowed("entry")]);
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
    let mut sink = |_: &str, _, _, _| { accepted += 1; true };
    let mut context = DirContext::new(0, &mut sink);
    assert_eq!(file.iterate_dir(&mut context), Err(VfsError::NoMemory));
    assert_eq!(context.pos(), 0);
    assert_eq!(file.position(), 0);
    assert_eq!(accepted, 0);
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
    });
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
