// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{
    borrow::Cow,
    boxed::Box,
    format,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
use core::{ffi::CStr, fmt, iter, str};

use kaddr_layout::{SIGNAL_TRAMPOLINE, USER_HEAP_BASE, USER_STACK_SIZE, USER_STACK_TOP};
use kcred::Cred;
use khal::paging::MappingFlags;
use kprocess::{AsThread, LiveAddressSpace, Process, TaskStat, procfs};
use ktask::{KtaskRef, WeakKtaskRef, current};
#[cfg(feature = "tee")]
use kvfs::DirMapping;
use kvfs::{
    CommandFile, Dentry, FileOperations, InodeOperations, InodeSymlinkOperations, LookupFlags,
    LookupIntent, MagicLinkOps, Metadata, MetadataUpdate, NodeFlags, NodePermission, NodeType,
    ResolvedObject, SeqFileInode, SeqIterator, SimpleDir, SimpleDirLookup, SimpleDirOps,
    SimpleFile, SimpleFileOperation, SimpleFs, SimpleFsNode, VfsError, VfsFile, VfsInode,
    VfsResult,
};

#[cfg(feature = "tee")]
use super::tee;
use crate::task_nodes::mounts::ProcMountIter;

/// Upgrades a weak task reference held by a procfs file closure.
///
/// Returns [`VfsError::NoSuchProcess`] (ESRCH) once the task has exited and
/// been reclaimed, matching Linux `get_proc_task()` failure semantics for
/// reads/writes on an already-open `/proc/<pid>/...` file. Lookup-time paths
/// that materialize dentries instead keep returning `NotFound` (ENOENT).
pub(super) fn upgrade_task(task_weak: &WeakKtaskRef) -> VfsResult<KtaskRef> {
    task_weak.upgrade().ok_or(VfsError::NoSuchProcess)
}

struct ProcessTaskDir {
    fs: Arc<SimpleFs>,
    process: Weak<Process>,
}

impl SimpleDirOps for ProcessTaskDir {
    fn link_count(&self) -> VfsResult<Option<u64>> {
        // Linux proc_task_getattr counts '.' and '..' plus one subdirectory
        // per unreleased group member, including a zombie leader before reap.
        // A retained fd reports base links after target release; it must not
        // switch to the caller's process or infer membership from exit flags.
        let threads = self
            .process
            .upgrade()
            .map_or(0, |process| procfs::thread_group_member_count(&process));
        Ok(Some(2 + threads as u64))
    }

    fn child_names<'a>(&'a self) -> VfsResult<Box<dyn Iterator<Item = Cow<'a, str>> + 'a>> {
        let Some(process) = self.process.upgrade() else {
            return Ok(Box::new(iter::empty()));
        };
        Ok(Box::new(
            process
                .threads()
                .into_iter()
                .map(|tid| tid.to_string().into()),
        ))
    }

    fn lookup_child(&self, lookup: SimpleDirLookup<'_>, name: &str) -> VfsResult<Dentry> {
        let process = self.process.upgrade().ok_or(VfsError::NotFound)?;
        let tid = name.parse::<u32>().map_err(|_| VfsError::NotFound)?;
        let task = procfs::thread_task(tid).map_err(|_| VfsError::NotFound)?;
        if task.as_thread().process().pid() != process.pid() {
            return Err(VfsError::NotFound);
        }

        Ok(make_thread_dir(lookup, name, self.fs.clone(), &task))
    }

    fn supports_dentry_cache(&self) -> bool {
        false
    }
}

// One status rendering is one sequential record. SeqFile owns unread bytes;
// the inode and iterator retain only a weak target, never a credential cache.
struct StatusIter {
    task: WeakKtaskRef,
}

impl SeqIterator for StatusIter {
    type Item = ();

    fn rewind(&mut self) {}

    fn start(&mut self) -> Option<Self::Item> {
        Some(())
    }

    fn next(&mut self) -> Option<Self::Item> {
        None
    }

    fn show(&self, item: &Self::Item, buf: &mut String) -> fmt::Result {
        self.try_show(item, buf).map_err(|_| fmt::Error)
    }

    fn try_show(&self, _item: &Self::Item, buf: &mut String) -> VfsResult<()> {
        let task = upgrade_task(&self.task)?;
        buf.push_str(&task_status(&task));
        Ok(())
    }
}

fn task_status(task: &KtaskRef) -> String {
    let thread = task.as_thread();
    // One objective snapshot belongs to the selected task, not the reader.
    let cred = thread.real_cred();
    format_task_status(
        &task.name(),
        thread.process().pid(),
        thread.tid() as u64,
        &cred,
    )
}

fn format_task_status(name: &str, tgid: u32, pid: u64, cred: &Cred) -> String {
    format!(
        "Name:\t{}\nTgid:\t{}\nPid:\t{}\nUid:\t{} {} {} {}\nGid:\t{} {} {} \
         {}\nCpus_allowed:\t1\nCpus_allowed_list:\t0\nMems_allowed:\t1\nMems_allowed_list:\t0\n",
        name,
        tgid,
        pid,
        cred.ruid(),
        cred.euid(),
        cred.suid(),
        cred.fsuid(),
        cred.rgid(),
        cred.egid(),
        cred.sgid(),
        cred.fsgid()
    )
}

fn format_oom_score_adj(value: i32) -> Vec<u8> {
    format!("{value}\n").into_bytes()
}

/// Linux `OOM_SCORE_ADJ_MIN` / `OOM_SCORE_ADJ_MAX`; writes outside this range
/// are rejected with `-EINVAL` (fs/proc/base.c `oom_score_adj_write()`).
const OOM_SCORE_ADJ_MIN: i32 = -1000;
const OOM_SCORE_ADJ_MAX: i32 = 1000;

fn parse_oom_score_adj_input(data: &[u8]) -> VfsResult<Option<i32>> {
    if data.is_empty() {
        return Ok(None);
    }

    str::from_utf8(data)
        .ok()
        .map(str::trim)
        .and_then(|it| it.parse::<i32>().ok())
        .map(Some)
        .ok_or(VfsError::InvalidInput)
}

fn proc_self_target() -> VfsResult<String> {
    let task = current();
    let thread = task.try_as_thread().ok_or(VfsError::NotFound)?;
    Ok(thread.process().pid().to_string())
}

