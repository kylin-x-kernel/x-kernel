// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Guest image loader — reads files from the kernel VFS into guest
//! physical memory.

use alloc::{sync::Arc, vec};

use kcred::Cred;
use kvfs::{Filename, NodePermission, VfsFile};

use crate::mm::GuestMem;

#[derive(Debug)]
pub enum LoadError {
    FileNotFound,
    ReadFailed,
    AddressTranslation,
    NoGuestMem,
    DtbPatchFailed,
}

const BUF_SIZE: usize = 4096;

fn open_kernel_file(path: &str) -> Result<Arc<VfsFile>, LoadError> {
    let fs_struct = fs_context::init_fs();
    let fs = fs_struct.lock();
    let cred = Arc::new(Cred::root());
    Filename::new(path)
        .open_with_flags_at(
            fs.root(),
            fs.pwd(),
            linux_raw_sys::general::O_RDONLY,
            NodePermission::empty(),
            NodePermission::empty(),
            cred,
        )
        .map_err(|e| {
            log::error!("[kvmm-loader] open {:?}: {:?}", path, e);
            LoadError::FileNotFound
        })
}

/// Load a file at `path` into guest physical memory starting at `load_gpa`.
///
/// The VM's second-stage page table translates `load_gpa` to a host
/// physical address, which is then converted to a kernel VA for the copy.
/// Returns the number of bytes loaded.
pub fn load_image_to_guest<G: GuestMem>(
    guest_mem: &G,
    path: &str,
    load_gpa: u64,
) -> Result<usize, LoadError> {
    let file = open_kernel_file(path)?;

    let mut buf = vec![0u8; BUF_SIZE];
    let mut offset: u64 = 0;
    let mut total: usize = 0;

    loop {
        let dst_offset = offset;
        let n = file.read_from(&mut buf[..], &mut offset).map_err(|e| {
            log::error!(
                "[kvmm-loader] read {:?} offset {}: {:?}",
                path,
                dst_offset,
                e
            );
            LoadError::ReadFailed
        })?;
        if n == 0 {
            break;
        }

        let dst_gpa = load_gpa + dst_offset;
        let hpa = guest_mem
            .gpa_to_hpa(dst_gpa)
            .ok_or(LoadError::AddressTranslation)?;
        let dst_va = kaddr_layout::p2v(hpa as usize) as *mut u8;

        // SAFETY: hpa → kernel VA is valid within the identity-mapped guest
        // RAM region; n ≤ BUF_SIZE and the destination is within guest RAM.
        unsafe {
            core::ptr::copy_nonoverlapping(buf.as_ptr(), dst_va, n);
        }

        total += n;
    }

    log::info!(
        "[kvmm-loader] loaded {:?}: {} bytes to GPA {:#x}",
        path,
        total,
        load_gpa,
    );
    Ok(total)
}

