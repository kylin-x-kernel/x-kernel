// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! 9P client and fid objects.

use alloc::{boxed::Box, collections::BTreeSet, string::String, sync::Arc, vec, vec::Vec};
use core::sync::atomic::{AtomicU32, Ordering};

use kerrno::{KError, KResult, LinuxError};
use ksync::Mutex;
use log::warn;

use crate::{
    message::{Message, read_qid, read_str, read_u8, read_u16, read_u32, read_u64},
    protocol::*,
    transport::P9Transport,
};

const FID_MODE_UNOPENED: u32 = u32::MAX;
const FID_MODE_INVALID: u32 = u32::MAX - 1;
const P9_DIRENT_NAME_MAX: usize = 255;

/// Access mode used to open a 9P fid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum P9OpenMode {
    /// Open for reading.
    Read      = P9_DOTL_RDONLY as u8,
    /// Open for writing.
    Write     = P9_DOTL_WRONLY as u8,
    /// Open for reading and writing.
    ReadWrite = P9_DOTL_RDWR as u8,
}

bitflags::bitflags! {
    /// Linux dotl open flags associated with a fid.
    ///
    /// `TLOPEN` carries only the bits admitted by Linux's `P9L_MODE_MASK`.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct P9OpenFlags: u32 {
        /// Create the file if it does not exist.
        const CREATE = 0o0000100;
        /// Require exclusive creation.
        const EXCLUSIVE = 0o0000200;
        /// Do not assign a controlling terminal.
        const NO_CONTROLLING_TTY = 0o0000400;
        /// Truncate the file while opening it.
        const TRUNCATE = 0o0001000;
        /// Append each write at the server-visible end of file.
        const APPEND = 0o0002000;
        /// Request nonblocking behavior.
        const NONBLOCK = 0o0004000;
        /// Request data-integrity synchronized writes.
        const DSYNC = 0o0010000;
        /// Enable signal-driven I/O.
        const ASYNC = 0o0020000;
        /// Request direct I/O.
        const DIRECT = 0o0040000;
        /// Allow large-file offsets.
        const LARGE_FILE = 0o0100000;
        /// Require a directory.
        const DIRECTORY = 0o0200000;
        /// Do not follow the final symbolic link.
        const NO_FOLLOW = 0o0400000;
        /// Do not update access time.
        const NO_ATIME = 0o1000000;
        /// Close the descriptor during exec.
        const CLOSE_ON_EXEC = 0o2000000;
        /// Request file-integrity synchronized writes.
        const SYNC = 0o4000000;
    }
}

impl P9OpenMode {
    fn flags(self) -> u32 {
        u32::from(self as u8)
    }
}

/// File attributes returned by 9P2000.L `TGETATTR`.
#[derive(Clone, Debug)]
pub struct FileAttr {
    /// Server identity and type information for the file.
    pub qid: Qid,
    /// Linux-compatible mode bits.
    pub mode: u32,
    /// Owning user ID.
    pub uid: u32,
    /// Owning group ID.
    pub gid: u32,
    /// Hard-link count.
    pub nlink: u64,
    /// Encoded special-device number.
    pub rdev: u64,
    /// File length in bytes.
    pub size: u64,
    /// Preferred I/O block size.
    pub blksize: u64,
    /// Allocated 512-byte block count.
    pub blocks: u64,
    /// Last-access time in Unix seconds.
    pub atime_sec: u64,
    /// Nanosecond part of the last-access time.
    pub atime_nsec: u32,
    /// Last-modification time in Unix seconds.
    pub mtime_sec: u64,
    /// Nanosecond part of the last-modification time.
    pub mtime_nsec: u32,
    /// Last-status-change time in Unix seconds.
    pub ctime_sec: u64,
    /// Nanosecond part of the last-status-change time.
    pub ctime_nsec: u32,
    /// Server inode generation.
    pub generation: u64,
}

/// Attribute changes carried by 9P2000.L `TSETATTR`.
///
/// This is the object-model counterpart of Linux `struct p9_iattr_dotl`;
/// `None` means that the corresponding protocol valid bit is not set.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct P9SetAttr {
    /// New permission mode.
    pub mode: Option<u32>,
    /// New owner user ID.
    pub uid: Option<u32>,
    /// New owner group ID.
    pub gid: Option<u32>,
    /// New file length.
    pub size: Option<u64>,
    /// Access-time update.
    pub atime: Option<P9SetAttrTime>,
    /// Modification-time update.
    pub mtime: Option<P9SetAttrTime>,
    /// Ask the server to update the status-change time.
    pub should_update_ctime: bool,
}

/// Timestamp assignment semantics for 9P2000.L `TSETATTR`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum P9SetAttrTime {
    /// Ask the server to use its current time.
    Current,
    /// Assign `(Unix seconds, nanoseconds)` exactly.
    Explicit(u64, u32),
}

/// One entry returned by 9P2000.L `TREADDIR`.
#[derive(Clone, Debug)]
pub struct P9DirEntry {
    /// Server identity of the entry.
    pub qid: Qid,
    /// Server offset at which iteration should resume.
    pub offset: u64,
    /// Entry name.
    pub name: String,
    /// Linux `d_type` value.
    pub entry_type: u8,
}

/// Filesystem statistics returned by 9P2000.L `TSTATFS`.
///
/// This has the same protocol-owned fields as Linux `struct p9_rstatfs`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct P9StatFs {
    /// Filesystem type identifier.
    pub fs_type: u32,
    /// Fundamental block size in bytes.
    pub block_size: u32,
    /// Total data blocks.
    pub blocks: u64,
    /// Free data blocks.
    pub blocks_free: u64,
    /// Data blocks available to unprivileged users.
    pub blocks_available: u64,
    /// Total file count.
    pub file_count: u64,
    /// Free file count.
    pub free_file_count: u64,
    /// Server filesystem identifier.
    pub fs_id: u64,
    /// Maximum filename length.
    pub name_length: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum P9Version {
    Unknown,
    P2000L,
}

struct P9ClientState {
    msize: u32,
    next_tag: u16,
    next_fid: u32,
    live_fids: BTreeSet<u32>,
    version: P9Version,
    transport: Box<dyn P9Transport>,
}

/// A connection to one 9P server.
///
/// Protocol state and transport access are serialized internally, matching
/// Linux's client-owned tag, fid, and request state. Filesystem code shares
/// this object through [`P9Fid`] rather than adding an outer session lock.
pub struct P9Client {
    state: Mutex<P9ClientState>,
}

/// A reference-counted 9P file identifier.
///
/// This is the object-oriented counterpart of Linux `struct p9_fid`. The
/// object keeps its client alive, records qid and open mode, and sends
/// `TCLUNK` when the final reference is dropped.
///
/// Dropping a live fid performs synchronous transport I/O. Fids must therefore
/// be released in sleepable task context, never in interrupt context or while
/// holding a spinlock.
pub struct P9Fid {
    client: Arc<P9Client>,
    fid: u32,
    qid: Mutex<Qid>,
    mode: AtomicU32,
    iounit: AtomicU32,
}