fn maps_permissions(flags: MappingFlags) -> [u8; 4] {
    [
        if flags.contains(MappingFlags::READ) {
            b'r'
        } else {
            b'-'
        },
        if flags.contains(MappingFlags::WRITE) {
            b'w'
        } else {
            b'-'
        },
        if flags.contains(MappingFlags::EXECUTE) {
            b'x'
        } else {
            b'-'
        },
        if flags.contains(MappingFlags::SHARED) {
            b's'
        } else {
            b'p'
        },
    ]
}

fn special_mapping_name(start: usize, end: usize, heap_top: usize) -> Option<&'static str> {
    let heap_start = USER_HEAP_BASE;
    if start < heap_top && end > heap_start {
        return Some("[heap]");
    }

    let stack_start = USER_STACK_TOP - USER_STACK_SIZE;
    if start < USER_STACK_TOP && end > stack_start {
        return Some("[stack]");
    }

    if start <= SIGNAL_TRAMPOLINE && end > SIGNAL_TRAMPOLINE {
        return Some("[sigtramp]");
    }

    None
}

#[derive(Clone)]
struct MapsEntry {
    start: usize,
    end: usize,
    flags: MappingFlags,
    offset: u64,
    inode: u64,
    path: Option<String>,
    name: Option<&'static str>,
}

fn backend_file_mapping(vma: &memspace::VmArea) -> Option<(u64, u64, Option<String>)> {
    let info = vma.file_mapping()?;
    Some((
        vma.file_offset_for(vma.start())?,
        info.inode,
        info.path.clone(),
    ))
}

const PROC_MAPS_ADDR_WIDTH: usize = 12;
const PROC_MAPS_PATH_COLUMN: usize = 73;

fn render_maps_line(item: &MapsEntry) -> String {
    let perms = maps_permissions(item.flags);
    #[expect(
        clippy::unwrap_used,
        reason = "CG-021-no-panic-on-reachable-failure: maps_permissions emits only the ASCII \
                  permission letters r, w, x, s, p and the dash"
    )]
    let mut line = format!(
        "{:0width$x}-{:0width$x} {} {:08x} 00:00 {}",
        item.start,
        item.end,
        core::str::from_utf8(&perms).unwrap(),
        item.offset,
        item.inode,
        width = PROC_MAPS_ADDR_WIDTH,
    );

    if let Some(name) = item.name {
        if line.len() < PROC_MAPS_PATH_COLUMN {
            for _ in line.len()..PROC_MAPS_PATH_COLUMN {
                line.push(' ');
            }
        } else {
            line.push(' ');
        }
        line.push_str(name);
    } else if let Some(path) = item.path.as_deref() {
        if line.len() < PROC_MAPS_PATH_COLUMN {
            for _ in line.len()..PROC_MAPS_PATH_COLUMN {
                line.push(' ');
            }
        } else {
            line.push(' ');
        }
        line.push_str(path);
    }

    line.push('\n');
    line
}

struct MapsIter {
    aspace: LiveAddressSpace,
    entries: Vec<MapsEntry>,
    next_index: usize,
}

impl MapsIter {
    /// Retains the captured MM and its matching heap marker after process exit.
    /// Reads re-walk that MM's VMA list; same-image mapping and heap updates
    /// remain visible, while a later process binding cannot retarget this view.
    ///
    /// Fails with `NoSuchProcess` when the runtime is already detached (the
    /// process is gone even though the task may not be reclaimed yet).
    fn new(task: &KtaskRef) -> VfsResult<Self> {
        let process = task.as_thread().process();
        Ok(Self {
            aspace: process.address_space()?,
            entries: Vec::new(),
            next_index: 0,
        })
    }
}

impl SeqIterator for MapsIter {
    type Item = MapsEntry;

    fn rewind(&mut self) {
        self.entries.clear();
        self.next_index = 0;
        let aspace = self.aspace.lock();
        for vma in aspace.vmas() {
            let start = vma.start().as_usize();
            let end = vma.end().as_usize();
            let (offset, inode, path) = backend_file_mapping(vma).unwrap_or((0, 0, None));
            self.entries.push(MapsEntry {
                start,
                end,
                flags: vma.flags(),
                offset,
                inode,
                path,
                name: special_mapping_name(start, end, self.aspace.heap_top()),
            });
        }
    }

    fn start(&mut self) -> Option<Self::Item> {
        self.rewind();
        self.next()
    }

    fn next(&mut self) -> Option<Self::Item> {
        let item = self.entries.get(self.next_index).cloned();
        if item.is_some() {
            self.next_index += 1;
        }
        item
    }

    fn show(&self, item: &Self::Item, buf: &mut String) -> core::fmt::Result {
        buf.push_str(&render_maps_line(item));
        Ok(())
    }
}

struct ProcFdLink {
    node: SimpleFsNode,
    task: WeakKtaskRef,
    fd: u32,
}

impl ProcFdLink {
    fn new(fs: Arc<SimpleFs>, task: WeakKtaskRef, fd: u32) -> Arc<Self> {
        Arc::new(Self {
            node: SimpleFsNode::new(
                fs,
                NodeType::Symlink,
                NodePermission::from_bits_truncate(0o777),
            ),
            task,
            fd,
        })
    }

    fn display_target(&self) -> VfsResult<String> {
        let task = self.task.upgrade().ok_or(VfsError::NotFound)?;
        task.as_thread()
            .process()
            .resources()?
            .snapshot_fd(self.fd as _)
            .map_err(|err| {
                if err == VfsError::BadFileDescriptor {
                    VfsError::NotFound
                } else {
                    err
                }
            })?
            .path()
            .display_path()
    }

    fn target_len(&self) -> VfsResult<u64> {
        Ok(self.display_target()?.len() as u64)
    }
}

impl InodeOperations for ProcFdLink {
    fn symlink_operations(&self) -> Option<&dyn InodeSymlinkOperations> {
        Some(self)
    }

    fn getattr(
        &self,
        idmap: &kvfs::MountIdmap,
        path: Option<&kvfs::Path>,
        request_mask: kvfs::GetattrRequestMask,
        query_flags: kvfs::GetattrQueryFlags,
    ) -> VfsResult<Metadata> {
        let mut metadata = self.node.getattr(idmap, path, request_mask, query_flags)?;
        metadata.size = self.target_len()?;
        Ok(metadata)
    }

