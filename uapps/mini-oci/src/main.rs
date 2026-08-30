// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{
    env,
    ffi::{CString, OsStr},
    fs, io,
    os::unix::{ffi::OsStrExt, process::CommandExt},
    path::{Path, PathBuf},
    process::Command,
};

use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Spec {
    #[serde(default)]
    hostname: String,
    root: Root,
    process: Process,
    #[serde(default)]
    mounts: Vec<Mount>,
}

#[derive(Deserialize)]
struct Root {
    path: PathBuf,
}

#[derive(Deserialize)]
struct Process {
    args: Vec<String>,
    #[serde(default)]
    env: Vec<String>,
    #[serde(default = "default_cwd")]
    cwd: PathBuf,
    #[serde(default)]
    user: User,
    #[serde(default)]
    capabilities: Option<serde_json::Value>,
    #[serde(default)]
    no_new_privileges: bool,
    #[serde(default)]
    seccomp: Option<serde_json::Value>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct User {
    #[serde(default)]
    uid: u32,
    #[serde(default)]
    gid: u32,
    #[serde(default)]
    additional_gids: Vec<u32>,
}

#[derive(Deserialize)]
struct Mount {
    destination: PathBuf,
    #[serde(rename = "type")]
    fs_type: String,
    #[serde(default)]
    source: String,
    #[serde(default)]
    options: Vec<String>,
}

fn default_cwd() -> PathBuf {
    PathBuf::from("/")
}

fn cstring(value: &OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL"))
}

fn mount(
    source: Option<&Path>,
    target: &Path,
    fs_type: Option<&str>,
    flags: libc::c_ulong,
) -> io::Result<()> {
    let source = source.map(|value| cstring(value.as_os_str())).transpose()?;
    let target = cstring(target.as_os_str())?;
    let fs_type = fs_type.map(|value| CString::new(value).unwrap());
    // SAFETY: all non-null pointers reference live NUL-terminated strings for
    // the duration of the syscall; mount data is intentionally null.
    let result = unsafe {
        libc::mount(
            source
                .as_ref()
                .map_or(std::ptr::null(), |value| value.as_ptr()),
            target.as_ptr(),
            fs_type
                .as_ref()
                .map_or(std::ptr::null(), |value| value.as_ptr()),
            flags,
            std::ptr::null(),
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn configure_mounts(bundle: &Path, rootfs: &Path, mounts: &[Mount]) -> io::Result<()> {
    mount(
        None,
        Path::new("/"),
        None,
        (libc::MS_PRIVATE | libc::MS_REC) as _,
    )?;
    for entry in mounts {
        let relative = entry
            .destination
            .strip_prefix("/")
            .map_err(|_| io::Error::other("mount destination must be absolute"))?;
        let target = rootfs.join(relative);
        fs::create_dir_all(&target)?;
        let readonly = entry.options.iter().any(|option| option == "ro");
        match entry.fs_type.as_str() {
            "bind" => {
                let source = Path::new(&entry.source);
                let source = if source.is_absolute() {
                    source.to_path_buf()
                } else {
                    bundle.join(source)
                };
                mount(Some(&source), &target, None, libc::MS_BIND as _)?;
                if readonly {
                    mount(
                        None,
                        &target,
                        None,
                        (libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY) as _,
                    )?;
                }
            }
            "proc" => mount(Some(Path::new("proc")), &target, Some("proc"), 0)?,
            "tmpfs" => mount(Some(Path::new("tmpfs")), &target, Some("tmpfs"), 0)?,
            other => return Err(io::Error::other(format!("unsupported mount type {other}"))),
        }
    }
    Ok(())
}

fn enter_container(bundle: &Path, spec: &Spec, ready_fd: libc::c_int) -> io::Result<()> {
    let mut ready = [0u8; 1];
    // SAFETY: ready_fd is the read end of a live pipe and `ready` is writable.
    if unsafe { libc::read(ready_fd, ready.as_mut_ptr().cast(), 1) } != 1 {
        return Err(io::Error::last_os_error());
    }
    if !spec.hostname.is_empty() {
        // SAFETY: hostname bytes are readable for their reported length.
        if unsafe { libc::sethostname(spec.hostname.as_ptr().cast(), spec.hostname.len()) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    if spec.process.capabilities.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "OCI capabilities are not implemented",
        ));
    }
    if spec.process.seccomp.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "OCI seccomp is not implemented",
        ));
    }
    if spec.process.no_new_privileges {
        // SAFETY: PR_SET_NO_NEW_PRIVS takes scalar arguments only.
        if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    let rootfs = bundle.join(&spec.root.path).canonicalize()?;
    configure_mounts(bundle, &rootfs, &spec.mounts)?;
    let root = cstring(rootfs.as_os_str())?;
    // SAFETY: root is a live NUL-terminated path.
    if unsafe { libc::chroot(root.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    env::set_current_dir(&spec.process.cwd)?;
    if !spec.process.user.additional_gids.is_empty() {
        // SAFETY: the group slice remains live for the duration of setgroups.
        if unsafe {
            libc::setgroups(
                spec.process.user.additional_gids.len(),
                spec.process.user.additional_gids.as_ptr(),
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    // SAFETY: setresgid/setresuid take scalar IDs and do not dereference pointers.
    if unsafe {
        libc::setresgid(
            spec.process.user.gid,
            spec.process.user.gid,
            spec.process.user.gid,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: see the setresgid justification above.
    if unsafe {
        libc::setresuid(
            spec.process.user.uid,
            spec.process.user.uid,
            spec.process.user.uid,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let program = spec
        .process
        .args
        .first()
        .ok_or_else(|| io::Error::other("process.args is empty"))?;
    let mut command = Command::new(program);
    command.args(&spec.process.args[1..]).env_clear();
    for entry in &spec.process.env {
        let (name, value) = entry
            .split_once('=')
            .ok_or_else(|| io::Error::other("invalid environment entry"))?;
        command.env(name, value);
    }
    Err(command.exec())
}

fn require_cgroup_hierarchy() -> io::Result<()> {
    fs::metadata("/sys/fs/cgroup/cgroup.controllers")
        .map(|_| ())
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                format!("cgroup v2 hierarchy is unavailable: {error}"),
            )
        })
}

fn run(id: &str, bundle: &Path) -> io::Result<i32> {
    let spec: Spec = serde_json::from_slice(&fs::read(bundle.join("config.json"))?)?;
    require_cgroup_hierarchy()?;
    let cgroup = PathBuf::from("/sys/fs/cgroup").join(format!("oci-{id}"));
    fs::create_dir(&cgroup)?;

    let mut pipe = [0; 2];
    // SAFETY: pipe points to storage for two file descriptors.
    if unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let flags = libc::CLONE_NEWNS | libc::CLONE_NEWUTS | libc::CLONE_NEWIPC | libc::SIGCHLD;
    // SAFETY: this uses clone's fork-like form with a null child stack. No
    // memory-sharing flag is set, so parent and child have separate address spaces.
    let pid = unsafe { libc::syscall(libc::SYS_clone, flags, 0, 0, 0, 0) as libc::pid_t };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        // SAFETY: the child closes its unused pipe endpoint.
        unsafe { libc::close(pipe[1]) };
        let result = enter_container(bundle, &spec, pipe[0]);
        eprintln!("mini-oci child: {}", result.unwrap_err());
        // SAFETY: terminate without running post-clone inherited destructors.
        unsafe { libc::_exit(127) };
    }
    // SAFETY: the parent closes its unused pipe endpoint.
    unsafe { libc::close(pipe[0]) };
    fs::write(cgroup.join("cgroup.procs"), format!("{pid}\n"))?;
    // SAFETY: pipe[1] is live and the byte is readable.
    if unsafe { libc::write(pipe[1], [1u8].as_ptr().cast(), 1) } != 1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: waitpid writes to `status`, and pid names our child.
    let mut status = 0;
    if unsafe { libc::waitpid(pid, &mut status, 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut remove_error = None;
    for _ in 0..100 {
        match fs::remove_dir(&cgroup) {
            Ok(()) => {
                remove_error = None;
                break;
            }
            Err(error) if error.raw_os_error() == Some(libc::EBUSY) => {
                remove_error = Some(error);
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    }
    if let Some(error) = remove_error {
        return Err(error);
    }
    if libc::WIFEXITED(status) {
        Ok(libc::WEXITSTATUS(status))
    } else {
        Ok(128 + libc::WTERMSIG(status))
    }
}

fn main() -> io::Result<()> {
    let mut args = env::args_os().skip(1);
    let command = args
        .next()
        .ok_or_else(|| io::Error::other("usage: mini-oci run ID BUNDLE"))?;
    if command != "run" {
        return Err(io::Error::other("only run is supported"));
    }
    let id = args
        .next()
        .ok_or_else(|| io::Error::other("missing container ID"))?;
    let bundle = args
        .next()
        .ok_or_else(|| io::Error::other("missing bundle path"))?;
    let status = run(id.to_string_lossy().as_ref(), Path::new(&bundle))?;
    std::process::exit(status)
}
