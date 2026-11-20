// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{borrow::Cow, format, string::String, sync::Arc};
use core::fmt::Write;

use kcpu_id_map::nr_cpus;
use kfd_objects::inotify_limits::InotifyLimit;
use kprocess::{AsThread, TaskStat, procfs};
use kvfs::{
    CommandFile, Dentry, DirMapping, GetattrQueryFlags, GetattrRequestMask, InodeOperations,
    Metadata, MetadataUpdate, MountIdmap, NodeFlags, NodePermission, Path, Permission, SimpleDir,
    SimpleFile, SimpleFileOperation, SimpleFileOps, SimpleFs, VfsError, VfsFile, VfsInode,
    VfsInodeInit, VfsResult,
};

/// One `/proc/sys/fs/inotify/` control.
///
/// Linux checks `proc_sys_permission` on open and `sysctl_perm` on every write
/// against the caller's *effective* uid, not the filesystem uid used by generic
/// DAC. The generic simple-file permission check would therefore reject a root
/// task that only changed `fsuid`, or admit an unprivileged task that changed
/// `fsuid` to zero. The wrapper applies that gate at open, while the write
/// closure repeats it because the descriptor may be inherited. Like Linux's
/// `proc_sys_setattr`, it rejects mode and owner changes so the fixed table
/// mode reported by `getattr` is also the mode access checks enforce.
struct InotifySysctlFile {
    file: Arc<SimpleFile>,
}

impl InodeOperations for InotifySysctlFile {
    fn getattr(
        &self,
        idmap: &MountIdmap,
        path: Option<&Path>,
        request_mask: GetattrRequestMask,
        query_flags: GetattrQueryFlags,
    ) -> VfsResult<Metadata> {
        self.file.getattr(idmap, path, request_mask, query_flags)
    }

    fn setattr(
        &self,
        idmap: &MountIdmap,
        dentry: &Dentry,
        update: MetadataUpdate,
    ) -> VfsResult<MetadataUpdate> {
        // Linux's `proc_sys_setattr` refuses `ATTR_MODE | ATTR_UID | ATTR_GID`
        // with `EPERM`; timestamps and size still go through `setattr_copy`.
        // Rejecting them keeps the table mode (0644) authoritative instead of
        // letting a successful chmod make `getattr` advertise a mode that the
        // euid-based permission hook does not enforce.
        if update.mode.is_some() || update.owner.is_some() {
            return Err(VfsError::OperationNotPermitted);
        }
        self.file.setattr(idmap, dentry, update)
    }

    fn permission(
        &self,
        _idmap: &MountIdmap,
        _inode: &VfsInode,
        permission: Permission,
        cred: &kcred::Cred,
    ) -> VfsResult<()> {
        let access = permission
            .intersection(Permission::MAY_READ | Permission::MAY_WRITE | Permission::MAY_EXEC);
        // The node is a root-owned regular file with mode 0644: everyone may
        // read it, only euid 0 may write it, and it is not executable. Linux
        // derives those decisions from euid, unlike generic DAC's fsuid.
        if access.contains(Permission::MAY_EXEC)
            || (access.contains(Permission::MAY_WRITE) && !cred.has_root_euid())
        {
            return Err(VfsError::PermissionDenied);
        }
        Ok(())
    }
}

/// One `/proc/sys/kernel` UTS name node.
enum UtsNameField {
    Hostname,
    DomainName,
}

/// `/proc/sys/kernel/hostname` and `domainname` (Linux `proc_doutsstring`).
///
/// Reads report the caller's UTS namespace and writes replace that namespace's
/// value, so a `systemd ProtectHostname` bind mount behaves like Linux. The
/// write path trims one trailing newline and requires `CAP_SYS_ADMIN`, the same
/// checks `proc_dostring` and sethostname(2) apply. `try_as_thread` keeps a
/// kernel-task reader failing cleanly instead of panicking in
/// `current_user_process()`.
struct UtsNameFile {
    field: UtsNameField,
}

/// Size of a UTS name buffer including the terminator: Linux `UTSLEN`
/// (`__NEW_UTS_LEN` = 64 name bytes plus the NUL).
const UTS_NAME_MAXLEN: usize = 65;

