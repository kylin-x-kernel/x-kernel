// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::{AtomicUsize, Ordering};

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
            if descriptor.len >= SECTOR_SIZE as u32 {
                self.data_addresses.push(address as usize);
            }
            if descriptor.flags & 2 != 0 {
                if descriptor.len == 1 {
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
fn unavailable_wait() -> DriverResult<Box<dyn BlockWaiter>> {
    Err(DriverError::NoMemory)
}
fn disk(features: u64, is_immediate: bool) -> (Arc<Disk>, Arc<SpinNoIrq<Hardware>>) {
    block_test_disk(features, is_immediate, unavailable_wait)
}

/// Creates a queue-aware fake disk using the caller's real host wait provider.
pub fn block_test_disk(
    features: u64,
    is_immediate: bool,
    prepare: PrepareBlockWait,
) -> (Arc<Disk>, Arc<SpinNoIrq<Hardware>>) {
    let mut base = MockTransport::new();
    base.features = features;
    base.config_space.borrow_mut()[..8].copy_from_slice(&1024u64.to_le_bytes());
    let hardware = Arc::new(SpinNoIrq::new(Hardware {
        is_immediate,
        ..Default::default()
    }));
    let disk = Disk::try_new_irq(
        QueueTransport {
            base,
            hardware: hardware.clone(),
        },
        prepare,
    )
    .unwrap();
    disk.enable_interrupts();
    (Arc::new(disk), hardware)
}

#[derive(Default)]
struct Signals {
    admission: AtomicUsize,
    completion: AtomicUsize,
}
impl BlockSignals for Signals {
    fn notify_admission(&self) {
        self.admission.fetch_add(1, Ordering::Relaxed);
    }

    fn notify_completion(&self) {
        self.completion.fetch_add(1, Ordering::Relaxed);
    }
}
struct Waiter<F: FnMut()> {
    signals: Arc<Signals>,
    complete: F,
    waits: usize,
}
impl<F: FnMut()> BlockWaiter for Waiter<F> {
    fn signals(&self) -> Arc<dyn BlockSignals> {
        self.signals.clone()
    }

    fn wait_admission(&mut self) -> DriverResult {
        Err(DriverError::NoMemory)
    }

    fn wait_completion(&mut self) {
        self.waits += 1;
        (self.complete)();
        core::assert_ne!(self.signals.completion.load(Ordering::Relaxed), 0);
    }
}

#[def_test(serial)]
fn request_read_write_flush_complete_without_driver_copy() {
    for features in [1 << 9, (1 << 9) | (1 << 28)] {
        for kind in 0..3 {
            for status in [0, 1, 2] {
                let (disk, hardware) = disk(features, false);
                let mut buffer = [0u8; SECTOR_SIZE];
                let pointer = NonNull::from(&mut buffer[..]);
                let operation = match kind {
                    0 => Operation::Read(3, pointer),
                    1 => Operation::Write(3, pointer),
                    _ => Operation::Flush,
                };
                let signals = Arc::new(Signals::default());
                let mut waiter = Waiter {
                    signals: signals.clone(),
                    waits: 0,
                    complete: || {
                        let token = hardware.lock().submitted[0];
                        hardware.lock().complete(token, status);
                        core::assert!(disk.handle(0).handled());
                        disk.process_completed_requests();
                        core::assert_eq!(MAPPINGS.load(Ordering::Relaxed), 0);
                    },
                };
                let result = disk.run_request(operation, &mut waiter);
                assert_eq!(
                    result,
                    match status {
                        0 => Ok(()),
                        1 => Err(DriverError::Io),
                        _ => Err(DriverError::Unsupported),
                    }
                );
                assert_eq!(signals.completion.load(Ordering::Relaxed), 1);
                assert_eq!(waiter.waits, 1);
                if kind != 2 {
                    assert_eq!(hardware.lock().data_addresses[0], buffer.as_ptr() as usize);
                }
                if kind == 0 {
                    assert!(buffer.iter().all(|byte| *byte == 0xa5));
                }
                assert!(!disk.handle(0).handled());
                disk.process_completed_requests();
                assert_eq!(signals.completion.load(Ordering::Relaxed), 1);
            }
        }
    }
}

#[def_test(serial)]
fn request_immediate_completion_and_noop_flush() {
    let (disk, hardware) = disk(0, true);
    let mut buffer = [0u8; SECTOR_SIZE];
    let signals = Arc::new(Signals::default());
    let mut waiter = Waiter {
        signals: signals.clone(),
        waits: 0,
        complete: || {
            core::assert!(disk.handle(0).handled());
            disk.process_completed_requests();
        },
    };
    assert_eq!(
        disk.run_request(
            Operation::Read(0, NonNull::from(&mut buffer[..])),
            &mut waiter
        ),
        Ok(())
    );
    assert_eq!(signals.completion.load(Ordering::Relaxed), 1);
    let mut waiter = Waiter {
        signals: Arc::new(Signals::default()),
        waits: 0,
        complete: || panic!("no-op flush waited"),
    };
    assert_eq!(disk.run_request(Operation::Flush, &mut waiter), Ok(()));
    assert_eq!(waiter.waits, 0);
    assert_eq!(hardware.lock().submitted.len(), 1);
    assert_eq!(MAPPINGS.load(Ordering::Relaxed), 0);
}

#[def_test(serial)]
fn request_fifo_exceeds_descriptors_and_cancels_head_and_middle() {
    let (disk, hardware) = disk(0, false);
    let mut buffers: Vec<_> = (0..24).map(|_| alloc::vec![0u8; SECTOR_SIZE]).collect();
    let signals: Vec<_> = (0..24).map(|_| Arc::new(Signals::default())).collect();
    let requests: Vec<_> = buffers
        .iter_mut()
        .zip(&signals)
        .map(|(buffer, signals)| {
            Box::pin(Request::new(
                Operation::Read(0, NonNull::from(buffer.as_mut_slice())),
                signals.clone(),
            ))
        })
        .collect();
    {
        let mut state = disk.state.lock();
        let BlockState {
            device,
            requests: queue,
            ..
        } = &mut *state;
        let queue = queue.as_mut().unwrap();
        for request in &requests {
            // SAFETY: these pinned test nodes and their buffers outlive all
            // queue references; this test retires every node before dropping them.
            unsafe {
                queue.enqueue(request);
            }
        }
        for (index, request) in requests.iter().enumerate() {
            assert_eq!(
                queue.try_submit(device.as_mut().unwrap(), request),
                index < 5
            );
        }
        assert_eq!(queue.pending.iter().count(), 19);
        queue.cancel_pending(&requests[5], DriverError::NoMemory);
        queue.cancel_pending(&requests[12], DriverError::NoMemory);
        assert!(core::ptr::eq(
            queue.pending.iter().next().unwrap(),
            &*requests[6]
        ));
    }
    // The synchronous error path must also unlink itself without disturbing the FIFO.
    let mut waiter = Waiter {
        signals: Arc::new(Signals::default()),
        waits: 0,
        complete: || panic!("cancelled request waited"),
    };
    assert_eq!(
        disk.run_request(Operation::Flush, &mut waiter),
        Err(DriverError::NoMemory)
    );
    let mut completed_count = 0;
    loop {
        let tokens = hardware.lock().submitted[completed_count..].to_vec();
        if tokens.is_empty() {
            break;
        }
        completed_count += tokens.len();
        for token in tokens.into_iter().rev() {
            hardware.lock().complete(token, 0);
        }
        disk.process_completed_requests();
        let mut state = disk.state.lock();
        let BlockState {
            device,
            requests: queue,
            ..
        } = &mut *state;
        let queue = queue.as_mut().unwrap();
        for request in &requests {
            // SAFETY: the device lock protects the state of these pinned nodes.
            if unsafe { *request.state.get() } == RequestState::Pending {
                queue.try_submit(device.as_mut().unwrap(), request);
            }
        }
    }
    assert_eq!(completed_count, 22);
    assert_eq!(MAPPINGS.load(Ordering::Relaxed), 0);
    for (index, signals) in signals.iter().enumerate() {
        assert_eq!(
            signals.completion.load(Ordering::Relaxed),
            usize::from(index != 5 && index != 12)
        );
    }
    assert!(signals[6].admission.load(Ordering::Relaxed) > 0);
    let state = disk.state.lock();
    assert!(state.requests.as_ref().unwrap().pending.is_empty());
    assert!(
        state
            .requests
            .as_ref()
            .unwrap()
            .in_flight
            .iter()
            .all(Option::is_none)
    );
}

#[def_test(serial)]
fn request_validation_and_wait_preparation_precede_submission() {
    let (disk, hardware) = disk(1 << 5, false);
    assert_eq!(
        disk.write_block(0, &[0; SECTOR_SIZE]),
        Err(DriverError::ReadOnly)
    );
    assert_eq!(
        disk.read_block(0, &mut [0; 3]),
        Err(DriverError::InvalidInput)
    );
    assert_eq!(
        disk.read_block(1024, &mut [0; SECTOR_SIZE]),
        Err(DriverError::InvalidInput)
    );
    assert_eq!(
        disk.read_block(0, &mut [0; SECTOR_SIZE]),
        Err(DriverError::NoMemory)
    );
    assert!(hardware.lock().submitted.is_empty());
    assert_eq!(MAPPINGS.load(Ordering::Relaxed), 0);
}

#[def_test]
fn request_metadata_layout_is_reported() {
    let requests = BlockRequests::new(16);
    unittest::ktest_println!(
        "blk IRQ metadata: request={}B align={} token-index={}B notification-scratch={}B",
        core::mem::size_of::<Request>(),
        core::mem::align_of::<Request>(),
        core::mem::size_of_val(&*requests.in_flight),
        requests.notifications.as_ref().unwrap().capacity()
            * core::mem::size_of::<Arc<dyn BlockSignals>>(),
    );
    assert_eq!(requests.in_flight.len(), 16);
    assert_eq!(
        requests.notifications.as_ref().unwrap().capacity(),
        requests.in_flight.len()
    );
}

#[def_test(serial)]
fn close_counts_preparation_and_keeps_retained_device_inert() {
    let (disk, hardware) = disk(0, false);
    // Admission precedes wait preparation. Closing must also wait for a call
    // that has not constructed a Request or published a descriptor yet.
    let call = disk.admit_call().unwrap();
    let signal = Arc::new(Signals::default());
    disk.begin_close(signal.clone());
    assert_eq!(signal.completion.load(Ordering::Relaxed), 0);
    assert_eq!(
        disk.read_block(0, &mut [0; SECTOR_SIZE]),
        Err(DriverError::Io)
    );
    assert!(hardware.lock().is_queue_live());
    drop(call);
    assert_eq!(signal.completion.load(Ordering::Relaxed), 1);
    disk.disable_interrupts();
    disk.finish_close();
    assert!(!hardware.lock().is_queue_live());
    assert_eq!(disk.num_blocks(), 1024);
    assert_eq!(disk.block_size(), SECTOR_SIZE);
    assert!(!disk.is_inherently_read_only());
    assert_eq!(disk.write_block(0, &[0; SECTOR_SIZE]), Err(DriverError::Io));
    assert_eq!(disk.flush(), Err(DriverError::Io));
    assert!(!disk.handle(0).handled());
    disk.process_completed_requests();
    disk.finish_close();
}

#[def_test(serial)]
fn preparation_error_retires_admission_before_close() {
    let (disk, hardware) = disk(0, false);
    assert_eq!(
        disk.read_block(0, &mut [0; SECTOR_SIZE]),
        Err(DriverError::NoMemory)
    );
    let signal = Arc::new(Signals::default());
    disk.begin_close(signal.clone());
    assert_eq!(signal.completion.load(Ordering::Relaxed), 1);
    disk.disable_interrupts();
    disk.finish_close();
    assert!(!hardware.lock().is_queue_live());
}
