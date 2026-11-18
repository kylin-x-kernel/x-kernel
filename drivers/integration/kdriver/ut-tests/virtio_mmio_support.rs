// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Test-only MMIO device fixtures for the real probe chain.
//!
//! A fixture lets the production `activate_virtio_mmio` path run without
//! physical MMIO: identification reads a synthetic header, and only the
//! matching candidate can open a transport. One header instance is shared by
//! every candidate probing the same range, so a candidate that writes a
//! register would be visible to the next one. The header is preloaded with a
//! non-zero status register at the protocol offset, and every identification
//! and open records what it observed there.
//!
//! The fixture replaces only the register source. It must receive the
//! production [`MmioClaim`] through [`Fixture::with_claim`] before it allows
//! any access, and it moves that claim into the opened transport, so a probe
//! that skips the shared claim contract cannot observe the fixture at all.

use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use core::{
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use driver_base::{DriverError, DriverResult};
use driver_os_api::MmioClaim;
use ksync::{Mutex, static_lock};
use virtio::{
    BufferDirection, DeviceStatus, OwnedMmioTransport, PhysAddr, mock_virtio::MockTransport,
};

use super::MmioAccess;

/// Calls observed through [`CountingVirtIoHal`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct HalCounts {
    pub(super) alloc_calls: usize,
    pub(super) dealloc_calls: usize,
    pub(super) share_calls: usize,
    pub(super) unshare_calls: usize,
    pub(super) share_bytes: usize,
    pub(super) unshare_bytes: usize,
    pub(super) alloc_failures: usize,
    pub(super) share_failures: usize,
}

static ALLOC_CALLS: AtomicUsize = AtomicUsize::new(0);
static DEALLOC_CALLS: AtomicUsize = AtomicUsize::new(0);
static SHARE_CALLS: AtomicUsize = AtomicUsize::new(0);
static UNSHARE_CALLS: AtomicUsize = AtomicUsize::new(0);
static SHARE_BYTES: AtomicUsize = AtomicUsize::new(0);
static UNSHARE_BYTES: AtomicUsize = AtomicUsize::new(0);
static ALLOC_FAILURES: AtomicUsize = AtomicUsize::new(0);
static SHARE_FAILURES: AtomicUsize = AtomicUsize::new(0);
/// Serializes fixture scenarios. `def_test(serial)` only orders tests inside
/// one worker group; claim and match tests can otherwise overlap and share the
/// process-wide HAL counters above.
static SCENARIO_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Clears this scenario's counts.
pub(super) fn reset_counts() {
    for counter in [
        &ALLOC_CALLS,
        &DEALLOC_CALLS,
        &SHARE_CALLS,
        &UNSHARE_CALLS,
        &SHARE_BYTES,
        &UNSHARE_BYTES,
        &ALLOC_FAILURES,
        &SHARE_FAILURES,
    ] {
        counter.store(0, Ordering::SeqCst);
    }
}

/// Reads this scenario's counts.
pub(super) fn counts() -> HalCounts {
    HalCounts {
        alloc_calls: ALLOC_CALLS.load(Ordering::SeqCst),
        dealloc_calls: DEALLOC_CALLS.load(Ordering::SeqCst),
        share_calls: SHARE_CALLS.load(Ordering::SeqCst),
        unshare_calls: UNSHARE_CALLS.load(Ordering::SeqCst),
        share_bytes: SHARE_BYTES.load(Ordering::SeqCst),
        unshare_bytes: UNSHARE_BYTES.load(Ordering::SeqCst),
        alloc_failures: ALLOC_FAILURES.load(Ordering::SeqCst),
        share_failures: SHARE_FAILURES.load(Ordering::SeqCst),
    }
}

/// HAL wrapper that forwards to the production implementation and counts calls.
///
/// Forwarding means the provider, bounce pool and coherent allocator are the
/// real ones; these counts only attribute calls to the fixture scenario.
pub(super) struct CountingVirtIoHal;

