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
use core::{ffi::CStr, iter, str};

use kaddr_layout::{SIGNAL_TRAMPOLINE, USER_HEAP_BASE, USER_STACK_SIZE, USER_STACK_TOP};
use khal::paging::MappingFlags;
use kprocess::{AsThread, Process, TaskStat, procfs};
use ktask::{KtaskRef, WeakKtaskRef, current};
#[cfg(feature = "tee")]
use kvfs::DirMapping;
use kvfs::{
    Dentry, FileOperations, InodeOperations, InodeSymlinkOperations, LookupFlags, LookupIntent,
    MagicLinkOps, Metadata, MetadataUpdate, NodeFlags, NodePermission, NodeType, ResolvedObject,
    RwFile, SeqFileInode, SeqIterator, SimpleDir, SimpleDirLookup, SimpleDirOps, SimpleFile,
    SimpleFileOperation, SimpleFs, SimpleFsNode, VfsError, VfsFile, VfsInode, VfsResult,
};

#[cfg(feature = "tee")]
use super::tee::{has_ta_info, make_ta_info_dir};
use crate::task_nodes::mounts::ProcMountIter;

struct ProcessTaskDir {
    fs: Arc<SimpleFs>,
    process: Weak<Process>,
}

impl SimpleDirOps for ProcessTaskDir {
    fn child_names<'a>(&'a self) -> Box<dyn Iterator<Item = Cow<'a, str>> + 'a> {
        let Some(process) = self.process.upgrade() else {
            return Box::new(iter::empty());
        };
        Box::new(
            process
                .threads()
                .into_iter()
                .map(|tid| tid.to_string().into()),
        )
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

fn task_status(task: &KtaskRef) -> String {
    let thread = task.as_thread();
    format_task_status(&task.name(), thread.process().pid(), thread.tid() as u64)
}

#[rustfmt::skip]
fn format_task_status(name: &str, tgid: u32, pid: u64) -> String {
    format!(
        "Name:\t{}\n\
        Tgid:\t{}\n\
        Pid:\t{}\n\
        Uid:\t0 0 0 0\n\
        Gid:\t0 0 0 0\n\
        Cpus_allowed:\t1\n\
        Cpus_allowed_list:\t0\n\
        Mems_allowed:\t1\n\
        Mems_allowed_list:\t0\n",
        name,
        tgid,
        pid
    )
}

fn format_oom_score_adj(value: i32) -> Vec<u8> {
    format!("{value}\n").into_bytes()
}

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
    task: WeakKtaskRef,
    entries: Vec<MapsEntry>,
    next_index: usize,
}

impl MapsIter {
    fn new(task: WeakKtaskRef) -> Self {
        let mut iter = Self {
            task,
            entries: Vec::new(),
            next_index: 0,
        };
        iter.rewind();
        iter
    }
}

impl SeqIterator for MapsIter {
    type Item = MapsEntry;