impl P9Client {
    /// Creates a client that owns one connected transport endpoint.
    ///
    /// Protocol negotiation is intentionally separate so the caller can
    /// construct the filesystem only after [`Self::negotiate`] succeeds.
    pub fn new(transport: Box<dyn P9Transport>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(P9ClientState {
                msize: DEFAULT_MSIZE,
                next_tag: 1,
                next_fid: 1,
                live_fids: BTreeSet::new(),
                version: P9Version::Unknown,
                transport,
            }),
        })
    }

    /// Negotiates 9P2000.L and attaches a fid to the server root.
    ///
    /// # Errors
    ///
    /// Returns an error if the endpoint rejects 9P2000.L, the attach fails, or
    /// either response is malformed.
    pub fn negotiate(self: &Arc<Self>) -> KResult<Arc<P9Fid>> {
        let (fid, qid) = {
            let mut state = self.state.lock();
            if state.version != P9Version::Unknown {
                return Err(KError::BadState);
            }
            let response = state.send_tversion("9P2000.L")?;
            if !response.starts_with("9P2000.L") {
                warn!("unsupported 9P version: {response}");
                return Err(KError::OperationNotSupported);
            }
            state.version = P9Version::P2000L;
            let fid = state.alloc_fid()?;
            let qid = match state.send_tattach(fid) {
                Ok(qid) => qid,
                Err(error) => {
                    state.release_fid(fid);
                    return Err(error);
                }
            };
            (fid, qid)
        };
        Ok(P9Fid::new(self.clone(), fid, qid))
    }

    /// Returns the largest non-negative timestamp second supported by the
    /// negotiated dialect.
    pub fn timestamp_max_seconds(&self) -> i64 {
        match self.state.lock().version {
            P9Version::Unknown | P9Version::P2000L => i64::MAX,
        }
    }

    /// Returns the largest payload supported by one 9P data request.
    pub fn max_data_size(&self) -> u32 {
        self.state.lock().msize.saturating_sub(P9_IOHDRSZ)
    }
}

impl P9Fid {
    fn new(client: Arc<P9Client>, fid: u32, qid: Qid) -> Arc<Self> {
        Arc::new(Self {
            client,
            fid,
            qid: Mutex::new(qid),
            mode: AtomicU32::new(FID_MODE_UNOPENED),
            iounit: AtomicU32::new(0),
        })
    }

    /// Returns the qid currently associated with this fid.
    pub fn qid(&self) -> Qid {
        *self.qid.lock()
    }

    /// Clones this fid at the server and walks zero or more child names.
    ///
    /// An empty component slice creates an independent fid for the same file.
    /// Each non-empty component must be one protocol name, not a path, and a
    /// request may contain at most Linux's `P9_MAXWELEM` components.
    ///
    /// # Errors
    ///
    /// Returns an error for an opened or removed fid, an invalid component,
    /// a partial walk, or a protocol/transport failure.
    pub fn walk(self: &Arc<Self>, names: &[&str]) -> KResult<Arc<Self>> {
        validate_names(names)?;

        let (new_fid, qid) = {
            let mut state = self.client.state.lock();
            self.ensure_unopened()?;
            let new_fid = state.alloc_fid()?;
            let qid = state.walk(self.fid, new_fid, names, self.qid())?;
            (new_fid, qid)
        };
        Ok(Self::new(self.client.clone(), new_fid, qid))
    }

    /// Opens this fid with the requested access mode.
    ///
    /// # Errors
    ///
    /// Returns an error if the fid is not live, has already been opened, or
    /// the server rejects the request.
    pub fn open(&self, mode: P9OpenMode, flags: P9OpenFlags) -> KResult<()> {
        let mut state = self.client.state.lock();
        // The client lock serializes the check, RPC, and publication, as one
        // transition; a separate opening state would duplicate that exclusion.
        self.ensure_unopened()?;
        let (qid, iounit) = state.lopen(self.fid, mode, flags)?;
        *self.qid.lock() = qid;
        self.iounit.store(iounit, Ordering::Release);
        self.mode
            .store(mode.flags() | flags.bits(), Ordering::Release);
        Ok(())
    }

    /// Creates a child directory and returns its qid.
    ///
    /// # Errors
    ///
    /// Returns an error if this is not a live unopened directory fid, `name`
    /// is not one protocol component, or the request fails.
    pub fn mkdir(&self, name: &str, mode: u32, gid: u32) -> KResult<Qid> {
        validate_name(name)?;
        let mut state = self.client.state.lock();
        self.ensure_unopened()?;
        state.mkdir(self.fid, name, mode, gid)
    }

    /// Creates a child node and returns its qid.
    ///
    /// # Errors
    ///
    /// Returns an error if this is not a live unopened directory fid, `name`
    /// is invalid, or the request fails.
    pub fn mknod(&self, name: &str, mode: u32, major: u32, minor: u32, gid: u32) -> KResult<Qid> {
        validate_name(name)?;
        let mut state = self.client.state.lock();
        self.ensure_unopened()?;
        state.mknod(self.fid, name, mode, major, minor, gid)
    }

    /// Creates a symbolic link child and returns its qid.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid name or target, an unusable directory
    /// fid, or a protocol/transport failure.
    pub fn symlink(&self, name: &str, target: &str, gid: u32) -> KResult<Qid> {
        validate_name(name)?;
        validate_string(target)?;
        let mut state = self.client.state.lock();
        self.ensure_unopened()?;
        state.symlink(self.fid, name, target, gid)
    }

    /// Creates `name` in this directory as a hard link to `target`.
    ///
    /// # Errors
    ///
    /// Returns an error if either fid is unusable, the fids belong to different
    /// clients, `name` is invalid, or the request fails.
    pub fn link(&self, target: &P9Fid, name: &str) -> KResult<()> {
        validate_name(name)?;
        if !Arc::ptr_eq(&self.client, &target.client) {
            return Err(KError::CrossesDevices);
        }
        let mut state = self.client.state.lock();
        self.ensure_unopened()?;
        target.ensure_live()?;
        state.link(self.fid, target.fid, name)
    }

    /// Removes the file referenced by this fid and consumes its server handle.
    ///
    /// # Errors
    ///
    /// Returns an error if the fid was already consumed or the request fails.
    /// The fid is considered consumed after a remove request even on error.
    pub fn remove(&self) -> KResult<()> {
        let mut state = self.client.state.lock();
        self.ensure_live()?;
        // Linux treats a remove fid as invalid even when TREMOVE fails.
        self.mode.store(FID_MODE_INVALID, Ordering::Release);
        state.remove(self.fid)
    }

    /// Renames this file into `new_dir` under `new_name`.
    ///
    /// # Errors
    ///
    /// Returns an error if a fid is unusable, the fids belong to different
    /// clients, the name is invalid, or the request fails.
    pub fn rename(&self, new_dir: &P9Fid, new_name: &str) -> KResult<()> {
        validate_name(new_name)?;
        if !Arc::ptr_eq(&self.client, &new_dir.client) {
            return Err(KError::CrossesDevices);
        }
        let mut state = self.client.state.lock();
        self.ensure_live()?;
        new_dir.ensure_unopened()?;
        state.rename(self.fid, new_dir.fid, new_name)
    }

    /// Renames one child between directory fids using 9P2000.L `TRENAMEAT`.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid names, unusable directory fids, fids from
    /// different clients, or a server/transport failure.
    pub fn rename_at(&self, old_name: &str, new_dir: &P9Fid, new_name: &str) -> KResult<()> {
        validate_name(old_name)?;
        validate_name(new_name)?;
        if !Arc::ptr_eq(&self.client, &new_dir.client) {
            return Err(KError::CrossesDevices);
        }
        let mut state = self.client.state.lock();
        self.ensure_unopened()?;
        new_dir.ensure_unopened()?;
        state.rename_at(self.fid, old_name, new_dir.fid, new_name)
    }

