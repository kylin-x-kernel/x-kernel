// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux-style pathname lookup.
//!
//! This module owns VFS pathname walking policy. Callers provide a
//! [`LookupContext`] plus typed [`LookupFlags`] and [`LookupIntent`]; namei then
//! applies the common final-component, symlink, and magic-link rules.

use alloc::string::String;

use crate::{
    Location, LookupFlags, LookupIntent, NodeType, ResolvedObject, VfsError, VfsResult,
    path::{Component, Path, PathBuf},
};

/// Default maximum number of symbolic or magic-link follows in one lookup.
pub const DEFAULT_MAX_LINKS: usize = 40;

/// Root and base directory for one pathname lookup.
#[derive(Debug, Clone)]
pub struct LookupContext {
    root: Location,
    base: Location,
}

impl LookupContext {
    /// Creates a lookup context from a process root and starting directory.
    pub fn new(root: Location, base: Location) -> Self {
        Self { root, base }
    }

    /// Returns the lookup root.
    pub fn root(&self) -> &Location {
        &self.root
    }

    /// Returns the starting directory.
    pub fn base(&self) -> &Location {
        &self.base
    }

    fn starting_location(&self, path: &Path) -> Location {
        if path.is_absolute() {
            self.root.clone()
        } else {
            self.base.clone()
        }
    }
}

/// Per-lookup state matching Linux `nameidata` at the X-Kernel abstraction
/// level.
struct Nameidata<'a> {
    context: &'a LookupContext,
    intent: LookupIntent,
    flags: LookupFlags,
    link_count: usize,
}

impl<'a> Nameidata<'a> {
    fn new(context: &'a LookupContext, intent: LookupIntent, flags: LookupFlags) -> Self {
        Self {
            context,
            intent,
            flags,
            link_count: 0,
        }
    }

    fn follow_flags_for_intermediate(&self) -> LookupFlags {
        self.flags | LookupFlags::FOLLOW_FINAL
    }

    fn reserve_link_follow(&mut self) -> VfsResult<()> {
        if self.link_count >= DEFAULT_MAX_LINKS {
            return Err(VfsError::FilesystemLoop);
        }
        self.link_count += 1;
        Ok(())
    }

    fn follow_symlink(&mut self, containing_dir: &Location, link: Location) -> VfsResult<Location> {
        self.reserve_link_follow()?;
        let target = link.read_link()?;
        if target.is_empty() {
            return Err(VfsError::NotFound);
        }

        let saved_base = LookupContext::new(self.context.root.clone(), containing_dir.clone());
        let saved_flags = self.flags;
        self.flags = self.follow_flags_for_intermediate();
        let resolved = self.walk_from(&saved_base, PathBuf::from(target).as_ref())?;
        self.flags = saved_flags;
        Ok(resolved)
    }

    fn follow_magic_link(&mut self, link: Location) -> VfsResult<Location> {
        let Some(mut magic_link) = link.magic_link() else {
            return Ok(link);
        };

        loop {
            self.reserve_link_follow()?;
            let resolved =
                magic_link.follow(self.intent, self.flags | LookupFlags::FOLLOW_FINAL)?;
            match resolved {
                ResolvedObject::Location(location) => return Ok(location),
                ResolvedObject::MagicLink(next) => magic_link = next,
            }
        }
    }

    fn step_into(
        &mut self,
        containing_dir: &Location,
        next: Location,
        is_final: bool,
    ) -> VfsResult<Location> {
        if next.magic_link().is_some() {
            if self.flags.rejects_magic_links() {
                return Err(VfsError::FilesystemLoop);
            }
            if !is_final || self.flags.follows_final() {
                return self.follow_magic_link(next);
            }
            return Ok(next);
        }

        if next.node_type() == NodeType::Symlink && (!is_final || self.flags.follows_final()) {
            return self.follow_symlink(containing_dir, next);
        }
        Ok(next)
    }

