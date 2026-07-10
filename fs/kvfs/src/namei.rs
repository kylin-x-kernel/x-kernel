// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Pathname lookup.
//!
//! This module owns VFS pathname walking policy. Callers provide process root,
//! lookup base, typed [`LookupFlags`], and [`LookupIntent`]; namei then applies
//! the common final-component, symlink, and magic-link rules.

use alloc::{borrow::ToOwned, string::String, sync::Arc, vec::Vec};

use crate::{
    AccMode, FMode, Filename, LookupFlags, LookupIntent, MountFlags, NodePermission, NodeType,
    OpenFlags, OpenHow, OpenParams, Path, ResolvedObject, VfsError, VfsFile, VfsFileBuilder,
    VfsInode, VfsResult, d_inode, d_is_dir, d_is_negative, d_is_symlink, path::PathBuf,
};

/// Deferred cleanup context used while resolving symbolic-link targets.
#[derive(Debug, Default)]
pub struct DelayedCall;

/// Default maximum number of symbolic or magic-link follows in one lookup.
pub const DEFAULT_MAX_LINKS: usize = 40;

const AT_FDCWD: i32 = -100;
const ND_ROOT_PRESET: u32 = 1;

#[derive(Clone, Debug, Eq, PartialEq)]
struct Qstr {
    name: String,
    len: usize,
    hash: u64,
}

impl Qstr {
    fn new(name: &str) -> Self {
        Self::new_len(name, name.len())
    }