    /// Unlinks one child from this directory using 9P2000.L `TUNLINKAT`.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid name, an unusable directory fid, or a
    /// server/transport failure.
    pub fn unlink_at(&self, name: &str, is_directory: bool) -> KResult<()> {
        validate_name(name)?;
        let mut state = self.client.state.lock();
        self.ensure_unopened()?;
        state.unlink_at(self.fid, name, is_directory)
    }

    /// Reads current file attributes.
    ///
    /// # Errors
    ///
    /// Returns an error if the fid is consumed or the response is missing the
    /// requested basic attributes or is otherwise malformed.
    pub fn getattr(&self) -> KResult<FileAttr> {
        let mut state = self.client.state.lock();
        self.ensure_live()?;
        state.getattr(self.fid)
    }

    /// Changes the supplied attributes in one request.
    ///
    /// # Errors
    ///
    /// Returns an error if the fid is consumed or the request fails.
    pub fn setattr(&self, attributes: P9SetAttr) -> KResult<()> {
        validate_setattr(&attributes)?;
        let mut state = self.client.state.lock();
        self.ensure_live()?;
        if attributes == P9SetAttr::default() {
            return Ok(());
        }
        state.setattr(self.fid, attributes)
    }

    /// Reads a symbolic-link target.
    ///
    /// # Errors
    ///
    /// Returns an error if the fid is consumed or the request fails.
    pub fn read_link(&self) -> KResult<String> {
        let mut state = self.client.state.lock();
        self.ensure_live()?;
        state.read_link(self.fid)
    }

    /// Reads at most `count` bytes from an opened fid at `offset`.
    ///
    /// # Errors
    ///
    /// Returns an error if the fid is not open or the response is malformed.
    pub fn read(&self, offset: u64, count: u32) -> KResult<Vec<u8>> {
        let mut state = self.client.state.lock();
        self.ensure_opened()?;
        let count = count.min(self.max_io_count(&state));
        if count == 0 {
            return Ok(Vec::new());
        }
        state.read(self.fid, offset, count)
    }

    /// Writes bytes to an opened fid at `offset`.
    ///
    /// A request larger than the negotiated message size is shortened and the
    /// returned byte count reports the accepted prefix.
    ///
    /// # Errors
    ///
    /// Returns an error if the fid is not open, the request fails, or the
    /// server reports writing more data than was sent.
    pub fn write(&self, offset: u64, data: &[u8]) -> KResult<usize> {
        let mut state = self.client.state.lock();
        self.ensure_opened()?;
        let max_count = self.max_io_count(&state) as usize;
        if data.is_empty() {
            return Ok(0);
        }
        state.write(self.fid, offset, &data[..data.len().min(max_count)])
    }

    /// Reads one server-sized batch of directory entries from an opened fid.
    ///
    /// # Errors
    ///
    /// Returns an error if the fid is not open or any returned entry is
    /// malformed.
    pub fn read_dir(&self, offset: u64) -> KResult<Vec<P9DirEntry>> {
        let mut state = self.client.state.lock();
        self.ensure_opened()?;
        let count = self.max_io_count(&state);
        state.read_dir(self.fid, offset, count)
    }

    /// Flushes file data and, unless `data_only` is set, metadata associated
    /// with this opened fid.
    ///
    /// # Errors
    ///
    /// Returns an error if the fid is not open or the request fails.
    pub fn fsync(&self, data_only: bool) -> KResult<()> {
        let mut state = self.client.state.lock();
        self.ensure_opened()?;
        state.fsync(self.fid, data_only)
    }

    /// Reads filesystem-wide statistics for the filesystem containing this fid.
    ///
    /// # Errors
    ///
    /// Returns the server errno or a protocol/transport error.
    pub fn statfs(&self) -> KResult<P9StatFs> {
        let mut state = self.client.state.lock();
        self.ensure_live()?;
        state.statfs(self.fid)
    }

    fn max_io_count(&self, state: &P9ClientState) -> u32 {
        let max_data = state.msize.saturating_sub(P9_IOHDRSZ);
        match self.iounit.load(Ordering::Acquire) {
            0 => max_data,
            iounit => iounit.min(max_data),
        }
    }

    fn ensure_live(&self) -> KResult<()> {
        if self.mode.load(Ordering::Acquire) == FID_MODE_INVALID {
            Err(KError::BadState)
        } else {
            Ok(())
        }
    }

    fn ensure_unopened(&self) -> KResult<()> {
        self.ensure_live()?;
        if self.mode.load(Ordering::Acquire) == FID_MODE_UNOPENED {
            Ok(())
        } else {
            Err(KError::BadState)
        }
    }

    fn ensure_opened(&self) -> KResult<()> {
        self.ensure_live()?;
        if self.mode.load(Ordering::Acquire) < FID_MODE_INVALID {
            Ok(())
        } else {
            Err(KError::BadState)
        }
    }
}

impl Drop for P9Fid {
    fn drop(&mut self) {
        if self.mode.swap(FID_MODE_INVALID, Ordering::AcqRel) != FID_MODE_INVALID
            && let Err(error) = self.client.state.lock().clunk(self.fid)
        {
            warn!("failed to clunk 9P fid {}: {}", self.fid, error);
        }
    }
}

impl P9ClientState {
    fn send_tversion(&mut self, version: &str) -> KResult<String> {
        let mut message = Message::new(TVERSION, NO_TAG);
        message.push_u32(self.msize);
        message.push_str(version);
        let response = self.send_recv(message.finish(), RVERSION, NO_TAG)?;

        let mut offset = 0;
        let msize = read_u32(&response, &mut offset)?;
        let version = match read_str(&response, &mut offset) {
            Ok(value) => value,
            Err(error) => {
                warn!(
                    "RVERSION parse error: {} (msize={}, remaining={} bytes)",
                    error,
                    msize,
                    response.len().saturating_sub(offset)
                );
                return Err(error);
            }
        };
        if msize < 4096 {
            warn!("server returned 9P msize below 4096: {msize}");
            return Err(KError::InvalidData);
        }
        self.msize = self.msize.min(msize);
        Ok(version)
    }

    fn send_tattach(&mut self, fid: u32) -> KResult<Qid> {
        let tag = self.alloc_tag();
        let mut message = Message::new(TATTACH, tag);
        message.push_u32(fid);
        message.push_u32(NO_FID);
        message.push_str("nobody");
        message.push_str("");
        message.push_u32(u32::MAX);
        let response = self.send_recv(message.finish(), RATTACH, tag)?;
        let mut offset = 0;
        read_qid(&response, &mut offset)
    }

    fn walk(&mut self, fid: u32, new_fid: u32, names: &[&str], source_qid: Qid) -> KResult<Qid> {
        let tag = self.alloc_tag();
        let mut message = Message::new(TWALK, tag);
        message.push_u32(fid);
        message.push_u32(new_fid);
        message.push_u16(names.len() as u16);
        for name in names {
            message.push_str(name);
        }
        let response = match self.send_recv(message.finish(), RWALK, tag) {
            Ok(response) => response,
            Err(error) => {
                self.release_fid(new_fid);
                return Err(error);
            }
        };

        let mut offset = 0;
        let count = match read_u16(&response, &mut offset) {
            Ok(count) => count as usize,
            Err(error) => {
                let _ = self.clunk(new_fid);
                return Err(error);
            }
        };
        let mut qid = source_qid;
        for _ in 0..count {
            match read_qid(&response, &mut offset) {
                Ok(walked_qid) => qid = walked_qid,
                Err(error) => {
                    if count != 0 {
                        let _ = self.clunk(new_fid);
                    } else {
                        self.release_fid(new_fid);
                    }
                    return Err(error);
                }
            }
        }
        if count != names.len() {
            if count != 0 {
                let _ = self.clunk(new_fid);
            } else {
                self.release_fid(new_fid);
            }
            return Err(KError::NotFound);
        }
        Ok(qid)
    }

