// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};

use kalloc_types::KBox;
use kvfs::{NodeFlags, NodePermission, Umode, VfsInodeInit};
use p9::{P9Client, P9Transport};
use unittest::{assert_eq, def_test};

use super::*;

#[def_test]
fn folio_window_clamps_to_the_file_end() {
    // Whole folios, then the partial folio holding the last bytes, then the
    // zero-filled folios past EOF.
    assert_eq!(folio_window(10_000, 0), Some((0, 4096)));
    assert_eq!(folio_window(10_000, 1), Some((4096, 4096)));
    assert_eq!(folio_window(10_000, 2), Some((8192, 1808)));
    assert_eq!(folio_window(10_000, 3), Some((12_288, 0)));
    assert_eq!(folio_window(0, 0), Some((0, 0)));
    assert_eq!(folio_window(10_000, u64::MAX), None);
}

#[def_test]
fn open_flags_follow_linux_dotl_mapping() {
    assert_eq!(
        p9_open_flags(OpenFlags::APPEND | OpenFlags::SYNC, false),
        P9OpenFlags::APPEND | P9OpenFlags::DSYNC | P9OpenFlags::SYNC
    );
    assert_eq!(
        p9_open_flags(OpenFlags::TRUNCATE, false),
        P9OpenFlags::empty()
    );
    assert_eq!(
        p9_open_flags(OpenFlags::CREATE | OpenFlags::EXCLUSIVE, true),
        P9OpenFlags::empty()
    );
}

/// The server honours at most one negotiated msize per TREAD, so a 4 KiB
/// folio takes several round trips.
const MSIZE: u32 = 4096;
const FILE_SIZE: usize = 5000;

#[derive(Clone, Default)]
struct FolioTransport {
    reads: Arc<AtomicUsize>,
    empty_read_at: Arc<AtomicUsize>,
    fsyncs: Arc<AtomicUsize>,
}

fn content_byte(offset: usize) -> u8 {
    (offset % 251) as u8
}

