// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Adapt a held device DMA capability to the vendor's raw allocation ABI.

use alloc::{collections::BTreeMap, sync::Arc};
use core::ptr::NonNull;

use driver_os_api::{DeviceDma, DmaAllocation, DmaDirection, DmaSpec, ResError, StreamingMapping};
use kspin::SpinNoIrq;
use virtio_drivers::{BufferDirection, DmaBackend, Error, PAGE_SIZE, PhysAddr, Result};

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
struct AllocationKey {
    bus: PhysAddr,
    cpu: usize,
    pages: usize,
    direction: u8,
    access_platform: bool,
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
struct MappingKey {
    bus: PhysAddr,
    cpu: usize,
    len: usize,
    direction: u8,
    access_platform: bool,
}

struct State {
    capability: Arc<dyn DeviceDma>,
    allocations: SpinNoIrq<BTreeMap<AllocationKey, DmaAllocation>>,
    mappings: SpinNoIrq<BTreeMap<MappingKey, StreamingMapping>>,
}

/// One device's DMA backend, shared only with clones of this instance.
///
/// Private records bridge the vendor's raw tuples to provider-owned allocation
/// and mapping tokens. Provider calls and token destruction happen outside the
/// record locks. Arc/BTreeMap metadata follows the kernel's infallible
/// allocation policy; metadata OOM is not reported as a recoverable DMA error.
#[derive(Clone)]
pub struct DeviceDmaBackend {
    state: Arc<State>,
}

impl DeviceDmaBackend {
    /// Retain an existing capability without allocating DMA memory.
    ///
    /// `access_platform` selects no alternate provider. The flag is retained in
    /// each exact release key, never silently changed on release. This is not
    /// general support for both DMA-bypass and platform-translated address
    /// spaces.
    ///
    /// # Safety
    /// The capability's acquisition, synchronization and release operations
    /// must be non-sleeping and legal under the RNG request lock and in IRQ
    /// context, as well as during task-context teardown. Metadata allocation
    /// must also be legal in those contexts. The supplied capability must
    /// produce addresses and platform-sharing effects actually usable in BOTH
    /// negotiated modes: DMA-bypass (`access_platform == false`) and platform
    /// DMA (`true`). The intended binding is QEMU identity DMA without an
    /// IOMMU. Arbitrary translated/IOMMU capabilities do not automatically meet
    /// this requirement; they need an explicit future mode contract. This
    /// adapter cannot discover or establish that policy.
    ///
    /// All users must supply initialized streaming buffers and retain exclusive
    /// access until unshare, stopping device access before release. The
    /// provider must preserve original bytes on a map/unmap round trip without
    /// device writes, including RX, because queue setup rollback also calls
    /// unshare. Keep an instance clone alive until every raw allocation/mapping
    /// is released, and do not drop the last clone with device access
    /// outstanding.
    pub unsafe fn new(capability: Arc<dyn DeviceDma>) -> Self {
        Self {
            state: Arc::new(State {
                capability,
                allocations: SpinNoIrq::new(BTreeMap::new()),
                mappings: SpinNoIrq::new(BTreeMap::new()),
            }),
        }
    }
}

// SAFETY: the unsafe constructor binds one stable, context-compatible provider
// and initialized/exclusive buffer discipline. Clones retain the same state.
// Each successful raw tuple retains a unique provider token; exact-key removal
// transfers it out of the lock before cleanup. Coherent validity and cache
// synchronization are supplied by DeviceDma, including byte-preserving
// unpublished rollback.
unsafe impl DmaBackend for DeviceDmaBackend {
    fn allocate(
        &self,
        pages: usize,
        direction: BufferDirection,
        access_platform: bool,
    ) -> Result<(PhysAddr, NonNull<u8>)> {
        let len = pages.checked_mul(PAGE_SIZE).ok_or(Error::InvalidParam)?;
        if len == 0 || len > isize::MAX as usize {
            return Err(Error::InvalidParam);
        }
        let spec = DmaSpec::new(len, PAGE_SIZE).with_direction(os_direction(direction));
        let allocation = self
            .state
            .capability
            .alloc_coherent(spec)
            .map_err(map_error)?;
        let Some(pointer) = NonNull::new(allocation.cpu_addr as *mut u8) else {
            self.state.capability.free_coherent(allocation);
            return Err(Error::InvalidParam);
        };
        if allocation.spec != spec {
            self.state.capability.free_coherent(allocation);
            return Err(Error::InvalidParam);
        }
        let cpu = pointer.as_ptr() as usize;
        let bus = allocation.bus_addr;
        if !cpu.is_multiple_of(PAGE_SIZE)
            || !bus.is_multiple_of(PAGE_SIZE as u64)
            || cpu.checked_add(len - 1).is_none()
            || bus.checked_add((len - 1) as u64).is_none()
        {
            self.state.capability.free_coherent(allocation);
            return Err(Error::InvalidParam);
        }
        let key = AllocationKey {
            bus,
            cpu,
            pages,
            direction: direction_code(direction),
            access_platform,
        };
        // `DmaAllocation` is a plain record, so a rejected duplicate must be
        // returned to the provider here rather than dropped.
        let rejected = {
            let mut records = self.state.allocations.lock();
            match records.entry(key) {
                alloc::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(allocation);
                    None
                }
                alloc::collections::btree_map::Entry::Occupied(_) => Some(allocation),
            }
        };
        if let Some(allocation) = rejected {
            self.state.capability.free_coherent(allocation);
            return Err(Error::AlreadyUsed);
        }
        Ok((bus, pointer))
    }