impl SimpleFileOps for UtsNameFile {
    fn read_all(&self) -> VfsResult<Cow<'_, [u8]>> {
        let task = ktask::current();
        let thread = task.try_as_thread().ok_or(VfsError::NotFound)?;
        let uts = thread.process().uts_ns()?;
        // Report the namespace bytes verbatim: the UTS owner accepts non-UTF-8
        // names and uname(2) returns them unchanged, so decoding here would
        // make the two interfaces disagree.
        let mut name = match self.field {
            UtsNameField::Hostname => uts.nodename(),
            UtsNameField::DomainName => uts.domainname(),
        };
        name.push(b'\n');
        Ok(Cow::Owned(name))
    }

    /// Applies Linux's `proc_dostring` write under the default strict mode:
    /// the write starts at the file position, stops at the first NUL or
    /// newline, truncates the value there, and a write that starts past the
    /// current value is a successful no-op.
    fn write(&self, _file: &VfsFile, data: &[u8], offset: u64) -> VfsResult<usize> {
        // Linux `test_perm()` gates sysctl writes on GLOBAL_ROOT_UID, not on
        // a capability in the UTS namespace owner.
        if !kprocess::current_cred().has_root_euid() {
            return Err(VfsError::OperationNotPermitted);
        }
        let task = ktask::current();
        let thread = task.try_as_thread().ok_or(VfsError::NotFound)?;
        let uts = thread.process().uts_ns()?;
        let current = match self.field {
            UtsNameField::Hostname => uts.nodename(),
            UtsNameField::DomainName => uts.domainname(),
        };
        let current_len = current
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(current.len())
            .min(UTS_NAME_MAXLEN - 1);
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        if start > current_len {
            return Ok(data.len());
        }
        let mut name = current[..start].to_vec();
        for &byte in data {
            if byte == 0 || byte == b'\n' || name.len() >= UTS_NAME_MAXLEN - 1 {
                break;
            }
            name.push(byte);
        }
        match self.field {
            UtsNameField::Hostname => uts.set_nodename(&name),
            UtsNameField::DomainName => uts.set_domainname(&name),
        }
        .map_err(|_| VfsError::InvalidInput)?;
        Ok(data.len())
    }
}

pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    root.add(
        "cmdline",
        SimpleFile::new_regular(fs.clone(), || {
            Ok(match khal::cmdline() {
                Some(cmdline) if !cmdline.is_empty() => format!("{cmdline}\n"),
                _ => String::from("\n"),
            })
        }),
    );
    root.add(
        "instret",
        SimpleFile::new_regular(fs.clone(), || {
            #[cfg(any(target_arch = "riscv32", target_arch = "riscv64"))]
            {
                Ok(format!("{}\n", riscv::register::instret::read64()))
            }
            #[cfg(not(any(target_arch = "riscv32", target_arch = "riscv64")))]
            {
                Ok(String::from("0\n"))
            }
        }),
    );
    root.add(
        "cpuinfo",
        SimpleFile::new_regular(fs.clone(), super::cpuinfo::read),
    );
    root.add(
        "stat",
        SimpleFile::new_regular(fs.clone(), || {
            // Linux exposes aggregate CPU time in USER_HZ ticks. This kernel
            // does not yet account per-thread CPU time (`TaskStat::utime` and
            // `stime` stay zero), so the aggregate reports no busy time and
            // idle is wall-clock uptime times the CPU count. `intr`, `ctxt`,
            // `processes` and `softirq` are not tracked either and stay zero;
            // `procs_running` is sampled from task state. The per-CPU lines a
            // full SMP kernel prints are not synthesized here.
            const USER_HZ: u64 = 100;
            let mut user = 0u64;
            let mut system = 0u64;
            let mut running = 0u64;
            for process in procfs::visible_processes() {
                for tid in procfs::thread_ids(&process) {
                    let Ok(task) = procfs::thread_task(tid) else {
                        continue;
                    };
                    let stat = TaskStat::from_thread(&task);
                    user = user.saturating_add(stat.utime);
                    system = system.saturating_add(stat.stime);
                    if stat.state == 'R' {
                        running = running.saturating_add(1);
                    }
                }
            }
            let uptime_ns = ktime::boottime().as_nanos_u64_saturating();
            let uptime_ticks = uptime_ns / (1_000_000_000 / USER_HZ);
            let idle = uptime_ticks
                .saturating_mul(nr_cpus() as u64)
                .saturating_sub(user.saturating_add(system));
            let btime = ktime::realtime().unix_seconds() - (uptime_ns / 1_000_000_000) as i64;
            Ok(format!(
                "cpu  {user} 0 {system} {idle} 0 0 0 0 0 0\nintr 0\nctxt 0\nbtime \
                 {btime}\nprocesses 0\nprocs_running {running}\nprocs_blocked 0\nsoftirq 0 0 0 0 \
                 0 0 0 0 0 0 0\n"
            ))
        }),
    );
    root.add(
        "filesystems",
        SimpleFile::new_regular(fs.clone(), || {
            let mut info = String::new();
            for file_system_type in kvfs::registered_filesystems() {
                #[expect(
                    clippy::unwrap_used,
                    reason = "CG-021-no-panic-on-reachable-failure: String writes are infallible \
                              and the formatted integers and strings cannot return fmt::Error"
                )]
                writeln!(
                    info,
                    "{}\t{}",
                    if file_system_type.requires_device() {
                        ""
                    } else {
                        "nodev"
                    },
                    file_system_type.name(),
                )
                .unwrap();
            }
            Ok(info)
        }),
    );
}

