// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! User-memory boundary for native legacy IPv4 iptables queries.

use kerrno::{KError, KResult, LinuxError};
use knet::{
    ExtensionKind, IptGetEntries, IptGetInfo, LegacyTableReader, SocketOps, TABLE_NAME_LEN,
    XtGetRevision,
};
use osvm::VirtPtr;
use posix_types::UserPtr;
use zerocopy::{FromBytes, IntoBytes};

// IPv4-specific option numbers are absent from `linux_raw_sys`.
const IPT_BASE_CTL: u32 = 64;
const IPT_SO_GET_INFO: u32 = IPT_BASE_CTL;
const IPT_SO_GET_ENTRIES: u32 = IPT_BASE_CTL + 1;
const IPT_SO_GET_REVISION_MATCH: u32 = IPT_BASE_CTL + 2;
const IPT_SO_GET_REVISION_TARGET: u32 = IPT_BASE_CTL + 3;

/// Supported legacy IPv4 iptables query options.
#[derive(Clone, Copy)]
pub(super) enum LegacyGetOption {
    Info,
    Entries,
    Revision(ExtensionKind),
}

impl LegacyGetOption {
    pub(super) fn from_raw(option: u32) -> Option<Self> {
        match option {
            IPT_SO_GET_INFO => Some(Self::Info),
            IPT_SO_GET_ENTRIES => Some(Self::Entries),
            IPT_SO_GET_REVISION_MATCH => Some(Self::Revision(ExtensionKind::Match)),
            IPT_SO_GET_REVISION_TARGET => Some(Self::Revision(ExtensionKind::Target)),
            _ => None,
        }
    }

    pub(super) fn get(self, socket: &knet::Socket, value: UserPtr<u8>, len: usize) -> KResult<()> {
        if socket.socket_family() != linux_raw_sys::net::AF_INET as i32 {
            return Err(LinuxError::ENOPROTOOPT.into());
        }
        if !kprocess::current_cred().has_cap(kcred::CAP_NET_ADMIN) {
            return Err(LinuxError::EPERM.into());
        }
        match self {
            Self::Info => self.get_info(value, len),
            Self::Entries => self.get_entries(value, len),
            Self::Revision(kind) => self.get_revision(kind, value, len),
        }
    }

    fn get_info(self, value: UserPtr<u8>, len: usize) -> KResult<()> {
        if len != size_of::<IptGetInfo>() {
            return Err(KError::InvalidInput);
        }
        let name = value.cast::<[u8; TABLE_NAME_LEN]>().read_vm()?;
        let info = LegacyTableReader::lookup(&name)?.info()?;
        osvm::write_vm_bytes(value.as_ptr().cast_mut(), info.as_bytes())?;
        Ok(())
    }

    fn get_entries(self, value: UserPtr<u8>, len: usize) -> KResult<()> {
        const HEADER_SIZE: usize = size_of::<IptGetEntries>();
        if len < HEADER_SIZE {
            return Err(KError::InvalidInput);
        }
        let bytes = value.cast::<[u8; HEADER_SIZE]>().read_vm()?;
        let header = IptGetEntries::read_from_bytes(&bytes).map_err(|_| KError::InvalidInput)?;
        if HEADER_SIZE.checked_add(header.size as usize) != Some(len) {
            return Err(KError::InvalidInput);
        }
        let entries = LegacyTableReader::lookup(&header.name)?.export_entries(header.size)?;
        osvm::write_vm_bytes(
            value.as_ptr().wrapping_add(HEADER_SIZE).cast_mut(),
            entries.entries(),
        )?;
        Ok(())
    }

    fn get_revision(self, kind: ExtensionKind, value: UserPtr<u8>, len: usize) -> KResult<()> {
        const SIZE: usize = size_of::<XtGetRevision>();
        if len != SIZE {
            return Err(KError::InvalidInput);
        }
        let bytes = value.cast::<[u8; SIZE]>().read_vm()?;
        let query = XtGetRevision::read_from_bytes(&bytes).map_err(|_| KError::InvalidInput)?;
        kind.check_revision(&query.name, query.revision)
    }
}
