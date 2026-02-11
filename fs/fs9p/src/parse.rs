//! Path handling and directory entry parsing.

use alloc::string::String;
use alloc::vec::Vec;

use crate::message::{read_qid, read_str, read_u16, read_u32, read_u64, read_u8};

/// Split a path into parent directory and leaf name.
pub(crate) fn split_parent_name(path: &str) -> Result<(&str, &str), String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() || trimmed == "/" {
        return Err(String::from("invalid path"));
    }
    let mut parts = trimmed.rsplitn(2, '/');
    let name = parts.next().unwrap_or("");
    let parent = parts.next().unwrap_or("");
    let parent = if parent.is_empty() { "/" } else { parent };
    if name.is_empty() {
        Err(String::from("invalid path"))
    } else {
        Ok((parent, name))
    }
}

/// Split a path into normalized components.
pub(crate) fn path_parts(path: &str) -> Vec<&str> {
    path.split('/').filter(|part| !part.is_empty() && *part != ".").collect()
}

/// Parse 9P2000 stat-based directory entries.
pub(crate) fn parse_dir_entries(data: &[u8], names: &mut Vec<String>) -> Result<(), String> {
    let mut offset = 0usize;
    while offset + 2 <= data.len() {
        let size = u16::from_le_bytes([data[offset], data[offset + 1]]) as usize;
        offset += 2;
        if offset + size > data.len() {
            break;
        }
        let entry = &data[offset..offset + size];
        offset += size;
        let name = parse_stat_name(entry)?;
        if name != "." && name != ".." {
            names.push(name);
        }
    }
    Ok(())
}

/// Parse 9P2000.L readdir entries and return the last offset.
pub(crate) fn parse_dir_entries_l(data: &[u8]) -> Result<(Vec<String>, Option<u64>), String> {
    let mut offset = 0usize;
    let mut names = Vec::new();
    let mut last_offset = None;
    while offset < data.len() {
        let _qid = read_qid(data, &mut offset)?;
        let entry_offset = read_u64(data, &mut offset)?;
        let _entry_type = read_u8(data, &mut offset)?;
        let name = read_str(data, &mut offset)?;
        if name != "." && name != ".." {
            names.push(name);
        }
        last_offset = Some(entry_offset);
    }
    Ok((names, last_offset))
}

fn parse_stat_name(buf: &[u8]) -> Result<String, String> {
    let mut offset = 0usize;
    if buf.len() < 39 {
        return Err(String::from("stat too short"));
    }
    let _type = read_u16(buf, &mut offset)?;
    let _dev = read_u32(buf, &mut offset)?;
    let _qid = read_qid(buf, &mut offset)?;
    let _mode = read_u32(buf, &mut offset)?;
    let _atime = read_u32(buf, &mut offset)?;
    let _mtime = read_u32(buf, &mut offset)?;
    let _length = read_u64(buf, &mut offset)?;
    let name = read_str(buf, &mut offset)?;
    Ok(name)
}
