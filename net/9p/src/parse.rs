// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! 9P client path handling and directory entry parsing.

use alloc::{string::String, vec::Vec};

use crate::message::{read_qid, read_str, read_u8, read_u16, read_u32, read_u64};

fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Err(String::from("invalid path"));
    }
    Ok(())
}

fn normalized_path_parts(path: &str) -> Result<Vec<&str>, String> {
    if path.is_empty() {
        return Err(String::from("invalid path"));
    }

    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return Err(String::from("invalid path"));
                }
            }
            name => {
                validate_name(name)?;
                parts.push(name);
            }
        }
    }
    Ok(parts)
}

/// Split a path into parent directory and leaf name.
pub(crate) fn split_parent_name(path: &str) -> Result<(&str, &str), String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() || trimmed == "/" {
        return Err(String::from("invalid path"));
    }
    normalized_path_parts(trimmed)?;
    let mut parts = trimmed.rsplitn(2, '/');
    let name = parts.next().unwrap_or("");
    validate_name(name)?;
    let parent = parts.next().unwrap_or("");
    let parent = if parent.is_empty() { "/" } else { parent };
    Ok((parent, name))
}

/// Split a path into normalized components.
pub(crate) fn path_parts(path: &str) -> Result<Vec<&str>, String> {
    normalized_path_parts(path)
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

#[cfg(unittest)]
mod tests {
    use alloc::vec;

    use unittest::{assert, assert_eq, def_test};

    #[def_test]
    fn path_parts_rejects_root_escape() {
        assert!(super::path_parts("").is_err());
        assert!(super::path_parts("../host").is_err());
        assert!(super::path_parts("/../../host").is_err());
    }

    #[def_test]
    fn path_parts_normalizes_inside_root() {
        assert_eq!(super::path_parts("/a/./b/../c").unwrap(), vec!["a", "c"]);
    }

    #[def_test]
    fn split_parent_name_rejects_invalid_leaf() {
        assert!(super::split_parent_name("/a/..").is_err());
        assert!(super::split_parent_name("/a/").is_ok());
    }
}
