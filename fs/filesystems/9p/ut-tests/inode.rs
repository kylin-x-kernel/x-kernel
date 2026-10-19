// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::boxed::Box;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use p9::{P9Client, P9Transport};
use unittest::{assert_eq, def_test};

use super::*;

#[derive(Clone, Default)]
struct MetadataTransport {
    requests: Arc<AtomicUsize>,
    setattr_valid: Arc<AtomicU32>,
    should_fail: Arc<AtomicBool>,
}

impl P9Transport for MetadataTransport {
    fn request(&mut self, request: &[u8], response: &mut [u8]) -> VfsResult<usize> {
        self.requests.fetch_add(1, Ordering::Relaxed);
        response[5..7].copy_from_slice(&request[5..7]);
        let size: u32 = match request[4] {
            100 => {
                response[4] = 101; // RVERSION
                response[7..11].copy_from_slice(&8192u32.to_le_bytes());
                response[11..13].copy_from_slice(&8u16.to_le_bytes());
                response[13..21].copy_from_slice(b"9P2000.L");
                21
            }
            104 => {
                response[4] = 105; // RATTACH, regular-file qid
                response[7..20].fill(0);
                response[12..20].copy_from_slice(&1u64.to_le_bytes());
                20
            }
            26 => {
                self.setattr_valid.store(
                    u32::from_le_bytes(request[11..15].try_into().unwrap()),
                    Ordering::Relaxed,
                );
                if self.should_fail.load(Ordering::Relaxed) {
                    return Err(VfsError::Io);
                }
                response[4] = 27; // RSETATTR
                7
            }
            120 => {
                response[4] = 121; // RCLUNK
                7
            }
            _ => return Err(VfsError::InvalidInput),
        };
        response[..4].copy_from_slice(&size.to_le_bytes());
        Ok(size as usize)
    }
}

fn metadata_fixture() -> (MetadataTransport, Arc<V9fsInode>, Dentry) {
    let transport = MetadataTransport::default();
    let client = P9Client::new(Box::new(transport.clone()));
    let node = V9fsInode::new(client.negotiate().unwrap());
    let inode = node.clone().into_vfs_inode(VfsInodeInit::new(
        3,
        16,
        Umode::new(
            NodeType::RegularFile,
            NodePermission::from_bits_truncate(0o600),
        ),
    ));
    let dentry = Dentry::new_file_from_inode(inode, None, String::from("timestamp"));
    (transport, node, dentry)
}

#[def_test]
fn automatic_timestamps_do_not_issue_remote_setattr() {
    let (transport, node, dentry) = metadata_fixture();
    let requests = transport.requests.load(Ordering::Relaxed);
    // Automatic timestamps must remain local even if remote setattr fails.
    transport.should_fail.store(true, Ordering::Relaxed);
    let timestamp = SystemTime::from_unix_seconds(123);
    let applied = node
        .update_time(&MountIdmap, &dentry, timestamp, InodeUpdateTime::Access)
        .unwrap();
    assert_eq!(applied.atime, Some(timestamp));
    assert_eq!(applied.mtime, None);
    assert_eq!(applied.ctime, None);
    let applied = node
        .update_time(
            &MountIdmap,
            &dentry,
            timestamp,
            InodeUpdateTime::ChangeAndModification,
        )
        .unwrap();
    assert_eq!(applied.atime, None);
    assert_eq!(applied.mtime, Some(timestamp));
    assert_eq!(applied.ctime, Some(timestamp));
    assert_eq!(transport.requests.load(Ordering::Relaxed), requests);
}

#[def_test]
fn explicit_setattr_does_not_publish_unconfirmed_ctime() {
    let (transport, _node, dentry) = metadata_fixture();
    let initial_ctime = dentry.inode_ref().metadata().ctime;
    let timestamp = SystemTime::from_unix_seconds(456);
    dentry
        .update_metadata(MetadataUpdate {
            size: Some(0),
            atime: Some(timestamp),
            mtime: Some(timestamp),
            ctime: Some(timestamp),
            ..Default::default()
        })
        .unwrap();
    // SIZE, ATIME, MTIME, CTIME, ATIME_SET, MTIME_SET.
    assert_eq!(transport.setattr_valid.load(Ordering::Relaxed), 0x1f8);
    let metadata = dentry.inode_ref().metadata();
    assert_eq!(metadata.size, 0);
    assert_eq!(metadata.atime, timestamp);
    assert_eq!(metadata.mtime, timestamp);
    assert_eq!(metadata.ctime, initial_ctime);

    transport.should_fail.store(true, Ordering::Relaxed);
    assert_eq!(
        dentry.update_metadata(MetadataUpdate {
            size: Some(8),
            atime: Some(SystemTime::from_unix_seconds(789)),
            ..Default::default()
        }),
        Err(VfsError::Io)
    );
    let metadata = dentry.inode_ref().metadata();
    assert_eq!(metadata.size, 0);
    assert_eq!(metadata.atime, timestamp);
    assert_eq!(metadata.ctime, initial_ctime);
}