    fn lopen(&mut self, fid: u32, mode: P9OpenMode, flags: P9OpenFlags) -> KResult<(Qid, u32)> {
        let tag = self.alloc_tag();
        let mut message = Message::new(TLOPEN, tag);
        message.push_u32(fid);
        message.push_u32((mode.flags() | flags.bits()) & P9L_MODE_MASK);
        let response = self.send_recv(message.finish(), RLOPEN, tag)?;
        let mut offset = 0;
        let qid = read_qid(&response, &mut offset)?;
        let iounit = read_u32(&response, &mut offset)?;
        Ok((qid, iounit))
    }

    fn mkdir(&mut self, fid: u32, name: &str, mode: u32, gid: u32) -> KResult<Qid> {
        let tag = self.alloc_tag();
        let mut message = Message::new(TMKDIR, tag);
        message.push_u32(fid);
        message.push_str(name);
        message.push_u32(mode);
        message.push_u32(gid);
        let response = self.send_recv(message.finish(), RMKDIR, tag)?;
        let mut offset = 0;
        read_qid(&response, &mut offset)
    }

    fn mknod(
        &mut self,
        fid: u32,
        name: &str,
        mode: u32,
        major: u32,
        minor: u32,
        gid: u32,
    ) -> KResult<Qid> {
        let tag = self.alloc_tag();
        let mut message = Message::new(TMKNOD, tag);
        message.push_u32(fid);
        message.push_str(name);
        message.push_u32(mode);
        message.push_u32(major);
        message.push_u32(minor);
        message.push_u32(gid);
        let response = self.send_recv(message.finish(), RMKNOD, tag)?;
        let mut offset = 0;
        read_qid(&response, &mut offset)
    }

    fn symlink(&mut self, fid: u32, name: &str, target: &str, gid: u32) -> KResult<Qid> {
        let tag = self.alloc_tag();
        let mut message = Message::new(TSYMLINK, tag);
        message.push_u32(fid);
        message.push_str(name);
        message.push_str(target);
        message.push_u32(gid);
        let response = self.send_recv(message.finish(), RSYMLINK, tag)?;
        let mut offset = 0;
        read_qid(&response, &mut offset)
    }

    fn link(&mut self, directory_fid: u32, target_fid: u32, name: &str) -> KResult<()> {
        let tag = self.alloc_tag();
        let mut message = Message::new(TLINK, tag);
        // 9P2000.L encodes dfid before the fid of the existing object.
        message.push_u32(directory_fid);
        message.push_u32(target_fid);
        message.push_str(name);
        self.send_recv(message.finish(), RLINK, tag).map(|_| ())
    }

    fn remove(&mut self, fid: u32) -> KResult<()> {
        let tag = self.alloc_tag();
        let mut message = Message::new(TREMOVE, tag);
        message.push_u32(fid);
        let result = self.send_recv(message.finish(), RREMOVE, tag).map(|_| ());
        self.release_fid(fid);
        result
    }

    fn rename(&mut self, fid: u32, new_dir_fid: u32, new_name: &str) -> KResult<()> {
        let tag = self.alloc_tag();
        let mut message = Message::new(TRENAME, tag);
        message.push_u32(fid);
        message.push_u32(new_dir_fid);
        message.push_str(new_name);
        self.send_recv(message.finish(), RRENAME, tag).map(|_| ())
    }

    fn rename_at(
        &mut self,
        old_dir_fid: u32,
        old_name: &str,
        new_dir_fid: u32,
        new_name: &str,
    ) -> KResult<()> {
        let tag = self.alloc_tag();
        let mut message = Message::new(TRENAMEAT, tag);
        message.push_u32(old_dir_fid);
        message.push_str(old_name);
        message.push_u32(new_dir_fid);
        message.push_str(new_name);
        self.send_recv(message.finish(), RRENAMEAT, tag).map(|_| ())
    }

    fn unlink_at(&mut self, directory_fid: u32, name: &str, is_directory: bool) -> KResult<()> {
        let tag = self.alloc_tag();
        let mut message = Message::new(TUNLINKAT, tag);
        message.push_u32(directory_fid);
        message.push_str(name);
        message.push_u32(if is_directory {
            P9_DOTL_AT_REMOVEDIR
        } else {
            0
        });
        self.send_recv(message.finish(), RUNLINKAT, tag).map(|_| ())
    }

    fn getattr(&mut self, fid: u32) -> KResult<FileAttr> {
        let tag = self.alloc_tag();
        let mut message = Message::new(TGETATTR, tag);
        message.push_u32(fid);
        message.push_u64(P9_STATS_BASIC | P9_STATS_GEN);
        let response = self.send_recv(message.finish(), RGETATTR, tag)?;

        let mut offset = 0;
        let valid = read_u64(&response, &mut offset)?;
        if valid & P9_STATS_BASIC != P9_STATS_BASIC {
            return Err(KError::InvalidData);
        }
        let qid = read_qid(&response, &mut offset)?;
        let mode = read_u32(&response, &mut offset)?;
        let uid = read_u32(&response, &mut offset)?;
        let gid = read_u32(&response, &mut offset)?;
        let nlink = read_u64(&response, &mut offset)?;
        let rdev = read_u64(&response, &mut offset)?;
        let size = read_u64(&response, &mut offset)?;
        let blksize = read_u64(&response, &mut offset)?;
        let blocks = read_u64(&response, &mut offset)?;
        let atime_sec = read_u64(&response, &mut offset)?;
        let atime_nsec = read_timestamp_nanoseconds(&response, &mut offset)?;
        let mtime_sec = read_u64(&response, &mut offset)?;
        let mtime_nsec = read_timestamp_nanoseconds(&response, &mut offset)?;
        let ctime_sec = read_u64(&response, &mut offset)?;
        let ctime_nsec = read_timestamp_nanoseconds(&response, &mut offset)?;
        let _btime_sec = read_u64(&response, &mut offset)?;
        let _btime_nsec = read_u64(&response, &mut offset)?;
        let generation = read_u64(&response, &mut offset)?;
        let _data_version = read_u64(&response, &mut offset)?;
        Ok(FileAttr {
            qid,
            mode,
            uid,
            gid,
            nlink,
            rdev,
            size,
            blksize,
            blocks,
            atime_sec,
            atime_nsec,
            mtime_sec,
            mtime_nsec,
            ctime_sec,
            ctime_nsec,
            generation: if valid & P9_STATS_GEN != 0 {
                generation
            } else {
                0
            },
        })
    }

