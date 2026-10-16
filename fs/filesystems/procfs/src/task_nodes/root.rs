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
            .files()
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
            .files()
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

/// Magic link backing `/proc/<pid>/exe`.
///
/// `readlink` reports the process executable display path; `open`/lookup
/// follows to the opened backing executable file (set on exec), so runtimes
/// can `open("/proc/self/exe")` and `fcntl(F_GET_SEALS)` it (runc/crun
/// self-clone re-exec for CVE-2019-5736).
struct ProcExeLink {
    node: SimpleFsNode,
    task: WeakKtaskRef,
}

impl ProcExeLink {
    fn new(fs: Arc<SimpleFs>, task: WeakKtaskRef) -> Arc<Self> {
        Arc::new(Self {
            node: SimpleFsNode::new(
                fs,
                NodeType::Symlink,
                NodePermission::from_bits_truncate(0o777),
            ),
            task,
        })
    }

    /// Requires the same real-cred read access as a ptrace read.
    ///
    /// Only the outlets that expose the link's target content call this, matching
    /// Linux's `proc_pid_get_link`; `getattr`/lstat does not. A kernel task has
    /// no user credentials to compare and is treated as privileged.
    fn check_access(&self) -> VfsResult<()> {
        let task = self.task.upgrade().ok_or(VfsError::NotFound)?;
        let target = task.as_thread();
        // A zombie keeps its task object until reaped, but Linux reports
        // `ENOENT` for its `/proc/<pid>/exe`: the image it ran is gone.
        if target.is_exiting() || target.process().is_exited() {
            return Err(VfsError::NotFound);
        }
        if let Some(caller) = ktask::current().try_as_thread() {
            kprocess::ptrace::check_read_real_creds_access(caller, target)
                .map_err(|_| VfsError::OperationNotPermitted)?;
        }
        Ok(())
    }

    fn display_target(&self) -> VfsResult<String> {
        let task = self.task.upgrade().ok_or(VfsError::NotFound)?;
        let process = task.as_thread().process();
        // Render from the opened backing file so `readlink` and `follow` name
        // the same object even when the exec display path is synthetic (e.g.
        // `execveat(AT_EMPTY_PATH)` on a memfd).
        let Some(file) = process.exe_file().ok().flatten() else {
            return process.exe_path();
        };
        file.path()
            .absolute_path()
            .map(|path| path.to_string())
            .or_else(|_| process.exe_path())
    }

    /// Returns the symlink's reported size.
    ///
    /// Linux's proc pid symlink inode keeps `i_size == 0`; the target string is
    /// produced only by the access-checked read paths, so `stat` neither fails
    /// nor reveals the target length.
    fn target_len(&self) -> VfsResult<u64> {
        Ok(0)
    }
}

impl InodeOperations for ProcExeLink {
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

impl InodeSymlinkOperations for ProcExeLink {
    fn get_link(
        &self,
        _dentry: Option<&Dentry>,
        _inode: &kvfs::VfsInode,
        _done: &mut kvfs::DelayedCall,
    ) -> VfsResult<String> {
        self.check_access()?;
        self.display_target()
    }
}

impl FileOperations for ProcExeLink {
    fn supports_read(&self) -> bool {
        true
    }

    fn read(&self, _file: &VfsFile, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        self.check_access()?;
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

impl MagicLinkOps for ProcExeLink {
    fn readlink_display(&self) -> VfsResult<String> {
        self.check_access()?;
        self.display_target()
    }

    fn follow(&self, _intent: LookupIntent, flags: LookupFlags) -> VfsResult<ResolvedObject> {
        if flags.rejects_magic_links() || !flags.follows_final() {
            return Err(VfsError::FilesystemLoop);
        }
        self.check_access()?;
        let task = self.task.upgrade().ok_or(VfsError::NotFound)?;
        let target = task.as_thread();
        let exe_file = target
            .process()
            .exe_file()
            .ok()
            .flatten()
            .ok_or(VfsError::NotFound)?;
        Ok(ResolvedObject::location(exe_file.path().clone()))
    }
}

fn proc_exe_link_child(
    lookup: SimpleDirLookup<'_>,
    name: &str,
    link: Arc<ProcExeLink>,
) -> VfsResult<Dentry> {
    let init = link.node.inode_init().with_size(link.target_len()?);
    let inode = VfsInode::new_file_with_flags(link, NodeFlags::NON_CACHEABLE, init);
    Ok(lookup.file_from_inode(name, inode))
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
        let resources = task.as_thread().files();
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
            .files()
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
            "exe" => proc_exe_link_child(lookup, name, ProcExeLink::new(fs.clone(), task_weak)),
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

unittest::path_mod_test!(tests, "../../ut-tests/task_nodes_root.rs");