    fn walk_from(&mut self, context: &LookupContext, path: &Path) -> VfsResult<Location> {
        if path.as_str().is_empty() {
            if self.flags.contains(LookupFlags::EMPTY_PATH) {
                return Ok(context.base().clone());
            }
            return Err(VfsError::NotFound);
        }

        let mut current = context.starting_location(path);
        let mut components = path.components().peekable();

        while let Some(component) = components.next() {
            let is_final = components.peek().is_none();
            match component {
                Component::RootDir => {
                    current = context.root().clone();
                }
                Component::CurDir => {}
                Component::ParentDir => {
                    if !current.ptr_eq(context.root()) {
                        current = current.parent().unwrap_or_else(|| context.root().clone());
                    }
                }
                Component::Normal(name) => {
                    current.check_is_dir()?;
                    let child = current.lookup_no_follow(name)?;
                    current = self.step_into(&current, child, is_final)?;
                }
            }
        }

        Ok(current)
    }

    fn walk(&mut self, path: &Path) -> VfsResult<Location> {
        self.walk_from(self.context, path)
    }
}

/// Resolve `path` to a VFS location using typed lookup policy.
pub fn lookup_location(
    context: &LookupContext,
    path: impl AsRef<Path>,
    intent: LookupIntent,
    flags: LookupFlags,
) -> VfsResult<Location> {
    Nameidata::new(context, intent, flags).walk(path.as_ref())
}

/// Resolve a path to its parent directory and final component name.
pub fn lookup_parent(
    context: &LookupContext,
    path: &Path,
    intent: LookupIntent,
) -> VfsResult<(Location, String)> {
    let mut nd = Nameidata::new(context, intent, LookupFlags::follow());
    let name = path.file_name();
    if let Some(name) = name {
        let parent = match path.parent() {
            Some(parent) if !parent.as_str().is_empty() => nd.walk(parent)?,
            _ => context.starting_location(path),
        };
        parent.check_is_dir()?;
        return Ok((parent, String::from(name)));
    }

    let location = nd.walk(path)?;
    if location.ptr_eq(context.root()) {
        return Err(VfsError::InvalidInput);
    }
    let parent = location.parent().ok_or(VfsError::InvalidInput)?;
    Ok((parent, String::from(location.name())))
}

/// Resolve a path to the parent directory of a to-be-created final component.
pub fn lookup_nonexistent<'a>(
    context: &LookupContext,
    path: &'a Path,
    intent: LookupIntent,
) -> VfsResult<(Location, &'a str)> {
    let name = path.file_name().ok_or(VfsError::InvalidInput)?;
    let mut nd = Nameidata::new(context, intent, LookupFlags::follow());
    let parent = match path.parent() {
        Some(parent) if !parent.as_str().is_empty() => nd.walk(parent)?,
        _ => context.starting_location(path),
    };
    parent.check_is_dir()?;
    Ok((parent, name))
}