    fn setattr(&mut self, fid: u32, attributes: P9SetAttr) -> KResult<()> {
        let mut valid = 0;
        if attributes.mode.is_some() {
            valid |= P9_SETATTR_MODE;
        }
        if attributes.uid.is_some() {
            valid |= P9_SETATTR_UID;
        }
        if attributes.gid.is_some() {
            valid |= P9_SETATTR_GID;
        }
        if attributes.size.is_some() {
            valid |= P9_SETATTR_SIZE;
        }
        if let Some(atime) = attributes.atime {
            valid |= P9_SETATTR_ATIME;
            if matches!(atime, P9SetAttrTime::Explicit(..)) {
                valid |= P9_SETATTR_ATIME_SET;
            }
        }
        if let Some(mtime) = attributes.mtime {
            valid |= P9_SETATTR_MTIME;
            if matches!(mtime, P9SetAttrTime::Explicit(..)) {
                valid |= P9_SETATTR_MTIME_SET;
            }
        }
        if attributes.should_update_ctime {
            valid |= P9_SETATTR_CTIME;
        }
        let (atime_sec, atime_nsec) = match attributes.atime {
            Some(P9SetAttrTime::Explicit(seconds, nanoseconds)) => (seconds, nanoseconds),
            None | Some(P9SetAttrTime::Current) => (0, 0),
        };
        let (mtime_sec, mtime_nsec) = match attributes.mtime {
            Some(P9SetAttrTime::Explicit(seconds, nanoseconds)) => (seconds, nanoseconds),
            None | Some(P9SetAttrTime::Current) => (0, 0),
        };
        let tag = self.alloc_tag();
        let mut message = Message::new(TSETATTR, tag);
        message.push_u32(fid);
        message.push_u32(valid);
        message.push_u32(attributes.mode.unwrap_or(0));
        message.push_u32(attributes.uid.unwrap_or(0));
        message.push_u32(attributes.gid.unwrap_or(0));
        message.push_u64(attributes.size.unwrap_or(0));
        message.push_u64(atime_sec);
        message.push_u64(u64::from(atime_nsec));
        message.push_u64(mtime_sec);
        message.push_u64(u64::from(mtime_nsec));
        self.send_recv(message.finish(), RSETATTR, tag).map(|_| ())
    }

    fn read_link(&mut self, fid: u32) -> KResult<String> {
        let tag = self.alloc_tag();
        let mut message = Message::new(TREADLINK, tag);
        message.push_u32(fid);
        let response = self.send_recv(message.finish(), RREADLINK, tag)?;
        let mut offset = 0;
        let target = read_str(&response, &mut offset)?;
        if target.contains('\0') {
            return Err(KError::InvalidData);
        }
        Ok(target)
    }

    fn read(&mut self, fid: u32, file_offset: u64, count: u32) -> KResult<Vec<u8>> {
        let tag = self.alloc_tag();
        let mut message = Message::new(TREAD, tag);
        message.push_u32(fid);
        message.push_u64(file_offset);
        message.push_u32(count);
        let response = self.send_recv(message.finish(), RREAD, tag)?;

        let mut offset = 0;
        let data_len = read_u32(&response, &mut offset)? as usize;
        if data_len > count as usize {
            return Err(KError::InvalidData);
        }
        let end = offset
            .checked_add(data_len)
            .filter(|end| *end <= response.len())
            .ok_or(KError::InvalidData)?;
        Ok(response[offset..end].to_vec())
    }

    fn write(&mut self, fid: u32, file_offset: u64, data: &[u8]) -> KResult<usize> {
        let count = u32::try_from(data.len()).map_err(|_| KError::InvalidInput)?;
        let tag = self.alloc_tag();
        let mut message = Message::new(TWRITE, tag);
        message.push_u32(fid);
        message.push_u64(file_offset);
        message.push_u32(count);
        message.push_bytes(data);
        let response = self.send_recv(message.finish(), RWRITE, tag)?;

        let mut offset = 0;
        let written = read_u32(&response, &mut offset)?;
        if written > count {
            return Err(KError::InvalidData);
        }
        Ok(written as usize)
    }

    fn read_dir(
        &mut self,
        fid: u32,
        directory_offset: u64,
        count: u32,
    ) -> KResult<Vec<P9DirEntry>> {
        let tag = self.alloc_tag();
        let mut message = Message::new(TREADDIR, tag);
        message.push_u32(fid);
        message.push_u64(directory_offset);
        message.push_u32(count);
        let response = self.send_recv(message.finish(), RREADDIR, tag)?;

        let mut offset = 0;
        let data_len = read_u32(&response, &mut offset)? as usize;
        if data_len > count as usize {
            return Err(KError::InvalidData);
        }
        let end = offset
            .checked_add(data_len)
            .filter(|end| *end <= response.len())
            .ok_or(KError::InvalidData)?;
        let data = &response[offset..end];

        let mut entries = Vec::new();
        let mut offset = 0;
        while offset < data.len() {
            let qid = read_qid(data, &mut offset)?;
            let entry_offset = read_u64(data, &mut offset)?;
            let entry_type = read_u8(data, &mut offset)?;
            let name = read_str(data, &mut offset)?;
            validate_dir_entry_name(&name)?;
            entries.push(P9DirEntry {
                qid,
                offset: entry_offset,
                entry_type,
                name,
            });
        }
        Ok(entries)
    }

    fn fsync(&mut self, fid: u32, data_only: bool) -> KResult<()> {
        let tag = self.alloc_tag();
        let mut message = Message::new(TFSYNC, tag);
        message.push_u32(fid);
        message.push_u32(u32::from(data_only));
        self.send_recv(message.finish(), RFSYNC, tag).map(|_| ())
    }

    fn statfs(&mut self, fid: u32) -> KResult<P9StatFs> {
        let tag = self.alloc_tag();
        let mut message = Message::new(TSTATFS, tag);
        message.push_u32(fid);
        let response = self.send_recv(message.finish(), RSTATFS, tag)?;
        let mut offset = 0;
        Ok(P9StatFs {
            fs_type: read_u32(&response, &mut offset)?,
            block_size: read_u32(&response, &mut offset)?,
            blocks: read_u64(&response, &mut offset)?,
            blocks_free: read_u64(&response, &mut offset)?,
            blocks_available: read_u64(&response, &mut offset)?,
            file_count: read_u64(&response, &mut offset)?,
            free_file_count: read_u64(&response, &mut offset)?,
            fs_id: read_u64(&response, &mut offset)?,
            name_length: read_u32(&response, &mut offset)?,
        })
    }

    fn clunk(&mut self, fid: u32) -> KResult<()> {
        let tag = self.alloc_tag();
        let mut message = Message::new(TCLUNK, tag);
        message.push_u32(fid);
        let result = self.send_recv(message.finish(), RCLUNK, tag).map(|_| ());
        self.release_fid(fid);
        result
    }

    fn send_recv(&mut self, request: Vec<u8>, expected_type: u8, tag: u16) -> KResult<Vec<u8>> {
        if request.len() > self.msize as usize {
            return Err(KError::InvalidInput);
        }
        let mut response = vec![0u8; self.msize as usize];
        let size = self.transport.request(&request, &mut response)?;
        if size > response.len() {
            return Err(KError::InvalidData);
        }
        if size < 7 {
            return Err(KError::InvalidData);
        }
        let response = &response[..size];
        let declared_size =
            u32::from_le_bytes([response[0], response[1], response[2], response[3]]) as usize;
        if declared_size != size {
            return Err(KError::InvalidData);
        }
        let response_type = response[4];
        let response_tag = u16::from_le_bytes([response[5], response[6]]);
        if response_tag != tag {
            return Err(KError::InvalidData);
        }
        if response_type == RERROR {
            let mut offset = 7;
            let message = read_str(response, &mut offset)?;
            warn!("legacy 9P RERROR response: {message}");
            return Err(KError::Io);
        }
        if response_type == RLERROR {
            let mut offset = 7;
            let errno = read_u32(response, &mut offset)?;
            let errno = i32::try_from(errno).map_err(|_| KError::InvalidData)?;
            if errno == 0 {
                return Err(KError::InvalidData);
            }
            return Err(KError::from(LinuxError::new(errno)));
        }
        if response_type != expected_type {
            warn!("unexpected 9P response type: {response_type}");
            return Err(KError::InvalidData);
        }
        Ok(response[7..].to_vec())
    }