// SAFETY: Every operation forwards unchanged to `VirtIoHalImpl`, which upholds
// the `Hal` contract. Counting only observes call arguments.
unsafe impl virtio::VirtIoHal for CountingVirtIoHal {
    fn dma_alloc(
        pages: usize,
        direction: BufferDirection,
        access_platform: bool,
    ) -> (PhysAddr, NonNull<u8>) {
        ALLOC_CALLS.fetch_add(1, Ordering::SeqCst);
        let allocation = <super::glue::VirtIoHalImpl as virtio::VirtIoHal>::dma_alloc(
            pages,
            direction,
            access_platform,
        );
        if allocation.0 == 0 {
            ALLOC_FAILURES.fetch_add(1, Ordering::SeqCst);
        }
        allocation
    }

    unsafe fn dma_dealloc(
        paddr: PhysAddr,
        vaddr: NonNull<u8>,
        pages: usize,
        access_platform: bool,
    ) -> i32 {
        DEALLOC_CALLS.fetch_add(1, Ordering::SeqCst);
        // SAFETY: Forwarded unchanged; the caller upholds the allocation contract.
        unsafe {
            <super::glue::VirtIoHalImpl as virtio::VirtIoHal>::dma_dealloc(
                paddr,
                vaddr,
                pages,
                access_platform,
            )
        }
    }

    unsafe fn mmio_phys_to_virt(paddr: PhysAddr, size: usize) -> NonNull<u8> {
        // SAFETY: Forwarded unchanged; the caller supplies a valid MMIO range.
        unsafe { <super::glue::VirtIoHalImpl as virtio::VirtIoHal>::mmio_phys_to_virt(paddr, size) }
    }

    unsafe fn share(
        buffer: NonNull<[u8]>,
        direction: BufferDirection,
        access_platform: bool,
    ) -> PhysAddr {
        SHARE_CALLS.fetch_add(1, Ordering::SeqCst);
        SHARE_BYTES.fetch_add(buffer.len(), Ordering::SeqCst);
        // SAFETY: Forwarded unchanged; the caller keeps the buffer valid.
        let paddr = unsafe {
            <super::glue::VirtIoHalImpl as virtio::VirtIoHal>::share(
                buffer,
                direction,
                access_platform,
            )
        };
        if paddr == 0 {
            SHARE_FAILURES.fetch_add(1, Ordering::SeqCst);
        }
        paddr
    }

    unsafe fn unshare(
        paddr: PhysAddr,
        buffer: NonNull<[u8]>,
        direction: BufferDirection,
        access_platform: bool,
    ) {
        UNSHARE_CALLS.fetch_add(1, Ordering::SeqCst);
        UNSHARE_BYTES.fetch_add(buffer.len(), Ordering::SeqCst);
        // SAFETY: Forwarded unchanged; device access has stopped and the buffer
        // is still valid, as required by the caller.
        unsafe {
            <super::glue::VirtIoHalImpl as virtio::VirtIoHal>::unshare(
                paddr,
                buffer,
                direction,
                access_platform,
            )
        }
    }
}

const MAGIC_VALUE: u32 = 0x7472_6976;
const MODERN_VERSION: u32 = 2;
/// Header must be at least the vendor reader's `CONFIG_SPACE_OFFSET`.
const HEADER_WORDS: usize = 0x80;
const HEADER_SIZE: usize = HEADER_WORDS * core::mem::size_of::<u32>();
/// Device status register (`0x70` in the VirtIO MMIO layout).
const STATUS_WORD: usize = 0x70 / core::mem::size_of::<u32>();
/// Non-zero status bits used to prove identification never resets the device.
pub(super) const INITIAL_STATUS: u32 = 0x0f;

/// One observed access to the fixture's synthetic header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Event {
    /// A candidate identified the device read-only.
    Identify,
    /// A candidate opened a transport.
    Open,
}

struct Header {
    words: [u32; HEADER_WORDS],
}

struct Spec {
    released: Arc<kcompletion::Completion>,
    device_type: virtio::DeviceType,
    header: Arc<Mutex<Header>>,
    opens: Arc<AtomicUsize>,
    events: Arc<Mutex<Vec<Event>>>,
    status_after_identify: Arc<Mutex<Vec<u32>>>,
    status_at_open: Arc<Mutex<Vec<u32>>>,
    status_history: Arc<kspin::SpinNoIrq<Vec<DeviceStatus>>>,
}

