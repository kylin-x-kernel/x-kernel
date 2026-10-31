// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{format, string::String, sync::Arc};
use core::fmt::Write;

use kbuild_config::ARCH;
use kcpu_id_map::for_each_present_logical_cpu;
use kfd_objects::inotify_limits::InotifyLimit;
use kvfs::{
    CommandFile, Dentry, DirMapping, GetattrQueryFlags, GetattrRequestMask, InodeOperations,
    Metadata, MetadataUpdate, MountIdmap, NodeFlags, NodePermission, Path, Permission, SimpleDir,
    SimpleDirLookup, SimpleFile, SimpleFileOperation, SimpleFs, VfsError, VfsInode, VfsInodeInit,
    VfsResult,
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
            || (access.contains(Permission::MAY_WRITE) && cred.euid() != 0)
        {
            return Err(VfsError::PermissionDenied);
        }
        Ok(())
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "CG-028-focused-functions: linear registration table: each statement registers one \
              independent procfs root entry"
)]
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
        SimpleFile::new_regular(fs.clone(), || {
            let mut info = String::new();
            for_each_present_logical_cpu(|present_index, cpu_id, cpu_count| {
                #[expect(
                    clippy::unwrap_used,
                    reason = "CG-021-no-panic-on-reachable-failure: String writes are infallible \
                              and the formatted integers and strings cannot return fmt::Error"
                )]
                writeln!(
                    info,
                    "processor\t: {}\ncpu cores\t: {}\narchitecture\t: {}",
                    cpu_id.as_usize(),
                    cpu_count,
                    ARCH,
                )
                .unwrap();
                if present_index + 1 < cpu_count {
                    info.push('\n');
                }
            });
            Ok(info)
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
    root.add("sys", {
        let mut sys = DirMapping::new();

        sys.add("kernel", {
            let mut kernel = DirMapping::new();
            kernel.add(
                "pid_max",
                SimpleFile::new_regular(fs.clone(), || Ok("32768\n")),
            );
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
                            if !file.cred().is_privileged() {
                                return Err(VfsError::OperationNotPermitted);
                            }
                            let text =
                                core::str::from_utf8(data).map_err(|_| VfsError::InvalidInput)?;
                            let level =
                                klogger::parse_log_level(text).ok_or(VfsError::InvalidInput)?;
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
                            let len =
                                klogger::filter_spec(&mut buf).ok_or(VfsError::InvalidInput)?;
                            let spec = core::str::from_utf8(&buf[..len])
                                .map_err(|_| VfsError::InvalidInput)?;
                            Ok(Some(format!("{spec}\n")))
                        }
                        SimpleFileOperation::Write { file, data } => {
                            if !file.cred().is_privileged() {
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
                    inotify.add_child(name, move |lookup: SimpleDirLookup<'_>, name| {
                        let init: VfsInodeInit = node.file.inode_init();
                        let inode = VfsInode::new_file_with_operations(
                            node.clone(),
                            node.clone(),
                            node.file.clone(),
                            NodeFlags::NON_CACHEABLE,
                            init,
                        );
                        Ok(lookup.file_from_inode(name, inode))
                    });
                }
                SimpleDir::new_maker(fs.clone(), Arc::new(inotify))
            });
            SimpleDir::new_maker(fs.clone(), Arc::new(fs_dir))
        });

        SimpleDir::new_maker(fs.clone(), Arc::new(sys))
    });
}

unittest::path_mod_test!(tests, "../../ut-tests/basic_nodes_root.rs");
