// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux path-resolution helpers for filesystem syscalls and exec.

use alloc::{borrow::ToOwned, string::String, sync::Arc};
use core::ffi::c_int;

use kerrno::{KError, KResult};
use kfd::{FileLike, Kstat};
use kfs::{Directory, File, FsContext};
use kthread::{current_process_state, current_thread, get_process_state};
use kvfs::Location;
use linux_raw_sys::general::{AT_EMPTY_PATH, AT_FDCWD, AT_SYMLINK_NOFOLLOW, O_NOFOLLOW, O_PATH};

/// Executes a function with the file system context for the given directory file descriptor.
///
/// If `dirfd` is `AT_FDCWD`, uses the current directory context.
/// Otherwise, resolves the directory from the given file descriptor and uses it as the base.
pub fn with_fs<R>(dirfd: c_int, f: impl FnOnce(&mut FsContext) -> KResult<R>) -> KResult<R> {
    let fs_context = kthread::current_fs_context();
    let mut fs = fs_context.lock();
    if dirfd == AT_FDCWD {
        f(&mut fs)
    } else {
        let dir = kthread::current_resources().get_file_like_as::<Directory>(dirfd)?;
        let dir = dir.inner().clone();
        f(&mut fs.with_current_dir(dir)?)
    }
}

/// The coarse shape of a path string before any runtime resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    /// The path string is empty.
    Empty,
    /// The path string is relative.
    Relative,
    /// The path string is absolute.
    Absolute,
}

impl PathKind {
    /// Classify a raw path string by its leading form.
    pub fn classify(path: &str) -> Self {
        if path.is_empty() {
            Self::Empty
        } else if path.starts_with('/') {
            Self::Absolute
        } else {
            Self::Relative
        }
    }
}

/// Whether a path is a procfd path, and if so, its parsed components.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParsedProcFdPath {
    /// Not a `/proc/.../fd/...` path.
    NotProcFd,
    /// A procfd path with invalid syntax.
    Invalid,
    /// A successfully parsed procfd path.
    Parsed(ProcFdPath),
}

/// High-level classification of a raw path string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassifiedPath {
    /// A normal path string, classified only by leading form.
    Plain(PathKind),
    /// A successfully parsed `/proc/<pid>/fd/<fd>` path.
    ProcFd(ProcFdPath),
    /// A string that looks like procfd syntax but is invalid.
    InvalidProcFd,
}

/// Parsed components of a `/proc/<pid>/fd/<fd>` path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcFdPath {
    pub pid: u32,
    pub fd: c_int,
}

/// Classifies a raw path string without touching runtime state.
pub fn classify_path(path: &str, current_pid: u32) -> ClassifiedPath {
    match classify_procfd_path(path, current_pid) {
        ParsedProcFdPath::NotProcFd => ClassifiedPath::Plain(PathKind::classify(path)),
        ParsedProcFdPath::Invalid => ClassifiedPath::InvalidProcFd,
        ParsedProcFdPath::Parsed(procfd) => ClassifiedPath::ProcFd(procfd),
    }
}

/// Classifies a path as a procfd reference, returning parsed pid and fd.
///
/// Supports `/proc/self/fd/<fd>` and `/proc/<pid>/fd/<fd>` forms.
pub fn classify_procfd_path(path: &str, current_pid: u32) -> ParsedProcFdPath {
    let Some(rest) = path.strip_prefix("/proc/") else {
        return ParsedProcFdPath::NotProcFd;
    };

    let (pid, fd) = if let Some(fd) = rest.strip_prefix("self/fd/") {
        (current_pid, fd)
    } else {
        let Some((pid, fd)) = rest.split_once("/fd/") else {
            return ParsedProcFdPath::NotProcFd;
        };
        let Ok(pid) = pid.parse() else {
            return ParsedProcFdPath::Invalid;
        };
        (pid, fd)
    };

    if pid == 0 || fd.is_empty() || fd.contains('/') {
        return ParsedProcFdPath::Invalid;
    }

    let Ok(fd) = fd.parse::<c_int>() else {
        return ParsedProcFdPath::Invalid;
    };
    if fd < 0 {
        return ParsedProcFdPath::Invalid;
    }

    ParsedProcFdPath::Parsed(ProcFdPath { pid, fd })
}

/// Result of resolving a path at a given directory.
#[derive(Clone)]
pub enum ResolveAtResult {
    File(Location),
    Other(Arc<dyn FileLike>),
}

#[derive(Clone)]
pub struct ResolvedPath {
    target: ResolveAtResult,
    display_path: String,
    kind: ResolvedPathKind,
}