static_lock! {
    static FIXTURES: Mutex<BTreeMap<usize, Spec>> = Mutex::new(BTreeMap::new());
}

// Every MMIO fixture scenario resets the global device registry and shared HAL
// counters. The fixture table is keyed by base, but those resets are global, so
// all scenarios (including the standalone match probes) must serialize. The
// guard must allow sleeping while runtime readers release their last handles.
static_lock! {
    pub(super) static MMIO_FIXTURE_LOCK: Mutex<()> = Mutex::new(());
}

/// Registers a fixture at a descriptor's first MMIO base address.
pub(super) fn install(
    base: usize,
    device_id: u32,
    device_type: virtio::DeviceType,
) -> FixtureHandle {
    let mut words = [0u32; HEADER_WORDS];
    words[0] = MAGIC_VALUE;
    words[1] = MODERN_VERSION;
    words[2] = device_id;
    words[STATUS_WORD] = INITIAL_STATUS;

    while SCENARIO_ACTIVE
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        core::hint::spin_loop();
    }
    let header = Arc::new(Mutex::new(Header { words }));
    let released = Arc::new(kcompletion::Completion::new());
    let opens = Arc::new(AtomicUsize::new(0));
    let events = Arc::new(Mutex::new(Vec::new()));
    let status_after_identify = Arc::new(Mutex::new(Vec::new()));
    let status_at_open = Arc::new(Mutex::new(Vec::new()));
    let status_history = Arc::new(kspin::SpinNoIrq::new(Vec::new()));
    reset_counts();

    FIXTURES.lock().insert(
        base,
        Spec {
            released: released.clone(),
            device_type,
            header: header.clone(),
            opens: opens.clone(),
            events: events.clone(),
            status_after_identify: status_after_identify.clone(),
            status_at_open: status_at_open.clone(),
            status_history: status_history.clone(),
        },
    );

    FixtureHandle {
        released,
        header,
        opens,
        events,
        status_after_identify,
        status_at_open,
        status_history,
        scenario_active: true,
    }
}

/// Drains opened transports before removing fixtures, including on test failure.
/// Call after unpublishing devices and dropping test-owned reader handles.
pub(super) fn clear() {
    let fixtures = core::mem::take(&mut *FIXTURES.lock());
    for spec in fixtures.values() {
        if spec.opens.load(Ordering::SeqCst) != 0 {
            ktask::completion::wait_for_completion(&spec.released)
                .expect("MMIO fixture cleanup requires sleepable context");
        }
    }
}

/// Observable state of one installed fixture.
pub(super) struct FixtureHandle {
    released: Arc<kcompletion::Completion>,
    header: Arc<Mutex<Header>>,
    opens: Arc<AtomicUsize>,
    events: Arc<Mutex<Vec<Event>>>,
    status_after_identify: Arc<Mutex<Vec<u32>>>,
    status_at_open: Arc<Mutex<Vec<u32>>>,
    status_history: Arc<kspin::SpinNoIrq<Vec<DeviceStatus>>>,
    scenario_active: bool,
}

impl FixtureHandle {
    /// Waits for queue cleanup and the final transport/claim drop, not unpublish.
    /// The scenario must remove its device and release its reader handles first.
    pub(super) fn wait_released(&self) {
        ktask::completion::wait_for_completion(&self.released)
            .expect("MMIO fixture teardown requires sleepable context");
    }

    /// Number of transports opened by all candidates.
    pub(super) fn opens(&self) -> usize {
        self.opens.load(Ordering::SeqCst)
    }

    /// Accesses in execution order.
    pub(super) fn events(&self) -> Vec<Event> {
        self.events.lock().clone()
    }

    /// Status observed immediately after each identification.
    pub(super) fn status_after_identify(&self) -> Vec<u32> {
        self.status_after_identify.lock().clone()
    }