    fn setattr(
        &self,
        idmap: &kvfs::MountIdmap,
        dentry: &Dentry,
        update: MetadataUpdate,
    ) -> VfsResult<MetadataUpdate> {
        self.node.setattr(idmap, dentry, update)
    }
}

impl InodeSymlinkOperations for ProcFdLink {
    fn get_link(
        &self,
        _dentry: Option<&Dentry>,
        _inode: &kvfs::VfsInode,
        _done: &mut kvfs::DelayedCall,
    ) -> VfsResult<String> {
        self.display_target()
    }
}

impl FileOperations for ProcFdLink {
    fn supports_read(&self) -> bool {
        true
    }

    fn read(&self, _file: &VfsFile, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let target = self.display_target()?;
        let data = target.as_bytes();
        if offset >= data.len() as u64 {
            return Ok(0);
        }
        let data = &data[offset as usize..];
        let read = data.len().min(buf.len());
        buf[..read].copy_from_slice(&data[..read]);
        Ok(read)
    }

    fn magic_link(self: Arc<Self>) -> Option<Arc<dyn MagicLinkOps>> {
        Some(self)
    }
}

fn proc_fd_link_child(
    lookup: SimpleDirLookup<'_>,
    name: &str,
    link: Arc<ProcFdLink>,
) -> VfsResult<Dentry> {
    let init = link.node.inode_init().with_size(link.target_len()?);
    let inode = VfsInode::new_file_with_flags(link, NodeFlags::NON_CACHEABLE, init);
    Ok(lookup.file_from_inode(name, inode))
}

impl MagicLinkOps for ProcFdLink {
    fn readlink_display(&self) -> VfsResult<String> {
        self.display_target()
    }

    fn follow(&self, _intent: LookupIntent, flags: LookupFlags) -> VfsResult<ResolvedObject> {
        if flags.rejects_magic_links() || !flags.follows_final() {
            return Err(VfsError::FilesystemLoop);
        }

        let task = self.task.upgrade().ok_or(VfsError::NotFound)?;
        let snapshot = task
            .as_thread()
            .process()
            .resources()?
            .snapshot_fd(self.fd as _)
            .map_err(|err| {
                if err == VfsError::BadFileDescriptor {
                    VfsError::NotFound
                } else {
                    err
                }
            })?;
        Ok(ResolvedObject::location(snapshot.file().path().clone()))
    }
}

struct ThreadFdDir {
    fs: Arc<SimpleFs>,
    task: WeakKtaskRef,
}

impl SimpleDirOps for ThreadFdDir {
    fn child_names<'a>(&'a self) -> VfsResult<Box<dyn Iterator<Item = Cow<'a, str>> + 'a>> {
        let Some(task) = self.task.upgrade() else {
            return Ok(Box::new(iter::empty()));
        };
        let Ok(resources) = task.as_thread().process().resources() else {
            return Ok(Box::new(iter::empty()));
        };
        let Ok(fd_table) = resources.fd_table() else {
            return Ok(Box::new(iter::empty()));
        };
        let ids = fd_table
            .read()
            .ids()
            .map(|id| Cow::Owned(id.to_string()))
            .collect::<Vec<_>>();
        Ok(Box::new(ids.into_iter()))
    }

    fn lookup_child(&self, lookup: SimpleDirLookup<'_>, name: &str) -> VfsResult<Dentry> {
        let fs = self.fs.clone();
        let task = self.task.upgrade().ok_or(VfsError::NotFound)?;
        let fd = name.parse::<u32>().map_err(|_| VfsError::NotFound)?;
        task.as_thread()
            .process()
            .resources()?
            .snapshot_fd(fd as _)
            .map_err(|err| {
                if err == VfsError::BadFileDescriptor {
                    VfsError::NotFound
                } else {
                    err
                }
            })?;
        proc_fd_link_child(lookup, name, ProcFdLink::new(fs, Arc::downgrade(&task), fd))
    }

    fn supports_dentry_cache(&self) -> bool {
        false
    }
}

struct ThreadDir {
    fs: Arc<SimpleFs>,
    task: WeakKtaskRef,
}

fn make_thread_dir(
    lookup: SimpleDirLookup<'_>,
    name: &str,
    fs: Arc<SimpleFs>,
    task: &KtaskRef,
) -> Dentry {
    let thread_dir = ThreadDir {
        fs: fs.clone(),
        task: Arc::downgrade(task),
    };

    #[cfg(feature = "tee")]
    if tee::has_ta_info(task) {
        let mut ext = DirMapping::new();
        ext.add_child("ta_info", {
            let fs = fs.clone();
            let task = Arc::downgrade(task);
            move |lookup, name| {
                Ok(tee::make_ta_info_dir(
                    lookup,
                    name,
                    fs.clone(),
                    task.clone(),
                ))
            }
        });
        return lookup.dir(
            name,
            SimpleDir::new_maker(fs.clone(), Arc::new(thread_dir.chain(ext))),
        );
    }

    lookup.dir(name, SimpleDir::new_maker(fs, Arc::new(thread_dir)))
}

const PROC_NS_BASE: u64 = 0xf000_0000;

fn namespace_inode(kind: &str) -> u64 {
    match kind {
        "mnt" => PROC_NS_BASE + 1,
        "pid" => PROC_NS_BASE + 2,
        "net" => PROC_NS_BASE + 3,
        "ipc" => PROC_NS_BASE + 4,
        "uts" => PROC_NS_BASE + 5,
        "user" => PROC_NS_BASE + 6,
        "cgroup" => PROC_NS_BASE + 7,
        _ => PROC_NS_BASE,
    }
}

fn namespace_link_target(kind: &str) -> String {
    format!("{kind}:[{}]", namespace_inode(kind))
}

fn task_namespace_link_target(kind: &str, task: &KtaskRef) -> VfsResult<String> {
    if kind == "cgroup" {
        let id = task
            .as_thread()
            .process()
            .cgroup_ns()
            .map_err(|_| VfsError::NotFound)?
            .id()
            .as_u64();
        Ok(format!("{kind}:[{id}]"))
    } else {
        Ok(namespace_link_target(kind))
    }
}

