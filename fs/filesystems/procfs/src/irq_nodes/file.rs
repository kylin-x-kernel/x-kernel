// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Stable IRQ affinity files over `SeqFileInode` and complete commands.

use alloc::{string::String, sync::Arc};
use core::fmt;

use kcpu_id_map::{
    KCpuMask,
    text::{
        CpuTextDomain, bitmap_capacity, list_capacity, parse_bitmap, parse_list, write_bitmap,
        write_list,
    },
};
use kerrno::LinuxError;
use kirq::IrqLineHandle;
use kvfs::{
    EmptyCommandPolicy, NodeFlags, NodePermission, SeekEndPolicy, SeqCommandOptions, SeqFileInode,
    SeqFileOptions, SeqIterator, SeqValidateFn, SimpleFs, VfsError, VfsFile, VfsInode, VfsResult,
};

use super::error;

pub(crate) const MAX_COMMAND_BYTES: usize = 128;
pub(crate) const AFFINITY_FILE_COUNT: usize = 4;
const REQUESTED_MODE: NodePermission = NodePermission::from_bits_truncate(0o644);
const READ_ONLY_MODE: NodePermission = NodePermission::from_bits_truncate(0o444);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AffinityFileKind {
    RequestedBitmap,
    RequestedList,
    EffectiveBitmap,
    EffectiveList,
}

impl AffinityFileKind {
    pub(crate) const ALL: [Self; AFFINITY_FILE_COUNT] = [
        Self::RequestedBitmap,
        Self::RequestedList,
        Self::EffectiveBitmap,
        Self::EffectiveList,
    ];

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::RequestedBitmap => "smp_affinity",
            Self::RequestedList => "smp_affinity_list",
            Self::EffectiveBitmap => "effective_affinity",
            Self::EffectiveList => "effective_affinity_list",
        }
    }

    const fn is_requested(self) -> bool {
        matches!(self, Self::RequestedBitmap | Self::RequestedList)
    }

    const fn is_list(self) -> bool {
        matches!(self, Self::RequestedList | Self::EffectiveList)
    }

    const fn mode(self, source_is_user_settable: bool) -> NodePermission {
        if self.is_requested() && source_is_user_settable {
            REQUESTED_MODE
        } else {
            READ_ONLY_MODE
        }
    }
}

struct AffinityIter {
    handle: Arc<IrqLineHandle>,
    kind: AffinityFileKind,
    domain: CpuTextDomain,
}

impl SeqIterator for AffinityIter {
    type Item = KCpuMask;

    fn rewind(&mut self) {}

    fn start(&mut self) -> Option<Self::Item> {
        self.try_start().ok().flatten()
    }

    fn next(&mut self) -> Option<Self::Item> {
        None
    }

    fn try_start(&mut self) -> VfsResult<Option<Self::Item>> {
        let snapshot = self
            .handle
            .affinity_snapshot()
            .map_err(error::map_affinity_read)?;
        let mask = if self.kind.is_requested() {
            snapshot.requested
        } else {
            snapshot.effective
        };
        match mask {
            Some(mask) => Ok(Some(mask)),
            None => Err(VfsError::from(LinuxError::ENODATA)),
        }
    }

    fn try_next(&mut self) -> VfsResult<Option<Self::Item>> {
        Ok(None)
    }

    fn show(&self, item: &Self::Item, buf: &mut String) -> fmt::Result {
        self.try_show(item, buf).map_err(|_| fmt::Error)
    }

    fn try_show(&self, item: &Self::Item, buf: &mut String) -> VfsResult<()> {
        let capacity = capacity(self.kind, &self.domain)?;
        buf.try_reserve(capacity.checked_add(1).ok_or(VfsError::NoMemory)?)
            .map_err(|_| VfsError::NoMemory)?;
        if self.kind.is_list() {
            write_list(*item, &self.domain, buf).map_err(crate::cpu_text_error::map_cpu_text)?;
        } else {
            write_bitmap(*item, &self.domain, buf).map_err(crate::cpu_text_error::map_cpu_text)?;
        }
        buf.push('\n');
        Ok(())
    }
}

fn validate_kind(
    handle: Arc<IrqLineHandle>,
    kind: AffinityFileKind,
    domain: CpuTextDomain,
) -> Option<SeqCommandOptions> {
    if !kind.is_requested() {
        return None;
    }
    let write_handle = handle.clone();
    Some(SeqCommandOptions {
        max_command_bytes: MAX_COMMAND_BYTES,
        validate_write: Some(Arc::new(move || {
            handle
                .check_user_affinity_change()
                .map_err(error::map_affinity_write)
        }) as Arc<SeqValidateFn>),
        write: Arc::new(move |_file: &VfsFile, data: &[u8]| {
            let mask = if kind.is_list() {
                parse_list(data, &domain).map_err(crate::cpu_text_error::map_cpu_text)?
            } else {
                parse_bitmap(data, &domain).map_err(crate::cpu_text_error::map_cpu_text)?
            };
            write_handle
                .set_user_affinity(&mask)
                .map_err(error::map_affinity_write)?;
            Ok(())
        }),
        empty: EmptyCommandPolicy::Callback,
    })
}

fn capacity(kind: AffinityFileKind, domain: &CpuTextDomain) -> VfsResult<usize> {
    if kind.is_list() {
        list_capacity(domain).map_err(crate::cpu_text_error::map_cpu_text)
    } else {
        bitmap_capacity(domain).map_err(crate::cpu_text_error::map_cpu_text)
    }
}

/// Creates the stable inode for one affinity file.
pub(crate) fn new_affinity_file_inode(
    fs: Arc<SimpleFs>,
    handle: Arc<IrqLineHandle>,
    kind: AffinityFileKind,
    source_is_user_settable: bool,
    domain: CpuTextDomain,
) -> VfsResult<Arc<VfsInode>> {
    let open_handle = handle.clone();
    let read_handle = handle.clone();
    let max_record_bytes = capacity(kind, &domain)?
        .checked_add(1)
        .ok_or(VfsError::NoMemory)?;
    let options = SeqFileOptions {
        permission: kind.mode(source_is_user_settable),
        lazy_open: true,
        max_record_bytes,
        reported_size: 0,
        seek_end: SeekEndPolicy::Reject,
        validate_open: Some(Arc::new(move || error::ensure_live(&open_handle))),
        validate_read: Some(Arc::new(move || error::ensure_live(&read_handle))),
        command: validate_kind(handle.clone(), kind, domain.clone()),
    };
    let iter_handle = handle;
    let seq = SeqFileInode::new_regular_with_options(
        fs,
        move || {
            Ok(AffinityIter {
                handle: iter_handle.clone(),
                kind,
                domain: domain.clone(),
            })
        },
        options,
    );
    let init = seq.inode_init();
    Ok(VfsInode::new_file_with_flags(
        seq,
        NodeFlags::NON_CACHEABLE,
        init,
    ))
}
