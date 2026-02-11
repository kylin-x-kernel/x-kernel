//! 9P session state and high-level operations.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use log::warn;

use crate::message::{dump_hex, read_qid, read_str, read_u8, read_u16, read_u32, read_u64, Message};
use crate::parse::{parse_dir_entries, parse_dir_entries_l, path_parts, split_parent_name};
use crate::protocol::*;
use crate::transport::Transport;

/// A single 9P connection/session.
pub struct P9Session {
    msize: u32,
    next_tag: u16,
    next_fid: u32,
    root_fid: u32,
    mount_tag: String,
    /// Negotiated 9P protocol version from TVERSION/RVERSION.
    p9_version: P9Version,
    transport: Box<dyn Transport>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum P9Version {
    Unknown,
    P2000,
    P2000U,
    P2000L,
}

impl P9Version {
    fn from_str(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "9p2000.l" => Some(P9Version::P2000L),
            "9p2000.u" => Some(P9Version::P2000U),
            "9p2000" => Some(P9Version::P2000),
            _ => None,
        }
    }

    /// Returns true if the negotiated protocol is 9P2000.L (Linux extensions).
    fn is_dotl(self) -> bool {
        matches!(self, P9Version::P2000L)
    }
}

/// File attributes returned by TGETATTR.
#[derive(Clone, Debug, Default)]
pub struct FileAttr {
    /// Qid type byte: 0x80 = directory, 0x02 = symlink, 0x00 = regular file.
    pub qid_type: u8,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub nlink: u64,
    pub size: u64,
    pub atime_sec: u64,
    pub mtime_sec: u64,
    pub ctime_sec: u64,
}

/// A directory entry with type information from structured readdir.
#[derive(Clone, Debug)]
pub struct P9DirEntry {
    pub name: String,
    /// d_type from dirent: 4=dir, 8=file, 10=symlink, 0=unknown.
    pub entry_type: u8,
}

impl P9Session {
    fn max_read_count(&self) -> u32 {
        // Leave headroom for 9P headers and directory entry parsing.
        self.msize.saturating_sub(64)
    }
    /// Create a new session with the given transport and mount tag.
    pub fn new(transport: Box<dyn Transport>, mount_tag: String) -> Self {
        Self {
            msize: DEFAULT_MSIZE,
            next_tag: 1,
            next_fid: 2,
            root_fid: 1,
            mount_tag,
            p9_version: P9Version::Unknown,
            transport,
        }
    }

    /// Negotiate protocol version and attach to the server root.
    pub fn negotiate(&mut self) -> Result<(), String> {
        let mut last_version = String::from("unknown");
        // QEMU uses case-sensitive strcmp for version matching and expects
        // uppercase "9P2000".  Try 9P2000.L first (Linux extension), then
        // 9P2000.u (Unix extension).
        for version in ["9P2000.L", "9P2000.u"] {
            let resp = self.send_tversion(version)?;
            if let Some(version) = P9Version::from_str(&resp) {
                self.p9_version = version;
                self.send_tattach()?;
                return Ok(());
            }
            warn!("RVERSION not accepted (req={}, resp={})", version, resp);
            last_version = resp;
        }
        Err(format!("unsupported 9p version: {}", last_version))
    }

    /// Returns the mount tag provided by the server device.
    pub fn mount_tag(&self) -> &str {
        &self.mount_tag
    }

    /// List directory entries at the provided path.
    pub fn list_dir(&mut self, path: &str) -> Result<Vec<String>, String> {
        let (fid, is_dir) = self.walk_path(path)?;
        if !is_dir {
            self.clunk(fid)?;
            return Err(String::from("not a directory"));
        }

        self.open_with_flags(fid, OREAD, P9_DOTL_RDONLY)?;

        let mut offset = 0u64;
        let mut names = Vec::new();
        loop {
            if self.p9_version.is_dotl() {
                let (chunk, next_offset) = self.readdir(fid, offset, self.msize - 64)?;
                if chunk.is_empty() {
                    break;
                }
                names.extend(chunk);
                match next_offset {
                    Some(next) if next > offset => offset = next,
                    _ => break,
                }
            } else {
                let data = self.read(fid, offset, self.msize - 64)?;
                if data.is_empty() {
                    break;
                }
                offset += data.len() as u64;
                parse_dir_entries(&data, &mut names)?;
            }
        }

        self.clunk(fid)?;
        Ok(names)
    }