struct ThreadNsDir {
    fs: Arc<SimpleFs>,
    task: WeakKtaskRef,
}

impl SimpleDirOps for ThreadNsDir {
    fn child_names<'a>(&'a self) -> VfsResult<Box<dyn Iterator<Item = Cow<'a, str>> + 'a>> {
        Ok(Box::new(
            ["mnt", "pid", "net", "ipc", "uts", "user", "cgroup"]
                .into_iter()
                .map(Cow::Borrowed),
        ))
    }

    fn lookup_child(&self, lookup: SimpleDirLookup<'_>, name: &str) -> VfsResult<Dentry> {
        if !matches!(
            name,
            "mnt" | "pid" | "net" | "ipc" | "uts" | "user" | "cgroup"
        ) {
            return Err(VfsError::NotFound);
        }
        let task = self.task.upgrade().ok_or(VfsError::NotFound)?;
        let target = task_namespace_link_target(name, &task)?;
        lookup.file(
            name,
            SimpleFile::new(self.fs.clone(), NodeType::Symlink, move || {
                Ok(target.clone())
            }),
        )
    }

    fn supports_dentry_cache(&self) -> bool {
        false
    }
}

impl SimpleDirOps for ThreadDir {
    fn child_names<'a>(&'a self) -> VfsResult<Box<dyn Iterator<Item = Cow<'a, str>> + 'a>> {
        Ok(Box::new(
            [
                "stat",
                "status",
                "oom_score_adj",
                "task",
                "maps",
                "mounts",
                "mountinfo",
                "mountstats",
                "cgroup",
                "ns",
                "cmdline",
                "comm",
                "exe",
                "fd",
            ]
            .into_iter()
            .map(Cow::Borrowed),
        ))
    }

    #[expect(
        clippy::too_many_lines,
        reason = "CG-028-focused-functions: TODO(cg): legacy per-task lookup with long arms; move \
                  each entry kind into its own builder"
    )]
    #[expect(
        clippy::excessive_nesting,
        reason = "CG-029-shallow-control-flow: TODO(cg): legacy per-task lookup; the builder \
                  split for too_many_lines also flattens it"
    )]
    fn lookup_child(&self, lookup: SimpleDirLookup<'_>, name: &str) -> VfsResult<Dentry> {
        let fs = self.fs.clone();
        let task_weak = self.task.clone();
        match name {
            "stat" => lookup.file(
                name,
                SimpleFile::new_regular(fs.clone(), {
                    let task_weak = task_weak.clone();
                    move || {
                        let task = upgrade_task(&task_weak)?;
                        Ok(format!("{}", TaskStat::from_thread(&task)?).into_bytes())
                    }
                }),
            ),
            "status" => lookup.file(
                name,
                SeqFileInode::new_regular(fs.clone(), {
                    let task_weak = task_weak.clone();
                    move || {
                        Ok(StatusIter {
                            task: task_weak.clone(),
                        })
                    }
                }),
            ),
            "oom_score_adj" => lookup.file(
                name,
                SimpleFile::new_regular(
                    fs.clone(),
                    CommandFile::new({
                        let task_weak = task_weak.clone();
                        move |req| {
                            let task = upgrade_task(&task_weak)?;
                            match req {
                                SimpleFileOperation::Read => {
                                    Ok(Some(format_oom_score_adj(task.as_thread().oom_score_adj())))
                                }
                                SimpleFileOperation::Write { data, .. } => {
                                    if let Some(value) = parse_oom_score_adj_input(data)? {
                                        if !(OOM_SCORE_ADJ_MIN..=OOM_SCORE_ADJ_MAX).contains(&value)
                                        {
                                            return Err(VfsError::InvalidInput);
                                        }
                                        task.as_thread().set_oom_score_adj(value);
                                    }
                                    Ok(None)
                                }
                            }
                        }
                    }),
                ),
            ),
            "task" => {
                let task = task_weak.upgrade().ok_or(VfsError::NotFound)?;
                Ok(lookup.dir(
                    name,
                    SimpleDir::new_maker(
                        fs.clone(),
                        Arc::new(ProcessTaskDir {
                            fs: fs.clone(),
                            process: Arc::downgrade(task.as_thread().process()),
                        }),
                    ),
                ))
            }
            "maps" => lookup.file(
                name,
                SeqFileInode::new_regular(fs.clone(), {
                    let task_weak = task_weak.clone();
                    move || {
                        let task = upgrade_task(&task_weak)?;
                        MapsIter::new(&task)
                    }
                }),
            ),
            "mounts" => lookup.file(
                name,
                SeqFileInode::new_regular(fs.clone(), {
                    let task_weak = task_weak.clone();
                    move || {
                        let task = upgrade_task(&task_weak)?;
                        let root = task
                            .as_thread()
                            .process()
                            .fs_context()
                            .map(|fs| fs.lock().root().clone())?;
                        Ok(ProcMountIter::mounts(Some(root)))
                    }
                }),
            ),
            "mountinfo" => lookup.file(
                name,
                SeqFileInode::new_regular(fs.clone(), {
                    let task_weak = task_weak.clone();
                    move || {
                        let task = upgrade_task(&task_weak)?;
                        let root = task
                            .as_thread()
                            .process()
                            .fs_context()
                            .map(|fs| fs.lock().root().clone())?;
                        Ok(ProcMountIter::mountinfo(Some(root)))
                    }
                }),
            ),
            "mountstats" => lookup.file(
                name,
                SeqFileInode::new_regular(fs.clone(), {
                    let task_weak = task_weak.clone();
                    move || {
                        let task = upgrade_task(&task_weak)?;
                        let root = task
                            .as_thread()
                            .process()
                            .fs_context()
                            .map(|fs| fs.lock().root().clone())?;
                        Ok(ProcMountIter::mountstats(Some(root)))
                    }
                }),
            ),
            "cgroup" => lookup.file(
                name,
                SimpleFile::new_regular(fs.clone(), {
                    let task_weak = task_weak.clone();
                    move || {
                        let task = upgrade_task(&task_weak)?;
                        let reader = current();
                        let reader = reader.try_as_thread().ok_or(VfsError::NotFound)?;
                        let view_root = reader
                            .process()
                            .cgroup_ns()
                            .map_err(|_| VfsError::NotFound)?
                            .root();
                        Ok(format!(
                            "0::{}\n",
                            task.as_thread()
                                .cgroup()
                                .ok_or(VfsError::NotFound)?
                                .path_from(&view_root)
                                .map_err(|_| VfsError::NotFound)?
                        ))
                    }
                }),
            ),
            "ns" => {
                let task = task_weak.upgrade().ok_or(VfsError::NotFound)?;
                Ok(lookup.dir(
                    name,
                    SimpleDir::new_maker(
                        fs.clone(),
                        Arc::new(ThreadNsDir {
                            fs: fs.clone(),
                            task: Arc::downgrade(&task),
                        }),
                    ),
                ))
            }
            "cmdline" => lookup.file(
                name,
                SimpleFile::new_regular(fs.clone(), {
                    let task_weak = task_weak.clone();
                    move || {
                        let task = upgrade_task(&task_weak)?;
                        let cmdline = task.as_thread().process().cmdline()?;
                        let mut buf = Vec::new();
                        for arg in cmdline.iter() {
                            buf.extend_from_slice(arg.as_bytes());
                            buf.push(0);
                        }
                        Ok(buf)
                    }
                }),
            ),
            "comm" => lookup.file(
                name,
                SimpleFile::new_regular(
                    fs.clone(),
                    CommandFile::new({
                        let task_weak = task_weak.clone();
                        move |req| {
                            let task = upgrade_task(&task_weak)?;
                            match req {
                                SimpleFileOperation::Read => {
                                    let mut bytes = vec![0; 16];
                                    let name = task.name();
                                    let copy_len = name.len().min(15);
                                    bytes[..copy_len].copy_from_slice(&name.as_bytes()[..copy_len]);
                                    bytes[copy_len] = b'\n';
                                    Ok(Some(bytes))
                                }
                                SimpleFileOperation::Write { data, .. } => {
                                    if !data.is_empty() {
                                        let mut input = [0; 16];
                                        let copy_len = data.len().min(15);
                                        input[..copy_len].copy_from_slice(&data[..copy_len]);
                                        task.set_name(
                                            CStr::from_bytes_until_nul(&input)
                                                .map_err(|_| VfsError::InvalidInput)?
                                                .to_str()
                                                .map_err(|_| VfsError::InvalidInput)?,
                                        );
                                    }
                                    Ok(None)
                                }
                            }
                        }
                    }),
                ),
            ),
            "exe" => lookup.file(
                name,
                SimpleFile::new(fs.clone(), NodeType::Symlink, {
                    let task_weak = task_weak.clone();
                    move || {
                        // Linux proc_exe_link() returns ENOENT when the task
                        // is gone: readlink is path resolution, unlike reads
                        // on an already-open status file.
                        let task = task_weak.upgrade().ok_or(VfsError::NotFound)?;
                        task.as_thread().process().exe_path()
                    }
                }),
            ),
            "fd" => {
                let task = task_weak.upgrade().ok_or(VfsError::NotFound)?;
                Ok(lookup.dir(
                    name,
                    SimpleDir::new_maker(
                        fs.clone(),
                        Arc::new(ThreadFdDir {
                            fs: fs.clone(),
                            task: Arc::downgrade(&task),
                        }),
                    ),
                ))
            }
            _ => Err(VfsError::NotFound),
        }
    }

    fn supports_dentry_cache(&self) -> bool {
        false
    }
}

