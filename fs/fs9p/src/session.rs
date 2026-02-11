//! 9P session state and high-level operations.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use log::{info, warn};

use crate::message::{dump_hex, read_qid, read_str, read_u16, read_u32, Message};
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
    version: String,
    transport: Box<dyn Transport>,
}

/// Basic information about a path in the 9P server.
pub struct NodeInfo {
    pub qid_path: u64,
    pub is_dir: bool,
    pub is_symlink: bool,
}

impl P9Session {
    /// Create a new session with the given transport and mount tag.
    pub fn new(transport: Box<dyn Transport>, mount_tag: String) -> Self {
        Self {
            msize: DEFAULT_MSIZE,
            next_tag: 1,
            next_fid: 2,
            root_fid: 1,
            mount_tag,
            version: String::from("unknown"),
            transport,
        }
    }

    /// Negotiate protocol version and attach to the server root.
    pub fn negotiate(&mut self) -> Result<(), String> {
        let mut last_version = String::from("unknown");
        for version in [
            "9p2000.L",
            "9p2000.u",
            "9p2000",
            "9P2000.L",
            "9P2000.u",
            "9P2000",
        ] {
            let resp = self.send_tversion(version)?;
            if resp.to_ascii_lowercase().starts_with("9p2000") {
                self.version = resp;
                self.send_tattach()?;
                return Ok(());
            }
            warn!("RVERSION not accepted (req={}, resp={})", version, resp);
            last_version = resp;
        }
        Err(format!("unsupported 9p version: {}", last_version))
    }