impl P9Transport for FolioTransport {
    fn request(&mut self, request: &[u8], response: &mut [u8]) -> VfsResult<usize> {
        response[5..7].copy_from_slice(&request[5..7]);
        let size = match request[4] {
            100 => {
                response[4] = 101; // RVERSION
                response[7..11].copy_from_slice(&MSIZE.to_le_bytes());
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
            110 => {
                response[4] = 111; // RWALK, zero walked qids
                response[7..9].copy_from_slice(&0u16.to_le_bytes());
                9
            }
            12 => {
                response[4] = 13; // RLOPEN, no iounit
                response[7..20].fill(0);
                response[12..20].copy_from_slice(&1u64.to_le_bytes());
                response[20..24].copy_from_slice(&0u32.to_le_bytes());
                24
            }
            116 => {
                let read = self.reads.fetch_add(1, Ordering::Relaxed) + 1;
                let offset = u64::from_le_bytes(request[11..19].try_into().unwrap());
                let count = u32::from_le_bytes(request[19..23].try_into().unwrap()) as usize;
                let max_data = (MSIZE - 24) as usize;
                let available = FILE_SIZE.saturating_sub(offset as usize);
                let len = if read == self.empty_read_at.load(Ordering::Relaxed) {
                    0
                } else {
                    count.min(max_data).min(available)
                };
                response[4] = 117; // RREAD
                response[7..11].copy_from_slice(&(len as u32).to_le_bytes());
                for (index, byte) in response[11..11 + len].iter_mut().enumerate() {
                    *byte = content_byte(offset as usize + index);
                }
                11 + len
            }
            50 => {
                self.fsyncs.fetch_add(1, Ordering::Relaxed);
                response[4] = 51; // RFSYNC
                7
            }
            120 => {
                response[4] = 121; // RCLUNK
                7
            }
            _ => return Err(VfsError::InvalidInput),
        };
        response[..4].copy_from_slice(&(size as u32).to_le_bytes());
        Ok(size)
    }
}

#[def_test]
fn read_folio_fills_a_folio_across_msize_limited_reads() {
    let transport = FolioTransport::default();
    let node = V9fsInode::new(
        P9Client::new(KBox::new_nowait(transport.clone()).unwrap())
            .negotiate()
            .unwrap(),
    );
    let inode = node.into_vfs_inode(VfsInodeInit::new(
        3,
        FILE_SIZE as u64,
        Umode::new(
            NodeType::RegularFile,
            NodePermission::from_bits_truncate(0o600),
        ),
    ));
    // Cacheless 9P keeps direct I/O and TSETATTR truncate on the fid path.
    assert!(inode.flags().contains(NodeFlags::NON_CACHEABLE));

    let mapping = AddressSpace::new(Arc::downgrade(&inode), &V9FS_ADDRESS_SPACE_OPERATIONS);
    let page = mapping
        .with_folio_or_create(0, |folio| Ok(folio.data().to_vec()))
        .unwrap();

    assert_eq!(page.len(), PAGE_SIZE_4K);
    assert_eq!(
        &page[..],
        &(0..PAGE_SIZE_4K).map(content_byte).collect::<Vec<_>>()[..]
    );

    // The last folio holds the file tail; the rest follows the KVFS contract.
    let tail = mapping
        .with_folio_or_create(1, |folio| Ok(folio.data().to_vec()))
        .unwrap();
    let tail_len = FILE_SIZE - PAGE_SIZE_4K;
    assert_eq!(
        &tail[..tail_len],
        &(PAGE_SIZE_4K..FILE_SIZE)
            .map(content_byte)
            .collect::<Vec<_>>()[..]
    );
    assert!(tail[tail_len..].iter().all(|byte| *byte == 0));
    // 4072 bytes fit in one msize-sized reply: two reads for the whole folio
    // and one for the 904-byte tail.
    assert_eq!(transport.reads.load(Ordering::Relaxed), 3);

    let beyond_eof = mapping
        .with_folio_or_create(2, |folio| Ok(folio.data().to_vec()))
        .unwrap();
    assert!(beyond_eof.iter().all(|byte| *byte == 0));
    assert_eq!(transport.reads.load(Ordering::Relaxed), 3);
}

#[def_test]
fn incomplete_folio_read_stays_non_uptodate_and_retries() {
    let transport = FolioTransport::default();
    transport.empty_read_at.store(2, Ordering::Relaxed);
    let node = V9fsInode::new(
        P9Client::new(KBox::new_nowait(transport.clone()).unwrap())
            .negotiate()
            .unwrap(),
    );
    let inode = node.into_vfs_inode(VfsInodeInit::new(
        4,
        FILE_SIZE as u64,
        Umode::new(
            NodeType::RegularFile,
            NodePermission::from_bits_truncate(0o600),
        ),
    ));
    let mapping = AddressSpace::new(Arc::downgrade(&inode), &V9FS_ADDRESS_SPACE_OPERATIONS);
    assert_eq!(
        mapping.with_folio_or_create(0, |_| Ok(())),
        Err(VfsError::Io)
    );
    mapping
        .with_folio(0, |folio| assert!(folio.is_none()))
        .unwrap();
    transport.empty_read_at.store(0, Ordering::Relaxed);
    let page = mapping
        .with_folio_or_create(0, |folio| Ok(folio.data().to_vec()))
        .unwrap();
    assert_eq!(
        page,
        (0..PAGE_SIZE_4K).map(content_byte).collect::<Vec<_>>()
    );
    assert_eq!(transport.reads.load(Ordering::Relaxed), 4);
}

struct SyncTestSuperOperations;

impl kvfs::SuperBlockOperations for SyncTestSuperOperations {
    fn statfs(&self, _: &kvfs::SuperBlock, _: &kvfs::Dentry) -> VfsResult<kvfs::StatFs> {
        Ok(kvfs::libfs::simple_statfs(0))
    }
}

#[def_test]
fn fsync_checks_local_dirty_mapping_before_requesting_server_persistence() {
    let transport = FolioTransport::default();
    let node = V9fsInode::new(
        P9Client::new(KBox::new_nowait(transport.clone()).unwrap())
            .negotiate()
            .unwrap(),
    );
    static FS_TYPE: kvfs::FileSystemType = kvfs::FileSystemType::internal("v9fs-sync-test");
    let super_block = kvfs::SuperBlock::new(&FS_TYPE, &SyncTestSuperOperations, |super_block| {
        let inode = super_block.get_or_init_inode(3, || {
            node.into_vfs_inode(VfsInodeInit::new(
                3,
                FILE_SIZE as u64,
                Umode::new(
                    NodeType::RegularFile,
                    NodePermission::from_bits_truncate(0o600),
                ),
            ))
        });
        kvfs::Dentry::new_file_from_inode(inode, None, "sync-test".into())
    });
    let file = kvfs::dentry_open(
        kvfs::Path::new(kvfs::Mount::new_root(&super_block), super_block.root_dir()),
        0,
        kcred::initial_cred(),
    )
    .unwrap();
    file.fsync(true).unwrap();
    assert_eq!(transport.fsyncs.load(Ordering::Relaxed), 1);
    file.mapping()
        .with_folio_or_create(0, |folio| {
            folio.mark_dirty();
            Ok(())
        })
        .unwrap();
    // Cacheless 9P currently has no dirty-folio writeback implementation. It
    // must report this local failure even when the server accepts TFSYNC.
    assert_eq!(file.fsync(true), Err(VfsError::NoSuchDevice));
    assert_eq!(transport.fsyncs.load(Ordering::Relaxed), 1);
    assert_eq!(
        file.fsync_range(PAGE_SIZE_4K as u64..2 * PAGE_SIZE_4K as u64, false),
        Ok(())
    );
    assert_eq!(transport.fsyncs.load(Ordering::Relaxed), 2);
    // The assertions deliberately leave unsupported dirty data. This fixture
    // now retires its mapping before the final mount shutdown attempts sync.
    file.mapping().evict().unwrap();
}