pub(crate) struct ProcFsHandler {
    fs: Arc<SimpleFs>,
}

impl ProcFsHandler {
    pub(crate) fn new(fs: Arc<SimpleFs>) -> Self {
        Self { fs }
    }
}

impl SimpleDirOps for ProcFsHandler {
    fn child_names<'a>(&'a self) -> VfsResult<Box<dyn Iterator<Item = Cow<'a, str>> + 'a>> {
        Ok(Box::new(
            procfs::visible_processes()
                .into_iter()
                .map(|process| Cow::Owned(process.pid().to_string()))
                .chain([Cow::Borrowed("self")]),
        ))
    }

    fn lookup_child(&self, lookup: SimpleDirLookup<'_>, name: &str) -> VfsResult<Dentry> {
        if name == "self" {
            return lookup.file(
                name,
                SimpleFile::new(self.fs.clone(), NodeType::Symlink, proc_self_target),
            );
        }

        let pid = name.parse::<u32>().map_err(|_| VfsError::NotFound)?;
        let task = procfs::process_task(pid).map_err(|_| VfsError::NotFound)?;
        Ok(make_thread_dir(lookup, name, self.fs.clone(), &task))
    }

    fn supports_dentry_cache(&self) -> bool {
        false
    }
}

#[unittest::mod_test]
mod tests {
    use khal::paging::MappingFlags;
    use kprocess::AsThread;
    use kvfs::SeqIterator;
    use unittest::{assert, assert_eq, def_test};

    use super::{
        MapsEntry, PROC_MAPS_ADDR_WIDTH, PROC_MAPS_PATH_COLUMN, format_oom_score_adj,
        format_task_status, maps_permissions, namespace_inode, namespace_link_target,
        parse_oom_score_adj_input, render_maps_line, special_mapping_name,
    };

    fn task_directory_path(process: alloc::sync::Weak<kprocess::Process>) -> kvfs::Path {
        static FS_TYPE: kvfs::FileSystemType = kvfs::FileSystemType::internal("proc-task-test");
        let super_block = kvfs::SimpleFs::new_with(&FS_TYPE, 0, move |fs| {
            let ops = alloc::sync::Arc::new(super::ProcessTaskDir {
                fs: fs.clone(),
                process,
            });
            kvfs::SimpleDir::new_maker(fs, ops)
        });
        kvfs::Mount::new_root(&super_block).root_path()
    }

    #[def_test(user)]
    fn test_task_directory_link_count_uses_its_live_target() {
        // This user-test fixture owns its process and starts no extra threads.
        let task = ktask::current();
        let process = task.as_thread().process();
        let path = task_directory_path(alloc::sync::Arc::downgrade(process));
        // start_user_task publishes this exact fixture thread before running it.
        assert!(process.threads().contains(&task.as_thread().tid()));
        let count = kprocess::procfs::thread_group_member_count(process);
        assert_eq!(path.getattr().unwrap().nlink, 2 + count as u64);
        assert_eq!(path.metadata().nlink, 2);
    }

