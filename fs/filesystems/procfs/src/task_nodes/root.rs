// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{
    borrow::Cow,
    format,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
use core::{ffi::CStr, fmt, fmt::Write as _, iter, str};

use kaddr_layout::{SIGNAL_TRAMPOLINE, USER_HEAP_BASE, USER_STACK_SIZE, USER_STACK_TOP};
use kalloc_types::KBox;
use kcpu_id_map::text::{CpuTextDomain, bitmap_capacity, list_capacity, write_bitmap, write_list};
use kcred::Cred;
use kfd_objects::inotify::Inotify;
use khal::paging::MappingFlags;
use kprocess::{AsThread, LiveAddressSpace, PidFd, Process, TaskStat, procfs};
use ktask::{KCpuMask, KtaskRef, WeakKtaskRef, current};
use ktime_types::SystemTime;
#[cfg(feature = "tee")]
use kvfs::DirMapping;
use kvfs::{
    CommandFile, Dentry, DeviceId, FileOperations, GetattrQueryFlags, GetattrRequestMask,
    InodeOperations, InodeSymlinkOperations, LookupFlags, LookupIntent, MagicLinkOps, Metadata,
    MetadataUpdate, MountIdmap, NodeFlags, NodePermission, NodeType, Path, ResolvedObject,
    SeqFileInode, SeqIterator, SimpleDir, SimpleDirLookup, SimpleDirOps, SimpleFile,
    SimpleFileOperation, SimpleFs, SimpleFsNode, Umode, VfsError, VfsFile, VfsFileBuilder,
    VfsInode, VfsInodeInit, VfsResult,
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

    fn child_names<'a>(&'a self) -> VfsResult<KBox<dyn Iterator<Item = Cow<'a, str>> + 'a>> {
        let Some(process) = self.process.upgrade() else {
            return Ok(KBox::new(iter::empty())?);
        };
        Ok(KBox::new(
            process
                .threads()
                .into_iter()
                .map(|tid| tid.to_string().into()),
        )?)
    }

    fn lookup_child(&self, lookup: SimpleDirLookup<'_>, name: &str) -> VfsResult<Dentry> {
        let process = self.process.upgrade().ok_or(VfsError::NotFound)?;
        let tid = crate::name::to_number(name).ok_or(VfsError::NotFound)?;
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
        buf.push_str(&task_status(&task)?);
        Ok(())
    }
}

/// One `/proc/<pid>/status` sample for a single task.
///
/// The fields belong to the target task, never to the reader. They are sampled
/// from the task, signal manager, credentials, MM, and CPU affinity one after
/// another; like Linux's own `do_task_stat`/`proc_pid_status` paths, this is
/// not a cross-subsystem atomic snapshot. Fields that share one lock are still
/// sampled together: the pending/blocked masks come from a single `SigLock`
/// critical section.
struct TaskStatus {
    stat: TaskStat,
    tgid: u32,
    cred: Arc<Cred>,
    shared_pending: u64,
    cpumask: KCpuMask,
}

impl TaskStatus {
    fn capture(task: &KtaskRef) -> Self {
        let thread = task.as_thread();
        let (stat, masks) = TaskStat::sample_thread(task);
        let tgid = thread.process().pid();
        let cred = thread.real_cred();
        let shared_pending = masks.shared_pending().bits();
        let cpumask = task.cpumask();
        Self {
            stat,
            tgid,
            cred,
            shared_pending,
            cpumask,
        }
    }
}

fn task_status(task: &KtaskRef) -> VfsResult<String> {
    format_task_status(&TaskStatus::capture(task))
}