    fn rewind(&mut self) {
        self.entries.clear();
        self.next_index = 0;

        let Some(task) = self.task.upgrade() else {
            return;
        };

        let process = task.as_thread().process();
        let heap_top = process.heap_top().unwrap_or(0);
        let Ok(aspace_ref) = process.address_space() else {
            return;
        };
        let aspace = aspace_ref.lock();
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
                name: special_mapping_name(start, end, heap_top),
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
    ) -> VfsResult<()> {
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
    fn child_names<'a>(&'a self) -> Box<dyn Iterator<Item = Cow<'a, str>> + 'a> {
        let Some(task) = self.task.upgrade() else {
            return Box::new(iter::empty());
        };
        let Ok(resources) = task.as_thread().process().resources() else {
            return Box::new(iter::empty());
        };
        let ids = resources
            .fd_table()
            .read()
            .ids()
            .map(|id| Cow::Owned(id.to_string()))
            .collect::<Vec<_>>();
        Box::new(ids.into_iter())
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
    if has_ta_info(task) {
        let mut ext = DirMapping::new();
        ext.add_child("ta_info", {
            let fs = fs.clone();
            let task = Arc::downgrade(task);
            move |lookup, name| Ok(make_ta_info_dir(lookup, name, fs.clone(), task.clone()))
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

struct ThreadNsDir {
    fs: Arc<SimpleFs>,
}

impl SimpleDirOps for ThreadNsDir {
    fn child_names<'a>(&'a self) -> Box<dyn Iterator<Item = Cow<'a, str>> + 'a> {
        Box::new(
            ["mnt", "pid", "net", "ipc", "uts", "user", "cgroup"]
                .into_iter()
                .map(Cow::Borrowed),
        )
    }

    fn lookup_child(&self, lookup: SimpleDirLookup<'_>, name: &str) -> VfsResult<Dentry> {
        if !matches!(
            name,
            "mnt" | "pid" | "net" | "ipc" | "uts" | "user" | "cgroup"
        ) {
            return Err(VfsError::NotFound);
        }
        let target = namespace_link_target(name);
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
    fn child_names<'a>(&'a self) -> Box<dyn Iterator<Item = Cow<'a, str>> + 'a> {
        Box::new(
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
        )
    }

    fn lookup_child(&self, lookup: SimpleDirLookup<'_>, name: &str) -> VfsResult<Dentry> {
        let fs = self.fs.clone();
        let task = self.task.upgrade().ok_or(VfsError::NotFound)?;
        match name {
            "stat" => lookup.file(
                name,
                SimpleFile::new_regular(fs.clone(), move || {
                    Ok(format!("{}", TaskStat::from_thread(&task)?).into_bytes())
                }),
            ),
            "status" => lookup.file(
                name,
                SimpleFile::new_regular(fs.clone(), move || Ok(task_status(&task))),
            ),
            "oom_score_adj" => lookup.file(
                name,
                SimpleFile::new_regular(
                    fs.clone(),
                    RwFile::new(move |req| match req {
                        SimpleFileOperation::Read => {
                            Ok(Some(format_oom_score_adj(task.as_thread().oom_score_adj())))
                        }
                        SimpleFileOperation::Write(data) => {
                            if let Some(value) = parse_oom_score_adj_input(data)? {
                                task.as_thread().set_oom_score_adj(value);
                            }
                            Ok(None)
                        }
                    }),
                ),
            ),
            "task" => Ok(lookup.dir(
                name,
                SimpleDir::new_maker(
                    fs.clone(),
                    Arc::new(ProcessTaskDir {
                        fs: fs.clone(),
                        process: Arc::downgrade(task.as_thread().process()),
                    }),
                ),
            )),
            "maps" => lookup.file(
                name,
                SeqFileInode::new_regular(fs.clone(), move || MapsIter::new(Arc::downgrade(&task))),
            ),
            "mounts" => lookup.file(
                name,
                SeqFileInode::new_regular(fs.clone(), move || {
                    let root = task
                        .as_thread()
                        .process()
                        .fs_context()
                        .ok()
                        .map(|fs| fs.lock().root().clone());
                    ProcMountIter::mounts(root)
                }),
            ),
            "mountinfo" => lookup.file(
                name,
                SeqFileInode::new_regular(fs.clone(), move || {
                    let root = task
                        .as_thread()
                        .process()
                        .fs_context()
                        .ok()
                        .map(|fs| fs.lock().root().clone());
                    ProcMountIter::mountinfo(root)
                }),
            ),
            "mountstats" => lookup.file(
                name,
                SeqFileInode::new_regular(fs.clone(), move || {
                    let root = task
                        .as_thread()
                        .process()
                        .fs_context()
                        .ok()
                        .map(|fs| fs.lock().root().clone());
                    ProcMountIter::mountstats(root)
                }),
            ),
            "cgroup" => lookup.file(
                name,
                SimpleFile::new_regular(fs.clone(), move || Ok("0::/\n")),
            ),
            "ns" => Ok(lookup.dir(
                name,
                SimpleDir::new_maker(fs.clone(), Arc::new(ThreadNsDir { fs: fs.clone() })),
            )),
            "cmdline" => lookup.file(
                name,
                SimpleFile::new_regular(fs.clone(), move || {
                    let cmdline = task.as_thread().process().cmdline()?;
                    let mut buf = Vec::new();
                    for arg in cmdline.iter() {
                        buf.extend_from_slice(arg.as_bytes());
                        buf.push(0);
                    }
                    Ok(buf)
                }),
            ),
            "comm" => lookup.file(
                name,
                SimpleFile::new_regular(
                    fs.clone(),
                    RwFile::new(move |req| match req {
                        SimpleFileOperation::Read => {
                            let mut bytes = vec![0; 16];
                            let name = task.name();
                            let copy_len = name.len().min(15);
                            bytes[..copy_len].copy_from_slice(&name.as_bytes()[..copy_len]);
                            bytes[copy_len] = b'\n';
                            Ok(Some(bytes))
                        }
                        SimpleFileOperation::Write(data) => {
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
                    }),
                ),
            ),
            "exe" => lookup.file(
                name,
                SimpleFile::new(fs.clone(), NodeType::Symlink, move || {
                    task.as_thread().process().exe_path()
                }),
            ),
            "fd" => Ok(lookup.dir(
                name,
                SimpleDir::new_maker(
                    fs.clone(),
                    Arc::new(ThreadFdDir {
                        fs: fs.clone(),
                        task: Arc::downgrade(&task),
                    }),
                ),
            )),
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
    fn child_names<'a>(&'a self) -> Box<dyn Iterator<Item = Cow<'a, str>> + 'a> {
        Box::new(
            procfs::visible_processes()
                .into_iter()
                .map(|process| Cow::Owned(process.pid().to_string()))
                .chain([Cow::Borrowed("self")]),
        )
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

#[cfg(unittest)]
mod tests {
    use khal::paging::MappingFlags;
    use unittest::{assert_eq, def_test};

    use super::{
        MapsEntry, PROC_MAPS_ADDR_WIDTH, PROC_MAPS_PATH_COLUMN, format_oom_score_adj,
        format_task_status, maps_permissions, namespace_inode, namespace_link_target,
        parse_oom_score_adj_input, render_maps_line, special_mapping_name,
    };

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
        let status = format_task_status("test-bin", 12, 34);

        assert!(status.contains("Name:\ttest-bin\n"));
        assert!(status.contains("Tgid:\t12\n"));
        assert!(status.contains("Pid:\t34\n"));
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