#[derive(Clone, Copy)]
enum ResolvedPathKind {
    FilesystemObject,
    ProcFd,
}

pub enum PathSource {
    Path(String),
    Resolved(ResolvedPath),
}

impl ResolveAtResult {
    pub fn into_file(self) -> Option<Location> {
        match self {
            Self::File(file) => Some(file),
            Self::Other(_) => None,
        }
    }

    pub fn stat(&self) -> KResult<Kstat> {
        match self {
            Self::File(file) => file.metadata().map(Kstat::from),
            Self::Other(file_like) => file_like.stat(),
        }
    }
}

impl ResolvedPath {
    fn from_file_like(display_path: String, file_like: Arc<dyn FileLike>) -> KResult<Self> {
        Ok(Self {
            target: resolve_result_from_file_like(file_like)?,
            display_path,
            kind: ResolvedPathKind::FilesystemObject,
        })
    }

    fn from_procfd_file_like(display_path: String, file_like: Arc<dyn FileLike>) -> KResult<Self> {
        Ok(Self {
            target: ResolveAtResult::File(file_like_location(file_like)?),
            display_path,
            kind: ResolvedPathKind::ProcFd,
        })
    }

    pub fn display_path(&self) -> &str {
        &self.display_path
    }

    pub fn location(&self) -> Option<Location> {
        match &self.target {
            ResolveAtResult::File(location) => Some(location.clone()),
            ResolveAtResult::Other(_) => None,
        }
    }

    pub fn is_procfd(&self) -> bool {
        matches!(self.kind, ResolvedPathKind::ProcFd)
    }

    pub fn into_result(self) -> ResolveAtResult {
        self.target
    }
}

fn resolve_result_from_file_like(file_like: Arc<dyn FileLike>) -> KResult<ResolveAtResult> {
    let f = file_like.clone();
    if let Some(file) = f.downcast_ref::<File>() {
        Ok(ResolveAtResult::File(file.backend()?.location().clone()))
    } else if let Some(dir) = f.downcast_ref::<Directory>() {
        Ok(ResolveAtResult::File(dir.inner().clone()))
    } else {
        Ok(ResolveAtResult::Other(file_like))
    }
}

fn file_like_location(file_like: Arc<dyn FileLike>) -> KResult<Location> {
    let f = file_like.clone();
    if let Some(file) = f.downcast_ref::<File>() {
        Ok(file.backend()?.location().clone())
    } else if let Some(dir) = f.downcast_ref::<Directory>() {
        Ok(dir.inner().clone())
    } else {
        Err(KError::OperationNotSupported)
    }
}

fn resolve_empty_path(dirfd: c_int, flags: u32) -> KResult<PathSource> {
    if flags & AT_EMPTY_PATH == 0 {
        return Err(KError::NotFound);
    }
    let proc_state = current_process_state();
    let file_like = proc_state.resources.get_file_like(dirfd)?;
    ResolvedPath::from_file_like(file_like.path().into_owned(), file_like).map(PathSource::Resolved)
}

fn is_live_procfd_path(path: &str) -> bool {
    matches!(
        classify_path(path, current_thread().pid()),
        ClassifiedPath::ProcFd(_)
    )
}

fn resolve_live_procfd(path: &str) -> KResult<Option<ResolvedPath>> {
    let current_pid = current_thread().pid();
    let procfd = match classify_path(path, current_pid) {
        ClassifiedPath::Plain(_) => return Ok(None),
        ClassifiedPath::InvalidProcFd => return Err(KError::NotFound),
        ClassifiedPath::ProcFd(procfd) => procfd,
    };

    let file_like = procfd_entry(procfd.pid, procfd.fd)?;
    ResolvedPath::from_procfd_file_like(path.to_owned(), file_like).map(Some)
}

fn resolve_nonempty_path_source(path: &str, flags: u32) -> KResult<PathSource> {
    if flags & AT_SYMLINK_NOFOLLOW == 0
        && let Some(resolved) = resolve_live_procfd(path)?
    {
        return Ok(PathSource::Resolved(resolved));
    }
    Ok(PathSource::Path(path.to_owned()))
}

pub fn resolve_path_source(dirfd: c_int, path: Option<&str>, flags: u32) -> KResult<PathSource> {
    match path {
        Some(path) if !path.is_empty() => resolve_nonempty_path_source(path, flags),
        _ => resolve_empty_path(dirfd, flags),
    }
}