    /// Status observed immediately before each transport open.
    pub(super) fn status_at_open(&self) -> Vec<u32> {
        self.status_at_open.lock().clone()
    }

    /// Current status register value.
    pub(super) fn status(&self) -> u32 {
        self.header.lock().words[STATUS_WORD]
    }

    /// Every status written to the opened transport, in order.
    pub(super) fn status_history(&self) -> Vec<DeviceStatus> {
        self.status_history.lock().clone()
    }
}

impl Drop for FixtureHandle {
    fn drop(&mut self) {
        if self.scenario_active {
            SCENARIO_ACTIVE.store(false, Ordering::Release);
        }
    }
}

/// In-use DMA resources reported by the production provider.
///
/// Snapshots are process-wide; scenarios which compare them run as `serial`
/// tests and do not assert on the bounce-pool total size, which may stay grown
/// after its first use.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct DmaSnapshot {
    pub(super) active_mappings: usize,
    pub(super) active_mapping_bytes: usize,
    pub(super) bounce_used_bytes: usize,
    /// Bounce-pool size, or zero before its lazy initialization.
    pub(super) bounce_total_bytes: usize,
    pub(super) coherent_allocations: usize,
    pub(super) coherent_bytes: usize,
}

impl DmaSnapshot {
    /// Captures the provider's current in-use resources.
    pub(super) fn capture() -> Self {
        let stats = driver_os_api_xkernel::dma_test_stats();
        Self {
            active_mappings: stats.active_mappings,
            active_mapping_bytes: stats.active_mapping_bytes,
            bounce_used_bytes: stats.bounce_used_bytes,
            bounce_total_bytes: stats.bounce_total_bytes,
            coherent_allocations: stats.coherent_allocations,
            coherent_bytes: stats.coherent_bytes,
        }
    }

    /// Coherent-allocation growth which is the bounce pool's one-time backing
    /// region between `before` and `self`.
    ///
    /// The pool is initialized lazily on first use and is never resized or
    /// released, so a scenario may leave at most exactly one extra coherent
    /// allocation: the backing region itself, of `bounce_total_bytes` bytes.
    /// Any other growth must be treated as a leak by the caller's exact
    /// comparison.
    pub(super) fn bounce_backing_growth(self, before: Self) -> (usize, usize) {
        assert!(
            before.bounce_total_bytes == 0 || before.bounce_total_bytes == self.bounce_total_bytes,
            "bounce pool changed size: {before:?} -> {self:?}"
        );
        if before.bounce_total_bytes == 0 && self.bounce_total_bytes != 0 {
            (1, self.bounce_total_bytes)
        } else {
            (0, 0)
        }
    }

    /// Differences since `before`.
    ///
    /// Uses checked subtraction so a resource count which decreased (a release
    /// the scenario did not own) fails the test instead of wrapping.
    pub(super) fn since(self, before: Self) -> DmaDelta {
        DmaDelta {
            active_mappings: self
                .active_mappings
                .checked_sub(before.active_mappings)
                .unwrap(),
            active_mapping_bytes: self
                .active_mapping_bytes
                .checked_sub(before.active_mapping_bytes)
                .unwrap(),
            bounce_used_bytes: self
                .bounce_used_bytes
                .checked_sub(before.bounce_used_bytes)
                .unwrap(),
            coherent_allocations: self
                .coherent_allocations
                .checked_sub(before.coherent_allocations)
                .unwrap(),
            coherent_bytes: self
                .coherent_bytes
                .checked_sub(before.coherent_bytes)
                .unwrap(),
        }
    }
}

/// Resource differences between two [`DmaSnapshot`]s.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct DmaDelta {
    pub(super) active_mappings: usize,
    pub(super) active_mapping_bytes: usize,
    pub(super) bounce_used_bytes: usize,
    pub(super) coherent_allocations: usize,
    pub(super) coherent_bytes: usize,
}