    /// Ensure the path points to a directory.
    pub fn ensure_dir(&mut self, path: &str) -> Result<(), String> {
        let (fid, is_dir) = self.walk_path(path)?;
        self.clunk(fid)?;
        if is_dir {
            Ok(())
        } else {
            Err(String::from("not a directory"))
        }
    }

    /// Create a directory at `path`.
    pub fn create_dir(&mut self, path: &str) -> Result<(), String> {
        let (parent, name) = split_parent_name(path)?;
        let (fid, is_dir) = self.walk_path(parent)?;
        if !is_dir {
            self.clunk(fid)?;
            return Err(String::from("parent is not a directory"));
        }

        if self.p9_version.is_dotl() {
            self.mkdir(fid, name, DMDIR | 0o755, 0)?;
        } else {
            self.create(fid, name, OREAD, DMDIR | 0o755)?;
        }
        self.clunk(fid)?;
        Ok(())
    }

    pub fn open_path_with_flags(&mut self, path: &str, mode_9p: u8, mode_dotl: u32) -> Result<u32, String> {
        let (fid, _is_dir) = self.walk_path(path)?;
        match self.open_with_flags(fid, mode_9p, mode_dotl) {
            Ok(()) => Ok(fid),
            Err(err) => {
                let _ = self.clunk(fid);
                Err(err)
            }
        }
    }

    pub fn close_fid(&mut self, fid: u32) -> Result<(), String> {
        self.clunk(fid)
    }

    pub fn read_fid(&mut self, fid: u32, offset: u64, count: u32) -> Result<Vec<u8>, String> {
        let max_count = self.max_read_count();
        let count = if count == 0 || count > max_count {
            max_count
        } else {
            count
        };
        self.read(fid, offset, count)
    }

    pub fn write_fid(&mut self, fid: u32, offset: u64, data: &[u8]) -> Result<usize, String> {
        self.write(fid, offset, data)
    }

    pub fn create_file(&mut self, path: &str) -> Result<u32, String> {
        self.create_file_with_flags(path, ORDWR, P9_DOTL_RDWR | P9_DOTL_CREATE, 0o644)
    }

    pub fn create_file_with_flags(
        &mut self,
        path: &str,
        mode_9p: u8,
        mode_dotl: u32,
        perm: u32,
    ) -> Result<u32, String> {
        let (parent, name) = split_parent_name(path)?;
        let (fid, is_dir) = self.walk_path(parent)?;
        if !is_dir {
            self.clunk(fid)?;
            return Err(String::from("parent is not a directory"));
        }

        let result = if self.p9_version.is_dotl() {
            self.lcreate(fid, name, mode_dotl | P9_DOTL_CREATE, perm, 0)
        } else {
            self.create(fid, name, mode_9p, perm)
        };

        match result {
            Ok(()) => Ok(fid),
            Err(err) => {
                let _ = self.clunk(fid);
                Err(err)
            }
        }
    }

    pub fn read_link(&mut self, path: &str) -> Result<String, String> {
        let (fid, _is_dir) = self.walk_path(path)?;
        let tag = self.alloc_tag();
        let mut msg = Message::new(TREADLINK, tag);
        msg.push_u32(fid);
        let resp = self.send_recv(msg.finish(), RREADLINK, tag);
        let target = match resp {
            Ok(resp) => {
                let mut offset = 0;
                read_str(&resp, &mut offset)
            }
            Err(err) => Err(err),
        };
        let _ = self.clunk(fid);
        target
    }

    pub fn link(&mut self, target: &str, link_path: &str) -> Result<(), String> {
        let (parent, name) = split_parent_name(link_path)?;
        let (dfid, is_dir) = self.walk_path(parent)?;
        if !is_dir {
            self.clunk(dfid)?;
            return Err(String::from("parent is not a directory"));
        }
        let (fid, _is_dir) = self.walk_path(target)?;

        let tag = self.alloc_tag();
        let mut msg = Message::new(TLINK, tag);
        msg.push_u32(fid);
        msg.push_u32(dfid);
        msg.push_str(name);
        let result = self.send_recv(msg.finish(), RLINK, tag).map(|_| ());

        let _ = self.clunk(fid);
        let _ = self.clunk(dfid);
        result
    }