pub fn resolve_open_path_source(
    dirfd: c_int,
    path: Option<&str>,
    open_flags: u32,
) -> KResult<PathSource> {
    if let Some(path) = path
        && open_flags & O_NOFOLLOW != 0
        && open_flags & O_PATH == 0
        && is_live_procfd_path(path)
    {
        return Err(KError::FilesystemLoop);
    }
    resolve_path_source(
        dirfd,
        path,
        if open_flags & O_NOFOLLOW != 0 {
            AT_SYMLINK_NOFOLLOW
        } else {
            0
        },
    )
}

fn resolve_filesystem_path(dirfd: c_int, path: &str, flags: u32) -> KResult<Location> {
    with_fs(dirfd, |fs| {
        if flags & AT_SYMLINK_NOFOLLOW != 0 {
            fs.resolve_no_follow(path)
        } else {
            fs.resolve(path)
        }
    })
}

pub fn resolve_at(dirfd: c_int, path: Option<&str>, flags: u32) -> KResult<ResolveAtResult> {
    match resolve_path_source(dirfd, path, flags)? {
        PathSource::Resolved(path) => Ok(path.into_result()),
        PathSource::Path(path) => {
            resolve_filesystem_path(dirfd, &path, flags).map(ResolveAtResult::File)
        }
    }
}

fn procfd_entry(pid: u32, fd: c_int) -> KResult<Arc<dyn FileLike>> {
    let proc_state = get_process_state(pid)?;
    proc_state
        .resources
        .fd_table()
        .read()
        .get(fd as usize)
        .map(|entry| entry.inner().clone())
        .ok_or(KError::BadFileDescriptor)
}

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::{
        ClassifiedPath, ParsedProcFdPath, PathKind, ProcFdPath, classify_path, classify_procfd_path,
    };

    #[def_test]
    fn path_kind_classifies_empty_relative_and_absolute() {
        assert_eq!(PathKind::classify(""), PathKind::Empty);
        assert_eq!(PathKind::classify("tmp/a"), PathKind::Relative);
        assert_eq!(PathKind::classify("/tmp/a"), PathKind::Absolute);
    }

    #[def_test]
    fn classify_path_distinguishes_plain_and_procfd() {
        assert_eq!(
            classify_path("", 123),
            ClassifiedPath::Plain(PathKind::Empty)
        );
        assert_eq!(
            classify_path("tmp/a", 123),
            ClassifiedPath::Plain(PathKind::Relative)
        );
        assert_eq!(
            classify_path("/tmp/a", 123),
            ClassifiedPath::Plain(PathKind::Absolute)
        );
        assert_eq!(
            classify_path("/proc/self/fd/7", 123),
            ClassifiedPath::ProcFd(ProcFdPath { pid: 123, fd: 7 })
        );
        assert_eq!(
            classify_path("/proc/self/fd/", 123),
            ClassifiedPath::InvalidProcFd
        );
    }

    #[def_test]
    fn procfd_path_parser_accepts_self_current_and_foreign_pid() {
        assert!(matches!(
            classify_procfd_path("/proc/self/fd/7", 123),
            ParsedProcFdPath::Parsed(ProcFdPath { pid: 123, fd: 7 })
        ));
        assert!(matches!(
            classify_procfd_path("/proc/123/fd/8", 123),
            ParsedProcFdPath::Parsed(ProcFdPath { pid: 123, fd: 8 })
        ));
        assert!(matches!(
            classify_procfd_path("/proc/456/fd/9", 123),
            ParsedProcFdPath::Parsed(ProcFdPath { pid: 456, fd: 9 })
        ));
    }

    #[def_test]
    fn procfd_path_parser_rejects_invalid_paths() {
        assert!(matches!(
            classify_procfd_path("/proc/0/fd/7", 123),
            ParsedProcFdPath::Invalid
        ));
        assert!(matches!(
            classify_procfd_path("/proc/self/fd/", 123),
            ParsedProcFdPath::Invalid
        ));
        assert!(matches!(
            classify_procfd_path("/proc/self/fd/7/extra", 123),
            ParsedProcFdPath::Invalid
        ));
        assert!(matches!(
            classify_procfd_path("/proc/not-a-pid/fd/7", 123),
            ParsedProcFdPath::Invalid
        ));
        assert!(matches!(
            classify_procfd_path("/proc/self/fd/-1", 123),
            ParsedProcFdPath::Invalid
        ));
        assert!(matches!(
            classify_procfd_path("/proc/123/fd/999999999999999999999", 123),
            ParsedProcFdPath::Invalid
        ));
        assert!(matches!(
            classify_procfd_path("/tmp/123/fd/7", 123),
            ParsedProcFdPath::NotProcFd
        ));
    }
}
