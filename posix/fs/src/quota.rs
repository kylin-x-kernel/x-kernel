// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Quota command decoding, authorization and user-memory conversion.

use alloc::sync::Arc;
use core::ffi::c_char;

use kcred::Cred;
use kerrno::{KResult, LinuxError};
use kquota::{Dqblk, QuotaFields, QuotaId, QuotaState, QuotaType};
use kvfs::{NodeType, QuotaOperations, SuperBlock, VfsInode};
use linux_raw_sys::general::AT_FDCWD;
use posix_types::{
    UserConstPtr, UserPtr,
    quota::{IfDqblk, IfDqinfo, IfNextDqblk},
};

use crate::path::resolve_at_with_cred;

mod xfs;

use xfs::*;

const Q_SYNC: u32 = 0x800001;
const Q_QUOTAON: u32 = 0x800002;
const Q_QUOTAOFF: u32 = 0x800003;
const Q_GETFMT: u32 = 0x800004;
const Q_GETINFO: u32 = 0x800005;
const Q_SETINFO: u32 = 0x800006;
const Q_GETQUOTA: u32 = 0x800007;
const Q_SETQUOTA: u32 = 0x800008;
const Q_GETNEXTQUOTA: u32 = 0x800009;
const QIF_ALL: u32 = 63;
const IIF_ALL: u32 = 7;
const DQF_SYS_FILE: u32 = 1 << 16;

fn quota_type(command: u32) -> KResult<QuotaType> {
    QuotaType::ALL
        .get((command & 0xff) as usize)
        .copied()
        .ok_or(LinuxError::EINVAL.into())
}

fn check_permission(cred: &Cred, command: u32, kind: QuotaType, id: u32) -> KResult<()> {
    let is_own_record = match kind {
        QuotaType::User => id == cred.euid(),
        QuotaType::Group => id == cred.egid() || cred.supplementary_groups().contains(&id),
        QuotaType::Project => false,
    };
    if matches!(
        command,
        Q_SYNC | Q_GETFMT | Q_GETINFO | Q_XQUOTASYNC | Q_XGETQSTAT | Q_XGETQSTATV
    ) || (matches!(command, Q_GETQUOTA | Q_XGETQUOTA) && is_own_record)
        || cred.has_sys_admin()
    {
        Ok(())
    } else {
        Err(LinuxError::EPERM.into())
    }
}

fn resolve_name(name: UserConstPtr<c_char>, cred: &Cred) -> KResult<kvfs::Path> {
    let name = name.load_string()?;
    resolve_at_with_cred(AT_FDCWD, Some(&name), 0, cred)?.into_path()
}

/// Controls quotas on the filesystem mounted from a block-device pathname.
/// A null pathname with Q_SYNC synchronizes all supporting mounted filesystems.
pub fn sys_quotactl(
    command: u32,
    special: UserConstPtr<c_char>,
    id: u32,
    address: usize,
) -> KResult<isize> {
    let kind = quota_type(command)?;
    let operation = command >> 8;
    if special.is_null() {
        if operation != Q_SYNC {
            return Err(LinuxError::ENODEV.into());
        }
        for sb in kvfs::super_block_registry().live_super_blocks() {
            let _ = sb.quota_control(kind, false, |ops| ops.sync(&sb, kind));
        }
        return Ok(0);
    }
    let cred = kprocess::current_cred();
    let path = resolve_name(special, &cred)?;
    if path.inode().node_type() != NodeType::BlockDevice {
        return Err(LinuxError::ENOTBLK.into());
    }
    let device = path.inode().rdev();
    let sb = kvfs::super_block_registry()
        .live_super_blocks()
        .into_iter()
        .find(|sb| {
            sb.block_device()
                .is_some_and(|block| block.device_number() == device)
        })
        .ok_or(LinuxError::ENODEV)?;
    let file = if operation == Q_QUOTAON {
        Some(resolve_name(address.into(), &cred).map(|path| path.inode().clone()))
    } else {
        None
    };
    control(&sb, &cred, operation, kind, id, address, file.as_ref())?;
    Ok(0)
}

