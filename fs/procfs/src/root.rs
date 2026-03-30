// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{
    format,
    string::{String, ToString},
    sync::Arc,
};

use indoc::indoc;
use kcore::vfs::{DirMaker, DirMapping, SimpleDir, SimpleDirOps, SimpleFile, SimpleFs};

use crate::{hooks::ProcFsHooks, mounts::render_proc_mounts, task::ProcFsHandler};

const DUMMY_MEMINFO: &str = indoc! {"
    MemTotal:       32536204 kB
    MemFree:         5506524 kB
    MemAvailable:   18768344 kB
    Buffers:            3264 kB
    Cached:         14454588 kB
    SwapCached:            0 kB
    Active:         18229700 kB
    Inactive:        6540624 kB
    Active(anon):   11380224 kB
    Inactive(anon):        0 kB
    Active(file):    6849476 kB
    Inactive(file):  6540624 kB
    Unevictable:      930088 kB
    Mlocked:            1136 kB
    SwapTotal:       4194300 kB
    SwapFree:        4194300 kB
    Zswap:                 0 kB
    Zswapped:              0 kB
    Dirty:             47952 kB
    Writeback:             0 kB
    AnonPages:      10992512 kB
    Mapped:          1361184 kB
    Shmem:           1068056 kB
    KReclaimable:     341440 kB
    Slab:             628996 kB
    SReclaimable:     341440 kB
    SUnreclaim:       287556 kB
    KernelStack:       28704 kB
    PageTables:        85308 kB
    SecPageTables:      2084 kB
    NFS_Unstable:          0 kB
    Bounce:                0 kB
    WritebackTmp:          0 kB
    CommitLimit:    20462400 kB
    Committed_AS:   45105316 kB
    VmallocTotal:   34359738367 kB
    VmallocUsed:      205924 kB
    VmallocChunk:          0 kB
    Percpu:            23840 kB
    HardwareCorrupted:     0 kB
    AnonHugePages:   1417216 kB
    ShmemHugePages:        0 kB
    ShmemPmdMapped:        0 kB
    FileHugePages:    477184 kB
    FilePmdMapped:    288768 kB
    CmaTotal:              0 kB
    CmaFree:               0 kB
    Unaccepted:            0 kB
    HugePages_Total:       0
    HugePages_Free:        0
    HugePages_Rsvd:        0
    HugePages_Surp:        0
    Hugepagesize:       2048 kB
    Hugetlb:               0 kB
    DirectMap4k:     1739900 kB
    DirectMap2M:    31492096 kB
    DirectMap1G:     1048576 kB
"};

fn render_interrupts(irq_count: usize) -> String {
    format!("0: {}\n", irq_count)
}

pub fn builder(fs: Arc<SimpleFs>, hooks: ProcFsHooks) -> DirMaker {
    let mut root = DirMapping::new();
    root.add(
        "cmdline",
        SimpleFile::new_regular(fs.clone(), || {
            Ok(match khal::cmdline() {
                Some(cmdline) if !cmdline.is_empty() => format!("{cmdline}\n"),
                _ => "\n".to_string(),
            })
        }),
    );
    root.add(
        "mounts",
        SimpleFile::new_regular(fs.clone(), || Ok(render_proc_mounts())),
    );
    root.add(
        "meminfo",
        SimpleFile::new_regular(fs.clone(), || Ok(DUMMY_MEMINFO)),
    );
    root.add(
        "meminfo2",
        SimpleFile::new_regular(fs.clone(), || {
            let allocator = kalloc::global_allocator();
            Ok(format!("{:?}\n", allocator.usages()))
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
                Ok("0\n".to_string())
            }
        }),
    );
    root.add(
        "interrupts",
        SimpleFile::new_regular(fs.clone(), move || {
            Ok(render_interrupts((hooks.irq_count)()))
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
            SimpleDir::new_maker(fs.clone(), Arc::new(kernel))
        });

        SimpleDir::new_maker(fs.clone(), Arc::new(sys))
    });

    let proc_dir = ProcFsHandler::new(fs.clone(), hooks);
    SimpleDir::new_maker(fs, Arc::new(proc_dir.chain(root)))
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert_eq, def_test};

    use super::render_interrupts;

    #[def_test]
    fn test_render_interrupts_includes_trailing_newline() {
        assert_eq!(render_interrupts(867), "0: 867\n");
    }
}