/// Read the display target of a symlink or Linux-style magic link.
pub fn read_link(context: &LookupContext, path: impl AsRef<Path>) -> VfsResult<String> {
    let location = lookup_location(
        context,
        path,
        LookupIntent::Readlink,
        LookupFlags::no_follow(),
    )?;
    if let Some(link) = location.magic_link() {
        link.readlink_display()
    } else {
        location.read_link()
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::{string::String, sync::Arc, vec::Vec};
    use core::{any::Any, task::Context, time::Duration};

    use hashbrown::HashMap;
    use kpoll::{IoEvents, Pollable};
    use unittest::{assert, assert_eq, def_test};

    use super::*;
    use crate::{
        DirEntry, DirEntrySink, DirNode, DirNodeOps, FileNode, FileNodeOps, Filesystem,
        MagicLinkOps, Metadata, MetadataUpdate, Mountpoint, NodeOps, NodePermission, Reference,
        ResolvedObject, StatFs, SuperBlockOperations, VfsError,
    };

    fn assert_send_sync<T: Send + Sync>() {}

    #[def_test]
    fn lookup_context_is_shareable() {
        assert_send_sync::<LookupContext>();
    }

    #[def_test]
    fn lookup_flags_are_typed_not_raw_bools() {
        let flags = LookupFlags::follow() | LookupFlags::NO_MAGIC_LINKS;
        assert!(flags.follows_final());
        assert!(flags.rejects_magic_links());
        assert_eq!(DEFAULT_MAX_LINKS, 40);
    }

    struct TestSuperBlock {
        root: DirEntry,
    }

    impl SuperBlockOperations for TestSuperBlock {
        fn name(&self) -> &str {
            "namei-test"
        }

        fn root_dentry(&self) -> DirEntry {
            self.root.clone()
        }

        fn statfs(&self) -> VfsResult<StatFs> {
            Ok(StatFs {
                fs_type: 0,
                block_size: 4096,
                blocks: 0,
                blocks_free: 0,
                blocks_available: 0,
                file_count: 0,
                free_file_count: 0,
                name_length: 255,
                fragment_size: 4096,
                mount_flags: 0,
            })
        }
    }

    struct TestDir {
        inode: u64,
        children: crate::Mutex<HashMap<String, DirEntry>>,
    }

    impl TestDir {
        fn new(inode: u64) -> Self {
            Self {
                inode,
                children: crate::Mutex::default(),
            }
        }

        fn insert(&self, name: &str, entry: DirEntry) {
            self.children.lock().insert(String::from(name), entry);
        }
    }

    impl NodeOps for TestDir {
        fn inode(&self) -> u64 {
            self.inode
        }

        fn metadata(&self) -> VfsResult<Metadata> {
            Ok(metadata(self.inode, NodeType::Directory, 0))
        }

        fn update_metadata(&self, _update: MetadataUpdate) -> VfsResult<()> {
            Ok(())
        }

        fn sync(&self, _data_only: bool) -> VfsResult<()> {
            Ok(())
        }

        fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
            self
        }
    }

    impl DirNodeOps for TestDir {
        fn read_dir(&self, _offset: u64, _sink: &mut dyn DirEntrySink) -> VfsResult<usize> {
            Ok(0)
        }

        fn lookup(&self, name: &str) -> VfsResult<DirEntry> {
            self.children
                .lock()
                .get(name)
                .cloned()
                .ok_or(VfsError::NotFound)
        }

        fn create(
            &self,
            _name: &str,
            _node_type: NodeType,
            _permission: NodePermission,
        ) -> VfsResult<DirEntry> {
            Err(VfsError::OperationNotSupported)
        }

        fn link(&self, _name: &str, _node: &DirEntry) -> VfsResult<DirEntry> {
            Err(VfsError::OperationNotSupported)
        }

        fn unlink(&self, _name: &str) -> VfsResult<()> {
            Err(VfsError::OperationNotSupported)
        }

        fn rename(&self, _src_name: &str, _dst_dir: &DirNode, _dst_name: &str) -> VfsResult<()> {
            Err(VfsError::OperationNotSupported)
        }
    }

    struct TestFile {
        inode: u64,
        data: crate::Mutex<Vec<u8>>,
        magic_link: Option<Arc<TestMagicLink>>,
    }

    impl TestFile {
        fn new(inode: u64, data: &[u8], magic_link: Option<Arc<TestMagicLink>>) -> Self {
            Self {
                inode,
                data: crate::Mutex::new(data.to_vec()),
                magic_link,
            }
        }
    }

    impl NodeOps for TestFile {
        fn inode(&self) -> u64 {
            self.inode
        }

        fn metadata(&self) -> VfsResult<Metadata> {
            Ok(metadata(
                self.inode,
                NodeType::RegularFile,
                self.data.lock().len() as u64,
            ))
        }

        fn update_metadata(&self, _update: MetadataUpdate) -> VfsResult<()> {
            Ok(())
        }

        fn sync(&self, _data_only: bool) -> VfsResult<()> {
            Ok(())
        }

        fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
            self
        }
    }

    impl Pollable for TestFile {
        fn poll(&self) -> IoEvents {
            IoEvents::IN | IoEvents::OUT
        }

        fn register(&self, _context: &mut Context<'_>, _events: IoEvents) {}
    }

    impl FileNodeOps for TestFile {
        fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
            let data = self.data.lock();
            let offset = offset as usize;
            if offset >= data.len() {
                return Ok(0);
            }
            let count = buf.len().min(data.len() - offset);
            buf[..count].copy_from_slice(&data[offset..offset + count]);
            Ok(count)
        }

        fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
            let offset = offset as usize;
            let mut data = self.data.lock();
            if offset + buf.len() > data.len() {
                data.resize(offset + buf.len(), 0);
            }
            data[offset..offset + buf.len()].copy_from_slice(buf);
            Ok(buf.len())
        }

        fn append(&self, buf: &[u8]) -> VfsResult<(usize, u64)> {
            let mut data = self.data.lock();
            data.extend_from_slice(buf);
            Ok((buf.len(), data.len() as u64))
        }

        fn set_len(&self, len: u64) -> VfsResult<()> {
            self.data.lock().resize(len as usize, 0);
            Ok(())
        }

        fn set_symlink(&self, target: &str) -> VfsResult<()> {
            *self.data.lock() = target.as_bytes().to_vec();
            Ok(())
        }

        fn magic_link(self: Arc<Self>) -> Option<Arc<dyn MagicLinkOps>> {
            self.magic_link
                .clone()
                .map(|link| link as Arc<dyn MagicLinkOps>)
        }
    }

    struct TestMagicLink {
        display: String,
        target: Location,
        last_intent: crate::Mutex<Option<LookupIntent>>,
        follow_count: crate::Mutex<usize>,
    }

    impl TestMagicLink {
        fn new(display: &str, target: Location) -> Self {
            Self {
                display: String::from(display),
                target,
                last_intent: crate::Mutex::new(None),
                follow_count: crate::Mutex::new(0),
            }
        }

        fn follow_count(&self) -> usize {
            *self.follow_count.lock()
        }

        fn last_intent(&self) -> Option<LookupIntent> {
            *self.last_intent.lock()
        }
    }

    impl MagicLinkOps for TestMagicLink {
        fn readlink_display(&self) -> VfsResult<String> {
            Ok(self.display.clone())
        }

        fn follow(&self, intent: LookupIntent, flags: LookupFlags) -> VfsResult<ResolvedObject> {
            if flags.rejects_magic_links() || !flags.follows_final() {
                return Err(VfsError::FilesystemLoop);
            }
            *self.last_intent.lock() = Some(intent);
            *self.follow_count.lock() += 1;
            Ok(ResolvedObject::location(self.target.clone()))
        }
    }

    struct TestTree {
        context: LookupContext,
        target: Location,
        magic_file: Arc<TestMagicLink>,
        magic_dir: Arc<TestMagicLink>,
    }

    fn metadata(inode: u64, node_type: NodeType, size: u64) -> Metadata {
        Metadata {
            device: 0,
            inode,
            nlink: 1,
            mode: NodePermission::default(),
            node_type,
            uid: 0,
            gid: 0,
            size,
            block_size: 4096,
            blocks: 1,
            rdev: Default::default(),
            atime: Duration::ZERO,
            mtime: Duration::ZERO,
            ctime: Duration::ZERO,
        }
    }

    fn file_entry(
        inode: u64,
        parent: &DirEntry,
        name: &str,
        node_type: NodeType,
        data: &[u8],
        magic_link: Option<Arc<TestMagicLink>>,
    ) -> DirEntry {
        DirEntry::new_file(
            FileNode::new(Arc::new(TestFile::new(inode, data, magic_link))),
            node_type,
            Reference::new(Some(parent.clone()), String::from(name)),
        )
    }

    fn dir_entry(inode: u64, parent: &DirEntry, name: &str) -> (DirEntry, Arc<TestDir>) {
        let ops = Arc::new(TestDir::new(inode));
        let entry = DirEntry::new_dir(
            |_| DirNode::new(ops.clone()),
            Reference::new(Some(parent.clone()), String::from(name)),
        );
        (entry, ops)
    }

    fn test_tree() -> TestTree {
        let root_ops = Arc::new(TestDir::new(1));
        let root = DirEntry::new_dir(|_| DirNode::new(root_ops.clone()), Reference::root());
        let target = file_entry(2, &root, "target", NodeType::RegularFile, b"target", None);
        root_ops.insert("target", target.clone());

        let link = file_entry(3, &root, "link", NodeType::Symlink, b"/target", None);
        root_ops.insert("link", link);

        let (target_dir, target_dir_ops) = dir_entry(4, &root, "dir");
        let leaf = file_entry(5, &target_dir, "leaf", NodeType::RegularFile, b"leaf", None);
        target_dir_ops.insert("leaf", leaf.clone());
        root_ops.insert("dir", target_dir.clone());

        let dir_link = file_entry(6, &root, "dirlink", NodeType::Symlink, b"/dir", None);
        root_ops.insert("dirlink", dir_link);

        let fs = Filesystem::new(Arc::new(TestSuperBlock { root: root.clone() }));
        let mount = Mountpoint::new_root(&fs);
        let root_location = mount.root_location();
        let target_location = Location::new(mount.clone(), target);
        let target_dir_location = Location::new(mount.clone(), target_dir);

        let magic_file = Arc::new(TestMagicLink::new(
            "/display/target",
            target_location.clone(),
        ));
        let magic_entry = file_entry(
            7,
            &root,
            "magic",
            NodeType::Symlink,
            b"/display/target",
            Some(magic_file.clone()),
        );
        root_ops.insert("magic", magic_entry);

        let magic_dir = Arc::new(TestMagicLink::new(
            "/display/dir",
            target_dir_location.clone(),
        ));
        let magic_dir_entry = file_entry(
            8,
            &root,
            "magicdir",
            NodeType::Symlink,
            b"/display/dir",
            Some(magic_dir.clone()),
        );
        root_ops.insert("magicdir", magic_dir_entry);

        TestTree {
            context: LookupContext::new(root_location.clone(), root_location.clone()),
            target: target_location,
            magic_file,
            magic_dir,
        }
    }

    #[def_test]
    fn final_symlink_follow_policy_is_typed() {
        let tree = test_tree();

        let followed = lookup_location(
            &tree.context,
            "/link",
            LookupIntent::Open,
            LookupFlags::follow(),
        )
        .unwrap();
        assert!(followed.ptr_eq(&tree.target));

        let link = lookup_location(
            &tree.context,
            "/link",
            LookupIntent::Open,
            LookupFlags::no_follow(),
        )
        .unwrap();
        assert_eq!(link.node_type(), NodeType::Symlink);
        assert_eq!(link.read_link().unwrap(), "/target");
    }

    #[def_test]
    fn non_final_symlink_is_followed_even_when_final_no_follow() {
        let tree = test_tree();

        let leaf = lookup_location(
            &tree.context,
            "/dirlink/leaf",
            LookupIntent::Open,
            LookupFlags::no_follow(),
        )
        .unwrap();
        assert_eq!(leaf.name(), "leaf");
        assert_eq!(leaf.node_type(), NodeType::RegularFile);
    }

    #[def_test]
    fn magic_link_follow_uses_lookup_intent() {
        let tree = test_tree();

        let followed = lookup_location(
            &tree.context,
            "/magic",
            LookupIntent::Exec,
            LookupFlags::follow(),
        )
        .unwrap();
        assert!(followed.ptr_eq(&tree.target));
        assert_eq!(tree.magic_file.last_intent(), Some(LookupIntent::Exec));
        assert_eq!(tree.magic_file.follow_count(), 1);
    }

    #[def_test]
    fn magic_link_no_follow_and_readlink_do_not_follow_target() {
        let tree = test_tree();

        let link = lookup_location(
            &tree.context,
            "/magic",
            LookupIntent::Open,
            LookupFlags::no_follow(),
        )
        .unwrap();
        assert_eq!(link.node_type(), NodeType::Symlink);
        assert_eq!(tree.magic_file.follow_count(), 0);

        assert_eq!(
            read_link(&tree.context, "/magic").unwrap(),
            "/display/target"
        );
        assert_eq!(tree.magic_file.follow_count(), 0);
    }

    #[def_test]
    fn magic_link_rejection_applies_to_final_and_non_final_components() {
        let tree = test_tree();

        let err = lookup_location(
            &tree.context,
            "/magic",
            LookupIntent::Open,
            LookupFlags::follow() | LookupFlags::NO_MAGIC_LINKS,
        )
        .unwrap_err();
        assert_eq!(err, VfsError::FilesystemLoop);

        let err = lookup_location(
            &tree.context,
            "/magicdir/leaf",
            LookupIntent::Open,
            LookupFlags::no_follow() | LookupFlags::NO_MAGIC_LINKS,
        )
        .unwrap_err();
        assert_eq!(err, VfsError::FilesystemLoop);
        assert_eq!(tree.magic_dir.follow_count(), 0);
    }

    #[def_test]
    fn non_final_magic_link_is_followed_by_namei() {
        let tree = test_tree();

        let leaf = lookup_location(
            &tree.context,
            "/magicdir/leaf",
            LookupIntent::Stat,
            LookupFlags::no_follow(),
        )
        .unwrap();
        assert_eq!(leaf.name(), "leaf");
        assert_eq!(tree.magic_dir.last_intent(), Some(LookupIntent::Stat));
    }
}