    pub fn symlink(&mut self, target: &str, link_path: &str) -> Result<(), String> {
        if !self.p9_version.is_dotl() {
            return Err(String::from("symlink requires 9P2000.L"));
        }
        let (parent, name) = split_parent_name(link_path)?;
        let (dfid, is_dir) = self.walk_path(parent)?;
        if !is_dir {
            self.clunk(dfid)?;
            return Err(String::from("parent is not a directory"));
        }

        let tag = self.alloc_tag();
        let mut msg = Message::new(TSYMLINK, tag);
        msg.push_u32(dfid);
        msg.push_str(name);
        msg.push_str(target);
        msg.push_u32(0);
        let result = self.send_recv(msg.finish(), RSYMLINK, tag).map(|_| ());

        let _ = self.clunk(dfid);
        result
    }

    pub fn remove_path(&mut self, path: &str) -> Result<(), String> {
        let (fid, _is_dir) = self.walk_path(path)?;
        let tag = self.alloc_tag();
        let mut msg = Message::new(TREMOVE, tag);
        msg.push_u32(fid);
        match self.send_recv(msg.finish(), RREMOVE, tag) {
            Ok(_) => Ok(()),
            Err(err) => {
                let _ = self.clunk(fid);
                Err(err)
            }
        }
    }

    pub fn truncate_fid(&mut self, fid: u32, size: u64) -> Result<(), String> {
        if self.p9_version.is_dotl() {
            self.setattr_size(fid, size)
        } else {
            Err(String::from("truncate requires 9P2000.L"))
        }
    }

    /// Get file attributes via TGETATTR (9P2000.L).
    pub fn getattr(&mut self, path: &str) -> Result<FileAttr, String> {
        if !self.p9_version.is_dotl() {
            return Err(String::from("getattr requires 9P2000.L"));
        }
        let (fid, _) = self.walk_path(path)?;
        let tag = self.alloc_tag();
        let mut msg = Message::new(TGETATTR, tag);
        msg.push_u32(fid);
        msg.push_u64(P9_STATS_BASIC);
        let result = self.send_recv(msg.finish(), RGETATTR, tag);
        let attr = match result {
            Ok(resp) => {
                let mut off = 0;
                let _valid = read_u64(&resp, &mut off)?;
                let qid = read_qid(&resp, &mut off)?;
                let mode = read_u32(&resp, &mut off)?;
                let uid = read_u32(&resp, &mut off)?;
                let gid = read_u32(&resp, &mut off)?;
                let nlink = read_u64(&resp, &mut off)?;
                let _rdev = read_u64(&resp, &mut off)?;
                let size = read_u64(&resp, &mut off)?;
                let _blksize = read_u64(&resp, &mut off)?;
                let _blocks = read_u64(&resp, &mut off)?;
                let atime_sec = read_u64(&resp, &mut off)?;
                let _atime_nsec = read_u64(&resp, &mut off)?;
                let mtime_sec = read_u64(&resp, &mut off)?;
                let _mtime_nsec = read_u64(&resp, &mut off)?;
                let ctime_sec = read_u64(&resp, &mut off)?;
                // remaining fields (ctime_nsec, btime, gen, data_version) skipped
                Ok(FileAttr {
                    qid_type: qid.type_,
                    mode,
                    uid,
                    gid,
                    nlink,
                    size,
                    atime_sec,
                    mtime_sec,
                    ctime_sec,
                })
            }
            Err(err) => Err(err),
        };
        let _ = self.clunk(fid);
        attr
    }

    /// Rename a file or directory via TRENAME (9P2000.L).
    pub fn rename_path(&mut self, old_path: &str, new_path: &str) -> Result<(), String> {
        if !self.p9_version.is_dotl() {
            return Err(String::from("rename requires 9P2000.L"));
        }
        let (fid, _) = self.walk_path(old_path)?;
        let (parent, name) = split_parent_name(new_path)?;
        let (dfid, is_dir) = self.walk_path(parent)?;
        if !is_dir {
            let _ = self.clunk(fid);
            let _ = self.clunk(dfid);
            return Err(String::from("target parent is not a directory"));
        }
        let tag = self.alloc_tag();
        let mut msg = Message::new(TRENAME, tag);
        msg.push_u32(fid);
        msg.push_u32(dfid);
        msg.push_str(name);
        let result = self.send_recv(msg.finish(), RRENAME, tag).map(|_| ());
        let _ = self.clunk(fid);
        let _ = self.clunk(dfid);
        result
    }