/// Count the guest CPUs described by a DTB file (`/cpus/cpu@N` nodes with
/// `device_type = "cpu"`), without touching guest memory.
///
/// Used to size `nr_vcpus` so the VM always matches its own DTB.
pub fn peek_dtb_cpu_count(path: &str) -> Result<usize, LoadError> {
    let file = open_kernel_file(path)?;

    let mut dtb: alloc::vec::Vec<u8> = alloc::vec![];
    let mut buf = alloc::vec![0u8; BUF_SIZE];
    let mut offset: u64 = 0;
    loop {
        let n = file
            .read_from(&mut buf[..], &mut offset)
            .map_err(|_| LoadError::ReadFailed)?;
        if n == 0 {
            break;
        }
        dtb.extend_from_slice(&buf[..n]);
    }

    if dtb.len() < 40 || be32(&dtb, 0) != FDT_MAGIC {
        log::error!("[kvmm-loader] DTB bad magic");
        return Err(LoadError::DtbPatchFailed);
    }

    let off_struct = be32(&dtb, 8) as usize;
    let off_strings = be32(&dtb, 12) as usize;
    let size_strings = be32(&dtb, 32) as usize;
    if off_strings + size_strings > dtb.len() || off_struct >= dtb.len() {
        return Err(LoadError::DtbPatchFailed);
    }

    let strings = &dtb[off_strings..off_strings + size_strings];
    let devtype_noff = find_string_offset(strings, b"device_type");

    // A node "counts" as a CPU if its name starts with "cpu" AND it has a
    // device_type property equal to "cpu" (the "cpus" container has no such
    // property, so it is excluded).
    let mut name_starts_cpu = false;
    let mut count = 0usize;
    let mut pos = off_struct;
    while pos + 4 <= dtb.len() {
        let token = be32(&dtb, pos);
        pos += 4;
        match token {
            FDT_BEGIN_NODE => {
                let name_start = pos;
                while pos < dtb.len() && dtb[pos] != 0 {
                    pos += 1;
                }
                let name = core::str::from_utf8(&dtb[name_start..pos]).unwrap_or("");
                pos += 1;
                pos = (pos + 3) & !3;
                name_starts_cpu = name.starts_with("cpu");
            }
            FDT_PROP => {
                if pos + 8 > dtb.len() {
                    break;
                }
                let len = be32(&dtb, pos) as usize;
                let nameoff = be32(&dtb, pos + 4);
                let data_pos = pos + 8;
                if name_starts_cpu
                    && Some(nameoff) == devtype_noff
                    && len >= 4
                    && data_pos + len <= dtb.len()
                    && &dtb[data_pos..data_pos + 4] == b"cpu\0"
                {
                    count += 1;
                }
                pos = data_pos + ((len + 3) & !3);
            }
            FDT_END_NODE => {}
            FDT_NOP => {}
            FDT_END => break,
            _ => break,
        }
    }

    log::info!("[kvmm-loader] DTB {:?}: {} cpu node(s)", path, count);
    Ok(count)
}

const FDT_MAGIC: u32 = 0xd00dfeed;
const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_NOP: u32 = 4;
const FDT_END: u32 = 9;

