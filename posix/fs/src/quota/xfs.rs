// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Extended quota ABI conversions shared by quota tools on all filesystems.

use kerrno::{KResult, LinuxError};
use kquota::{Dqblk, QuotaFields, QuotaId, QuotaState, QuotaType};
use kvfs::{QuotaOperations, SuperBlock};
use posix_types::{
    UserConstPtr, UserPtr,
    quota::{FsDiskQuota, FsQFileStat, FsQuotaStat, FsQuotaStatV},
};

pub(super) const Q_XQUOTAON: u32 = 0x5801;
pub(super) const Q_XQUOTAOFF: u32 = 0x5802;
pub(super) const Q_XGETQUOTA: u32 = 0x5803;
pub(super) const Q_XSETQLIM: u32 = 0x5804;
pub(super) const Q_XGETQSTAT: u32 = 0x5805;
pub(super) const Q_XQUOTARM: u32 = 0x5806;
pub(super) const Q_XQUOTASYNC: u32 = 0x5807;
pub(super) const Q_XGETQSTATV: u32 = 0x5808;
pub(super) const Q_XGETNEXTQUOTA: u32 = 0x5809;

pub(super) fn control(
    sb: &SuperBlock,
    ops: &dyn QuotaOperations,
    command: u32,
    kind: QuotaType,
    id: u32,
    address: usize,
) -> KResult<()> {
    match command {
        // These commands require optional filesystem callbacks for toggling
        // system-file accounting or removing quota storage.
        Q_XQUOTAON | Q_XQUOTAOFF | Q_XQUOTARM => {
            let _flags = UserConstPtr::<u32>::from(address).read_vm()?;
            Err(LinuxError::ENOSYS.into())
        }
        Q_XQUOTASYNC => ops.sync(sb, kind),
        Q_XGETQSTAT | Q_XGETQSTATV => {
            if command == Q_XGETQSTATV && UserConstPtr::<i8>::from(address).read_vm()? != 1 {
                return Err(LinuxError::EINVAL.into());
            }
            let stat = state(sb, kind)?;
            if command == Q_XGETQSTATV {
                UserPtr::from(address).write_vm(stat)?;
            } else {
                UserPtr::from(address).write_vm(FsQuotaStat {
                    version: stat.version,
                    flags: stat.flags,
                    user: stat.user,
                    group: if stat.group.inode != 0 {
                        stat.group
                    } else {
                        stat.project
                    },
                    in_core_records: stat.in_core_records,
                    block_grace: stat.block_grace,
                    inode_grace: stat.inode_grace,
                    ..FsQuotaStat::default()
                })?;
            }
            Ok(())
        }
        Q_XGETQUOTA | Q_XGETNEXTQUOTA => {
            let (qid, data) = if command == Q_XGETNEXTQUOTA {
                ops.next(sb, kind, id)?
            } else {
                let qid = QuotaId::new(kind, id)?;
                (qid, ops.get(sb, qid)?)
            };
            UserPtr::from(address).write_vm(to_abi(qid, data)?)?;
            Ok(())
        }
        Q_XSETQLIM => {
            let mut data = UserConstPtr::<FsDiskQuota>::from(address).read_vm()?;
            if id == 0 && data.fieldmask & (0x1c0 | 0xe00) != 0 {
                if data.fieldmask & (0x100 | 0xe00) != 0 {
                    return Err(LinuxError::EINVAL.into());
                }
                let (space, inodes) = {
                    let quota = sb.quota_info();
                    let info = quota.info(kind).ok_or(LinuxError::ESRCH)?;
                    (info.space_grace_seconds(), info.inode_grace_seconds())
                };
                let space = if data.fieldmask & 0x40 != 0 {
                    u32::try_from(timer(data.block_timer, data.block_timer_hi, data.fieldmask))
                        .map_err(|_| LinuxError::EINVAL)?
                } else {
                    space
                };
                let inodes = if data.fieldmask & 0x80 != 0 {
                    u32::try_from(timer(data.inode_timer, data.inode_timer_hi, data.fieldmask))
                        .map_err(|_| LinuxError::EINVAL)?
                } else {
                    inodes
                };
                ops.set_grace_times(sb, kind, space, inodes)?;
                data.fieldmask &= !(0x1c0 | 0xe00);
            }
            let (value, fields) = from_abi(data)?;
            ops.set(sb, QuotaId::new(kind, id)?, value, fields)
        }
        _ => Err(LinuxError::EINVAL.into()),
    }
}