    /// Change file mode via TSETATTR (9P2000.L).
    pub fn setattr_mode(&mut self, path: &str, mode: u32) -> Result<(), String> {
        if !self.p9_version.is_dotl() {
            return Err(String::from("setattr requires 9P2000.L"));
        }
        let (fid, _) = self.walk_path(path)?;
        let tag = self.alloc_tag();
        let mut msg = Message::new(TSETATTR, tag);
        msg.push_u32(fid);
        msg.push_u32(P9_SETATTR_MODE); // valid: mode only
        msg.push_u32(mode);            // mode
        msg.push_u32(0);               // uid
        msg.push_u32(0);               // gid
        msg.push_u64(0);               // size
        msg.push_u64(0);               // atime_sec
        msg.push_u64(0);               // atime_nsec
        msg.push_u64(0);               // mtime_sec
        msg.push_u64(0);               // mtime_nsec
        let result = self.send_recv(msg.finish(), RSETATTR, tag).map(|_| ());
        let _ = self.clunk(fid);
        result
    }

    /// List directory entries with type information.
    pub fn list_dir_entries(&mut self, path: &str) -> Result<Vec<P9DirEntry>, String> {
        let (fid, is_dir) = self.walk_path(path)?;
        if !is_dir {
            self.clunk(fid)?;
            return Err(String::from("not a directory"));
        }
        self.open_with_flags(fid, OREAD, P9_DOTL_RDONLY)?;

        let mut dir_offset = 0u64;
        let mut entries = Vec::new();
        loop {
            if self.p9_version.is_dotl() {
                let (chunk, next_offset) =
                    self.readdir_entries(fid, dir_offset, self.msize - 64)?;
                if chunk.is_empty() {
                    break;
                }
                entries.extend(chunk);
                match next_offset {
                    Some(next) if next > dir_offset => dir_offset = next,
                    _ => break,
                }
            } else {
                let data = self.read(fid, dir_offset, self.msize - 64)?;
                if data.is_empty() {
                    break;
                }
                dir_offset += data.len() as u64;
                let mut names = Vec::new();
                parse_dir_entries(&data, &mut names)?;
                for name in names {
                    entries.push(P9DirEntry { name, entry_type: 0 });
                }
            }
        }
        self.clunk(fid)?;
        Ok(entries)
    }

    /// Flush file data to storage via TFSYNC (9P2000.L).
    pub fn fsync_fid(&mut self, fid: u32) -> Result<(), String> {
        if !self.p9_version.is_dotl() {
            return Err(String::from("fsync requires 9P2000.L"));
        }
        let tag = self.alloc_tag();
        let mut msg = Message::new(TFSYNC, tag);
        msg.push_u32(fid);
        self.send_recv(msg.finish(), RFSYNC, tag).map(|_| ())
    }

    fn walk_path(&mut self, path: &str) -> Result<(u32, bool), String> {
        let fid = self.alloc_fid();
        let names = path_parts(path);
        let qids = self.walk(self.root_fid, fid, &names)?;
        let is_dir = qids
            .last()
            .map(|q| q.type_ & 0x80 != 0)
            .unwrap_or(true);
        Ok((fid, is_dir))
    }

    fn send_tversion(&mut self, version: &str) -> Result<String, String> {
        let tag = NO_TAG;
        let mut msg = Message::new(TVERSION, tag);
        msg.push_u32(self.msize);
        msg.push_str(version);
        let resp = self.send_recv(msg.finish(), RVERSION, tag)?;

        let mut offset = 0;
        let msize = read_u32(&resp, &mut offset)?;
        let version = match read_str(&resp, &mut offset) {
            Ok(value) => value,
            Err(err) => {
                warn!(
                    "RVERSION parse error: {} (msize={}, remaining={})",
                    err,
                    msize,
                    dump_hex(&resp[offset..])
                );
                return Err(err);
            }
        };
        self.msize = msize.max(256);
        Ok(version)
    }

    fn send_tattach(&mut self) -> Result<(), String> {
        let tag = self.alloc_tag();
        let mut msg = Message::new(TATTACH, tag);
        msg.push_u32(self.root_fid);
        msg.push_u32(NO_FID);
        msg.push_str("root");
        msg.push_str(&self.mount_tag);
        if self.p9_version.is_dotl() {
            msg.push_u32(0);
        }
        let _ = self.send_recv(msg.finish(), RATTACH, tag)?;
        Ok(())
    }

