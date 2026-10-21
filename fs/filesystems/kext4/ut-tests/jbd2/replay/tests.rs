// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::collections::BTreeMap;
use core::cell::RefCell;

use unittest::def_test;

use super::*;
use crate::{
    error::{Ext4Error, Ext4Result},
    jbd2::{JournalBlockReader, JournalSuperblock, scan_journal},
};

const JBD2_DESCRIPTOR_BLOCK: u32 = 1;
const JBD2_COMMIT_BLOCK: u32 = 2;
const JBD2_REVOKE_BLOCK: u32 = 5;
const JBD2_FEATURE_INCOMPAT_REVOKE: u32 = 0x0000_0001;
const JBD2_FLAG_ESCAPE: u16 = 1;
const JBD2_FLAG_SAME_UUID: u16 = 2;
const JBD2_FLAG_LAST_TAG: u16 = 8;
const UUID: [u8; 16] = [0xa5; 16];
const BLOCK_SIZE: usize = 1024;
const JOURNAL_BLOCKS: u32 = 1024;
const SEQUENCE: u32 = u32::MAX - 1;

struct TestJournal {
    blocks: BTreeMap<u32, Vec<u8>>,
}

struct TestWriter {
    blocks: RefCell<BTreeMap<u64, Vec<u8>>>,
    is_flushed: RefCell<bool>,
}

impl TestJournal {
    fn new() -> Self {
        Self {
            blocks: BTreeMap::new(),
        }
    }

    fn insert(&mut self, block: u32, bytes: Vec<u8>) {
        self.blocks.insert(block, bytes);
    }
}

impl JournalBlockReader for TestJournal {
    fn read_journal_block(&self, block: JournalBlock, output: &mut [u8]) -> Ext4Result<()> {
        let bytes = self
            .blocks
            .get(&block.get())
            .ok_or(Ext4Error::OutOfBounds)?;
        if bytes.len() != output.len() {
            return Err(Ext4Error::InvalidBufferLength {
                expected: output.len(),
                actual: bytes.len(),
            });
        }
        output.copy_from_slice(bytes);
        Ok(())
    }
}

impl TestWriter {
    fn new() -> Self {
        Self {
            blocks: RefCell::new(BTreeMap::new()),
            is_flushed: RefCell::new(false),
        }
    }
}

impl JournalReplayBlockWriter for TestWriter {
    fn write_replay_block(&self, block: JournalTargetBlock, input: &[u8]) -> Ext4Result<()> {
        self.blocks.borrow_mut().insert(block.get(), input.to_vec());
        Ok(())
    }

    fn flush_replay(&self) -> Ext4Result<()> {
        *self.is_flushed.borrow_mut() = true;
        Ok(())
    }
}

#[def_test]
fn plans_committed_updates_in_log_order() {
    let superblock = journal_superblock(1);
    let mut journal = TestJournal::new();
    journal.insert(1, descriptor_with_tag(100, 0, SEQUENCE));
    journal.insert(2, vec![0x11; BLOCK_SIZE]);
    journal.insert(3, block_header(JBD2_COMMIT_BLOCK, SEQUENCE));
    journal.insert(
        4,
        descriptor_with_tag(200, JBD2_FLAG_ESCAPE, SEQUENCE.wrapping_add(1)),
    );
    journal.insert(5, vec![0x22; BLOCK_SIZE]);
    journal.insert(6, block_header(JBD2_COMMIT_BLOCK, SEQUENCE.wrapping_add(1)));
    journal.insert(7, vec![0; BLOCK_SIZE]);

    let scan = scan_journal(&superblock, &journal).unwrap();
    let plan = plan_journal_replay(&scan);

    assert_eq!(plan.head(), JournalBlock::new(7));
    assert_eq!(
        plan.next_sequence(),
        TransactionId::new(SEQUENCE.wrapping_add(2))
    );
    assert_eq!(plan.revoke_hit_count(), 0);
    assert_eq!(
        plan.updates(),
        &[
            JournalReplayUpdate {
                transaction: TransactionId::new(SEQUENCE),
                target: JournalTargetBlock::new(100),
                log_block: JournalBlock::new(2),
                is_escaped: false,
            },
            JournalReplayUpdate {
                transaction: TransactionId::new(SEQUENCE.wrapping_add(1)),
                target: JournalTargetBlock::new(200),
                log_block: JournalBlock::new(5),
                is_escaped: true,
            },
        ]
    );
}