/// Controls quotas through any open file on the target filesystem.
pub fn sys_quotactl_fd(fd: i32, command: u32, id: u32, address: usize) -> KResult<isize> {
    let file = kprocess::current_files().get_file(fd)?;
    let kind = quota_type(command)?;
    let sb = file.path().mount().super_block();
    let operation = command >> 8;
    if !matches!(
        operation,
        Q_SYNC
            | Q_GETFMT
            | Q_GETINFO
            | Q_GETQUOTA
            | Q_GETNEXTQUOTA
            | Q_XQUOTASYNC
            | Q_XGETQSTAT
            | Q_XGETQSTATV
            | Q_XGETQUOTA
            | Q_XGETNEXTQUOTA
    ) && sb.is_readonly()
    {
        return Err(LinuxError::EROFS.into());
    }
    control(
        sb,
        &kprocess::current_cred(),
        operation,
        kind,
        id,
        address,
        None,
    )?;
    Ok(0)
}

fn control(
    sb: &SuperBlock,
    cred: &Cred,
    command: u32,
    kind: QuotaType,
    id: u32,
    address: usize,
    file: Option<&KResult<Arc<VfsInode>>>,
) -> KResult<()> {
    let is_exclusive = matches!(command, Q_QUOTAON | Q_QUOTAOFF | Q_XQUOTAON | Q_XQUOTAOFF);
    sb.quota_control(kind, is_exclusive, |ops| {
        check_permission(cred, command, kind, id)?;
        match command {
            Q_SYNC => ops.sync(sb, kind),
            Q_QUOTAON => quota_on(sb, ops, kind, id, file),
            Q_QUOTAOFF if ops.has_system_files(sb) => ops.quota_disable(sb, 2 << (kind as u32 * 2)),
            Q_QUOTAOFF => ops.quota_off(sb, kind),
            Q_GETFMT => {
                let format = {
                    let quota = sb.quota_info();
                    let info = quota
                        .info(kind)
                        .filter(|info| !info.state().contains(QuotaState::SUSPENDED))
                        .ok_or(LinuxError::ESRCH)?;
                    info.format().id()
                };
                UserPtr::<u32>::from(address)
                    .write_vm(format)
                    .map_err(Into::into)
            }
            Q_GETINFO => {
                let has_system_files = ops.has_system_files(sb);
                let value = {
                    let quota = sb.quota_info();
                    let info = quota
                        .info(kind)
                        .filter(|info| !info.state().contains(QuotaState::SUSPENDED))
                        .ok_or(LinuxError::ESRCH)?;
                    IfDqinfo {
                        block_grace: u64::from(info.space_grace_seconds()),
                        inode_grace: u64::from(info.inode_grace_seconds()),
                        flags: if has_system_files { DQF_SYS_FILE } else { 0 },
                        valid: IIF_ALL,
                    }
                };
                UserPtr::<IfDqinfo>::from(address)
                    .write_vm(value)
                    .map_err(Into::into)
            }
            Q_SETINFO => {
                let value = UserConstPtr::<IfDqinfo>::from(address).read_vm()?;
                if value.valid & !IIF_ALL != 0 || value.valid & 4 != 0 && value.flags != 0 {
                    return Err(LinuxError::EINVAL.into());
                }
                let (space, inodes) = {
                    let quota = sb.quota_info();
                    let info = quota.info(kind).ok_or(LinuxError::ESRCH)?;
                    (info.space_grace_seconds(), info.inode_grace_seconds())
                };
                let space = if value.valid & 1 != 0 {
                    u32::try_from(value.block_grace).map_err(|_| LinuxError::EINVAL)?
                } else {
                    space
                };
                let inodes = if value.valid & 2 != 0 {
                    u32::try_from(value.inode_grace).map_err(|_| LinuxError::EINVAL)?
                } else {
                    inodes
                };
                ops.set_grace_times(sb, kind, space, inodes)
            }
            Q_GETQUOTA => {
                let value = ops.get(sb, QuotaId::new(kind, id)?)?;
                UserPtr::<IfDqblk>::from(address)
                    .write_vm(to_abi(value)?)
                    .map_err(Into::into)
            }
            Q_SETQUOTA => {
                let value = UserConstPtr::<IfDqblk>::from(address).read_vm()?;
                let (value, fields) = from_abi(value)?;
                ops.set(sb, QuotaId::new(kind, id)?, value, fields)
            }
            Q_GETNEXTQUOTA => {
                let (id, value) = ops.next(sb, kind, id)?;
                let value = to_abi(value)?;
                UserPtr::<IfNextDqblk>::from(address)
                    .write_vm(IfNextDqblk {
                        block_hard_limit: value.block_hard_limit,
                        block_soft_limit: value.block_soft_limit,
                        current_space: value.current_space,
                        inode_hard_limit: value.inode_hard_limit,
                        inode_soft_limit: value.inode_soft_limit,
                        current_inodes: value.current_inodes,
                        block_time: value.block_time,
                        inode_time: value.inode_time,
                        valid: value.valid,
                        id: id.id(),
                    })
                    .map_err(Into::into)
            }
            _ => xfs::control(sb, ops, command, kind, id, address),
        }
    })
}