fn be32(buf: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

fn find_string_offset(strings: &[u8], name: &[u8]) -> Option<u32> {
    if strings.len() < name.len() + 1 {
        return None;
    }
    for i in 0..=strings.len() - name.len() - 1 {
        if (i == 0 || strings[i - 1] == 0)
            && strings[i..i + name.len()] == *name
            && strings[i + name.len()] == 0
        {
            return Some(i as u32);
        }
    }
    None
}

/// Patch `linux,initrd-start` and `linux,initrd-end` properties in a DTB
/// that has already been loaded into guest memory.
pub fn patch_dtb_initrd<G: GuestMem>(
    guest_mem: &G,
    dtb_gpa: u64,
    dtb_size: usize,
    initrd_start: u64,
    initrd_end: u64,
) -> Result<(), LoadError> {
    let mut dtb = vec![0u8; dtb_size];
    read_guest_mem(guest_mem, dtb_gpa, &mut dtb)?;

    if dtb.len() < 40 || be32(&dtb, 0) != FDT_MAGIC {
        log::error!("[kvmm-loader] DTB bad magic");
        return Err(LoadError::DtbPatchFailed);
    }

    let off_struct = be32(&dtb, 8) as usize;
    let off_strings = be32(&dtb, 12) as usize;
    let size_strings = be32(&dtb, 32) as usize;

    if off_strings + size_strings > dtb.len() || off_struct >= dtb.len() {
        return Err(LoadError::DtbPatchFailed);
    }

    let strings = &dtb[off_strings..off_strings + size_strings];
    let start_noff = find_string_offset(strings, b"linux,initrd-start");
    let end_noff = find_string_offset(strings, b"linux,initrd-end");

    if start_noff.is_none() && end_noff.is_none() {
        log::warn!("[kvmm-loader] DTB has no initrd properties");
        return Err(LoadError::DtbPatchFailed);
    }

    let mut pos = off_struct;
    let mut patched = 0u32;

    loop {
        if pos + 4 > dtb.len() {
            break;
        }
        let token = be32(&dtb, pos);
        pos += 4;

        match token {
            FDT_BEGIN_NODE => {
                while pos < dtb.len() && dtb[pos] != 0 {
                    pos += 1;
                }
                pos += 1;
                pos = (pos + 3) & !3;
            }
            FDT_PROP => {
                if pos + 8 > dtb.len() {
                    break;
                }
                let len = be32(&dtb, pos) as usize;
                let nameoff = be32(&dtb, pos + 4);

                if len == 8 && pos + 8 + 8 <= dtb.len() {
                    if Some(nameoff) == start_noff {
                        dtb[pos + 8..pos + 16].copy_from_slice(&initrd_start.to_be_bytes());
                        patched += 1;
                    } else if Some(nameoff) == end_noff {
                        dtb[pos + 8..pos + 16].copy_from_slice(&initrd_end.to_be_bytes());
                        patched += 1;
                    }
                }

                pos += 8 + ((len + 3) & !3);
            }
            FDT_END_NODE | FDT_NOP => {}
            FDT_END => break,
            _ => break,
        }
    }

    if patched > 0 {
        write_guest_mem(guest_mem, dtb_gpa, &dtb)?;
        log::info!(
            "[kvmm-loader] patched DTB: initrd [{:#x}, {:#x})",
            initrd_start,
            initrd_end,
        );
    }

    Ok(())
}

/// Patch the `reg` property of the `/memory` node in a DTB already loaded
/// into guest memory, so the guest sees the per-VM base/size.
///
/// The number of address/size cells is taken from the root node's
/// `#address-cells`/`#size-cells` (defaulting to 2/2, the standard arm64
/// layout). The memory node is identified by a node name starting with
/// `memory` (e.g. `memory@70000000`); its node-name unit need not match the
/// new base, since Linux reads the address from `reg`.
pub fn patch_dtb_memory<G: GuestMem>(
    guest_mem: &G,
    dtb_gpa: u64,
    dtb_size: usize,
    mem_base: u64,
    mem_size: u64,
) -> Result<(), LoadError> {
    let mut dtb = vec![0u8; dtb_size];
    read_guest_mem(guest_mem, dtb_gpa, &mut dtb)?;

    if dtb.len() < 40 || be32(&dtb, 0) != FDT_MAGIC {
        log::error!("[kvmm-loader] DTB bad magic");
        return Err(LoadError::DtbPatchFailed);
    }

    let off_struct = be32(&dtb, 8) as usize;
    let off_strings = be32(&dtb, 12) as usize;
    let size_strings = be32(&dtb, 32) as usize;

    if off_strings + size_strings > dtb.len() || off_struct >= dtb.len() {
        return Err(LoadError::DtbPatchFailed);
    }

    let strings = &dtb[off_strings..off_strings + size_strings];
    let reg_noff = find_string_offset(strings, b"reg");
    let addr_cells_noff = find_string_offset(strings, b"#address-cells");
    let size_cells_noff = find_string_offset(strings, b"#size-cells");

    let mut addr_cells: u32 = 2;
    let mut size_cells: u32 = 2;
    let mut depth: u32 = 0;
    let mut in_memory = false;
    let mut memory_depth: u32 = 0;
    let mut patched = 0u32;

    let mut pos = off_struct;
    loop {
        if pos + 4 > dtb.len() {
            break;
        }
        let token = be32(&dtb, pos);
        pos += 4;

        match token {
            FDT_BEGIN_NODE => {
                // Node name: null-terminated, then 4-byte aligned.
                let name_start = pos;
                while pos < dtb.len() && dtb[pos] != 0 {
                    pos += 1;
                }
                let name = core::str::from_utf8(&dtb[name_start..pos]).unwrap_or("");
                pos += 1; // skip NUL
                pos = (pos + 3) & !3;

                depth += 1;
                if depth == 2 && name.starts_with("memory") {
                    in_memory = true;
                    memory_depth = depth;
                }
            }
            FDT_PROP => {
                if pos + 8 > dtb.len() {
                    break;
                }
                let len = be32(&dtb, pos) as usize;
                let nameoff = be32(&dtb, pos + 4);
                let data_pos = pos + 8;

                // Root node properties (#address-cells/#size-cells live here).
                if depth == 1 && len == 4 && data_pos + 4 <= dtb.len() {
                    if Some(nameoff) == addr_cells_noff {
                        addr_cells = be32(&dtb, data_pos);
                    } else if Some(nameoff) == size_cells_noff {
                        size_cells = be32(&dtb, data_pos);
                    }
                }

                // The memory node's `reg` property: rewrite base/size.
                if in_memory
                    && Some(nameoff) == reg_noff
                    && (addr_cells as usize + size_cells as usize) * 4 == len
                    && data_pos + len <= dtb.len()
                {
                    encode_cells(&mut dtb, data_pos, addr_cells as usize, mem_base);
                    encode_cells(
                        &mut dtb,
                        data_pos + addr_cells as usize * 4,
                        size_cells as usize,
                        mem_size,
                    );
                    patched += 1;
                }

                pos = data_pos + ((len + 3) & !3);
            }
            FDT_END_NODE => {
                if in_memory && depth == memory_depth {
                    in_memory = false;
                }
                depth = depth.saturating_sub(1);
            }
            FDT_NOP => {}
            FDT_END => break,
            _ => break,
        }
    }

    if patched > 0 {
        write_guest_mem(guest_mem, dtb_gpa, &dtb)?;
        log::info!(
            "[kvmm-loader] patched DTB: memory base={:#x} size={:#x} ({}+{} cells)",
            mem_base,
            mem_size,
            addr_cells,
            size_cells,
        );
    } else {
        log::warn!("[kvmm-loader] DTB has no patchable /memory reg node");
    }

    Ok(())
}

/// Write `value` as `cells` big-endian u32 cells into `dtb` at `offset`.
fn encode_cells(dtb: &mut [u8], offset: usize, cells: usize, value: u64) {
    for i in 0..cells {
        let shift = (cells - 1 - i) * 32;
        let cell = ((value >> shift) & 0xFFFF_FFFF) as u32;
        dtb[offset + i * 4..offset + i * 4 + 4].copy_from_slice(&cell.to_be_bytes());
    }
}

/// Disable selected DTB nodes by replacing their structure-block subtree with NOPs.
pub fn nop_dtb_nodes<G: GuestMem>(
    guest_mem: &G,
    dtb_gpa: u64,
    dtb_size: usize,
    names: &[&str],
) -> Result<u32, LoadError> {
    let mut dtb = vec![0u8; dtb_size];
    read_guest_mem(guest_mem, dtb_gpa, &mut dtb)?;

    if dtb.len() < 40 || be32(&dtb, 0) != FDT_MAGIC {
        log::error!("[kvmm-loader] DTB bad magic");
        return Err(LoadError::DtbPatchFailed);
    }

    let off_struct = be32(&dtb, 8) as usize;
    let size_struct = be32(&dtb, 36) as usize;
    let end_struct = off_struct
        .checked_add(size_struct)
        .filter(|end| *end <= dtb.len())
        .ok_or(LoadError::DtbPatchFailed)?;

    let mut pos = off_struct;
    let mut patched = 0u32;

    while pos + 4 <= end_struct {
        let token_pos = pos;
        let token = be32(&dtb, pos);
        pos += 4;

        match token {
            FDT_BEGIN_NODE => {
                let name_start = pos;
                while pos < end_struct && dtb[pos] != 0 {
                    pos += 1;
                }
                if pos >= end_struct {
                    return Err(LoadError::DtbPatchFailed);
                }
                let name = core::str::from_utf8(&dtb[name_start..pos]).unwrap_or("");
                pos += 1;
                pos = (pos + 3) & !3;

                if names.contains(&name) {
                    let node_end = find_node_end(&dtb, pos, end_struct)?;
                    nop_struct_range(&mut dtb, token_pos, node_end);
                    patched += 1;
                    pos = node_end;
                }
            }
            FDT_PROP => {
                if pos + 8 > end_struct {
                    return Err(LoadError::DtbPatchFailed);
                }
                let len = be32(&dtb, pos) as usize;
                pos += 8 + ((len + 3) & !3);
                if pos > end_struct {
                    return Err(LoadError::DtbPatchFailed);
                }
            }
            FDT_END_NODE | FDT_NOP => {}
            FDT_END => break,
            _ => return Err(LoadError::DtbPatchFailed),
        }
    }

    if patched > 0 {
        write_guest_mem(guest_mem, dtb_gpa, &dtb)?;
        log::info!("[kvmm-loader] disabled {} DTB node(s)", patched);
    }

    Ok(patched)
}

fn find_node_end(dtb: &[u8], mut pos: usize, end_struct: usize) -> Result<usize, LoadError> {
    let mut depth = 1u32;

    while pos + 4 <= end_struct {
        let token = be32(dtb, pos);
        pos += 4;

        match token {
            FDT_BEGIN_NODE => {
                while pos < end_struct && dtb[pos] != 0 {
                    pos += 1;
                }
                if pos >= end_struct {
                    return Err(LoadError::DtbPatchFailed);
                }
                pos += 1;
                pos = (pos + 3) & !3;
                depth += 1;
            }
            FDT_PROP => {
                if pos + 8 > end_struct {
                    return Err(LoadError::DtbPatchFailed);
                }
                let len = be32(dtb, pos) as usize;
                pos += 8 + ((len + 3) & !3);
                if pos > end_struct {
                    return Err(LoadError::DtbPatchFailed);
                }
            }
            FDT_END_NODE => {
                depth -= 1;
                if depth == 0 {
                    return Ok(pos);
                }
            }
            FDT_NOP => {}
            FDT_END => return Err(LoadError::DtbPatchFailed),
            _ => return Err(LoadError::DtbPatchFailed),
        }
    }

    Err(LoadError::DtbPatchFailed)
}

fn nop_struct_range(dtb: &mut [u8], start: usize, end: usize) {
    for chunk in dtb[start..end].chunks_exact_mut(4) {
        chunk.copy_from_slice(&FDT_NOP.to_be_bytes());
    }
}

fn read_guest_mem<G: GuestMem>(guest_mem: &G, gpa: u64, buf: &mut [u8]) -> Result<(), LoadError> {
    for offset in (0..buf.len()).step_by(4096) {
        let chunk = core::cmp::min(4096, buf.len() - offset);
        let hpa = guest_mem
            .gpa_to_hpa(gpa + offset as u64)
            .ok_or(LoadError::AddressTranslation)?;
        let src = kaddr_layout::p2v(hpa as usize) as *const u8;
        // SAFETY: HPA from valid Stage-2 mapping; copy bounded by chunk size.
        unsafe {
            core::ptr::copy_nonoverlapping(src, buf[offset..].as_mut_ptr(), chunk);
        }
    }
    Ok(())
}

fn write_guest_mem<G: GuestMem>(guest_mem: &G, gpa: u64, buf: &[u8]) -> Result<(), LoadError> {
    for offset in (0..buf.len()).step_by(4096) {
        let chunk = core::cmp::min(4096, buf.len() - offset);
        let hpa = guest_mem
            .gpa_to_hpa(gpa + offset as u64)
            .ok_or(LoadError::AddressTranslation)?;
        let dst = kaddr_layout::p2v(hpa as usize) as *mut u8;
        // SAFETY: HPA from valid Stage-2 mapping; copy bounded by chunk size.
        unsafe {
            core::ptr::copy_nonoverlapping(buf[offset..].as_ptr(), dst, chunk);
        }
    }
    Ok(())
}
