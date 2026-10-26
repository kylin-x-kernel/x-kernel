// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Pathname lookup.
//!
//! This module owns VFS pathname walking policy. Callers provide process root,
//! lookup base, typed [`LookupFlags`], and [`LookupIntent`]; namei then applies
//! the common final-component, symlink, and magic-link rules.

use alloc::{borrow::ToOwned, string::String, sync::Arc, vec::Vec};

use kcred::Cred;

use crate::{
    AccMode, DeviceId, FMode, Filename, LookupFlags, LookupIntent, MountFlags, NodePermission,
    NodeType, OpenFlags, OpenHow, OpenParams, Path, ResolvedObject, Umode, VfsError, VfsFile,
    VfsFileBuilder, VfsInode, VfsResult, d_inode, d_is_dir, d_is_negative, d_is_symlink,
    node::LookupCreateResult, path::PathBuf,
};

/// Deferred cleanup context used while resolving symbolic-link targets.
#[derive(Debug, Default)]
pub struct DelayedCall;

/// Default maximum number of symbolic or magic-link follows in one lookup.
pub(crate) const DEFAULT_MAX_LINKS: usize = 40;

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

    fn into_create(self) -> VfsResult<(Path, Qstr, bool)> {
        if self.last_type != LastType::Norm {
            return Err(VfsError::AlreadyExists);
        }
        Ok((self.parent, self.last, self.has_trailing_slash))
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
        self.name.as_pathname().as_str().ends_with('/')
    }

    fn reserve_link_follow(&mut self) -> VfsResult<()> {
        if self.total_link_count >= DEFAULT_MAX_LINKS {
            return Err(VfsError::FilesystemLoop);
        }
        self.total_link_count += 1;
        Ok(())
    }

    fn follow_symlink(
        &mut self,
        containing_dir: &Path,
        link: Path,
        cred: &Cred,
    ) -> VfsResult<Path> {
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
        let resolved = nd.path_lookupat(containing_dir, cred);
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
        cred: &Cred,
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
            return self.follow_symlink(containing_dir, next, cred);
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

    fn walk_component(
        &mut self,
        name: &str,
        component: WalkComponent,
        cred: &Cred,
    ) -> VfsResult<()> {
        let qstr = self.hash_name(name);
        let name = qstr.as_str();
        // The container's directory-ness is owned by `link_path_walk`'s
        // `d_can_lookup` check, which runs for every component before it is
        // walked (and before the search permission), mirroring Linux.
        let containing_dir = self.path.clone();
        let next = match self.lookup_fast(name) {
            Some(dentry) => dentry?,
            None => self.lookup_slow(name)?,
        }
        .resolve_final_mount();
        let next = self.step_into(&containing_dir, next, component, cred)?;
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
                // A "." is a component wherever it appears: Linux keeps it (leading or
                // not), so the walk still verifies that the component it follows
                // sits in a directory (`d_can_lookup` -> ENOTDIR). Dropping a
                // non-leading "." made `file/.` resolve as `file` and succeed.
                "." => Some((LastType::Dot, ".")),
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
        cred: &Cred,
    ) -> VfsResult<()> {
        if last_type == LastType::Norm {
            self.last_type = LastType::Norm;
            return match walk_component {
                WalkComponent::More => self.walk_component(name, WalkComponent::More, cred),
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

    fn link_path_walk(&mut self, base: &Path, cred: &Cred) -> VfsResult<Path> {
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
            let has_more = Self::has_more_path_components(pathname, at_start);
            if last_type != LastType::Root {
                // The component being resolved must sit in a directory, and
                // Linux checks that before the search permission: `d_can_lookup`
                // reports ENOTDIR first, `may_lookup` only then EACCES
                // (fs/namei.c `link_path_walk`). Without this ordering a path
                // like `file/child` or `file/.` failed the MAY_EXEC check on the
                // regular file itself and surfaced EACCES instead.
                if !self.path.dentry().can_lookup() {
                    return Err(VfsError::NotADirectory);
                }
                self.path.permission(crate::Permission::MAY_EXEC, cred)?;
            }
            if !has_more {
                self.walk_path_component(last_type, name, WalkComponent::Final, cred)?;
                return Ok(self.path.clone());
            }
            self.walk_path_component(last_type, name, WalkComponent::More, cred)?;
        }

        Ok(self.path.clone())
    }

    fn path_lookupat(&mut self, base: &Path, cred: &Cred) -> VfsResult<Path> {
        let path = self.link_path_walk(base, cred)?;
        if self.pathname.is_empty() {
            return self.complete_path_lookup(path);
        }
        if self.last_type == LastType::Norm && self.has_trailing_slashes() {
            self.flags |= LookupFlags::FOLLOW_FINAL | LookupFlags::DIRECTORY;
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
                self.walk_component(&name, WalkComponent::Final, cred)?;
            }
        }
        self.complete_path_lookup(self.path.clone())
    }

    fn complete_path_lookup(&self, path: Path) -> VfsResult<Path> {
        if self.flags.contains(LookupFlags::DIRECTORY) && !path.dentry().can_lookup() {
            return Err(VfsError::NotADirectory);
        }
        Ok(path)
    }

    fn parent_lookup(
        root: &Path,
        base: &Path,
        name: &'a Filename,
        intent: LookupIntent,
        cred: &Cred,
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
            let parent = parent_nd.path_lookupat(base, cred)?;
            nd.total_link_count = parent_nd.total_link_count;
            parent
        };
        if !d_is_dir(parent.dentry()) {
            return Err(VfsError::NotADirectory);
        }
        parent.permission(crate::Permission::MAY_EXEC, cred)?;
        nd.last_type = last_type;
        nd.last = Some(Qstr::new_len(raw_last, last_len));
        Ok(ParentLookup {
            parent,
            last: nd.last.as_ref().ok_or(VfsError::InvalidInput)?.clone(),
            last_type: nd.last_type,
            has_trailing_slash: nd.has_trailing_slashes(),
        })
    }

    fn readlink(root: &Path, base: &Path, name: &Filename, cred: &Cred) -> VfsResult<String> {
        let location = name.lookup_at(
            root,
            base,
            LookupIntent::Readlink,
            LookupFlags::no_follow(),
            cred,
        )?;
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
    fn may_open(
        path: &Path,
        acc_mode: AccMode,
        open_flags: OpenFlags,
        cred: &Cred,
    ) -> VfsResult<MayOpenResult> {
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
        path.permission(crate::Permission::MAY_OPEN | acc_mode.permission(), cred)?;
        Ok(MayOpenResult { truncate })
    }

    fn check_resolved_open(
        path: &Path,
        flags: &OpenParams,
        lookup_flags: LookupFlags,
        was_created: bool,
    ) -> VfsResult<()> {
        if flags.will_create() {
            if flags.is_exclusive_create() && !was_created {
                return Err(VfsError::AlreadyExists);
            }
            if d_is_dir(path.dentry()) {
                return Err(VfsError::IsADirectory);
            }
        }
        // The trailing-slash demand rides the lookup flags across symlink
        // expansion (see [`Self::open_path`]) and is consumed here exactly as
        // [`Self::complete_path_lookup`] consumes it for plain lookups.
        if lookup_flags.contains(LookupFlags::DIRECTORY) && !path.dentry().can_lookup() {
            return Err(VfsError::NotADirectory);
        }
        Ok(())
    }

    fn handle_truncate(file: &VfsFile, may_open: &MayOpenResult) -> VfsResult<()> {
        if !may_open.truncate {
            return Ok(());
        }
        file.path().truncate_opened(0)
    }

    fn may_open_resolved(
        path: &Path,
        flags: &OpenParams,
        lookup_flags: LookupFlags,
        was_created: bool,
        cred: &Cred,
    ) -> VfsResult<MayOpenResult> {
        Self::check_resolved_open(path, flags, lookup_flags, was_created)?;
        let (open_flag, acc_mode) = flags.may_open_args(was_created);
        Self::may_open(path, acc_mode, open_flag, cred)
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
        umask: NodePermission,
        cred: &Cred,
    ) -> VfsResult<Path> {
        file.clear_created();

        let name = self
            .last
            .as_ref()
            .map(Qstr::as_str)
            .ok_or(VfsError::InvalidInput)?;
        let dir = self.path.dentry().as_dir()?;
        if !flags.is_exclusive_create() {
            match dir.lookup(name) {
                Ok(entry) => return Ok(self.path.with_dentry(entry)),
                Err(err) if err.canonicalize() == VfsError::NotFound => {}
                Err(err) => return Err(err),
            }
        }

        if !flags.will_create() {
            return Err(VfsError::NotFound);
        }

        match dir.lookup_or_create_with(name, flags.is_exclusive_create(), |candidate| {
            self.path.vfs_create(
                candidate,
                flags.mode(),
                umask,
                flags.is_exclusive_create(),
                cred,
            )
        })? {
            LookupCreateResult::Existing(entry) => Ok(self.path.with_dentry(entry)),
            LookupCreateResult::Created(entry) => {
                file.mark_created();
                Ok(self.path.with_dentry(entry))
            }
        }
    }

    fn open_last_lookups(
        &mut self,
        file: &mut VfsFileBuilder,
        flags: &OpenParams,
        umask: NodePermission,
        cred: &Cred,
    ) -> VfsResult<Option<PathBuf>> {
        if self.last_type != LastType::Norm {
            self.handle_dots(self.last_type);
            return Ok(None);
        }

        let path = if let Some(path) = self.lookup_fast_for_open(flags)? {
            path
        } else {
            self.lookup_open(file, flags, umask, cred)?
        };

        if file.was_created() {
            self.set_path(path.resolve_final_mount());
            return Ok(None);
        }

        let containing_dir = self.path.clone();
        let path = path.resolve_final_mount();
        if d_is_symlink(path.dentry())
            && flags.lookup_flags().follows_final()
            // Magic links (e.g. `/proc/self/exe`, `/proc/self/fd/N`) are also
            // symlinks, but must be followed through the magic-link machinery
            // (`step_into` -> `follow_magic_link`) rather than `read_link`, so
            // that `open` reaches the backing file rather than re-resolving the
            // display path (which is stale for memfd-backed exe links).
            && d_inode(path.dentry()).magic_link().is_none()
        {
            self.reserve_link_follow()?;
            let target = d_inode(path.dentry()).read_link(path.dentry())?;
            if target.is_empty() {
                return Err(VfsError::NotFound);
            }
            return Ok(Some(PathBuf::from(target)));
        }
        let path = self.step_into(&containing_dir, path, WalkComponent::Final, cred)?;
        self.set_path(path);
        Ok(None)
    }

    fn open_path(
        root: &Path,
        base: &Path,
        filename: &Filename,
        mut file: VfsFileBuilder,
        flags: &OpenParams,
        umask: NodePermission,
        cred: &Cred,
    ) -> VfsResult<Arc<VfsFile>> {
        let mut name = filename.clone();
        let mut walk_base = base.clone();
        let mut total_link_count = 0;
        // Resolution constraints accumulate in the lookup flags, as Linux keeps
        // them in `nd->flags` while it expands symlinks (`path_openat`). A
        // trailing slash demands a directory for the final resolved path, and
        // that demand cannot live in the path string: `link/` where `link`
        // points at a regular file must fail with ENOTDIR, yet the expanded
        // target ("target") carries no slash. Each hop's `Nameidata` inherits
        // the accumulated flags and contributes its own trailing-slash demand
        // through `lookup_fast_for_open`.
        let mut lookup_flags = flags.lookup_flags();
        loop {
            let mut nd = Nameidata::new(root, &walk_base, &name, LookupIntent::Open, lookup_flags);
            nd.total_link_count = total_link_count;
            nd.link_path_walk(&walk_base, cred)?;
            // `open_last_lookups` is where `lookup_fast_for_open` turns this
            // hop's trailing slash into `DIRECTORY`, so the demand is only
            // visible in `nd.flags` after it runs.
            let next = nd.open_last_lookups(&mut file, flags, umask, cred)?;
            lookup_flags |= nd.flags;
            let Some(next) = next else {
                let path = nd.path.clone();
                let may_open =
                    Self::may_open_resolved(&path, flags, lookup_flags, file.was_created(), cred)?;
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
        cred: &Cred,
    ) -> VfsResult<Arc<VfsFile>> {
        let path =
            filename.lookup_at(root, base, LookupIntent::Open, flags.lookup_flags(), cred)?;
        file.vfs_open(path)
    }

    fn path_openat(
        root: &Path,
        base: &Path,
        filename: &Filename,
        flags: &OpenParams,
        umask: NodePermission,
        cred: Arc<Cred>,
    ) -> VfsResult<Arc<VfsFile>> {
        let file = VfsFileBuilder::allocate(flags.file_flags(), cred.clone())?;
        if flags.is_path() {
            Self::do_o_path(root, base, filename, file, flags, &cred)
        } else {
            Self::open_path(root, base, filename, file, flags, umask, &cred)
        }
    }
}

/// Opens an already resolved VFS location.
pub fn dentry_open(path: Path, flags: u32, cred: Arc<Cred>) -> VfsResult<Arc<VfsFile>> {
    let flags = OpenFlags::from_bits(flags).ok_or(VfsError::InvalidInput)?;
    VfsFileBuilder::allocate(flags, cred)?.vfs_open(path)
}

impl Filename {
    /// Opens this filename from raw `O_*` flag bits and creation permissions.
    ///
    /// Create-only mount and permission errors are reported only after the
    /// locked final lookup confirms that the final component is still absent.
    pub fn open_with_flags_at(
        &self,
        root: &Path,
        base: &Path,
        flags: u32,
        mode: NodePermission,
        umask: NodePermission,
        cred: Arc<Cred>,
    ) -> VfsResult<Arc<VfsFile>> {
        let flags = OpenHow::from_legacy(flags, mode).into_open_params()?;
        Nameidata::path_openat(root, base, self, &flags, umask, cred)
    }

    /// Resolves this filename relative to `root` and `base`.
    pub fn lookup_at(
        &self,
        root: &Path,
        base: &Path,
        intent: LookupIntent,
        flags: LookupFlags,
        cred: &Cred,
    ) -> VfsResult<Path> {
        Nameidata::new(root, base, self, intent, flags).path_lookupat(base, cred)
    }

    /// Resolves this filename to its parent directory and final component.
    pub fn parent_at(
        &self,
        root: &Path,
        base: &Path,
        intent: LookupIntent,
        cred: &Cred,
    ) -> VfsResult<ParentLookup> {
        Nameidata::parent_lookup(root, base, self, intent, cred)
    }

    /// Creates a directory with the final lookup and VFS policy under the
    /// parent directory namespace lock.
    ///
    /// # Errors
    ///
    /// Returns [`VfsError::AlreadyExists`] when the final component already
    /// exists, or the pathname, mount, authorization, or filesystem error from
    /// the corresponding VFS stage.
    pub fn mkdir_at(
        &self,
        root: &Path,
        base: &Path,
        permission: NodePermission,
        umask: NodePermission,
        cred: &Cred,
    ) -> VfsResult<Path> {
        let (parent, last, _) = self
            .parent_at(root, base, LookupIntent::Open, cred)?
            .into_create()?;
        let name = last.as_str();

        parent
            .dentry()
            .as_dir()?
            .create_exclusive_with(name, |candidate| {
                parent.vfs_mkdir(candidate, permission, umask, cred)
            })
            .map(|entry| parent.with_dentry(entry))
    }

    /// Creates a node accepted by Linux `mknodat(2)` semantics.
    ///
    /// This method validates `mode` before pathname lookup. A syscall caller
    /// must additionally perform the same validation before selecting a
    /// directory file descriptor so an invalid type takes error precedence over
    /// an invalid `dirfd`.
    /// The final component lookup, directory authorization, device-node
    /// privilege check, mode preparation, and filesystem callback run as one
    /// parent-directory operation.
    ///
    /// # Errors
    ///
    /// Returns [`VfsError::OperationNotPermitted`] for a directory mode or an
    /// unprivileged device-node request, [`VfsError::InvalidInput`] for another
    /// unsupported type, and the error produced by the corresponding pathname,
    /// authorization, mount, or filesystem stage.
    pub fn mknod_at(
        &self,
        root: &Path,
        base: &Path,
        mode: Umode,
        device: DeviceId,
        umask: NodePermission,
        cred: &Cred,
    ) -> VfsResult<Path> {
        let node_type = may_mknod(mode)?;
        let mode = mode.with_node_type(node_type);
        let (parent, last, has_trailing_slash) = self
            .parent_at(root, base, LookupIntent::Open, cred)?
            .into_create()?;
        let name = last.as_str();
        let device = if matches!(node_type, NodeType::CharacterDevice | NodeType::BlockDevice) {
            device
        } else {
            DeviceId::default()
        };

        parent
            .dentry()
            .as_dir()?
            .create_exclusive_with(name, |candidate| {
                if has_trailing_slash {
                    return Err(VfsError::NotFound);
                }
                if node_type == NodeType::RegularFile {
                    parent.vfs_create(candidate, mode, umask, true, cred)
                } else {
                    parent.vfs_mknod(candidate, mode, device, umask, cred)
                }
            })
            .map(|entry| parent.with_dentry(entry))
    }

    /// Creates a symbolic link with a single locked final lookup.
    ///
    /// # Errors
    ///
    /// Returns [`VfsError::AlreadyExists`] when the destination exists,
    /// [`VfsError::NotFound`] for an absent destination with trailing slashes,
    /// or the pathname, authorization, mount, or filesystem error.
    pub fn symlink_at(
        &self,
        root: &Path,
        base: &Path,
        target: &str,
        cred: &Cred,
    ) -> VfsResult<Path> {
        let (parent, last, has_trailing_slash) = self
            .parent_at(root, base, LookupIntent::Open, cred)?
            .into_create()?;
        let name = last.as_str();

        parent
            .dentry()
            .as_dir()?
            .create_exclusive_with(name, |candidate| {
                if has_trailing_slash {
                    return Err(VfsError::NotFound);
                }
                parent.vfs_symlink(candidate, target, cred)
            })
            .map(|entry| parent.with_dentry(entry))
    }

    /// Creates a hard link with a single locked target lookup.
    ///
    /// # Errors
    ///
    /// Returns [`VfsError::AlreadyExists`] when the destination exists,
    /// [`VfsError::CrossesDevices`] when source and destination mounts differ,
    /// or the pathname, authorization, mount, or filesystem error.
    pub fn link_at(&self, root: &Path, base: &Path, source: &Path, cred: &Cred) -> VfsResult<Path> {
        let (parent, last, has_trailing_slash) = self
            .parent_at(root, base, LookupIntent::Open, cred)?
            .into_create()?;
        let name = last.as_str();

        parent
            .dentry()
            .as_dir()?
            .create_exclusive_with(name, |candidate| {
                if has_trailing_slash {
                    return Err(VfsError::NotFound);
                }
                parent.vfs_link(candidate, source, cred)
            })
            .map(|entry| parent.with_dentry(entry))
    }

    /// Unlinks a non-directory entry with a single locked final lookup.
    ///
    /// # Errors
    ///
    /// Returns the pathname, target-type, authorization, mount, or filesystem
    /// error for the final entry protected by the parent namespace lock.
    pub fn unlink_at(&self, root: &Path, base: &Path, cred: &Cred) -> VfsResult<()> {
        let lookup = self.parent_at(root, base, LookupIntent::Open, cred)?;
        if lookup.last_type != LastType::Norm {
            return Err(VfsError::IsADirectory);
        }
        lookup
            .parent
            .unlink_with_pathname(lookup.last.as_str(), lookup.has_trailing_slash, cred)
    }

    /// Removes a directory with a single locked final lookup.
    ///
    /// # Errors
    ///
    /// Returns Linux-compatible errors for root, dot, and dotdot final
    /// components, or the pathname, authorization, mount, or filesystem error.
    pub fn rmdir_at(&self, root: &Path, base: &Path, cred: &Cred) -> VfsResult<()> {
        let lookup = self.parent_at(root, base, LookupIntent::Open, cred)?;
        match lookup.last_type {
            LastType::Norm => lookup.parent.rmdir(lookup.last.as_str(), cred),
            LastType::Dotdot => Err(VfsError::DirectoryNotEmpty),
            LastType::Dot => Err(VfsError::InvalidInput),
            LastType::Root => Err(VfsError::ResourceBusy),
        }
    }

    /// Reads the display target of this symlink or magic link.
    pub fn readlink_at(&self, root: &Path, base: &Path, cred: &Cred) -> VfsResult<String> {
        Nameidata::readlink(root, base, self, cred)
    }
}

/// Validates a mode for Linux `mknod(2)` and returns its canonical node type.
///
/// # Errors
///
/// Returns [`VfsError::OperationNotPermitted`] for a directory request and
/// [`VfsError::InvalidInput`] for another unsupported type encoding.
pub fn may_mknod(mode: Umode) -> VfsResult<NodeType> {
    match mode.mknod_node_type() {
        node_type @ (NodeType::RegularFile
        | NodeType::CharacterDevice
        | NodeType::BlockDevice
        | NodeType::Fifo
        | NodeType::Socket) => Ok(node_type),
        NodeType::Directory => Err(VfsError::OperationNotPermitted),
        NodeType::Unknown | NodeType::Symlink => Err(VfsError::InvalidInput),
    }
}

unittest::path_mod_test!(tests, "../ut-tests/namei.rs");