#[def_test]
fn ignores_uncommitted_transaction_updates_and_revokes() {
    let superblock = journal_superblock(1);
    let mut journal = TestJournal::new();
    journal.insert(1, descriptor_with_tag(100, 0, SEQUENCE));
    journal.insert(2, vec![0x11; BLOCK_SIZE]);
    journal.insert(3, block_header(JBD2_COMMIT_BLOCK, SEQUENCE));
    journal.insert(4, revoke_with_block(100, SEQUENCE.wrapping_add(1)));
    journal.insert(5, vec![0; BLOCK_SIZE]);

    let scan = scan_journal(&superblock, &journal).unwrap();
    let plan = plan_journal_replay(&scan);

    assert_eq!(plan.revoke_hit_count(), 0);
    assert_eq!(plan.updates().len(), 1);
    assert_eq!(plan.updates()[0].target(), JournalTargetBlock::new(100));
}

#[def_test]
fn committed_revoke_suppresses_same_or_earlier_transaction_update() {
    let superblock = journal_superblock(1);
    let mut journal = TestJournal::new();
    journal.insert(1, descriptor_with_tag(100, 0, SEQUENCE));
    journal.insert(2, vec![0x11; BLOCK_SIZE]);
    journal.insert(3, block_header(JBD2_COMMIT_BLOCK, SEQUENCE));
    journal.insert(4, revoke_with_block(100, SEQUENCE.wrapping_add(1)));
    journal.insert(5, block_header(JBD2_COMMIT_BLOCK, SEQUENCE.wrapping_add(1)));
    journal.insert(6, vec![0; BLOCK_SIZE]);

    let scan = scan_journal(&superblock, &journal).unwrap();
    let plan = plan_journal_replay(&scan);

    assert!(plan.is_empty());
    assert_eq!(plan.revoke_hit_count(), 1);
}

#[def_test]
fn later_update_after_revoke_still_replays() {
    let superblock = journal_superblock(1);
    let mut journal = TestJournal::new();
    journal.insert(1, descriptor_with_tag(100, 0, SEQUENCE));
    journal.insert(2, vec![0x11; BLOCK_SIZE]);
    journal.insert(3, block_header(JBD2_COMMIT_BLOCK, SEQUENCE));
    journal.insert(4, revoke_with_block(100, SEQUENCE.wrapping_add(1)));
    journal.insert(5, block_header(JBD2_COMMIT_BLOCK, SEQUENCE.wrapping_add(1)));
    journal.insert(6, descriptor_with_tag(100, 0, SEQUENCE.wrapping_add(2)));
    journal.insert(7, vec![0x22; BLOCK_SIZE]);
    journal.insert(8, block_header(JBD2_COMMIT_BLOCK, SEQUENCE.wrapping_add(2)));
    journal.insert(9, vec![0; BLOCK_SIZE]);

    let scan = scan_journal(&superblock, &journal).unwrap();
    let plan = plan_journal_replay(&scan);

    assert_eq!(plan.revoke_hit_count(), 1);
    assert_eq!(plan.updates().len(), 1);
    assert_eq!(
        plan.updates()[0].transaction(),
        TransactionId::new(SEQUENCE.wrapping_add(2))
    );
    assert_eq!(plan.updates()[0].target(), JournalTargetBlock::new(100));
    assert_eq!(plan.updates()[0].log_block(), JournalBlock::new(7));
}

#[def_test]
fn reads_escaped_replay_update_with_restored_magic() {
    let superblock = journal_superblock(1);
    let mut journal = TestJournal::new();
    let mut escaped = vec![0x33; BLOCK_SIZE];
    escaped[..4].copy_from_slice(&0u32.to_be_bytes());
    journal.insert(1, descriptor_with_tag(100, JBD2_FLAG_ESCAPE, SEQUENCE));
    journal.insert(2, escaped);
    journal.insert(3, block_header(JBD2_COMMIT_BLOCK, SEQUENCE));
    journal.insert(4, vec![0; BLOCK_SIZE]);

    let scan = scan_journal(&superblock, &journal).unwrap();
    let plan = plan_journal_replay(&scan);
    let mut output = vec![0; BLOCK_SIZE];
    read_journal_replay_update(&journal, &plan.updates()[0], &mut output).unwrap();

    assert_eq!(&output[..4], &JBD2_MAGIC_NUMBER.to_be_bytes());
    assert_eq!(output[4], 0x33);
}