    #[def_test(user)]
    fn test_task_directory_link_count_after_target_is_dropped() {
        // The user fixture has already initialized global init. This separate
        // parentless identity publishes no task or PID slot and has one owner.
        let process = kprocess::Process::new_init(0x7fff_ff01);
        let weak = alloc::sync::Arc::downgrade(&process);
        let path = task_directory_path(weak.clone());
        assert_eq!(alloc::sync::Arc::strong_count(&process), 1);
        drop(process);
        assert!(weak.upgrade().is_none());
        assert_eq!(path.getattr().unwrap().nlink, 2);
        assert_eq!(path.metadata().nlink, 2);
    }

    #[def_test(user)]
    fn test_maps_rewind_observes_captured_image_heap_updates() {
        let task = ktask::current();
        let process = task.as_thread().process();
        let old_heap_top = process.heap_top().unwrap();
        let mut maps = super::MapsIter::new(&task).unwrap();
        let start = memaddr::VirtAddr::from_usize(super::USER_HEAP_BASE);
        let end = super::USER_HEAP_BASE + memaddr::PAGE_SIZE_4K;
        maps.aspace
            .lock()
            .map(
                start,
                memaddr::PAGE_SIZE_4K,
                MappingFlags::READ | MappingFlags::WRITE,
                false,
                memspace::VmRuntimeRef::new_anon_private(start, khal::paging::PageSize::Size4K),
            )
            .unwrap();
        process.set_heap_top(end).unwrap();
        maps.rewind();
        let entry = maps
            .entries
            .iter()
            .find(|entry| entry.start == start.as_usize())
            .unwrap();
        assert_eq!(entry.name, Some("[heap]"));

        process.set_heap_top(super::USER_HEAP_BASE).unwrap();
        maps.rewind();
        let entry = maps
            .entries
            .iter()
            .find(|entry| entry.start == start.as_usize())
            .unwrap();
        assert_eq!(entry.name, None);
        maps.aspace
            .lock()
            .unmap(start, memaddr::PAGE_SIZE_4K)
            .unwrap();
        process.set_heap_top(old_heap_top).unwrap();
    }

    #[def_test]
    fn test_namespace_inode_has_expected_known_values() {
        assert_eq!(namespace_inode("mnt"), 0xf000_0001);
        assert_eq!(namespace_inode("pid"), 0xf000_0002);
        assert_eq!(namespace_inode("net"), 0xf000_0003);
    }

    #[def_test]
    fn test_namespace_inode_unknown_uses_base_value() {
        assert_eq!(namespace_inode("unknown"), 0xf000_0000);
    }

    #[def_test]
    fn test_namespace_link_target_formats_inode_reference() {
        assert_eq!(namespace_link_target("mnt"), "mnt:[4026531841]");
    }

    #[def_test]
    fn test_task_status_includes_process_name() {
        let status = format_task_status("test-bin", 12, 34, &kcred::Cred::root());

        assert!(status.contains("Name:\ttest-bin\n"));
        assert!(status.contains("Tgid:\t12\n"));
        assert!(status.contains("Pid:\t34\n"));
    }

    // These fixtures publish targets but never activate them. Reader credentials
    // are restored before a target is inspected, including every error return.
    struct ScopedStatusReaderCred {
        reader: kprocess::CurrentThread,
        original: alloc::sync::Arc<kcred::Cred>,
    }

    impl ScopedStatusReaderCred {
        fn install(next: kcred::Cred) -> Self {
            let reader = kprocess::current_user_thread();
            let original = reader.real_cred();
            core::assert!(alloc::sync::Arc::ptr_eq(&original, &reader.cred()));
            let guard = Self { reader, original };
            guard.reader.commit_creds(next);
            guard
        }
    }

    impl Drop for ScopedStatusReaderCred {
        fn drop(&mut self) {
            self.reader.commit_creds(self.original.prepare());
        }
    }

    fn status_credential_target(
        cred: kcred::Cred,
        is_sibling: bool,
    ) -> kvfs::VfsResult<kprocess::PublishedUserTask> {
        let prepared = {
            let _restore = ScopedStatusReaderCred::install(cred);
            let reader = kprocess::current_user_thread();
            if is_sibling {
                reader.prepare_thread_clone()?
            } else {
                reader.prepare_process_fork(kprocess::ProcessForkConfig {
                    parent: kprocess::ForkParent::Caller,
                    address_space: kprocess::ForkAddressSpace::Shared,
                    fs: kprocess::ForkFs::Shared,
                    signal_actions: kprocess::ForkSignalActions::Shared,
                    fd_table: kprocess::ForkFdTable::Shared,
                    namespace_flags: Default::default(),
                    exit_signal: None,
                })?
            }
        };
        let (thread, identity) = prepared.into_parts();
        kprocess::publish_user_task(ktask::TaskInner::new_user(
            || {},
            alloc::string::String::from("procfs-status-credential-target"),
            identity,
            thread,
        ))
    }

    fn status_id_columns(status: &str, key: &str) -> Option<[u32; 4]> {
        let mut rows = status.lines().filter_map(|line| line.strip_prefix(key));
        let row = rows.next()?;
        if rows.next().is_some() {
            return None;
        }
        let mut columns = row.split_whitespace();
        let ids = [
            columns.next()?.parse().ok()?,
            columns.next()?.parse().ok()?,
            columns.next()?.parse().ok()?,
            columns.next()?.parse().ok()?,
        ];
        if columns.next().is_some() {
            return None;
        }
        Some(ids)
    }

    fn status_distinct_ids() -> kcred::Cred {
        let mut cred = kcred::Cred::root();
        cred.set_resgid(Some(201), Some(202), Some(203)).unwrap();
        cred.set_fsgid(204);
        // Keeping effective UID zero permits a fourth, distinct filesystem UID.
        cred.set_resuid(Some(101), Some(0), Some(103)).unwrap();
        cred.set_fsuid(104);
        cred
    }

    static STATUS_TEST_FS_TYPE: kvfs::FileSystemType =
        kvfs::FileSystemType::internal("procfs-status-credential-test");