fn format_task_status(status: &TaskStatus) -> VfsResult<String> {
    let domain = CpuTextDomain::runtime().map_err(crate::cpu_text_error::map_cpu_text)?;
    let stat = &status.stat;
    // Linux prints "S (sleeping)": parsers key on the letter and the wording is
    // for whoever reads the file, so both are emitted as in fs/proc/array.c.
    let state_name = match stat.state {
        'R' => "running",
        'S' => "sleeping",
        'Z' => "zombie",
        _ => "unknown",
    };

    let mut out = String::new();
    push_status_line(
        &mut out,
        format_args!("Name:\t{}", escape_status_name(&stat.comm)),
    );
    push_status_line(
        &mut out,
        format_args!("State:\t{} ({})", stat.state, state_name),
    );
    push_status_line(&mut out, format_args!("Tgid:\t{}", status.tgid));
    push_status_line(&mut out, format_args!("Pid:\t{}", stat.pid));
    push_status_line(&mut out, format_args!("PPid:\t{}", stat.ppid));
    push_status_line(
        &mut out,
        format_args!(
            "Uid:\t{}\t{}\t{}\t{}",
            status.cred.ruid(),
            status.cred.euid(),
            status.cred.suid(),
            status.cred.fsuid()
        ),
    );
    push_status_line(
        &mut out,
        format_args!(
            "Gid:\t{}\t{}\t{}\t{}",
            status.cred.rgid(),
            status.cred.egid(),
            status.cred.sgid(),
            status.cred.fsgid()
        ),
    );
    // Linux's `proc_pid_status()` emits `VmSize` only when the task has an
    // address space; do not turn an absent MM into a misleading `0 kB` line.
    if let Some(vsize) = stat.vsize {
        // Linux `task_mem()` uses `seq_put_decimal_ull_width(..., 8)`, i.e.
        // a right-aligned decimal field followed by a literal " kB".
        push_status_line(&mut out, format_args!("VmSize:\t{:>8} kB", vsize / 1024));
    }
    push_status_line(&mut out, format_args!("Threads:\t{}", stat.num_threads));
    push_status_line(&mut out, format_args!("SigPnd:\t{:016x}", stat.signal));
    push_status_line(
        &mut out,
        format_args!("ShdPnd:\t{:016x}", status.shared_pending),
    );
    push_status_line(&mut out, format_args!("SigBlk:\t{:016x}", stat.blocked));
    push_status_line(&mut out, format_args!("SigIgn:\t{:016x}", stat.sigignore));
    push_status_line(&mut out, format_args!("SigCgt:\t{:016x}", stat.sigcatch));
    // Linux renders these with `%*pb` / `cpumask_pr_args` over the runtime
    // `nr_cpu_ids`, not over the compile-time `NR_CPUS` capacity.
    push_cpu_line(&mut out, "Cpus_allowed:\t", status.cpumask, &domain, false)?;
    push_cpu_line(
        &mut out,
        "Cpus_allowed_list:\t",
        status.cpumask,
        &domain,
        true,
    )?;
    // X-Kernel currently models a single memory node and has no mempolicy.
    // Linux parsers expect the mask/list pair, so report that sole node rather
    // than inventing per-task NUMA state.
    push_status_line(&mut out, format_args!("Mems_allowed:\t1"));
    push_status_line(&mut out, format_args!("Mems_allowed_list:\t0"));
    Ok(out)
}

fn push_cpu_line(
    out: &mut String,
    label: &str,
    mask: KCpuMask,
    domain: &CpuTextDomain,
    list: bool,
) -> VfsResult<()> {
    let capacity = if list {
        list_capacity(domain)
    } else {
        bitmap_capacity(domain)
    }
    .map_err(crate::cpu_text_error::map_cpu_text)?;
    let reserved = label
        .len()
        .checked_add(capacity)
        .and_then(|value| value.checked_add(1))
        .ok_or(VfsError::NoMemory)?;
    out.try_reserve(reserved).map_err(|_| VfsError::NoMemory)?;
    out.push_str(label);
    if list {
        write_list(mask, domain, out).map_err(crate::cpu_text_error::map_cpu_text)?;
    } else {
        write_bitmap(mask, domain, out).map_err(crate::cpu_text_error::map_cpu_text)?;
    }
    out.push('\n');
    Ok(())
}

/// Escapes a task name for `/proc/<pid>/status` like Linux.
///
/// Linux renders this field with `proc_task_name(..., true)`, whose escape
/// dictionary is exactly `"\n\\"`: newline becomes the two characters `\n` and
/// backslash becomes `\\`. Other bytes, including tabs and spaces, pass
/// through unchanged. Keeping the record single-line prevents a task name from
/// injecting additional status fields or records.
fn escape_status_name(name: &str) -> Cow<'_, str> {
    if !name
        .as_bytes()
        .iter()
        .any(|&byte| byte == b'\n' || byte == b'\\')
    {
        return Cow::Borrowed(name);
    }

    let mut escaped = String::with_capacity(name.len());
    for ch in name.chars() {
        match ch {
            '\n' => escaped.push_str("\\n"),
            '\\' => escaped.push_str("\\\\"),
            _ => escaped.push(ch),
        }
    }
    Cow::Owned(escaped)
}

