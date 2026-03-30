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
use indoc::indoc;
use kcore::{
    task::{AsThread, TaskStat, get_process_data, get_task, processes},
    vfs::{NodeOpsMux, RwFile, SimpleDir, SimpleDirOps, SimpleFile, SimpleFileOperation, SimpleFs},
};
use kprocess::Process;
use ktask::{KtaskRef, WeakKtaskRef, current};

use crate::{
    hooks::ProcFsHooks,
    mounts::{render_mountinfo, render_mountstats, render_proc_mounts},
};

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
            "maps" => SimpleFile::new_regular(fs, move || {
                Ok(indoc! {"
                    7f000000-7f001000 r--p 00000000 00:00 0          [vdso]
                    7f001000-7f003000 r-xp 00001000 00:00 0          [vdso]
                    7f003000-7f005000 r--p 00003000 00:00 0          [vdso]
                    7f005000-7f007000 rw-p 00005000 00:00 0          [vdso]
                "})
            })
            .into(),
            "mounts" => {
                SimpleFile::new_regular(fs.clone(), move || Ok(render_proc_mounts())).into()
            }
            "mountinfo" => {
                SimpleFile::new_regular(fs.clone(), move || Ok(render_mountinfo())).into()
            }
            "mountstats" => SimpleFile::new_regular(fs, move || Ok(render_mountstats())).into(),
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
    use unittest::{assert_eq, def_test};

    use super::{
        format_oom_score_adj, namespace_inode, namespace_link_target, parse_oom_score_adj_input,
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
}
