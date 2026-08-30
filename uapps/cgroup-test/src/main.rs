// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{
    fs,
    io::{self, ErrorKind},
    path::PathBuf,
};

fn write(path: PathBuf, value: &str) -> io::Result<()> {
    fs::write(&path, value)
        .map_err(|error| io::Error::new(error.kind(), format!("write {}: {error}", path.display())))
}

fn main() -> io::Result<()> {
    let initial = fs::read_to_string("/proc/self/cgroup")?;
    if initial != "0::/\n" {
        return Err(io::Error::other(format!(
            "unexpected initial membership: {initial:?}"
        )));
    }

    let root = PathBuf::from("/sys/fs/cgroup");
    write(root.join("cgroup.subtree_control"), "+pids\n")?;
    let group = root.join(format!("xkernel-test-{}", std::process::id()));
    fs::create_dir(&group)?;
    write(group.join("pids.max"), "1\n")?;
    write(group.join("cgroup.procs"), "0\n")?;

    let moved = fs::read_to_string("/proc/self/cgroup")?;
    let expected = format!("0::/{}\n", group.file_name().unwrap().to_string_lossy());
    if moved != expected {
        return Err(io::Error::other(format!(
            "migration mismatch: expected {expected:?}, got {moved:?}"
        )));
    }

    // SAFETY: fork has no pointer arguments. The child exits immediately and
    // does not touch state protected by another thread because this test is
    // single-threaded.
    let child = unsafe { libc::fork() };
    if child == 0 {
        // SAFETY: `_exit` terminates the child without running inherited Rust
        // destructors after fork.
        unsafe { libc::_exit(0) };
    }
    if child > 0 {
        return Err(io::Error::other("fork unexpectedly bypassed pids.max"));
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() != Some(libc::EAGAIN) && error.kind() != ErrorKind::WouldBlock {
        return Err(io::Error::other(format!(
            "fork failed with {error}, expected EAGAIN"
        )));
    }

    write(root.join("cgroup.procs"), "0\n")?;
    fs::remove_dir(&group)?;
    println!("cgroup v2 pids regression: PASS");
    Ok(())
}