fn quota_on(
    sb: &SuperBlock,
    ops: &dyn QuotaOperations,
    kind: QuotaType,
    format_id: u32,
    file: Option<&KResult<Arc<VfsInode>>>,
) -> KResult<()> {
    if ops.has_system_files(sb) {
        return ops.quota_enable(sb, 2 << (kind as u32 * 2));
    }
    let file = file
        .ok_or(LinuxError::EINVAL)?
        .as_ref()
        .map_err(|error| *error)?;
    ops.quota_on(sb, kind, format_id, file)
}

fn to_abi(value: Dqblk) -> KResult<IfDqblk> {
    Ok(IfDqblk {
        block_hard_limit: (value.space_hard_limit_bytes as u64).div_ceil(1024),
        block_soft_limit: (value.space_soft_limit_bytes as u64).div_ceil(1024),
        current_space: value.total_bytes()? as u64,
        inode_hard_limit: value.inode_hard_limit as u64,
        inode_soft_limit: value.inode_soft_limit as u64,
        current_inodes: value.used_inodes as u64,
        block_time: value.space_expires_at as u64,
        inode_time: value.inode_expires_at as u64,
        valid: QIF_ALL,
        abi_padding: 0,
    })
}

fn from_abi(value: IfDqblk) -> KResult<(Dqblk, QuotaFields)> {
    let mut data = Dqblk::default();
    let mut fields = QuotaFields::empty();
    for (mask, field, input, target, shift) in [
        (
            1,
            QuotaFields::SPACE_HARD,
            value.block_hard_limit,
            &mut data.space_hard_limit_bytes,
            10,
        ),
        (
            1,
            QuotaFields::SPACE_SOFT,
            value.block_soft_limit,
            &mut data.space_soft_limit_bytes,
            10,
        ),
        (
            2,
            QuotaFields::SPACE_USED,
            value.current_space,
            &mut data.used_bytes,
            0,
        ),
        (
            4,
            QuotaFields::INODE_HARD,
            value.inode_hard_limit,
            &mut data.inode_hard_limit,
            0,
        ),
        (
            4,
            QuotaFields::INODE_SOFT,
            value.inode_soft_limit,
            &mut data.inode_soft_limit,
            0,
        ),
        (
            8,
            QuotaFields::INODES,
            value.current_inodes,
            &mut data.used_inodes,
            0,
        ),
        (
            16,
            QuotaFields::SPACE_TIME,
            value.block_time,
            &mut data.space_expires_at,
            0,
        ),
        (
            32,
            QuotaFields::INODE_TIME,
            value.inode_time,
            &mut data.inode_expires_at,
            0,
        ),
    ] {
        if value.valid & mask != 0 {
            if input > (i64::MAX as u64 >> shift) {
                return Err(LinuxError::ERANGE.into());
            }
            *target = (input << shift) as i64;
            fields.insert(field);
        }
    }
    Ok((data, fields))
}

unittest::path_mod_test!(tests, "../ut-tests/quota.rs");