    fn alloc_tag(&mut self) -> u16 {
        loop {
            let tag = self.next_tag;
            self.next_tag = self.next_tag.wrapping_add(1);
            if tag != NO_TAG {
                return tag;
            }
        }
    }

    fn alloc_fid(&mut self) -> KResult<u32> {
        let start = self.next_fid;
        loop {
            let fid = self.next_fid;
            self.next_fid = self.next_fid.wrapping_add(1);
            if fid != NO_FID && self.live_fids.insert(fid) {
                return Ok(fid);
            }
            if self.next_fid == start {
                return Err(KError::StorageFull);
            }
        }
    }

    fn release_fid(&mut self, fid: u32) {
        let was_live = self.live_fids.remove(&fid);
        debug_assert!(was_live, "released 9P fid must be allocated");
    }
}

fn validate_names(names: &[&str]) -> KResult<()> {
    if names.len() > P9_MAXWELEM {
        return Err(KError::InvalidInput);
    }
    for name in names {
        validate_name(name)?;
    }
    Ok(())
}

fn validate_name(name: &str) -> KResult<()> {
    validate_string(name)?;
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err(KError::InvalidInput);
    }
    Ok(())
}

fn validate_dir_entry_name(name: &str) -> KResult<()> {
    if name.is_empty()
        || name.len() > P9_DIRENT_NAME_MAX
        || name.contains('/')
        || name.contains('\0')
    {
        return Err(KError::InvalidData);
    }
    Ok(())
}

fn validate_string(value: &str) -> KResult<()> {
    if value.len() > u16::MAX as usize {
        return Err(KError::NameTooLong);
    }
    if value.contains('\0') {
        return Err(KError::InvalidInput);
    }
    Ok(())
}

fn validate_setattr(attributes: &P9SetAttr) -> KResult<()> {
    if matches!(
        attributes.atime,
        Some(P9SetAttrTime::Explicit(_, nanoseconds)) if nanoseconds >= 1_000_000_000
    ) || matches!(
        attributes.mtime,
        Some(P9SetAttrTime::Explicit(_, nanoseconds)) if nanoseconds >= 1_000_000_000
    ) {
        return Err(KError::InvalidInput);
    }
    Ok(())
}

fn read_timestamp_nanoseconds(buf: &[u8], offset: &mut usize) -> KResult<u32> {
    let nanoseconds = read_u64(buf, offset)?;
    if nanoseconds >= 1_000_000_000 {
        return Err(KError::InvalidData);
    }
    Ok(nanoseconds as u32)
}

#[unittest::mod_test]
mod tests {
    use alloc::{boxed::Box, vec, vec::Vec};

    use kerrno::KResult;
    use unittest::{assert_eq, def_test};

    use super::*;

    struct FixedResponseTransport {
        response: Vec<u8>,
        reported_size: usize,
    }

    struct RecordingEmptyResponseTransport {
        request: Arc<Mutex<Vec<u8>>>,
        response_type: u8,
    }

    struct RecordingOpenTransport {
        request: Arc<Mutex<Vec<u8>>>,
    }

    struct RecordingStatFsTransport {
        request: Arc<Mutex<Vec<u8>>>,
    }

    struct NegotiationTransport {
        requests: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl P9Transport for FixedResponseTransport {
        fn request(&mut self, _request: &[u8], response: &mut [u8]) -> KResult<usize> {
            let copied = self.response.len().min(response.len());
            response[..copied].copy_from_slice(&self.response[..copied]);
            Ok(self.reported_size)
        }
    }

    impl P9Transport for RecordingEmptyResponseTransport {
        fn request(&mut self, request: &[u8], response: &mut [u8]) -> KResult<usize> {
            *self.request.lock() = request.to_vec();
            response[..4].copy_from_slice(&7u32.to_le_bytes());
            response[4] = self.response_type;
            response[5..7].copy_from_slice(&request[5..7]);
            Ok(7)
        }
    }

    impl P9Transport for RecordingOpenTransport {
        fn request(&mut self, request: &[u8], response: &mut [u8]) -> KResult<usize> {
            *self.request.lock() = request.to_vec();
            if matches!(request[4], TREAD | TWRITE | TREADDIR) {
                response[..4].copy_from_slice(&11u32.to_le_bytes());
                response[4] = request[4] + 1;
                response[5..7].copy_from_slice(&request[5..7]);
                response[7..11].copy_from_slice(&0u32.to_le_bytes());
                return Ok(11);
            }
            response[..4].copy_from_slice(&24u32.to_le_bytes());
            response[4] = RLOPEN;
            response[5..7].copy_from_slice(&request[5..7]);
            response[7] = 0;
            response[8..12].copy_from_slice(&0u32.to_le_bytes());
            response[12..20].copy_from_slice(&1u64.to_le_bytes());
            response[20..24].copy_from_slice(&8u32.to_le_bytes());
            Ok(24)
        }
    }

    impl P9Transport for RecordingStatFsTransport {
        fn request(&mut self, request: &[u8], response: &mut [u8]) -> KResult<usize> {
            *self.request.lock() = request.to_vec();
            let mut packet = Vec::new();
            packet.extend_from_slice(&67u32.to_le_bytes());
            packet.push(RSTATFS);
            packet.extend_from_slice(&request[5..7]);
            packet.extend_from_slice(&0x0102_1997u32.to_le_bytes());
            packet.extend_from_slice(&4096u32.to_le_bytes());
            for value in [1000u64, 400, 300, 200, 100, 42] {
                packet.extend_from_slice(&value.to_le_bytes());
            }
            packet.extend_from_slice(&255u32.to_le_bytes());
            response[..packet.len()].copy_from_slice(&packet);
            Ok(packet.len())
        }
    }

    impl P9Transport for NegotiationTransport {
        fn request(&mut self, request: &[u8], response: &mut [u8]) -> KResult<usize> {
            self.requests.lock().push(request.to_vec());
            let mut packet = Vec::new();
            match request[4] {
                TVERSION => {
                    packet.extend_from_slice(&21u32.to_le_bytes());
                    packet.push(RVERSION);
                    packet.extend_from_slice(&request[5..7]);
                    packet.extend_from_slice(&DEFAULT_MSIZE.to_le_bytes());
                    packet.extend_from_slice(&8u16.to_le_bytes());
                    packet.extend_from_slice(b"9P2000.L");
                }
                TATTACH => {
                    packet.extend_from_slice(&20u32.to_le_bytes());
                    packet.push(RATTACH);
                    packet.extend_from_slice(&request[5..7]);
                    packet.push(0x80);
                    packet.extend_from_slice(&0u32.to_le_bytes());
                    packet.extend_from_slice(&1u64.to_le_bytes());
                }
                TCLUNK => {
                    packet.extend_from_slice(&7u32.to_le_bytes());
                    packet.push(RCLUNK);
                    packet.extend_from_slice(&request[5..7]);
                }
                _ => return Err(KError::InvalidInput),
            }
            response[..packet.len()].copy_from_slice(&packet);
            Ok(packet.len())
        }
    }

    fn test_client(response: Vec<u8>, reported_size: usize) -> Arc<P9Client> {
        Arc::new(P9Client {
            state: Mutex::new(P9ClientState {
                msize: response.len().max(8) as u32,
                next_tag: 1,
                next_fid: 1,
                live_fids: BTreeSet::new(),
                version: P9Version::Unknown,
                transport: Box::new(FixedResponseTransport {
                    response,
                    reported_size,
                }),
            }),
        })
    }