/// Appends one procfs status line.
///
/// Writing into a `String` is infallible, so the formatting result is
/// intentionally discarded; this helper keeps the textual layout readable
/// instead of forcing one large, fragile `format!` literal.
fn push_status_line(out: &mut String, args: fmt::Arguments<'_>) {
    let _ = out.write_fmt(args);
    out.push('\n');
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

    // Linux `oom_score_adj_write()` copies into a zero-initialized buffer
    // (`char buffer[PROC_NUMBUF] = {}` + `copy_from_user`, fs/proc/base.c) and
    // parses it with `kstrtoint(strstrip(buffer))`, so the first NUL byte
    // terminates the number. Writers such as runc's nsexec send the netlink
    // payload verbatim including its NUL terminator ("0\0"); that trailing
    // NUL must not poison the parse.
    let data = match data.iter().position(|&b| b == 0) {
        Some(end) => &data[..end],
        None => data,
    };

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

/// Magic link backing `/proc/<pid>/root`.
struct ProcRootLink {
    node: SimpleFsNode,
    task: WeakKtaskRef,
}

impl ProcRootLink {
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

    fn check_access(&self) -> VfsResult<()> {
        let task = self.task.upgrade().ok_or(VfsError::NotFound)?;
        let target = task.as_thread();
        if target.is_exiting() || target.process().is_exited() {
            return Err(VfsError::NotFound);
        }
        if let Some(caller) = ktask::current().try_as_thread() {
            kprocess::ptrace::check_read_fs_creds_access(caller, target)
                .map_err(|_| VfsError::OperationNotPermitted)?;
        }
        Ok(())
    }

    fn target_path(&self) -> VfsResult<Path> {
        self.check_access()?;
        let task = self.task.upgrade().ok_or(VfsError::NotFound)?;
        let process = task.as_thread().process();
        let fs_context = process.fs_context().map_err(|_| VfsError::NotFound)?;
        Ok(fs_context.lock().root().clone())
    }

    fn target_len(&self) -> VfsResult<u64> {
        Ok(0)
    }
}

impl InodeOperations for ProcRootLink {
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

impl InodeSymlinkOperations for ProcRootLink {
    fn get_link(
        &self,
        _dentry: Option<&Dentry>,
        _inode: &kvfs::VfsInode,
        _done: &mut kvfs::DelayedCall,
    ) -> VfsResult<String> {
        self.target_path()?.display_path()
    }
}

impl FileOperations for ProcRootLink {
    fn supports_read(&self) -> bool {
        true
    }

    fn read(&self, _file: &VfsFile, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let target = self.target_path()?.display_path()?;
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

impl MagicLinkOps for ProcRootLink {
    fn readlink_display(&self) -> VfsResult<String> {
        self.target_path()?.display_path()
    }

    fn follow(&self, _intent: LookupIntent, flags: LookupFlags) -> VfsResult<ResolvedObject> {
        if flags.rejects_magic_links() || !flags.follows_final() {
            return Err(VfsError::FilesystemLoop);
        }
        Ok(ResolvedObject::location(self.target_path()?))
    }
}

fn proc_root_link_child(
    lookup: SimpleDirLookup<'_>,
    name: &str,
    link: Arc<ProcRootLink>,
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
    fn child_names<'a>(&'a self) -> VfsResult<KBox<dyn Iterator<Item = Cow<'a, str>> + 'a>> {
        let Some(task) = self.task.upgrade() else {
            return Ok(KBox::new(iter::empty())?);
        };
        let resources = task.as_thread().files();
        let Ok(fd_table) = resources.fd_table() else {
            return Ok(KBox::new(iter::empty())?);
        };
        let ids = fd_table
            .read()
            .ids()
            .map(|id| Cow::Owned(id.to_string()))
            .collect::<Vec<_>>();
        Ok(KBox::new(ids.into_iter())?)
    }

    fn lookup_child(&self, lookup: SimpleDirLookup<'_>, name: &str) -> VfsResult<Dentry> {
        let fs = self.fs.clone();
        let task = self.task.upgrade().ok_or(VfsError::NotFound)?;
        let fd = crate::name::to_number(name).ok_or(VfsError::NotFound)?;
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
        proc_fd_link_child(
            lookup,
            name,
            ProcFdLink::new(fs, KtaskRef::downgrade(&task), fd),
        )
    }

    fn supports_dentry_cache(&self) -> bool {
        false
    }
}
struct ThreadFdInfoDir {
    fs: Arc<SimpleFs>,
    task: WeakKtaskRef,
}

impl SimpleDirOps for ThreadFdInfoDir {
    fn child_names<'a>(&'a self) -> VfsResult<KBox<dyn Iterator<Item = Cow<'a, str>> + 'a>> {
        let Some(task) = self.task.upgrade() else {
            return Ok(KBox::new(iter::empty())?);
        };
        let Ok(fd_table) = task.as_thread().files().fd_table() else {
            return Ok(KBox::new(iter::empty())?);
        };
        let ids = fd_table
            .read()
            .ids()
            .map(|id| Cow::Owned(id.to_string()))
            .collect::<Vec<_>>();
        Ok(KBox::new(ids.into_iter())?)
    }

    fn lookup_child(&self, lookup: SimpleDirLookup<'_>, name: &str) -> VfsResult<Dentry> {
        let fs = self.fs.clone();
        let task = self.task.upgrade().ok_or(VfsError::NotFound)?;
        let fd = crate::name::to_number(name).ok_or(VfsError::NotFound)?;
        // Reject an unborn descriptor the way `/proc/<pid>/fd` does; the read
        // side re-checks, because the descriptor can close in between.
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
        let task = KtaskRef::downgrade(&task);
        lookup.file(
            name,
            SimpleFile::new_regular_with_permission(
                fs,
                NodePermission::OTHER_READ
                    | NodePermission::GROUP_READ
                    | NodePermission::OWNER_READ,
                move || {
                    let task = task.upgrade().ok_or(VfsError::NotFound)?;
                    // The descriptor may have closed since lookup. Linux
                    // re-looks it up at read time and reports ENOENT, so use
                    // the same translation as the lookup path rather than
                    // leaking the fd-table's EBADF through this node.
                    let snapshot =
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
                    // The access gate, as on Linux's fdinfo permission hook:
                    // `ptrace_may_access(PTRACE_MODE_READ_FSCREDS)` -- the
                    // caller's filesystem credentials, not the real ones
                    // `/proc/<pid>/exe` uses, so a setuid binary running as
                    // the target's user is not barred from its own fdinfo.
                    // A denied reader gets `PermissionDenied` (`EACCES`),
                    // which is what Linux returns here.
                    if let Some(caller) = ktask::current().try_as_thread() {
                        kprocess::ptrace::check_read_fs_creds_access(caller, task.as_thread())
                            .map_err(|_| VfsError::PermissionDenied)?;
                    }
                    Ok(render_fdinfo(snapshot.file(), snapshot.cloexec()))
                },
            ),
        )
    }

    fn supports_dentry_cache(&self) -> bool {
        false
    }
}

/// Linux `O_CLOEXEC`, which `fs/proc/fd.c` folds into the flags it prints for
/// a descriptor (`close_on_exec(fd, files)` -> `f_flags |= O_CLOEXEC`).
///
/// Declared here rather than pulled from a dependency: this crate needs the
/// single value, and procfs is not on the raw-sys surface.
const O_CLOEXEC: u32 = 0o2000000;

/// Renders one `/proc/<pid>/fdinfo/<fd>` entry.
///
/// The generic lines are the ones this kernel can answer: the file offset,
/// the open file description's flags, and the mount id of its location, in
/// Linux's order. Type-specific lines are rendered for objects that have them;
/// inotify and pidfd do, in man 5 proc_pid_fdinfo's field order and hex
/// formatting (`wd`, `ino`, `sdev`, `mask`, `ignored_mask`). The trailing
/// `fhandle-*` fields that page documents are absent because those exist only
/// for file handles exported through an NFS file handle, which this kernel
/// does not produce. `ignored_mask` is always 0: this object removes a watch
/// immediately instead of parking it in an ignored state, so there is no
/// interval to report. The inotify line keeps Linux's trailing space before
/// those absent `fhandle-*` fields.
fn render_fdinfo(file: &VfsFile, cloexec: bool) -> String {
    let mut out = String::new();
    // Linux prints these four lines for every descriptor, in this order and
    // with these formats (`fs/proc/fd.c`): `flags:` carries a literal leading
    // zero, is octal, and folds in the descriptor's close-on-exec bit.
    let mut flags = file.flags().bits();
    if cloexec {
        flags |= O_CLOEXEC;
    }
    let _ = writeln!(out, "pos:\t{}", file.position());
    let _ = writeln!(out, "flags:\t0{:o}", flags);
    let _ = writeln!(out, "mnt_id:\t{}", file.path().mount().mount_id());
    let _ = writeln!(out, "ino:\t{}", file.path().metadata().inode);
    if let Ok(inotify) = Inotify::from_file(file) {
        for watch in inotify.fdinfo_watches() {
            let _ = writeln!(
                out,
                // Linux's prefix ends in a space before `show_mark_fhandle`;
                // with no exported fhandle the line therefore ends in a space.
                "inotify wd:{:x} ino:{:x} sdev:{:x} mask:{:x} ignored_mask:0 ",
                watch.watch_descriptor, watch.inode, watch.device, watch.mask
            );
        }
    }
    // Linux's pidfd_show_fdinfo() emits the referenced task's id *as seen from
    // the PID namespace of the procfs instance being read*, and -1 once the pid
    // has no task attached (a waitable zombie still reports, a reaped one does
    // not). glibc and systemd resolve a clone3(CLONE_PIDFD) pidfd to a pid by
    // reading this line from /proc/self/fdinfo/<pidfd>; without it
    // posix_spawn/pidfd_spawn cannot report the child pid and service spawn
    // fails. This procfs does not model a per-mount PID namespace, so the
    // reading task's active namespace is the observer — the same number that
    // task's getpid(2)/wait(2) use. A kernel-task reader has none and gets the
    // internal number instead of panicking in `current_user_process()`.
    if let Ok(pidfd) = PidFd::from_file(file) {
        let observer = ktask::current()
            .try_as_thread()
            .map(|thread| thread.process().active_pid_namespace());
        let _ = writeln!(out, "Pid:\t{}", pidfd.fdinfo_pid(observer.as_ref()));
    }
    out
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
        task: KtaskRef::downgrade(task),
    };

    #[cfg(feature = "tee")]
    if tee::has_ta_info(task) {
        let mut ext = DirMapping::new();
        ext.add_child("ta_info", {
            let fs = fs.clone();
            let task = KtaskRef::downgrade(task);
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

/// Namespace kinds exposed through `/proc/<pid>/ns`.
///
/// Ordering matches Linux's proc directory listing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcNsKind {
    Mnt,
    Pid,
    PidForChildren,
    Net,
    Ipc,
    Uts,
    User,
    Cgroup,
}

impl ProcNsKind {
    const ALL: [ProcNsKind; 8] = [
        Self::Mnt,
        Self::Pid,
        Self::PidForChildren,
        Self::Net,
        Self::Ipc,
        Self::Uts,
        Self::User,
        Self::Cgroup,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Mnt => "mnt",
            Self::Pid => "pid",
            Self::PidForChildren => "pid_for_children",
            Self::Net => "net",
            Self::Ipc => "ipc",
            Self::Uts => "uts",
            Self::User => "user",
            Self::Cgroup => "cgroup",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }
}

/// Returns the namespace object a node of `kind` pins at lookup time.
///
/// Only `ns/net` does: libnetwork bind-mounts it and re-opens the file
/// through `setns(2)` long after the creator exited, so the reference must
/// outlive the task. Every other kind resolves through the task's weak
/// reference at open, keeping the "owner gone -> `NoSuchProcess`" contract.
///
/// The identity and the pinned object come from one snapshot: resolving the
/// namespace twice would let a concurrent `setns(CLONE_NEWNET)` give the
/// inode number of one namespace and the pin of another, so `stat()` (which
/// container tooling compares to identify namespaces) would disagree with the
/// namespace `open()` hands to `setns(2)`.
fn namespace_identity(
    kind: ProcNsKind,
    task: &KtaskRef,
) -> VfsResult<(u64, Option<Arc<kns::NetNamespace>>)> {
    match kind {
        ProcNsKind::Net => {
            let net_ns = task.as_thread().process().net_ns()?;
            Ok((net_ns.id().as_u64(), Some(net_ns)))
        }
        _ => Ok((task_namespace_inode(kind, task)?, None)),
    }
}

/// Returns the namespace-identity inode for `kind` of `task`.
///
/// The inode is the namespace's stable kernel identity, so two processes
/// attached to the same namespace report the same number (Linux nsfs style),
/// and `stat(2)` can distinguish namespaces. This is the identity used when a
/// namespace file is bind-mounted, and it is derived from the same snapshot as
/// any pinned namespace object (see [`namespace_identity`]).
fn task_namespace_inode(kind: ProcNsKind, task: &KtaskRef) -> VfsResult<u64> {
    let process = task.as_thread().process();
    Ok(match kind {
        ProcNsKind::Mnt => process.mnt_ns()?.id().as_u64(),
        // `ns/pid` is the namespace the task itself lives in
        // (task_active_pid_ns); the children slot is the separate
        // `ns/pid_for_children` entry below.
        ProcNsKind::Pid => process.active_pid_namespace().id(),
        ProcNsKind::PidForChildren => process.pid_ns_for_children()?.id(),
        ProcNsKind::Net => process.net_ns()?.id().as_u64(),
        ProcNsKind::Ipc => process.ipc_ns()?.id().as_u64(),
        ProcNsKind::Uts => process.uts_ns()?.id().as_u64(),
        ProcNsKind::Cgroup => process.cgroup_ns()?.id().as_u64(),
        ProcNsKind::User => kcred::initial_user_namespace().id().as_u64(),
    })
}

/// Snapshots the namespace of `kind` owned by `task` into an [`NsFd`].
///
/// The snapshot is taken at open time, pinning the current namespace object so
/// the fd stays valid across later `unshare`/`setns` by the owning process.
fn task_namespace_fd(kind: ProcNsKind, task: &KtaskRef) -> VfsResult<kns::NsFd> {
    let process = task.as_thread().process();
    Ok(match kind {
        ProcNsKind::Mnt => kns::NsFd::Mnt(process.mnt_ns()?),
        ProcNsKind::Pid => kns::NsFd::Pid(process.active_pid_namespace()),
        ProcNsKind::PidForChildren => kns::NsFd::Pid(process.pid_ns_for_children()?),
        ProcNsKind::Net => kns::NsFd::Net(process.net_ns()?),
        ProcNsKind::Ipc => kns::NsFd::Ipc(process.ipc_ns()?),
        ProcNsKind::Uts => kns::NsFd::Uts(process.uts_ns()?),
        ProcNsKind::Cgroup => kns::NsFd::Cgroup(process.cgroup_ns()?),
        ProcNsKind::User => kns::NsFd::User(kcred::initial_user_namespace()),
    })
}

/// A `/proc/<pid>/ns/<type>` file pinned to one namespace.
///
/// The inode number is the namespace identity (so `stat()` distinguishes
/// namespaces and a bind mount preserves identity), and opening the file
/// attaches an [`NsFd`] to the open description, which `setns(2)` downcasts.
///
/// Survival of the owning process depends on the kind: `ns/net` keeps the
/// namespace object pinned at lookup ([`pinned_net_ns`], Linux nsfs semantics),
/// so it still opens after the creator exited, while every other kind holds
/// only a weak task reference and fails with `ESRCH` once that process is
/// gone, mirroring `get_proc_task()`.
struct NsProcFile {
    task: WeakKtaskRef,
    kind: ProcNsKind,
    inode: u64,
    /// Network namespace files survive their owning process (Linux nsfs pins
    /// the namespace object): libnetwork bind-mounts `ns/net` and re-opens it
    /// through `setns(2)` long after the creator exited, so the namespace
    /// reference is pinned at lookup instead of resolved from the task at
    /// open. Every other kind keeps the weak task reference.
    pinned_net_ns: Option<Arc<kns::NetNamespace>>,
}

impl NsProcFile {
    fn materialize(
        lookup: SimpleDirLookup<'_>,
        kind: ProcNsKind,
        task: &KtaskRef,
    ) -> VfsResult<Dentry> {
        let (inode, pinned_net_ns) = namespace_identity(kind, task)?;
        let node = Arc::new(Self {
            task: KtaskRef::downgrade(task),
            kind,
            inode,
            pinned_net_ns,
        });
        let private_data: Arc<dyn core::any::Any + Send + Sync> = node.clone();
        let inode_operations: Arc<dyn InodeOperations> = node.clone();
        let file_operations: Arc<dyn FileOperations> = node.clone();
        let mode = Umode::new(
            NodeType::RegularFile,
            NodePermission::from_bits_truncate(0o444),
        );
        let init = VfsInodeInit::new(inode, 0, mode).with_owner_links_and_rdev(
            0,
            0,
            1,
            DeviceId::default(),
        );
        let vfs_inode = VfsInode::new_file_with_operations(
            private_data,
            inode_operations,
            file_operations,
            NodeFlags::NON_CACHEABLE,
            init,
        );
        Ok(lookup.file_from_inode(kind.name(), vfs_inode))
    }

    fn metadata(&self) -> Metadata {
        Metadata {
            device: 0,
            inode: self.inode,
            nlink: 1,
            mode: Umode::new(
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o444),
            ),
            uid: 0,
            gid: 0,
            size: 0,
            block_size: 0,
            blocks: 0,
            rdev: DeviceId::default(),
            atime: SystemTime::UNIX_EPOCH,
            mtime: SystemTime::UNIX_EPOCH,
            ctime: SystemTime::UNIX_EPOCH,
        }
    }
}

impl InodeOperations for NsProcFile {
    fn getattr(
        &self,
        _idmap: &MountIdmap,
        _path: Option<&Path>,
        _request_mask: GetattrRequestMask,
        _query_flags: GetattrQueryFlags,
    ) -> VfsResult<Metadata> {
        Ok(self.metadata())
    }

    fn setattr(
        &self,
        _idmap: &MountIdmap,
        _dentry: &Dentry,
        update: MetadataUpdate,
    ) -> VfsResult<MetadataUpdate> {
        // Namespace files are immutable snapshots; accept metadata updates that
        // do not mutate the identity so `chmod`/`touch` of time fields can
        // still resolve, mirroring how pseudo files typically behave.
        Ok(update)
    }
}

impl FileOperations for NsProcFile {
    fn open(self: Arc<Self>, _inode: &VfsInode, file: &mut VfsFileBuilder) -> VfsResult<()> {
        let nsfd = if let Some(net_ns) = &self.pinned_net_ns {
            kns::NsFd::Net(net_ns.clone())
        } else {
            let task = self.task.upgrade().ok_or(VfsError::NoSuchProcess)?;
            task_namespace_fd(self.kind, &task)?
        };
        file.set_private_data(Arc::new(nsfd));
        Ok(())
    }

    fn supports_read(&self) -> bool {
        true
    }

    fn read(&self, _file: &VfsFile, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Ok(0)
    }
}

struct ThreadNsDir {
    task: WeakKtaskRef,
}

impl SimpleDirOps for ThreadNsDir {
    fn child_names<'a>(&'a self) -> VfsResult<KBox<dyn Iterator<Item = Cow<'a, str>> + 'a>> {
        Ok(KBox::new(
            ProcNsKind::ALL
                .into_iter()
                .map(|kind| Cow::Borrowed(kind.name())),
        )?)
    }

    fn lookup_child(&self, lookup: SimpleDirLookup<'_>, name: &str) -> VfsResult<Dentry> {
        let kind = ProcNsKind::from_name(name).ok_or(VfsError::NotFound)?;
        let task = self.task.upgrade().ok_or(VfsError::NotFound)?;
        NsProcFile::materialize(lookup, kind, &task)
    }

    fn supports_dentry_cache(&self) -> bool {
        false
    }
}

impl SimpleDirOps for ThreadDir {
    fn child_names<'a>(&'a self) -> VfsResult<KBox<dyn Iterator<Item = Cow<'a, str>> + 'a>> {
        Ok(KBox::new(
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
                "root",
                "auxv",
                "fd",
                "fdinfo",
            ]
            .into_iter()
            .map(Cow::Borrowed),
        )?)
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
                        Ok(format!("{}", TaskStat::from_thread(&task)).into_bytes())
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
                            task: KtaskRef::downgrade(&task),
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
            "auxv" => {
                let task = task_weak.upgrade().ok_or(VfsError::NotFound)?;
                let owner = task.as_thread().real_cred();
                lookup.file(
                    name,
                    SimpleFile::new_snapshot_with_owner(
                        fs.clone(),
                        NodePermission::OWNER_READ,
                        owner.euid(),
                        owner.egid(),
                        {
                            let task_weak = task_weak.clone();
                            move || {
                                let task = upgrade_task(&task_weak)?;
                                // Linux exposes auxv only to a reader that may
                                // inspect the target task and keeps the file
                                // readable by the target's effective uid. The
                                // snapshot captures this image generation when
                                // the descriptor is opened.
                                if let Some(caller) = ktask::current().try_as_thread() {
                                    kprocess::ptrace::check_read_fs_creds_access(
                                        caller,
                                        task.as_thread(),
                                    )
                                    .map_err(|_| VfsError::PermissionDenied)?;
                                }
                                let auxv = task.as_thread().process().auxv()?;
                                Ok((*auxv).clone())
                            }
                        },
                    ),
                )
            }
            "exe" => proc_exe_link_child(lookup, name, ProcExeLink::new(fs.clone(), task_weak)),
            "root" => proc_root_link_child(lookup, name, ProcRootLink::new(fs.clone(), task_weak)),
            "fd" => {
                let task = task_weak.upgrade().ok_or(VfsError::NotFound)?;
                Ok(lookup.dir(
                    name,
                    SimpleDir::new_maker(
                        fs.clone(),
                        Arc::new(ThreadFdDir {
                            fs: fs.clone(),
                            task: KtaskRef::downgrade(&task),
                        }),
                    ),
                ))
            }
            "fdinfo" => {
                let task = task_weak.upgrade().ok_or(VfsError::NotFound)?;
                // Linux exposes fdinfo only to a reader that may inspect the
                // task (`ptrace_may_access(PTRACE_MODE_READ_FSCREDS)`). The
                // gate here is the per-reader credentials check in each file's
                // read closure below -- the filesystem-credentials variant of
                // the ptrace check -- not the file mode: the mode stays
                // readable so that a reader which passes the check is not
                // stopped one layer earlier, and a reader which fails it gets
                // `EACCES` from the read, as Linux's fdinfo permission hook
                // reports at open time. The directory itself
                // keeps the default mode and only lists descriptor numbers,
                // which `/proc/<pid>/fd` already names.
                Ok(lookup.dir(
                    name,
                    SimpleDir::new_maker(
                        fs.clone(),
                        Arc::new(ThreadFdInfoDir {
                            fs: fs.clone(),
                            task: KtaskRef::downgrade(&task),
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
    fn child_names<'a>(&'a self) -> VfsResult<KBox<dyn Iterator<Item = Cow<'a, str>> + 'a>> {
        Ok(KBox::new(
            procfs::visible_processes()
                .into_iter()
                .map(|process| Cow::Owned(process.pid().to_string()))
                .chain([Cow::Borrowed("self")]),
        )?)
    }

    fn lookup_child(&self, lookup: SimpleDirLookup<'_>, name: &str) -> VfsResult<Dentry> {
        if name == "self" {
            return lookup.file(
                name,
                SimpleFile::new(self.fs.clone(), NodeType::Symlink, proc_self_target),
            );
        }

        let pid = crate::name::to_number(name).ok_or(VfsError::NotFound)?;
        let task = procfs::process_task(pid).map_err(|_| VfsError::NotFound)?;
        Ok(make_thread_dir(lookup, name, self.fs.clone(), &task))
    }

    fn supports_dentry_cache(&self) -> bool {
        false
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/task_nodes_root.rs");