#[expect(
    clippy::too_many_lines,
    reason = "CG-028-focused-functions: TODO(cg): legacy sysctl registration; split kernel \
              logging handlers from directory assembly"
)]
pub(crate) fn add_sys_entries(sys: &mut DirMapping, fs: Arc<SimpleFs>) {
    sys.add("kernel", {
        let mut kernel = DirMapping::new();
        kernel.add(
            "pid_max",
            SimpleFile::new_regular(fs.clone(), || Ok("32768\n")),
        );
        // systemd ProtectHostname bind-mounts these over the service's
        // namespace; they must exist. Report and update the caller's UTS
        // namespace state (same source as uname(2)) so reads stay
        // consistent with sethostname/setdomainname.
        kernel.add(
            "hostname",
            SimpleFile::new_regular_with_permission(
                fs.clone(),
                NodePermission::from_bits_truncate(0o644),
                UtsNameFile {
                    field: UtsNameField::Hostname,
                },
            ),
        );
        kernel.add(
            "domainname",
            SimpleFile::new_regular_with_permission(
                fs.clone(),
                NodePermission::from_bits_truncate(0o644),
                UtsNameFile {
                    field: UtsNameField::DomainName,
                },
            ),
        );
        // systemd reads these while building a service's exec context; a
        // missing node makes the spawn fail with ENOENT.
        kernel.add(
            "cap_last_cap",
            SimpleFile::new_regular(fs.clone(), || Ok(format!("{}\n", kcred::CAP_LAST_CAP))),
        );
        kernel.add(
            "overflowuid",
            SimpleFile::new_regular(fs.clone(), || Ok("65534\n")),
        );
        kernel.add(
            "overflowgid",
            SimpleFile::new_regular(fs.clone(), || Ok("65534\n")),
        );
        kernel.add(
            "threads-max",
            SimpleFile::new_regular(fs.clone(), || Ok("65536\n")),
        );
        kernel.add("random", {
            let mut random = DirMapping::new();
            // A per-boot identifier; systemd uses it for invocation IDs.
            random.add(
                "boot_id",
                SimpleFile::new_regular(fs.clone(), || {
                    Ok(format!("{}\n", crate::boot_id::boot_id()))
                }),
            );
            SimpleDir::new_maker(fs.clone(), Arc::new(random))
        });
        kernel.add("pty", {
            let mut pty = DirMapping::new();
            let permission = NodePermission::from_bits_truncate(0o444);
            pty.add(
                "max",
                SimpleFile::new_regular_with_permission(fs.clone(), permission, || {
                    Ok(format!("{}\n", ktty::tty::pty_limit()))
                }),
            );
            pty.add(
                "nr",
                SimpleFile::new_regular_with_permission(fs.clone(), permission, || {
                    Ok(format!("{}\n", ktty::tty::pty_allocated_count()))
                }),
            );
            SimpleDir::new_maker(fs.clone(), Arc::new(pty))
        });
        // Runtime console-level control, with the contract this node has
        // always had: `cat` reports the console default level as a number in
        // X-Kernel's own order (0=off, 1=error, 2=warn, 3=info, 4=debug,
        // 5=trace), and a privileged write (`echo 4 > ...`, `echo debug >
        // ...`) changes it immediately without a rebuild.
        //
        // The per-target filter lives in `log_filter` below instead of here:
        // this node keeps its historical single-value shape (and stays
        // readable by existing scripts), and the module filter gets a node of
        // its own rather than riding along in a private expression.
        //
        // NOTE: although this node reuses the Linux `/proc/sys/kernel/
        // printk` path, the value domain is X-Kernel-private and NOT
        // Linux `console_loglevel`-compatible: numbers here are
        // `0..=5` with higher = more verbose, while Linux reports four
        // fields (`console_loglevel default_message_loglevel min max`) and
        // treats a higher number as *less* verbose.
        kernel.add(
            "printk",
            SimpleFile::new_regular_with_permission(
                fs.clone(),
                NodePermission::OWNER_READ
                    | NodePermission::OWNER_WRITE
                    | NodePermission::GROUP_READ
                    | NodePermission::OTHER_READ,
                CommandFile::new(|operation| match operation {
                    SimpleFileOperation::Read => Ok(Some(format!(
                        "{}\n",
                        klogger::level_number(klogger::current_log_level())
                    ))),
                    SimpleFileOperation::Write { file, data } => {
                        if !file.cred().has_root_euid() {
                            return Err(VfsError::OperationNotPermitted);
                        }
                        let text =
                            core::str::from_utf8(data).map_err(|_| VfsError::InvalidInput)?;
                        let level = klogger::parse_log_level(text).ok_or(VfsError::InvalidInput)?;
                        // Level only: the module filter below is a separate
                        // knob and must survive a level write.
                        klogger::set_console_default(level);
                        Ok(None)
                    }
                }),
            ),
        );
        // Per-target console filter: an optional default level plus
        // `target=level` overrides, matched by module-path prefix. Reads
        // return the canonical spec of what is in force; a privileged write
        // installs a whole spec. A level above the build cap is accepted (it
        // stays readable) but has no records to act on.
        kernel.add(
            "log_filter",
            SimpleFile::new_regular_with_permission(
                fs.clone(),
                NodePermission::OWNER_READ
                    | NodePermission::OWNER_WRITE
                    | NodePermission::GROUP_READ
                    | NodePermission::OTHER_READ,
                CommandFile::new(|operation| match operation {
                    SimpleFileOperation::Read => {
                        let mut buf = [0u8; klogger::MAX_FILTER_SPEC];
                        let len = klogger::filter_spec(&mut buf).ok_or(VfsError::InvalidInput)?;
                        let spec = core::str::from_utf8(&buf[..len])
                            .map_err(|_| VfsError::InvalidInput)?;
                        Ok(Some(format!("{spec}\n")))
                    }
                    SimpleFileOperation::Write { file, data } => {
                        if !file.cred().has_root_euid() {
                            return Err(VfsError::OperationNotPermitted);
                        }
                        let text =
                            core::str::from_utf8(data).map_err(|_| VfsError::InvalidInput)?;
                        klogger::set_filter_spec(text).map_err(|_| VfsError::InvalidInput)?;
                        Ok(None)
                    }
                }),
            ),
        );
        SimpleDir::new_maker(fs.clone(), Arc::new(kernel))
    });

    // Linux publishes the inotify resource limits here. The numbers are
    // the live limits the kernel enforces (kfd_objects owns the ledger),
    // so what a reader sees cannot drift from what a reservation checks.
    // Linux's `proc_sys_permission` checks the opener's euid and
    // `proc_sys_call_handler` rechecks the writer's current euid on every
    // write, so the inode wrapper applies the euid gate at open and the
    // closure passes `current_cred()` rather than the file's open-time
    // credential. The value domain is a base-0 count in `0..=i32::MAX`,
    // and lowering a limit does not revoke existing instances, watches, or
    // queued events.
    sys.add("fs", {
        let mut fs_dir = DirMapping::new();
        fs_dir.add("inotify", {
            let mut inotify = DirMapping::new();
            for (name, limit) in [
                ("max_queued_events", InotifyLimit::QueuedEvents),
                ("max_user_instances", InotifyLimit::UserInstances),
                ("max_user_watches", InotifyLimit::UserWatches),
            ] {
                let file = SimpleFile::new_regular_with_permission(
                    fs.clone(),
                    NodePermission::OWNER_READ
                        | NodePermission::OWNER_WRITE
                        | NodePermission::GROUP_READ
                        | NodePermission::OTHER_READ,
                    CommandFile::new(move |operation| match operation {
                        SimpleFileOperation::Read => Ok(Some(format!("{}\n", limit.value()))),
                        SimpleFileOperation::Write { file: _, data } => {
                            limit.write(data, &kprocess::current_cred())?;
                            Ok(None)
                        }
                    }),
                );
                let node = Arc::new(InotifySysctlFile { file });
                let init: VfsInodeInit = node.file.inode_init();
                let inode = VfsInode::new_file_with_operations(
                    node.clone(),
                    node.clone(),
                    node.file.clone(),
                    NodeFlags::NON_CACHEABLE,
                    init,
                );
                inotify.add(name, inode);
            }
            SimpleDir::new_maker(fs.clone(), Arc::new(inotify))
        });
        let mut quota = DirMapping::new();
        quota.add(
            "allocated_dquots",
            SimpleFile::new_regular(fs.clone(), || {
                let allocated: usize = kvfs::super_block_registry()
                    .live_super_blocks()
                    .iter()
                    .map(|sb| sb.quota_info().cached_records())
                    .sum();
                Ok(format!("{allocated}\n"))
            }),
        );
        fs_dir.add_dir("quota", SimpleDir::new_maker(fs.clone(), Arc::new(quota)));
        SimpleDir::new_maker(fs, Arc::new(fs_dir))
    });
}

unittest::path_mod_test!(tests, "../../ut-tests/basic_nodes_root.rs");