    #[def_test]
    fn test_negotiate_uses_linux_default_attach_identity() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let client = P9Client::new(Box::new(NegotiationTransport {
            requests: requests.clone(),
        }));

        let root = client.negotiate().unwrap();

        let requests = requests.lock();
        let attach = &requests[1];
        assert_eq!(attach[4], TATTACH);
        assert_eq!(&attach[17..23], b"nobody");
        assert_eq!(u16::from_le_bytes(attach[23..25].try_into().unwrap()), 0);
        assert_eq!(
            u32::from_le_bytes(attach[25..29].try_into().unwrap()),
            u32::MAX
        );
        drop(requests);
        drop(root);
    }

    #[def_test]
    fn test_send_recv_rejects_transport_length_beyond_buffer() {
        let client = test_client(Vec::new(), 9);
        assert_eq!(
            client
                .state
                .lock()
                .send_recv(Vec::new(), RVERSION, 1)
                .unwrap_err(),
            KError::InvalidData
        );
    }

    #[def_test]
    fn test_fid_allocator_never_wraps_into_a_live_identifier() {
        let client = test_client(Vec::new(), 0);
        let mut state = client.state.lock();
        state.live_fids.insert(NO_FID - 1);
        state.live_fids.insert(0);
        state.next_fid = NO_FID - 1;
        assert_eq!(state.alloc_fid(), Ok(1));
        state.release_fid(0);
        state.next_fid = 0;
        assert_eq!(state.alloc_fid(), Ok(0));
    }

    #[def_test]
    fn test_final_fid_reference_clunks_and_releases_identifier() {
        let request = Arc::new(Mutex::new(Vec::new()));
        let mut live_fids = BTreeSet::new();
        live_fids.insert(41);
        let client = Arc::new(P9Client {
            state: Mutex::new(P9ClientState {
                msize: 64,
                next_tag: 1,
                next_fid: 42,
                live_fids,
                version: P9Version::P2000L,
                transport: Box::new(RecordingEmptyResponseTransport {
                    request: request.clone(),
                    response_type: RCLUNK,
                }),
            }),
        });
        let fid = P9Fid::new(
            client.clone(),
            41,
            Qid {
                type_: 0,
                version: 0,
                path: 1,
            },
        );

        drop(fid);

        let request = request.lock();
        assert_eq!(request[4], TCLUNK);
        assert_eq!(u32::from_le_bytes(request[7..11].try_into().unwrap()), 41);
        drop(request);
        assert_eq!(client.state.lock().live_fids.contains(&41), false);
    }

    #[def_test]
    fn test_send_recv_rejects_header_length_mismatch() {
        let mut response = vec![0u8; 7];
        response[..4].copy_from_slice(&8u32.to_le_bytes());
        response[4] = RVERSION;
        response[5..7].copy_from_slice(&1u16.to_le_bytes());
        let client = test_client(response, 7);
        assert_eq!(
            client
                .state
                .lock()
                .send_recv(Vec::new(), RVERSION, 1)
                .unwrap_err(),
            KError::InvalidData
        );
    }

    #[def_test]
    fn test_readlink_rejects_embedded_nul_from_server() {
        let mut response = vec![0u8; 12];
        response[..4].copy_from_slice(&12u32.to_le_bytes());
        response[4] = RREADLINK;
        response[5..7].copy_from_slice(&1u16.to_le_bytes());
        response[7..9].copy_from_slice(&3u16.to_le_bytes());
        response[9..12].copy_from_slice(b"a\0b");
        let client = test_client(response, 12);

        assert_eq!(client.state.lock().read_link(41), Err(KError::InvalidData));
    }

    #[def_test]
    fn test_rlerror_preserves_server_errno() {
        let mut response = vec![0u8; 11];
        response[..4].copy_from_slice(&11u32.to_le_bytes());
        response[4] = RLERROR;
        response[5..7].copy_from_slice(&1u16.to_le_bytes());
        response[7..11].copy_from_slice(&(LinuxError::ENOSYS.into_raw() as u32).to_le_bytes());
        let client = test_client(response, 11);
        assert_eq!(
            client
                .state
                .lock()
                .send_recv(Vec::new(), RVERSION, 1)
                .unwrap_err()
                .canonicalize(),
            KError::Unsupported
        );
    }

    #[def_test]
    fn test_dir_entry_name_matches_linux_limit() {
        assert_eq!(validate_dir_entry_name(&"a".repeat(255)), Ok(()));
        assert_eq!(
            validate_dir_entry_name(&"a".repeat(256)),
            Err(KError::InvalidData)
        );
        assert_eq!(
            validate_dir_entry_name("invalid\0name"),
            Err(KError::InvalidData)
        );
    }

    #[def_test]
    fn test_walk_rejects_more_than_linux_maxwelem() {
        let names = ["name"; P9_MAXWELEM + 1];
        assert_eq!(validate_names(&names), Err(KError::InvalidInput));
    }

    #[def_test]
    fn test_link_encodes_directory_fid_before_target_fid() {
        let request = Arc::new(Mutex::new(Vec::new()));
        let client = Arc::new(P9Client {
            state: Mutex::new(P9ClientState {
                msize: 64,
                next_tag: 1,
                next_fid: 1,
                live_fids: BTreeSet::new(),
                version: P9Version::P2000L,
                transport: Box::new(RecordingEmptyResponseTransport {
                    request: request.clone(),
                    response_type: RLINK,
                }),
            }),
        });
        let qid = Qid {
            type_: 0,
            version: 0,
            path: 1,
        };
        let directory = P9Fid::new(client.clone(), 41, qid);
        let target = P9Fid::new(client, 72, qid);

        directory.link(&target, "linked").unwrap();

        let request = request.lock();
        assert_eq!(request[4], TLINK);
        assert_eq!(u32::from_le_bytes(request[7..11].try_into().unwrap()), 41);
        assert_eq!(u32::from_le_bytes(request[11..15].try_into().unwrap()), 72);
        directory.mode.store(FID_MODE_INVALID, Ordering::Release);
        target.mode.store(FID_MODE_INVALID, Ordering::Release);
    }

    #[def_test]
    fn test_open_encodes_linux_server_mode_mask() {
        let request = Arc::new(Mutex::new(Vec::new()));
        let client = Arc::new(P9Client {
            state: Mutex::new(P9ClientState {
                msize: 64,
                next_tag: 1,
                next_fid: 1,
                live_fids: BTreeSet::new(),
                version: P9Version::P2000L,
                transport: Box::new(RecordingOpenTransport {
                    request: request.clone(),
                }),
            }),
        });
        let fid = P9Fid::new(
            client,
            41,
            Qid {
                type_: 0,
                version: 0,
                path: 1,
            },
        );

        fid.open(
            P9OpenMode::ReadWrite,
            P9OpenFlags::CREATE
                | P9OpenFlags::EXCLUSIVE
                | P9OpenFlags::TRUNCATE
                | P9OpenFlags::APPEND
                | P9OpenFlags::SYNC,
        )
        .unwrap();

        let request = request.lock();
        assert_eq!(request[4], TLOPEN);
        assert_eq!(u32::from_le_bytes(request[7..11].try_into().unwrap()), 41);
        assert_eq!(
            u32::from_le_bytes(request[11..15].try_into().unwrap()),
            P9_DOTL_RDWR
                | P9OpenFlags::CREATE.bits()
                | P9OpenFlags::EXCLUSIVE.bits()
                | P9OpenFlags::TRUNCATE.bits()
                | P9OpenFlags::APPEND.bits()
        );
        fid.mode.store(FID_MODE_INVALID, Ordering::Release);
    }

