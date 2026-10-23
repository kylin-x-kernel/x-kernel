// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec::Vec;
use core::{
    ptr::NonNull,
    sync::atomic::{AtomicUsize, Ordering},
};

use block::completion::BlockSignals;
use unittest::{assert, assert_eq, def_test};
use virtio_drivers::{
    BufferDirection, PhysAddr,
    transport::{DeviceStatus, DeviceType, InterruptStatus},
};
use zerocopy::{FromBytes, Immutable, IntoBytes};

use super::*;
use crate::mock_virtio::{MockHal, MockTransport};

static MAPPINGS: AtomicUsize = AtomicUsize::new(0);
/// Identity-mapped HAL with mapping-retirement accounting for block tests.
pub struct TrackedHal;
// SAFETY: delegates allocation and address translation to the identity-mapped
// test HAL; counts only mappings from this test HAL, never production mappings.
unsafe impl Hal for TrackedHal {
    fn dma_alloc(
        pages: usize,
        direction: BufferDirection,
        platform: bool,
    ) -> (PhysAddr, NonNull<u8>) {
        MockHal::dma_alloc(pages, direction, platform)
    }

    unsafe fn dma_dealloc(
        paddr: PhysAddr,
        vaddr: NonNull<u8>,
        pages: usize,
        platform: bool,
    ) -> i32 {
        // SAFETY: exact allocation tuple forwarded unchanged to its allocator.
        unsafe { MockHal::dma_dealloc(paddr, vaddr, pages, platform) }
    }

    unsafe fn mmio_phys_to_virt(paddr: PhysAddr, size: usize) -> NonNull<u8> {
        // SAFETY: forwards the mock's valid identity-mapped address contract.
        unsafe { MockHal::mmio_phys_to_virt(paddr, size) }
    }

    unsafe fn share(buffer: NonNull<[u8]>, direction: BufferDirection, platform: bool) -> PhysAddr {
        MAPPINGS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: the request retains this exact buffer through unshare.
        unsafe { MockHal::share(buffer, direction, platform) }
    }

