// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! User-memory handlers for native legacy IPv4 iptables socket options.

use kerrno::{KError, KResult, LinuxError};
use knet::{
    ExtensionKind, IptGetEntries, IptGetInfo, IptReplace, LegacyTableHandle, SocketOps,
    TABLE_NAME_LEN, XtCounters, XtCountersInfo, XtGetRevision,
};
use osvm::VirtPtr;
use posix_types::{UserConstPtr, UserPtr};
use zerocopy::{FromBytes, IntoBytes};

// IPv4-specific option numbers are absent from `linux_raw_sys`.
const IPT_BASE_CTL: u32 = 64;
const IPT_SO_GET_INFO: u32 = IPT_BASE_CTL;
const IPT_SO_GET_ENTRIES: u32 = IPT_BASE_CTL + 1;
const IPT_SO_GET_REVISION_MATCH: u32 = IPT_BASE_CTL + 2;
const IPT_SO_GET_REVISION_TARGET: u32 = IPT_BASE_CTL + 3;
const IPT_SO_SET_REPLACE: u32 = IPT_BASE_CTL;
const IPT_SO_SET_ADD_COUNTERS: u32 = IPT_BASE_CTL + 1;

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
        check_access(socket)?;
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
        let info = LegacyTableHandle::lookup(&name)?.info()?;
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
        let entries = LegacyTableHandle::lookup(&header.name)?.export_entries(header.size)?;
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

/// Supported legacy IPv4 iptables write options.
#[derive(Clone, Copy)]
pub(super) enum LegacySetOption {
    Replace,
    AddCounters,
}

impl LegacySetOption {
    pub(super) fn from_raw(option: u32) -> Option<Self> {
        match option {
            IPT_SO_SET_REPLACE => Some(Self::Replace),
            IPT_SO_SET_ADD_COUNTERS => Some(Self::AddCounters),
            _ => None,
        }
    }

    pub(super) fn set(
        self,
        socket: &knet::Socket,
        value: UserConstPtr<u8>,
        len: usize,
    ) -> KResult<()> {
        check_access(socket)?;
        match self {
            Self::Replace => self.replace(value, len),
            Self::AddCounters => self.add_counters(value, len),
        }
    }

    fn replace(self, value: UserConstPtr<u8>, len: usize) -> KResult<()> {
        const HEADER_SIZE: usize = size_of::<IptReplace>();
        if len < HEADER_SIZE {
            return Err(KError::InvalidInput);
        }
        let bytes = value.cast::<[u8; HEADER_SIZE]>().read_vm()?;
        let header = IptReplace::read_from_bytes(&bytes).map_err(|_| KError::InvalidInput)?;
        let entries_size_bytes = header.entries_size_bytes(len)?;
        let entries = osvm::load_vec(value.as_ptr().wrapping_add(HEADER_SIZE), entries_size_bytes)?;
        let table = LegacyTableHandle::lookup(&header.name)?;
        let counters = table.replace(header.replacement(&entries))?;
        // The new table is already active, so a failed counter copy back to user memory is ignored.
        let _ = osvm::write_vm_bytes(header.counters as *mut u8, counters.as_slice().as_bytes());
        Ok(())
    }

    fn add_counters(self, value: UserConstPtr<u8>, len: usize) -> KResult<()> {
        const HEADER_SIZE: usize = size_of::<XtCountersInfo>();
        if len <= HEADER_SIZE {
            return Err(KError::InvalidInput);
        }
        let bytes = value.cast::<[u8; HEADER_SIZE]>().read_vm()?;
        let header = XtCountersInfo::read_from_bytes(&bytes).map_err(|_| KError::InvalidInput)?;
        let counters_size_bytes = header.counters_size_bytes(len)?;
        let bytes = osvm::load_vec(
            value.as_ptr().wrapping_add(HEADER_SIZE),
            counters_size_bytes,
        )?;
        let counters = bytes
            .chunks_exact(size_of::<XtCounters>())
            .map(|bytes| XtCounters::read_from_bytes(bytes).map_err(|_| KError::InvalidInput))
            .collect::<KResult<alloc::vec::Vec<_>>>()?;
        LegacyTableHandle::lookup(&header.name)?.add_counters(&counters)
    }
}

fn check_access(socket: &knet::Socket) -> KResult<()> {
    if socket.socket_family() != linux_raw_sys::net::AF_INET as i32 {
        return Err(LinuxError::ENOPROTOOPT.into());
    }
    if !crate::net_capable(&kprocess::current_cred(), kcred::CAP_NET_ADMIN) {
        return Err(LinuxError::EPERM.into());
    }
    Ok(())
}