    #[def_test]
    fn test_open_failure_can_retry_and_io_obeys_server_iounit() {
        let client = test_client(Vec::new(), 0);
        let fid = P9Fid::new(
            client.clone(),
            41,
            Qid {
                type_: 0,
                version: 0,
                path: 1,
            },
        );
        assert_eq!(
            fid.open(P9OpenMode::ReadWrite, P9OpenFlags::empty())
                .is_err(),
            true
        );

        let request = Arc::new(Mutex::new(Vec::new()));
        {
            let mut state = client.state.lock();
            state.msize = 4096;
            state.transport = Box::new(RecordingOpenTransport {
                request: request.clone(),
            });
        }
        fid.open(P9OpenMode::ReadWrite, P9OpenFlags::empty())
            .unwrap();
        assert_eq!(
            fid.open(P9OpenMode::Read, P9OpenFlags::empty()),
            Err(KError::BadState)
        );
        fid.read(0, 32).unwrap();
        assert_eq!(
            u32::from_le_bytes(request.lock()[19..23].try_into().unwrap()),
            8
        );
        fid.write(0, &[0; 32]).unwrap();
        assert_eq!(
            u32::from_le_bytes(request.lock()[19..23].try_into().unwrap()),
            8
        );
        fid.read_dir(0).unwrap();
        assert_eq!(
            u32::from_le_bytes(request.lock()[19..23].try_into().unwrap()),
            8
        );
        fid.mode.store(FID_MODE_INVALID, Ordering::Release);
    }

    #[def_test]
    fn test_statfs_decodes_linux_dotl_fields() {
        let request = Arc::new(Mutex::new(Vec::new()));
        let client = Arc::new(P9Client {
            state: Mutex::new(P9ClientState {
                msize: 128,
                next_tag: 1,
                next_fid: 1,
                live_fids: BTreeSet::new(),
                version: P9Version::P2000L,
                transport: Box::new(RecordingStatFsTransport {
                    request: request.clone(),
                }),
            }),
        });
        let fid = P9Fid::new(
            client,
            41,
            Qid {
                type_: 0x80,
                version: 0,
                path: 1,
            },
        );

        let stats = fid.statfs().unwrap();
        assert_eq!(
            stats,
            P9StatFs {
                fs_type: 0x0102_1997,
                block_size: 4096,
                blocks: 1000,
                blocks_free: 400,
                blocks_available: 300,
                file_count: 200,
                free_file_count: 100,
                fs_id: 42,
                name_length: 255,
            }
        );
        let request = request.lock();
        assert_eq!(request[4], TSTATFS);
        assert_eq!(u32::from_le_bytes(request[7..11].try_into().unwrap()), 41);
        fid.mode.store(FID_MODE_INVALID, Ordering::Release);
    }

    #[def_test]
    fn test_setattr_encodes_valid_bits_and_timestamps() {
        let request = Arc::new(Mutex::new(Vec::new()));
        let client = Arc::new(P9Client {
            state: Mutex::new(P9ClientState {
                msize: 128,
                next_tag: 1,
                next_fid: 1,
                live_fids: BTreeSet::new(),
                version: P9Version::P2000L,
                transport: Box::new(RecordingEmptyResponseTransport {
                    request: request.clone(),
                    response_type: RSETATTR,
                }),
            }),
        });
        let fid = P9Fid::new(
            client,
            41,
            Qid {
                type_: 0,
                version: 0,
                path: 1,
            },
        );

        fid.setattr(P9SetAttr {
            mode: Some(0o640),
            uid: Some(1000),
            gid: Some(1001),
            size: Some(123),
            atime: Some(P9SetAttrTime::Explicit(10, 20)),
            mtime: Some(P9SetAttrTime::Current),
            should_update_ctime: true,
        })
        .unwrap();

        let request = request.lock();
        assert_eq!(request[4], TSETATTR);
        assert_eq!(u32::from_le_bytes(request[7..11].try_into().unwrap()), 41);
        assert_eq!(
            u32::from_le_bytes(request[11..15].try_into().unwrap()),
            0xff
        );
        assert_eq!(
            u32::from_le_bytes(request[15..19].try_into().unwrap()),
            0o640
        );
        assert_eq!(
            u32::from_le_bytes(request[19..23].try_into().unwrap()),
            1000
        );
        assert_eq!(
            u32::from_le_bytes(request[23..27].try_into().unwrap()),
            1001
        );
        assert_eq!(u64::from_le_bytes(request[27..35].try_into().unwrap()), 123);
        assert_eq!(u64::from_le_bytes(request[35..43].try_into().unwrap()), 10);
        assert_eq!(u64::from_le_bytes(request[43..51].try_into().unwrap()), 20);
        assert_eq!(u64::from_le_bytes(request[51..59].try_into().unwrap()), 0);
        assert_eq!(u64::from_le_bytes(request[59..67].try_into().unwrap()), 0);
        fid.mode.store(FID_MODE_INVALID, Ordering::Release);
    }

    #[def_test]
    fn test_renameat_encodes_directory_fids_and_names() {
        let request = Arc::new(Mutex::new(Vec::new()));
        let client = Arc::new(P9Client {
            state: Mutex::new(P9ClientState {
                msize: 128,
                next_tag: 1,
                next_fid: 1,
                live_fids: BTreeSet::new(),
                version: P9Version::P2000L,
                transport: Box::new(RecordingEmptyResponseTransport {
                    request: request.clone(),
                    response_type: RRENAMEAT,
                }),
            }),
        });
        let qid = Qid {
            type_: 0x80,
            version: 0,
            path: 1,
        };
        let old_dir = P9Fid::new(client.clone(), 41, qid);
        let new_dir = P9Fid::new(client, 72, qid);

        old_dir.rename_at("old", &new_dir, "new").unwrap();

        let request = request.lock();
        assert_eq!(request[4], TRENAMEAT);
        assert_eq!(u32::from_le_bytes(request[7..11].try_into().unwrap()), 41);
        assert_eq!(&request[13..16], b"old");
        assert_eq!(u32::from_le_bytes(request[16..20].try_into().unwrap()), 72);
        assert_eq!(&request[22..25], b"new");
        old_dir.mode.store(FID_MODE_INVALID, Ordering::Release);
        new_dir.mode.store(FID_MODE_INVALID, Ordering::Release);
    }

    #[def_test]
    fn test_unlinkat_encodes_remove_directory_flag() {
        let request = Arc::new(Mutex::new(Vec::new()));
        let client = Arc::new(P9Client {
            state: Mutex::new(P9ClientState {
                msize: 128,
                next_tag: 1,
                next_fid: 1,
                live_fids: BTreeSet::new(),
                version: P9Version::P2000L,
                transport: Box::new(RecordingEmptyResponseTransport {
                    request: request.clone(),
                    response_type: RUNLINKAT,
                }),
            }),
        });
        let directory = P9Fid::new(
            client,
            41,
            Qid {
                type_: 0x80,
                version: 0,
                path: 1,
            },
        );

        directory.unlink_at("victim", true).unwrap();

        let request = request.lock();
        assert_eq!(request[4], TUNLINKAT);
        assert_eq!(u32::from_le_bytes(request[7..11].try_into().unwrap()), 41);
        assert_eq!(&request[13..19], b"victim");
        assert_eq!(
            u32::from_le_bytes(request[19..23].try_into().unwrap()),
            P9_DOTL_AT_REMOVEDIR
        );
        directory.mode.store(FID_MODE_INVALID, Ordering::Release);
    }
}