    unsafe fn unshare(
        paddr: PhysAddr,
        buffer: NonNull<[u8]>,
        direction: BufferDirection,
        platform: bool,
    ) {
        // SAFETY: the dependency returns the same share tuple when popping it.
        unsafe { MockHal::unshare(paddr, buffer, direction, platform) };
        core::assert!(MAPPINGS.fetch_sub(1, Ordering::Relaxed) > 0);
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Descriptor {
    address: u64,
    len: u32,
    flags: u16,
    next: u16,
}

/// Queue memory and completions owned by the fake device side of a block test.
#[derive(Default)]
pub struct Hardware {
    descriptors: usize,
    available: usize,
    used: usize,
    depth: usize,
    seen: u16,
    completed: u16,
    submitted: Vec<u16>,
    is_immediate: bool,
    is_pending_irq: bool,
    data_addresses: Vec<usize>,
}

impl Hardware {
    /// Whether the fake still has a live driver-owned descriptor queue.
    pub fn is_queue_live(&self) -> bool {
        self.descriptors != 0
    }

    /// Completes the currently submitted batch; invoked by the test device task.
    pub fn complete_pending(&mut self) -> usize {
        let tokens = self.submitted[usize::from(self.completed)..].to_vec();
        let count = tokens.len();
        for token in tokens.into_iter().rev() {
            self.complete(token, 0);
        }
        count
    }

    /// Number of requests observed by the fake device, including retired ones.
    pub fn submitted_count(&self) -> usize {
        self.submitted.len()
    }

    fn notified(&mut self) {
        core::assert_ne!(self.available, 0);
        let available = self.available as *const u16;
        // SAFETY: queue_set retains the aligned avail ring through queue_unset;
        // offset one selects its index field under the fake hardware lock.
        let index_ptr = unsafe { available.add(1) };
        // SAFETY: The driver published this initialized avail index before notify.
        let end = unsafe { index_ptr.read_volatile() };
        while self.seen != end {
            // SAFETY: Modulo depth selects an in-bounds avail ring entry, after
            // the two-word header, in the live queue allocation.
            let slot = unsafe { available.add(2 + usize::from(self.seen) % self.depth) };
            // SAFETY: The driver initialized this aligned slot before publishing end.
            let token = unsafe { slot.read_volatile() };
            self.submitted.push(token);
            self.seen = self.seen.wrapping_add(1);
            if self.is_immediate {
                self.complete(token, 0);
            }
        }
    }

    fn complete(&mut self, token: u16, status: u8) {
        core::assert!(usize::from(token) < self.depth);
        core::assert_ne!(self.descriptors, 0);
        let mut table = self.descriptors as *const Descriptor;
        let mut index = usize::from(token);
        let mut count = self.depth;
        // SAFETY: token was checked against the live published descriptor table's
        // depth; queue_set keeps its allocation valid under the hardware lock.
        let head_ptr = unsafe { table.add(index) };
        // SAFETY: The driver initialized this aligned descriptor before notify.
        let head = unsafe { head_ptr.read_volatile() };
        if head.flags & 4 != 0 {
            table = head.address as *const Descriptor;
            index = 0;
            count = head.len as usize / core::mem::size_of::<Descriptor>();
        }
        for step in 0..count {
            core::assert!(index < count);
            // SAFETY: index is checked against the direct/indirect table length;
            // TrackedHal identity addresses refer to live driver-owned tables.
            let descriptor_ptr = unsafe { table.add(index) };
            // SAFETY: The published descriptor is initialized and aligned, and
            // no driver mutation overlaps this hardware-locked completion.
            let descriptor = unsafe { descriptor_ptr.read_volatile() };
            let address = descriptor.address as *mut u8;
            if step != 0 && descriptor.flags & 1 != 0 {
                self.data_addresses.push(address as usize);
            }
            if descriptor.flags & 2 != 0 {
                if descriptor.flags & 1 == 0 {
                    // SAFETY: The writable descriptor grants this fake device
                    // one live response byte, mapped identically by TrackedHal.
                    unsafe { address.write_volatile(status) };
                } else {
                    // SAFETY: The writable descriptor grants descriptor.len
                    // live bytes retained by the submitting driver until completion.
                    unsafe { address.write_bytes(0xa5, descriptor.len as usize) };
                }
            }
            if descriptor.flags & 1 == 0 {
                break;
            }
            core::assert!(step + 1 < count, "cyclic fake descriptor chain");
            index = usize::from(descriptor.next);
        }
        let used = self.used as *mut u8;
        // SAFETY: queue_set retains the used ring; modulo depth selects an
        // eight-byte entry after its four-byte header, aligned for u32 access.
        let slot = unsafe {
            used.add(4 + (usize::from(self.completed) % self.depth) * 8)
                .cast::<u32>()
        };
        // SAFETY: The fake device owns this in-bounds used-ring token field.
        unsafe { slot.write_volatile(u32::from(token)) };
        // SAFETY: The second u32 is within the same eight-byte used-ring entry.
        let length = unsafe { slot.add(1) };
        // SAFETY: The fake exclusively writes the initialized, aligned used entry.
        unsafe { length.write_volatile(0) };
        core::sync::atomic::fence(Ordering::Release);
        self.completed = self.completed.wrapping_add(1);
        // SAFETY: The u16 used index is at byte offset two of the live ring header.
        let used_index = unsafe { used.add(2).cast::<u16>() };
        // SAFETY: The fake owns this aligned index; the release fence publishes
        // the completed entry before the driver can observe the new index.
        unsafe { used_index.write_volatile(self.completed) };
        self.is_pending_irq = true;
    }
}

/// Transport that publishes actual dependency rings to the fake device.
pub struct QueueTransport {
    base: MockTransport,
    hardware: Arc<SpinNoIrq<Hardware>>,
}
impl Transport for QueueTransport {
    fn device_type(&self) -> DeviceType {
        self.base.device_type()
    }

    fn read_device_features(&mut self) -> u64 {
        self.base.read_device_features()
    }

    fn write_driver_features(&mut self, features: u64) {
        self.base.write_driver_features(features);
    }

    fn max_queue_size(&mut self, queue: u16) -> u32 {
        self.base.max_queue_size(queue)
    }

    fn notify(&mut self, _queue: u16) {
        self.hardware.lock().notified();
    }

    fn get_status(&self) -> DeviceStatus {
        self.base.get_status()
    }

    fn set_status(&mut self, status: DeviceStatus) {
        self.base.set_status(status);
    }

    fn set_guest_page_size(&mut self, size: u32) {
        self.base.set_guest_page_size(size);
    }

    fn requires_legacy_layout(&self) -> bool {
        false
    }

    fn queue_set(
        &mut self,
        _queue: u16,
        size: u32,
        descriptors: PhysAddr,
        available: PhysAddr,
        used: PhysAddr,
    ) {
        let mut hardware = self.hardware.lock();
        hardware.depth = size as usize;
        hardware.descriptors = descriptors as usize;
        hardware.available = available as usize;
        hardware.used = used as usize;
    }

    fn queue_unset(&mut self, _queue: u16) {
        let mut hardware = self.hardware.lock();
        hardware.descriptors = 0;
        hardware.available = 0;
        hardware.used = 0;
    }

    fn queue_used(&mut self, _queue: u16) -> bool {
        false
    }

    fn ack_interrupt(&mut self) -> InterruptStatus {
        if core::mem::take(&mut self.hardware.lock().is_pending_irq) {
            InterruptStatus::from_bits_truncate(1)
        } else {
            InterruptStatus::empty()
        }
    }

    fn read_config_generation(&self) -> u32 {
        0
    }

    fn read_config_space<V: FromBytes + IntoBytes>(
        &self,
        offset: usize,
    ) -> virtio_drivers::Result<V> {
        self.base.read_config_space(offset)
    }

    fn write_config_space<V: IntoBytes + Immutable>(
        &mut self,
        offset: usize,
        value: V,
    ) -> virtio_drivers::Result<()> {
        self.base.write_config_space(offset, value)
    }
}

type Disk = VirtIoBlkDev<TrackedHal, QueueTransport>;
fn disk(features: u64, is_immediate: bool) -> (Arc<Disk>, Arc<SpinNoIrq<Hardware>>) {
    block_test_disk(features, is_immediate)
}

/// Creates a queue-aware fake disk using the caller's real host wait provider.
pub fn block_test_disk(features: u64, is_immediate: bool) -> (Arc<Disk>, Arc<SpinNoIrq<Hardware>>) {
    let mut base = MockTransport::new();
    base.features = features;
    base.config_space.borrow_mut()[..8].copy_from_slice(&1024u64.to_le_bytes());
    base.config_space.borrow_mut()[8..12].copy_from_slice(&1024u32.to_le_bytes());
    base.config_space.borrow_mut()[12..16]
        .copy_from_slice(&(MAX_DATA_SEGMENTS as u32).to_le_bytes());
    let hardware = Arc::new(SpinNoIrq::new(Hardware {
        is_immediate,
        ..Default::default()
    }));
    let disk = Disk::try_new_irq(QueueTransport {
        base,
        hardware: hardware.clone(),
    })
    .unwrap();
    disk.enable_interrupts();
    (Arc::new(disk), hardware)
}

#[derive(Default)]
struct Signals {
    completion: AtomicUsize,
}

#[def_test(serial)]
fn polling_requests_reclaim_split_transfers_without_interrupts() {
    let mut base = MockTransport::new();
    base.features = (1 << 1) | (1 << 2); // SIZE_MAX and SEG_MAX.
    base.config_space.borrow_mut()[..8].copy_from_slice(&1024u64.to_le_bytes());
    base.config_space.borrow_mut()[8..12].copy_from_slice(&1024u32.to_le_bytes());
    base.config_space.borrow_mut()[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
    let hardware = Arc::new(SpinNoIrq::new(Hardware {
        is_immediate: true,
        ..Default::default()
    }));
    let disk = Arc::new(
        Disk::try_new(QueueTransport {
            base,
            hardware: hardware.clone(),
        })
        .unwrap(),
    );
    assert!(!disk.state.lock().is_irq_driven);
    assert_eq!(
        disk.state.lock().device.as_ref().unwrap().max_segments(),
        MAX_DATA_SEGMENTS
    );
    let device = view(&disk);
    for operation in [BioOperation::Read, BioOperation::Write] {
        let before = hardware.lock().completed;
        let result = Arc::new(SpinNoIrq::new(None));
        let completed = result.clone();
        device.submit_bio(block::Bio::new(
            device.clone(),
            7,
            operation,
            vec![block::BioVec::new(vec![0x3c; 16 * 1024].into_boxed_slice())],
            move |status, vectors| *completed.lock() = Some((status, vectors)),
        ));
        // notify drives the fake device; only the polling path can reclaim its
        // used descriptors and submit the suffix. No IRQ executor runs here.
        let (status, vectors) = result.lock().take().expect("polling completes inline");
        assert_eq!(status, Ok(()));
        let expected = if operation == BioOperation::Read { 0xa5 } else { 0x3c };
        assert!(vectors[0].bytes().iter().all(|byte| *byte == expected));
        assert_eq!(hardware.lock().completed - before, 2);
        assert!(disk.state.lock().requests.is_drained());
        assert_eq!(disk.state.lock().active_requests, 0);
        assert_eq!(MAPPINGS.load(Ordering::Relaxed), 0);
    }
    let drained = Arc::new(Signals::default());
    disk.begin_close(drained.clone());
    assert_eq!(drained.completion.load(Ordering::Relaxed), 1);
    disk.finish_close();
    assert!(!hardware.lock().is_queue_live());
}
impl BlockSignals for Signals {
    fn notify_completion(&self) {
        self.completion.fetch_add(1, Ordering::Relaxed);
    }
}

fn view(disk: &Arc<Disk>) -> Arc<block::BlockDevice> {
    block::Gendisk::alloc_mq(
        alloc::string::String::from("virtio-test"),
        0,
        0,
        1,
        disk.clone(),
    )
    .unwrap()
    .part0()
}

#[def_test(serial)]
fn request_read_write_flush_complete_without_driver_copy() {
    for features in [1 << 9, (1 << 9) | (1 << 28)] {
        for operation in [BioOperation::Read, BioOperation::Write, BioOperation::Flush] {
            for status in [0, 1, 2] {
                let (disk, hardware) = disk(features, false);
                let device = view(&disk);
                let buffer = vec![0u8; SECTOR_SIZE].into_boxed_slice();
                let address = buffer.as_ptr() as usize;
                let vectors = if operation == BioOperation::Flush {
                    Vec::new()
                } else {
                    vec![block::BioVec::new(buffer)]
                };
                let result = Arc::new(SpinNoIrq::new(None));
                let completed = result.clone();
                device.submit_bio(block::Bio::new(
                    device.clone(),
                    if operation == BioOperation::Flush {
                        0
                    } else {
                        3
                    },
                    operation,
                    vectors,
                    move |status, vectors| {
                        *completed.lock() = Some((status, vectors));
                    },
                ));
                assert!(result.lock().is_none());
                assert!(MAPPINGS.load(Ordering::Relaxed) > 0);
                let token = hardware.lock().submitted[0];
                hardware.lock().complete(token, status);
                assert!(disk.handle(0).handled());
                disk.process_completed_requests();
                assert_eq!(MAPPINGS.load(Ordering::Relaxed), 0);
                let (actual, vectors) = result.lock().take().unwrap();
                assert_eq!(
                    actual,
                    match status {
                        0 => Ok(()),
                        1 => Err(DriverError::Io),
                        _ => Err(DriverError::Unsupported),
                    }
                );
                if operation != BioOperation::Flush {
                    assert_eq!(hardware.lock().data_addresses[0], address);
                    assert_eq!(vectors[0].bytes().as_ptr() as usize, address);
                }
                if operation == BioOperation::Read {
                    assert!(vectors[0].bytes().iter().all(|byte| *byte == 0xa5));
                }
                disk.process_completed_requests();
                assert!(result.lock().is_none());
            }
        }
    }
}

#[def_test(serial)]
fn submit_batch_then_wait_preserves_fifo_and_owned_scatter_memory() {
    let (disk, hardware) = disk(0, false);
    let device = view(&disk);
    let mut waits = Vec::new();
    for _ in 0..24 {
        // Memory boundaries need not even align to 512-byte sectors.
        let vectors = vec![
            block::BioVec::new(vec![0; 17].into_boxed_slice()),
            block::BioVec::new(vec![0; 495].into_boxed_slice()),
        ];
        let (bio, wait) =
            block::completion::BioWait::prepare(device.clone(), 0, BioOperation::Read, vectors)
                .unwrap_or_else(|(error, _)| panic!("prepare: {error:?}"));
        device.submit_bio(bio);
        waits.push(wait);
    }
    // All submissions returned without a caller waiting or hardware completing.
    assert_eq!(hardware.lock().submitted_count(), 5);
    assert_eq!(disk.state.lock().active_requests, 24);
    let drain = Arc::new(Signals::default());
    disk.begin_close(drain.clone());
    assert_eq!(drain.completion.load(Ordering::Relaxed), 0);
    // Dropping a waiting handle must not free pending or DMA-owned data.
    drop(waits.pop());
    while hardware.lock().completed < 24 {
        hardware.lock().complete_pending();
        disk.process_completed_requests();
    }
    for wait in waits {
        let (status, vectors) = wait.wait();
        status.unwrap();
        assert_eq!(vectors[0].bytes().len(), 17);
        assert_eq!(vectors[1].bytes().len(), 495);
        assert!(
            vectors
                .iter()
                .all(|v| v.bytes().iter().all(|byte| *byte == 0xa5))
        );
    }
    assert_eq!(MAPPINGS.load(Ordering::Relaxed), 0);
    assert_eq!(drain.completion.load(Ordering::Relaxed), 1);
    disk.disable_interrupts();
    disk.finish_close();
    assert!(!hardware.lock().is_queue_live());
    assert_eq!(device.read_block(0, &mut [0; 512]), Err(DriverError::Io));
    disk.finish_close();
}

#[def_test(serial)]
fn request_immediate_completion_and_noop_flush() {
    let (disk, hardware) = disk(0, true);
    let device = view(&disk);
    let (bio, wait) = block::completion::BioWait::prepare(
        device.clone(),
        0,
        BioOperation::Read,
        vec![block::BioVec::new(vec![0; 512].into_boxed_slice())],
    )
    .unwrap_or_else(|(error, _)| panic!("prepare: {error:?}"));
    device.submit_bio(bio);
    disk.process_completed_requests();
    assert!(wait.wait().0.is_ok());
    assert_eq!(device.flush(), Ok(()));
    assert_eq!(hardware.lock().submitted_count(), 1);
    assert_eq!(MAPPINGS.load(Ordering::Relaxed), 0);
}

#[def_test(serial)]
fn request_validation_precedes_submission() {
    let (disk, hardware) = disk(1 << 5, false);
    let device = view(&disk);
    assert_eq!(device.write_block(0, &[0; 512]), Err(DriverError::ReadOnly));
    assert_eq!(
        device.read_block(0, &mut [0; 3]),
        Err(DriverError::InvalidInput)
    );
    assert_eq!(
        device.read_block(1024, &mut [0; 512]),
        Err(DriverError::InvalidInput)
    );
    assert!(hardware.lock().submitted.is_empty());
    assert_eq!(MAPPINGS.load(Ordering::Relaxed), 0);
}

#[def_test(serial)]
fn scatter_transfers_preserve_addresses_and_split_only_at_device_limits() {
    for features in [(1 << 1) | (1 << 2), (1 << 1) | (1 << 2) | (1 << 28)] {
        for operation in [BioOperation::Read, BioOperation::Write] {
            let (disk, hardware) = disk(features, false);
            let device = view(&disk);
            // Two sub-sector segments go directly to hardware. The final large
            // extent exceeds both size_max and one chain's descriptor budget.
            let vectors: Vec<_> = [17, 495, 16 * 1024]
                .into_iter()
                .map(|len| block::BioVec::new(vec![0; len].into_boxed_slice()))
                .collect();
            let addresses: Vec<_> = vectors
                .iter()
                .map(|v| v.bytes().as_ptr() as usize)
                .collect();
            let (bio, wait) =
                block::completion::BioWait::prepare(device.clone(), 7, operation, vectors)
                    .unwrap_or_else(|(error, _)| panic!("prepare: {error:?}"));
            device.submit_bio(bio);
            assert_eq!(hardware.lock().submitted_count(), 1);
            hardware.lock().complete_pending();
            disk.process_completed_requests();
            // The common request has not completed; its remaining extent was
            // submitted by completion processing, without waking the caller.
            assert_eq!(disk.state.lock().active_requests, 1);
            assert_eq!(hardware.lock().submitted_count(), 2);
            hardware.lock().complete_pending();
            disk.process_completed_requests();
            let (status, vectors) = wait.wait();
            status.unwrap();
            assert_eq!(disk.state.lock().active_requests, 0);
            assert_eq!(MAPPINGS.load(Ordering::Relaxed), 0);
            let expected: Vec<_> = addresses[..2]
                .iter()
                .copied()
                .chain((0..16).map(|i| addresses[2] + i * 1024))
                .collect();
            assert_eq!(hardware.lock().data_addresses, expected);
            assert_eq!(
                vectors
                    .iter()
                    .map(|v| v.bytes().as_ptr() as usize)
                    .collect::<Vec<_>>(),
                addresses
            );
            let value = if operation == BioOperation::Read {
                0xa5
            } else {
                0
            };
            assert!(
                vectors
                    .iter()
                    .all(|v| v.bytes().iter().all(|byte| *byte == value))
            );
        }
    }
}