    fn walk(&mut self, fid: u32, new_fid: u32, names: &[&str]) -> Result<Vec<Qid>, String> {
        let tag = self.alloc_tag();
        let mut msg = Message::new(TWALK, tag);
        msg.push_u32(fid);
        msg.push_u32(new_fid);
        msg.push_u16(names.len() as u16);
        for name in names {
            msg.push_str(name);
        }
        let resp = self.send_recv(msg.finish(), RWALK, tag)?;

        let mut offset = 0;
        let nwqid = read_u16(&resp, &mut offset)? as usize;
        if nwqid < names.len() {
            return Err(String::from("walk failed"));
        }

        let mut qids = Vec::with_capacity(nwqid);
        for _ in 0..nwqid {
            qids.push(read_qid(&resp, &mut offset)?);
        }
        Ok(qids)
    }

    fn open_with_flags(&mut self, fid: u32, mode_9p: u8, mode_dotl: u32) -> Result<(), String> {
        let tag = self.alloc_tag();
        if self.p9_version.is_dotl() {
            let mut msg = Message::new(TLOPEN, tag);
            msg.push_u32(fid);
            msg.push_u32(mode_dotl);
            let _ = self.send_recv(msg.finish(), RLOPEN, tag)?;
        } else {
            let mut msg = Message::new(TOPEN, tag);
            msg.push_u32(fid);
            msg.push_u8(mode_9p);
            let _ = self.send_recv(msg.finish(), ROPEN, tag)?;
        }
        Ok(())
    }

    fn create(&mut self, fid: u32, name: &str, mode: u8, perm: u32) -> Result<(), String> {
        let tag = self.alloc_tag();
        let mut msg = Message::new(TCREATE, tag);
        msg.push_u32(fid);
        msg.push_str(name);
        msg.push_u32(perm);
        msg.push_u8(mode);
        let _ = self.send_recv(msg.finish(), RCREATE, tag)?;
        Ok(())
    }

    fn lcreate(
        &mut self,
        fid: u32,
        name: &str,
        flags: u32,
        mode: u32,
        gid: u32,
    ) -> Result<(), String> {
        let tag = self.alloc_tag();
        let mut msg = Message::new(TLCREATE, tag);
        msg.push_u32(fid);
        msg.push_str(name);
        msg.push_u32(flags);
        msg.push_u32(mode);
        msg.push_u32(gid);
        let _ = self.send_recv(msg.finish(), RLCREATE, tag)?;
        Ok(())
    }

    fn mkdir(&mut self, fid: u32, name: &str, perm: u32, gid: u32) -> Result<(), String> {
        let tag = self.alloc_tag();
        let mut msg = Message::new(TMKDIR, tag);
        msg.push_u32(fid);
        msg.push_str(name);
        msg.push_u32(perm);
        msg.push_u32(gid);
        let _ = self.send_recv(msg.finish(), RMKDIR, tag)?;
        Ok(())
    }

    fn read(&mut self, fid: u32, offset: u64, count: u32) -> Result<Vec<u8>, String> {
        let tag = self.alloc_tag();
        let mut msg = Message::new(TREAD, tag);
        msg.push_u32(fid);
        msg.push_u64(offset);
        msg.push_u32(count);
        let resp = self.send_recv(msg.finish(), RREAD, tag)?;

        let mut offset = 0;
        let data_len = read_u32(&resp, &mut offset)? as usize;
        if offset + data_len > resp.len() {
            return Err(String::from("short read response"));
        }
        Ok(resp[offset..offset + data_len].to_vec())
    }

    fn readdir(
        &mut self,
        fid: u32,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<String>, Option<u64>), String> {
        let tag = self.alloc_tag();
        let mut msg = Message::new(TREADDIR, tag);
        msg.push_u32(fid);
        msg.push_u64(offset);
        msg.push_u32(count);
        let resp = self.send_recv(msg.finish(), RREADDIR, tag)?;

        let mut offset = 0;
        let data_len = read_u32(&resp, &mut offset)? as usize;
        if offset + data_len > resp.len() {
            return Err(String::from("short readdir response"));
        }
        parse_dir_entries_l(&resp[offset..offset + data_len])
    }