/// Builds a fresh fixture handle for a probe of `base`, if one is installed.
///
/// All handles for one base share the same header, so mutations would remain
/// visible across candidates.
pub(super) fn fixture_for(base: usize) -> Option<Fixture> {
    let (
        device_type,
        header,
        opens,
        events,
        status_after_identify,
        status_at_open,
        status_history,
        released,
    ) = {
        let fixtures = FIXTURES.lock();
        let spec = fixtures.get(&base)?;
        (
            spec.device_type,
            spec.header.clone(),
            spec.opens.clone(),
            spec.events.clone(),
            spec.status_after_identify.clone(),
            spec.status_at_open.clone(),
            spec.status_history.clone(),
            spec.released.clone(),
        )
    };
    Some(Fixture {
        released,
        device_type,
        header,
        opens,
        events,
        status_after_identify,
        status_at_open,
        status_history,
        claim: None,
    })
}

/// One probe's view of a synthetic MMIO region and the transport it may open.
pub(super) struct Fixture {
    released: Arc<kcompletion::Completion>,
    device_type: virtio::DeviceType,
    header: Arc<Mutex<Header>>,
    opens: Arc<AtomicUsize>,
    events: Arc<Mutex<Vec<Event>>>,
    status_after_identify: Arc<Mutex<Vec<u32>>>,
    status_at_open: Arc<Mutex<Vec<u32>>>,
    status_history: Arc<kspin::SpinNoIrq<Vec<DeviceStatus>>>,
    claim: Option<MmioClaim>,
}

impl Fixture {
    /// Attaches the production claim acquired for this range.
    ///
    /// `activate_virtio_mmio` claims before it consults the fixture table; a
    /// fixture without a claim refuses every access.
    pub(super) fn with_claim(mut self, claim: MmioClaim) -> Self {
        self.claim = Some(claim);
        self
    }

    fn current_status(&self) -> u32 {
        self.header.lock().words[STATUS_WORD]
    }
}

/// Signals only after the owned transport and its MMIO claim have been dropped.
pub(super) struct ReleaseNotification(Arc<kcompletion::Completion>);

impl Drop for ReleaseNotification {
    fn drop(&mut self) {
        self.0.complete_all();
    }
}

impl MmioAccess for Fixture {
    // Tuple fields drop in order: release the real claim before notifying.
    type Transport = OwnedMmioTransport<(MmioClaim, ReleaseNotification), MockTransport>;

    fn identify(&mut self, size: usize) -> Result<virtio::MmioIdentification, DriverError> {
        if self.claim.is_none() {
            return Err(DriverError::BadState);
        }
        if size > HEADER_SIZE {
            return Err(DriverError::InvalidInput);
        }
        // Keep the lock across the read so the synthetic registers cannot be
        // observed or changed by another access while the vendor reader holds
        // its `UniqueMmioPointer`.
        let (result, status) = {
            let mut header = self.header.lock();
            let Some(ptr) = NonNull::new(header.words.as_mut_ptr().cast::<u8>()) else {
                return Err(DriverError::BadState);
            };
            // SAFETY: the inline header is 4-byte aligned, `size <= HEADER_SIZE`,
            // and the header mutex excludes every other fixture access for the
            // duration of the call.
            let result = unsafe { virtio::identify_mmio_device(ptr, size) };
            (result, header.words[STATUS_WORD])
        };
        self.status_after_identify.lock().push(status);
        self.events.lock().push(Event::Identify);
        result.map_err(super::mmio_error)
    }

    fn open(self, _size: usize) -> DriverResult<Self::Transport> {
        // A transport resets the device on drop, so it may only exist while the
        // claim that made the range exclusive is held. Moving the claim into
        // the owned transport keeps it alive until after that reset.
        let status = self.current_status();
        let claim = self.claim.ok_or(DriverError::BadState)?;
        self.status_at_open.lock().push(status);
        self.events.lock().push(Event::Open);
        self.opens.fetch_add(1, Ordering::SeqCst);
        self.released.reinit();
        let mut transport = MockTransport::new_with_type(self.device_type);
        transport.status_history = self.status_history;
        Ok(OwnedMmioTransport::new(
            transport,
            (claim, ReleaseNotification(self.released)),
        ))
    }

    fn is_fixture(&self) -> bool {
        true
    }
}
