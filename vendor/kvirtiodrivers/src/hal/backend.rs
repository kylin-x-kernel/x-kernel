use super::{BufferDirection, Hal, PhysAddr};
use crate::{Error, Result};
use core::{marker::PhantomData, ptr::NonNull};

/// DMA operations owned by one device instance, independent of MMIO translation.
///
/// Implementations retain the device's address space, DMA constraints and cache
/// policy. Cloning must retain that same instance, not select a current device.
/// Allocation and sharing failures must leave no allocation or mapping behind.
/// Release operations are infallible and must be legal in the caller's context.
///
/// # Safety
///
/// Successful allocation has the validity, alignment, zeroing and exclusivity
/// guarantees of [`Hal::dma_alloc`]. Allocations must support coherent CPU/device
/// access throughout their lifetime: queue code provides ordering barriers but
/// does not call cache-maintenance hooks for ring memory. A non-coherent platform
/// must provide a suitable mapping, not just flush the allocation once.
/// Sharing and unsharing have the memory and
/// cache synchronization guarantees of [`Hal::share`] and [`Hal::unshare`].
/// Clones must accept each other's allocations and mappings for release, and
/// keep the underlying device address space alive until the final release.
pub unsafe trait DmaBackend: Clone {
    /// Allocates zeroed DMA pages. A successful device address may be zero.
    fn allocate(
        &self,
        pages: usize,
        direction: BufferDirection,
        access_platform: bool,
    ) -> Result<(PhysAddr, NonNull<u8>)>;

    /// Releases a previous allocation through the allocating instance.
    ///
    /// # Safety
    ///
    /// All arguments must match a live allocation from this instance or a clone.
    /// Neither the device nor any CPU may access the allocation after this call.
    unsafe fn deallocate(
        &self,
        paddr: PhysAddr,
        vaddr: NonNull<u8>,
        pages: usize,
        direction: BufferDirection,
        access_platform: bool,
    );

    /// Maps a buffer for this device; failure retains no mapping.
    ///
    /// # Safety
    ///
    /// The caller upholds [`Hal::share`]'s buffer validity and exclusivity
    /// requirements and retains the buffer until the matching `unshare`.
    unsafe fn share(
        &self,
        buffer: NonNull<[u8]>,
        direction: BufferDirection,
        access_platform: bool,
    ) -> Result<PhysAddr>;

    /// Unmaps a buffer and performs any required cache maintenance or copy-back.
    ///
    /// # Safety
    ///
    /// Arguments must match a successful, not yet unmapped `share` on this
    /// instance or a clone. Device access must have stopped; the buffer remains
    /// valid and exclusively held throughout this call.
    unsafe fn unshare(
        &self,
        paddr: PhysAddr,
        buffer: NonNull<[u8]>,
        direction: BufferDirection,
        access_platform: bool,
    );
}

/// Compatibility backend forwarding to the original static [`Hal`] API.
#[derive(Debug)]
pub struct StaticHal<H: Hal>(PhantomData<fn() -> H>);

impl<H: Hal> Clone for StaticHal<H> {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl<H: Hal> Default for StaticHal<H> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

// SAFETY: Every operation forwards unchanged to the same Hal implementation;
// clones carry no per-instance state and therefore share that implementation.
unsafe impl<H: Hal> DmaBackend for StaticHal<H> {
    fn allocate(
        &self,
        pages: usize,
        direction: BufferDirection,
        access_platform: bool,
    ) -> Result<(PhysAddr, NonNull<u8>)> {
        let allocation = H::dma_alloc(pages, direction, access_platform);
        if allocation.0 == 0 {
            Err(Error::DmaError)
        } else {
            Ok(allocation)
        }
    }

    unsafe fn deallocate(
        &self,
        paddr: PhysAddr,
        vaddr: NonNull<u8>,
        pages: usize,
        _direction: BufferDirection,
        access_platform: bool,
    ) {
        // SAFETY: The caller supplies the live allocation and original parameters.
        let result = unsafe { H::dma_dealloc(paddr, vaddr, pages, access_platform) };
        assert_eq!(result, 0, "failed to deallocate DMA");
    }

    unsafe fn share(
        &self,
        buffer: NonNull<[u8]>,
        direction: BufferDirection,
        access_platform: bool,
    ) -> Result<PhysAddr> {
        // SAFETY: The caller upholds the same buffer contract as Hal::share.
        Ok(unsafe { H::share(buffer, direction, access_platform) })
    }

    unsafe fn unshare(
        &self,
        paddr: PhysAddr,
        buffer: NonNull<[u8]>,
        direction: BufferDirection,
        access_platform: bool,
    ) {
        // SAFETY: The caller supplies the original live mapping after device access ends.
        unsafe { H::unshare(paddr, buffer, direction, access_platform) };
    }
}