    /// List directory entries at the provided path.
    pub fn list_dir(&mut self, path: &str) -> Result<Vec<String>, String> {
        let (fid, is_dir, _, _) = self.walk_path(path)?;
        if !is_dir {
            self.clunk(fid)?;
            return Err(String::from("not a directory"));
        }

        self.open(fid, OREAD)?;

        let mut offset = 0u64;
        let mut names = Vec::new();
        loop {
            if self.version.to_ascii_lowercase().ends_with(".l") {
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
        let (fid, is_dir, _, _) = self.walk_path(path)?;
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
        let (fid, is_dir, _, _) = self.walk_path(parent)?;
        if !is_dir {
            self.clunk(fid)?;
            return Err(String::from("parent is not a directory"));
        }

        self.mkdir(fid, name, 0o755)?;
        self.clunk(fid)?;
        Ok(())
    }

    /// Returns basic info for the given path.
    pub fn lookup_path(&mut self, path: &str) -> Result<NodeInfo, String> {
        let (fid, is_dir, is_symlink, qid_path) = self.walk_path(path)?;
        self.clunk(fid)?;
        Ok(NodeInfo {
            qid_path,
            is_dir,
            is_symlink,
        })
    }

    /// Reads a file at the given path.
    pub fn read_file(&mut self, path: &str, offset: u64, count: u32) -> Result<Vec<u8>, String> {
        let (fid, is_dir, _, _) = self.walk_path(path)?;
        if is_dir {
            self.clunk(fid)?;
            return Err(String::from("is a directory"));
        }
        self.open(fid, OREAD)?;
        let data = self.read(fid, offset, count)?;
        self.clunk(fid)?;
        Ok(data)
    }

    /// Writes a file at the given path.
    pub fn write_file(&mut self, path: &str, offset: u64, data: &[u8]) -> Result<usize, String> {
        let (fid, is_dir, _, _) = self.walk_path(path)?;
        if is_dir {
            self.clunk(fid)?;
            return Err(String::from("is a directory"));
        }
        self.open(fid, OWRITE)?;
        let wrote = self.write(fid, offset, data)?;
        self.clunk(fid)?;
        Ok(wrote)
    }

    /// Creates a regular file at the given path.
    pub fn create_file(&mut self, path: &str, perm: u32) -> Result<(), String> {
        let (parent, name) = split_parent_name(path)?;
        let (fid, is_dir, _, _) = self.walk_path(parent)?;
        if !is_dir {
            self.clunk(fid)?;
            return Err(String::from("parent is not a directory"));
        }
        self.create(fid, name, OREAD, perm)?;
        self.clunk(fid)?;
        Ok(())
    }

    pub fn create_symlink(&mut self, path: &str, target: &str) -> Result<(), String> {
        if !self.version.to_ascii_lowercase().ends_with(".l") {
            return Err(String::from("unsupported"));
        }
        let (parent, name) = split_parent_name(path)?;
        let (fid, is_dir, _, _) = self.walk_path(parent)?;
        if !is_dir {
            self.clunk(fid)?;
            return Err(String::from("parent is not a directory"));
        }
        let _ = self.symlink(fid, name, target, 0)?;
        self.clunk(fid)?;
        Ok(())
    }

    pub fn read_link(&mut self, path: &str) -> Result<String, String> {
        if !self.version.to_ascii_lowercase().ends_with(".l") {
            return Err(String::from("unsupported"));
        }
        let (fid, is_dir, is_symlink, _) = self.walk_path(path)?;
        if is_dir {
            self.clunk(fid)?;
            return Err(String::from("is a directory"));
        }
        if !is_symlink {
            self.clunk(fid)?;
            return Err(String::from("not a symlink"));
        }
        let target = self.readlink(fid)?;
        self.clunk(fid)?;
        Ok(target)
    }

    /// Removes a file or directory at the given path.
    pub fn remove_path(&mut self, path: &str) -> Result<(), String> {
        let (fid, _is_dir, _, _) = self.walk_path(path)?;
        let result = self.remove(fid);
        if result.is_err() {
            let _ = self.clunk(fid);
        }
        result
    }

    /// Returns a suggested maximum read size for file operations.
    pub fn read_chunk_size(&self) -> u32 {
        self.msize.saturating_sub(64).max(256)
    }

    fn walk_path(&mut self, path: &str) -> Result<(u32, bool, bool, u64), String> {
        let fid = self.alloc_fid();
        let names = path_parts(path);
        let qids = self.walk(self.root_fid, fid, &names)?;
        let (is_dir, is_symlink, qid_path) = qids
            .last()
            .map(|qid| (qid.is_dir(), qid.is_symlink(), qid.path()))
            .unwrap_or((true, false, self.root_fid as u64));
        Ok((fid, is_dir, is_symlink, qid_path))
    }

    fn remove(&mut self, fid: u32) -> Result<(), String> {
        let tag = self.alloc_tag();
        let mut msg = Message::new(TREMOVE, tag);
        msg.push_u32(fid);
        let _ = self.send_recv(msg.finish(), RREMOVE, tag)?;
        Ok(())
    }

    fn send_tversion(&mut self, version: &str) -> Result<String, String> {
        let tag = NO_TAG;
        let mut msg = Message::new(TVERSION, tag);
        msg.push_u32(self.msize);
        msg.push_str(version);
        info!("9p tversion: req={}, msize={}", version, self.msize);
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
        info!("9p rversion: resp={}, msize={}", version, msize);
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
        if self.version.to_ascii_lowercase().ends_with(".l") {
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

    fn open(&mut self, fid: u32, mode: u8) -> Result<(), String> {
        let tag = self.alloc_tag();
        if self.version.to_ascii_lowercase().ends_with(".l") {
            let mut msg = Message::new(TLOPEN, tag);
            msg.push_u32(fid);
            msg.push_u32(mode as u32);
            let _ = self.send_recv(msg.finish(), RLOPEN, tag)?;
        } else {
            let mut msg = Message::new(TOPEN, tag);
            msg.push_u32(fid);
            msg.push_u8(mode);
            let _ = self.send_recv(msg.finish(), ROPEN, tag)?;
        }
        Ok(())
    }

    fn create(&mut self, fid: u32, name: &str, mode: u8, perm: u32) -> Result<(), String> {
        let tag = self.alloc_tag();
        if self.version.to_ascii_lowercase().ends_with(".l") {
            let mut msg = Message::new(TLCREATE, tag);
            msg.push_u32(fid);
            msg.push_str(name);
            msg.push_u32(mode as u32);
            msg.push_u32(perm);
            msg.push_u32(0);
            let _ = self.send_recv(msg.finish(), RLCREATE, tag)?;
        } else {
            let mut msg = Message::new(TCREATE, tag);
            msg.push_u32(fid);
            msg.push_str(name);
            msg.push_u32(perm);
            msg.push_u8(mode);
            let _ = self.send_recv(msg.finish(), RCREATE, tag)?;
        }
        Ok(())
    }

    fn mkdir(&mut self, fid: u32, name: &str, perm: u32) -> Result<(), String> {
        let tag = self.alloc_tag();
        if self.version.to_ascii_lowercase().ends_with(".l") {
            let mut msg = Message::new(TMKDIR, tag);
            msg.push_u32(fid);
            msg.push_str(name);
            msg.push_u32(perm);
            msg.push_u32(0);
            let _ = self.send_recv(msg.finish(), RMKDIR, tag)?;
        } else {
            self.create(fid, name, OREAD, DMDIR | perm)?;
        }
        Ok(())
    }

    fn symlink(&mut self, fid: u32, name: &str, target: &str, gid: u32) -> Result<Qid, String> {
        let tag = self.alloc_tag();
        let mut msg = Message::new(TSYMLINK, tag);
        msg.push_u32(fid);
        msg.push_str(name);
        msg.push_str(target);
        msg.push_u32(gid);
        let resp = self.send_recv(msg.finish(), RSYMLINK, tag)?;

        let mut offset = 0;
        let qid = read_qid(&resp, &mut offset)?;
        Ok(qid)
    }

    fn readlink(&mut self, fid: u32) -> Result<String, String> {
        let tag = self.alloc_tag();
        let mut msg = Message::new(TREADLINK, tag);
        msg.push_u32(fid);
        let resp = self.send_recv(msg.finish(), RREADLINK, tag)?;

        let mut offset = 0;
        read_str(&resp, &mut offset)
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

    fn send_recv(&mut self, req: Vec<u8>, expect: u8, tag: u16) -> Result<Vec<u8>, String> {
        let mut resp = vec![0u8; self.msize as usize];
        let size = self.transport.request(&req, &mut resp)?;
        if size < 7 {
            let req_type = req.get(4).copied().unwrap_or(0);
            warn!(
                "9p short response: req_type={}, expect={}, tag={}, size={}",
                req_type,
                expect,
                tag,
                size
            );
            return Err(String::from("short 9p response"));
        }
        let resp = &resp[..size];
        let resp_type = resp[4];
        let resp_tag = u16::from_le_bytes([resp[5], resp[6]]);
        if resp_type == RERROR {
            let mut offset = 7;
            let msg = read_str(resp, &mut offset).unwrap_or_else(|_| String::from("unknown"));
            warn!(
                "9p rerror: expect={}, tag={}, msg={}, body={}",
                expect,
                tag,
                msg,
                dump_hex(&resp[7..])
            );
            return Err(msg);
        }
        if resp_type == RLERROR {
            let mut offset = 7;
            let errno = read_u32(resp, &mut offset).unwrap_or(0);
            warn!(
                "9p rlerror: expect={}, tag={}, errno={}, body={}",
                expect,
                tag,
                errno,
                dump_hex(&resp[7..])
            );
            return Err(format!("rlerror errno={}", errno));
        }
        if resp_type != expect {
            warn!(
                "9p unexpected response: expect={}, got={}, tag={}, resp_tag={}, body={}",
                expect,
                resp_type,
                tag,
                resp_tag,
                dump_hex(&resp[7..])
            );
            return Err(format!("unexpected response type: {}", resp_type));
        }
        if resp_tag != tag {
            warn!(
                "9p tag mismatch: expect_tag={}, resp_tag={}, type={}, body={}",
                tag,
                resp_tag,
                resp_type,
                dump_hex(&resp[7..])
            );
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