    fn status_test_root() -> kvfs::Path {
        let fs = kvfs::SimpleFs::new_with(&STATUS_TEST_FS_TYPE, 0x9fa0, |fs| {
            kvfs::SimpleDir::new_maker(
                fs.clone(),
                alloc::sync::Arc::new(super::ProcFsHandler::new(fs)),
            )
        });
        kvfs::Mount::new_root(&fs).root_path()
    }

    fn open_status(
        root: &kvfs::Path,
        name: alloc::string::String,
    ) -> kvfs::VfsResult<alloc::sync::Arc<kvfs::VfsFile>> {
        kvfs::Filename::new(name).open_with_flags_at(
            root,
            root,
            0,
            kvfs::NodePermission::empty(),
            kvfs::NodePermission::empty(),
            kcred::initial_cred(),
        )
    }

    fn read_status(file: &kvfs::VfsFile) -> kvfs::VfsResult<alloc::string::String> {
        let mut bytes = alloc::vec::Vec::new();
        let mut chunk = [0u8; 31];
        for _ in 0..128 {
            let count = file.read(&mut chunk)?;
            if count == 0 {
                return alloc::string::String::from_utf8(bytes)
                    .map_err(|_| kvfs::VfsError::InvalidData);
            }
            bytes.extend_from_slice(&chunk[..count]);
        }
        Err(kvfs::VfsError::BadState)
    }

    #[def_test(user, serial)]
    fn test_status_root_credential_columns_remain_zero() {
        let _reader = ScopedStatusReaderCred::install(kcred::Cred::new(71, 81));
        let target = status_credential_target(kcred::Cred::root(), false).unwrap();
        let status = super::task_status(target.task());
        assert_eq!(status_id_columns(&status, "Uid:"), Some([0; 4]));
        assert_eq!(status_id_columns(&status, "Gid:"), Some([0; 4]));
    }

    #[def_test(user, serial)]
    fn test_status_nonzero_target_credentials_are_not_reader_credentials() {
        let _reader = ScopedStatusReaderCred::install(kcred::Cred::new(71, 81));
        let target = status_credential_target(kcred::Cred::new(1234, 4321), false).unwrap();
        let reader = ktask::current();
        assert!(!alloc::sync::Arc::ptr_eq(target.task(), &reader));
        assert!(!alloc::sync::Arc::ptr_eq(
            target.task().as_thread().process(),
            reader.as_thread().process()
        ));
        assert_eq!(kprocess::current_real_cred().ruid(), 71);
        assert_eq!(kprocess::current_real_cred().rgid(), 81);
        let status = super::task_status(target.task());
        assert_eq!(status_id_columns(&status, "Uid:"), Some([1234; 4]));
        assert_eq!(status_id_columns(&status, "Gid:"), Some([4321; 4]));
    }

    #[def_test(user, serial)]
    fn test_status_orders_real_effective_saved_filesystem_ids() {
        let _reader = ScopedStatusReaderCred::install(kcred::Cred::new(71, 81));
        let target = status_credential_target(status_distinct_ids(), false).unwrap();
        let cred = target.task().as_thread().real_cred();
        assert_eq!(
            [cred.ruid(), cred.euid(), cred.suid(), cred.fsuid()],
            [101, 0, 103, 104]
        );
        assert_eq!(
            [cred.rgid(), cred.egid(), cred.sgid(), cred.fsgid()],
            [201, 202, 203, 204]
        );
        let status = super::task_status(target.task());
        assert_eq!(status_id_columns(&status, "Uid:"), Some([101, 0, 103, 104]));
        assert_eq!(
            status_id_columns(&status, "Gid:"),
            Some([201, 202, 203, 204])
        );
    }

    #[def_test(user, serial)]
    fn test_status_same_process_threads_keep_their_own_credentials() {
        let _reader = ScopedStatusReaderCred::install(kcred::Cred::new(71, 81));
        let first = status_credential_target(kcred::Cred::new(1234, 4321), true).unwrap();
        let second = status_credential_target(kcred::Cred::new(2345, 5432), true).unwrap();
        let reader = ktask::current();
        let owner = reader.as_thread().process();
        assert!(alloc::sync::Arc::ptr_eq(
            first.task().as_thread().process(),
            owner
        ));
        assert!(alloc::sync::Arc::ptr_eq(
            second.task().as_thread().process(),
            owner
        ));
        assert!(first.task().as_thread().tid() != second.task().as_thread().tid());
        let first_status = super::task_status(first.task());
        let second_status = super::task_status(second.task());
        assert_eq!(status_id_columns(&first_status, "Uid:"), Some([1234; 4]));
        assert_eq!(status_id_columns(&first_status, "Gid:"), Some([4321; 4]));
        assert_eq!(status_id_columns(&second_status, "Uid:"), Some([2345; 4]));
        assert_eq!(status_id_columns(&second_status, "Gid:"), Some([5432; 4]));
    }

    #[def_test(user, serial)]
    fn test_status_reader_credential_change_does_not_retarget_snapshot() {
        let _reader = ScopedStatusReaderCred::install(kcred::Cred::new(71, 81));
        let target = status_credential_target(kcred::Cred::new(1234, 4321), false).unwrap();
        let before = super::task_status(target.task());
        {
            let _changed = ScopedStatusReaderCred::install(kcred::Cred::new(98, 89));
            assert_eq!(kprocess::current_real_cred().ruid(), 98);
            let changed = super::task_status(target.task());
            assert_eq!(status_id_columns(&changed, "Uid:"), Some([1234; 4]));
            assert_eq!(status_id_columns(&changed, "Gid:"), Some([4321; 4]));
        }
        assert_eq!(kprocess::current_real_cred().ruid(), 71);
        assert_eq!(kprocess::current_real_cred().rgid(), 81);
        let after = super::task_status(target.task());
        assert_eq!(
            status_id_columns(&before, "Uid:"),
            status_id_columns(&after, "Uid:")
        );
        assert_eq!(
            status_id_columns(&before, "Gid:"),
            status_id_columns(&after, "Gid:")
        );
    }