    unsafe fn deallocate(
        &self,
        bus: PhysAddr,
        pointer: NonNull<u8>,
        pages: usize,
        direction: BufferDirection,
        access_platform: bool,
    ) {
        let key = AllocationKey {
            bus,
            cpu: pointer.as_ptr() as usize,
            pages,
            direction: direction_code(direction),
            access_platform,
        };
        let allocation = { self.state.allocations.lock().remove(&key) };
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: unsafe deallocate requires the exact \
                      live tuple from this instance, consumed once"
        )]
        let allocation = allocation.expect("DMA allocation release tuple is not active");
        self.state.capability.free_coherent(allocation);
    }

    unsafe fn share(
        &self,
        buffer: NonNull<[u8]>,
        direction: BufferDirection,
        access_platform: bool,
    ) -> Result<PhysAddr> {
        let len = buffer.len();
        if len == 0 || len > isize::MAX as usize {
            return Err(Error::InvalidParam);
        }
        let cpu = buffer.as_ptr() as *mut u8 as usize;
        if cpu.checked_add(len - 1).is_none() {
            return Err(Error::InvalidParam);
        }
        // SAFETY: the constructor and share caller jointly guarantee initialized,
        // live, exclusive storage until unshare. No device has this mapping yet.
        let mapping = unsafe {
            self.state
                .capability
                .map_streaming(buffer, os_direction(direction))
        }
        .map_err(map_error)?;
        let bus = mapping.bus_addr();
        if mapping.len() != len || bus.checked_add((len - 1) as u64).is_none() {
            drop(mapping);
            return Err(Error::InvalidParam);
        }
        let key = MappingKey {
            bus,
            cpu,
            len,
            direction: direction_code(direction),
            access_platform,
        };
        insert_unique(&self.state.mappings, key, mapping)?;
        Ok(bus)
    }

    unsafe fn unshare(
        &self,
        bus: PhysAddr,
        buffer: NonNull<[u8]>,
        direction: BufferDirection,
        access_platform: bool,
    ) {
        let key = MappingKey {
            bus,
            cpu: buffer.as_ptr() as *mut u8 as usize,
            len: buffer.len(),
            direction: direction_code(direction),
            access_platform,
        };
        let mapping = { self.state.mappings.lock().remove(&key) };
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: unsafe unshare requires the exact \
                      live tuple from this instance after device access stops"
        )]
        let mapping = mapping.expect("DMA streaming release tuple is not active");
        drop(mapping);
    }
}

fn insert_unique<Key: Ord, Token>(
    records: &SpinNoIrq<BTreeMap<Key, Token>>,
    key: Key,
    token: Token,
) -> Result {
    let rejected = {
        let mut records = records.lock();
        match records.entry(key) {
            alloc::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(token);
                None
            }
            alloc::collections::btree_map::Entry::Occupied(_) => Some(token),
        }
    };
    if let Some(token) = rejected {
        drop(token);
        return Err(Error::AlreadyUsed);
    }
    Ok(())
}

fn direction_code(direction: BufferDirection) -> u8 {
    match direction {
        BufferDirection::DriverToDevice => 0,
        BufferDirection::DeviceToDriver => 1,
        BufferDirection::Both => 2,
    }
}

fn os_direction(direction: BufferDirection) -> DmaDirection {
    match direction {
        BufferDirection::DriverToDevice => DmaDirection::DriverToDevice,
        BufferDirection::DeviceToDriver => DmaDirection::DeviceToDriver,
        BufferDirection::Both => DmaDirection::Bidirectional,
    }
}

fn map_error(error: ResError) -> Error {
    match error {
        ResError::InvalidResource => Error::InvalidParam,
        ResError::Unsupported => Error::Unsupported,
        ResError::NoMemory => Error::DmaError,
        ResError::Busy | ResError::AlreadyExists => Error::AlreadyUsed,
        ResError::BadState | ResError::NotFound | ResError::Interrupted => Error::NotReady,
        ResError::MappingFailed | ResError::Io => Error::IoError,
    }
}

unittest::path_mod_test!(tests, "../ut-tests/device_dma_unittest.rs");
