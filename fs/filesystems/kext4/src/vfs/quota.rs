// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Quota control callbacks and backing-file lifecycle under VFS admission.

use alloc::sync::Arc;

use kerrno::{KResult, LinuxError};
use kquota::QuotaType;
use kvfs::{SuperBlock, VfsInode};

use super::into_vfs_err;
use crate::{Ext4Inode, quota::Ext4QuotaIo};

impl kvfs::QuotaOperations for crate::vfs::Ext4SuperOperations {
    fn supported_types(&self, _sb: &SuperBlock) -> u8 {
        (1 << QuotaType::User as u8) | (1 << QuotaType::Group as u8)
    }

    fn has_system_files(&self, sb: &SuperBlock) -> bool {
        sb.private::<crate::vfs::Ext4Mount>()
            .is_ok_and(|mount| mount.read().has_quota_feature())
    }

    fn quota_on(
        &self,
        sb: &SuperBlock,
        kind: QuotaType,
        format_id: u32,
        file: &Arc<VfsInode>,
    ) -> KResult<()> {
        if sb.is_readonly() {
            return Err(LinuxError::EROFS.into());
        }
        let mount = sb.private::<crate::vfs::Ext4Mount>()?;
        if mount.read().quota_mount_options == 0 {
            return Err(LinuxError::EINVAL.into());
        }
        if !core::ptr::eq(file.super_block()?.as_ref(), sb) {
            return Err(LinuxError::EXDEV.into());
        }
        if file.node_type() != kvfs::NodeType::RegularFile {
            return Err(LinuxError::EACCES.into());
        }
        let inode = file.private::<Ext4Inode>()?;
        if inode.is_noquota() || sb.quota_info().info(kind).is_some() {
            return Err(LinuxError::EBUSY.into());
        }
        let format = kquota::QuotaFormat::from_id(format_id)?;
        let data_guard = file.lock_data();
        file.invalidate_cached_pages()?;
        sb.sync_for_quota()?;
        inode.set_noquota(true);
        inode.drop_quota_records(None);
        let result = (|| {
            let mut state = mount.write(sb);
            state.ensure_writable().map_err(into_vfs_err)?;
            // Drain mutations made since the VFS sync before discarding the
            // metadata cache shared with ordinary file writes.
            state.sync_filesystem().map_err(into_vfs_err)?;
            state.metadata_io.invalidate_all();
            // The selected file remains input until its header is accepted.
            // The read context cannot write or begin a filesystem mutation.
            sb.quota_info().enable(
                &mut Ext4QuotaIo {
                    filesystem: &state,
                    handle: None,
                },
                kind,
                file,
                format,
                kquota::QuotaState::USAGE_ENABLED | kquota::QuotaState::LIMITS_ENABLED,
            )?;
            state
                .set_quota_file_protection(inode, true)
                .map_err(into_vfs_err)
        })();
        if let Err(error) = result {
            drop(data_guard);
            if let Err(cleanup) = mount.disable_internal_quota(sb, kind) {
                warn!("KExt4 failed to undo quota activation after {error:?}: {cleanup:?}");
            }
            if sb.quota_info().info(kind).is_none() {
                inode.set_noquota(false);
                if file.write_count() != 0
                    && let Err(cleanup) = mount.write(sb).initialize_inode_quotas(inode)
                {
                    warn!("KExt4 failed to restore quota records after {error:?}: {cleanup:?}");
                }
            }
            return Err(error);
        }
        // Existing writable descriptors can allocate without reopening their
        // inodes. Initialize them before ordinary write admission resumes.
        let initialized = sb.for_each_inode(|live| {
            if live.write_count() != 0 {
                mount
                    .write(sb)
                    .initialize_inode_quotas(live.private()?)
                    .map_err(into_vfs_err)?;
            }
            Ok(())
        });
        drop(data_guard);
        if initialized.is_err() {
            let cleanup = self.quota_off(sb, kind);
            initialized.and(cleanup)?;
        }
        Ok(())
    }

    fn quota_off(&self, sb: &SuperBlock, kind: QuotaType) -> KResult<()> {
        if sb.quota_info().info(kind).is_none() {
            return Err(LinuxError::ESRCH.into());
        }
        if !sb.is_readonly() {
            sb.sync_for_quota()?;
        }
        let mount = sb.private::<crate::vfs::Ext4Mount>()?;
        mount.disable_internal_quota(sb, kind).map_err(into_vfs_err)
    }

    fn sync(&self, sb: &SuperBlock, kind: QuotaType) -> KResult<()> {
        if !sb.is_readonly() {
            sb.sync_for_quota()?;
            if !self.has_system_files(sb) {
                let file = sb.quota_info().info(kind).map(|info| info.file().clone());
                if let Some(file) = file {
                    file.invalidate_cached_pages()?;
                }
            }
        }
        Ok(())
    }

    fn get(&self, sb: &SuperBlock, id: kquota::QuotaId) -> KResult<kquota::Dqblk> {
        sb.private::<crate::vfs::Ext4Mount>()?
            .write(sb)
            .quota_record_operation(sb, id, |quota, io| quota.get(io, id))
    }

    fn next(
        &self,
        sb: &SuperBlock,
        kind: QuotaType,
        minimum: u32,
    ) -> KResult<(kquota::QuotaId, kquota::Dqblk)> {
        let mut state = sb.private::<crate::vfs::Ext4Mount>()?.write(sb);
        let id = state.read_quota_record(sb, |quota, io| {
            quota
                .next_id(io, kind, minimum)?
                .ok_or_else(|| LinuxError::ENOENT.into())
        })?;
        state.quota_record_operation(sb, id, |quota, io| Ok((id, quota.get(io, id)?)))
    }

    fn set(
        &self,
        sb: &SuperBlock,
        id: kquota::QuotaId,
        value: kquota::Dqblk,
        fields: kquota::QuotaFields,
    ) -> KResult<()> {
        if sb.is_readonly() {
            return Err(LinuxError::EROFS.into());
        }
        sb.private::<crate::vfs::Ext4Mount>()?
            .write(sb)
            .quota_record_operation(sb, id, |quota, io| {
                quota.set(io, id, value, fields, ktime::realtime().unix_seconds())?;
                quota.sync_dquot(io, id)
            })
    }

    fn set_grace_times(
        &self,
        sb: &SuperBlock,
        kind: QuotaType,
        space_seconds: u32,
        inode_seconds: u32,
    ) -> KResult<()> {
        if sb.is_readonly() {
            return Err(LinuxError::EROFS.into());
        }
        let mut state = sb.private::<crate::vfs::Ext4Mount>()?.write(sb);
        state.sync_quota_item(&mut sb.quota_info(), |quota, io| {
            quota.set_grace_times(io, kind, space_seconds, inode_seconds)
        })
    }
}

/// Authorizes bypass only for a current caller carrying the resource capability.
/// Kernel writeback consumes reservations admitted earlier and acquires no
/// independent user privilege for additional unreserved allocation.
pub(crate) fn can_bypass_quota_limits() -> bool {
    use kprocess::AsThread;

    ktask::current().try_as_thread().is_some() && kprocess::current_cred().has_sys_resource()
}