    fn new_len(name: &str, len: usize) -> Self {
        debug_assert!(len <= name.len());
        let mut hash = 0xcbf29ce484222325u64;
        for byte in &name.as_bytes()[..len] {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        Self {
            name: String::from(name),
            len,
            hash,
        }
    }

    fn as_str(&self) -> &str {
        &self.name[..self.len]
    }

    fn has_trailing_slashes(&self) -> bool {
        self.name.len() > self.len
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LastType {
    Root,
    Dot,
    Dotdot,
    Norm,
}

/// Result of resolving a pathname to its parent and final component.
///
/// Callers get the parent path plus a classified final component instead of
/// collapsing root, dot, and dotdot into an empty or normal name.
#[derive(Clone, Debug)]
pub struct ParentLookup {
    parent: Path,
    last: Qstr,
    last_type: LastType,
    has_trailing_slash: bool,
}

impl ParentLookup {
    /// Returns the parent path.
    pub fn parent(&self) -> &Path {
        &self.parent
    }

    /// Returns the final component name.
    pub fn name(&self) -> &str {
        self.last.as_str()
    }

    /// Returns the classified final component type.
    pub fn last_type(&self) -> LastType {
        self.last_type
    }

    /// Returns whether the original pathname had trailing slashes after the
    /// final component.
    pub fn has_trailing_slash(&self) -> bool {
        self.has_trailing_slash
    }

    /// Consumes this lookup and returns a normal final component.
    pub fn into_normal(self) -> VfsResult<(Path, String)> {
        if self.last_type != LastType::Norm {
            return Err(VfsError::InvalidInput);
        }
        Ok((self.parent, self.last.as_str().to_owned()))
    }

    fn prepare_create(
        &self,
        name: &Filename,
        lookup_flags: LookupFlags,
    ) -> VfsResult<(Path, String)> {
        if self.last_type() != LastType::Norm {
            return Err(VfsError::AlreadyExists);
        }
        let parent = self.parent().clone();
        let last = self.name();
        match parent.dentry().as_dir()?.lookup(last) {
            Ok(_) => return Err(VfsError::AlreadyExists),
            Err(err)
                if err.canonicalize() == VfsError::NotFound
                    && self.has_trailing_slash()
                    && !lookup_flags.contains(LookupFlags::DIRECTORY) =>
            {
                return Err(VfsError::NotFound);
            }
            Err(err) if err.canonicalize() == VfsError::NotFound => {}
            Err(err) => return Err(err),
        }
        let pathname = name.as_pathname();
        let last = pathname
            .file_name()
            .filter(|file_name| *file_name == self.name())
            .ok_or(VfsError::InvalidInput)?;
        Ok((parent, last.to_owned()))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WalkComponent {
    More,
    Final,
}

/// Per-lookup state matching `struct nameidata`.
struct Nameidata<'a> {
    path: Path,
    root: Path,
    last: Option<Qstr>,
    last_type: LastType,
    inode: Option<Arc<VfsInode>>,
    state: u32,
    depth: usize,
    total_link_count: usize,
    stack: Vec<PathBuf>,
    name: &'a Filename,
    pathname: &'a str,
    dfd: i32,
    intent: LookupIntent,
    flags: LookupFlags,
}

impl<'a> Nameidata<'a> {
    fn new(
        root: &Path,
        base: &Path,
        filename: &'a Filename,
        intent: LookupIntent,
        flags: LookupFlags,
    ) -> Self {
        let pathname = filename.as_pathname();
        let path = if pathname.is_absolute() {
            root.clone()
        } else {
            base.clone()
        };
        Self {
            path,
            root: root.clone(),
            last: None,
            last_type: LastType::Root,
            inode: None,
            state: 0,
            depth: 0,
            total_link_count: 0,
            stack: Vec::new(),
            name: filename,
            pathname: pathname.as_str(),
            dfd: AT_FDCWD,
            intent,
            flags,
        }
    }

    fn path_init(&mut self, base: &Path) {
        self.pathname = self.name.as_pathname().as_str();
        self.state |= ND_ROOT_PRESET;
        let starts_at_root = self.pathname.starts_with('/') && self.state & ND_ROOT_PRESET != 0;
        let starts_at_base = self.dfd == AT_FDCWD || !starts_at_root;
        self.path = if starts_at_root {
            self.root.clone()
        } else if starts_at_base {
            base.clone()
        } else {
            self.path.clone()
        };
        self.inode = Some(d_inode(self.path.dentry()).clone());
    }

    fn follow_flags_for_intermediate(&self) -> LookupFlags {
        self.flags | LookupFlags::FOLLOW_FINAL
    }

    fn has_trailing_slashes(&self) -> bool {
        self.last.as_ref().is_some_and(Qstr::has_trailing_slashes)
    }

    fn reserve_link_follow(&mut self) -> VfsResult<()> {
        if self.total_link_count >= DEFAULT_MAX_LINKS {
            return Err(VfsError::FilesystemLoop);
        }
        self.total_link_count += 1;
        Ok(())
    }

    fn follow_symlink(&mut self, containing_dir: &Path, link: Path) -> VfsResult<Path> {
        self.reserve_link_follow()?;
        let target = d_inode(link.dentry()).read_link(link.dentry())?;
        if target.is_empty() {
            return Err(VfsError::NotFound);
        }

        self.stack.push(PathBuf::from(target.as_str()));
        self.depth = self.stack.len();
        let saved_flags = self.flags;
        self.flags = self.follow_flags_for_intermediate();
        let target_name = Filename::new(target.as_str());
        let mut nd = Nameidata::new(
            &self.root,
            containing_dir,
            &target_name,
            self.intent,
            self.flags,
        );
        nd.total_link_count = self.total_link_count;
        let resolved = nd.path_lookupat(containing_dir);
        self.total_link_count = nd.total_link_count;
        self.flags = saved_flags;
        self.stack.pop();
        self.depth = self.stack.len();
        resolved
    }

    fn follow_magic_link(&mut self, link: Path) -> VfsResult<Path> {
        let Some(mut magic_link) = d_inode(link.dentry()).magic_link() else {
            return Ok(link);
        };

        loop {
            self.reserve_link_follow()?;
            let resolved =
                magic_link.follow(self.intent, self.flags | LookupFlags::FOLLOW_FINAL)?;
            match resolved {
                ResolvedObject::Path(location) => return Ok(location),
                ResolvedObject::MagicLink(next) => magic_link = next,
            }
        }
    }

    fn step_into(
        &mut self,
        containing_dir: &Path,
        next: Path,
        component: WalkComponent,
    ) -> VfsResult<Path> {
        if d_inode(next.dentry()).magic_link().is_some() {
            if self.flags.rejects_magic_links() {
                return Err(VfsError::FilesystemLoop);
            }
            if component == WalkComponent::More || self.flags.follows_final() {
                return self.follow_magic_link(next);
            }
            return Ok(next);
        }

        if d_is_symlink(next.dentry())
            && (component == WalkComponent::More || self.flags.follows_final())
        {
            return self.follow_symlink(containing_dir, next);
        }
        Ok(next)
    }

    fn set_path(&mut self, path: Path) {
        self.inode = Some(d_inode(path.dentry()).clone());
        self.path = path;
    }

    fn hash_name(&mut self, name: &str) -> Qstr {
        let qstr = Qstr::new(name);
        self.last = Some(qstr.clone());
        qstr
    }

    fn handle_dots(&mut self, last_type: LastType) {
        match last_type {
            LastType::Root => self.set_path(self.root.clone()),
            LastType::Dot => {}
            LastType::Dotdot => {
                if !self.path.ptr_eq(&self.root) {
                    let parent = self.path.parent().unwrap_or_else(|| self.root.clone());
                    self.set_path(parent);
                }
            }
            LastType::Norm => {}
        }
    }

    fn lookup_fast(&self, name: &str) -> Option<VfsResult<Path>> {
        self.path.dentry().lookup_cache(name).map(|dentry| {
            if d_is_negative(&dentry) {
                Err(VfsError::NotFound)
            } else {
                Ok(Path::new(self.path.mount().clone(), dentry))
            }
        })
    }

    fn lookup_slow(&self, name: &str) -> VfsResult<Path> {
        Ok(Path::new(
            self.path.mount().clone(),
            self.path.dentry().lookup(name)?,
        ))
    }

    fn walk_component(&mut self, name: &str, component: WalkComponent) -> VfsResult<()> {
        let qstr = self.hash_name(name);
        let name = qstr.as_str();
        if !d_is_dir(self.path.dentry()) {
            return Err(VfsError::NotADirectory);
        }
        let containing_dir = self.path.clone();
        let next = match self.lookup_fast(name) {
            Some(dentry) => dentry?,
            None => self.lookup_slow(name)?,
        }
        .resolve_final_mount();
        let next = self.step_into(&containing_dir, next, component)?;
        self.set_path(next);
        Ok(())
    }

    fn next_path_component<'p>(
        pathname: &mut &'p str,
        at_start: &mut bool,
    ) -> Option<(LastType, &'p str)> {
        loop {
            if (*pathname).is_empty() {
                return None;
            }
            let current = *pathname;
            let (component, rest) = match current.find('/') {
                Some(index) => (&current[..index], &current[index + 1..]),
                None => (current, ""),
            };
            *pathname = rest;
            let result = match component {
                "" if *at_start => Some((LastType::Root, "/")),
                "" => None,
                "." if *at_start => Some((LastType::Dot, ".")),
                "." => None,
                ".." => Some((LastType::Dotdot, "..")),
                name => Some((LastType::Norm, name)),
            };
            *at_start = false;
            if let Some(result) = result {
                return Some(result);
            }
        }
    }

    fn has_more_path_components(pathname: &str, at_start: bool) -> bool {
        let mut pathname = pathname;
        let mut at_start = at_start;
        Self::next_path_component(&mut pathname, &mut at_start).is_some()
    }

    fn walk_path_component(
        &mut self,
        last_type: LastType,
        name: &str,
        walk_component: WalkComponent,
    ) -> VfsResult<()> {
        if last_type == LastType::Norm {
            self.last_type = LastType::Norm;
            return match walk_component {
                WalkComponent::More => self.walk_component(name, WalkComponent::More),
                WalkComponent::Final => {
                    self.hash_name(name);
                    Ok(())
                }
            };
        }
        self.last_type = last_type;
        self.hash_name(name);
        if walk_component == WalkComponent::More {
            self.handle_dots(last_type);
        }
        Ok(())
    }

    fn link_path_walk(&mut self, base: &Path) -> VfsResult<Path> {
        if self.pathname.is_empty() {
            if self.flags.contains(LookupFlags::EMPTY_PATH) {
                return Ok(base.clone());
            }
            return Err(VfsError::NotFound);
        }

        self.path_init(base);
        let mut pathname = self.pathname;
        let mut at_start = true;

        while let Some((last_type, name)) = Self::next_path_component(&mut pathname, &mut at_start)
        {
            if !Self::has_more_path_components(pathname, at_start) {
                self.walk_path_component(last_type, name, WalkComponent::Final)?;
                return Ok(self.path.clone());
            }
            self.walk_path_component(last_type, name, WalkComponent::More)?;
        }

        Ok(self.path.clone())
    }

    fn path_lookupat(&mut self, base: &Path) -> VfsResult<Path> {
        let path = self.link_path_walk(base)?;
        if self.pathname.is_empty() {
            return Ok(path);
        }
        match self.last_type {
            LastType::Root | LastType::Dot | LastType::Dotdot => {
                self.handle_dots(self.last_type);
            }
            LastType::Norm => {
                let name = self
                    .last
                    .as_ref()
                    .map(Qstr::as_str)
                    .ok_or(VfsError::InvalidInput)?
                    .to_owned();
                self.walk_component(&name, WalkComponent::Final)?;
            }
        }
        Ok(self.path.clone())
    }

    fn parent_lookup(
        root: &Path,
        base: &Path,
        name: &'a Filename,
        intent: LookupIntent,
    ) -> VfsResult<ParentLookup> {
        let mut nd = Nameidata::new(root, base, name, intent, LookupFlags::follow());
        let pathname = name.as_pathname();
        let pathname = pathname.as_str();
        if pathname.is_empty() {
            return Err(VfsError::NotFound);
        }

        let mut end = pathname.len();
        while end > 0 && pathname.as_bytes()[end - 1] == b'/' {
            end -= 1;
        }

        if end == 0 {
            nd.last = Some(Qstr::new_len("/", 1));
            nd.last_type = LastType::Root;
            return Ok(ParentLookup {
                parent: nd.root.clone(),
                last: nd.last.as_ref().ok_or(VfsError::InvalidInput)?.clone(),
                last_type: nd.last_type,
                has_trailing_slash: nd.has_trailing_slashes(),
            });
        }

        let start = pathname[..end]
            .rfind('/')
            .map(|index| index + 1)
            .unwrap_or(0);
        let raw_last = &pathname[start..];
        let last_len = end - start;
        let last = &raw_last[..last_len];
        let last_type = match last {
            "." => LastType::Dot,
            ".." => LastType::Dotdot,
            _ => LastType::Norm,
        };

        let parent = if start == 0 {
            if name.as_pathname().is_absolute() {
                root.clone()
            } else {
                base.clone()
            }
        } else {
            let parent_name = if start == 1 {
                "/"
            } else {
                &pathname[..start - 1]
            };
            let parent_filename = Filename::new(parent_name);
            let mut parent_nd =
                Nameidata::new(root, base, &parent_filename, intent, LookupFlags::follow());
            parent_nd.total_link_count = nd.total_link_count;
            let parent = parent_nd.path_lookupat(base)?;
            nd.total_link_count = parent_nd.total_link_count;
            parent
        };
        if !d_is_dir(parent.dentry()) {
            return Err(VfsError::NotADirectory);
        }
        nd.last_type = last_type;
        nd.last = Some(Qstr::new_len(raw_last, last_len));
        Ok(ParentLookup {
            parent,
            last: nd.last.as_ref().ok_or(VfsError::InvalidInput)?.clone(),
            last_type: nd.last_type,
            has_trailing_slash: nd.has_trailing_slashes(),
        })
    }

    fn readlink(root: &Path, base: &Path, name: &Filename) -> VfsResult<String> {
        let location =
            name.lookup_at(root, base, LookupIntent::Readlink, LookupFlags::no_follow())?;
        if let Some(link) = d_inode(location.dentry()).magic_link() {
            link.readlink_display()
        } else {
            d_inode(location.dentry()).read_link(location.dentry())
        }
    }
}

/// Result of path-level open policy checks.
#[derive(Clone, Copy, Debug)]
pub(crate) struct MayOpenResult {
    pub(crate) truncate: bool,
}

impl<'a> Nameidata<'a> {
    fn may_open(path: &Path, acc_mode: AccMode, open_flags: OpenFlags) -> VfsResult<MayOpenResult> {
        let f_mode = FMode::from_open_flags(open_flags);
        if f_mode.contains(FMode::PATH) || open_flags.contains(OpenFlags::PATH) {
            return Ok(MayOpenResult { truncate: false });
        }

        if d_is_symlink(path.dentry()) {
            return Err(VfsError::FilesystemLoop);
        }
        if open_flags.contains(OpenFlags::DIRECTORY) && !d_is_dir(path.dentry()) {
            return Err(VfsError::NotADirectory);
        }

        let node_type = d_inode(path.dentry()).node_type();
        let truncate =
            open_flags.contains(OpenFlags::TRUNCATE) && node_type == NodeType::RegularFile;
        if d_is_dir(path.dentry())
            && (acc_mode.requires_write() || open_flags.contains(OpenFlags::TRUNCATE))
        {
            return Err(VfsError::IsADirectory);
        }
        if matches!(node_type, NodeType::CharacterDevice | NodeType::BlockDevice)
            && path.mount().flags().contains(MountFlags::NODEV)
        {
            return Err(VfsError::PermissionDenied);
        }
        if f_mode.contains(FMode::EXEC)
            && path.mount().flags().contains(MountFlags::NOEXEC)
            && node_type == NodeType::RegularFile
        {
            return Err(VfsError::PermissionDenied);
        }
        if acc_mode.requires_write() || open_flags.contains(OpenFlags::APPEND) || truncate {
            path.check_writable_mount()?;
        }
        Ok(MayOpenResult { truncate })
    }

    fn check_resolved_open(path: &Path, flags: &OpenParams, was_created: bool) -> VfsResult<()> {
        if flags.will_create() {
            if flags.is_exclusive_create() && !was_created {
                return Err(VfsError::AlreadyExists);
            }
            if d_is_dir(path.dentry()) {
                return Err(VfsError::IsADirectory);
            }
        }
        if flags.lookup_flags().contains(LookupFlags::DIRECTORY) && !d_is_dir(path.dentry()) {
            return Err(VfsError::NotADirectory);
        }
        Ok(())
    }

    fn handle_truncate(file: &VfsFile, may_open: &MayOpenResult) -> VfsResult<()> {
        if !may_open.truncate {
            return Ok(());
        }
        file.path().truncate(0)
    }

    fn may_open_resolved(
        path: &Path,
        flags: &OpenParams,
        was_created: bool,
    ) -> VfsResult<MayOpenResult> {
        Self::check_resolved_open(path, flags, was_created)?;
        let (open_flag, acc_mode) = flags.may_open_args(was_created);
        Self::may_open(path, acc_mode, open_flag)
    }

    fn lookup_fast_for_open(&mut self, flags: &OpenParams) -> VfsResult<Option<Path>> {
        if flags.will_create() {
            if self.has_trailing_slashes() {
                return Err(VfsError::IsADirectory);
            }
            if flags.is_exclusive_create() {
                return Ok(None);
            }
        }

        if self.has_trailing_slashes() {
            self.flags |= LookupFlags::FOLLOW_FINAL | LookupFlags::DIRECTORY;
        }

        let name = self
            .last
            .as_ref()
            .map(Qstr::as_str)
            .ok_or(VfsError::InvalidInput)?;
        let Some(result) = self.lookup_fast(name) else {
            return Ok(None);
        };
        match result {
            Ok(path) => Ok(Some(path)),
            Err(err) if err.canonicalize() == VfsError::NotFound && flags.will_create() => Ok(None),
            Err(err) => Err(err),
        }
    }

    fn lookup_open(
        &mut self,
        file: &mut VfsFileBuilder,
        flags: &OpenParams,
        got_write: bool,
    ) -> VfsResult<Path> {
        file.clear_created();

        let name = self
            .last
            .as_ref()
            .map(Qstr::as_str)
            .ok_or(VfsError::InvalidInput)?;
        let dir = self.path.dentry().as_dir()?;
        match dir.lookup(name) {
            Ok(entry) => return Ok(self.path.with_dentry(entry)),
            Err(err) if err.canonicalize() == VfsError::NotFound => {}
            Err(err) => return Err(err),
        }

        if !flags.will_create() {
            return Err(VfsError::NotFound);
        }
        if !got_write {
            return Err(VfsError::ReadOnlyFilesystem);
        }

        let inode = dir.vfs_inode();
        let entry = inode.create_with_mode(dir, name, flags.mode(), flags.is_exclusive_create())?;
        dir.insert_cache(name.to_owned(), entry.clone());
        file.mark_created();
        Ok(self.path.with_dentry(entry))
    }

    fn open_last_lookups(
        &mut self,
        file: &mut VfsFileBuilder,
        flags: &OpenParams,
    ) -> VfsResult<Option<PathBuf>> {
        if self.last_type != LastType::Norm {
            self.handle_dots(self.last_type);
            return Ok(None);
        }

        let path = if let Some(path) = self.lookup_fast_for_open(flags)? {
            path
        } else {
            let got_write = flags.needs_write_mount() && self.path.check_writable_mount().is_ok();
            self.lookup_open(file, flags, got_write)?
        };

        if file.was_created() {
            self.set_path(path.resolve_final_mount());
            return Ok(None);
        }

        let containing_dir = self.path.clone();
        let path = path.resolve_final_mount();
        if d_is_symlink(path.dentry()) && flags.lookup_flags().follows_final() {
            self.reserve_link_follow()?;
            let target = d_inode(path.dentry()).read_link(path.dentry())?;
            if target.is_empty() {
                return Err(VfsError::NotFound);
            }
            return Ok(Some(PathBuf::from(target)));
        }
        let path = self.step_into(&containing_dir, path, WalkComponent::Final)?;
        self.set_path(path);
        Ok(None)
    }

    fn open_path(
        root: &Path,
        base: &Path,
        filename: &Filename,
        mut file: VfsFileBuilder,
        flags: &OpenParams,
    ) -> VfsResult<Arc<VfsFile>> {
        let mut name = filename.clone();
        let mut walk_base = base.clone();
        let mut total_link_count = 0;
        loop {
            let mut nd = Nameidata::new(
                root,
                &walk_base,
                &name,
                LookupIntent::Open,
                flags.lookup_flags(),
            );
            nd.total_link_count = total_link_count;
            nd.link_path_walk(&walk_base)?;
            let Some(next) = nd.open_last_lookups(&mut file, flags)? else {
                let path = nd.path.clone();
                let may_open = Self::may_open_resolved(&path, flags, file.was_created())?;
                let opened = file.vfs_open(path)?;
                Self::handle_truncate(&opened, &may_open)?;
                return Ok(opened);
            };
            walk_base = if next.is_absolute() {
                root.clone()
            } else {
                nd.path.clone()
            };
            total_link_count = nd.total_link_count;
            name = Filename::new(next);
        }
    }

    fn do_o_path(
        root: &Path,
        base: &Path,
        filename: &Filename,
        file: VfsFileBuilder,
        flags: &OpenParams,
    ) -> VfsResult<Arc<VfsFile>> {
        let path = filename.lookup_at(root, base, LookupIntent::Open, flags.lookup_flags())?;
        file.vfs_open(path)
    }

    fn path_openat(
        root: &Path,
        base: &Path,
        filename: &Filename,
        flags: &OpenParams,
    ) -> VfsResult<Arc<VfsFile>> {
        let file = VfsFileBuilder::allocate(flags.file_flags())?;
        if flags.is_path() {
            Self::do_o_path(root, base, filename, file, flags)
        } else {
            Self::open_path(root, base, filename, file, flags)
        }
    }
}

/// Opens an already resolved VFS location.
pub fn dentry_open(path: Path, flags: u32) -> VfsResult<Arc<VfsFile>> {
    let flags = OpenFlags::from_bits(flags).ok_or(VfsError::InvalidInput)?;
    VfsFileBuilder::allocate(flags)?.vfs_open(path)
}

impl Filename {
    /// Opens this filename from raw `O_*` flag bits and creation permissions.
    pub fn open_with_flags_at(
        &self,
        root: &Path,
        base: &Path,
        flags: u32,
        mode: NodePermission,
    ) -> VfsResult<Arc<VfsFile>> {
        let flags = OpenHow::from_legacy(flags, mode).into_open_params()?;
        Nameidata::path_openat(root, base, self, &flags)
    }

    /// Resolves this filename relative to `root` and `base`.
    pub fn lookup_at(
        &self,
        root: &Path,
        base: &Path,
        intent: LookupIntent,
        flags: LookupFlags,
    ) -> VfsResult<Path> {
        Nameidata::new(root, base, self, intent, flags).path_lookupat(base)
    }

    /// Resolves this filename to its parent directory and final component.
    pub fn parent_at(
        &self,
        root: &Path,
        base: &Path,
        intent: LookupIntent,
    ) -> VfsResult<ParentLookup> {
        Nameidata::parent_lookup(root, base, self, intent)
    }

    /// Resolves this filename to the parent directory of a to-be-created component.
    pub fn create_at(
        &self,
        root: &Path,
        base: &Path,
        intent: LookupIntent,
        lookup_flags: LookupFlags,
    ) -> VfsResult<(Path, String)> {
        let lookup = self.parent_at(root, base, intent)?;
        lookup.prepare_create(self, lookup_flags)
    }

    /// Reads the display target of this symlink or magic link.
    pub fn readlink_at(&self, root: &Path, base: &Path) -> VfsResult<String> {
        Nameidata::readlink(root, base, self)
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::{string::String, sync::Arc, vec::Vec};
    use core::time::Duration;

    use hashbrown::HashMap;
    use unittest::{assert, assert_eq, def_test};

    use super::*;
    use crate::{
        Dentry, DirContext, FileDirOperations, FileOperations, InodeDirOperations, InodeOperations,
        InodeSymlinkOperations, MagicLinkOps, Metadata, MetadataUpdate, Mount, NodePermission,
        NodeType, ResolvedObject, StatFs, SuperBlock, SuperBlockOperations, VfsError, VfsFile,
        VfsInode, VfsInodeInit,
    };

    #[def_test]
    fn lookup_flags_are_typed_not_raw_bools() {
        let flags = LookupFlags::follow() | LookupFlags::NO_MAGIC_LINKS;
        assert!(flags.follows_final());
        assert!(flags.rejects_magic_links());
        assert_eq!(DEFAULT_MAX_LINKS, 40);
    }

    struct TestSuperBlock {
        root: Dentry,
    }

    impl SuperBlockOperations for TestSuperBlock {
        fn name(&self) -> &str {
            "namei-test"
        }

        fn root_dentry(&self) -> Dentry {
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
                mount_flags: crate::StatFsFlags::empty(),
            })
        }
    }

    struct TestDir {
        inode: u64,
        children: crate::Mutex<HashMap<String, Dentry>>,
    }

    impl TestDir {
        fn new(inode: u64) -> Self {
            Self {
                inode,
                children: crate::Mutex::default(),
            }
        }

        fn insert(&self, name: &str, entry: Dentry) {
            self.children.lock().insert(String::from(name), entry);
        }
    }

    impl InodeOperations for TestDir {
        fn directory_operations(&self) -> Option<&dyn InodeDirOperations> {
            Some(self)
        }

        fn getattr(
            &self,
            _idmap: &crate::MountIdmap,
            _path: Option<&crate::Path>,
            _request_mask: crate::GetattrRequestMask,
            _query_flags: crate::GetattrQueryFlags,
        ) -> VfsResult<Metadata> {
            Ok(kstat(self.inode, NodeType::Directory, 0))
        }

        fn setattr(
            &self,
            _idmap: &crate::MountIdmap,
            _dentry: &Dentry,
            _update: MetadataUpdate,
        ) -> VfsResult<()> {
            Ok(())
        }
    }

    impl InodeDirOperations for TestDir {
        fn lookup(
            &self,
            _dir: &VfsInode,
            dentry: &Dentry,
            _flags: crate::InodeLookupFlags,
        ) -> VfsResult<Dentry> {
            self.children
                .lock()
                .get(dentry.name())
                .cloned()
                .ok_or(VfsError::NotFound)
        }

        fn create(
            &self,
            _idmap: &crate::MountIdmap,
            _dir: &VfsInode,
            _dentry: &Dentry,
            _mode: crate::Umode,
            _exclusive: bool,
        ) -> VfsResult<Dentry> {
            Err(VfsError::OperationNotSupported)
        }

        fn link(
            &self,
            _old_dentry: &Dentry,
            _dir: &VfsInode,
            _new_dentry: &Dentry,
        ) -> VfsResult<Dentry> {
            Err(VfsError::OperationNotSupported)
        }

        fn unlink(&self, _dir: &VfsInode, _dentry: &Dentry) -> VfsResult<()> {
            Err(VfsError::OperationNotSupported)
        }

        fn rename(
            &self,
            _idmap: &crate::MountIdmap,
            _old_dir: &VfsInode,
            _old_dentry: &Dentry,
            _new_dir: &VfsInode,
            _new_dentry: &Dentry,
            _flags: crate::RenameFlags,
        ) -> VfsResult<()> {
            Err(VfsError::OperationNotSupported)
        }
    }

    impl FileOperations for TestDir {
        fn dir_operations(&self) -> Option<&dyn FileDirOperations> {
            Some(self)
        }

        fn supports_read(&self) -> bool {
            true
        }

        fn read(&self, _file: &VfsFile, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
            Err(VfsError::IsADirectory)
        }
    }

    impl FileDirOperations for TestDir {
        fn iterate_shared(
            &self,
            _file: &crate::VfsFile,
            _ctx: &mut DirContext<'_>,
        ) -> VfsResult<usize> {
            Ok(0)
        }
    }

    struct TestFile {
        inode: u64,
        node_type: NodeType,
        data: crate::Mutex<Vec<u8>>,
        magic_link: Option<Arc<TestMagicLink>>,
    }

    impl TestFile {
        fn new(
            inode: u64,
            node_type: NodeType,
            data: &[u8],
            magic_link: Option<Arc<TestMagicLink>>,
        ) -> Self {
            Self {
                inode,
                node_type,
                data: crate::Mutex::new(data.to_vec()),
                magic_link,
            }
        }
    }

    impl InodeOperations for TestFile {
        fn symlink_operations(&self) -> Option<&dyn InodeSymlinkOperations> {
            if self.node_type == NodeType::Symlink {
                Some(self)
            } else {
                None
            }
        }

        fn getattr(
            &self,
            _idmap: &crate::MountIdmap,
            _path: Option<&crate::Path>,
            _request_mask: crate::GetattrRequestMask,
            _query_flags: crate::GetattrQueryFlags,
        ) -> VfsResult<Metadata> {
            Ok(kstat(
                self.inode,
                self.node_type,
                self.data.lock().len() as u64,
            ))
        }

        fn setattr(
            &self,
            _idmap: &crate::MountIdmap,
            _dentry: &Dentry,
            _update: MetadataUpdate,
        ) -> VfsResult<()> {
            Ok(())
        }
    }

    impl InodeSymlinkOperations for TestFile {
        fn get_link(
            &self,
            _dentry: Option<&Dentry>,
            _inode: &VfsInode,
            _done: &mut crate::DelayedCall,
        ) -> VfsResult<String> {
            String::from_utf8(self.data.lock().clone()).map_err(|_| VfsError::InvalidData)
        }
    }

    impl FileOperations for TestFile {
        fn supports_read(&self) -> bool {
            true
        }

        fn supports_write(&self) -> bool {
            true
        }

        fn read(&self, _file: &VfsFile, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
            let data = self.data.lock();
            let offset = offset as usize;
            if offset >= data.len() {
                return Ok(0);
            }
            let count = buf.len().min(data.len() - offset);
            buf[..count].copy_from_slice(&data[offset..offset + count]);
            Ok(count)
        }

        fn write(&self, _file: &VfsFile, buf: &[u8], offset: u64) -> VfsResult<usize> {
            let offset = offset as usize;
            let mut data = self.data.lock();
            if offset + buf.len() > data.len() {
                data.resize(offset + buf.len(), 0);
            }
            data[offset..offset + buf.len()].copy_from_slice(buf);
            Ok(buf.len())
        }

        fn magic_link(self: Arc<Self>) -> Option<Arc<dyn MagicLinkOps>> {
            self.magic_link
                .clone()
                .map(|link| link as Arc<dyn MagicLinkOps>)
        }
    }

    struct TestMagicLink {
        display: String,
        target: Path,
        last_intent: crate::Mutex<Option<LookupIntent>>,
        follow_count: crate::Mutex<usize>,
    }

    impl TestMagicLink {
        fn new(display: &str, target: Path) -> Self {
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
        root: Path,
        base: Path,
        target: Path,
        magic_file: Arc<TestMagicLink>,
        magic_dir: Arc<TestMagicLink>,
    }

    fn kstat(inode: u64, node_type: NodeType, size: u64) -> Metadata {
        Metadata {
            device: 0,
            inode,
            nlink: 1,
            mode: crate::Umode::new(node_type, NodePermission::default()),
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
        parent: &Dentry,
        name: &str,
        node_type: NodeType,
        data: &[u8],
        magic_link: Option<Arc<TestMagicLink>>,
    ) -> Dentry {
        let inode = VfsInode::new_file(
            Arc::new(TestFile::new(inode, node_type, data, magic_link)),
            inode_init(inode, node_type, data.len() as u64),
        );
        Dentry::new_file_from_inode(inode, Some(parent.clone()), String::from(name))
    }

    fn dir_entry(inode: u64, parent: &Dentry, name: &str) -> (Dentry, Arc<TestDir>) {
        let ops = Arc::new(TestDir::new(inode));
        let inode =
            VfsInode::new_openable_dir(ops.clone(), inode_init(inode, NodeType::Directory, 0));
        let entry = Dentry::new_dir_from_inode(inode, Some(parent.clone()), String::from(name));
        (entry, ops)
    }

    fn inode_init(inode: u64, node_type: NodeType, size: u64) -> VfsInodeInit {
        VfsInodeInit::new(
            inode,
            size,
            crate::Umode::new(node_type, NodePermission::default()),
        )
        .with_owner_links_and_rdev(0, 0, 1, Default::default())
        .with_stat_data(4096, 1, Duration::ZERO, Duration::ZERO, Duration::ZERO)
    }

    fn test_tree() -> TestTree {
        let root_ops = Arc::new(TestDir::new(1));
        let root_inode =
            VfsInode::new_openable_dir(root_ops.clone(), inode_init(1, NodeType::Directory, 0));
        let root = Dentry::new_dir_from_inode(root_inode, None, String::new());
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

        let fs = SuperBlock::new(Arc::new(TestSuperBlock { root: root.clone() }));
        let mount = Mount::new_root(&fs);
        let root_location = mount.root_path();
        let target_location = Path::new(mount.clone(), target);
        let target_dir_location = Path::new(mount.clone(), target_dir);

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
            root: root_location.clone(),
            base: root_location,
            target: target_location,
            magic_file,
            magic_dir,
        }
    }

    #[def_test]
    fn negative_cached_dentry_is_not_a_pathwalk_target() {
        let tree = test_tree();
        let negative =
            Dentry::new_negative(Some(tree.root.dentry().clone()), String::from("missing"));
        tree.root
            .dentry()
            .insert_cache(String::from("missing"), negative);

        let err = Filename::new("/missing")
            .lookup_at(
                &tree.root,
                &tree.base,
                LookupIntent::Open,
                LookupFlags::follow(),
            )
            .unwrap_err();
        assert_eq!(err, VfsError::NotFound);
    }

    #[def_test]
    fn parent_at_preserves_root_final_type() {
        let tree = test_tree();

        let lookup = Filename::new("/")
            .parent_at(&tree.root, &tree.base, LookupIntent::Open)
            .unwrap();

        assert!(lookup.parent().ptr_eq(&tree.root));
        assert_eq!(lookup.name(), "/");
        assert_eq!(lookup.last_type(), LastType::Root);
    }

    #[def_test]
    fn create_at_reports_existing_root() {
        let tree = test_tree();

        let err = Filename::new("/")
            .create_at(
                &tree.root,
                &tree.base,
                LookupIntent::Open,
                LookupFlags::empty(),
            )
            .unwrap_err();

        assert_eq!(err, VfsError::AlreadyExists);
    }

    #[def_test]
    fn create_at_preserves_trailing_slash_policy() {
        let tree = test_tree();

        let err = Filename::new("/missing/")
            .create_at(
                &tree.root,
                &tree.base,
                LookupIntent::Open,
                LookupFlags::empty(),
            )
            .unwrap_err();
        assert_eq!(err, VfsError::NotFound);

        let (parent, name) = Filename::new("/missing/")
            .create_at(
                &tree.root,
                &tree.base,
                LookupIntent::Open,
                LookupFlags::DIRECTORY,
            )
            .unwrap();
        assert!(parent.ptr_eq(&tree.root));
        assert_eq!(name, "missing");
    }

    #[def_test]
    fn final_symlink_follow_policy_is_typed() {
        let tree = test_tree();

        let followed = Filename::new("/link")
            .lookup_at(
                &tree.root,
                &tree.base,
                LookupIntent::Open,
                LookupFlags::follow(),
            )
            .unwrap();
        assert!(followed.ptr_eq(&tree.target));

        let link = Filename::new("/link")
            .lookup_at(
                &tree.root,
                &tree.base,
                LookupIntent::Open,
                LookupFlags::no_follow(),
            )
            .unwrap();
        assert_eq!(link.dentry().node_type(), NodeType::Symlink);
        assert_eq!(link.dentry().read_link().unwrap(), "/target");
    }

    #[def_test]
    fn non_final_symlink_is_followed_even_when_final_no_follow() {
        let tree = test_tree();

        let leaf = Filename::new("/dirlink/leaf")
            .lookup_at(
                &tree.root,
                &tree.base,
                LookupIntent::Open,
                LookupFlags::no_follow(),
            )
            .unwrap();
        assert_eq!(leaf.name(), "leaf");
        assert_eq!(leaf.dentry().node_type(), NodeType::RegularFile);
    }

    #[def_test]
    fn magic_link_follow_uses_lookup_intent() {
        let tree = test_tree();

        let followed = Filename::new("/magic")
            .lookup_at(
                &tree.root,
                &tree.base,
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

        let link = Filename::new("/magic")
            .lookup_at(
                &tree.root,
                &tree.base,
                LookupIntent::Open,
                LookupFlags::no_follow(),
            )
            .unwrap();
        assert_eq!(link.dentry().node_type(), NodeType::Symlink);
        assert_eq!(tree.magic_file.follow_count(), 0);

        assert_eq!(
            Filename::new("/magic")
                .readlink_at(&tree.root, &tree.base)
                .unwrap(),
            "/display/target"
        );
        assert_eq!(tree.magic_file.follow_count(), 0);
    }

    #[def_test]
    fn magic_link_rejection_applies_to_final_and_non_final_components() {
        let tree = test_tree();

        let err = Filename::new("/magic")
            .lookup_at(
                &tree.root,
                &tree.base,
                LookupIntent::Open,
                LookupFlags::follow() | LookupFlags::NO_MAGIC_LINKS,
            )
            .unwrap_err();
        assert_eq!(err, VfsError::FilesystemLoop);

        let err = Filename::new("/magicdir/leaf")
            .lookup_at(
                &tree.root,
                &tree.base,
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

        let leaf = Filename::new("/magicdir/leaf")
            .lookup_at(
                &tree.root,
                &tree.base,
                LookupIntent::Stat,
                LookupFlags::no_follow(),
            )
            .unwrap();
        assert_eq!(leaf.name(), "leaf");
        assert_eq!(tree.magic_dir.last_intent(), Some(LookupIntent::Stat));
    }
}
