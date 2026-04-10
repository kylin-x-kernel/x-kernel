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

use fs_ng_vfs::{NodeType, VfsError, VfsResult};
use kcore::{
    config::{SIGNAL_TRAMPOLINE, USER_HEAP_BASE, USER_STACK_SIZE, USER_STACK_TOP},
    task::{AsThread, TaskStat, get_process_data, get_task, processes},
    vfs::{
        NodeOpsMux, RwFile, SeqFileNode, SeqIterator, SimpleDir, SimpleDirOps, SimpleFile,
        SimpleFileOperation, SimpleFs,
    },
};
use khal::paging::MappingFlags;
use kprocess::Process;
use ktask::{KtaskRef, WeakKtaskRef, current};
use memaddr::VirtAddr;
use memspace::backend::Backend;
use memspace_file::{CowBackend, FileBackend};

use crate::{hooks::ProcFsHooks, mounts::ProcMountIter};

struct ProcessTaskDir {
    fs: Arc<SimpleFs>,
    process: Weak<Process>,
    hooks: ProcFsHooks,
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

    fn lookup_child(&self, name: &str) -> VfsResult<NodeOpsMux> {
        let process = self.process.upgrade().ok_or(VfsError::NotFound)?;
        let tid = name.parse::<u32>().map_err(|_| VfsError::NotFound)?;
        let task = get_task(tid).map_err(|_| VfsError::NotFound)?;
        if task.as_thread().proc_data.proc.pid() != process.pid() {
            return Err(VfsError::NotFound);
        }

        Ok(NodeOpsMux::Dir(SimpleDir::new_maker(
            self.fs.clone(),
            Arc::new(ThreadDir {
                fs: self.fs.clone(),
                task: Arc::downgrade(&task),
                hooks: self.hooks,
            }),
        )))
    }

    fn supports_dentry_cache(&self) -> bool {
        false
    }
}