#[def_test]
fn executes_replay_plan_and_flushes_writer() {
    let superblock = journal_superblock(1);
    let mut journal = TestJournal::new();
    let mut escaped = vec![0x44; BLOCK_SIZE];
    escaped[..4].copy_from_slice(&0u32.to_be_bytes());
    journal.insert(1, descriptor_with_tag(100, JBD2_FLAG_ESCAPE, SEQUENCE));
    journal.insert(2, escaped);
    journal.insert(3, block_header(JBD2_COMMIT_BLOCK, SEQUENCE));
    journal.insert(4, vec![0; BLOCK_SIZE]);

    let scan = scan_journal(&superblock, &journal).unwrap();
    let plan = plan_journal_replay(&scan);
    let writer = TestWriter::new();
    let report = replay_journal_plan(&journal, &writer, &plan, BLOCK_SIZE).unwrap();

    let blocks = writer.blocks.borrow();
    let written = blocks.get(&100).expect("target block was replayed");
    assert_eq!(&written[..4], &JBD2_MAGIC_NUMBER.to_be_bytes());
    assert_eq!(written[4], 0x44);
    assert_eq!(
        report,
        JournalReplayReport {
            update_count: 1,
            revoke_hit_count: 0,
            head: JournalBlock::new(4),
            next_sequence: TransactionId::new(SEQUENCE.wrapping_add(1)),
        }
    );
    assert!(*writer.is_flushed.borrow());
}

#[def_test]
fn executes_scanned_replay_and_flushes_writer_without_plan_vector() {
    let superblock = journal_superblock(1);
    let mut journal = TestJournal::new();
    let mut escaped = vec![0x55; BLOCK_SIZE];
    escaped[..4].copy_from_slice(&0u32.to_be_bytes());
    journal.insert(1, descriptor_with_tag(100, JBD2_FLAG_ESCAPE, SEQUENCE));
    journal.insert(2, escaped);
    journal.insert(3, block_header(JBD2_COMMIT_BLOCK, SEQUENCE));
    journal.insert(4, revoke_with_block(200, SEQUENCE.wrapping_add(1)));
    journal.insert(5, block_header(JBD2_COMMIT_BLOCK, SEQUENCE.wrapping_add(1)));
    journal.insert(6, vec![0; BLOCK_SIZE]);

    let scan = scan_journal(&superblock, &journal).unwrap();
    let writer = TestWriter::new();
    let applied = replay_scanned_journal(&journal, &writer, &scan, BLOCK_SIZE).unwrap();

    let blocks = writer.blocks.borrow();
    let written = blocks.get(&100).expect("target block was replayed");
    assert_eq!(&written[..4], &JBD2_MAGIC_NUMBER.to_be_bytes());
    assert_eq!(written[4], 0x55);
    assert_eq!(
        applied.into_report(),
        JournalReplayReport {
            update_count: 1,
            revoke_hit_count: 0,
            head: JournalBlock::new(6),
            next_sequence: TransactionId::new(SEQUENCE.wrapping_add(2)),
        }
    );
    assert!(*writer.is_flushed.borrow());
}

fn journal_superblock(start: u32) -> JournalSuperblock {
    let mut bytes = [0; BLOCK_SIZE];
    put_be_u32(&mut bytes, 0, JBD2_MAGIC_NUMBER);
    put_be_u32(&mut bytes, 4, 4);
    put_be_u32(&mut bytes, 12, BLOCK_SIZE as u32);
    put_be_u32(&mut bytes, 16, JOURNAL_BLOCKS);
    put_be_u32(&mut bytes, 20, 1);
    put_be_u32(&mut bytes, 24, SEQUENCE);
    put_be_u32(&mut bytes, 28, start);
    put_be_u32(&mut bytes, 40, JBD2_FEATURE_INCOMPAT_REVOKE);
    bytes[48..64].copy_from_slice(&UUID);
    put_be_u32(&mut bytes, 64, 1);
    JournalSuperblock::decode(&bytes, BLOCK_SIZE as u32, JOURNAL_BLOCKS, UUID).unwrap()
}

fn descriptor_with_tag(target: u32, flags: u16, sequence: u32) -> Vec<u8> {
    let mut bytes = block_header(JBD2_DESCRIPTOR_BLOCK, sequence);
    put_be_u32(&mut bytes, 12, target);
    put_be_u16(&mut bytes, 18, flags | JBD2_FLAG_LAST_TAG);
    if flags & JBD2_FLAG_SAME_UUID == 0 {
        bytes[20..36].copy_from_slice(&UUID);
    }
    bytes
}

fn revoke_with_block(target: u32, sequence: u32) -> Vec<u8> {
    let mut bytes = block_header(JBD2_REVOKE_BLOCK, sequence);
    put_be_u32(&mut bytes, 12, 20);
    put_be_u32(&mut bytes, 16, target);
    bytes
}

fn block_header(block_type: u32, sequence: u32) -> Vec<u8> {
    let mut bytes = vec![0; BLOCK_SIZE];
    put_be_u32(&mut bytes, 0, JBD2_MAGIC_NUMBER);
    put_be_u32(&mut bytes, 4, block_type);
    put_be_u32(&mut bytes, 8, sequence);
    bytes
}

fn put_be_u16(output: &mut [u8], offset: usize, value: u16) {
    output[offset..offset + 2].copy_from_slice(&value.to_be_bytes());
}

fn put_be_u32(output: &mut [u8], offset: usize, value: u32) {
    output[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}