fn state(sb: &SuperBlock, selected: QuotaType) -> KResult<FsQuotaStatV> {
    let (mut result, files) = {
        let quota = sb.quota_info();
        let mut result = FsQuotaStatV {
            version: 1,
            in_core_records: quota.cached_records() as u32,
            ..FsQuotaStatV::default()
        };
        let files = QuotaType::ALL.map(|kind| {
            let info = quota
                .info(kind)
                .filter(|info| !info.state().contains(QuotaState::SUSPENDED))?;
            result.flags |= 1 << (kind as u8 * 2);
            if info.state().contains(QuotaState::LIMITS_ENABLED) {
                result.flags |= 2 << (kind as u8 * 2);
            }
            if kind == selected {
                result.block_grace = info.space_grace_seconds() as i32;
                result.inode_grace = info.inode_grace_seconds() as i32;
            }
            Some(info.file().clone())
        });
        (result, files)
    };
    if result.flags == 0 {
        return Err(LinuxError::ENOSYS.into());
    }
    for (file, target) in
        files
            .iter()
            .zip([&mut result.user, &mut result.group, &mut result.project])
    {
        if let Some(file) = file {
            *target = FsQFileStat {
                inode: file.inode(),
                blocks: file.metadata().blocks,
                extents: 1,
                padding: 0,
            };
        }
    }
    Ok(result)
}

fn timer(low: i32, high: i8, fields: u16) -> i64 {
    if fields & 0x8000 != 0 {
        i64::from(low as u32) | (i64::from(high) << 32)
    } else {
        i64::from(low)
    }
}

fn to_abi(id: QuotaId, data: Dqblk) -> KResult<FsDiskQuota> {
    let is_big_time = i32::try_from(data.space_expires_at).is_err()
        || i32::try_from(data.inode_expires_at).is_err();
    Ok(FsDiskQuota {
        version: 1,
        flags: match id.kind() {
            QuotaType::User => 1,
            QuotaType::Group => 4,
            QuotaType::Project => 2,
        },
        fieldmask: if is_big_time { 0x8000 } else { 0 },
        id: id.id(),
        block_hard_limit: (data.space_hard_limit_bytes as u64).div_ceil(512),
        block_soft_limit: (data.space_soft_limit_bytes as u64).div_ceil(512),
        inode_hard_limit: data.inode_hard_limit as u64,
        inode_soft_limit: data.inode_soft_limit as u64,
        block_count: (data.total_bytes()? as u64).div_ceil(512),
        inode_count: data.used_inodes as u64,
        inode_timer: data.inode_expires_at as i32,
        block_timer: data.space_expires_at as i32,
        inode_timer_hi: if is_big_time {
            (data.inode_expires_at >> 32) as i8
        } else {
            0
        },
        block_timer_hi: if is_big_time {
            (data.space_expires_at >> 32) as i8
        } else {
            0
        },
        ..FsDiskQuota::default()
    })
}

fn from_abi(value: FsDiskQuota) -> KResult<(Dqblk, QuotaFields)> {
    if value.fieldmask & (0x30 | 0x100 | 0xe00 | 0x4000) != 0 {
        return Err(LinuxError::EINVAL.into());
    }
    let mut data = Dqblk::default();
    let mut fields = QuotaFields::empty();
    for (mask, field, input, target, shift) in [
        (
            1,
            QuotaFields::INODE_SOFT,
            value.inode_soft_limit,
            &mut data.inode_soft_limit,
            0,
        ),
        (
            2,
            QuotaFields::INODE_HARD,
            value.inode_hard_limit,
            &mut data.inode_hard_limit,
            0,
        ),
        (
            4,
            QuotaFields::SPACE_SOFT,
            value.block_soft_limit,
            &mut data.space_soft_limit_bytes,
            9,
        ),
        (
            8,
            QuotaFields::SPACE_HARD,
            value.block_hard_limit,
            &mut data.space_hard_limit_bytes,
            9,
        ),
        (
            0x1000,
            QuotaFields::SPACE_USED,
            value.block_count,
            &mut data.used_bytes,
            9,
        ),
        (
            0x2000,
            QuotaFields::INODES,
            value.inode_count,
            &mut data.used_inodes,
            0,
        ),
    ] {
        if value.fieldmask & mask != 0 {
            if input > i64::MAX as u64 >> shift {
                return Err(LinuxError::ERANGE.into());
            }
            *target = (input << shift) as i64;
            fields.insert(field);
        }
    }
    if value.fieldmask & 0x40 != 0 {
        fields.insert(QuotaFields::SPACE_TIME);
        data.space_expires_at = timer(value.block_timer, value.block_timer_hi, value.fieldmask);
    }
    if value.fieldmask & 0x80 != 0 {
        fields.insert(QuotaFields::INODE_TIME);
        data.inode_expires_at = timer(value.inode_timer, value.inode_timer_hi, value.fieldmask);
    }
    Ok((data, fields))
}