#[rustfmt::skip]
fn task_status(task: &KtaskRef) -> String {
    format!(
        "Tgid:\t{}\n\
        Pid:\t{}\n\
        Uid:\t0 0 0 0\n\
        Gid:\t0 0 0 0\n\
        Cpus_allowed:\t1\n\
        Cpus_allowed_list:\t0\n\
        Mems_allowed:\t1\n\
        Mems_allowed_list:\t0\n",
        task.as_thread().proc_data.proc.pid(),
        task.id().as_u64()
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

fn backend_file_mapping(
    area_start: VirtAddr,
    backend: &Backend,
) -> Option<(u64, u64, Option<String>)> {
    if let Some(cow) = backend.downcast_dynamic_ref::<CowBackend>() {
        let (file, file_start) = cow.file_mapping()?;
        let rel = area_start.as_usize().saturating_sub(cow.start().as_usize()) as u64;
        let inode = file.location().inode();
        let path = file
            .location()
            .absolute_path()
            .ok()
            .map(|it| it.to_string());
        Some((file_start + rel, inode, path))
    } else if let Some(file) = backend.downcast_dynamic_ref::<FileBackend>() {
        let inode = file.cache().location().inode();
        let path = file
            .cache()
            .location()
            .absolute_path()
            .ok()
            .map(|it| it.to_string());
        Some((file.offset_for(area_start), inode, path))
    } else {
        None
    }
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

        let proc_data = &task.as_thread().proc_data;
        let heap_top = proc_data.get_heap_top();
        let aspace = proc_data.aspace.lock();
        for area in aspace.areas() {
            let start = area.start().as_usize();
            let end = area.end().as_usize();
            let (offset, inode, path) =
                backend_file_mapping(area.start(), area.backend()).unwrap_or((0, 0, None));
            self.entries.push(MapsEntry {
                start,
                end,
                flags: area.flags(),
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

struct ThreadFdDir {
    fs: Arc<SimpleFs>,
    task: WeakKtaskRef,
    hooks: ProcFsHooks,
}

impl SimpleDirOps for ThreadFdDir {
    fn child_names<'a>(&'a self) -> Box<dyn Iterator<Item = Cow<'a, str>> + 'a> {
        let Some(task) = self.task.upgrade() else {
            return Box::new(iter::empty());
        };
        let ids = (self.hooks.fd_ids)(&task)
            .into_iter()
            .map(|id| Cow::Owned(id.to_string()))
            .collect::<Vec<_>>();
        Box::new(ids.into_iter())
    }

    fn lookup_child(&self, name: &str) -> VfsResult<NodeOpsMux> {
        let fs = self.fs.clone();
        let task = self.task.upgrade().ok_or(VfsError::NotFound)?;
        let fd = name.parse::<u32>().map_err(|_| VfsError::NotFound)?;
        let path = (self.hooks.fd_path)(&task, fd)?;
        Ok(SimpleFile::new(fs, NodeType::Symlink, move || Ok(path.clone())).into())
    }

    fn supports_dentry_cache(&self) -> bool {
        false
    }
}

struct ThreadDir {
    fs: Arc<SimpleFs>,
    task: WeakKtaskRef,
    hooks: ProcFsHooks,
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

    fn lookup_child(&self, name: &str) -> VfsResult<NodeOpsMux> {
        if !matches!(
            name,
            "mnt" | "pid" | "net" | "ipc" | "uts" | "user" | "cgroup"
        ) {
            return Err(VfsError::NotFound);
        }
        let target = namespace_link_target(name);
        Ok(
            SimpleFile::new(self.fs.clone(), NodeType::Symlink, move || {
                Ok(target.clone())
            })
            .into(),
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

    fn lookup_child(&self, name: &str) -> VfsResult<NodeOpsMux> {
        let fs = self.fs.clone();
        let task = self.task.upgrade().ok_or(VfsError::NotFound)?;
        Ok(match name {
            "stat" => SimpleFile::new_regular(fs, move || {
                Ok(format!("{}", TaskStat::from_thread(&task)?).into_bytes())
            })
            .into(),
            "status" => SimpleFile::new_regular(fs, move || Ok(task_status(&task))).into(),
            "oom_score_adj" => SimpleFile::new_regular(
                fs,
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
            )
            .into(),
            "task" => SimpleDir::new_maker(
                fs.clone(),
                Arc::new(ProcessTaskDir {
                    fs,
                    process: Arc::downgrade(&task.as_thread().proc_data.proc),
                    hooks: self.hooks,
                }),
            )
            .into(),
            "maps" => SeqFileNode::new_regular(fs, MapsIter::new(Arc::downgrade(&task))).into(),
            "mounts" => SeqFileNode::new_regular(fs.clone(), ProcMountIter::mounts()).into(),
            "mountinfo" => SeqFileNode::new_regular(fs.clone(), ProcMountIter::mountinfo()).into(),
            "mountstats" => SeqFileNode::new_regular(fs, ProcMountIter::mountstats()).into(),
            "cgroup" => SimpleFile::new_regular(fs.clone(), move || Ok("0::/\n")).into(),
            "ns" => SimpleDir::new_maker(fs.clone(), Arc::new(ThreadNsDir { fs })).into(),
            "cmdline" => SimpleFile::new_regular(fs, move || {
                let cmdline = task.as_thread().proc_data.cmdline.read();
                let mut buf = Vec::new();
                for arg in cmdline.iter() {
                    buf.extend_from_slice(arg.as_bytes());
                    buf.push(0);
                }
                Ok(buf)
            })
            .into(),
            "comm" => SimpleFile::new_regular(
                fs,
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
            )
            .into(),
            "exe" => SimpleFile::new(fs, NodeType::Symlink, move || {
                Ok(task.as_thread().proc_data.exe_path.read().clone())
            })
            .into(),
            "fd" => SimpleDir::new_maker(
                fs.clone(),
                Arc::new(ThreadFdDir {
                    fs,
                    task: Arc::downgrade(&task),
                    hooks: self.hooks,
                }),
            )
            .into(),
            _ => return Err(VfsError::NotFound),
        })
    }

    fn supports_dentry_cache(&self) -> bool {
        false
    }
}

pub struct ProcFsHandler {
    fs: Arc<SimpleFs>,
    hooks: ProcFsHooks,
}

impl ProcFsHandler {
    pub fn new(fs: Arc<SimpleFs>, hooks: ProcFsHooks) -> Self {
        Self { fs, hooks }
    }
}

impl SimpleDirOps for ProcFsHandler {
    fn child_names<'a>(&'a self) -> Box<dyn Iterator<Item = Cow<'a, str>> + 'a> {
        Box::new(
            processes()
                .into_iter()
                .map(|proc_data| Cow::Owned(proc_data.proc.pid().to_string()))
                .chain([Cow::Borrowed("self")]),
        )
    }

    fn lookup_child(&self, name: &str) -> VfsResult<NodeOpsMux> {
        let task = if name == "self" {
            current().clone()
        } else {
            let pid = name.parse::<u32>().map_err(|_| VfsError::NotFound)?;
            let proc_data = get_process_data(pid).map_err(|_| VfsError::NotFound)?;
            proc_data
                .proc
                .threads()
                .into_iter()
                .find_map(|tid| get_task(tid).ok())
                .ok_or(VfsError::NotFound)?
        };
        Ok(NodeOpsMux::Dir(SimpleDir::new_maker(
            self.fs.clone(),
            Arc::new(ThreadDir {
                fs: self.fs.clone(),
                task: Arc::downgrade(&task),
                hooks: self.hooks,
            }),
        )))
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
        maps_permissions, namespace_inode, namespace_link_target, parse_oom_score_adj_input,
        render_maps_line, special_mapping_name,
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
