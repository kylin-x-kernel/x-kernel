// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{format, string::String, sync::Arc};
use core::fmt::Write;

use kbuild_config::ARCH;
use kcpu_id_map::for_each_present_logical_cpu;
use kvfs::{
    CommandFile, DirMapping, NodePermission, SimpleDir, SimpleFile, SimpleFileOperation, SimpleFs,
    VfsError,
};

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
            // Runtime log-level control: `cat` shows the current level as a
            // number in X-Kernel's own level order (0=off, 1=error, 2=warn,
            // 3=info, 4=debug, 5=trace); a privileged write (`echo 4 >
            // /proc/sys/kernel/printk` or `echo debug > ...`) switches the
            // level immediately without a rebuild.
            //
            // NOTE: although this node reuses the Linux `/proc/sys/kernel/
            // printk` path, the value domain is X-Kernel-private and NOT
            // Linux `console_loglevel`-compatible: here higher = more
            // verbose (4 = debug), while Linux 4 means warn-only and its
            // read side reports four numbers instead of one.
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
                            klogger::set_log_level_filter(level);
                            Ok(None)
                        }
                    }),
                ),
            );
            SimpleDir::new_maker(fs.clone(), Arc::new(kernel))
        });

        SimpleDir::new_maker(fs.clone(), Arc::new(sys))
    });
}

#[cfg(unittest)]
mod tests {
    use alloc::sync::Arc;

    use kvfs::{
        DirMapping, FileSystemType, Filename, Mount, NodePermission, OpenFlags, SimpleDir, SimpleFs,
    };
    use unittest::{assert, assert_eq, def_test};

    use super::add_root_entries;

    static TEST_FILE_SYSTEM_TYPE: FileSystemType = FileSystemType::internal("procfs-printk-test");

    #[def_test]
    fn printk_node_reads_and_writes_runtime_log_level() {
        let super_block = SimpleFs::new_with(&TEST_FILE_SYSTEM_TYPE, 0, |fs| {
            let mut root = DirMapping::new();
            add_root_entries(&mut root, fs.clone());
            SimpleDir::new_maker(fs, Arc::new(root))
        });
        let root = Mount::new_root(&super_block).root_path();
        let printk = Filename::new("/sys/kernel/printk")
            .open_with_flags_at(
                &root,
                &root,
                OpenFlags::READ_WRITE.bits(),
                NodePermission::empty(),
                NodePermission::empty(),
                kcred::initial_cred(),
            )
            .unwrap();

        let previous = klogger::current_log_level();

        // A level number (with trailing newline, as `echo` writes it) takes
        // effect immediately...
        assert_eq!(printk.write(b"4\n"), Ok(2));
        assert_eq!(klogger::current_log_level(), log::LevelFilter::Debug);
        // ...and so does a level name.
        assert_eq!(printk.write(b"warn"), Ok(4));
        assert_eq!(klogger::current_log_level(), log::LevelFilter::Warn);

        // Reading reports the canonical number of the active level.
        // (`read_from` with an explicit position sidesteps the file offset
        // advanced by the writes above.)
        let mut buf = [0u8; 16];
        let mut pos = 0;
        assert_eq!(printk.read_from(&mut buf, &mut pos), Ok(2));
        assert_eq!(&buf[..2], b"2\n");

        // Invalid or out-of-range levels are rejected and leave the level
        // unchanged.
        assert!(printk.write(b"banana").is_err());
        assert!(printk.write(b"6").is_err());
        assert_eq!(klogger::current_log_level(), log::LevelFilter::Warn);

        klogger::set_log_level_filter(previous);
    }
}