    #[def_test(user, serial)]
    fn test_status_vfs_process_and_thread_paths_render_selected_credentials() {
        let _reader = ScopedStatusReaderCred::install(kcred::Cred::new(71, 81));
        let target = status_credential_target(status_distinct_ids(), false).unwrap();
        let root = status_test_root();
        let thread = target.task().as_thread();
        for name in [
            alloc::format!("/{}/status", thread.process().pid()),
            alloc::format!("/{}/task/{}/status", thread.process().pid(), thread.tid()),
        ] {
            let file = open_status(&root, name).unwrap();
            let text = read_status(&file).unwrap();
            assert_eq!(status_id_columns(&text, "Uid:"), Some([101, 0, 103, 104]));
            assert_eq!(status_id_columns(&text, "Gid:"), Some([201, 202, 203, 204]));
            assert!(text.contains(&alloc::format!("Pid:\t{}\n", thread.tid())));
        }
        assert_eq!(kprocess::current_real_cred().ruid(), 71);
    }

    #[def_test(user, serial)]
    fn test_status_partial_reads_keep_one_record_and_rewind_refreshes_credentials() {
        let _reader = ScopedStatusReaderCred::install(kcred::Cred::new(7, 8));
        let target = ktask::current();
        let root = status_test_root();
        let pid = target.as_thread().process().pid();
        let name = alloc::format!("/{pid}/status");
        let first = open_status(&root, name.clone()).unwrap();
        let not_yet_read = open_status(&root, name).unwrap();
        let original = super::task_status(&target);
        let split = original.find("Gid:").unwrap();
        let mut prefix = alloc::vec![0u8; split];
        assert_eq!(first.read(&mut prefix).unwrap(), split);

        // Change both contents and width after the first record has started.
        let _changed = ScopedStatusReaderCred::install(kcred::Cred::new(1234567890, 987654321));
        let mut observed = alloc::string::String::from_utf8(prefix).unwrap();
        observed.push_str(&read_status(&first).unwrap());
        assert_eq!(status_id_columns(&observed, "Uid:"), Some([7; 4]));
        assert_eq!(status_id_columns(&observed, "Gid:"), Some([8; 4]));

        // Each open has its own read state; contents are not frozen at open.
        let newer = read_status(&not_yet_read).unwrap();
        assert_eq!(status_id_columns(&newer, "Uid:"), Some([1234567890; 4]));
        assert_eq!(status_id_columns(&newer, "Gid:"), Some([987654321; 4]));

        assert_eq!(first.llseek(0, 0).unwrap(), 0);
        let rewound = read_status(&first).unwrap();
        assert_eq!(status_id_columns(&rewound, "Uid:"), Some([1234567890; 4]));
        assert_eq!(status_id_columns(&rewound, "Gid:"), Some([987654321; 4]));
    }

    #[def_test(user, serial)]
    fn test_status_collected_target_keeps_esrch_without_reader_fallback() {
        let _reader = ScopedStatusReaderCred::install(kcred::Cred::new(71, 81));
        let target = status_credential_target(kcred::Cred::new(1234, 4321), false).unwrap();
        let weak = alloc::sync::Arc::downgrade(target.task());
        let root = status_test_root();
        let pid = target.task().as_thread().process().pid();
        let file = open_status(&root, alloc::format!("/{pid}/status")).unwrap();
        target.abort();
        assert!(matches!(
            super::upgrade_task(&weak),
            Err(kvfs::VfsError::NoSuchProcess)
        ));
        assert_eq!(
            file.read(&mut [0u8; 31]),
            Err(kvfs::VfsError::NoSuchProcess)
        );
        assert_eq!(kprocess::current_real_cred().ruid(), 71);
    }

    #[def_test(user, serial)]
    fn test_status_retained_target_preserves_credentials_after_abort() {
        let _reader = ScopedStatusReaderCred::install(kcred::Cred::new(71, 81));
        let target = status_credential_target(kcred::Cred::new(1234, 4321), false).unwrap();
        let retained = target.task().clone();
        let weak = alloc::sync::Arc::downgrade(&retained);
        target.abort();
        let status = super::task_status(&retained);
        assert_eq!(status_id_columns(&status, "Uid:"), Some([1234; 4]));
        assert_eq!(status_id_columns(&status, "Gid:"), Some([4321; 4]));
        drop(retained);
        assert!(matches!(
            super::upgrade_task(&weak),
            Err(kvfs::VfsError::NoSuchProcess)
        ));
    }

    #[def_test]
    fn test_format_oom_score_adj_appends_newline() {
        assert_eq!(format_oom_score_adj(200), b"200\n");
    }

    #[def_test]
    fn test_parse_oom_score_adj_accepts_newline_terminated_input() {
        assert_eq!(parse_oom_score_adj_input(b"100\n").unwrap(), Some(100));
    }

    #[def_test]
    fn test_parse_oom_score_adj_accepts_empty_input_as_noop() {
        assert_eq!(parse_oom_score_adj_input(b"").unwrap(), None);
    }

    #[def_test]
    fn test_parse_oom_score_adj_rejects_invalid_input() {
        assert!(parse_oom_score_adj_input(b"abc\n").is_err());
    }

    #[def_test]
    fn test_maps_permissions_formats_shared_exec() {
        assert_eq!(
            maps_permissions(MappingFlags::READ | MappingFlags::EXECUTE | MappingFlags::SHARED),
            *b"r-xs"
        );
    }

    #[def_test]
    fn test_special_mapping_name_recognizes_heap_and_stack() {
        assert_eq!(
            special_mapping_name(
                super::USER_HEAP_BASE,
                super::USER_HEAP_BASE + 0x1000,
                super::USER_HEAP_BASE + 0x2000
            ),
            Some("[heap]")
        );
        assert_eq!(
            special_mapping_name(
                super::USER_STACK_TOP - 0x1000,
                super::USER_STACK_TOP,
                super::USER_HEAP_BASE
            ),
            Some("[stack]")
        );
    }

    #[def_test]
    fn test_render_maps_line_aligns_path_column() {
        let line = render_maps_line(&MapsEntry {
            start: 0x4000_0000,
            end: 0x4000_1000,
            flags: MappingFlags::READ | MappingFlags::WRITE,
            offset: 0,
            inode: 42,
            path: Some("/bin/test".into()),
            name: None,
        });

        assert_eq!(&line[..PROC_MAPS_ADDR_WIDTH], "000040000000");
        assert_eq!(line.chars().nth(PROC_MAPS_PATH_COLUMN), Some('/'));
    }
}