    fn readdir_entries(
        &mut self,
        fid: u32,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<P9DirEntry>, Option<u64>), String> {
        let tag = self.alloc_tag();
        let mut msg = Message::new(TREADDIR, tag);
        msg.push_u32(fid);
        msg.push_u64(offset);
        msg.push_u32(count);
        let resp = self.send_recv(msg.finish(), RREADDIR, tag)?;

        let mut off = 0;
        let data_len = read_u32(&resp, &mut off)? as usize;
        if off + data_len > resp.len() {
            return Err(String::from("short readdir response"));
        }
        let data = &resp[off..off + data_len];

        let mut entries = Vec::new();
        let mut last_offset = None;
        let mut parse_off = 0usize;
        while parse_off < data.len() {
            let _qid = read_qid(data, &mut parse_off)?;
            let entry_offset = read_u64(data, &mut parse_off)?;
            let entry_type = read_u8(data, &mut parse_off)?;
            let name = read_str(data, &mut parse_off)?;
            if name != "." && name != ".." {
                entries.push(P9DirEntry { name, entry_type });
            }
            last_offset = Some(entry_offset);
        }
        Ok((entries, last_offset))
    }

    #[allow(dead_code)]
    fn write(&mut self, fid: u32, offset: u64, data: &[u8]) -> Result<usize, String> {
        let tag = self.alloc_tag();
        let mut msg = Message::new(TWRITE, tag);
        msg.push_u32(fid);
        msg.push_u64(offset);
        msg.push_u32(data.len() as u32);
        msg.push_bytes(data);
        let resp = self.send_recv(msg.finish(), RWRITE, tag)?;

        let mut offset = 0;
        let wrote = read_u32(&resp, &mut offset)? as usize;
        Ok(wrote)
    }

    fn clunk(&mut self, fid: u32) -> Result<(), String> {
        let tag = self.alloc_tag();
        let mut msg = Message::new(TCLUNK, tag);
        msg.push_u32(fid);
        let _ = self.send_recv(msg.finish(), RCLUNK, tag)?;
        Ok(())
    }

    fn setattr_size(&mut self, fid: u32, size: u64) -> Result<(), String> {
        let tag = self.alloc_tag();
        let mut msg = Message::new(TSETATTR, tag);
        msg.push_u32(fid);
        msg.push_u32(P9_ATTR_SIZE);
        msg.push_u32(0);
        msg.push_u32(0);
        msg.push_u32(0);
        msg.push_u64(size);
        msg.push_u64(0);
        msg.push_u64(0);
        msg.push_u64(0);
        msg.push_u64(0);
        let _ = self.send_recv(msg.finish(), RSETATTR, tag)?;
        Ok(())
    }


    fn send_recv(&mut self, req: Vec<u8>, expect: u8, tag: u16) -> Result<Vec<u8>, String> {
        let mut resp = vec![0u8; self.msize as usize];
        let size = self.transport.request(&req, &mut resp)?;
        if size < 7 {
            return Err(String::from("short 9p response"));
        }
        let resp = &resp[..size];
        let resp_type = resp[4];
        let resp_tag = u16::from_le_bytes([resp[5], resp[6]]);
        if resp_type == RERROR {
            let mut offset = 7;
            let msg = read_str(resp, &mut offset).unwrap_or_else(|_| String::from("unknown"));
            return Err(msg);
        }
        if resp_type == RLERROR {
            let mut offset = 7;
            let errno = read_u32(resp, &mut offset).unwrap_or(0);
            return Err(format!("rlerror errno={}", errno));
        }
        if resp_type != expect {
            return Err(format!("unexpected response type: {}", resp_type));
        }
        if resp_tag != tag {
            return Err(String::from("tag mismatch"));
        }
        Ok(resp[7..].to_vec())
    }

    fn alloc_tag(&mut self) -> u16 {
        let mut tag = self.next_tag;
        self.next_tag = self.next_tag.wrapping_add(1);
        if tag == NO_TAG {
            tag = self.next_tag;
            self.next_tag = self.next_tag.wrapping_add(1);
        }
        tag
    }

    fn alloc_fid(&mut self) -> u32 {
        let fid = self.next_fid;
        self.next_fid = self.next_fid.wrapping_add(1);
        fid
    }
}
